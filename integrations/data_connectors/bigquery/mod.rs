use crate::data_connectors::{
    string_setting, ConnectionConfig, Connector, ConnectorError, ConnectorFuture, FieldSummary,
    IndexSummary, IntegrationDescriptor,
};
use crate::search::SearchRequest;
use crate::search::{
    BreakdownCount, HistogramInterval, HistogramOptions, MAX_BREAKDOWN_VALUES,
    MAX_HISTOGRAM_BUCKETS,
};
use axum::http::StatusCode;
use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine};
use reqwest::{Client, Method, Response, Url};
use serde::{de::DeserializeOwned, Deserialize, Serialize};
use serde_json::{json, Value};
use sqlparser::{ast::Statement, dialect::BigQueryDialect, parser::Parser};
use std::{
    hash::Hash,
    sync::Arc,
    time::{Duration, Instant},
};
use thiserror::Error;
use yup_oauth2::{
    authenticator::DefaultAuthenticator, ServiceAccountAuthenticator, ServiceAccountKey,
};

const API_ROOT: &str = "https://bigquery.googleapis.com/";
const BIGQUERY_SCOPE: &str = "https://www.googleapis.com/auth/bigquery";
const MAX_QUERY_BYTES: usize = 8_192;
const MAX_CREDENTIAL_BYTES: usize = 65_536;
const MAX_RESPONSE_BYTES: usize = 16 * 1024 * 1024;
const MAX_PAGE_SIZE: usize = 100;
const MAX_BYTES_BILLED: u64 = 10 * 1024 * 1024 * 1024;
const REQUEST_TIMEOUT: Duration = Duration::from_secs(15);
const MAX_LISTED_TABLES: usize = 5_000;
const MAX_CURSOR_BYTES: usize = 8_192;
const MAX_PAGE_TOKEN_BYTES: usize = 2_048;
const MAX_HISTOGRAM_ROWS: usize = MAX_HISTOGRAM_BUCKETS as usize * (MAX_BREAKDOWN_VALUES + 1);

pub fn descriptor() -> IntegrationDescriptor {
    IntegrationDescriptor {
        id: "bigquery",
        display_name: "Google BigQuery",
        query_language: "SQL",
        description: "Read-only Standard SQL queries over one configured BigQuery dataset",
        connection_fields: vec![
            json!({"name":"url","label":"API URL","input_type":"url","required":true,"secret":false,"editable":true,"placeholder":API_ROOT}),
            json!({"name":"project_id","label":"Billing project ID","input_type":"text","required":true,"secret":false,"editable":true,"placeholder":"my-gcp-project"}),
            json!({"name":"dataset_id","label":"Dataset ID","input_type":"text","required":true,"secret":false,"editable":true,"placeholder":"analytics"}),
            json!({"name":"location","label":"Dataset location (optional)","input_type":"text","required":false,"secret":false,"editable":true,"placeholder":"US"}),
            json!({"name":"service_account_json","label":"Service-account JSON key","input_type":"password","required":true,"secret":true,"editable":true,"group":"credentials","placeholder":"Paste the JSON key contents"}),
        ],
        query: json!({"placeholder":"SELECT * FROM `{index}`","default_query":"SELECT * FROM `{index}`","field_query":"SELECT {field} FROM `{index}`","identifier_quote":"","help":"Read-only BigQuery Standard SQL. Each query is limited to 10 GiB billed bytes.","document_mode":"inline","sort_requires_projection":true,"histogram_total_from_buckets":false,"schema_label":"BigQuery table schema"}),
    }
}

#[derive(Debug, Error)]
enum BigQueryError {
    #[error("BigQuery connection details are invalid")]
    InvalidConnection,
    #[error("BigQuery request must be one read-only Standard SQL query under 8192 bytes")]
    InvalidQuery,
    #[error("BigQuery credentials could not be used")]
    Authentication,
    #[error("BigQuery request could not be completed")]
    Request,
    #[error("BigQuery rejected the request with HTTP {0}")]
    Rejected(u16),
    #[error("BigQuery returned an unusable response")]
    InvalidResponse,
}

struct BigQueryClient {
    endpoint: Url,
    project_id: String,
    dataset_id: String,
    location: Option<String>,
    service_account: ServiceAccountKey,
    authenticator: tokio::sync::OnceCell<DefaultAuthenticator>,
    http: Client,
}

pub fn connect(config: &ConnectionConfig) -> Result<Arc<dyn Connector>, ConnectorError> {
    let endpoint = string_setting(config, "url")?.ok_or_else(ConnectorError::invalid_connection)?;
    let project_id =
        string_setting(config, "project_id")?.ok_or_else(ConnectorError::invalid_connection)?;
    let dataset_id =
        string_setting(config, "dataset_id")?.ok_or_else(ConnectorError::invalid_connection)?;
    let location = string_setting(config, "location")?.filter(|value| !value.trim().is_empty());
    let credentials = string_setting(config, "service_account_json")?
        .ok_or_else(ConnectorError::invalid_connection)?;
    let client = BigQueryClient::new(&endpoint, &project_id, &dataset_id, location, &credentials)?;
    Ok(Arc::new(client))
}

impl BigQueryClient {
    fn new(
        endpoint: &str,
        project_id: &str,
        dataset_id: &str,
        location: Option<String>,
        credentials: &str,
    ) -> Result<Self, BigQueryError> {
        let endpoint = Url::parse(endpoint).map_err(|_| BigQueryError::InvalidConnection)?;
        if endpoint.as_str() != API_ROOT
            || !valid_project_id(project_id)
            || !valid_dataset_id(dataset_id)
            || location.as_deref().is_some_and(|value| {
                value.is_empty()
                    || value.len() > 32
                    || !value
                        .bytes()
                        .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-')
            })
            || credentials.is_empty()
            || credentials.len() > MAX_CREDENTIAL_BYTES
        {
            return Err(BigQueryError::InvalidConnection);
        }
        let service_account = yup_oauth2::parse_service_account_key(credentials.as_bytes())
            .map_err(|_| BigQueryError::InvalidConnection)?;
        if service_account.key_type.as_deref() != Some("service_account")
            || !valid_token_uri(&service_account.token_uri)
        {
            return Err(BigQueryError::InvalidConnection);
        }
        let http = Client::builder()
            .timeout(REQUEST_TIMEOUT)
            .redirect(reqwest::redirect::Policy::none())
            .build()
            .map_err(|_| BigQueryError::InvalidConnection)?;
        Ok(Self {
            endpoint,
            project_id: project_id.to_owned(),
            dataset_id: dataset_id.to_owned(),
            location,
            service_account,
            authenticator: tokio::sync::OnceCell::new(),
            http,
        })
    }
    #[cfg(test)]
    fn test_client(endpoint: &str) -> Self {
        let credentials = json!({
            "type":"service_account",
            "project_id":"my-gcp-project",
            "private_key_id":"test-key",
            "private_key":"not-a-real-private-key",
            "client_email":"test@my-gcp-project.iam.gserviceaccount.com",
            "token_uri":"https://oauth2.googleapis.com/token"
        })
        .to_string();
        let service_account =
            yup_oauth2::parse_service_account_key(credentials.as_bytes()).unwrap();
        Self {
            endpoint: Url::parse(endpoint).unwrap(),
            project_id: "my-gcp-project".to_owned(),
            dataset_id: "analytics".to_owned(),
            location: Some("US".to_owned()),
            service_account,
            authenticator: tokio::sync::OnceCell::new(),
            http: Client::builder()
                .timeout(REQUEST_TIMEOUT)
                .redirect(reqwest::redirect::Policy::none())
                .build()
                .unwrap(),
        }
    }

    async fn authenticator(&self) -> Result<&DefaultAuthenticator, BigQueryError> {
        self.authenticator
            .get_or_try_init(|| async {
                ServiceAccountAuthenticator::builder(self.service_account.clone())
                    .with_timeout(REQUEST_TIMEOUT)
                    .build()
                    .await
                    .map_err(|_| BigQueryError::Authentication)
            })
            .await
    }

    async fn access_token(&self, auth: &DefaultAuthenticator) -> Result<String, BigQueryError> {
        let token = auth
            .token(&[BIGQUERY_SCOPE])
            .await
            .map_err(|_| BigQueryError::Authentication)?;
        token
            .token()
            .map(str::to_owned)
            .filter(|token| !token.is_empty())
            .ok_or(BigQueryError::Authentication)
    }

    fn api_url(&self, path: &str) -> Result<Url, BigQueryError> {
        self.endpoint
            .join(path)
            .map_err(|_| BigQueryError::InvalidConnection)
    }

    async fn json_request<T: DeserializeOwned>(
        &self,
        method: Method,
        url: Url,
        token: &str,
        body: Option<Value>,
    ) -> Result<T, BigQueryError> {
        let mut request = self
            .http
            .request(method, url)
            .bearer_auth(token)
            .header(reqwest::header::ACCEPT, "application/json");
        if let Some(body) = body {
            request = request.json(&body);
        }
        let response = request.send().await.map_err(|_| BigQueryError::Request)?;
        self.decode_response(response).await
    }

    async fn decode_response<T: DeserializeOwned>(
        &self,
        mut response: Response,
    ) -> Result<T, BigQueryError> {
        if !response.status().is_success() {
            return Err(BigQueryError::Rejected(response.status().as_u16()));
        }
        if response
            .content_length()
            .is_some_and(|length| length > MAX_RESPONSE_BYTES as u64)
        {
            return Err(BigQueryError::InvalidResponse);
        }
        let mut bytes = Vec::new();
        while let Some(chunk) = response.chunk().await.map_err(|_| BigQueryError::Request)? {
            if bytes.len().saturating_add(chunk.len()) > MAX_RESPONSE_BYTES {
                return Err(BigQueryError::InvalidResponse);
            }
            bytes.extend_from_slice(&chunk);
        }
        serde_json::from_slice(&bytes).map_err(|_| BigQueryError::InvalidResponse)
    }

    async fn test_connection_with_token(&self, token: &str) -> Result<(), BigQueryError> {
        let path = format!(
            "bigquery/v2/projects/{}/datasets/{}",
            self.project_id, self.dataset_id
        );
        let _: Value = self
            .json_request(Method::GET, self.api_url(&path)?, token, None)
            .await?;
        let query_path = format!("bigquery/v2/projects/{}/queries", self.project_id);
        let mut query_url = self.api_url(&query_path)?;
        if let Some(location) = &self.location {
            query_url
                .query_pairs_mut()
                .append_pair("location", location);
        }
        let _: Value = self
            .json_request(
                Method::POST,
                query_url,
                token,
                Some(json!({
                    "query": "SELECT 1",
                    "useLegacySql": false,
                    "dryRun": true,
                    "maximumBytesBilled": MAX_BYTES_BILLED.to_string(),
                })),
            )
            .await?;
        Ok(())
    }

    async fn list_tables_with_token(&self, token: &str) -> Result<Vec<String>, BigQueryError> {
        let path = format!(
            "bigquery/v2/projects/{}/datasets/{}/tables",
            self.project_id, self.dataset_id
        );
        let mut page_token: Option<String> = None;
        let mut names = Vec::new();
        let mut pages = 0usize;
        loop {
            pages += 1;
            if pages > MAX_LISTED_TABLES / 1_000 + 1 {
                return Err(BigQueryError::InvalidResponse);
            }
            let mut url = self.api_url(&path)?;
            {
                let mut query = url.query_pairs_mut();
                query.append_pair("maxResults", "1000");
                if let Some(page_token) = &page_token {
                    query.append_pair("pageToken", page_token);
                }
            }
            let page: TableListResponse = self.json_request(Method::GET, url, token, None).await?;
            for table in page.tables.unwrap_or_default() {
                let table_id = table
                    .table_reference
                    .and_then(|reference| reference.table_id)
                    .filter(|table_id| valid_table_id(table_id))
                    .ok_or(BigQueryError::InvalidResponse)?;
                names.push(format!(
                    "{}.{}.{}",
                    self.project_id, self.dataset_id, table_id
                ));
                if names.len() > MAX_LISTED_TABLES {
                    return Err(BigQueryError::InvalidResponse);
                }
            }
            page_token = page.next_page_token;
            if page_token.is_none() {
                break;
            }
        }
        names.sort();
        Ok(names)
    }

    async fn fields_with_token(
        &self,
        index: &str,
        token: &str,
    ) -> Result<Vec<FieldSchema>, BigQueryError> {
        let table_id = self.table_name(index)?;
        let path = format!(
            "bigquery/v2/projects/{}/datasets/{}/tables/{}",
            self.project_id, self.dataset_id, table_id
        );
        let table: TableResponse = self
            .json_request(Method::GET, self.api_url(&path)?, token, None)
            .await?;
        let fields = table
            .schema
            .map(|schema| schema.fields)
            .ok_or(BigQueryError::InvalidResponse)?;
        let mut result = Vec::new();
        flatten_schema(&fields, "", 0, &mut result)?;
        Ok(result)
    }

    async fn query_job(
        &self,
        token: &str,
        sql: &str,
        page_size: usize,
    ) -> Result<QueryPage, BigQueryError> {
        let path = format!("bigquery/v2/projects/{}/queries", self.project_id);
        let mut url = self.api_url(&path)?;
        if let Some(location) = &self.location {
            url.query_pairs_mut().append_pair("location", location);
        }
        let body = json!({
            "query": sql,
            "useLegacySql": false,
            "maximumBytesBilled": MAX_BYTES_BILLED.to_string(),
            "maxResults": page_size,
            "timeoutMs": 10_000,
        });
        let mut page: QueryPage = self
            .json_request(Method::POST, url, token, Some(body))
            .await?;
        let job_id = page
            .job_reference
            .as_ref()
            .and_then(|reference| reference.job_id.as_deref())
            .ok_or(BigQueryError::InvalidResponse)?
            .to_owned();
        if !valid_job_id(&job_id) {
            return Err(BigQueryError::InvalidResponse);
        }
        let started = Instant::now();
        while !page.job_complete {
            if started.elapsed() >= REQUEST_TIMEOUT {
                return Err(BigQueryError::Request);
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
            page = self
                .get_query_results(token, &job_id, page.page_token.as_deref(), None, page_size)
                .await?;
        }
        Ok(page)
    }

    async fn get_query_results(
        &self,
        token: &str,
        job_id: &str,
        page_token: Option<&str>,
        start_index: Option<u64>,
        page_size: usize,
    ) -> Result<QueryPage, BigQueryError> {
        if job_id.is_empty()
            || job_id.len() > 256
            || !job_id
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || b"_-".contains(&byte))
        {
            return Err(BigQueryError::InvalidQuery);
        }
        let path = format!(
            "bigquery/v2/projects/{}/queries/{}",
            self.project_id, job_id
        );
        let mut url = self.api_url(&path)?;
        {
            let mut query = url.query_pairs_mut();
            query.append_pair("maxResults", &page_size.to_string());
            if let Some(location) = &self.location {
                query.append_pair("location", location);
            }
            if let Some(page_token) = page_token {
                query.append_pair("pageToken", page_token);
            }
            if let Some(start_index) = start_index {
                query.append_pair("startIndex", &start_index.to_string());
            }
        }
        self.json_request(Method::GET, url, token, None).await
    }

    async fn query_job_with_token(
        &self,
        token: &str,
        sql: &str,
        page_size: usize,
        start_index: Option<u64>,
    ) -> Result<(QueryPage, Option<String>), BigQueryError> {
        let first = self.query_job(token, sql, page_size).await?;
        let Some(start_index) = start_index.filter(|index| *index > 0) else {
            return Ok((first, None));
        };
        let reference = first
            .job_reference
            .as_ref()
            .ok_or(BigQueryError::InvalidResponse)?;
        let job_id = reference
            .job_id
            .as_deref()
            .ok_or(BigQueryError::InvalidResponse)?;
        let page = self
            .get_query_results(token, job_id, None, Some(start_index), page_size)
            .await?;
        Ok((page, Some(job_id.to_owned())))
    }

    async fn histogram_with_token(
        &self,
        token: &str,
        sql: &str,
        time_field: &str,
        breakdown_field: Option<&str>,
        range: Option<&crate::search::TimeRange>,
        options: &HistogramOptions,
    ) -> Result<(Vec<Value>, Option<String>), BigQueryError> {
        let interval = if let Some(interval) = options.interval {
            interval
        } else if let Some(range) = range {
            let span = range.end.saturating_sub(range.start) as f64;
            HistogramInterval::automatic(span).ok_or(BigQueryError::InvalidQuery)?
        } else {
            let field = quote_field_path(time_field)?;
            let extents_sql = format!(
                "SELECT MIN(TIMESTAMP(__ig_source.{field})) AS min_time, MAX(TIMESTAMP(__ig_source.{field})) AS max_time FROM ({sql}) AS __ig_source"
            );
            let extents = self.query_job(token, &extents_sql, 1).await?;
            let fields = extents
                .schema
                .as_ref()
                .map(|schema| schema.fields.as_slice())
                .unwrap_or_default();
            let values = decode_rows(extents.rows.as_deref().unwrap_or_default(), fields)?;
            let Some(extents) = values.first() else {
                return Ok((Vec::new(), None));
            };
            let min = extents.get("min_time").and_then(parse_timestamp_value);
            let max = extents.get("max_time").and_then(parse_timestamp_value);
            let (Some(min), Some(max)) = (min, max) else {
                return Ok((Vec::new(), None));
            };
            HistogramInterval::automatic(max.saturating_sub(min) as f64)
                .ok_or(BigQueryError::InvalidResponse)?
        };

        let time = quote_field_path(time_field)?;
        let bucket = timestamp_bucket_expression(
            &format!("__ig_histogram_source.{time}"),
            interval,
            &options.time_zone,
        );
        let aggregation_sql = if let Some(field) = breakdown_field {
            let breakdown = quote_field_path(field)?;
            format!(
                "WITH __ig_histogram_source AS ({sql}), __ig_top_values AS (SELECT CAST({breakdown} AS STRING) AS key, COUNT(*) AS total FROM __ig_histogram_source GROUP BY key ORDER BY total DESC LIMIT {MAX_BREAKDOWN_VALUES}), __ig_grouped AS (SELECT {bucket} AS timestamp, CAST({breakdown} AS STRING) AS key, COUNT(*) AS count FROM __ig_histogram_source GROUP BY timestamp, key) SELECT timestamp, IF(key IN (SELECT key FROM __ig_top_values), key, '{other}') AS key, SUM(count) AS count FROM __ig_grouped GROUP BY timestamp, key ORDER BY timestamp, key",
                other = crate::search::BREAKDOWN_OTHER_KEY,
            )
        } else {
            format!(
                "SELECT {bucket} AS timestamp, COUNT(*) AS count FROM ({sql}) AS __ig_histogram_source GROUP BY timestamp ORDER BY timestamp"
            )
        };
        let page = self
            .query_job(token, &aggregation_sql, MAX_HISTOGRAM_ROWS)
            .await?;
        if page.page_token.is_some() {
            return Err(BigQueryError::InvalidResponse);
        }
        let schema = page
            .schema
            .as_ref()
            .map(|schema| schema.fields.as_slice())
            .unwrap_or_default();
        let rows = decode_rows(page.rows.as_deref().unwrap_or_default(), schema)?;
        let mut buckets = std::collections::BTreeMap::<i64, (u64, Vec<BreakdownCount>)>::new();
        for row in rows {
            let timestamp = row
                .get("timestamp")
                .and_then(parse_timestamp_value)
                .ok_or(BigQueryError::InvalidResponse)?;
            let count = row
                .get("count")
                .and_then(Value::as_u64)
                .ok_or(BigQueryError::InvalidResponse)?;
            let bucket = buckets.entry(timestamp).or_default();
            bucket.0 = bucket.0.saturating_add(count);
            if breakdown_field.is_some() {
                let key = row
                    .get("key")
                    .and_then(Value::as_str)
                    .unwrap_or(crate::search::BREAKDOWN_OTHER_KEY)
                    .to_owned();
                bucket.1.push(BreakdownCount { key, count });
            }
        }
        if buckets.len() > MAX_HISTOGRAM_BUCKETS as usize {
            return Err(BigQueryError::InvalidResponse);
        }
        let histogram = buckets
            .into_iter()
            .map(|(timestamp, (count, breakdown))| {
                let mut value = json!({"timestamp":timestamp,"label":null,"count":count});
                if breakdown_field.is_some() {
                    value["breakdown"] = json!(breakdown);
                }
                value
            })
            .collect();
        Ok((
            histogram,
            Some(format!("{}{}", interval.count, interval.suffix())),
        ))
    }

    fn table_name<'a>(&self, index: &'a str) -> Result<&'a str, BigQueryError> {
        let prefix = format!("{}.{}.", self.project_id, self.dataset_id);
        let table = index
            .strip_prefix(&prefix)
            .filter(|table| valid_table_id(table))
            .ok_or(BigQueryError::InvalidQuery)?;
        Ok(table)
    }
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct TableListResponse {
    tables: Option<Vec<TableListItem>>,
    next_page_token: Option<String>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct TableListItem {
    table_reference: Option<TableReference>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct TableReference {
    table_id: Option<String>,
}

#[derive(Deserialize)]
struct TableResponse {
    schema: Option<TableSchema>,
}

#[derive(Deserialize)]
struct TableSchema {
    fields: Vec<FieldSchema>,
}

#[derive(Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
struct FieldSchema {
    name: String,
    #[serde(rename = "type")]
    field_type: String,
    mode: Option<String>,
    #[serde(default)]
    fields: Vec<FieldSchema>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct JobReference {
    job_id: Option<String>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct QueryPage {
    job_reference: Option<JobReference>,
    job_complete: bool,
    total_rows: Option<String>,
    page_token: Option<String>,
    schema: Option<TableSchema>,
    rows: Option<Vec<Value>>,
}

#[derive(Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct SearchCursor {
    job_id: String,
    page_token: String,
    query_hash: u64,
    next_offset: u64,
}

impl SearchCursor {
    fn encode(&self) -> Result<String, BigQueryError> {
        if !valid_job_id(&self.job_id)
            || self.next_offset > u64::MAX - MAX_PAGE_SIZE as u64
            || self.page_token.is_empty()
            || self.page_token.len() > MAX_PAGE_TOKEN_BYTES
        {
            return Err(BigQueryError::InvalidResponse);
        }
        let json = serde_json::to_vec(self).map_err(|_| BigQueryError::InvalidResponse)?;
        let token = URL_SAFE_NO_PAD.encode(json);
        if token.len() > MAX_CURSOR_BYTES {
            return Err(BigQueryError::InvalidResponse);
        }
        Ok(token)
    }

    fn decode(token: &str) -> Result<Self, BigQueryError> {
        if token.is_empty() || token.len() > MAX_CURSOR_BYTES {
            return Err(BigQueryError::InvalidQuery);
        }
        let bytes = URL_SAFE_NO_PAD
            .decode(token)
            .map_err(|_| BigQueryError::InvalidQuery)?;
        let cursor: Self =
            serde_json::from_slice(&bytes).map_err(|_| BigQueryError::InvalidQuery)?;
        if cursor.page_token.is_empty()
            || cursor.page_token.len() > MAX_PAGE_TOKEN_BYTES
            || cursor.next_offset > u64::MAX - MAX_PAGE_SIZE as u64
            || !valid_job_id(&cursor.job_id)
        {
            return Err(BigQueryError::InvalidQuery);
        }
        Ok(cursor)
    }
}

fn flatten_schema(
    fields: &[FieldSchema],
    parent: &str,
    depth: usize,
    output: &mut Vec<FieldSchema>,
) -> Result<(), BigQueryError> {
    if depth > 15 || output.len() > 10_000 {
        return Err(BigQueryError::InvalidResponse);
    }
    for field in fields {
        if field.name.is_empty()
            || !field
                .name
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || byte == b'_')
        {
            return Err(BigQueryError::InvalidResponse);
        }
        let name = if parent.is_empty() {
            field.name.clone()
        } else {
            format!("{parent}.{}", field.name)
        };
        if field.fields.is_empty() {
            output.push(FieldSchema {
                name,
                field_type: field.field_type.clone(),
                mode: field.mode.clone(),
                fields: Vec::new(),
            });
        } else {
            flatten_schema(&field.fields, &name, depth + 1, output)?;
        }
    }
    Ok(())
}

fn query_fingerprint<T: Hash>(value: &T) -> u64 {
    use std::hash::Hasher;
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    value.hash(&mut hasher);
    hasher.finish()
}

fn quote_field_path(value: &str) -> Result<String, BigQueryError> {
    if value.is_empty() || value.len() > 256 {
        return Err(BigQueryError::InvalidQuery);
    }
    let parts = value.split('.').collect::<Vec<_>>();
    if parts.iter().any(|part| {
        part.is_empty()
            || !part
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || byte == b'_')
    }) {
        return Err(BigQueryError::InvalidQuery);
    }
    Ok(parts
        .into_iter()
        .map(|part| format!("`{part}`"))
        .collect::<Vec<_>>()
        .join("."))
}

fn decode_rows(rows: &[Value], fields: &[FieldSchema]) -> Result<Vec<Value>, BigQueryError> {
    rows.iter()
        .map(|row| {
            let values = row
                .get("f")
                .and_then(Value::as_array)
                .ok_or(BigQueryError::InvalidResponse)?;
            if values.len() != fields.len() {
                return Err(BigQueryError::InvalidResponse);
            }
            let mut source = serde_json::Map::new();
            for (field, value) in fields.iter().zip(values) {
                let value = value.get("v").ok_or(BigQueryError::InvalidResponse)?;
                source.insert(field.name.clone(), decode_value(value, field)?);
            }
            Ok(Value::Object(source))
        })
        .collect()
}

fn decode_value(value: &Value, field: &FieldSchema) -> Result<Value, BigQueryError> {
    if value.is_null() {
        return Ok(Value::Null);
    }
    if field.mode.as_deref() == Some("REPEATED") {
        let values = value.as_array().ok_or(BigQueryError::InvalidResponse)?;
        let mut element_field = field.clone();
        element_field.mode = None;
        return values
            .iter()
            .map(|value| {
                decode_value(
                    value.get("v").ok_or(BigQueryError::InvalidResponse)?,
                    &element_field,
                )
            })
            .collect::<Result<Vec<_>, _>>()
            .map(Value::Array);
    }
    if !field.fields.is_empty() || matches!(field.field_type.as_str(), "RECORD" | "STRUCT") {
        let nested = value
            .get("f")
            .and_then(Value::as_array)
            .ok_or(BigQueryError::InvalidResponse)?;
        if nested.len() != field.fields.len() {
            return Err(BigQueryError::InvalidResponse);
        }
        let mut object = serde_json::Map::new();
        for (child, value) in field.fields.iter().zip(nested) {
            object.insert(
                child.name.clone(),
                decode_value(value.get("v").ok_or(BigQueryError::InvalidResponse)?, child)?,
            );
        }
        return Ok(Value::Object(object));
    }
    let raw = value.as_str().ok_or(BigQueryError::InvalidResponse)?;
    match field.field_type.as_str() {
        "INT64" => raw
            .parse::<i64>()
            .map(|value| json!(value))
            .map_err(|_| BigQueryError::InvalidResponse),
        "FLOAT64" => raw
            .parse::<f64>()
            .ok()
            .and_then(serde_json::Number::from_f64)
            .map(Value::Number)
            .ok_or(BigQueryError::InvalidResponse),
        "BOOL" => raw
            .parse::<bool>()
            .map(Value::Bool)
            .map_err(|_| BigQueryError::InvalidResponse),
        _ => Ok(Value::String(raw.to_owned())),
    }
}

fn valid_project_id(value: &str) -> bool {
    (6..=30).contains(&value.len())
        && value.as_bytes()[0].is_ascii_lowercase()
        && value.as_bytes()[value.len() - 1].is_ascii_alphanumeric()
        && value
            .bytes()
            .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'-')
}

fn valid_dataset_id(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 1024
        && value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || byte == b'_')
}

fn valid_table_id(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 1024
        && value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || b"_-".contains(&byte))
}

fn valid_token_uri(value: &str) -> bool {
    matches!(
        value,
        "https://oauth2.googleapis.com/token" | "https://accounts.google.com/o/oauth2/token"
    )
}

fn validate_query(query: &str) -> Result<(), BigQueryError> {
    normalize_query(query).map(|_| ())
}

fn normalize_query(query: &str) -> Result<String, BigQueryError> {
    if query.is_empty() || query.len() > MAX_QUERY_BYTES {
        return Err(BigQueryError::InvalidQuery);
    }
    let dialect = BigQueryDialect {};
    let statements = Parser::parse_sql(&dialect, query).map_err(|_| BigQueryError::InvalidQuery)?;
    if statements.len() != 1 || !matches!(statements.first(), Some(Statement::Query(_))) {
        return Err(BigQueryError::InvalidQuery);
    }
    statements
        .into_iter()
        .next()
        .map(|statement| statement.to_string())
        .ok_or(BigQueryError::InvalidQuery)
}

impl Connector for BigQueryClient {
    fn test_connection(&self) -> ConnectorFuture<'_, ()> {
        Box::pin(async move {
            let auth = self.authenticator().await?;
            let token = self.access_token(&auth).await?;
            self.test_connection_with_token(&token).await?;
            Ok(())
        })
    }

    fn indices(&self) -> ConnectorFuture<'_, Vec<IndexSummary>> {
        Box::pin(async move {
            let auth = self.authenticator().await?;
            let token = self.access_token(&auth).await?;
            Ok(self
                .list_tables_with_token(&token)
                .await?
                .into_iter()
                .map(|name| IndexSummary {
                    name,
                    document_count: None,
                    health: None,
                })
                .collect())
        })
    }

    fn fields<'a>(&'a self, index: &'a str) -> ConnectorFuture<'a, Vec<FieldSummary>> {
        Box::pin(async move {
            let auth = self.authenticator().await?;
            let token = self.access_token(&auth).await?;
            Ok(self
                .fields_with_token(index, &token)
                .await?
                .into_iter()
                .map(|field| FieldSummary {
                    is_time_field: is_time_type(&field.field_type),
                    name: field.name,
                    field_type: field.field_type,
                })
                .collect())
        })
    }

    fn search(&self, request: SearchRequest) -> ConnectorFuture<'_, Value> {
        Box::pin(async move {
            let started = Instant::now();
            let histogram_options = request
                .histogram_options()
                .map_err(|_| BigQueryError::InvalidQuery)?;
            if request.size == 0
                || request.size > MAX_PAGE_SIZE
                || (request.cursor.is_none() && request.from > 10_000)
            {
                return Err(BigQueryError::InvalidQuery.into());
            }
            self.table_name(&request.index)?;
            let query = if request.query.trim().is_empty() {
                format!("SELECT * FROM `{}` LIMIT 50", request.index)
            } else {
                request.query.trim().to_owned()
            };
            validate_query(&query)?;

            let auth = self.authenticator().await?;
            let token = self.access_token(&auth).await?;
            let schema_fields = if request.histogram_field.is_some()
                || request.histogram_breakdown_field.is_some()
            {
                self.fields_with_token(&request.index, &token).await?
            } else {
                Vec::new()
            };
            let time_field = match request.histogram_field.as_deref() {
                Some(name) => {
                    let field = schema_fields
                        .iter()
                        .find(|field| field.name == name && is_time_type(&field.field_type))
                        .ok_or(BigQueryError::InvalidQuery)?;
                    Some((name.to_owned(), field.field_type.clone()))
                }
                None if request.time_range.is_some() => {
                    return Err(BigQueryError::InvalidQuery.into())
                }
                None => None,
            };
            let breakdown_field = request
                .histogram_breakdown_field
                .as_deref()
                .map(|name| {
                    schema_fields
                        .iter()
                        .find(|field| field.name == name)
                        .map(|field| field.name.as_str())
                        .ok_or(BigQueryError::InvalidQuery)
                })
                .transpose()?;
            if breakdown_field.is_some() && time_field.is_none() {
                return Err(BigQueryError::InvalidQuery.into());
            }
            let sql = build_search_sql(&query, &request, time_field.as_ref())?;
            let fingerprint = query_fingerprint(&(
                request.index.as_str(),
                sql.as_str(),
                self.location.as_deref(),
                request.size,
            ));
            let (page, job_id, next_offset) = match request.cursor.as_deref() {
                Some(cursor) if !cursor.is_empty() => {
                    let cursor = SearchCursor::decode(cursor)?;
                    if cursor.query_hash != fingerprint {
                        return Err(BigQueryError::InvalidQuery.into());
                    }
                    let page = self
                        .get_query_results(
                            &token,
                            &cursor.job_id,
                            Some(&cursor.page_token),
                            None,
                            request.size,
                        )
                        .await?;
                    if !page.job_complete {
                        return Err(BigQueryError::Request.into());
                    }
                    let offset = cursor.next_offset;
                    (page, cursor.job_id, offset)
                }
                cursor => {
                    let start_index = if cursor.is_none() {
                        Some(request.from as u64)
                    } else {
                        None
                    };
                    let (page, job_id) = self
                        .query_job_with_token(&token, &sql, request.size, start_index)
                        .await?;
                    let offset = start_index.unwrap_or(0);
                    let job_id = job_id
                        .or_else(|| {
                            page.job_reference
                                .as_ref()
                                .and_then(|reference| reference.job_id.clone())
                        })
                        .ok_or(BigQueryError::InvalidResponse)?;
                    (page, job_id, offset)
                }
            };
            let total = page
                .total_rows
                .as_deref()
                .unwrap_or("0")
                .parse::<u64>()
                .map_err(|_| BigQueryError::InvalidResponse)?;
            let schema = page.schema.map(|schema| schema.fields).unwrap_or_default();
            let rows = page.rows.unwrap_or_default();
            let sources = decode_rows(&rows, &schema)?;
            let columns = schema
                .iter()
                .map(|field| field.name.clone())
                .collect::<Vec<_>>();
            let hits = sources
                .into_iter()
                .enumerate()
                .map(|(index, source)| {
                    json!({
                        "index": request.index,
                        "id": format!("{job_id}:{}", next_offset + index as u64),
                        "score": null,
                        "source": source,
                    })
                })
                .collect::<Vec<_>>();
            let next_cursor = page
                .page_token
                .filter(|token| !token.is_empty())
                .map(|page_token| {
                    SearchCursor {
                        job_id,
                        page_token,
                        query_hash: fingerprint,
                        next_offset: next_offset + hits.len() as u64,
                    }
                    .encode()
                })
                .transpose()?;
            let include_histogram = request.include_histogram.unwrap_or(true);
            let first_page = request.cursor.as_deref().is_none_or(str::is_empty);
            let (histogram, histogram_interval, histogram_unavailable_reason) = if include_histogram
                && first_page
            {
                match time_field.as_ref() {
                        Some((field, _)) => match self
                            .histogram_with_token(
                                &token,
                                &sql,
                                field,
                                breakdown_field,
                                request.time_range.as_ref(),
                                &histogram_options,
                            )
                            .await
                        {
                            Ok((histogram, interval)) => (histogram, interval, None),
                            Err(_) => (
                                Vec::new(),
                                None,
                                Some("BigQuery histogram aggregation failed or exceeded the 10 GiB per-query billed-bytes limit"),
                            ),
                        },
                        None => (
                            Vec::new(),
                            None,
                            Some("Select a timestamp field to build a BigQuery histogram"),
                        ),
                    }
            } else {
                (Vec::new(), None, None)
            };
            let mut response = json!({
                "took": started.elapsed().as_millis().min(u128::from(u64::MAX)) as u64,
                "timed_out": false,
                "total": total,
                "total_relation": "eq",
                "columns": columns,
                "hits": hits,
                "histogram": histogram,
            });
            if let Some(interval) = histogram_interval {
                response["histogram_interval"] = json!(interval);
            }
            if let Some(reason) = histogram_unavailable_reason {
                response["histogram_unavailable_reason"] = json!(reason);
            }
            if let Some(cursor) = next_cursor {
                response["next_cursor"] = json!(cursor);
            }
            Ok(response)
        })
    }

    fn document<'a>(&'a self, _index: &'a str, _id: &'a str) -> ConnectorFuture<'a, Value> {
        Box::pin(async { Err(ConnectorError::invalid_connection()) })
    }
}

impl From<BigQueryError> for ConnectorError {
    fn from(error: BigQueryError) -> Self {
        let (status, code, message) = match error {
            BigQueryError::InvalidConnection => (
                StatusCode::BAD_REQUEST,
                "invalid_bigquery_connection",
                "BigQuery URL, project, dataset, or service-account key is invalid",
            ),
            BigQueryError::InvalidQuery => (
                StatusCode::BAD_REQUEST,
                "invalid_bigquery_query",
                "BigQuery accepts one read-only Standard SQL query under 8192 bytes",
            ),
            BigQueryError::Authentication => (
                StatusCode::BAD_GATEWAY,
                "bigquery_authentication_failed",
                "BigQuery authentication failed; check the service-account key and permissions",
            ),
            BigQueryError::Request => (
                StatusCode::BAD_GATEWAY,
                "bigquery_unreachable",
                "Could not complete the BigQuery request; it may have timed out",
            ),
            BigQueryError::Rejected(status) => (
                if status == 404 {
                    StatusCode::NOT_FOUND
                } else if status == 400 {
                    StatusCode::BAD_REQUEST
                } else {
                    StatusCode::BAD_GATEWAY
                },
                "bigquery_rejected_request",
                "BigQuery rejected the request; check query, dataset location, and IAM permissions",
            ),
            BigQueryError::InvalidResponse => (
                StatusCode::BAD_GATEWAY,
                "bigquery_response_error",
                "BigQuery returned an unusable response",
            ),
        };
        Self {
            status,
            code,
            message: message.to_owned(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use wiremock::{matchers, Mock, MockServer, ResponseTemplate};

    #[test]
    fn read_only_query_validation_accepts_bigquery_queries_only() {
        for query in [
            "SELECT 1",
            "WITH sample AS (SELECT 1 AS value) SELECT value FROM sample",
            "SELECT `event-time` FROM `project.dataset.table-name`",
        ] {
            assert!(validate_query(query).is_ok(), "{query}");
        }
        for query in [
            "",
            "SELECT 1; SELECT 2",
            "INSERT INTO `project.dataset.table` VALUES (1)",
            "CREATE TABLE `project.dataset.table` (value INT64)",
            "SELECT 1; DROP TABLE `project.dataset.table`",
        ] {
            assert!(validate_query(query).is_err(), "{query}");
        }
        assert_eq!(normalize_query("SELECT 1;").unwrap(), "SELECT 1");
        assert!(validate_query(&"x".repeat(MAX_QUERY_BYTES + 1)).is_err());
    }

    #[test]
    fn descriptor_preserves_nested_field_paths_and_explicitly_quotes_tables() {
        let descriptor = descriptor();
        assert_eq!(descriptor.query["identifier_quote"], "");
        assert_eq!(
            descriptor.query["field_query"],
            "SELECT {field} FROM `{index}`"
        );
        assert_eq!(descriptor.query["default_query"], "SELECT * FROM `{index}`");
        assert_eq!(descriptor.query["sort_requires_projection"], true);
        assert_eq!(descriptor.query["histogram_total_from_buckets"], false);
    }

    #[test]
    fn project_dataset_table_and_token_identifiers_are_bounded() {
        assert!(valid_project_id("my-gcp-project"));
        assert!(!valid_project_id("-project"));
        assert!(!valid_project_id("UPPERCASE"));
        assert!(valid_dataset_id("analytics_2026"));
        assert!(!valid_dataset_id("analytics-prod"));
        assert!(valid_table_id("table-name_1"));
        assert!(!valid_table_id("table.name"));
        assert!(valid_token_uri("https://oauth2.googleapis.com/token"));
        assert!(!valid_token_uri("http://oauth2.googleapis.com/token"));
        assert!(!valid_token_uri("https://attacker.example/token"));
    }

    #[test]
    fn cursors_bound_page_tokens_job_ids_and_offsets() {
        let cursor = SearchCursor {
            job_id: "job_123".to_owned(),
            page_token: "next-page".to_owned(),
            query_hash: 42,
            next_offset: 100,
        };
        let token = cursor.encode().unwrap();
        let decoded = SearchCursor::decode(&token).unwrap();
        assert_eq!(decoded.job_id, cursor.job_id);
        assert_eq!(decoded.page_token, cursor.page_token);
        assert_eq!(decoded.query_hash, cursor.query_hash);
        assert_eq!(decoded.next_offset, cursor.next_offset);

        let overflow = SearchCursor {
            next_offset: u64::MAX,
            ..cursor
        };
        assert!(overflow.encode().is_err());
        assert!(SearchCursor::decode(&"x".repeat(MAX_CURSOR_BYTES + 1)).is_err());
    }

    #[tokio::test]
    async fn table_listing_follows_page_tokens_and_qualifies_table_ids() {
        let server = MockServer::start().await;
        Mock::given(matchers::method("GET"))
            .and(matchers::path(
                "/bigquery/v2/projects/my-gcp-project/datasets/analytics/tables",
            ))
            .and(matchers::query_param("maxResults", "1000"))
            .and(matchers::query_param_is_missing("pageToken"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "tables":[{"tableReference":{"tableId":"events"}}],
                "nextPageToken":"page-two"
            })))
            .expect(1)
            .mount(&server)
            .await;
        Mock::given(matchers::method("GET"))
            .and(matchers::path(
                "/bigquery/v2/projects/my-gcp-project/datasets/analytics/tables",
            ))
            .and(matchers::query_param("pageToken", "page-two"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "tables":[{"tableReference":{"tableId":"sessions"}}]
            })))
            .expect(1)
            .mount(&server)
            .await;

        let client = BigQueryClient::test_client(&format!("{}/", server.uri()));
        assert_eq!(
            client.list_tables_with_token("test-token").await.unwrap(),
            [
                "my-gcp-project.analytics.events",
                "my-gcp-project.analytics.sessions"
            ]
        );
    }

    #[tokio::test]
    async fn field_discovery_flattens_nested_leaves_and_marks_actual_time_types() {
        let server = MockServer::start().await;
        Mock::given(matchers::method("GET"))
            .and(matchers::path(
                "/bigquery/v2/projects/my-gcp-project/datasets/analytics/tables/events",
            ))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "schema":{"fields":[
                    {"name":"event","type":"RECORD","fields":[
                        {"name":"created_at","type":"TIMESTAMP"},
                        {"name":"day","type":"DATE"},
                        {"name":"message","type":"STRING"}
                    ]},
                    {"name":"labels","type":"STRING","mode":"REPEATED"}
                ]}
            })))
            .expect(1)
            .mount(&server)
            .await;

        let client = BigQueryClient::test_client(&format!("{}/", server.uri()));
        let fields = client
            .fields_with_token("my-gcp-project.analytics.events", "test-token")
            .await
            .unwrap();
        let fields = fields
            .into_iter()
            .map(|field| (field.name, field.field_type))
            .collect::<Vec<_>>();
        assert_eq!(
            fields,
            [
                ("event.created_at".to_owned(), "TIMESTAMP".to_owned()),
                ("event.day".to_owned(), "DATE".to_owned()),
                ("event.message".to_owned(), "STRING".to_owned()),
                ("labels".to_owned(), "STRING".to_owned()),
            ]
        );
        assert!(is_time_type("TIMESTAMP"));
        assert!(is_time_type("DATE"));
        assert!(!is_time_type("STRING"));
    }

    #[tokio::test]
    async fn query_jobs_apply_byte_caps_and_continue_with_native_page_tokens() {
        let server = MockServer::start().await;
        Mock::given(matchers::method("POST"))
            .and(matchers::path(
                "/bigquery/v2/projects/my-gcp-project/queries",
            ))
            .and(matchers::header("authorization", "Bearer test-token"))
            .and(matchers::body_partial_json(json!({
                "query":"SELECT 1 AS value",
                "useLegacySql":false,
                "maximumBytesBilled":MAX_BYTES_BILLED.to_string(),
                "maxResults":1
            })))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "jobReference":{"jobId":"job_123"},
                "jobComplete":true,
                "totalRows":"2",
                "schema":{"fields":[{"name":"value","type":"INT64"}]},
                "rows":[{"f":[{"v":"1"}]}],
                "pageToken":"next-page"
            })))
            .expect(1)
            .mount(&server)
            .await;
        Mock::given(matchers::method("GET"))
            .and(matchers::path(
                "/bigquery/v2/projects/my-gcp-project/queries/job_123",
            ))
            .and(matchers::query_param("pageToken", "next-page"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "jobComplete":true,
                "totalRows":"2",
                "schema":{"fields":[{"name":"value","type":"INT64"}]},
                "rows":[{"f":[{"v":"2"}]}]
            })))
            .expect(1)
            .mount(&server)
            .await;

        let client = BigQueryClient::test_client(&format!("{}/", server.uri()));
        let first = client
            .query_job_with_token("test-token", "SELECT 1 AS value", 1, None)
            .await
            .unwrap()
            .0;
        assert_eq!(first.total_rows.as_deref(), Some("2"));
        assert_eq!(first.page_token.as_deref(), Some("next-page"));
        let last = client
            .get_query_results("test-token", "job_123", Some("next-page"), None, 1)
            .await
            .unwrap();
        let sources =
            decode_rows(last.rows.as_deref().unwrap(), &last.schema.unwrap().fields).unwrap();
        assert_eq!(sources, [json!({"value":2})]);
    }

    #[test]
    fn row_conversion_preserves_nested_records_repeated_values_and_numeric_types() {
        let fields = vec![
            FieldSchema {
                name: "event".to_owned(),
                field_type: "RECORD".to_owned(),
                mode: None,
                fields: vec![FieldSchema {
                    name: "count".to_owned(),
                    field_type: "INT64".to_owned(),
                    mode: None,
                    fields: Vec::new(),
                }],
            },
            FieldSchema {
                name: "labels".to_owned(),
                field_type: "STRING".to_owned(),
                mode: Some("REPEATED".to_owned()),
                fields: Vec::new(),
            },
        ];
        let rows = vec![json!({"f":[
            {"v":{"f":[{"v":"42"}]}},
            {"v":[{"v":"api"},{"v":"prod"}]}
        ]})];
        assert_eq!(
            decode_rows(&rows, &fields).unwrap(),
            [json!({"event":{"count":42},"labels":["api","prod"]})]
        );
    }

    #[test]
    fn query_wrapper_applies_inclusive_time_filter_and_safe_sort() {
        let request: SearchRequest = serde_json::from_value(json!({
            "index":"my-gcp-project.analytics.events",
            "query":"SELECT created_at, message FROM `my-gcp-project.analytics.events`",
            "histogram_field":"created_at",
            "time_range":{"start":1000,"end":2000},
            "sort_by":"created_at",
            "sort_order":"desc"
        }))
        .unwrap();
        let sql = build_search_sql(
            &request.query,
            &request,
            Some(&("created_at".to_owned(), "TIMESTAMP".to_owned())),
        )
        .unwrap();
        assert!(sql.contains("TIMESTAMP_MILLIS(1000)"));
        assert!(sql.contains("TIMESTAMP_MILLIS(2000)"));
        assert!(sql.ends_with("ORDER BY __ig_source.`created_at` DESC"));

        let mut invalid = request;
        invalid.sort_order = Some("sideways".to_owned());
        assert!(build_search_sql(
            &invalid.query,
            &invalid,
            Some(&("created_at".to_owned(), "TIMESTAMP".to_owned()))
        )
        .is_err());
    }

    #[tokio::test]
    async fn histogram_query_returns_bucket_counts_with_shared_interval_shape() {
        let server = MockServer::start().await;
        Mock::given(matchers::method("POST"))
            .and(matchers::path(
                "/bigquery/v2/projects/my-gcp-project/queries",
            ))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "jobReference":{"jobId":"hist_1"},
                "jobComplete":true,
                "totalRows":"1",
                "schema":{"fields":[
                    {"name":"timestamp","type":"TIMESTAMP"},
                    {"name":"count","type":"INT64"}
                ]},
                "rows":[{"f":[
                    {"v":"2026-10-22 12:00:00+00:00"},
                    {"v":"3"}
                ]}]
            })))
            .expect(1)
            .mount(&server)
            .await;

        let client = BigQueryClient::test_client(&format!("{}/", server.uri()));
        let options = HistogramOptions {
            interval: HistogramInterval::parse("5m"),
            breakdown_field: None,
            time_zone: "UTC".to_owned(),
        };
        let (histogram, interval) = client
            .histogram_with_token(
                "test-token",
                "SELECT created_at FROM `my-gcp-project.analytics.events`",
                "created_at",
                None,
                None,
                &options,
            )
            .await
            .unwrap();
        assert_eq!(interval.as_deref(), Some("5m"));
        assert_eq!(histogram.len(), 1);
        assert_eq!(histogram[0]["count"], 3);
        assert_eq!(histogram[0]["timestamp"], 1_792_670_400_000_i64);
        assert_eq!(histogram[0]["label"], Value::Null);
    }

    #[test]
    fn connection_requires_google_endpoint_and_rejects_untrusted_token_urls() {
        let valid_credentials = json!({
            "type":"service_account",
            "project_id":"my-gcp-project",
            "private_key_id":"test-key",
            "private_key":"not-a-real-private-key",
            "client_email":"test@my-gcp-project.iam.gserviceaccount.com",
            "token_uri":"https://oauth2.googleapis.com/token"
        })
        .to_string();
        let valid = json!({
            "url":API_ROOT,
            "project_id":"my-gcp-project",
            "dataset_id":"analytics",
            "service_account_json":valid_credentials
        });
        let config = serde_json::from_value(valid).unwrap();
        assert!(connect(&config).is_ok());

        let invalid_credentials = json!({
            "type":"service_account",
            "private_key":"not-a-real-private-key",
            "client_email":"test@example.com",
            "token_uri":"https://attacker.example/token"
        })
        .to_string();
        let invalid = json!({
            "url":API_ROOT,
            "project_id":"my-gcp-project",
            "dataset_id":"analytics",
            "service_account_json":invalid_credentials
        });
        let config = serde_json::from_value(invalid).unwrap();
        assert!(connect(&config).is_err());
    }
}

fn is_time_type(field_type: &str) -> bool {
    matches!(
        field_type.to_ascii_uppercase().as_str(),
        "DATE" | "DATETIME" | "TIMESTAMP"
    )
}

fn build_search_sql(
    query: &str,
    request: &SearchRequest,
    time_field: Option<&(String, String)>,
) -> Result<String, BigQueryError> {
    let mut sql = format!("SELECT __ig_source.* FROM ({query}) AS __ig_source");
    if let Some(range) = request.time_range.as_ref() {
        if !range.is_valid() {
            return Err(BigQueryError::InvalidQuery);
        }
        let (field, _) = time_field.ok_or(BigQueryError::InvalidQuery)?;
        let field = quote_field_path(&field)?;
        sql.push_str(&format!(
            " WHERE TIMESTAMP(__ig_source.{field}) BETWEEN TIMESTAMP_MILLIS({}) AND TIMESTAMP_MILLIS({})",
            range.start, range.end
        ));
    }
    if let Some(field) = request.sort_by.as_deref() {
        let field = quote_field_path(field)?;
        let order = match request.sort_order.as_deref().unwrap_or("asc") {
            "asc" => "ASC",
            "desc" => "DESC",
            _ => return Err(BigQueryError::InvalidQuery),
        };
        sql.push_str(&format!(" ORDER BY __ig_source.{field} {order}"));
    } else if request
        .sort_order
        .as_deref()
        .is_some_and(|order| order != "asc")
    {
        return Err(BigQueryError::InvalidQuery);
    }
    validate_query(&sql)?;
    Ok(sql)
}

fn timestamp_bucket_expression(
    timestamp: &str,
    interval: HistogramInterval,
    time_zone: &str,
) -> String {
    let fixed_seconds = match interval.unit {
        crate::search::IntervalUnit::Millisecond => None,
        crate::search::IntervalUnit::Second => Some(i64::from(interval.count)),
        crate::search::IntervalUnit::Minute => Some(i64::from(interval.count) * 60),
        crate::search::IntervalUnit::Hour => Some(i64::from(interval.count) * 3_600),
        crate::search::IntervalUnit::Day if interval.count > 1 => {
            Some(i64::from(interval.count) * 86_400)
        }
        crate::search::IntervalUnit::Week
        | crate::search::IntervalUnit::Month
        | crate::search::IntervalUnit::Year
        | crate::search::IntervalUnit::Day => None,
    };
    if interval.unit == crate::search::IntervalUnit::Millisecond {
        let millis = interval.count;
        format!("TIMESTAMP_MILLIS(DIV(UNIX_MILLIS({timestamp}), {millis}) * {millis})")
    } else if let Some(seconds) = fixed_seconds {
        format!("TIMESTAMP_SECONDS(DIV(UNIX_SECONDS({timestamp}), {seconds}) * {seconds})")
    } else {
        let part = match interval.unit {
            crate::search::IntervalUnit::Day => "DAY",
            crate::search::IntervalUnit::Week => "WEEK",
            crate::search::IntervalUnit::Month => "MONTH",
            crate::search::IntervalUnit::Year => "YEAR",
            _ => unreachable!(),
        };
        format!("TIMESTAMP_TRUNC({timestamp}, {part}, '{time_zone}')")
    }
}

fn parse_timestamp_value(value: &Value) -> Option<i64> {
    let value = value.as_str()?;
    chrono::DateTime::parse_from_rfc3339(value)
        .ok()
        .map(|timestamp| timestamp.timestamp_millis())
        .or_else(|| {
            let value = value.replace(' ', "T");
            chrono::DateTime::parse_from_rfc3339(&value)
                .ok()
                .map(|timestamp| timestamp.timestamp_millis())
        })
}

fn valid_job_id(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 256
        && value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || b"_-".contains(&byte))
}
