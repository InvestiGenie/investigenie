use super::{LokiClient, LokiError, LokiSettings};
use crate::data_connectors::{
    credential_setting, string_setting, ConnectionConfig, Connector, ConnectorError,
    ConnectorFuture, FieldSummary, IndexSummary, InvestigationFilter,
    InvestigationFilterOperator,
};
use crate::search::SearchRequest;
use axum::http::StatusCode;
use serde_json::{json, Value};
use std::sync::Arc;

pub fn connect(config: &ConnectionConfig) -> Result<Arc<dyn Connector>, ConnectorError> {
    let raw_url = string_setting(config, "url")?.ok_or_else(ConnectorError::invalid_connection)?;
    let url = url::Url::parse(&raw_url).map_err(|_| ConnectorError::invalid_connection())?;
    let username = credential_setting(config, "username")?;
    let password = string_setting(config, "password")?.filter(|value| !value.is_empty());
    let bearer_token = credential_setting(config, "bearer_token")?;
    let auth_type = string_setting(config, "auth_type")?.unwrap_or_else(|| {
        if bearer_token.is_some() {
            "bearer".to_owned()
        } else if username.is_some() || password.is_some() {
            "basic".to_owned()
        } else {
            "none".to_owned()
        }
    });
    // Hidden credential inputs for other modes may be submitted empty; only the selected mode's values matter.
    let (username, password, bearer_token) = match auth_type.as_str() {
        "none" => (None, None, None),
        "basic" => (username, password, None),
        "bearer" => (None, None, bearer_token),
        _ => (username, password, bearer_token),
    };
    Ok(Arc::new(LokiClient::new(LokiSettings {
        url,
        auth_type,
        username,
        password,
        bearer_token,
        tenant_id: string_setting(config, "tenant_id")?,
    })?))
}

impl Connector for LokiClient {
    fn public_configuration(&self) -> ConnectionConfig {
        let mut configuration = ConnectionConfig::new();
        configuration.insert("auth_type".to_owned(), json!(self.auth_type));
        if let Some(tenant_id) = self.tenant_id.as_deref() {
            configuration.insert("tenant_id".to_owned(), json!(tenant_id));
        }
        configuration
    }

    fn test_connection(&self) -> ConnectorFuture<'_, ()> {
        Box::pin(async move { Ok(LokiClient::test_connection(self).await?) })
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
        Box::pin(async move { Ok(LokiClient::fields(self, index).await?) })
    }

    fn search(&self, request: SearchRequest) -> ConnectorFuture<'_, Value> {
        Box::pin(async move { Ok(LokiClient::search(self, request).await?) })
    }

    fn investigation_filter_operators(
        &self,
        field_type: &str,
    ) -> Vec<InvestigationFilterOperator> {
        match field_type {
            "label" => vec![InvestigationFilterOperator::Eq],
            "text" => vec![InvestigationFilterOperator::Contains],
            _ => Vec::new(),
        }
    }

    fn investigation_supports_aggregation_field(&self, field_type: &str) -> bool {
        field_type == "label"
    }

    fn build_investigation_query(
        &self,
        index: &str,
        filters: &[InvestigationFilter],
    ) -> Result<String, ConnectorError> {
        super::validate_index(index)?;
        let mut labels = Vec::new();
        let mut line_filters = Vec::new();
        for filter in filters {
            if !crate::search::safe_field_name(&filter.field) {
                return Err(invalid_investigation_filter());
            }
            match (&*filter.field_type, filter.operator, &filter.value) {
                ("label", InvestigationFilterOperator::Eq, value) => {
                    labels.push(format!(
                        "{}=\"{}\"",
                        filter.field,
                        escape_logql_string(&investigation_value(value)?)
                    ));
                }
                ("text", InvestigationFilterOperator::Contains, Value::String(value)) => {
                    line_filters.push(format!(" |= \"{}\"", escape_logql_string(value)));
                }
                _ => return Err(invalid_investigation_filter()),
            }
        }
        let selector = if labels.is_empty() {
            "{}".to_owned()
        } else {
            format!("{{{}}}", labels.join(","))
        };
        let query = format!("{selector}{}", line_filters.join(""));
        if query.len() > super::MAX_QUERY_BYTES {
            return Err(invalid_investigation_filter());
        }
        Ok(query)
    }

    fn document<'a>(&'a self, _index: &'a str, _id: &'a str) -> ConnectorFuture<'a, Value> {
        Box::pin(async { Err(ConnectorError::invalid_connection()) })
    }
}

fn investigation_value(value: &Value) -> Result<String, ConnectorError> {
    match value {
        Value::String(value) => Ok(value.clone()),
        Value::Number(value) => Ok(value.to_string()),
        Value::Bool(value) => Ok(value.to_string()),
        _ => Err(invalid_investigation_filter()),
    }
}

fn escape_logql_string(value: &str) -> String {
    value
        .replace('\\', "\\\\")
        .replace('"', "\\\"")
        .replace('\n', "\\n")
        .replace('\r', "\\r")
}

fn invalid_investigation_filter() -> ConnectorError {
    ConnectorError {
        status: StatusCode::BAD_REQUEST,
        code: "invalid_investigation_filter",
        message: "The investigation filter is not supported by this data source".to_owned(),
    }
}

#[cfg(test)]
mod investigation_tests {
    use super::*;

    #[test]
    fn investigation_query_uses_only_validated_logql_filter_shapes() {
        let config = serde_json::from_value(json!({"url":"http://localhost:3100"})).unwrap();
        let connector = connect(&config).unwrap();
        let query = connector
            .build_investigation_query(
                "logs",
                &[
                    InvestigationFilter {
                        field: "app".to_owned(),
                        field_type: "label".to_owned(),
                        operator: InvestigationFilterOperator::Eq,
                        value: json!("api"),
                    },
                    InvestigationFilter {
                        field: "_msg".to_owned(),
                        field_type: "text".to_owned(),
                        operator: InvestigationFilterOperator::Contains,
                        value: json!("database \"timeout\""),
                    },
                ],
            )
            .unwrap();
        assert_eq!(query, "{app=\"api\"} |= \"database \\\"timeout\\\"\"");
        assert!(connector
            .build_investigation_query(
                "logs",
                &[InvestigationFilter {
                    field: "app".to_owned(),
                    field_type: "label".to_owned(),
                    operator: InvestigationFilterOperator::Gt,
                    value: json!(1),
                }]
            )
            .is_err());
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
