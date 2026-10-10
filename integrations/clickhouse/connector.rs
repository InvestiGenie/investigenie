use super::{ClickHouseClient, ClickHouseError, ClickHouseTimeField};
use crate::integrations::{
    string_setting, ConnectionConfig, Connector, ConnectorError, ConnectorFuture, FieldSummary,
    IndexSummary,
};
use crate::search::SearchRequest;
use axum::http::StatusCode;
use serde_json::{json, Value};
use std::sync::Arc;

pub fn connect(config: &ConnectionConfig) -> Result<Arc<dyn Connector>, ConnectorError> {
    let url = string_setting(config, "url")?.ok_or_else(ConnectorError::invalid_connection)?;
    Ok(Arc::new(ClickHouseClient::new(
        &url,
        string_setting(config, "database")?,
        string_setting(config, "username")?,
        string_setting(config, "password")?,
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
            let result = self
                .search_page(
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
                )
                .await?;
            let hits: Vec<Value> = result.rows.into_iter().enumerate().map(|(offset, source)| json!({"index":request.index,"id":format!("row-{}",row_offset + offset as u64 + 1),"score":null,"source":source})).collect();
            let relation = if result.next_cursor.is_some() { "gte" } else { "eq" };
            let mut response = json!({"took":result.took,"timed_out":false,"total":result.total,"total_relation":relation,"columns":result.columns,"hits":hits,"histogram":result.histogram,"histogram_unavailable_reason":result.histogram_unavailable_reason});
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

impl From<ClickHouseError> for ConnectorError {
    fn from(error: ClickHouseError) -> Self {
        let (status, code, message) = match error {
            ClickHouseError::InvalidUrl
            | ClickHouseError::InvalidIdentifier
            | ClickHouseError::InvalidQuery => (
                StatusCode::BAD_REQUEST,
                "invalid_clickhouse_request",
                "ClickHouse URL, identifier, or read-only SQL is invalid",
            ),
            ClickHouseError::Rejected { status, .. } => (
                match status {
                    404 => StatusCode::NOT_FOUND,
                    400 => StatusCode::BAD_REQUEST,
                    _ => StatusCode::BAD_GATEWAY,
                },
                "clickhouse_rejected_request",
                "ClickHouse rejected the request",
            ),
            ClickHouseError::Request => (
                StatusCode::BAD_GATEWAY,
                "clickhouse_unreachable",
                "Could not complete the ClickHouse request; it may have timed out",
            ),
            ClickHouseError::ClientConfiguration | ClickHouseError::InvalidResponse => (
                StatusCode::BAD_GATEWAY,
                "clickhouse_response_error",
                "ClickHouse returned an unusable response",
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
    fn rejected_errors_preserve_codes_and_status_without_upstream_secrets() {
        for (upstream, expected) in [
            (400, StatusCode::BAD_REQUEST),
            (404, StatusCode::NOT_FOUND),
            (403, StatusCode::BAD_GATEWAY),
            (500, StatusCode::BAD_GATEWAY),
        ] {
            let error: ConnectorError = ClickHouseError::Rejected {
                status: upstream,
                detail: "password=secret".to_owned(),
            }
            .into();
            assert_eq!(error.status, expected);
            assert_eq!(error.code, "clickhouse_rejected_request");
            assert!(!error.message.contains("secret"));
        }
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
