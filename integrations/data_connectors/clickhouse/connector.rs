use super::{ClickHouseClient, ClickHouseError, ClickHouseTimeField};
use crate::data_connectors::{
    string_setting, ConnectionConfig, Connector, ConnectorError, ConnectorFuture, FieldSummary,
    IndexSummary,
};
use crate::search::SearchRequest;
use axum::http::StatusCode;
use serde_json::{json, Value};
use std::sync::Arc;

pub fn connect(config: &ConnectionConfig) -> Result<Arc<dyn Connector>, ConnectorError> {
    let url = string_setting(config, "url")?.ok_or_else(ConnectorError::invalid_connection)?;
    let max_execution_time = string_setting(config, "max_execution_time")?;
    Ok(Arc::new(ClickHouseClient::with_execution_limit(
        &url,
        string_setting(config, "database")?,
        string_setting(config, "username")?,
        string_setting(config, "password")?,
        max_execution_time.as_deref(),
    )?))
}

pub fn is_time_field(field_type: &str) -> bool {
    let normalized = field_type.to_ascii_lowercase();
    let field_type = normalized
        .strip_prefix("nullable(")
        .and_then(|inner| inner.strip_suffix(')'))
        .unwrap_or(&normalized);
    matches!(field_type, "date" | "date32" | "datetime" | "datetime64")
        || (field_type.starts_with("datetime(") && field_type.ends_with(')'))
        || (field_type.starts_with("datetime64(") && field_type.ends_with(')'))
}

impl Connector for ClickHouseClient {
    fn test_connection(&self) -> ConnectorFuture<'_, ()> {
        Box::pin(async move { Ok(self.test_connection().await?) })
    }
    fn indices(&self) -> ConnectorFuture<'_, Vec<IndexSummary>> {
        Box::pin(async move {
            Ok(self
                .list_tables()
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
            Ok(self
                .fields(index)
                .await?
                .into_iter()
                .map(|(name, field_type)| FieldSummary {
                    is_time_field: is_time_field(&field_type),
                    name,
                    field_type,
                })
                .collect())
        })
    }
    fn search(&self, request: SearchRequest) -> ConnectorFuture<'_, Value> {
        Box::pin(async move {
            let row_offset = super::cursor_offset(request.cursor.as_deref(), request.from)?;
            let histogram_options = request
                .histogram_options()
                .map_err(|_| ClickHouseError::InvalidQuery)?;
            let result = self
                .search_page_with_histogram(
                    &request.query,
                    request.from,
                    request.size,
                    request
                        .histogram_field
                        .as_deref()
                        .map(|name| ClickHouseTimeField {
                            name,
                            range: request.time_range.as_ref(),
                        }),
                    request.sort_by.as_deref(),
                    request.sort_order.as_deref(),
                    request.cursor.as_deref(),
                    request.include_histogram,
                    &histogram_options,
                )
                .await?;
            let hits: Vec<Value> = result.rows.into_iter().enumerate().map(|(offset, source)| json!({"index":request.index,"id":format!("row-{}",row_offset + offset as u64 + 1),"score":null,"source":source})).collect();
            let relation = if result.next_cursor.is_some() {
                "gte"
            } else {
                "eq"
            };
            let mut response = json!({"took":result.took,"timed_out":false,"total":result.total,"total_relation":relation,"columns":result.columns,"hits":hits,"histogram":result.histogram,"histogram_unavailable_reason":result.histogram_unavailable_reason});
            if let Some(interval) = result.histogram_interval {
                response["histogram_interval"] = json!(interval);
            }
            if let Some(cursor) = result.next_cursor {
                response["next_cursor"] = json!(cursor);
            }
            Ok(response)
        })
    }
    fn document<'a>(&'a self, _index: &'a str, _id: &'a str) -> ConnectorFuture<'a, Value> {
        Box::pin(async { Err(ConnectorError::invalid_connection()) })
    }
}

fn rejected_limit_setting(detail: &str) -> Option<&'static str> {
    let detail = detail.to_ascii_lowercase();
    let mentions_read_only = ["readonly", "read-only", "read only"]
        .iter()
        .any(|term| detail.contains(term));
    let denies_change = [
        "cannot modify",
        "not allowed",
        "not permitted",
        "cannot be changed",
    ]
    .iter()
    .any(|term| detail.contains(term));
    if !mentions_read_only || !denies_change {
        return None;
    }
    detail
        .contains("max_execution_time")
        .then_some("max_execution_time")
}

fn clickhouse_error_code(detail: &str) -> Option<u32> {
    if let Ok(body) = serde_json::from_str::<Value>(detail) {
        for key in ["code", "exception_code"] {
            if let Some(code) = body.get(key).and_then(Value::as_u64) {
                if let Ok(code) = u32::try_from(code) {
                    return Some(code);
                }
            }
        }
    }
    let code = detail.split_once("Code:")?.1.trim_start();
    let digits: String = code.chars().take_while(char::is_ascii_digit).collect();
    digits.parse().ok()
}

impl From<ClickHouseError> for ConnectorError {
    fn from(error: ClickHouseError) -> Self {
        let (status, code, message) = match error {
            ClickHouseError::InvalidUrl
            | ClickHouseError::InvalidIdentifier
            | ClickHouseError::InvalidQuery => (
                StatusCode::BAD_REQUEST,
                "invalid_clickhouse_request",
                "ClickHouse URL, identifier, or read-only SQL is invalid".to_owned(),
            ),
            ClickHouseError::InvalidExecutionTime => (
                StatusCode::BAD_REQUEST,
                "invalid_clickhouse_execution_time",
                "Max execution time must be a whole number from 1 to 600 seconds".to_owned(),
            ),
            ClickHouseError::Rejected { status, detail } => {
                let api_status = match status {
                    400 => StatusCode::BAD_REQUEST,
                    404 => StatusCode::NOT_FOUND,
                    _ => StatusCode::BAD_GATEWAY,
                };
                if let Some(setting) = rejected_limit_setting(&detail) {
                    return Self {
                        status: api_status,
                        code: "clickhouse_limit_setting_denied",
                        message: format!("ClickHouse denied the `{setting}` query limit in read-only mode. Allow this setting for read-only queries in the ClickHouse user profile."),
                    };
                }
                let (code, message) = match status {
                    401 | 403 => (
                        "clickhouse_access_denied",
                        format!("ClickHouse denied access (HTTP {status}); check the credentials and user grants"),
                    ),
                    429 => (
                        "clickhouse_rate_limited",
                        "ClickHouse rate-limited the request (HTTP 429); retry after reducing request frequency".to_owned(),
                    ),
                    500..=599 => (
                        "clickhouse_server_error",
                        match clickhouse_error_code(&detail) {
                            Some(code) => format!("ClickHouse returned a server error (HTTP {status}, error code {code}); check server logs and query limits"),
                            None => format!("ClickHouse returned a server error (HTTP {status}); check server logs and query limits"),
                        },
                    ),
                    _ => (
                        "clickhouse_rejected_request",
                        format!("ClickHouse rejected the request (HTTP {status})"),
                    ),
                };
                (api_status, code, message)
            }
            ClickHouseError::Request => (
                StatusCode::BAD_GATEWAY,
                "clickhouse_unreachable",
                "Could not complete the ClickHouse request; it may have timed out".to_owned(),
            ),
            ClickHouseError::ClientConfiguration | ClickHouseError::InvalidResponse => (
                StatusCode::BAD_GATEWAY,
                "clickhouse_response_error",
                "ClickHouse returned an unusable response".to_owned(),
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

    #[test]
    fn descriptor_exposes_optional_execution_time_setting() {
        let field = crate::data_connectors::clickhouse::descriptor()
            .connection_fields
            .into_iter()
            .find(|field| field["name"] == "max_execution_time")
            .unwrap();
        assert_eq!(field["required"], false);
        assert_eq!(field["secret"], false);
        assert_eq!(field["editable"], true);
    }

    #[test]
    fn rejected_errors_preserve_codes_and_status_without_upstream_secrets() {
        for (upstream, expected) in [
            (400, StatusCode::BAD_REQUEST),
            (404, StatusCode::NOT_FOUND),
            (401, StatusCode::BAD_GATEWAY),
            (403, StatusCode::BAD_GATEWAY),
            (429, StatusCode::BAD_GATEWAY),
            (500, StatusCode::BAD_GATEWAY),
        ] {
            let error: ConnectorError = ClickHouseError::Rejected {
                status: upstream,
                detail: "password=secret".to_owned(),
            }
            .into();
            assert_eq!(error.status, expected);
            assert_eq!(
                error.code,
                match upstream {
                    401 | 403 => "clickhouse_access_denied",
                    429 => "clickhouse_rate_limited",
                    500..=599 => "clickhouse_server_error",
                    _ => "clickhouse_rejected_request",
                }
            );
            assert!(!error.message.contains("secret"));
        }
    }

    #[test]
    fn readonly_execution_time_errors_name_only_the_rejected_setting() {
        let error: ConnectorError = ClickHouseError::Rejected {
            status: 500,
            detail: "Code: 164. Cannot modify max_execution_time in readonly mode; password=secret"
                .to_owned(),
        }
        .into();
        assert_eq!(error.code, "clickhouse_limit_setting_denied");
        assert!(error.message.contains("max_execution_time"));
        assert!(!error.message.contains("password"));

        let error: ConnectorError = ClickHouseError::Rejected {
            status: 500,
            detail: "Code: 241. Memory limit exceeded".to_owned(),
        }
        .into();
        assert!(error.message.contains("error code 241"));
    }
    #[test]
    fn time_fields_accept_dates_and_timestamps_only() {
        for field_type in [
            "Date",
            "Date32",
            "DateTime('UTC')",
            "Nullable(Date)",
            "Nullable(DateTime64(3, 'UTC'))",
        ] {
            assert!(is_time_field(field_type), "{field_type}");
        }
        for field_type in [
            "keyword",
            "UInt64",
            "String",
            "datetime_invalid",
            "Array(DateTime)",
        ] {
            assert!(!is_time_field(field_type), "{field_type}");
        }
    }
}
