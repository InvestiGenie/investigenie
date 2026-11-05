use super::{
    ElasticsearchAuthentication, ElasticsearchClient, ElasticsearchError, ElasticsearchSettings,
};
use crate::data_connectors::{
    string_setting, ConnectionConfig, Connector, ConnectorError, ConnectorFuture, FieldSummary,
    IndexSummary, InvestigationFilter, InvestigationFilterOperator,
    InvestigationGroupCountRequest, InvestigationGroupCountResult,
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
    fn investigation_filter_operators(
        &self,
        field_type: &str,
    ) -> Vec<InvestigationFilterOperator> {
        match field_type.trim().to_ascii_lowercase().as_str() {
            "keyword" | "constant_keyword" | "wildcard" | "text" => vec![
                InvestigationFilterOperator::Eq,
                InvestigationFilterOperator::Contains,
            ],
            "long" | "integer" | "short" | "byte" | "double" | "float" | "half_float"
            | "scaled_float" | "unsigned_long" | "date" | "date_nanos" => vec![
                InvestigationFilterOperator::Eq,
                InvestigationFilterOperator::Gt,
                InvestigationFilterOperator::Gte,
                InvestigationFilterOperator::Lt,
                InvestigationFilterOperator::Lte,
            ],
            "boolean" | "ip" | "version" => vec![InvestigationFilterOperator::Eq],
            _ => Vec::new(),
        }
    }
    fn investigation_supports_aggregation_field(&self, field_type: &str) -> bool {
        matches!(
            field_type.trim().to_ascii_lowercase().as_str(),
            "keyword"
                | "constant_keyword"
                | "boolean"
                | "ip"
                | "long"
                | "integer"
                | "short"
                | "byte"
                | "double"
                | "float"
                | "half_float"
                | "scaled_float"
                | "unsigned_long"
                | "version"
        )
    }
    fn build_investigation_query(
        &self,
        _index: &str,
        filters: &[InvestigationFilter],
    ) -> Result<String, ConnectorError> {
        filters
            .iter()
            .map(|filter| {
                if !self
                    .investigation_filter_operators(&filter.field_type)
                    .contains(&filter.operator)
                {
                    return Err(invalid_investigation_filter());
                }
                elasticsearch_filter(filter)
            })
            .collect::<Result<Vec<_>, _>>()
            .map(|clauses| clauses.join(" AND "))
    }
    fn investigation_supports_group_count(&self) -> bool {
        true
    }
    fn investigation_group_count(
        &self,
        request: InvestigationGroupCountRequest,
    ) -> ConnectorFuture<'_, InvestigationGroupCountResult> {
        Box::pin(async move {
            if request.group_by.is_empty()
                || request.group_by.len() > 3
                || !(2..=1_000_000_000).contains(&request.minimum_count)
                || !(1..=100).contains(&request.limit)
                || request.time_range.is_some_and(|range| !range.is_valid())
            {
                return Err(ElasticsearchError::InvalidIndex.into());
            }
            super::validate_index(&request.index)?;
            let fields = self.fields(&request.index).await?;
            let field_types = fields
                .iter()
                .map(|field| (field.name.as_str(), field.field_type.as_str()))
                .collect::<std::collections::BTreeMap<_, _>>();
            for field in &request.group_by {
                if !crate::search::safe_field_name(field)
                    || field == "_ig_duplicate_count"
                    || !field_types
                        .get(field.as_str())
                        .is_some_and(|field_type| is_aggregatable_type(field_type))
                {
                    return Err(ElasticsearchError::InvalidIndex.into());
                }
                let field_capabilities = self
                    .field_capabilities(&SearchRequest {
                        index: request.index.clone(),
                        sort_by: Some(field.clone()),
                        ..Default::default()
                    })
                    .await
                    .ok_or(ElasticsearchError::InvalidIndex)?;
                let caps = field_capabilities
                    .get(field)
                    .ok_or(ElasticsearchError::InvalidIndex)?;
                if caps.is_empty()
                    || caps
                        .iter()
                        .any(|capability| !capability.aggregatable || !is_aggregatable_type(&capability.field_type))
                {
                    return Err(ElasticsearchError::InvalidIndex.into());
                }
            }
            for filter in &request.filters {
                if !field_types
                    .get(filter.field.as_str())
                    .is_some_and(|field_type| *field_type == filter.field_type)
                    || !self
                        .investigation_filter_operators(&filter.field_type)
                        .contains(&filter.operator)
                {
                    return Err(ElasticsearchError::InvalidIndex.into());
                }
            }
            if request.time_range.is_some() {
                if !fields.iter().any(|field| {
                    field.name == request.time_field && field.is_time_field
                }) {
                    return Err(ElasticsearchError::InvalidIndex.into());
                }
            }
            let mut filters = request
                .filters
                .iter()
                .map(elasticsearch_filter_query)
                .collect::<Result<Vec<_>, _>>()?;
            if let Some(range) = request.time_range {
                filters.push(json!({"range": {(request.time_field.clone()): {
                    "gte": range.start,
                    "lte": range.end,
                    "format": "epoch_millis"
                }}}));
            }
            let query = if filters.is_empty() {
                json!({"match_all":{}})
            } else {
                json!({"bool":{"filter":filters}})
            };
            let group_aggregation = if request.group_by.len() == 1 {
                json!({"terms":{
                    "field":request.group_by[0],
                    "size":request.limit + 1,
                    "min_doc_count":request.minimum_count,
                    "order":{"_count":"desc"}
                }})
            } else {
                json!({"multi_terms":{
                    "terms":request.group_by.iter().map(|field| json!({"field":field})).collect::<Vec<_>>(),
                    "size":request.limit + 1,
                    "min_doc_count":request.minimum_count,
                    "order":{"_count":"desc"}
                }})
            };
            let body = json!({
                "size":0,
                "track_total_hits":false,
                "query":query,
                "aggs":{"_ig_duplicate_groups":group_aggregation}
            });
            let url = self.url_for(&[request.index.as_str(), "_search"])?;
            let response = self
                .client
                .post(url)
                .json(&body)
                .send()
                .await
                .map_err(|_| ElasticsearchError::Request)?;
            let result: Value = super::decode(response).await?;
            let buckets = result
                .pointer("/aggregations/_ig_duplicate_groups/buckets")
                .and_then(Value::as_array)
                .ok_or(ElasticsearchError::InvalidResponse)?;
            let mut rows = Vec::new();
            let mut columns = request.group_by.clone();
            columns.push("_ig_duplicate_count".to_owned());
            for bucket in buckets.iter().take(request.limit) {
                let count = bucket
                    .get("doc_count")
                    .and_then(Value::as_u64)
                    .ok_or(ElasticsearchError::InvalidResponse)?;
                if count < request.minimum_count {
                    continue;
                }
                let key = bucket.get("key").ok_or(ElasticsearchError::InvalidResponse)?;
                let values = if request.group_by.len() == 1 {
                    vec![key.clone()]
                } else {
                    key.as_array()
                        .cloned()
                        .ok_or(ElasticsearchError::InvalidResponse)?
                };
                if values.len() != request.group_by.len() {
                    return Err(ElasticsearchError::InvalidResponse.into());
                }
                let mut row = serde_json::Map::new();
                for (field, value) in request.group_by.iter().zip(values) {
                    row.insert(field.clone(), value);
                }
                row.insert("_ig_duplicate_count".to_owned(), json!(count));
                rows.push(Value::Object(row));
            }
            let truncated = buckets.len() > request.limit
                || result
                    .pointer("/aggregations/_ig_duplicate_groups/sum_other_doc_count")
                    .and_then(Value::as_u64)
                    .is_some_and(|count| count > 0);
            let query = format!(
                "POST /{}/_search\n{}",
                request.index,
                serde_json::to_string_pretty(&body).unwrap_or_default()
            );
            Ok(InvestigationGroupCountResult {
                query_language: "Elasticsearch Query DSL".to_owned(),
                query,
                columns,
                rows,
                truncated,
            })
        })
    }
    fn document<'a>(&'a self, index: &'a str, id: &'a str) -> ConnectorFuture<'a, Value> {
        Box::pin(async move { Ok(json!(self.document(index, id).await?)) })
    }
}

fn is_aggregatable_type(field_type: &str) -> bool {
    matches!(
        field_type,
        "keyword"
            | "constant_keyword"
            | "wildcard"
            | "boolean"
            | "ip"
            | "long"
            | "integer"
            | "short"
            | "byte"
            | "double"
            | "float"
            | "half_float"
            | "scaled_float"
            | "unsigned_long"
            | "version"
    )
}

fn elasticsearch_filter_query(filter: &InvestigationFilter) -> Result<Value, ConnectorError> {
    if !crate::search::safe_field_name(&filter.field) {
        return Err(invalid_investigation_filter());
    }
    let value = match &filter.value {
        Value::String(value) => Value::String(value.clone()),
        Value::Number(value) => Value::Number(value.clone()),
        Value::Bool(value) => Value::Bool(*value),
        _ => return Err(invalid_investigation_filter()),
    };
    Ok(match filter.operator {
        InvestigationFilterOperator::Eq => json!({"term":{(filter.field.clone()):value}}),
        InvestigationFilterOperator::Contains if filter.value.is_string() => {
            let needle = filter.value.as_str().unwrap_or_default();
            if filter.field_type == "text" {
                json!({"match_phrase":{(filter.field.clone()):needle}})
            } else {
                let pattern = format!("*{}*", escape_wildcard(needle));
                json!({"wildcard":{(filter.field.clone()):{"value":pattern}}})
            }
        }
        InvestigationFilterOperator::Gt => json!({"range":{(filter.field.clone()):{"gt":value}}}),
        InvestigationFilterOperator::Gte => json!({"range":{(filter.field.clone()):{"gte":value}}}),
        InvestigationFilterOperator::Lt => json!({"range":{(filter.field.clone()):{"lt":value}}}),
        InvestigationFilterOperator::Lte => json!({"range":{(filter.field.clone()):{"lte":value}}}),
        InvestigationFilterOperator::Contains => return Err(invalid_investigation_filter()),
    })
}

fn escape_wildcard(value: &str) -> String {
    value
        .replace('\\', "\\\\")
        .replace('*', "\\*")
        .replace('?', "\\?")
}


fn elasticsearch_filter(filter: &InvestigationFilter) -> Result<String, ConnectorError> {
    if !crate::search::safe_field_name(&filter.field) {
        return Err(invalid_investigation_filter());
    }
    let value = filter
        .value
        .as_str()
        .map(str::to_owned)
        .unwrap_or_else(|| filter.value.to_string());
    let escaped = value
        .chars()
        .flat_map(|character| {
            if "\\+-=&|><!(){}[]^\"~*?:/".contains(character) {
                vec!['\\', character]
            } else {
                vec![character]
            }
        })
        .collect::<String>();
    Ok(match filter.operator {
        InvestigationFilterOperator::Eq => format!("{}:\"{}\"", filter.field, escaped),
        InvestigationFilterOperator::Contains if filter.value.is_string() => {
            format!("{}:*{}*", filter.field, escaped)
        }
        InvestigationFilterOperator::Contains => return Err(invalid_investigation_filter()),
        InvestigationFilterOperator::Gt => format!("{}:{{{} TO *]", filter.field, escaped),
        InvestigationFilterOperator::Gte => format!("{}:[{} TO *]", filter.field, escaped),
        InvestigationFilterOperator::Lt => format!("{}:[* TO {}}}", filter.field, escaped),
        InvestigationFilterOperator::Lte => format!("{}:[* TO {}]", filter.field, escaped),
    })
}

fn invalid_investigation_filter() -> ConnectorError {
    ConnectorError {
        status: StatusCode::BAD_REQUEST,
        code: "invalid_investigation_filter",
        message: "The investigation filter is not supported by this data source".to_owned(),
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
    fn investigation_filters_use_supported_types_and_escape_wildcards() {
        let config = serde_json::from_value(json!({"url":"http://localhost:9200"})).unwrap();
        let connector = connect(&config).unwrap();
        assert!(connector
            .investigation_filter_operators("keyword")
            .contains(&InvestigationFilterOperator::Contains));
        assert!(!connector
            .investigation_filter_operators("object")
            .contains(&InvestigationFilterOperator::Eq));
        let query = connector
            .build_investigation_query(
                "events",
                &[InvestigationFilter {
                    field: "login".to_owned(),
                    field_type: "keyword".to_owned(),
                    operator: InvestigationFilterOperator::Contains,
                    value: json!("user*name"),
                }],
            )
            .unwrap();
        assert_eq!(query, "login:*user\\*name*");
    }

    #[tokio::test]
    async fn grouped_count_uses_bounded_terms_aggregation_and_reports_truncation() {
        use wiremock::{matchers, Mock, MockServer, ResponseTemplate};

        let server = MockServer::start().await;
        Mock::given(matchers::method("GET"))
            .and(matchers::path("/events/_field_caps"))
            .and(matchers::query_param("fields", "*"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "fields": {
                    "timestamp":{"date":{"type":"date","aggregatable":true,"metadata_field":false}},
                    "login":{"keyword":{"type":"keyword","aggregatable":true,"metadata_field":false}}
                }
            })))
            .expect(1)
            .mount(&server)
            .await;
        Mock::given(matchers::method("GET"))
            .and(matchers::path("/events/_field_caps"))
            .and(matchers::query_param("fields", "login"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "fields": {
                    "login":{"keyword":{"type":"keyword","aggregatable":true,"metadata_field":false}}
                }
            })))
            .expect(1)
            .mount(&server)
            .await;
        Mock::given(matchers::method("POST"))
            .and(matchers::path("/events/_search"))
            .and(|request: &wiremock::Request| {
                let body: Value = serde_json::from_slice(&request.body).unwrap();
                body["size"] == 0
                    && body["aggs"]["_ig_duplicate_groups"]["terms"]["field"] == "login"
                    && body["aggs"]["_ig_duplicate_groups"]["terms"]["size"] == 2
                    && body["aggs"]["_ig_duplicate_groups"]["terms"]["min_doc_count"] == 2
            })
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "timed_out":false,
                "_shards":{"failed":0},
                "aggregations":{"_ig_duplicate_groups":{
                    "buckets":[
                        {"key":"alice","doc_count":3},
                        {"key":"bob","doc_count":2}
                    ],
                    "sum_other_doc_count":0
                }}
            })))
            .expect(1)
            .mount(&server)
            .await;
        let config = serde_json::from_value(json!({"url":server.uri()})).unwrap();
        let connector = connect(&config).unwrap();
        let result = connector
            .investigation_group_count(InvestigationGroupCountRequest {
                index: "events".to_owned(),
                time_field: "timestamp".to_owned(),
                filters: Vec::new(),
                group_by: vec!["login".to_owned()],
                minimum_count: 2,
                limit: 1,
                time_range: None,
            })
            .await
            .unwrap();
        assert_eq!(result.query_language, "Elasticsearch Query DSL");
        assert!(result.query.contains("/_search"));
        assert_eq!(result.columns, ["login", "_ig_duplicate_count"]);
        assert_eq!(result.rows, [json!({"login":"alice","_ig_duplicate_count":3})]);
        assert!(result.truncated);
    }

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
