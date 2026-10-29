use super::{ApiFlavor, PrometheusClient, PrometheusError, PrometheusSettings, PROMETHEUS};
use crate::connector_auth::HttpAuth;
use crate::data_connectors::{
    basic_credentials, string_setting, ConnectionConfig, Connector, ConnectorError,
    ConnectorFuture, FieldSummary, IndexSummary,
};
use crate::search::SearchRequest;
use axum::http::StatusCode;
use serde_json::{json, Value};
use std::sync::Arc;

pub fn connect(config: &ConnectionConfig) -> Result<Arc<dyn Connector>, ConnectorError> {
    let raw_url = string_setting(config, "url")?.ok_or_else(ConnectorError::invalid_connection)?;
    let url = url::Url::parse(&raw_url).map_err(|_| ConnectorError::invalid_connection())?;
    let auth = match basic_credentials(config)? {
        Some((username, password)) => HttpAuth::Basic {
            username,
            password: Some(password),
        },
        None => HttpAuth::None,
    };
    Ok(Arc::new(PrometheusClient::new(PrometheusSettings {
        url,
        auth,
        tenant_id: None,
        flavor: &PROMETHEUS,
    })?))
}

impl PrometheusClient {
    fn connector_error(&self, error: PrometheusError) -> ConnectorError {
        if matches!(error, PrometheusError::Rejected { status: 401, .. }) {
            self.auth().invalidate();
        }
        connector_error(error, self.flavor())
    }
}

impl Connector for PrometheusClient {
    fn public_configuration(&self) -> ConnectionConfig {
        if !self.flavor().configurable_auth {
            return ConnectionConfig::new();
        }
        let mut configuration = self.auth().public_configuration();
        if let Some(tenant_id) = self.tenant_id() {
            configuration.insert("tenant_id".to_owned(), json!(tenant_id));
        }
        configuration
    }

    fn test_connection(&self) -> ConnectorFuture<'_, ()> {
        Box::pin(async move {
            PrometheusClient::test_connection(self)
                .await
                .map_err(|error| self.connector_error(error))
        })
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
        Box::pin(async move {
            PrometheusClient::fields(self, index)
                .await
                .map_err(|error| self.connector_error(error))
        })
    }

    fn search(&self, request: SearchRequest) -> ConnectorFuture<'_, Value> {
        Box::pin(async move {
            PrometheusClient::search(self, request)
                .await
                .map_err(|error| self.connector_error(error))
        })
    }

    fn document<'a>(&'a self, _index: &'a str, _id: &'a str) -> ConnectorFuture<'a, Value> {
        Box::pin(async { Err(ConnectorError::invalid_connection()) })
    }
}

impl From<PrometheusError> for ConnectorError {
    fn from(error: PrometheusError) -> Self {
        connector_error(error, &PROMETHEUS)
    }
}

/// Curated errors for any server that speaks the Prometheus HTTP API, named after `flavor`.
pub fn connector_error(error: PrometheusError, flavor: &ApiFlavor) -> ConnectorError {
    let name = flavor.name;
    let codes = &flavor.codes;
    let (status, code, message) = match error {
        PrometheusError::InvalidUrl | PrometheusError::InvalidConfiguration => (
            StatusCode::BAD_REQUEST,
            codes.invalid_connection,
            format!("{name} connection details are invalid"),
        ),
        PrometheusError::InvalidQuery => (
            StatusCode::BAD_REQUEST,
            codes.invalid_request,
            "PromQL query or search parameters are invalid or exceed configured limits".to_owned(),
        ),
        PrometheusError::TooManyResults | PrometheusError::ResponseTooLarge => (
            StatusCode::PAYLOAD_TOO_LARGE,
            codes.result_limit_exceeded,
            format!("{name} returned more samples than the connector reads at once; narrow the time range or selector"),
        ),
        PrometheusError::SortTooLarge => (
            StatusCode::PAYLOAD_TOO_LARGE,
            codes.result_limit_exceeded,
            "Sorting by a label or value is limited to 50,000 samples; sort by time or narrow the time range or selector".to_owned(),
        ),
        PrometheusError::PartialResponse => (
            StatusCode::BAD_GATEWAY,
            codes.partial_response,
            format!("{name} returned query warnings, so the result may be incomplete"),
        ),
        PrometheusError::ClientConfiguration => (
            StatusCode::BAD_GATEWAY,
            codes.client_error,
            format!("Could not configure the {name} HTTP client"),
        ),
        PrometheusError::Request => (
            StatusCode::BAD_GATEWAY,
            codes.unreachable,
            format!("Could not complete the {name} request; it may have timed out"),
        ),
        PrometheusError::Authentication => (
            StatusCode::BAD_GATEWAY,
            codes.authentication_failed,
            "Could not obtain an OAuth 2.0 access token; check the token URL, client ID, and client secret".to_owned(),
        ),
        PrometheusError::Rejected { status, error_type } => {
            let (status, message) = rejected_message(name, status, &error_type);
            (status, codes.rejected_request, message)
        }
        PrometheusError::InvalidResponse => (
            StatusCode::BAD_GATEWAY,
            codes.response_error,
            format!("{name} returned an unusable response"),
        ),
    };
    ConnectorError {
        status,
        code,
        message,
    }
}

/// Maps Prometheus API error categories to curated messages; upstream error text is never returned.
fn rejected_message(name: &str, status: u16, error_type: &str) -> (StatusCode, String) {
    match (status, error_type) {
        (_, "bad_data") | (400, _) => (
            StatusCode::BAD_REQUEST,
            format!("{name} could not parse the PromQL query; check the expression"),
        ),
        (_, "execution") | (422, _) => (
            StatusCode::BAD_REQUEST,
            format!("{name} could not evaluate the query within its limits; narrow the time range or selector"),
        ),
        (_, "timeout" | "canceled") | (503, _) => (
            StatusCode::GATEWAY_TIMEOUT,
            format!("{name} timed out evaluating the query; narrow the time range or selector"),
        ),
        (401 | 403, _) => (
            StatusCode::BAD_GATEWAY,
            format!("{name} rejected the configured credentials"),
        ),
        (404, _) => (
            StatusCode::NOT_FOUND,
            format!("{name} API was not found at this URL; check the connection URL"),
        ),
        (429, _) => (
            StatusCode::SERVICE_UNAVAILABLE,
            format!("{name} is rate limiting requests; retry shortly"),
        ),
        _ => (StatusCode::BAD_GATEWAY, format!("{name} rejected the request")),
    }
}
