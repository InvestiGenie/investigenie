use super::{VictoriaMetricsClient, VictoriaMetricsError, VictoriaMetricsSettings};
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
    Ok(Arc::new(VictoriaMetricsClient::new(VictoriaMetricsSettings {
        url,
        username,
        password,
    })?))
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

    fn document<'a>(&'a self, _index: &'a str, _id: &'a str) -> ConnectorFuture<'a, Value> {
        Box::pin(async { Err(ConnectorError::invalid_connection()) })
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
