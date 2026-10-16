use super::{
    ElasticsearchAuthentication, ElasticsearchClient, ElasticsearchError, ElasticsearchSettings,
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
    if !matches!(url.scheme(), "http" | "https")
        || url.host_str().is_none()
        || !url.username().is_empty()
        || url.password().is_some()
        || url.query().is_some()
        || url.fragment().is_some()
    {
        return Err(ConnectorError::invalid_connection());
    }
    let username = string_setting(config, "username")?.filter(|value| !value.trim().is_empty());
    let password = string_setting(config, "password")?.filter(|value| !value.is_empty());
    let api_key = string_setting(config, "api_key")?.filter(|value| !value.trim().is_empty());
    let authentication = match (api_key, username, password) {
        (Some(key), None, None) => Some(ElasticsearchAuthentication::ApiKey(key)),
        (None, Some(username), Some(password)) => {
            Some(ElasticsearchAuthentication::Basic { username, password })
        }
        (None, None, None) => None,
        _ => return Err(ConnectorError::invalid_connection()),
    };
    let inferred = match &authentication {
        None => "none",
        Some(ElasticsearchAuthentication::Basic { .. }) => "basic",
        Some(ElasticsearchAuthentication::ApiKey(_)) => "api_key",
    };
    if config.contains_key("auth_type")
        && string_setting(config, "auth_type")?.as_deref() != Some(inferred)
    {
        return Err(ConnectorError::invalid_connection());
    }
    let tls_insecure = match config.get("tls_insecure") {
        None | Some(Value::Null) => false,
        Some(Value::Bool(value)) => *value,
        _ => return Err(ConnectorError::invalid_connection()),
    };
    Ok(Arc::new(ElasticsearchClient::new(ElasticsearchSettings {
        url,
        authentication,
        tls_insecure,
    })?))
}

impl Connector for ElasticsearchClient {
    fn public_configuration(&self) -> ConnectionConfig {
        serde_json::from_value(json!({"auth_type":self.auth_type})).unwrap()
    }
    fn test_connection(&self) -> ConnectorFuture<'_, ()> {
        Box::pin(async move {
            self.list_indices().await?;
            Ok(())
        })
    }
    fn indices(&self) -> ConnectorFuture<'_, Vec<IndexSummary>> {
        Box::pin(async move { Ok(self.list_indices().await?) })
    }
    fn fields<'a>(&'a self, index: &'a str) -> ConnectorFuture<'a, Vec<FieldSummary>> {
        Box::pin(async move { Ok(self.fields(index).await?) })
    }
    fn search(&self, request: SearchRequest) -> ConnectorFuture<'_, Value> {
        Box::pin(async move { Ok(json!(self.search(request).await?)) })
    }
    fn document<'a>(&'a self, index: &'a str, id: &'a str) -> ConnectorFuture<'a, Value> {
        Box::pin(async move { Ok(json!(self.document(index, id).await?)) })
    }
}

impl From<ElasticsearchError> for ConnectorError {
    fn from(error: ElasticsearchError) -> Self {
        let (status, code, message) = match error {
            ElasticsearchError::Rejected { status, .. } => (
                match status {
                    404 => StatusCode::NOT_FOUND,
                    400 => StatusCode::BAD_REQUEST,
                    _ => StatusCode::BAD_GATEWAY,
                },
                "elasticsearch_rejected_request",
                "Elasticsearch rejected the request",
            ),
            ElasticsearchError::InvalidIndex => (
                StatusCode::BAD_REQUEST,
                "invalid_search_parameter",
                "One or more search parameters are invalid or exceed configured limits",
            ),
            ElasticsearchError::Request => (
                StatusCode::BAD_GATEWAY,
                "elasticsearch_unreachable",
                "Could not complete the Elasticsearch request; it may have timed out",
            ),
            ElasticsearchError::ClientConfiguration | ElasticsearchError::InvalidResponse => (
                StatusCode::BAD_GATEWAY,
                "elasticsearch_response_error",
                "Elasticsearch returned an unusable response",
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
mod tests {
    use super::*;

    #[test]
    fn authentication_and_endpoint_validation_preserve_security() {
        for settings in [
            json!({"url":"http://user:secret@localhost:9200"}),
            json!({"url":"http://localhost:9200?secret=value"}),
            json!({"url":"http://localhost:9200#fragment"}),
            json!({"url":"file:///secret"}),
            json!({"url":"http://localhost:9200","api_key":"secret","username":"user","password":"secret"}),
            json!({"url":"http://localhost:9200","username":"user","password":""}),
            json!({"url":"http://localhost:9200","auth_type":"basic"}),
            json!({"url":"http://localhost:9200","auth_type":"unknown"}),
            json!({"url":"http://localhost:9200","auth_type":null}),
            json!({"url":"http://localhost:9200","auth_type":"none","api_key":"secret"}),
            json!({"url":"http://localhost:9200","tls_insecure":"true"}),
        ] {
            assert!(
                connect(&serde_json::from_value(settings.clone()).unwrap()).is_err(),
                "{settings}"
            );
        }
        for settings in [
            json!({"url":"http://localhost:9200"}),
            json!({"url":"http://localhost:9200","auth_type":"none"}),
            json!({"url":"http://localhost:9200","auth_type":"basic","username":"user","password":"secret"}),
            json!({"url":"http://localhost:9200","auth_type":"basic","username":"user","password":"   "}),
            json!({"url":"http://localhost:9200","auth_type":"api_key","api_key":"secret"}),
        ] {
            assert!(connect(&serde_json::from_value(settings).unwrap()).is_ok());
        }
    }

    #[test]
    fn public_configuration_infers_legacy_authentication_without_secrets() {
        for (credentials, mode) in [
            (json!({}), "none"),
            (json!({"username":"user","password":" secret "}), "basic"),
            (json!({"api_key":"secret"}), "api_key"),
        ] {
            let mut config = credentials.as_object().unwrap().clone();
            config.insert("url".to_owned(), json!("http://localhost:9200"));
            let connector = connect(&config).unwrap();
            assert_eq!(json!(connector.public_configuration()), json!({"auth_type":mode}));
        }
    }

    #[test]
    fn rejected_errors_preserve_codes_and_status_without_upstream_secrets() {
        for (upstream, expected) in [
            (400, StatusCode::BAD_REQUEST),
            (404, StatusCode::NOT_FOUND),
            (401, StatusCode::BAD_GATEWAY),
            (500, StatusCode::BAD_GATEWAY),
        ] {
            let error: ConnectorError = ElasticsearchError::Rejected {
                status: upstream,
                detail: "password=secret".to_owned(),
            }
            .into();
            assert_eq!(error.status, expected);
            assert_eq!(error.code, "elasticsearch_rejected_request");
            assert!(!error.message.contains("secret"));
        }
    }
}
