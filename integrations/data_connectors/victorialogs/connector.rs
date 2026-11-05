use super::{
    VictoriaLogsClient, VictoriaLogsError, VictoriaLogsSettings,
};
use crate::data_connectors::{
    basic_credentials, string_setting, ConnectionConfig, Connector, ConnectorError,
    ConnectorFuture, FieldSummary, IndexSummary, InvestigationFilter,
    InvestigationFilterOperator,
};
use crate::search::SearchRequest;
use axum::http::StatusCode;
use serde_json::{json, Value};
use std::sync::Arc;

pub fn connect(config: &ConnectionConfig) -> Result<Arc<dyn Connector>, ConnectorError> {
    let raw_url = string_setting(config, "url")?.ok_or_else(ConnectorError::invalid_connection)?;
    let url = url::Url::parse(&raw_url).map_err(|_| ConnectorError::invalid_connection())?;
    let (username, password) = basic_credentials(config)?.unzip();
    let account_id = string_setting(config, "account_id")?;
    let project_id = string_setting(config, "project_id")?;
    Ok(Arc::new(VictoriaLogsClient::new(VictoriaLogsSettings {
        url,
        username,
        password,
        account_id,
        project_id,
    })?))
}

impl Connector for VictoriaLogsClient {
    fn public_configuration(&self) -> ConnectionConfig {
        let mut configuration = ConnectionConfig::new();
        if let Some(account_id) = self.account_id.as_deref() {
            configuration.insert("account_id".to_owned(), json!(account_id));
        }
        if let Some(project_id) = self.project_id.as_deref() {
            configuration.insert("project_id".to_owned(), json!(project_id));
        }
        configuration
    }

    fn test_connection(&self) -> ConnectorFuture<'_, ()> {
        Box::pin(async move { Ok(VictoriaLogsClient::test_connection(self).await?) })
    }

    fn indices(&self) -> ConnectorFuture<'_, Vec<IndexSummary>> {
        Box::pin(async {
            Ok(vec![IndexSummary {
                name: "logs".to_owned(),
                document_count: None,
                health: None,
            }])
        })
    }

    fn fields<'a>(&'a self, index: &'a str) -> ConnectorFuture<'a, Vec<FieldSummary>> {
        Box::pin(async move { Ok(VictoriaLogsClient::fields(self, index).await?) })
    }

    fn search(&self, request: SearchRequest) -> ConnectorFuture<'_, Value> {
        Box::pin(async move { Ok(VictoriaLogsClient::search(self, request).await?) })
    }

    fn investigation_filter_operators(
        &self,
        field_type: &str,
    ) -> Vec<InvestigationFilterOperator> {
        match field_type.to_ascii_lowercase().as_str() {
            "timestamp" => vec![
                InvestigationFilterOperator::Eq,
                InvestigationFilterOperator::Gt,
                InvestigationFilterOperator::Gte,
                InvestigationFilterOperator::Lt,
                InvestigationFilterOperator::Lte,
            ],
            "text" | "keyword" | "stream" => vec![
                InvestigationFilterOperator::Eq,
                InvestigationFilterOperator::Contains,
            ],
            "integer" | "uint64" | "int64" | "float64" | "number" => vec![
                InvestigationFilterOperator::Eq,
                InvestigationFilterOperator::Gt,
                InvestigationFilterOperator::Gte,
                InvestigationFilterOperator::Lt,
                InvestigationFilterOperator::Lte,
            ],
            _ => Vec::new(),
        }
    }

    fn investigation_supports_aggregation_field(&self, field_type: &str) -> bool {
        matches!(
            field_type.to_ascii_lowercase().as_str(),
            "text" | "keyword" | "stream" | "integer" | "uint64" | "int64" | "float64" | "number"
        )
    }

    fn build_investigation_query(
        &self,
        index: &str,
        filters: &[InvestigationFilter],
    ) -> Result<String, ConnectorError> {
        super::validate_index(index)?;
        let clauses = filters
            .iter()
            .map(victorialogs_filter)
            .collect::<Result<Vec<_>, _>>()?;
        let query = clauses.join(" AND ");
        if query.len() > super::MAX_QUERY_BYTES {
            return Err(invalid_investigation_filter());
        }
        Ok(query)
    }

    fn investigation_supports_group_count(&self) -> bool {
        true
    }

    fn investigation_group_count(
        &self,
        request: crate::data_connectors::InvestigationGroupCountRequest,
    ) -> ConnectorFuture<'_, crate::data_connectors::InvestigationGroupCountResult> {
        Box::pin(async move {
            if request.group_by.is_empty()
                || request.group_by.len() > 3
                || !(2..=1_000_000_000).contains(&request.minimum_count)
                || !(1..=100).contains(&request.limit)
            {
                return Err(VictoriaLogsError::InvalidQuery.into());
            }
            let fields = self.fields(&request.index).await?;
            for field in &request.group_by {
                if !crate::search::safe_field_name(field)
                    || field == "_ig_duplicate_count"
                    || !fields.iter().any(|metadata| {
                        metadata.name == *field
                            && self.investigation_supports_aggregation_field(&metadata.field_type)
                    })
                {
                    return Err(VictoriaLogsError::InvalidQuery.into());
                }
            }
            for filter in &request.filters {
                let Some(metadata) = fields.iter().find(|field| field.name == filter.field) else {
                    return Err(VictoriaLogsError::InvalidQuery.into());
                };
                if metadata.field_type != filter.field_type
                    || !self
                        .investigation_filter_operators(&metadata.field_type)
                        .contains(&filter.operator)
                {
                    return Err(VictoriaLogsError::InvalidQuery.into());
                }
            }
            if request.time_range.is_some_and(|range| !range.is_valid())
                || !fields.iter().any(|field| {
                    field.name == request.time_field && field.is_time_field
                })
            {
                return Err(VictoriaLogsError::InvalidQuery.into());
            }
            let base_query = self.build_investigation_query(&request.index, &request.filters)?;
            let base_query = if base_query.is_empty() {
                "*".to_owned()
            } else {
                base_query
            };
            let query = victorialogs_group_count_query(
                &base_query,
                &request.group_by,
                request.minimum_count,
                request.limit,
            )?;
            let response = VictoriaLogsClient::search(
                self,
                SearchRequest {
                    index: request.index,
                    query: query.clone(),
                    size: request.limit,
                    time_range: request.time_range,
                    include_histogram: Some(false),
                    ..Default::default()
                },
            )
            .await?;
            let hits = response
                .get("hits")
                .and_then(Value::as_array)
                .cloned()
                .unwrap_or_default();
            let rows = hits
                .iter()
                .filter_map(|hit| hit.get("source").cloned())
                .collect::<Vec<_>>();
            let columns = request
                .group_by
                .iter()
                .cloned()
                .chain(std::iter::once("_ig_duplicate_count".to_owned()))
                .collect();
            let truncated = response.get("total_relation").and_then(Value::as_str) == Some("gte")
                || rows.len() >= request.limit;
            Ok(crate::data_connectors::InvestigationGroupCountResult {
                query_language: "LogsQL".to_owned(),
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

fn victorialogs_filter(filter: &InvestigationFilter) -> Result<String, ConnectorError> {
    if !crate::search::safe_field_name(&filter.field) {
        return Err(invalid_investigation_filter());
    }
    let value = match &filter.value {
        Value::String(value) => value.clone(),
        Value::Number(value) => value.to_string(),
        Value::Bool(value) => value.to_string(),
        _ => return Err(invalid_investigation_filter()),
    };
    let value = format!("\"{}\"", escape_logsql_string(&value));
    Ok(match filter.operator {
        InvestigationFilterOperator::Eq => format!("{}:={value}", filter.field),
        InvestigationFilterOperator::Contains => format!("{}:{value}", filter.field),
        InvestigationFilterOperator::Gt => format!("{}:>{value}", filter.field),
        InvestigationFilterOperator::Gte => format!("{}:>={value}", filter.field),
        InvestigationFilterOperator::Lt => format!("{}:<{value}", filter.field),
        InvestigationFilterOperator::Lte => format!("{}:<={value}", filter.field),
    })
}

fn victorialogs_group_count_query(
    base_query: &str,
    group_by: &[String],
    minimum_count: u64,
    limit: usize,
) -> Result<String, VictoriaLogsError> {
    if group_by.is_empty()
        || group_by.len() > 3
        || group_by.iter().any(|field| !crate::search::safe_field_name(field))
        || !(2..=1_000_000_000).contains(&minimum_count)
        || !(1..=100).contains(&limit)
    {
        return Err(VictoriaLogsError::InvalidQuery);
    }
    let query = format!(
        "{base_query} | stats by ({}) count() as _ig_duplicate_count | filter _ig_duplicate_count:>={minimum_count} | sort by (_ig_duplicate_count desc) | limit {limit}",
        group_by.join(", ")
    );
    if query.len() > super::MAX_QUERY_BYTES {
        return Err(VictoriaLogsError::InvalidQuery);
    }
    Ok(query)
}

fn escape_logsql_string(value: &str) -> String {
    value
        .replace('\\', "\\\\")
        .replace('"', "\\\"")
        .replace('\n', "\\n")
        .replace('\r', "\\r")
}

fn invalid_investigation_filter() -> ConnectorError {
    ConnectorError {
        status: StatusCode::BAD_REQUEST,
        code: "invalid_investigation_filter",
        message: "The investigation filter is not supported by this data source".to_owned(),
    }
}

impl From<VictoriaLogsError> for ConnectorError {
    fn from(error: VictoriaLogsError) -> Self {
        let (status, code, message) = match error {
            VictoriaLogsError::InvalidUrl | VictoriaLogsError::InvalidConfiguration => (
                StatusCode::BAD_REQUEST,
                "invalid_victorialogs_connection",
                "VictoriaLogs connection details are invalid",
            ),
            VictoriaLogsError::InvalidQuery => (
                StatusCode::BAD_REQUEST,
                "invalid_victorialogs_request",
                "VictoriaLogs query or search parameters are invalid or exceed configured limits",
            ),
            VictoriaLogsError::ClientConfiguration => (
                StatusCode::BAD_GATEWAY,
                "victorialogs_client_error",
                "Could not configure the VictoriaLogs HTTP client",
            ),
            VictoriaLogsError::Request => (
                StatusCode::BAD_GATEWAY,
                "victorialogs_unreachable",
                "Could not complete the VictoriaLogs request; it may have timed out",
            ),
            VictoriaLogsError::Rejected { status } => (
                match status {
                    400 => StatusCode::BAD_REQUEST,
                    401 | 403 => StatusCode::BAD_GATEWAY,
                    404 => StatusCode::NOT_FOUND,
                    _ => StatusCode::BAD_GATEWAY,
                },
                "victorialogs_rejected_request",
                "VictoriaLogs rejected the request",
            ),
            VictoriaLogsError::InvalidResponse | VictoriaLogsError::ResponseTooLarge => (
                StatusCode::BAD_GATEWAY,
                "victorialogs_response_error",
                "VictoriaLogs returned an unusable response",
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
mod investigation_tests {
    use super::*;

    #[test]
    fn investigation_filters_use_exact_logs_lql_syntax_and_escape_strings() {
        let exact = InvestigationFilter {
            field: "login".to_owned(),
            field_type: "keyword".to_owned(),
            operator: InvestigationFilterOperator::Eq,
            value: json!("user\"name"),
        };
        assert_eq!(victorialogs_filter(&exact).unwrap(), "login:=\"user\\\"name\"");
        let contains = InvestigationFilter {
            field_type: "text".to_owned(),
            operator: InvestigationFilterOperator::Contains,
            value: json!("two words"),
            ..exact
        };
        assert_eq!(victorialogs_filter(&contains).unwrap(), "login:\"two words\"");
    }

    #[test]
    fn grouped_query_uses_bounded_logs_aggregate_pipeline() {
        let query =
            victorialogs_group_count_query("status:=500", &["login".to_owned()], 2, 50).unwrap();
        assert_eq!(
            query,
            "status:=500 | stats by (login) count() as _ig_duplicate_count | filter _ig_duplicate_count:>=2 | sort by (_ig_duplicate_count desc) | limit 50"
        );
    }
}
