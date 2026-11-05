use super::{PrometheusClient, PrometheusError, PrometheusSettings};
use crate::data_connectors::{
    basic_credentials, string_setting, ConnectionConfig, Connector, ConnectorError,
    ConnectorFuture, FieldSummary, IndexSummary, InvestigationFilter,
    InvestigationFilterOperator, MetricNames, MetricQueryRequest, MetricQueryResult,
};
use crate::search::SearchRequest;
use axum::http::StatusCode;
use serde_json::Value;
use std::sync::Arc;

pub fn connect(config: &ConnectionConfig) -> Result<Arc<dyn Connector>, ConnectorError> {
    let raw_url = string_setting(config, "url")?.ok_or_else(ConnectorError::invalid_connection)?;
    let url = url::Url::parse(&raw_url).map_err(|_| ConnectorError::invalid_connection())?;
    let (username, password) = basic_credentials(config)?.unzip();
    Ok(Arc::new(PrometheusClient::new(PrometheusSettings {
        url,
        username,
        password,
    })?))
}

impl Connector for PrometheusClient {
    fn test_connection(&self) -> ConnectorFuture<'_, ()> {
        Box::pin(async move { Ok(PrometheusClient::test_connection(self).await?) })
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
        Box::pin(async move { Ok(PrometheusClient::fields(self, index).await?) })
    }

    fn search(&self, request: SearchRequest) -> ConnectorFuture<'_, Value> {
        Box::pin(async move { Ok(PrometheusClient::search(self, request).await?) })
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
        Box::pin(async move { Ok(PrometheusClient::metric_names(self, index).await?) })
    }

    fn metric_query(&self, request: MetricQueryRequest) -> ConnectorFuture<'_, MetricQueryResult> {
        Box::pin(async move { Ok(PrometheusClient::metric_query(self, request).await?) })
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

#[cfg(test)]
mod investigation_tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn investigation_filters_compile_to_escaped_promql_label_matchers() {
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
            field: "__name__".to_owned(),
            field_type: "label".to_owned(),
            operator: InvestigationFilterOperator::Gt,
            value: json!(1),
        }])
        .is_err());
    }
}

impl From<PrometheusError> for ConnectorError {
    fn from(error: PrometheusError) -> Self {
        let (status, code, message) = match error {
            PrometheusError::InvalidUrl | PrometheusError::InvalidConfiguration => (
                StatusCode::BAD_REQUEST,
                "invalid_prometheus_connection",
                "Prometheus connection details are invalid",
            ),
            PrometheusError::InvalidQuery => (
                StatusCode::BAD_REQUEST,
                "invalid_prometheus_request",
                "PromQL query or search parameters are invalid or exceed configured limits",
            ),
            PrometheusError::TooManyResults | PrometheusError::ResponseTooLarge => (
                StatusCode::PAYLOAD_TOO_LARGE,
                "prometheus_result_limit_exceeded",
                "Prometheus returned more samples than the connector reads at once; narrow the time range or selector",
            ),
            PrometheusError::SortTooLarge => (
                StatusCode::PAYLOAD_TOO_LARGE,
                "prometheus_result_limit_exceeded",
                "Sorting by a label or value is limited to 50,000 samples; sort by time or narrow the time range or selector",
            ),
            PrometheusError::PartialResponse => (
                StatusCode::BAD_GATEWAY,
                "prometheus_partial_response",
                "Prometheus returned query warnings, so the result may be incomplete",
            ),
            PrometheusError::ClientConfiguration => (
                StatusCode::BAD_GATEWAY,
                "prometheus_client_error",
                "Could not configure the Prometheus HTTP client",
            ),
            PrometheusError::Request => (
                StatusCode::BAD_GATEWAY,
                "prometheus_unreachable",
                "Could not complete the Prometheus request; it may have timed out",
            ),
            PrometheusError::Rejected { status, error_type } => {
                let (status, message) = rejected_message(status, &error_type);
                (status, "prometheus_rejected_request", message)
            }
            PrometheusError::InvalidResponse => (
                StatusCode::BAD_GATEWAY,
                "prometheus_response_error",
                "Prometheus returned an unusable response",
            ),
        };
        Self {
            status,
            code,
            message: message.to_owned(),
        }
    }
}

/// Maps Prometheus API error categories to curated messages; upstream error text is never returned.
fn rejected_message(status: u16, error_type: &str) -> (StatusCode, &'static str) {
    match (status, error_type) {
        (_, "bad_data") | (400, _) => (
            StatusCode::BAD_REQUEST,
            "Prometheus could not parse the PromQL query; check the expression",
        ),
        (_, "execution") | (422, _) => (
            StatusCode::BAD_REQUEST,
            "Prometheus could not evaluate the query within its limits; narrow the time range or selector",
        ),
        (_, "timeout" | "canceled") | (503, _) => (
            StatusCode::GATEWAY_TIMEOUT,
            "Prometheus timed out evaluating the query; narrow the time range or selector",
        ),
        (401 | 403, _) => (
            StatusCode::BAD_GATEWAY,
            "Prometheus rejected the configured credentials",
        ),
        (404, _) => (
            StatusCode::NOT_FOUND,
            "Prometheus API was not found at this URL; check the connection URL",
        ),
        (429, _) => (
            StatusCode::SERVICE_UNAVAILABLE,
            "Prometheus is rate limiting requests; retry shortly",
        ),
        _ => (StatusCode::BAD_GATEWAY, "Prometheus rejected the request"),
    }
}
