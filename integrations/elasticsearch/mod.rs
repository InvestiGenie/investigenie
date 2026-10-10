use std::time::Duration;

use base64::{engine::general_purpose::STANDARD as BASE64, Engine};
use reqwest::{header, Client, Response, Url};
use serde::{de::DeserializeOwned, Deserialize, Serialize};
use serde_json::{json, Value};
use thiserror::Error;

use crate::integrations::IntegrationDescriptor;
pub use crate::integrations::{FieldSummary, IndexSummary};
pub use crate::search::SearchRequest;
mod connector;
pub use connector::connect;

#[derive(Clone)]
pub struct ElasticsearchSettings {
    pub url: Url,
    pub authentication: Option<ElasticsearchAuthentication>,
    pub tls_insecure: bool,
}

#[derive(Clone)]
pub enum ElasticsearchAuthentication {
    ApiKey(String),
    Basic { username: String, password: String },
}

const REQUEST_TIMEOUT: Duration = Duration::from_secs(15);
const SEARCH_TIMEOUT: &str = "10s";
const MAX_ERROR_DETAIL_CHARS: usize = 400;

pub fn descriptor() -> IntegrationDescriptor {
    IntegrationDescriptor {
        id: "elasticsearch",
        display_name: "Elasticsearch",
        query_language: "Lucene",
        description: "Lucene queries over indices and data streams",
        connection_fields: vec![
            json!({"name":"url","label":"URL","input_type":"url","required":true,"secret":false,"editable":true,"placeholder":"http://localhost:9200"}),
            json!({"name":"auth_type","label":"Authentication","input_type":"select","required":false,"secret":false,"editable":true,"group":"authentication","placeholder":"","options":[{"value":"none","label":"None"},{"value":"basic","label":"Basic"},{"value":"api_key","label":"API key"}]}),
            json!({"name":"username","label":"Username","input_type":"text","required":true,"secret":true,"editable":true,"group":"credentials","placeholder":"","visible_when":{"field":"auth_type","value":"basic"}}),
            json!({"name":"password","label":"Password","input_type":"password","required":true,"secret":true,"editable":true,"group":"credentials","placeholder":"","visible_when":{"field":"auth_type","value":"basic"}}),
            json!({"name":"api_key","label":"API key","input_type":"password","required":true,"secret":true,"editable":true,"group":"credentials","placeholder":"","visible_when":{"field":"auth_type","value":"api_key"}}),
            json!({"name":"tls_insecure","label":"Skip TLS verification","input_type":"checkbox","required":false,"secret":false,"editable":false,"group":"authentication","placeholder":""}),
        ],
        query: json!({"placeholder":"Enter a Lucene query, e.g. response.status_code:500","default_query":"","field_query":"{query} AND {field}:","identifier_quote":"","help":"Lucene query runs against the selected Elasticsearch data view","document_mode":"lookup","sort_requires_projection":false,"histogram_total_from_buckets":false,"schema_label":"Elasticsearch mappings"}),
    }
}

#[derive(Clone)]
pub struct ElasticsearchClient {
    base_url: Url,
    client: Client,
    auth_type: &'static str,
}

#[derive(Debug, Error)]
pub enum ElasticsearchError {
    #[error("Could not configure the Elasticsearch HTTP client")]
    ClientConfiguration,
    #[error("Elasticsearch request could not be completed")]
    Request,
    #[error("Elasticsearch returned HTTP {status}: {detail}")]
    Rejected { status: u16, detail: String },
    #[error("Elasticsearch returned an invalid response")]
    InvalidResponse,
    #[error("Index pattern contains unsupported characters")]
    InvalidIndex,
}

#[derive(Debug, Deserialize)]
struct IndexRecord {
    index: String,
    #[serde(rename = "docs.count")]
    document_count: Option<String>,
    health: Option<String>,
}

#[derive(Debug, Deserialize)]
struct SearchEnvelope {
    took: Option<u64>,
    timed_out: Option<bool>,
    hits: SearchHits,
    aggregations: Option<SearchAggregations>,
}

#[derive(Debug, Deserialize)]
struct SearchAggregations {
    documents_over_time: Option<AutoDateHistogram>,
}

#[derive(Debug, Deserialize)]
struct AutoDateHistogram {
    buckets: Vec<ElasticsearchHistogramBucket>,
}

#[derive(Debug, Deserialize)]
struct ElasticsearchHistogramBucket {
    key: i64,
    key_as_string: Option<String>,
    doc_count: u64,
}

#[derive(Debug, Deserialize)]
struct SearchHits {
    total: Option<TotalHits>,
    hits: Vec<SearchHit>,
}

#[derive(Debug, Deserialize)]
#[serde(untagged)]
enum TotalHits {
    Detailed { value: u64, relation: String },
    Count(u64),
}

#[derive(Debug, Deserialize, Serialize)]
pub struct SearchHit {
    #[serde(rename(deserialize = "_index", serialize = "index"))]
    pub index: String,
    #[serde(rename(deserialize = "_id", serialize = "id"))]
    pub id: String,
    #[serde(rename(deserialize = "_score", serialize = "score"))]
    pub score: Option<f64>,
    #[serde(rename(deserialize = "_source", serialize = "source"))]
    pub source: Option<Value>,
}

#[derive(Debug, Serialize)]
pub struct SearchResult {
    pub took: Option<u64>,
    pub timed_out: bool,
    pub total: u64,
    pub total_relation: String,
    pub hits: Vec<SearchHit>,
    pub histogram: Vec<HistogramBucket>,
}

#[derive(Debug, Serialize)]
pub struct HistogramBucket {
    pub timestamp: i64,
    pub label: Option<String>,
    pub count: u64,
}

#[derive(Debug, Deserialize, Serialize)]
pub struct Document {
    #[serde(rename(deserialize = "_index", serialize = "index"))]
    pub index: String,
    #[serde(rename(deserialize = "_id", serialize = "id"))]
    pub id: String,
    pub found: bool,
    #[serde(rename(deserialize = "_source", serialize = "source"))]
    pub source: Option<Value>,
}

impl ElasticsearchClient {
    pub fn new(settings: ElasticsearchSettings) -> Result<Self, ElasticsearchError> {
        let mut builder = Client::builder()
            .timeout(REQUEST_TIMEOUT)
            .danger_accept_invalid_certs(settings.tls_insecure);
        let auth_type = match &settings.authentication {
            None => "none",
            Some(ElasticsearchAuthentication::Basic { .. }) => "basic",
            Some(ElasticsearchAuthentication::ApiKey(_)) => "api_key",
        };
        match settings.authentication {
            Some(ElasticsearchAuthentication::ApiKey(api_key)) => {
                builder =
                    builder.default_headers(authorization_header(format!("ApiKey {api_key}"))?);
            }
            Some(ElasticsearchAuthentication::Basic { username, password }) => {
                let encoded = BASE64.encode(format!("{username}:{password}"));
                builder =
                    builder.default_headers(authorization_header(format!("Basic {encoded}"))?);
            }
            None => {}
        }
        let client = builder
            .build()
            .map_err(|_| ElasticsearchError::ClientConfiguration)?;

        Ok(Self {
            base_url: settings.url,
            client,
            auth_type,
        })
    }

    pub async fn list_indices(&self) -> Result<Vec<IndexSummary>, ElasticsearchError> {
        let url = self.url_for(&["_cat", "indices"])?;
        let response = self
            .client
            .get(url)
            .query(&[("format", "json"), ("h", "index,docs.count,health")])
            .send()
            .await
            .map_err(|_| ElasticsearchError::Request)?;
        let records: Vec<IndexRecord> = decode(response).await?;

        Ok(records
            .into_iter()
            .map(|record| IndexSummary {
                name: record.index,
                document_count: record.document_count,
                health: record.health,
            })
            .collect())
    }

    pub async fn fields(&self, index: &str) -> Result<Vec<FieldSummary>, ElasticsearchError> {
        validate_index(index)?;
        let url = self.url_for(&[index, "_mapping"])?;
        let mappings: Value = decode(
            self.client
                .get(url)
                .send()
                .await
                .map_err(|_| ElasticsearchError::Request)?,
        )
        .await?;
        let mut fields = std::collections::BTreeMap::new();

        if let Some(indices) = mappings.as_object() {
            for mapping in indices.values() {
                if let Some(properties) = mapping.pointer("/mappings/properties") {
                    collect_fields(properties, "", &mut fields);
                }
            }
        }

        Ok(fields
            .into_iter()
            .map(|(name, field_type)| FieldSummary {
                is_time_field: matches!(field_type.as_str(), "date" | "date_nanos"),
                name,
                field_type,
            })
            .collect())
    }

    pub async fn search(&self, request: SearchRequest) -> Result<SearchResult, ElasticsearchError> {
        validate_index(&request.index)?;
        validate_search(&request)?;

        let query = if request.query.trim().is_empty() {
            json!({ "match_all": {} })
        } else {
            json!({
                "query_string": {
                    "query": request.query,
                    "default_field": "*",
                    "allow_leading_wildcard": false,
                    "lenient": false
                }
            })
        };

        let query = if let Some(range) = request.time_range.as_ref() {
            let field = request
                .histogram_field
                .as_deref()
                .ok_or(ElasticsearchError::InvalidIndex)?;
            json!({ "bool": {
                "must": [query],
                "filter": [{ "range": { (field): { "gte": range.start, "lte": range.end, "format": "epoch_millis" } } }]
            } })
        } else {
            query
        };

        let sort = request.sort_by.as_ref().map_or_else(
            || json!([{"_score": {"order": "desc"}}]),
            |field| json!([{(field): {"order": request.sort_order.as_deref().unwrap_or("asc")}}]),
        );
        let body = json!({
            "query": query,
            "from": request.from,
            "size": request.size,
            "sort": sort,
            "timeout": SEARCH_TIMEOUT,
            "track_total_hits": 10000
        });
        let mut body = body;
        if let Some(field) = request.histogram_field.as_deref() {
            body["aggs"] = json!({
                "documents_over_time": {
                    "auto_date_histogram": {
                        "field": field,
                        "buckets": 40
                    }
                }
            });
        }
        let url = self.url_for(&[request.index.as_str(), "_search"])?;
        let response = self
            .client
            .post(url)
            .json(&body)
            .send()
            .await
            .map_err(|_| ElasticsearchError::Request)?;
        let envelope: SearchEnvelope = decode(response).await?;
        let (total, total_relation) = match envelope.hits.total {
            Some(TotalHits::Detailed { value, relation }) => (value, relation),
            Some(TotalHits::Count(value)) => (value, "eq".to_owned()),
            None => (0, "eq".to_owned()),
        };
        let histogram = envelope
            .aggregations
            .and_then(|aggregations| aggregations.documents_over_time)
            .map(|aggregation| {
                aggregation
                    .buckets
                    .into_iter()
                    .map(|bucket| HistogramBucket {
                        timestamp: bucket.key,
                        label: bucket.key_as_string,
                        count: bucket.doc_count,
                    })
                    .collect()
            })
            .unwrap_or_default();

        Ok(SearchResult {
            took: envelope.took,
            timed_out: envelope.timed_out.unwrap_or(false),
            total,
            total_relation,
            hits: envelope.hits.hits,
            histogram,
        })
    }

    pub async fn document(&self, index: &str, id: &str) -> Result<Document, ElasticsearchError> {
        validate_index(index)?;
        if id.is_empty() || id.len() > 512 {
            return Err(ElasticsearchError::InvalidIndex);
        }
        let url = self.url_for(&[index, "_doc", id])?;
        let response = self
            .client
            .get(url)
            .send()
            .await
            .map_err(|_| ElasticsearchError::Request)?;
        decode(response).await
    }

    fn url_for(&self, segments: &[&str]) -> Result<Url, ElasticsearchError> {
        let mut url = self.base_url.clone();
        let mut path = url
            .path_segments_mut()
            .map_err(|_| ElasticsearchError::ClientConfiguration)?;
        path.pop_if_empty();
        for segment in segments {
            path.push(segment);
        }
        drop(path);
        Ok(url)
    }
}

fn authorization_header(value: String) -> Result<header::HeaderMap, ElasticsearchError> {
    let authorization = value
        .parse::<header::HeaderValue>()
        .map_err(|_| ElasticsearchError::ClientConfiguration)?;
    let mut headers = header::HeaderMap::new();
    headers.insert(header::AUTHORIZATION, authorization);
    Ok(headers)
}

fn validate_index(index: &str) -> Result<(), ElasticsearchError> {
    if index.is_empty()
        || index.len() > 255
        || !index.bytes().all(|byte| {
            byte.is_ascii_lowercase() || byte.is_ascii_digit() || "-*._?,".contains(byte as char)
        })
    {
        return Err(ElasticsearchError::InvalidIndex);
    }
    Ok(())
}

fn validate_search(request: &SearchRequest) -> Result<(), ElasticsearchError> {
    if request
        .time_range
        .as_ref()
        .is_some_and(|range| !range.is_valid() || request.histogram_field.is_none())
    {
        return Err(ElasticsearchError::InvalidIndex);
    }
    if request.query.len() > 4096
        || request.size == 0
        || request.size > 100
        || request.from > 10_000
    {
        return Err(ElasticsearchError::InvalidIndex);
    }
    if request.sort_by.as_deref().is_some_and(|field| {
        field.is_empty()
            || field.len() > 256
            || !field
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || "._@-".contains(byte as char))
    }) {
        return Err(ElasticsearchError::InvalidIndex);
    }
    if request
        .sort_order
        .as_deref()
        .is_some_and(|order| order != "asc" && order != "desc")
    {
        return Err(ElasticsearchError::InvalidIndex);
    }
    if request.histogram_field.as_deref().is_some_and(|field| {
        field.is_empty()
            || field.len() > 256
            || !field
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || "._@-".contains(byte as char))
    }) {
        return Err(ElasticsearchError::InvalidIndex);
    }
    Ok(())
}

fn collect_fields(
    value: &Value,
    prefix: &str,
    fields: &mut std::collections::BTreeMap<String, String>,
) {
    let Some(properties) = value.as_object() else {
        return;
    };

    for (name, definition) in properties {
        let field_name = if prefix.is_empty() {
            name.clone()
        } else {
            format!("{prefix}.{name}")
        };
        let field_type = definition
            .get("type")
            .and_then(Value::as_str)
            .unwrap_or("object")
            .to_owned();
        fields.insert(field_name.clone(), field_type);
        if let Some(children) = definition.get("properties") {
            collect_fields(children, &field_name, fields);
        }
        if let Some(multi_fields) = definition.get("fields") {
            collect_fields(multi_fields, &field_name, fields);
        }
    }
}

async fn decode<T: DeserializeOwned>(response: Response) -> Result<T, ElasticsearchError> {
    let status = response.status();
    if !status.is_success() {
        let body = response.text().await.unwrap_or_default();
        let detail = serde_json::from_str::<Value>(&body)
            .ok()
            .and_then(|value| {
                value
                    .pointer("/error/reason")
                    .and_then(Value::as_str)
                    .map(str::to_owned)
            })
            .unwrap_or_else(|| "Elasticsearch request failed".to_owned());
        return Err(ElasticsearchError::Rejected {
            status: status.as_u16(),
            detail: detail.chars().take(MAX_ERROR_DETAIL_CHARS).collect(),
        });
    }

    response
        .json::<T>()
        .await
        .map_err(|_| ElasticsearchError::InvalidResponse)
}

#[cfg(test)]
mod tests {
    use super::{
        collect_fields, validate_index, validate_search, Document, SearchHit, SearchRequest,
    };
    use serde_json::json;
    use url::Url;
    use wiremock::{matchers, Mock, MockServer, ResponseTemplate};

    use super::ElasticsearchClient;
    use super::{ElasticsearchAuthentication, ElasticsearchSettings};

    #[test]
    fn validate_index_accepts_patterns_and_rejects_path_segments() {
        assert!(validate_index("logs-*,metrics_2025.01").is_ok());
        assert!(validate_index("logs/../secrets").is_err());
    }

    #[test]
    fn validate_search_enforces_result_and_query_bounds() {
        let valid = SearchRequest {
            index: "logs-*".to_owned(),
            query: "service.name:api".to_owned(),
            from: 0,
            size: 50,
            sort_by: Some("@timestamp".to_owned()),
            sort_order: Some("desc".to_owned()),
            histogram_field: Some("@timestamp".to_owned()),
            time_range: None,
        };
        assert!(validate_search(&valid).is_ok());

        let oversized = SearchRequest { size: 101, ..valid };
        assert!(validate_search(&oversized).is_err());
    }

    #[test]
    fn collect_fields_flattens_nested_mapping_properties() {
        let mapping = json!({
            "message": {"type": "text"},
            "host": {"properties": {"name": {"type": "keyword"}}}
        });
        let mut fields = std::collections::BTreeMap::new();
        collect_fields(&mapping, "", &mut fields);
        assert_eq!(fields.get("message").map(String::as_str), Some("text"));
        assert_eq!(fields.get("host.name").map(String::as_str), Some("keyword"));
    }

    #[test]
    fn search_hit_serializes_using_frontend_contract_names() {
        let hit: SearchHit = serde_json::from_value(json!({
            "_index": "discover-demo-logs",
            "_id": "doc-1",
            "_score": 1.0,
            "_source": {"message": "sample"}
        }))
        .expect("Elasticsearch hit deserializes");

        let serialized = serde_json::to_value(hit).expect("API hit serializes");
        assert_eq!(serialized["index"], "discover-demo-logs");
        assert_eq!(serialized["id"], "doc-1");
        assert_eq!(serialized["source"]["message"], "sample");
        assert!(serialized.get("_index").is_none());
    }

    #[test]
    fn document_serializes_using_frontend_contract_names() {
        let document: Document = serde_json::from_value(json!({
            "_index": "discover-demo-logs",
            "_id": "doc-1",
            "found": true,
            "_source": {"message": "sample"}
        }))
        .expect("Elasticsearch document deserializes");

        let serialized = serde_json::to_value(document).expect("API document serializes");
        assert_eq!(serialized["id"], "doc-1");
        assert_eq!(serialized["source"]["message"], "sample");
        assert!(serialized.get("_id").is_none());
    }

    #[tokio::test]
    async fn search_filters_documents_and_histogram_by_configured_time_window() {
        let server = MockServer::start().await;
        Mock::given(matchers::method("POST"))
            .and(|request: &wiremock::Request| {
                let body: serde_json::Value =
                    serde_json::from_slice(&request.body).expect("JSON search body");
                body["query"]["bool"]["filter"][0]["range"]["created_at"]
                    == json!({"gte": 1000, "lte": 2000, "format": "epoch_millis"})
                    && body["query"]["bool"]["must"][0]["query_string"]["query"] == "message:ok"
                    && body["aggs"]["documents_over_time"]["auto_date_histogram"]["field"]
                        == "created_at"
            })
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "hits": {"total": {"value": 0, "relation": "eq"}, "hits": []},
                "aggregations": {"documents_over_time": {"buckets": []}}
            })))
            .expect(1)
            .mount(&server)
            .await;
        let client = ElasticsearchClient::new(ElasticsearchSettings {
            url: Url::parse(&server.uri()).expect("mock URL"),
            authentication: None,
            tls_insecure: false,
        })
        .expect("valid client");
        let mut request = SearchRequest {
            index: "logs".to_owned(),
            query: "message:ok".to_owned(),
            from: 0,
            size: 50,
            sort_by: None,
            sort_order: None,
            histogram_field: Some("created_at".to_owned()),
            time_range: Some(crate::search::TimeRange {
                start: 1000,
                end: 2000,
            }),
        };
        request.time_range = Some(crate::search::TimeRange {
            start: 2000,
            end: 1000,
        });
        assert!(super::validate_search(&request).is_err());
        request.time_range = Some(crate::search::TimeRange {
            start: 1000,
            end: 2000,
        });
        client
            .search(request)
            .await
            .expect("filtered search succeeds");
    }

    #[tokio::test]
    async fn search_translates_lucene_and_keeps_api_key_on_server_request() {
        let server = MockServer::start().await;
        Mock::given(matchers::method("POST"))
            .and(matchers::path("/logs-*/_search"))
            .and(matchers::header("authorization", "ApiKey test-secret"))
            .and(matchers::body_json(json!({
                "query": {
                    "query_string": {
                        "query": "service.name:gateway",
                        "default_field": "*",
                        "allow_leading_wildcard": false,
                        "lenient": false
                    }
                },
                "from": 0,
                "size": 50,
                "sort": [{"_score": {"order": "desc"}}],
                "timeout": "10s",
                "track_total_hits": 10000,
                "aggs": {
                    "documents_over_time": {
                        "auto_date_histogram": {
                            "field": "@timestamp",
                            "buckets": 40
                        }
                    }
                }
            })))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "took": 3,
                "timed_out": false,
                "hits": {
                    "total": {"value": 1, "relation": "eq"},
                    "hits": [{"_index": "logs-2026", "_id": "a1", "_score": 1.0, "_source": {"message": "ok"}}]
                },
                "aggregations": {
                    "documents_over_time": {
                        "buckets": [{"key": 1791400000000_i64, "key_as_string": "2026-10-07T20:26:40.000Z", "doc_count": 1}]
                    }
                }
            })))
            .mount(&server)
            .await;

        let client = ElasticsearchClient::new(ElasticsearchSettings {
            url: Url::parse(&server.uri()).expect("mock server URL is valid"),
            authentication: Some(ElasticsearchAuthentication::ApiKey(
                "test-secret".to_owned(),
            )),
            tls_insecure: false,
        })
        .expect("client settings are valid");
        let result = client
            .search(SearchRequest {
                index: "logs-*".to_owned(),
                query: "service.name:gateway".to_owned(),
                from: 0,
                size: 50,
                sort_by: None,
                sort_order: None,
                histogram_field: Some("@timestamp".to_owned()),
                time_range: None,
            })
            .await
            .expect("mocked search succeeds");

        assert_eq!(result.total, 1);
        assert_eq!(result.hits[0].id, "a1");
        assert_eq!(result.histogram.len(), 1);
        assert_eq!(result.histogram[0].timestamp, 1_791_400_000_000);
        assert_eq!(result.histogram[0].count, 1);
    }

    #[tokio::test]
    async fn basic_authentication_is_sent_only_to_elasticsearch() {
        let server = MockServer::start().await;
        Mock::given(matchers::method("GET"))
            .and(matchers::path("/_cat/indices"))
            .and(matchers::header(
                "authorization",
                "Basic ZWxhc3RpYzpzZWNyZXQ=",
            ))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!([])))
            .mount(&server)
            .await;

        let client = ElasticsearchClient::new(ElasticsearchSettings {
            url: Url::parse(&server.uri()).expect("mock server URL is valid"),
            authentication: Some(ElasticsearchAuthentication::Basic {
                username: "elastic".to_owned(),
                password: "secret".to_owned(),
            }),
            tls_insecure: false,
        })
        .expect("client settings are valid");

        assert!(client
            .list_indices()
            .await
            .expect("mocked index listing succeeds")
            .is_empty());
    }
}
