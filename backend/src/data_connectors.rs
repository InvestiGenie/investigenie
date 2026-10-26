use axum::http::StatusCode;
use serde::Serialize;
use serde_json::{Map, Value};
use std::{future::Future, pin::Pin};

pub type ConnectionConfig = Map<String, Value>;
pub type ConnectorFuture<'a, T> =
    Pin<Box<dyn Future<Output = Result<T, ConnectorError>> + Send + 'a>>;

pub trait Connector: Send + Sync {
    fn public_configuration(&self) -> ConnectionConfig {
        ConnectionConfig::new()
    }
    fn test_connection(&self) -> ConnectorFuture<'_, ()>;
    fn indices(&self) -> ConnectorFuture<'_, Vec<IndexSummary>>;
    fn fields<'a>(&'a self, index: &'a str) -> ConnectorFuture<'a, Vec<FieldSummary>>;
    fn search(&self, request: crate::search::SearchRequest) -> ConnectorFuture<'_, Value>;
    fn document<'a>(&'a self, index: &'a str, id: &'a str) -> ConnectorFuture<'a, Value>;
}

#[derive(Debug)]
pub struct ConnectorError {
    pub status: StatusCode,
    pub code: &'static str,
    pub message: String,
}

impl ConnectorError {
    pub fn invalid_connection() -> Self {
        Self {
            status: StatusCode::BAD_REQUEST,
            code: "invalid_connection",
            message: "Connection details are invalid or incomplete".to_owned(),
        }
    }
}

#[derive(Debug, Serialize)]
pub struct IndexSummary {
    pub name: String,
    pub document_count: Option<String>,
    pub health: Option<String>,
}

#[derive(Debug, Serialize, PartialEq, Eq)]
pub struct FieldSummary {
    pub name: String,
    pub field_type: String,
    pub is_time_field: bool,
}

include!(concat!(env!("OUT_DIR"), "/data_connectors.rs"));

#[derive(Debug, Clone, Serialize)]
pub struct IntegrationDescriptor {
    pub id: &'static str,
    pub display_name: &'static str,
    pub icon: &'static str,
    pub query_language: &'static str,
    pub description: &'static str,
    pub connection_fields: Vec<Value>,
    pub query: Value,
}

pub fn string_setting(
    config: &ConnectionConfig,
    name: &str,
) -> Result<Option<String>, ConnectorError> {
    match config.get(name) {
        None | Some(Value::Null) => Ok(None),
        Some(Value::String(value)) => Ok(Some(value.clone())),
        _ => Err(ConnectorError::invalid_connection()),
    }
}

/// Reads an optional credential. Forms submit untouched secret inputs as empty strings, so blank means unset.
pub fn credential_setting(
    config: &ConnectionConfig,
    name: &str,
) -> Result<Option<String>, ConnectorError> {
    Ok(string_setting(config, name)?.filter(|value| !value.trim().is_empty()))
}

/// Reads optional basic credentials: no username means no authentication, and a username may use an empty password.
pub fn basic_credentials(
    config: &ConnectionConfig,
) -> Result<Option<(String, String)>, ConnectorError> {
    let password = string_setting(config, "password")?.filter(|value| !value.is_empty());
    match (credential_setting(config, "username")?, password) {
        (Some(username), password) => Ok(Some((username, password.unwrap_or_default()))),
        (None, None) => Ok(None),
        (None, Some(_)) => Err(ConnectorError::invalid_connection()),
    }
}

pub fn editable_configuration(
    descriptor: &IntegrationDescriptor,
    existing: &ConnectionConfig,
    updates: &ConnectionConfig,
) -> Result<ConnectionConfig, ConnectorError> {
    let mut config = existing.clone();
    for field in &descriptor.connection_fields {
        if field["editable"] == true {
            if let Some(name) = field["name"].as_str() {
                if let Some(value) = updates.get(name) {
                    if field["secret"] == true && !value.is_string() {
                        return Err(ConnectorError::invalid_connection());
                    }
                    config.insert(name.to_owned(), value.clone());
                }
            }
        }
    }
    for field in &descriptor.connection_fields {
        if let (Some(name), Some(selector)) = (
            field["name"].as_str(),
            field["visible_when"]["field"].as_str(),
        ) {
            if descriptor
                .connection_fields
                .iter()
                .any(|candidate| candidate["name"] == selector && candidate["editable"] == true)
                && updates
                    .get(selector)
                    .is_some_and(|value| value != &field["visible_when"]["value"])
            {
                config.remove(name);
            }
        }
    }
    Ok(config)
}

pub fn public_configuration(
    descriptor: &IntegrationDescriptor,
    config: &ConnectionConfig,
) -> ConnectionConfig {
    descriptor
        .connection_fields
        .iter()
        .filter(|field| field["secret"] == false)
        .filter_map(|field| {
            let name = field["name"].as_str()?;
            Some((name.to_owned(), config.get(name)?.clone()))
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn registry_ids_are_unique_and_match_folders() {
        validate_registry();
    }

    #[test]
    fn data_registry_preserves_source_ids_and_excludes_ai_transports() {
        let ids = registered()
            .into_iter()
            .map(|item| item.id)
            .collect::<Vec<_>>();
        assert!(ids.contains(&"elasticsearch"));
        assert!(ids.contains(&"clickhouse"));
        assert!(!ids.contains(&"openai"));
        assert!(connect("openai", &ConnectionConfig::new()).is_err());
    }

    #[test]
    fn blank_credentials_mean_no_authentication_for_every_optional_auth_connector() {
        for (id, url) in [
            ("prometheus", "http://localhost:9090"),
            ("victoriametrics", "http://localhost:8428"),
            ("victorialogs", "http://localhost:9428"),
            ("loki", "http://localhost:3100"),
            ("clickhouse", "http://localhost:8123"),
            ("elasticsearch", "http://localhost:9200"),
        ] {
            for credentials in [
                json!({"username":"","password":""}),
                json!({"username":"  ","password":""}),
                json!({"username":"","password":"","bearer_token":"","api_key":""}),
            ] {
                let mut config = credentials.as_object().unwrap().clone();
                config.insert("url".to_owned(), json!(url));
                assert!(connect(id, &config).is_ok(), "{id}: {credentials}");
            }
            if id != "elasticsearch" && id != "loki" {
                let config = json!({"url":url,"username":"reader","password":""});
                assert!(connect(id, config.as_object().unwrap()).is_ok(), "{id}");
            }
        }
        let config = json!({"username":"","password":"secret"});
        assert!(basic_credentials(config.as_object().unwrap()).is_err());
        let config = json!({"username":"reader","password":""});
        assert_eq!(
            basic_credentials(config.as_object().unwrap()).unwrap(),
            Some(("reader".to_owned(), String::new()))
        );
    }

    #[test]
    fn editable_configuration_replaces_strings_and_retains_omitted_credentials() {
        let descriptor = elasticsearch::descriptor();
        let existing = json!({"auth_type":"basic","username":"user","password":" original ","tls_insecure":false});
        let existing = existing.as_object().unwrap();
        for updates in [json!({}), json!({"auth_type":"basic"})] {
            assert_eq!(
                editable_configuration(&descriptor, existing, updates.as_object().unwrap())
                    .unwrap(),
                *existing
            );
        }
        let updates = json!({"password":"","tls_insecure":true,"unknown":"ignored"});
        let replaced =
            editable_configuration(&descriptor, existing, updates.as_object().unwrap()).unwrap();
        assert_eq!(replaced["password"], "");
        assert_eq!(replaced["username"], "user");
        assert_eq!(replaced["tls_insecure"], false);
        assert!(!replaced.contains_key("unknown"));
        for malformed in [Value::Null, json!(false), json!(1), json!([]), json!({})] {
            let updates = json!({"password":malformed});
            assert!(
                editable_configuration(&descriptor, existing, updates.as_object().unwrap())
                    .is_err()
            );
        }
        for mode in ["none", "api_key"] {
            let updates = json!({"auth_type":mode,"api_key":" new key "});
            let switched =
                editable_configuration(&descriptor, existing, updates.as_object().unwrap())
                    .unwrap();
            assert!(!switched.contains_key("username"));
            assert!(!switched.contains_key("password"));
            assert_eq!(
                switched.get("api_key"),
                (mode == "api_key").then_some(&json!(" new key "))
            );
        }
    }

    #[test]
    fn conditional_edits_and_public_filtering_are_metadata_driven() {
        let mut descriptor = clickhouse::descriptor();
        descriptor.connection_fields = vec![
            json!({"name":"mode","secret":false,"editable":true}),
            json!({"name":"token","secret":true,"editable":true,"visible_when":{"field":"mode","value":"token"}}),
            json!({"name":"label","secret":false,"editable":true}),
        ];
        let existing = json!({"mode":"token","token":"saved","unlisted":"private"});
        let updates = json!({"mode":"anonymous","token":"replacement","label":"public"});
        let config = editable_configuration(
            &descriptor,
            existing.as_object().unwrap(),
            updates.as_object().unwrap(),
        )
        .unwrap();
        assert!(!config.contains_key("token"));
        assert_eq!(config["unlisted"], "private");
        let computed =
            json!({"mode":"token","token":"secret","unlisted":"private","label":"public"});
        assert_eq!(
            json!(public_configuration(
                &descriptor,
                computed.as_object().unwrap()
            )),
            json!({"mode":"token","label":"public"})
        );
    }

    #[test]
    fn catalog_metadata_matches_frontend_contract() {
        for descriptor in registered() {
            let serialized = serde_json::to_value(&descriptor).unwrap();
            for key in ["id", "display_name", "query_language", "description"] {
                assert!(serialized[key].is_string());
            }
            for field in &descriptor.connection_fields {
                for key in ["name", "label", "placeholder"] {
                    assert!(field[key].is_string(), "{field}");
                }
                for key in ["required", "secret", "editable"] {
                    assert!(field[key].is_boolean(), "{field}");
                }
                if let Some(group) = field.get("group") {
                    assert!(matches!(
                        group.as_str(),
                        Some("authentication" | "credentials")
                    ));
                }
                if field["secret"] == true {
                    assert_eq!(field["editable"], true);
                    assert_eq!(field["group"], "credentials");
                }
                assert!(matches!(
                    field["input_type"].as_str(),
                    Some("text" | "url" | "password" | "checkbox" | "select")
                ));
            }
            for key in [
                "placeholder",
                "default_query",
                "field_query",
                "identifier_quote",
                "help",
                "schema_label",
            ] {
                assert!(descriptor.query[key].is_string());
            }
            for key in ["sort_requires_projection", "histogram_total_from_buckets"] {
                assert!(descriptor.query[key].is_boolean());
            }
            assert!(matches!(
                descriptor.query["document_mode"].as_str(),
                Some("lookup" | "inline")
            ));
            let url = descriptor
                .connection_fields
                .iter()
                .find(|field| field["name"] == "url")
                .unwrap();
            assert_eq!(url["required"], true);
            assert_eq!(url["editable"], true);
        }
        let es = elasticsearch::descriptor();
        assert_eq!(
            es.query,
            json!({"placeholder":"Enter a Lucene query, e.g. response.status_code:500","default_query":"","field_query":"{query} AND {field}:","identifier_quote":"","help":"Lucene query runs against the selected Elasticsearch data view","document_mode":"lookup","sort_requires_projection":false,"histogram_total_from_buckets":false,"schema_label":"Elasticsearch mappings"})
        );
        let selector = es
            .connection_fields
            .iter()
            .find(|field| field["name"] == "auth_type")
            .unwrap();
        assert_eq!(selector["editable"], true);
        assert_eq!(selector["group"], "authentication");
        assert_eq!(
            selector["options"],
            json!([{"value":"none","label":"None"},{"value":"basic","label":"Basic"},{"value":"api_key","label":"API key"}])
        );
        for (name, value) in [
            ("username", "basic"),
            ("password", "basic"),
            ("api_key", "api_key"),
        ] {
            let field = es
                .connection_fields
                .iter()
                .find(|field| field["name"] == name)
                .unwrap();
            assert_eq!(
                field["visible_when"],
                json!({"field":"auth_type","value":value})
            );
            assert_eq!(field["secret"], true);
        }
        let tls = es
            .connection_fields
            .iter()
            .find(|field| field["name"] == "tls_insecure")
            .unwrap();
        assert_eq!(tls["input_type"], "checkbox");
        assert_eq!(tls["editable"], false);
        assert_eq!(tls["group"], "authentication");
        let ch = clickhouse::descriptor();
        assert_eq!(
            ch.query,
            json!({"placeholder":"SELECT * FROM {index} LIMIT 50","default_query":"SELECT * FROM {index} LIMIT 50","field_query":"SELECT {field} FROM {index}","identifier_quote":"`","help":"Read-only SQL runs against the selected ClickHouse table","document_mode":"inline","sort_requires_projection":true,"histogram_total_from_buckets":true,"schema_label":"ClickHouse table schema"})
        );
        assert!(!ch
            .connection_fields
            .iter()
            .any(|field| field["name"] == "auth_type"));
        assert_eq!(
            ch.connection_fields
                .iter()
                .find(|field| field["name"] == "database")
                .unwrap()["editable"],
            true
        );
    }
}
