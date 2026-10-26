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
            ElasticsearchError::Rejected {
                status,
                kind,
                detail,
            } => {
                let (status, message) = rejected_message(status, &kind, &detail);
                (status, "elasticsearch_rejected_request", message.to_owned())
            }
            ElasticsearchError::InvalidIndex => (
                StatusCode::BAD_REQUEST,
                "invalid_search_parameter",
                "One or more search parameters are invalid or exceed configured limits".to_owned(),
            ),
            ElasticsearchError::UnsortableField(field) => (
                StatusCode::BAD_REQUEST,
                "elasticsearch_unsortable_field",
                format!(
                    "Field `{field}` cannot be sorted in every matched index; text and object fields are not sortable, so use a keyword field such as `{field}.keyword`"
                ),
            ),
            ElasticsearchError::IncompatibleField(field) => (
                StatusCode::BAD_REQUEST,
                "elasticsearch_incompatible_field",
                format!(
                    "Field `{field}` has incompatible types across the indices matched by this pattern; sort by another field or narrow the pattern"
                ),
            ),
            ElasticsearchError::TimeFieldConflict(field) => (
                StatusCode::BAD_REQUEST,
                "elasticsearch_time_field_conflict",
                format!(
                    "No usable index matched by this pattern maps `{field}` as a date; choose another time field or narrow the pattern"
                ),
            ),
            ElasticsearchError::PartialResults => (
                StatusCode::BAD_GATEWAY,
                "elasticsearch_partial_results",
                "Elasticsearch could not search every shard matched by this pattern; narrow the pattern or retry".to_owned(),
            ),
            ElasticsearchError::Request => (
                StatusCode::BAD_GATEWAY,
                "elasticsearch_unreachable",
                "Could not complete the Elasticsearch request; it may have timed out".to_owned(),
            ),
            ElasticsearchError::ClientConfiguration | ElasticsearchError::InvalidResponse => (
                StatusCode::BAD_GATEWAY,
                "elasticsearch_response_error",
                "Elasticsearch returned an unusable response".to_owned(),
            ),
        };
        Self {
            status,
            code,
            message,
        }
    }
}

/// Maps Elasticsearch error types to curated messages; upstream details are never returned.
fn rejected_message(status: u16, kind: &str, detail: &str) -> (StatusCode, &'static str) {
    let detail = detail.to_ascii_lowercase();
    let fallback = match status {
        404 => StatusCode::NOT_FOUND,
        400 => StatusCode::BAD_REQUEST,
        _ => StatusCode::BAD_GATEWAY,
    };
    match kind {
        "index_not_found_exception" => (
            StatusCode::NOT_FOUND,
            "No Elasticsearch index matches this Data View pattern",
        ),
        "index_closed_exception" => (
            StatusCode::BAD_REQUEST,
            "The Data View pattern matches a closed index; exclude it or open it in Elasticsearch",
        ),
        "too_many_buckets_exception" => (
            StatusCode::BAD_REQUEST,
            "The histogram has too many buckets; choose a larger interval or a shorter time range",
        ),
        "search_timeout_exception" => (
            StatusCode::GATEWAY_TIMEOUT,
            "Elasticsearch search timed out; narrow the time range, query, or pattern",
        ),
        "es_rejected_execution_exception" | "circuit_breaking_exception" => (
            StatusCode::SERVICE_UNAVAILABLE,
            "Elasticsearch is overloaded; retry or narrow the time range",
        ),
        _ if status == 429 => (
            StatusCode::SERVICE_UNAVAILABLE,
            "Elasticsearch is overloaded; retry or narrow the time range",
        ),
        _ if detail.contains("fielddata") || detail.contains("text fields are not optimised") => (
            StatusCode::BAD_REQUEST,
            "Text fields cannot be sorted or used for breakdowns; use a keyword field such as `<field>.keyword`",
        ),
        _ if detail.contains("no mapping found") => (
            StatusCode::BAD_REQUEST,
            "A sort or breakdown field is not mapped in every index matched by this pattern",
        ),
        _ if detail.contains("incompatible") || detail.contains("different type") => (
            StatusCode::BAD_REQUEST,
            "A field has different types across the indices matched by this pattern; choose another field or narrow the pattern",
        ),
        "query_shard_exception" | "parse_exception" | "query_parsing_exception"
            if detail.contains("query") =>
        {
            (
                StatusCode::BAD_REQUEST,
                "Elasticsearch could not parse the query; check the Lucene syntax",
            )
        }
        _ => (fallback, "Elasticsearch rejected the request"),
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
            assert_eq!(
                json!(connector.public_configuration()),
                json!({"auth_type":mode})
            );
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
                kind: "some_exception".to_owned(),
                detail: "password=secret".to_owned(),
            }
            .into();
            assert_eq!(error.status, expected);
            assert_eq!(error.code, "elasticsearch_rejected_request");
            assert!(!error.message.contains("secret"));
        }
    }

    #[test]
    fn rejected_errors_use_curated_messages_by_error_type() {
        for (status, kind, detail, expected_status, expected) in [
            (404, "index_not_found_exception", "no such index [x]", StatusCode::NOT_FOUND, "No Elasticsearch index"),
            (400, "illegal_argument_exception", "Text fields are not optimised for operations", StatusCode::BAD_REQUEST, "Text fields cannot be sorted"),
            (400, "query_shard_exception", "No mapping found for [x] in order to sort on", StatusCode::BAD_REQUEST, "not mapped in every index"),
            (400, "query_shard_exception", "Failed to parse query [a AND]", StatusCode::BAD_REQUEST, "could not parse the query"),
            (429, "", "", StatusCode::SERVICE_UNAVAILABLE, "overloaded"),
            (500, "search_timeout_exception", "Time exceeded", StatusCode::GATEWAY_TIMEOUT, "timed out"),
        ] {
            let error: ConnectorError = ElasticsearchError::Rejected {
                status,
                kind: kind.to_owned(),
                detail: detail.to_owned(),
            }
            .into();
            assert_eq!(error.status, expected_status, "{kind}: {detail}");
            assert!(error.message.contains(expected), "{}", error.message);
            assert!(!error.message.contains(detail) || detail.is_empty());
        }
    }
}
