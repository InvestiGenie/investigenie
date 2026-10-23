use super::{PrometheusClient, PrometheusError, PrometheusSettings};
use crate::data_connectors::{
    string_setting, ConnectionConfig, Connector, ConnectorError, ConnectorFuture, FieldSummary,
    IndexSummary,
};
use crate::search::SearchRequest;
use axum::http::StatusCode;
use serde_json::Value;
use std::sync::Arc;

pub fn connect(config: &ConnectionConfig) -> Result<Arc<dyn Connector>, ConnectorError> {
    let raw_url = string_setting(config, "url")?.ok_or_else(ConnectorError::invalid_connection)?;
    let url = url::Url::parse(&raw_url).map_err(|_| ConnectorError::invalid_connection())?;
    Ok(Arc::new(PrometheusClient::new(PrometheusSettings {
        url,
        username: string_setting(config, "username")?,
        password: string_setting(config, "password")?,
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
                "Prometheus result exceeds the connector's bounded series, sample, or response limit; narrow the query or time range",
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
            PrometheusError::Rejected { status } => (
                match status {
                    400 | 422 => StatusCode::BAD_REQUEST,
                    401 | 403 => StatusCode::BAD_GATEWAY,
                    404 => StatusCode::NOT_FOUND,
                    503 => StatusCode::GATEWAY_TIMEOUT,
                    _ => StatusCode::BAD_GATEWAY,
                },
                "prometheus_rejected_request",
                "Prometheus rejected the request",
            ),
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
