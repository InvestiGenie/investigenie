use super::{VictoriaMetricsClient, VictoriaMetricsError, VictoriaMetricsSettings};
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
    Ok(Arc::new(VictoriaMetricsClient::new(VictoriaMetricsSettings {
        url,
        username: string_setting(config, "username")?,
        password: string_setting(config, "password")?,
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
                "VictoriaMetrics result exceeds the connector's bounded sample limit; narrow the time range or metric selector",
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
            VictoriaMetricsError::Rejected { status } => (
                match status {
                    400 => StatusCode::BAD_REQUEST,
                    401 | 403 => StatusCode::BAD_GATEWAY,
                    404 => StatusCode::NOT_FOUND,
                    _ => StatusCode::BAD_GATEWAY,
                },
                "victoriametrics_rejected_request",
                "VictoriaMetrics rejected the request",
            ),
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
