use std::time::{Duration, Instant};

use crate::data_connectors::IntegrationDescriptor;
use crate::search::{BreakdownCount, HistogramOptions, BREAKDOWN_OTHER_KEY, MAX_BREAKDOWN_VALUES};
mod connector;
pub mod investigation;
pub use connector::connect;
use reqwest::{Client, Response, Url};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use thiserror::Error;

const REQUEST_TIMEOUT: Duration = Duration::from_secs(15);
const MAX_QUERY_BYTES: usize = 8192;
const MAX_PAGE_SIZE: usize = 100;
const MAX_CURSOR_OFFSET: u64 = i64::MAX as u64 - MAX_PAGE_SIZE as u64;

pub(super) fn cursor_offset(cursor: Option<&str>, from: usize) -> Result<u64, ClickHouseError> {
    match cursor {
        None if from <= 10_000 => Ok(from as u64),
        Some("") => Ok(0),
        Some(token) if token.len() <= 19 && token.bytes().all(|byte| byte.is_ascii_digit()) => {
            token
                .parse::<u64>()
                .ok()
                .filter(|offset| *offset <= MAX_CURSOR_OFFSET)
                .ok_or(ClickHouseError::InvalidQuery)
        }
        _ => Err(ClickHouseError::InvalidQuery),
    }
}

#[derive(Clone, Copy)]
pub struct ClickHouseTimeField<'a> {
    pub name: &'a str,
    pub range: Option<&'a crate::search::TimeRange>,
}

pub fn descriptor() -> IntegrationDescriptor {
    IntegrationDescriptor {
        id: "clickhouse",
        display_name: "ClickHouse",
        icon: "clickhouse",
        query_language: "SQL",
        description: "SQL queries over databases and tables",
        connection_fields: vec![
            serde_json::json!({"name":"url","label":"URL","input_type":"url","required":true,"secret":false,"editable":true,"placeholder":"http://localhost:8123"}),
            serde_json::json!({"name":"database","label":"Database","input_type":"text","required":false,"secret":false,"editable":true,"placeholder":"default"}),
            serde_json::json!({"name":"max_execution_time","label":"Max execution time (seconds, optional)","input_type":"text","required":false,"secret":false,"editable":true,"placeholder":"Blank uses the server default (client timeout: 15s)"}),
            serde_json::json!({"name":"username","label":"Username","input_type":"text","required":false,"secret":true,"editable":true,"group":"credentials","placeholder":""}),
            serde_json::json!({"name":"password","label":"Password","input_type":"password","required":false,"secret":true,"editable":true,"group":"credentials","placeholder":""}),
        ],
        query: serde_json::json!({"placeholder":"SELECT * FROM {index} LIMIT 50","default_query":"SELECT * FROM {index} LIMIT 50","field_query":"SELECT {field} FROM {index}","identifier_quote":"`","help":"Read-only SQL runs against the selected ClickHouse table","document_mode":"inline","sort_requires_projection":true,"histogram_total_from_buckets":true,"schema_label":"ClickHouse table schema"}),
        ..Default::default()
    }
}

#[derive(Clone)]
pub struct ClickHouseClient {
    url: Url,
    client: Client,
    database: Option<String>,
    username: Option<String>,
    password: Option<String>,
    max_execution_time: Option<u64>,
}

#[derive(Debug, Error)]
pub enum ClickHouseError {
    #[error("ClickHouse URL must be an absolute HTTP or HTTPS URL without embedded credentials, query, or fragment")]
    InvalidUrl,
    #[error("ClickHouse database or table name is invalid")]
    InvalidIdentifier,
    #[error("ClickHouse SQL must be a single read-only SELECT or WITH query under 8192 bytes")]
    InvalidQuery,
    #[error("Could not configure the ClickHouse HTTP client")]
    ClientConfiguration,
    #[error("ClickHouse max execution time must be a whole number from 1 to 600 seconds")]
    InvalidExecutionTime,
    #[error("ClickHouse request could not be completed")]
    Request,
    #[error("ClickHouse returned HTTP {status}: {detail}")]
    Rejected { status: u16, detail: String },
    #[error("ClickHouse returned an invalid JSON response")]
    InvalidResponse,
}

#[derive(Debug, Deserialize)]
struct JsonResponse {
    meta: Vec<ColumnMeta>,
    data: Vec<Value>,
    rows: u64,
}

#[derive(Debug, Deserialize)]
struct ColumnMeta {
    name: String,
}

#[derive(Debug)]
pub struct ClickHouseSearchResult {
    pub next_cursor: Option<String>,
    pub took: u64,
    pub columns: Vec<String>,
    pub rows: Vec<Value>,
    pub total: u64,
    pub histogram: Vec<ClickHouseHistogramBucket>,
    pub histogram_unavailable_reason: Option<&'static str>,
    pub histogram_interval: Option<String>,
}

#[derive(Debug, Serialize)]
pub struct ClickHouseHistogramBucket {
    pub timestamp: i64,
    pub label: Option<String>,
    pub count: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub breakdown: Option<Vec<BreakdownCount>>,
}

#[derive(Deserialize)]
struct HistogramRow {
    timestamp: String,
    count: String,
}

fn decode_histogram_rows(
    rows: Vec<Value>,
) -> Result<Vec<ClickHouseHistogramBucket>, ClickHouseError> {
    rows.into_iter()
        .map(|row| {
            let row: HistogramRow =
                serde_json::from_value(row).map_err(|_| ClickHouseError::InvalidResponse)?;
            Ok(ClickHouseHistogramBucket {
                timestamp: row
                    .timestamp
                    .parse()
                    .map_err(|_| ClickHouseError::InvalidResponse)?,
                label: None,
                count: row
                    .count
                    .parse()
                    .map_err(|_| ClickHouseError::InvalidResponse)?,
                breakdown: None,
            })
        })
        .collect()
}

#[derive(Deserialize)]
struct BreakdownRow {
    timestamp: String,
    key: String,
    count: String,
}

impl ClickHouseClient {
    #[cfg(test)]
    pub fn new(
        raw_url: &str,
        database: Option<String>,
        username: Option<String>,
        password: Option<String>,
    ) -> Result<Self, ClickHouseError> {
        Self::with_execution_limit(raw_url, database, username, password, None)
    }

    pub(super) fn with_execution_limit(
        raw_url: &str,
        database: Option<String>,
        username: Option<String>,
        password: Option<String>,
        max_execution_time: Option<&str>,
    ) -> Result<Self, ClickHouseError> {
        let url = Url::parse(raw_url).map_err(|_| ClickHouseError::InvalidUrl)?;
        if !matches!(url.scheme(), "http" | "https")
            || url.host_str().is_none()
            || !url.username().is_empty()
            || url.password().is_some()
            || url.query().is_some()
            || url.fragment().is_some()
        {
            return Err(ClickHouseError::InvalidUrl);
        }
        if database
            .as_deref()
            .is_some_and(|value| !valid_identifier(value))
        {
            return Err(ClickHouseError::InvalidIdentifier);
        }
        let username = username.filter(|value| !value.trim().is_empty());
        if username.is_none() && password.as_deref().is_some_and(|value| !value.is_empty()) {
            return Err(ClickHouseError::ClientConfiguration);
        }
        let max_execution_time = parse_max_execution_time(max_execution_time)?;
        let request_timeout = max_execution_time
            .map(|seconds| Duration::from_secs(seconds.saturating_add(5)))
            .unwrap_or(REQUEST_TIMEOUT);
        let client = Client::builder()
            .timeout(request_timeout)
            .build()
            .map_err(|_| ClickHouseError::ClientConfiguration)?;

        Ok(Self {
            url,
            client,
            database,
            username,
            password,
            max_execution_time,
        })
    }

    pub async fn test_connection(&self) -> Result<(), ClickHouseError> {
        self.execute("SELECT 1 FORMAT JSON").await.map(|_| ())
    }

    pub async fn list_tables(&self) -> Result<Vec<String>, ClickHouseError> {
        let query = if let Some(database) = self.database.as_deref() {
            format!(
                "SELECT name FROM system.tables WHERE database = '{}' ORDER BY name FORMAT JSON",
                database.replace('\'', "''")
            )
        } else {
            "SELECT name FROM system.tables WHERE database = currentDatabase() ORDER BY name FORMAT JSON".to_owned()
        };
        let result = self.execute(&query).await?;
        let rows = result
            .get("data")
            .and_then(Value::as_array)
            .ok_or(ClickHouseError::InvalidResponse)?;
        Ok(rows
            .iter()
            .filter_map(|row| row.get("name").and_then(Value::as_str).map(str::to_owned))
            .collect())
    }

    pub async fn fields(&self, table: &str) -> Result<Vec<(String, String)>, ClickHouseError> {
        if !valid_identifier(table) {
            return Err(ClickHouseError::InvalidIdentifier);
        }
        let qualified = self
            .database
            .as_deref()
            .map(|database| format!("{}.{}", quote_identifier(database), quote_identifier(table)))
            .unwrap_or_else(|| quote_identifier(table));
        let result = self
            .execute(&format!("DESCRIBE TABLE {qualified} FORMAT JSON"))
            .await?;
        let response: JsonResponse =
            serde_json::from_value(result).map_err(|_| ClickHouseError::InvalidResponse)?;
        Ok(response
            .data
            .into_iter()
            .filter_map(|row| {
                Some((
                    row.get("name")?.as_str()?.to_owned(),
                    row.get("type")?.as_str()?.to_owned(),
                ))
            })
            .collect())
    }

    #[allow(clippy::too_many_arguments)]
    pub async fn search_page_with_histogram(
        &self,
        query: &str,
        from: usize,
        size: usize,
        time_field: Option<ClickHouseTimeField<'_>>,
        sort_by: Option<&str>,
        sort_order: Option<&str>,
        cursor: Option<&str>,
        include_histogram: Option<bool>,
        histogram_options: &HistogramOptions,
    ) -> Result<ClickHouseSearchResult, ClickHouseError> {
        validate_read_query(query)?;
        let offset = cursor_offset(cursor, from)?;
        let histogram_field = time_field.map(|field| field.name);
        let filtered_query = match time_field {
            Some(ClickHouseTimeField {
                name,
                range: Some(range),
            }) => {
                if !range.is_valid() || !valid_identifier(name) {
                    return Err(ClickHouseError::InvalidQuery);
                }
                format!("SELECT * FROM ({query}) WHERE toUnixTimestamp64Milli(toDateTime64({}, 3, 'UTC')) BETWEEN {} AND {}", quote_identifier(name), range.start, range.end)
            }
            _ => query.to_owned(),
        };
        if size == 0 || size > MAX_PAGE_SIZE {
            return Err(ClickHouseError::InvalidQuery);
        }
        if sort_order.is_some_and(|order| order != "asc" && order != "desc") {
            return Err(ClickHouseError::InvalidQuery);
        }
        let ordering = if let Some(field) = sort_by {
            if !valid_identifier(field) {
                return Err(ClickHouseError::InvalidIdentifier);
            }
            let direction = if sort_order == Some("asc") {
                "ASC"
            } else {
                "DESC"
            };
            let tie = if cursor.is_some() {
                ", tuple(*) ASC"
            } else {
                ""
            };
            format!(" ORDER BY {} {direction}{tie}", quote_identifier(field))
        } else if cursor.is_some() {
            " ORDER BY ALL".to_owned()
        } else {
            String::new()
        };
        let bounded_query = format!(
            "SELECT * FROM ({filtered_query}){ordering} LIMIT {size} OFFSET {offset} FORMAT JSON"
        );
        let started = Instant::now();
        let response = self.execute(&bounded_query).await?;
        let parsed: JsonResponse =
            serde_json::from_value(response).map_err(|_| ClickHouseError::InvalidResponse)?;
        let mut histogram = Vec::new();
        let mut histogram_unavailable_reason = None;
        let mut histogram_interval = None;
        if let Some(field) = histogram_field
            .filter(|_| include_histogram.unwrap_or(true) && cursor.is_none_or(str::is_empty))
        {
            if parsed.meta.iter().any(|column| column.name == field) {
                let result = match histogram_options.interval {
                    Some(_) => {
                        let breakdown = histogram_options
                            .breakdown_field
                            .as_deref()
                            .filter(|name| parsed.meta.iter().any(|column| column.name == *name));
                        histogram_interval = histogram_options
                            .interval
                            .map(|interval| format!("{}{}", interval.count, interval.suffix()));
                        self.interval_histogram(
                            &filtered_query,
                            field,
                            breakdown,
                            histogram_options,
                        )
                        .await
                    }
                    None => {
                        let buckets = self.histogram(&filtered_query, field).await;
                        match (buckets, histogram_options.breakdown_field.as_deref()) {
                            (Ok(buckets), Some(breakdown))
                                if !buckets.is_empty()
                                    && parsed
                                        .meta
                                        .iter()
                                        .any(|column| column.name == breakdown) =>
                            {
                                let first = buckets.first().unwrap().timestamp;
                                let last = buckets.last().unwrap().timestamp;
                                let span = last.saturating_sub(first).max(1);
                                let mut options = histogram_options.clone();
                                options.interval =
                                    crate::search::HistogramInterval::automatic(span as f64);
                                histogram_interval = options.interval.map(|interval| {
                                    format!("{}{}", interval.count, interval.suffix())
                                });
                                self.interval_histogram(
                                    &filtered_query,
                                    field,
                                    Some(breakdown),
                                    &options,
                                )
                                .await
                            }
                            (result, _) => result,
                        }
                    }
                };
                match result {
                    Ok(buckets) => histogram = buckets,
                    Err(error) => {
                        let upstream_status = match error {
                            ClickHouseError::Rejected { status, .. } => Some(status),
                            _ => None,
                        };
                        tracing::warn!(
                            upstream_status,
                            "ClickHouse histogram query failed; returning rows without a histogram"
                        );
                        histogram_unavailable_reason = Some(
                            "ClickHouse could not calculate the histogram; document results are still available.",
                        );
                    }
                };
            } else {
                histogram_unavailable_reason =
                    Some("The SQL result does not include the data view's time field.");
            }
        }
        let elapsed = started.elapsed().as_millis().min(u64::MAX as u128) as u64;
        Ok(ClickHouseSearchResult {
            next_cursor: if cursor.is_some() && parsed.data.len() == size {
                Some(
                    offset
                        .checked_add(size as u64)
                        .filter(|next| *next <= MAX_CURSOR_OFFSET)
                        .ok_or(ClickHouseError::InvalidQuery)?
                        .to_string(),
                )
            } else {
                None
            },
            took: elapsed,
            columns: parsed.meta.into_iter().map(|column| column.name).collect(),
            total: if cursor.is_some() {
                offset.saturating_add(parsed.data.len() as u64)
            } else {
                parsed.rows
            },
            rows: parsed.data,
            histogram,
            histogram_unavailable_reason,
            histogram_interval,
        })
    }

    async fn interval_histogram(
        &self,
        query: &str,
        field: &str,
        breakdown_field: Option<&str>,
        options: &HistogramOptions,
    ) -> Result<Vec<ClickHouseHistogramBucket>, ClickHouseError> {
        let interval = options.interval.ok_or(ClickHouseError::InvalidQuery)?;
        if !valid_identifier(field) || breakdown_field.is_some_and(|name| !valid_identifier(name)) {
            return Err(ClickHouseError::InvalidIdentifier);
        }
        let zone = format!(
            "'{}'",
            options.time_zone.replace(['\\', '\''], "")
        );
        let field = quote_identifier(field);
        let bucket = format!(
            "toUnixTimestamp64Milli(toDateTime64(toStartOfInterval(toDateTime64({field}, 3, {zone}), INTERVAL {} {}, {zone}), 3, {zone}))",
            interval.count,
            interval.clickhouse_unit()
        );
        let source = format!("FROM ({query}) WHERE isNotNull({field})");
        let totals_query = format!(
            "SELECT toString({bucket}) AS timestamp, toString(count()) AS count {source} \
             GROUP BY timestamp ORDER BY toInt64(timestamp) LIMIT 1001 FORMAT JSON"
        );
        let response: JsonResponse = serde_json::from_value(self.execute(&totals_query).await?)
            .map_err(|_| ClickHouseError::InvalidResponse)?;
        let mut buckets = decode_histogram_rows(response.data)?;
        let Some(breakdown_field) = breakdown_field else {
            return Ok(buckets);
        };
        let breakdown = quote_identifier(breakdown_field);
        let breakdown_query = format!(
            "WITH breakdown_source AS (SELECT {bucket} AS bucket_ms, ifNull(toString({breakdown}), '(null)') AS breakdown_key {source}), \
             top_keys AS (SELECT breakdown_key FROM breakdown_source GROUP BY breakdown_key ORDER BY count() DESC, breakdown_key LIMIT {MAX_BREAKDOWN_VALUES}) \
             SELECT toString(bucket_ms) AS timestamp, breakdown_key AS key, toString(count()) AS count FROM breakdown_source \
             WHERE breakdown_key IN (SELECT breakdown_key FROM top_keys) GROUP BY bucket_ms, breakdown_key ORDER BY bucket_ms LIMIT 4004 FORMAT JSON"
        );
        let response: JsonResponse = serde_json::from_value(self.execute(&breakdown_query).await?)
            .map_err(|_| ClickHouseError::InvalidResponse)?;
        let mut rows = Vec::with_capacity(response.data.len());
        let mut key_totals = Vec::<(String, u64)>::new();
        for row in response.data {
            let row: BreakdownRow =
                serde_json::from_value(row).map_err(|_| ClickHouseError::InvalidResponse)?;
            let timestamp: i64 = row
                .timestamp
                .parse()
                .map_err(|_| ClickHouseError::InvalidResponse)?;
            let count: u64 = row
                .count
                .parse()
                .map_err(|_| ClickHouseError::InvalidResponse)?;
            match key_totals.iter_mut().find(|(key, _)| *key == row.key) {
                Some((_, total)) => *total += count,
                None => key_totals.push((row.key.clone(), count)),
            }
            rows.push((timestamp, row.key, count));
        }
        key_totals.sort_by(|left, right| right.1.cmp(&left.1).then_with(|| left.0.cmp(&right.0)));
        for bucket in &mut buckets {
            let mut counts = key_totals
                .iter()
                .map(|(key, _)| BreakdownCount {
                    key: key.clone(),
                    count: rows
                        .iter()
                        .filter(|(timestamp, row_key, _)| {
                            *timestamp == bucket.timestamp && row_key == key
                        })
                        .map(|(_, _, count)| *count)
                        .sum(),
                })
                .collect::<Vec<_>>();
            let listed: u64 = counts.iter().map(|value| value.count).sum();
            counts.push(BreakdownCount {
                key: BREAKDOWN_OTHER_KEY.to_owned(),
                count: bucket.count.saturating_sub(listed),
            });
            bucket.breakdown = Some(counts);
        }
        Ok(buckets)
    }

    async fn histogram(
        &self,
        query: &str,
        field: &str,
    ) -> Result<Vec<ClickHouseHistogramBucket>, ClickHouseError> {
        if field.is_empty() || field.len() > 255 {
            return Err(ClickHouseError::InvalidIdentifier);
        }
        let field = quote_identifier(field);
        let histogram_query = format!(
            "WITH histogram_source AS (SELECT toUnixTimestamp64Milli(toDateTime64({field}, 3, 'UTC')) AS time_ms FROM ({query}) WHERE isNotNull({field})), \
             histogram_bounds AS (SELECT greatest(toInt64(1), intDiv(max(time_ms) - min(time_ms), 60) + 1) AS interval_ms FROM histogram_source) \
             SELECT toString(intDiv(time_ms, interval_ms) * interval_ms) AS timestamp, toString(count()) AS count \
             FROM histogram_source CROSS JOIN histogram_bounds GROUP BY intDiv(time_ms, interval_ms) * interval_ms \
             ORDER BY intDiv(time_ms, interval_ms) * interval_ms LIMIT 61 FORMAT JSON"
        );
        let response: JsonResponse = serde_json::from_value(self.execute(&histogram_query).await?)
            .map_err(|_| ClickHouseError::InvalidResponse)?;
        decode_histogram_rows(response.data)
    }

    async fn execute(&self, query: &str) -> Result<Value, ClickHouseError> {
        let mut request = self
            .client
            .post(self.url.clone())
            .query(&[("default_format", "JSON"), ("readonly", "1")])
            .body(query.to_owned());
        if let Some(max_execution_time) = self.max_execution_time {
            request = request.query(&[("max_execution_time", max_execution_time)]);
        }
        if let Some(database) = self.database.as_deref() {
            request = request.query(&[("database", database)]);
        }
        if let Some(username) = self.username.as_deref() {
            request =
                request.basic_auth(username, Some(self.password.as_deref().unwrap_or_default()));
        }
        decode(request.send().await.map_err(|_| ClickHouseError::Request)?).await
    }
}

fn validate_read_query(query: &str) -> Result<(), ClickHouseError> {
    let normalized = query.trim_start();
    let first_word = normalized
        .split(|character: char| !character.is_ascii_alphabetic())
        .next()
        .unwrap_or_default();
    if query.trim().is_empty()
        || query.len() > MAX_QUERY_BYTES
        || query.contains(';')
        || !first_word.eq_ignore_ascii_case("SELECT") && !first_word.eq_ignore_ascii_case("WITH")
        || query.to_ascii_uppercase().contains(" FORMAT ")
    {
        return Err(ClickHouseError::InvalidQuery);
    }
    Ok(())
}

fn parse_max_execution_time(value: Option<&str>) -> Result<Option<u64>, ClickHouseError> {
    let Some(value) = value.map(str::trim).filter(|value| !value.is_empty()) else {
        return Ok(None);
    };
    let seconds = value
        .parse::<u64>()
        .map_err(|_| ClickHouseError::InvalidExecutionTime)?;
    if !(1..=600).contains(&seconds) {
        return Err(ClickHouseError::InvalidExecutionTime);
    }
    Ok(Some(seconds))
}

fn valid_identifier(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 255
        && value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || byte == b'_' || byte == b'.')
}

fn quote_identifier(value: &str) -> String {
    format!("`{}`", value.replace('`', "``"))
}

async fn decode(response: Response) -> Result<Value, ClickHouseError> {
    let status = response.status();
    if !status.is_success() {
        let detail = response.text().await.unwrap_or_default();
        return Err(ClickHouseError::Rejected {
            status: status.as_u16(),
            detail: detail.chars().take(400).collect(),
        });
    }
    response
        .json::<Value>()
        .await
        .map_err(|_| ClickHouseError::InvalidResponse)
}

#[cfg(test)]
mod tests {
    use super::{
        validate_read_query, ClickHouseClient, ClickHouseError, ClickHouseSearchResult,
        ClickHouseTimeField, HistogramOptions,
    };
    use wiremock::{matchers, Mock, MockServer, ResponseTemplate};

    trait SearchTestHelpers {
        async fn search(
            &self,
            query: &str,
            from: usize,
            size: usize,
            time_field: Option<ClickHouseTimeField<'_>>,
            sort_by: Option<&str>,
            sort_order: Option<&str>,
        ) -> Result<ClickHouseSearchResult, ClickHouseError>;

        #[allow(clippy::too_many_arguments)]
        async fn search_page(
            &self,
            query: &str,
            from: usize,
            size: usize,
            time_field: Option<ClickHouseTimeField<'_>>,
            sort_by: Option<&str>,
            sort_order: Option<&str>,
            cursor: Option<&str>,
            include_histogram: Option<bool>,
        ) -> Result<ClickHouseSearchResult, ClickHouseError>;
    }

    impl SearchTestHelpers for ClickHouseClient {
        async fn search(
            &self,
            query: &str,
            from: usize,
            size: usize,
            time_field: Option<ClickHouseTimeField<'_>>,
            sort_by: Option<&str>,
            sort_order: Option<&str>,
        ) -> Result<ClickHouseSearchResult, ClickHouseError> {
            self.search_page_with_histogram(
                query,
                from,
                size,
                time_field,
                sort_by,
                sort_order,
                None,
                None,
                &HistogramOptions::default(),
            )
            .await
        }

        async fn search_page(
            &self,
            query: &str,
            from: usize,
            size: usize,
            time_field: Option<ClickHouseTimeField<'_>>,
            sort_by: Option<&str>,
            sort_order: Option<&str>,
            cursor: Option<&str>,
            include_histogram: Option<bool>,
        ) -> Result<ClickHouseSearchResult, ClickHouseError> {
            self.search_page_with_histogram(
                query,
                from,
                size,
                time_field,
                sort_by,
                sort_order,
                cursor,
                include_histogram,
                &HistogramOptions::default(),
            )
            .await
        }
    }

    #[test]
    fn max_execution_time_is_optional_and_bounded() {
        assert_eq!(super::parse_max_execution_time(None).unwrap(), None);
        assert_eq!(super::parse_max_execution_time(Some(" ")).unwrap(), None);
        for value in ["1", "10", "600"] {
            assert!(super::parse_max_execution_time(Some(value)).is_ok());
        }
        for value in ["0", "601", "1.5", "invalid"] {
            assert!(matches!(
                super::parse_max_execution_time(Some(value)),
                Err(ClickHouseError::InvalidExecutionTime)
            ));
        }
    }

    #[tokio::test]
    async fn max_execution_time_is_sent_only_when_configured() {
        let server = MockServer::start().await;
        Mock::given(matchers::method("POST"))
            .and(matchers::body_string("SELECT 1 FORMAT JSON"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "meta": [], "data": [], "rows": 0
            })))
            .expect(3)
            .mount(&server)
            .await;

        for limit in [None, Some(""), Some("7")] {
            let client =
                ClickHouseClient::with_execution_limit(&server.uri(), None, None, None, limit)
                    .unwrap();
            client.test_connection().await.unwrap();
        }

        let requests = server.received_requests().await.unwrap();
        assert_eq!(requests.len(), 3);
        assert!(!requests[0].url.as_str().contains("max_execution_time"));
        assert!(!requests[1].url.as_str().contains("max_execution_time"));
        assert!(requests[2].url.as_str().contains("max_execution_time=7"));
    }

    #[tokio::test]
    async fn paginated_requests_do_not_override_readonly_profile_limits() {
        let server = MockServer::start().await;
        Mock::given(matchers::method("POST"))
            .and(matchers::query_param("readonly", "1"))
            .and(matchers::body_string(
                "SELECT * FROM (SELECT 1 AS value) ORDER BY ALL LIMIT 10 OFFSET 0 FORMAT JSON",
            ))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "meta": [{"name":"value"}], "data": [{"value":1}], "rows": 1
            })))
            .expect(1)
            .mount(&server)
            .await;
        let client =
            ClickHouseClient::with_execution_limit(&server.uri(), None, None, None, None).unwrap();
        client
            .search_page_with_histogram(
                "SELECT 1 AS value",
                0,
                10,
                None,
                None,
                None,
                Some(""),
                Some(false),
                &HistogramOptions::default(),
            )
            .await
            .unwrap();

        let requests = server.received_requests().await.unwrap();
        let query_keys = requests[0]
            .url
            .query_pairs()
            .map(|(key, _)| key.into_owned())
            .collect::<Vec<_>>();
        assert_eq!(query_keys, ["default_format", "readonly"]);
    }

    #[test]
    fn numeric_cursor_offsets_are_checked_without_the_legacy_ceiling() {
        assert_eq!(super::cursor_offset(Some(""), 90000).unwrap(), 0);
        assert_eq!(super::cursor_offset(Some("10001"), 0).unwrap(), 10001);
        assert_eq!(super::cursor_offset(None, 10000).unwrap(), 10000);
        assert!(super::cursor_offset(None, 10001).is_err());
        assert!(super::cursor_offset(Some(&super::MAX_CURSOR_OFFSET.to_string()), 0).is_ok());
        for token in [
            "-1",
            "+1",
            " 1",
            "1.0",
            "1; SELECT 1",
            "18446744073709551616",
            "9223372036854775807",
        ] {
            assert!(super::cursor_offset(Some(token), 0).is_err(), "{token}");
        }
    }

    #[tokio::test]
    async fn cursor_connector_advances_offsets_and_skips_subsequent_histograms() {
        use crate::data_connectors::Connector;
        use serde_json::{json, Value};
        let server = MockServer::start().await;
        for (offset, data) in [
            (0, json!([{"created_at":"a"},{"created_at":"b"}])),
            (2, json!([{"created_at":"c"}])),
        ] {
            let rows = data.as_array().unwrap().len();
            Mock::given(matchers::method("POST"))
                .and(matchers::query_param("readonly", "1"))
                .and(matchers::query_param("max_execution_time", "10"))
                .and(matchers::body_string(format!("SELECT * FROM (SELECT created_at FROM events) ORDER BY `created_at` ASC, tuple(*) ASC LIMIT 2 OFFSET {offset} FORMAT JSON")))
                .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                    "meta":[{"name":"created_at"}],"data":data,"rows":rows
                }))).expect(1).mount(&server).await;
        }
        Mock::given(matchers::method("POST"))
            .and(matchers::query_param("max_execution_time", "10"))
            .and(|request: &wiremock::Request| {
                String::from_utf8_lossy(&request.body).starts_with("WITH histogram_source")
            })
            .respond_with(
                ResponseTemplate::new(200).set_body_json(json!({"meta":[],"data":[],"rows":0})),
            )
            .expect(1)
            .mount(&server)
            .await;
        let client =
            ClickHouseClient::with_execution_limit(&server.uri(), None, None, None, Some("10"))
                .unwrap();
        let request = |token: &str| {
            serde_json::from_value(json!({
            "index":"events","query":"SELECT created_at FROM events","from":99999,"size":2,
            "sort_by":"created_at","sort_order":"asc","histogram_field":"created_at","cursor":token
        })).unwrap()
        };
        let first: Value = Connector::search(&client, request("")).await.unwrap();
        assert_eq!(first["next_cursor"], "2");
        assert_eq!(first["total"], 2);
        assert_eq!(first["total_relation"], "gte");
        assert_eq!(first["hits"][0]["id"], "row-1");
        let last = Connector::search(&client, request(first["next_cursor"].as_str().unwrap()))
            .await
            .unwrap();
        assert!(last.get("next_cursor").is_none());
        assert_eq!(last["total"], 3);
        assert_eq!(last["total_relation"], "eq");
        assert_eq!(last["hits"][0]["id"], "row-3");
        assert_eq!(last["histogram"], json!([]));
    }

    #[tokio::test]
    async fn all_time_breakdown_resolves_one_interval_for_every_series() {
        use crate::data_connectors::Connector;
        use serde_json::json;
        let server = MockServer::start().await;
        Mock::given(matchers::method("POST"))
            .and(|request: &wiremock::Request| {
                String::from_utf8_lossy(&request.body).starts_with("SELECT * FROM (")
            })
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "meta":[{"name":"created_at"},{"name":"service"}],
                "data":[{"created_at":"2026-10-16","service":"api"}],"rows":1
            })))
            .expect(1)
            .mount(&server)
            .await;
        Mock::given(matchers::method("POST"))
            .and(|request: &wiremock::Request| String::from_utf8_lossy(&request.body).starts_with("WITH histogram_source"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "meta":[],"data":[{"timestamp":"0","count":"4"},{"timestamp":"60000","count":"2"}],"rows":2
            }))).expect(1).mount(&server).await;
        Mock::given(matchers::method("POST"))
            .and(|request: &wiremock::Request| {
                let body = String::from_utf8_lossy(&request.body);
                body.starts_with("SELECT toString(") && body.contains("INTERVAL 5 SECOND")
            })
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "meta":[],"data":[{"timestamp":"0","count":"4"},{"timestamp":"60000","count":"2"}],"rows":2
            }))).expect(1).mount(&server).await;
        Mock::given(matchers::method("POST"))
            .and(|request: &wiremock::Request| {
                let body = String::from_utf8_lossy(&request.body);
                body.starts_with("WITH breakdown_source") && body.contains("INTERVAL 5 SECOND")
            })
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "meta":[],"data":[{"timestamp":"0","key":"api","count":"3"},{"timestamp":"60000","key":"api","count":"2"}],"rows":2
            }))).expect(1).mount(&server).await;
        let client = ClickHouseClient::new(&server.uri(), None, None, None).unwrap();
        let request = serde_json::from_value(json!({
            "index":"events","query":"SELECT created_at, service FROM events",
            "histogram_field":"created_at","histogram_breakdown_field":"service","cursor":""
        }))
        .unwrap();
        let result = Connector::search(&client, request).await.unwrap();
        assert_eq!(result["histogram_interval"], "5s");
        assert_eq!(
            result["histogram"][0]["breakdown"],
            json!([
                {"key":"api","count":3},{"key":"__other__","count":1}
            ])
        );
    }

    #[tokio::test]
    async fn interval_histograms_bucket_in_timezone_and_break_down_top_values() {
        use crate::data_connectors::Connector;
        use serde_json::{json, Value};
        let server = MockServer::start().await;
        Mock::given(matchers::method("POST"))
            .and(|request: &wiremock::Request| {
                String::from_utf8_lossy(&request.body).starts_with("SELECT * FROM (")
            })
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "meta":[{"name":"created_at"},{"name":"service"}],
                "data":[{"created_at":"2026-10-10 00:00:00","service":"api"}],"rows":1
            })))
            .expect(1)
            .mount(&server)
            .await;
        Mock::given(matchers::method("POST"))
            .and(|request: &wiremock::Request| {
                let body = String::from_utf8_lossy(&request.body);
                body.starts_with("SELECT toString(toUnixTimestamp64Milli(")
                    && body.contains("INTERVAL 1 MINUTE, 'Europe/Berlin'")
            })
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "meta":[],"rows":2,
                "data":[{"timestamp":"0","count":"6"},{"timestamp":"60000","count":"2"}]
            })))
            .expect(1)
            .mount(&server)
            .await;
        Mock::given(matchers::method("POST"))
            .and(|request: &wiremock::Request| {
                let body = String::from_utf8_lossy(&request.body);
                body.starts_with("WITH breakdown_source") && body.contains("toString(`service`)")
            })
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "meta":[],"rows":3,
                "data":[
                    {"timestamp":"0","key":"web","count":"1"},
                    {"timestamp":"0","key":"api","count":"4"},
                    {"timestamp":"60000","key":"api","count":"2"}
                ]
            })))
            .expect(1)
            .mount(&server)
            .await;
        let client = ClickHouseClient::new(&server.uri(), None, None, None).unwrap();
        let request = serde_json::from_value(json!({
            "index":"events","query":"SELECT created_at, service FROM events","histogram_field":"created_at",
            "histogram_interval":"1m","histogram_breakdown_field":"service","time_zone":"Europe/Berlin",
            "time_range":{"start":0,"end":120000},"cursor":""
        }))
        .unwrap();
        let result: Value = Connector::search(&client, request).await.unwrap();
        assert_eq!(result["histogram_interval"], "1m");
        assert_eq!(
            result["histogram"][0]["breakdown"],
            json!([{"key":"api","count":4},{"key":"web","count":1},{"key":"__other__","count":1}])
        );
        assert_eq!(
            result["histogram"][1]["breakdown"],
            json!([{"key":"api","count":2},{"key":"web","count":0},{"key":"__other__","count":0}])
        );
    }

    #[tokio::test]
    async fn cursor_search_passes_deep_offsets_and_histogram_opt_out() {
        use serde_json::json;
        let server = MockServer::start().await;
        for (token, offset) in [("", 0), ("10001", 10001)] {
            Mock::given(matchers::method("POST"))
                .and(matchers::body_string(format!("SELECT * FROM (SELECT created_at FROM events) ORDER BY ALL LIMIT 1 OFFSET {offset} FORMAT JSON")))
                .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                    "meta":[{"name":"created_at"}],"data":[{"created_at":"2026-10-10"}],"rows":1
                }))).expect(1).mount(&server).await;
            let client = ClickHouseClient::new(&server.uri(), None, None, None).unwrap();
            let result = client
                .search_page(
                    "SELECT created_at FROM events",
                    usize::MAX,
                    1,
                    Some(super::ClickHouseTimeField {
                        name: "created_at",
                        range: None,
                    }),
                    None,
                    None,
                    Some(token),
                    Some(false),
                )
                .await
                .unwrap();
            assert_eq!(
                result.next_cursor.as_deref(),
                Some((offset + 1).to_string().as_str())
            );
            assert!(result.histogram.is_empty());
        }
    }

    #[tokio::test]
    async fn cursor_search_rejects_invalid_offsets_and_sizes_before_io() {
        let server = MockServer::start().await;
        let client = ClickHouseClient::new(&server.uri(), None, None, None).unwrap();
        for (token, size) in [("invalid", 1), ("", 101), ("", 0)] {
            assert!(client
                .search_page("SELECT 1", 0, size, None, None, None, Some(token), None)
                .await
                .is_err());
        }
        assert!(client
            .search("SELECT 1", 10001, 1, None, None, None)
            .await
            .is_err());
        assert!(server.received_requests().await.unwrap().is_empty());
    }

    #[test]
    fn read_query_accepts_select_and_with_only() {
        assert!(validate_read_query("SELECT count() FROM events").is_ok());
        assert!(validate_read_query("WITH 1 AS value SELECT value").is_ok());
        assert!(matches!(
            validate_read_query("DROP TABLE events"),
            Err(ClickHouseError::InvalidQuery)
        ));
        assert!(matches!(
            validate_read_query("SELECT 1; DROP TABLE events"),
            Err(ClickHouseError::InvalidQuery)
        ));
    }

    #[tokio::test]
    async fn test_connection_authenticates_with_an_empty_password() {
        let server = MockServer::start().await;
        Mock::given(matchers::method("POST"))
            .and(matchers::header("authorization", "Basic ZXhwbG9yZXI6"))
            .and(matchers::query_param("readonly", "1"))
            .and(matchers::body_string("SELECT 1 FORMAT JSON"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "meta": [{"name": "1", "type": "UInt8"}],
                "data": [{"1": 1}],
                "rows": 1
            })))
            .expect(2)
            .mount(&server)
            .await;

        for password in [None, Some(String::new())] {
            let client =
                ClickHouseClient::new(&server.uri(), None, Some("explorer".to_owned()), password)
                    .expect("username with an empty password is valid");
            client
                .test_connection()
                .await
                .expect("empty-password authentication succeeds");
        }
    }

    #[test]
    fn password_without_username_is_rejected() {
        assert!(matches!(
            ClickHouseClient::new(
                "https://example.com",
                None,
                None,
                Some("test-password".to_owned())
            ),
            Err(ClickHouseError::ClientConfiguration)
        ));
    }

    #[tokio::test]
    async fn search_histogram_counts_matching_rows_beyond_the_current_page() {
        let server = MockServer::start().await;
        Mock::given(matchers::method("POST"))
            .and(matchers::body_string("SELECT * FROM (SELECT created_at FROM events WHERE status = 200) LIMIT 10 OFFSET 20 FORMAT JSON"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "meta": [{"name": "created_at", "type": "DateTime"}],
                "data": [{"created_at": "2026-10-08 00:00:00"}], "rows": 1
            })))
            .expect(1).mount(&server).await;
        Mock::given(matchers::method("POST"))
            .and(matchers::query_param("readonly", "1"))
            .and(|request: &wiremock::Request| {
                let body = String::from_utf8_lossy(&request.body);
                body.contains("FROM (SELECT created_at FROM events WHERE status = 200) WHERE isNotNull(`created_at`)")
                    && body.ends_with("LIMIT 61 FORMAT JSON")
            })
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "meta": [{"name": "timestamp", "type": "String"}, {"name": "count", "type": "String"}],
                "data": [{"timestamp": "1791417600000", "count": "250"}], "rows": 1
            })))
            .expect(1).mount(&server).await;
        let client = ClickHouseClient::new(&server.uri(), None, None, None).expect("valid client");
        let result = client
            .search(
                "SELECT created_at FROM events WHERE status = 200",
                20,
                10,
                Some(super::ClickHouseTimeField {
                    name: "created_at",
                    range: None,
                }),
                None,
                None,
            )
            .await
            .expect("rows and histogram load");
        assert_eq!(result.histogram[0].timestamp, 1_791_417_600_000);
        assert_eq!(result.histogram[0].count, 250);
        assert_eq!(result.rows.len(), 1);
    }

    #[tokio::test]
    async fn search_without_time_field_keeps_rows_and_explains_missing_histogram() {
        let server = MockServer::start().await;
        Mock::given(matchers::method("POST"))
            .and(matchers::body_string(
                "SELECT * FROM (SELECT count() AS total FROM events) LIMIT 10 OFFSET 0 FORMAT JSON",
            ))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "meta": [{"name": "total", "type": "UInt64"}],
                "data": [{"total": "250"}], "rows": 1
            })))
            .expect(1)
            .mount(&server)
            .await;
        let client = ClickHouseClient::new(&server.uri(), None, None, None).expect("valid client");
        let result = client
            .search(
                "SELECT count() AS total FROM events",
                0,
                10,
                Some(super::ClickHouseTimeField {
                    name: "created_at",
                    range: None,
                }),
                None,
                None,
            )
            .await
            .expect("row-only SQL remains supported");
        assert!(result.histogram.is_empty());
        assert!(result.histogram_unavailable_reason.is_some());
        assert_eq!(result.rows.len(), 1);
    }

    #[tokio::test]
    async fn histogram_server_error_does_not_discard_successful_rows() {
        let server = MockServer::start().await;
        Mock::given(matchers::method("POST"))
            .and(matchers::body_string(
                "SELECT * FROM (SELECT * FROM actors) ORDER BY ALL LIMIT 50 OFFSET 0 FORMAT JSON",
            ))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "meta": [{"name":"created_at","type":"DateTime"},{"name":"name","type":"String"}],
                "data": [{"created_at":"2026-10-14 12:00:00","name":"Ada"}],
                "rows": 1
            })))
            .expect(1)
            .mount(&server)
            .await;
        Mock::given(matchers::method("POST"))
            .and(|request: &wiremock::Request| {
                String::from_utf8_lossy(&request.body).starts_with("WITH histogram_source")
            })
            .respond_with(ResponseTemplate::new(500).set_body_string("histogram conversion failed"))
            .expect(1)
            .mount(&server)
            .await;

        let client = ClickHouseClient::new(&server.uri(), None, None, None).unwrap();
        let result = client
            .search_page_with_histogram(
                "SELECT * FROM actors",
                0,
                50,
                Some(super::ClickHouseTimeField {
                    name: "created_at",
                    range: None,
                }),
                None,
                None,
                Some(""),
                Some(true),
                &super::HistogramOptions::default(),
            )
            .await
            .unwrap();

        assert_eq!(result.rows.len(), 1);
        assert!(result.histogram.is_empty());
        assert!(result
            .histogram_unavailable_reason
            .unwrap()
            .contains("document results are still available"));
    }

    #[tokio::test]
    async fn search_applies_the_same_time_window_to_rows_and_histogram() {
        let server = MockServer::start().await;
        let filtered = "SELECT * FROM (SELECT created_at FROM events) WHERE toUnixTimestamp64Milli(toDateTime64(`created_at`, 3, 'UTC')) BETWEEN 1000 AND 2000";
        Mock::given(matchers::method("POST"))
            .and(matchers::body_string(format!("SELECT * FROM ({filtered}) ORDER BY `created_at` ASC LIMIT 10 OFFSET 0 FORMAT JSON")))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "meta": [{"name": "created_at", "type": "DateTime64(3)"}], "data": [], "rows": 0
            })))
            .expect(1).mount(&server).await;
        Mock::given(matchers::method("POST"))
            .and(move |request: &wiremock::Request| {
                let body = String::from_utf8_lossy(&request.body);
                body.starts_with("WITH histogram_source") && body.contains(filtered)
            })
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_json(serde_json::json!({"meta": [], "data": [], "rows": 0})),
            )
            .expect(1)
            .mount(&server)
            .await;
        let client = ClickHouseClient::new(&server.uri(), None, None, None).expect("valid client");
        let range = crate::search::TimeRange {
            start: 1000,
            end: 2000,
        };
        let result = client
            .search(
                "SELECT created_at FROM events",
                0,
                10,
                Some(super::ClickHouseTimeField {
                    name: "created_at",
                    range: Some(&range),
                }),
                Some("created_at"),
                Some("asc"),
            )
            .await
            .expect("time filtered search succeeds");
        assert!(result.histogram_unavailable_reason.is_none());
        let invalid = crate::search::TimeRange {
            start: 2000,
            end: 1000,
        };
        assert!(client
            .search(
                "SELECT created_at FROM events",
                0,
                10,
                Some(super::ClickHouseTimeField {
                    name: "created_at",
                    range: Some(&invalid)
                }),
                None,
                None
            )
            .await
            .is_err());
    }

    #[tokio::test]
    async fn search_orders_columns_before_pagination() {
        let server = MockServer::start().await;
        for direction in ["ASC", "DESC"] {
            Mock::given(matchers::method("POST"))
                .and(matchers::query_param("readonly", "1"))
                .and(matchers::body_string(format!("SELECT * FROM (SELECT value FROM events) ORDER BY `value` {direction} LIMIT 10 OFFSET 20 FORMAT JSON")))
                .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                    "meta": [{"name": "value", "type": "UInt64"}], "data": [{"value": 42}], "rows": 1
                })))
                .expect(1).mount(&server).await;
        }
        let client = ClickHouseClient::new(&server.uri(), None, None, None).expect("valid client");
        for direction in ["asc", "desc"] {
            let result = client
                .search(
                    "SELECT value FROM events",
                    20,
                    10,
                    None,
                    Some("value"),
                    Some(direction),
                )
                .await
                .expect("sorted query succeeds");
            assert_eq!(result.rows.len(), 1);
        }
    }

    #[tokio::test]
    async fn search_rejects_invalid_sort_fields_and_directions_before_requesting() {
        let server = MockServer::start().await;
        let client = ClickHouseClient::new(&server.uri(), None, None, None).expect("valid client");
        assert!(client
            .search(
                "SELECT 1",
                0,
                10,
                None,
                Some("value` DESC; DROP TABLE events"),
                Some("asc")
            )
            .await
            .is_err());
        assert!(client
            .search("SELECT 1", 0, 10, None, Some("value"), Some("invalid"))
            .await
            .is_err());
        assert!(server
            .received_requests()
            .await
            .expect("mock requests available")
            .is_empty());
    }

    #[tokio::test]
    async fn search_wraps_sql_as_read_only_bounded_json_request() {
        let server = MockServer::start().await;
        Mock::given(matchers::method("POST"))
            .and(matchers::query_param("readonly", "1"))
            .and(matchers::query_param("database", "analytics"))
            .and(matchers::body_string(
                "SELECT * FROM (SELECT 1 AS value) LIMIT 10 OFFSET 0 FORMAT JSON",
            ))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "meta": [{"name": "value", "type": "UInt8"}],
                "data": [{"value": 1}],
                "rows": 1
            })))
            .mount(&server)
            .await;

        let client = ClickHouseClient::new(&server.uri(), Some("analytics".to_owned()), None, None)
            .expect("mock server URL is valid");
        let result = client
            .search("SELECT 1 AS value", 0, 10, None, None, None)
            .await
            .expect("mocked query succeeds");
        assert_eq!(result.columns, ["value"]);
        assert_eq!(result.rows.len(), 1);
        assert_eq!(result.total, 1);
    }
}
