use std::time::{Duration, Instant};

use crate::integrations::IntegrationDescriptor;
mod connector;
pub use connector::connect;
use reqwest::{Client, Response, Url};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use thiserror::Error;

const REQUEST_TIMEOUT: Duration = Duration::from_secs(15);
const MAX_QUERY_BYTES: usize = 8192;
const MAX_PAGE_SIZE: usize = 100;

#[derive(Clone, Copy)]
pub struct ClickHouseTimeField<'a> {
    pub name: &'a str,
    pub range: Option<&'a crate::search::TimeRange>,
}

pub fn descriptor() -> IntegrationDescriptor {
    IntegrationDescriptor {
        id: "clickhouse",
        display_name: "ClickHouse",
        query_language: "SQL",
        description: "SQL queries over databases and tables",
        connection_fields: vec![
            serde_json::json!({"name":"url","label":"URL","input_type":"url","required":true,"secret":false,"editable":true,"placeholder":"http://localhost:8123"}),
            serde_json::json!({"name":"database","label":"Database","input_type":"text","required":false,"secret":false,"editable":true,"placeholder":"default"}),
            serde_json::json!({"name":"username","label":"Username","input_type":"text","required":false,"secret":true,"editable":true,"group":"credentials","placeholder":""}),
            serde_json::json!({"name":"password","label":"Password","input_type":"password","required":false,"secret":true,"editable":true,"group":"credentials","placeholder":""}),
        ],
        query: serde_json::json!({"placeholder":"SELECT * FROM {index} LIMIT 50","default_query":"SELECT * FROM {index} LIMIT 50","field_query":"SELECT {field} FROM {index}","identifier_quote":"`","help":"Read-only SQL runs against the selected ClickHouse table","document_mode":"inline","sort_requires_projection":true,"histogram_total_from_buckets":true,"schema_label":"ClickHouse table schema"}),
    }
}

#[derive(Clone)]
pub struct ClickHouseClient {
    url: Url,
    client: Client,
    database: Option<String>,
    username: Option<String>,
    password: Option<String>,
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
    pub took: u64,
    pub columns: Vec<String>,
    pub rows: Vec<Value>,
    pub total: u64,
    pub histogram: Vec<ClickHouseHistogramBucket>,
    pub histogram_unavailable_reason: Option<&'static str>,
}

#[derive(Debug, Serialize)]
pub struct ClickHouseHistogramBucket {
    pub timestamp: i64,
    pub label: Option<String>,
    pub count: u64,
}

#[derive(Deserialize)]
struct HistogramRow {
    timestamp: String,
    count: String,
}

impl ClickHouseClient {
    pub fn new(
        raw_url: &str,
        database: Option<String>,
        username: Option<String>,
        password: Option<String>,
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
        let client = Client::builder()
            .timeout(REQUEST_TIMEOUT)
            .build()
            .map_err(|_| ClickHouseError::ClientConfiguration)?;

        Ok(Self {
            url,
            client,
            database,
            username,
            password,
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
            .into_iter()
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

    pub async fn search(
        &self,
        query: &str,
        from: usize,
        size: usize,
        time_field: Option<ClickHouseTimeField<'_>>,
        sort_by: Option<&str>,
        sort_order: Option<&str>,
    ) -> Result<ClickHouseSearchResult, ClickHouseError> {
        validate_read_query(query)?;
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
        if size == 0 || size > MAX_PAGE_SIZE || from > 10_000 {
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
            format!(" ORDER BY {} {direction}", quote_identifier(field))
        } else {
            String::new()
        };
        let bounded_query = format!(
            "SELECT * FROM ({filtered_query}){ordering} LIMIT {size} OFFSET {from} FORMAT JSON"
        );
        let started = Instant::now();
        let response = self.execute(&bounded_query).await?;
        let parsed: JsonResponse =
            serde_json::from_value(response).map_err(|_| ClickHouseError::InvalidResponse)?;
        let mut histogram = Vec::new();
        let mut histogram_unavailable_reason = None;
        if let Some(field) = histogram_field {
            if parsed.meta.iter().any(|column| column.name == field) {
                histogram = self.histogram(&filtered_query, field).await?;
            } else {
                histogram_unavailable_reason =
                    Some("The SQL result does not include the data view's time field.");
            }
        }
        let elapsed = started.elapsed().as_millis().min(u64::MAX as u128) as u64;
        Ok(ClickHouseSearchResult {
            took: elapsed,
            columns: parsed.meta.into_iter().map(|column| column.name).collect(),
            rows: parsed.data,
            total: parsed.rows,
            histogram,
            histogram_unavailable_reason,
        })
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
        response
            .data
            .into_iter()
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
                })
            })
            .collect()
    }

    async fn execute(&self, query: &str) -> Result<Value, ClickHouseError> {
        let mut request = self
            .client
            .post(self.url.clone())
            .query(&[("default_format", "JSON"), ("readonly", "1")])
            .body(query.to_owned());
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
    use super::{validate_read_query, ClickHouseClient, ClickHouseError};
    use wiremock::{matchers, Mock, MockServer, ResponseTemplate};

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
