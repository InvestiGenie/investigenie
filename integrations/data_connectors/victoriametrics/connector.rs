use super::{VictoriaMetricsClient, VictoriaMetricsError, VictoriaMetricsSettings};
use crate::data_connectors::{
    basic_credentials, string_setting, ConnectionConfig, Connector, ConnectorError,
    ConnectorFuture, FieldSummary, IndexSummary, MetricNames, MetricQueryRequest,
    InvestigationFilter, InvestigationFilterOperator, MetricQueryResult,
};
use crate::search::SearchRequest;
use axum::http::StatusCode;
use serde_json::Value;
use std::sync::Arc;

pub fn connect(config: &ConnectionConfig) -> Result<Arc<dyn Connector>, ConnectorError> {
    let raw_url = string_setting(config, "url")?.ok_or_else(ConnectorError::invalid_connection)?;
    let url = url::Url::parse(&raw_url).map_err(|_| ConnectorError::invalid_connection())?;
    let (username, password) = basic_credentials(config)?.unzip();
    Ok(Arc::new(VictoriaMetricsClient::new(
        VictoriaMetricsSettings {
            url,
            username,
            password,
        },
    )?))
}

impl Connector for VictoriaMetricsClient {
    fn test_connection(&self) -> ConnectorFuture<'_, ()> {
        Box::pin(async move { Ok(VictoriaMetricsClient::test_connection(self).await?) })
    }

    fn indices(&self) -> ConnectorFuture<'_, Vec<IndexSummary>> {
        Box::pin(async {
            Ok(vec![IndexSummary {
                name: "metrics".to_owned(),
                document_count: None,
                health: None,
            }])
        })
    }

    fn fields<'a>(&'a self, index: &'a str) -> ConnectorFuture<'a, Vec<FieldSummary>> {
        Box::pin(async move { Ok(VictoriaMetricsClient::fields(self, index).await?) })
    }

    fn search(&self, request: SearchRequest) -> ConnectorFuture<'_, Value> {
        Box::pin(async move { Ok(VictoriaMetricsClient::search(self, request).await?) })
    }

    fn investigation_filter_operators(
        &self,
        field_type: &str,
    ) -> Vec<InvestigationFilterOperator> {
        if field_type == "label" {
            vec![
                InvestigationFilterOperator::Eq,
                InvestigationFilterOperator::Contains,
            ]
        } else {
            Vec::new()
        }
    }

    fn investigation_supports_aggregation_field(&self, field_type: &str) -> bool {
        field_type == "label"
    }

    fn build_investigation_query(
        &self,
        index: &str,
        filters: &[InvestigationFilter],
    ) -> Result<String, ConnectorError> {
        super::validate_index(index)?;
        label_selector(filters)
    }

    fn metric_names<'a>(&'a self, index: &'a str) -> ConnectorFuture<'a, MetricNames> {
        Box::pin(async move { Ok(VictoriaMetricsClient::metric_names(self, index).await?) })
    }

    fn metric_query(&self, request: MetricQueryRequest) -> ConnectorFuture<'_, MetricQueryResult> {
        Box::pin(async move { Ok(VictoriaMetricsClient::metric_query(self, request).await?) })
    }

    fn document<'a>(&'a self, _index: &'a str, _id: &'a str) -> ConnectorFuture<'a, Value> {
        Box::pin(async { Err(ConnectorError::invalid_connection()) })
    }
}

fn label_selector(filters: &[InvestigationFilter]) -> Result<String, ConnectorError> {
    let mut matchers = Vec::new();
    for filter in filters {
        if filter.field_type != "label"
            || !super::is_label_name(&filter.field)
            || !matches!(
                filter.operator,
                InvestigationFilterOperator::Eq | InvestigationFilterOperator::Contains
            )
        {
            return Err(invalid_investigation_filter());
        }
        let Some(value) = filter.value.as_str() else {
            return Err(invalid_investigation_filter());
        };
        let (operator, value) = match filter.operator {
            InvestigationFilterOperator::Eq => ("=", value.to_owned()),
            InvestigationFilterOperator::Contains => {
                ("=~", format!(".*{}.*", escape_re2(value)))
            }
            _ => return Err(invalid_investigation_filter()),
        };
        matchers.push(format!(
            "{}{}\"{}\"",
            filter.field,
            operator,
            value
                .replace('\\', "\\\\")
                .replace('"', "\\\"")
                .replace('\n', "\\n")
                .replace('\r', "\\r")
        ));
    }
    if matchers.is_empty() {
        Ok("{__name__!=\"\"}".to_owned())
    } else {
        Ok(format!("{{{}}}", matchers.join(",")))
    }
}

fn escape_re2(value: &str) -> String {
    let mut escaped = String::new();
    for character in value.chars() {
        if "\\.+*?()|[]{}^$".contains(character) {
            escaped.push('\\');
        }
        escaped.push(character);
    }
    escaped
}

fn invalid_investigation_filter() -> ConnectorError {
    ConnectorError {
        status: StatusCode::BAD_REQUEST,
        code: "invalid_investigation_filter",
        message: "The investigation filter is not supported by this data source".to_owned(),
    }
}

impl From<VictoriaMetricsError> for ConnectorError {
    fn from(error: VictoriaMetricsError) -> Self {
        let (status, code, message) = match error {
            VictoriaMetricsError::InvalidUrl | VictoriaMetricsError::InvalidConfiguration => (
                StatusCode::BAD_REQUEST,
                "invalid_victoriametrics_connection",
                "VictoriaMetrics connection details are invalid",
            ),
            VictoriaMetricsError::InvalidQuery => (
                StatusCode::BAD_REQUEST,
                "invalid_victoriametrics_request",
                "VictoriaMetrics query or search parameters are invalid or exceed configured limits",
            ),
            VictoriaMetricsError::TooManySamples | VictoriaMetricsError::ResponseTooLarge => (
                StatusCode::PAYLOAD_TOO_LARGE,
                "victoriametrics_result_limit_exceeded",
                "VictoriaMetrics returned more samples than the connector reads at once; narrow the time range or metric selector",
            ),
            VictoriaMetricsError::SortTooLarge => (
                StatusCode::PAYLOAD_TOO_LARGE,
                "victoriametrics_result_limit_exceeded",
                "Sorting by a label or value is limited to 50,000 samples; sort by time or narrow the time range or selector",
            ),
            VictoriaMetricsError::ClientConfiguration => (
                StatusCode::BAD_GATEWAY,
                "victoriametrics_client_error",
                "Could not configure the VictoriaMetrics HTTP client",
            ),
            VictoriaMetricsError::Request => (
                StatusCode::BAD_GATEWAY,
                "victoriametrics_unreachable",
                "Could not complete the VictoriaMetrics request; it may have timed out",
            ),
            VictoriaMetricsError::Rejected { status } => {
                let (status, message) = match status {
                    400 => (
                        StatusCode::BAD_REQUEST,
                        "VictoriaMetrics could not parse the query; enter a series selector such as up{job=\"api\"}",
                    ),
                    422 => (
                        StatusCode::BAD_REQUEST,
                        "VictoriaMetrics refused the query because it exceeds server limits; narrow the selector or time range",
                    ),
                    401 | 403 => (
                        StatusCode::BAD_GATEWAY,
                        "VictoriaMetrics rejected the configured credentials",
                    ),
                    404 => (
                        StatusCode::NOT_FOUND,
                        "VictoriaMetrics API was not found at this URL; check the connection URL",
                    ),
                    429 => (
                        StatusCode::SERVICE_UNAVAILABLE,
                        "VictoriaMetrics is rate limiting requests; retry shortly",
                    ),
                    503 | 504 => (
                        StatusCode::GATEWAY_TIMEOUT,
                        "VictoriaMetrics timed out; narrow the selector or time range",
                    ),
                    _ => (StatusCode::BAD_GATEWAY, "VictoriaMetrics rejected the request"),
                };
                (status, "victoriametrics_rejected_request", message)
            }
            VictoriaMetricsError::InvalidResponse => (
                StatusCode::BAD_GATEWAY,
                "victoriametrics_response_error",
                "VictoriaMetrics returned an unusable response",
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
    use serde_json::json;

    #[test]
    fn investigation_filters_compile_to_escaped_metricsql_label_matchers() {
        let query = label_selector(&[
            InvestigationFilter {
                field: "job".to_owned(),
                field_type: "label".to_owned(),
                operator: InvestigationFilterOperator::Eq,
                value: json!("api"),
            },
            InvestigationFilter {
                field: "instance".to_owned(),
                field_type: "label".to_owned(),
                operator: InvestigationFilterOperator::Contains,
                value: json!("api.1"),
            },
        ])
        .unwrap();
        assert_eq!(query, "{job=\"api\",instance=~\".*api\\\\.1.*\"}");
        assert!(label_selector(&[InvestigationFilter {
            field: "instance".to_owned(),
            field_type: "label".to_owned(),
            operator: InvestigationFilterOperator::Lt,
            value: json!("api"),
        }])
        .is_err());
    }
}
