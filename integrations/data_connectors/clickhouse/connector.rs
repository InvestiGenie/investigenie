use super::{ClickHouseClient, ClickHouseError, ClickHouseTimeField};
use crate::data_connectors::{
    string_setting, ConnectionConfig, Connector, ConnectorError, ConnectorFuture, FieldSummary,
    IndexSummary, InvestigationFilter, InvestigationFilterOperator,
    InvestigationGroupCountRequest, InvestigationGroupCountResult,
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
    fn investigation_filter_operators(
        &self,
        field_type: &str,
    ) -> Vec<InvestigationFilterOperator> {
        let normalized = base_clickhouse_type(field_type);
        let string_field = normalized.starts_with("string");
        if !is_investigation_scalar_type(&normalized) {
            return Vec::new();
        }
        let mut operators = vec![InvestigationFilterOperator::Eq];
        if string_field {
            operators.push(InvestigationFilterOperator::Contains);
        }
        operators.extend([
            InvestigationFilterOperator::Gt,
            InvestigationFilterOperator::Gte,
            InvestigationFilterOperator::Lt,
            InvestigationFilterOperator::Lte,
        ]);
        operators
    }
    fn investigation_supports_aggregation_field(&self, field_type: &str) -> bool {
        is_investigation_scalar_type(field_type)
    }
    fn investigation_supports_group_count(&self) -> bool {
        true
    }
    fn build_investigation_query(
        &self,
        index: &str,
        filters: &[InvestigationFilter],
    ) -> Result<String, ConnectorError> {
        let table = quote_table(index)?;
        let clauses = filters
            .iter()
            .map(|filter| {
                if !self
                    .investigation_filter_operators(&filter.field_type)
                    .contains(&filter.operator)
                {
                    return Err(ClickHouseError::InvalidQuery.into());
                }
                clickhouse_filter(filter)
            })
            .collect::<Result<Vec<_>, _>>()?;
        let where_clause = if clauses.is_empty() {
            String::new()
        } else {
            format!(" WHERE {}", clauses.join(" AND "))
        };
        Ok(format!("SELECT * FROM {table}{where_clause}"))
    }
    fn investigation_group_count(
        &self,
        request: InvestigationGroupCountRequest,
    ) -> ConnectorFuture<'_, InvestigationGroupCountResult> {
        Box::pin(async move {
            let available_fields = self.fields(&request.index).await?;
            let field_types = available_fields
                .iter()
                .cloned()
                .collect::<std::collections::BTreeMap<_, _>>();
            if request.group_by.is_empty()
                || request.group_by.len() > 3
                || !(2..=1_000_000_000).contains(&request.minimum_count)
                || !(1..=100).contains(&request.limit)
            {
                return Err(ClickHouseError::InvalidQuery.into());
            }
            for field in &request.group_by {
                let Some(field_type) = field_types.get(field) else {
                    return Err(ClickHouseError::InvalidIdentifier.into());
                };
                if !is_investigation_scalar_type(field_type) {
                    return Err(ClickHouseError::InvalidQuery.into());
                }
            }
            let mut clauses = request
                .filters
                .iter()
                .map(|filter| {
                    let Some(field_type) = field_types.get(&filter.field) else {
                        return Err(ClickHouseError::InvalidIdentifier);
                    };
                    if field_type != &filter.field_type
                        || !self
                        .investigation_filter_operators(field_type)
                        .contains(&filter.operator)
                    {
                        return Err(ClickHouseError::InvalidQuery);
                    }
                    clickhouse_filter(filter).map_err(|_| ClickHouseError::InvalidQuery)
                })
                .collect::<Result<Vec<_>, _>>()?;
            if let Some(range) = request.time_range {
                if !range.is_valid()
                    || !available_fields.iter().any(|(name, field_type)| {
                        name == &request.time_field && is_time_field(field_type)
                    })
                {
                    return Err(ClickHouseError::InvalidQuery.into());
                }
                clauses.push(format!(
                    "toUnixTimestamp64Milli(toDateTime64({}, 3, 'UTC')) BETWEEN {} AND {}",
                    super::quote_identifier(&request.time_field),
                    range.start,
                    range.end
                ));
            }
            let where_clause = if clauses.is_empty() {
                String::new()
            } else {
                format!(" WHERE {}", clauses.join(" AND "))
            };
            let group_fields = request
                .group_by
                .iter()
                .map(|field| super::quote_identifier(field))
                .collect::<Vec<_>>();
            let query = format!(
                "SELECT {}, count() AS `_ig_duplicate_count` FROM {}{where_clause} GROUP BY {} HAVING count() >= {} ORDER BY `_ig_duplicate_count` DESC LIMIT {}",
                group_fields.join(", "),
                quote_table(&request.index)?,
                group_fields.join(", "),
                request.minimum_count,
                request.limit
            );
            super::validate_read_query(&query)?;
            let response = self.execute(&query).await?;
            let columns = response
                .get("meta")
                .and_then(Value::as_array)
                .into_iter()
                .flatten()
                .filter_map(|column| column.get("name").and_then(Value::as_str))
                .map(str::to_owned)
                .collect();
            let mut rows = response
                .get("data")
                .and_then(Value::as_array)
                .cloned()
                .ok_or(ClickHouseError::InvalidResponse)?;
            let truncated = rows.len() >= request.limit;
            rows.truncate(request.limit);
            Ok(InvestigationGroupCountResult {
                query_language: "SQL".to_owned(),
                query,
                columns,
                rows,
                truncated,
            })
        })
    }
    fn document<'a>(&'a self, _index: &'a str, _id: &'a str) -> ConnectorFuture<'a, Value> {
        Box::pin(async { Err(ConnectorError::invalid_connection()) })
    }
}

fn is_investigation_scalar_type(field_type: &str) -> bool {
    let normalized = base_clickhouse_type(field_type);
    ![
        "array(",
        "map(",
        "tuple(",
        "nested",
        "object",
        "aggregatefunction",
        "json",
    ]
    .iter()
    .any(|unsupported| normalized.contains(unsupported))
}

fn base_clickhouse_type(field_type: &str) -> String {
    let mut normalized = field_type.trim().to_ascii_lowercase();
    loop {
        let Some(inner) = ["nullable(", "lowcardinality("]
            .iter()
            .find_map(|wrapper| normalized.strip_prefix(wrapper))
            .and_then(|inner| inner.strip_suffix(')'))
        else {
            return normalized;
        };
        normalized = inner.to_owned();
    }
}

fn quote_table(table: &str) -> Result<String, ConnectorError> {
    if table.is_empty()
        || !table
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || b"_.".contains(&byte))
        || table.split('.').any(str::is_empty)
    {
        return Err(ClickHouseError::InvalidIdentifier.into());
    }
    Ok(table
        .split('.')
        .map(super::quote_identifier)
        .collect::<Vec<_>>()
        .join("."))
}

fn clickhouse_filter(filter: &InvestigationFilter) -> Result<String, ConnectorError> {
    if !super::valid_identifier(&filter.field) {
        return Err(ClickHouseError::InvalidIdentifier.into());
    }
    let field = super::quote_identifier(&filter.field);
    let value = match &filter.value {
        Value::String(value) => format!("'{}'", value.replace('\'', "''")),
        Value::Bool(value) => value.to_string(),
        Value::Number(value) => value.to_string(),
        _ => return Err(ClickHouseError::InvalidQuery.into()),
    };
    Ok(match filter.operator {
        InvestigationFilterOperator::Eq => format!("{field} = {value}"),
        InvestigationFilterOperator::Contains => {
            let Some(text) = filter.value.as_str() else {
                return Err(ClickHouseError::InvalidQuery.into());
            };
            format!(
                "{field} LIKE '%{}%'",
                text.replace('\\', "\\\\")
                    .replace('%', "\\%")
                    .replace('_', "\\_")
                    .replace('\'', "''")
            )
        }
        InvestigationFilterOperator::Gt => format!("{field} > {value}"),
        InvestigationFilterOperator::Gte => format!("{field} >= {value}"),
        InvestigationFilterOperator::Lt => format!("{field} < {value}"),
        InvestigationFilterOperator::Lte => format!("{field} <= {value}"),
    })
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
    fn investigation_filters_use_connector_capabilities_and_escape_sql_values() {
        let config = serde_json::from_value(json!({
            "url":"http://localhost:8123",
            "database":"analytics"
        }))
        .unwrap();
        let connector = connect(&config).unwrap();
        assert!(connector.investigation_supports_group_count());
        assert!(connector
            .investigation_filter_operators("LowCardinality(Nullable(String))")
            .contains(&InvestigationFilterOperator::Contains));
        let query = connector
            .build_investigation_query(
                "events",
                &[InvestigationFilter {
                    field: "message".to_owned(),
                    field_type: "String".to_owned(),
                    operator: InvestigationFilterOperator::Contains,
                    value: json!("error%' OR 1=1 --"),
                }],
            )
            .unwrap();
        assert_eq!(
            query,
            "SELECT * FROM `events` WHERE `message` LIKE '%error\\%'' OR 1=1 --%'"
        );
    }

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
