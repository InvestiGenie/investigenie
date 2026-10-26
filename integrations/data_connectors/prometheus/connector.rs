use super::{PrometheusClient, PrometheusError, PrometheusSettings};
use crate::data_connectors::{
    basic_credentials, string_setting, ConnectionConfig, Connector, ConnectorError,
    ConnectorFuture, FieldSummary, IndexSummary,
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

    fn document<'a>(&'a self, _index: &'a str, _id: &'a str) -> ConnectorFuture<'a, Value> {
        Box::pin(async { Err(ConnectorError::invalid_connection()) })
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
