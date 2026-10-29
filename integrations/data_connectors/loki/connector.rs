use super::{LokiClient, LokiError, LokiSettings, MAX_QUERY_LENGTH_SETTING};
use crate::connector_auth::HttpAuth;
use crate::data_connectors::{
    string_setting, ConnectionConfig, Connector, ConnectorError, ConnectorFuture, FieldSummary,
    IndexSummary,
};
use crate::search::SearchRequest;
use axum::http::StatusCode;
use serde_json::{json, Value};
use std::sync::Arc;

pub fn connect(config: &ConnectionConfig) -> Result<Arc<dyn Connector>, ConnectorError> {
    let raw_url = string_setting(config, "url")?.ok_or_else(ConnectorError::invalid_connection)?;
    let url = url::Url::parse(&raw_url).map_err(|_| ConnectorError::invalid_connection())?;
    let max_query_length = match config.get(MAX_QUERY_LENGTH_SETTING) {
        None | Some(Value::Null) => None,
        Some(value) => Some(
            value
                .as_i64()
                .filter(|limit| *limit >= 0)
                .ok_or_else(ConnectorError::invalid_connection)?,
        ),
    };
    Ok(Arc::new(LokiClient::new(LokiSettings {
        url,
        auth: HttpAuth::from_config(config)?,
        tenant_id: string_setting(config, "tenant_id")?,
        max_query_length,
    })?))
}

impl LokiClient {
    fn connector_error(&self, error: LokiError) -> ConnectorError {
        if matches!(error, LokiError::Rejected { status: 401 }) {
            self.auth().invalidate();
        }
        error.into()
    }
}

impl Connector for LokiClient {
    fn public_configuration(&self) -> ConnectionConfig {
        let mut configuration = self.auth().public_configuration();
        if let Some(tenant_id) = self.tenant_id() {
            configuration.insert("tenant_id".to_owned(), json!(tenant_id));
        }
        if let Some(limit) = self.max_query_length() {
            configuration.insert(MAX_QUERY_LENGTH_SETTING.to_owned(), json!(limit));
        }
        configuration
    }

    fn learned_configuration(&self) -> ConnectionConfig {
        let mut configuration = ConnectionConfig::new();
        if let Some(limit) = self.learned_max_query_length() {
            configuration.insert(MAX_QUERY_LENGTH_SETTING.to_owned(), json!(limit));
        }
        configuration
    }

    fn test_connection(&self) -> ConnectorFuture<'_, ()> {
        Box::pin(async move {
            LokiClient::test_connection(self)
                .await
                .map_err(|error| self.connector_error(error))
        })
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
        Box::pin(async move {
            LokiClient::fields(self, index)
                .await
                .map_err(|error| self.connector_error(error))
        })
    }

    fn search(&self, request: SearchRequest) -> ConnectorFuture<'_, Value> {
        Box::pin(async move {
            LokiClient::search(self, request)
                .await
                .map_err(|error| self.connector_error(error))
        })
    }

    fn document<'a>(&'a self, _index: &'a str, _id: &'a str) -> ConnectorFuture<'a, Value> {
        Box::pin(async { Err(ConnectorError::invalid_connection()) })
    }
}

impl From<LokiError> for ConnectorError {
    fn from(error: LokiError) -> Self {
        let (status, code, message) = match error {
            LokiError::InvalidUrl | LokiError::InvalidConfiguration => (
                StatusCode::BAD_REQUEST,
                "invalid_loki_connection",
                "Loki connection or authentication details are invalid",
            ),
            LokiError::InvalidQuery => (
                StatusCode::BAD_REQUEST,
                "invalid_loki_request",
                "LogQL query or search parameters are invalid or exceed configured limits",
            ),
            LokiError::SelectorRequired => (
                StatusCode::BAD_REQUEST,
                "loki_selector_required",
                "Loki needs a stream selector with at least one non-empty label matcher, such as {app=\"api\"}",
            ),
            LokiError::TooManyEntries | LokiError::ResponseTooLarge => (
                StatusCode::PAYLOAD_TOO_LARGE,
                "loki_result_limit_exceeded",
                "Loki result exceeds the connector's bounded entry or response limit; narrow the query or time range",
            ),
            LokiError::ClientConfiguration => (
                StatusCode::BAD_GATEWAY,
                "loki_client_error",
                "Could not configure the Loki HTTP client",
            ),
            LokiError::Request => (
                StatusCode::BAD_GATEWAY,
                "loki_unreachable",
                "Could not complete the Loki request; it may have timed out",
            ),
            LokiError::Rejected { status } => (
                match status {
                    400 => StatusCode::BAD_REQUEST,
                    401 | 403 => StatusCode::BAD_GATEWAY,
                    404 => StatusCode::NOT_FOUND,
                    429 | 503 => StatusCode::SERVICE_UNAVAILABLE,
                    _ => StatusCode::BAD_GATEWAY,
                },
                "loki_rejected_request",
                "Loki rejected the request",
            ),
            LokiError::Authentication => (
                StatusCode::BAD_GATEWAY,
                "loki_authentication_failed",
                "Could not obtain an OAuth 2.0 access token; check the token URL, client ID, and client secret",
            ),
            LokiError::QueryTooLong { .. } => (
                StatusCode::BAD_REQUEST,
                "loki_query_too_long",
                "The time range is longer than Loki's max_query_length; choose a shorter time range",
            ),
            LokiError::InvalidResponse => (
                StatusCode::BAD_GATEWAY,
                "loki_response_error",
                "Loki returned an unusable response",
            ),
        };
        Self {
            status,
            code,
            message: message.to_owned(),
        }
    }
}
