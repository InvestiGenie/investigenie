use super::{
    VictoriaLogsClient, VictoriaLogsError, VictoriaLogsSettings,
};
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
    let username = string_setting(config, "username")?;
    let password = string_setting(config, "password")?;
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

    fn document<'a>(&'a self, _index: &'a str, _id: &'a str) -> ConnectorFuture<'a, Value> {
        Box::pin(async { Err(ConnectorError::invalid_connection()) })
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
