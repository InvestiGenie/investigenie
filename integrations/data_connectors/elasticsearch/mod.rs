use std::collections::{BTreeSet, HashMap};
use std::time::Duration;

use base64::{engine::general_purpose::STANDARD as BASE64, Engine};
use reqwest::{header, Client, Response, Url};
use serde::{de::DeserializeOwned, Deserialize, Serialize};
use serde_json::{json, Value};
use thiserror::Error;

use crate::data_connectors::IntegrationDescriptor;
pub use crate::data_connectors::{FieldSummary, IndexSummary};
pub use crate::search::SearchRequest;
use crate::search::{BreakdownCount, HistogramOptions, BREAKDOWN_OTHER_KEY, MAX_BREAKDOWN_VALUES};
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
const MAX_SCOPED_INDICES_BYTES: usize = 4096;

pub fn descriptor() -> IntegrationDescriptor {
    IntegrationDescriptor {
        id: "elasticsearch",
        display_name: "Elasticsearch",
        icon: "elasticsearch",
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
    Rejected {
        status: u16,
        kind: String,
        detail: String,
    },
    #[error("Elasticsearch returned an invalid response")]
    InvalidResponse,
    #[error("Index pattern contains unsupported characters")]
    InvalidIndex,
    #[error("Field {0} cannot be sorted")]
    UnsortableField(String),
    #[error("Field {0} has incompatible types across matched indices")]
    IncompatibleField(String),
    #[error("Time field {0} is not a date in the matched indices")]
    TimeFieldConflict(String),
    #[error("Elasticsearch could not search every matched shard")]
    PartialResults,
}

struct FieldCapability {
    field_type: String,
    aggregatable: bool,
    indices: Option<Vec<String>>,
}

type FieldCapabilities = HashMap<String, Vec<FieldCapability>>;

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
    pit_id: Option<String>,
    #[serde(rename = "_shards")]
    shards: Option<Value>,
    hits: SearchHits,
    aggregations: Option<SearchAggregations>,
}

#[derive(Debug, Deserialize)]
struct SearchAggregations {
    documents_over_time: Option<AutoDateHistogram>,
    breakdown_values: Option<BreakdownTerms>,
}

#[derive(Debug, Deserialize)]
struct AutoDateHistogram {
    buckets: Vec<ElasticsearchHistogramBucket>,
}

#[derive(Debug, Deserialize)]
struct BreakdownTerms {
    buckets: Vec<BreakdownTermBucket>,
}

#[derive(Debug, Deserialize)]
struct BreakdownTermBucket {
    key: Value,
    key_as_string: Option<String>,
    over_time: AutoDateHistogram,
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
    #[serde(default, skip_serializing)]
    sort: Vec<Value>,
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
    #[serde(skip_serializing_if = "Option::is_none")]
    pub next_cursor: Option<String>,
    pub took: Option<u64>,
    pub timed_out: bool,
    pub total: u64,
    pub total_relation: String,
    pub hits: Vec<SearchHit>,
    pub histogram: Vec<HistogramBucket>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub histogram_interval: Option<String>,
}

#[derive(Debug, Serialize)]
pub struct HistogramBucket {
    pub timestamp: i64,
    pub label: Option<String>,
    pub count: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub breakdown: Option<Vec<BreakdownCount>>,
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

#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct SearchCursor {
    pit_id: String,
    sort: Vec<Value>,
}

impl SearchCursor {
    fn validate(&self) -> Result<(), ElasticsearchError> {
        if self.pit_id.is_empty()
            || self.pit_id.len() > 16_384
            || self.sort.is_empty()
            || self.sort.len() > 2
            || self.sort.iter().any(|value| match value {
                Value::Null | Value::Number(_) => false,
                Value::String(value) => value.len() > 1024,
                _ => true,
            })
            || !self
                .sort
                .last()
                .is_some_and(|value| value.as_i64().is_some())
        {
            return Err(ElasticsearchError::InvalidIndex);
        }
        Ok(())
    }

    fn decode(token: &str) -> Result<Self, ElasticsearchError> {
        if token.len() > 32_768 {
            return Err(ElasticsearchError::InvalidIndex);
        }
        let bytes = BASE64
            .decode(token)
            .map_err(|_| ElasticsearchError::InvalidIndex)?;
        let cursor: Self =
            serde_json::from_slice(&bytes).map_err(|_| ElasticsearchError::InvalidIndex)?;
        cursor.validate()?;
        Ok(cursor)
    }

    fn encode(&self) -> Result<String, ElasticsearchError> {
        self.validate()
            .map_err(|_| ElasticsearchError::InvalidResponse)?;
        let bytes = serde_json::to_vec(self).map_err(|_| ElasticsearchError::InvalidResponse)?;
        Ok(BASE64.encode(bytes))
    }
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
        let mut histogram_options = request
            .histogram_options()
            .map_err(|_| ElasticsearchError::InvalidIndex)?;
        let capabilities = self.field_capabilities(&request).await;
        let scope = search_scope(&request, capabilities.as_ref())?;
        let sort = sort_clause(&request, capabilities.as_ref())?;

        let query = if request.query.trim().is_empty() {
            json!({ "match_all": {} })
        } else {
            json!({
                "query_string": {
                    "query": request.query,
                    "default_field": "*",
                    "allow_leading_wildcard": false,
                    "lenient": true
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

        let body = json!({
            "query": query,
            "from": request.from,
            "size": request.size,
            "sort": sort,
            "timeout": SEARCH_TIMEOUT,
            "track_total_hits": request.track_total_hits.unwrap_or(true)
        });
        let mut body = body;
        if let Some(field) = request.histogram_field.as_deref().filter(|_| {
            request.include_histogram.unwrap_or(true)
                && request.cursor.as_deref().is_none_or(str::is_empty)
        }) {
            body["aggs"] = histogram_aggregations(field, &request, &histogram_options);
        }
        let mut pit_id = None;
        let url = if let Some(token) = request.cursor.as_deref() {
            let cursor = if token.is_empty() {
                let response = self
                    .client
                    .post(self.url_for(&[scope.as_str(), "_pit"])?)
                    .query(&[
                        ("keep_alive", "1m"),
                        ("allow_partial_search_results", "false"),
                    ])
                    .send()
                    .await
                    .map_err(|_| ElasticsearchError::Request)?;
                let opened: Value = decode(response).await?;
                let id = opened
                    .get("id")
                    .and_then(Value::as_str)
                    .filter(|id| !id.is_empty() && id.len() <= 16_384)
                    .ok_or(ElasticsearchError::InvalidResponse)?;
                SearchCursor {
                    pit_id: id.to_owned(),
                    sort: Vec::new(),
                }
            } else {
                SearchCursor::decode(token)?
            };
            body.as_object_mut().unwrap().remove("from");
            if request.sort_by.as_deref() != Some("_shard_doc") {
                body["sort"]
                    .as_array_mut()
                    .unwrap()
                    .push(json!({"_shard_doc": "asc"}));
            }
            if !cursor.sort.is_empty() {
                if cursor.sort.len() != body["sort"].as_array().unwrap().len() {
                    return Err(ElasticsearchError::InvalidIndex);
                }
                body["search_after"] = json!(cursor.sort);
            }
            body["pit"] = json!({"id": cursor.pit_id, "keep_alive": "1m"});
            pit_id = Some(cursor.pit_id);
            self.url_for(&["_search"])?
        } else {
            self.url_for(&[scope.as_str(), "_search"])?
        };
        if histogram_options.breakdown_field.is_some()
            && histogram_options.interval.is_none()
            && body.get("aggs").is_some()
        {
            let field = request.histogram_field.as_deref().unwrap();
            let mut bounds_body = json!({
                "size":0,"track_total_hits":false,"timeout":SEARCH_TIMEOUT,
                "query":body["query"],
                "aggs":{"first_timestamp":{"min":{"field":field}},"last_timestamp":{"max":{"field":field}}}
            });
            if let Some(pit) = body.get("pit") {
                bounds_body["pit"] = pit.clone();
            }
            let bounds = match self
                .client
                .post(url.clone())
                .query(&[("allow_partial_search_results", "false")])
                .json(&bounds_body)
                .send()
                .await
            {
                Ok(response) => decode::<Value>(response).await,
                Err(_) => Err(ElasticsearchError::Request),
            };
            let bounds = match bounds {
                Ok(bounds) => bounds,
                Err(error) => {
                    if let Some(id) = pit_id.as_deref() {
                        self.close_pit(id).await;
                    }
                    return Err(error);
                }
            };
            if let Some(id) = bounds.get("pit_id").and_then(Value::as_str) {
                pit_id = Some(id.to_owned());
                body["pit"]["id"] = json!(id);
            }
            if bounds.pointer("/_shards/failed").and_then(Value::as_u64) != Some(0) {
                if let Some(id) = pit_id.as_deref() {
                    self.close_pit(id).await;
                }
                return Err(ElasticsearchError::PartialResults);
            }
            // A timed-out bounds probe may miss documents, so keep the automatic histogram instead.
            let bounds_complete = bounds.get("timed_out").and_then(Value::as_bool) == Some(false);
            if let (true, Some(first), Some(last)) = (
                bounds_complete,
                bounds
                    .pointer("/aggregations/first_timestamp/value")
                    .and_then(Value::as_f64),
                bounds
                    .pointer("/aggregations/last_timestamp/value")
                    .and_then(Value::as_f64),
            ) {
                let span = (last - first).max(1.0);
                histogram_options.interval = crate::search::HistogramInterval::automatic(span);
                body["aggs"] = histogram_aggregations(field, &request, &histogram_options);
            }
        }
        let response = self
            .client
            .post(url)
            .query(&[("allow_partial_search_results", "false")])
            .json(&body)
            .send()
            .await;
        let decoded = match response {
            Ok(response) => decode::<SearchEnvelope>(response).await,
            Err(_) => Err(ElasticsearchError::Request),
        };
        let envelope = match decoded {
            Ok(envelope) => envelope,
            Err(error) => {
                if let Some(id) = pit_id.as_deref() {
                    self.close_pit(id).await;
                }
                return Err(error);
            }
        };
        let shards_failed = envelope
            .shards
            .as_ref()
            .and_then(|shards| shards.get("failed"))
            .and_then(Value::as_u64);
        let timed_out = envelope.timed_out.unwrap_or(false);
        let mut next_cursor = None;
        if let Some(opened_id) = pit_id {
            let id = envelope.pit_id.as_deref().unwrap_or(&opened_id);
            if shards_failed != Some(0) {
                self.close_pit(id).await;
                return Err(ElasticsearchError::PartialResults);
            }
            // Continuing after a timed-out page could skip documents, so return it without a cursor.
            if !timed_out && envelope.hits.hits.len() >= request.size {
                let cursor = SearchCursor {
                    pit_id: id.to_owned(),
                    sort: envelope.hits.hits.last().unwrap().sort.clone(),
                };
                match cursor.encode() {
                    Ok(token) => next_cursor = Some(token),
                    Err(error) => {
                        self.close_pit(id).await;
                        return Err(error);
                    }
                }
            } else {
                self.close_pit(id).await;
            }
        } else if shards_failed.is_some_and(|failed| failed > 0) {
            return Err(ElasticsearchError::PartialResults);
        }
        let (total, total_relation) = match envelope.hits.total {
            Some(TotalHits::Detailed { value, relation }) => (value, relation),
            Some(TotalHits::Count(value)) => (value, "eq".to_owned()),
            None => (0, "eq".to_owned()),
        };
        let histogram_interval = histogram_options
            .interval
            .filter(|_| envelope.aggregations.is_some())
            .map(|interval| format!("{}{}", interval.count, interval.suffix()));
        let histogram = envelope
            .aggregations
            .map(histogram_buckets)
            .unwrap_or_default();

        Ok(SearchResult {
            next_cursor,
            took: envelope.took,
            timed_out,
            total,
            total_relation,
            hits: envelope.hits.hits,
            histogram,
            histogram_interval,
        })
    }

    async fn field_capabilities(&self, request: &SearchRequest) -> Option<FieldCapabilities> {
        let mut fields = Vec::new();
        if let Some(field) = request.histogram_field.as_deref() {
            fields.push(field);
        }
        if let Some(field) = request
            .sort_by
            .as_deref()
            .filter(|field| !matches!(*field, "_score" | "_shard_doc" | "_doc"))
            .filter(|field| !fields.contains(field))
        {
            fields.push(field);
        }
        if fields.is_empty() {
            return None;
        }
        let url = self
            .url_for(&[request.index.as_str(), "_field_caps"])
            .ok()?;
        let response = self
            .client
            .get(url)
            .query(&[
                ("fields", fields.join(",")),
                ("include_unmapped", "true".to_owned()),
            ])
            .send()
            .await
            .ok()?;
        // Capabilities only refine scope and sorting; the search itself still reports real failures.
        let response: Value = decode(response).await.ok()?;
        let mapped = response.get("fields")?.as_object()?;
        Some(
            fields
                .into_iter()
                .filter_map(|field| {
                    let types = mapped.get(field)?.as_object()?;
                    let capabilities = types
                        .iter()
                        .map(|(field_type, details)| FieldCapability {
                            field_type: field_type.clone(),
                            aggregatable: details
                                .get("aggregatable")
                                .and_then(Value::as_bool)
                                .unwrap_or(false),
                            indices: details.get("indices").and_then(Value::as_array).map(
                                |indices| {
                                    indices
                                        .iter()
                                        .filter_map(Value::as_str)
                                        .map(str::to_owned)
                                        .collect()
                                },
                            ),
                        })
                        .collect();
                    Some((field.to_owned(), capabilities))
                })
                .collect(),
        )
    }

    async fn close_pit(&self, id: &str) {
        if let Ok(url) = self.url_for(&["_pit"]) {
            let _ = self
                .client
                .delete(url)
                .json(&json!({"id": id}))
                .send()
                .await;
        }
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

fn histogram_aggregations(
    field: &str,
    request: &SearchRequest,
    options: &HistogramOptions,
) -> Value {
    let over_time = match options.interval {
        Some(interval) => {
            let kind = if interval.is_calendar() {
                "calendar_interval"
            } else {
                "fixed_interval"
            };
            let mut histogram = json!({ "date_histogram": {
                "field": field,
                (kind): format!("{}{}", interval.count, interval.suffix()),
                "time_zone": options.time_zone,
                "min_doc_count": 0
            } });
            if let Some(range) = request.time_range {
                histogram["date_histogram"]["extended_bounds"] =
                    json!({"min": range.start, "max": range.end});
            }
            histogram
        }
        _ if options.time_zone != "UTC" => {
            json!({ "auto_date_histogram": { "field": field, "buckets": 40, "time_zone": options.time_zone } })
        }
        _ => json!({ "auto_date_histogram": { "field": field, "buckets": 40 } }),
    };
    let mut aggregations = json!({ "documents_over_time": over_time.clone() });
    // Automatic intervals can differ per sub-bucket, so breakdowns need an explicit interval.
    if let (Some(breakdown), Some(_)) = (options.breakdown_field.as_deref(), options.interval) {
        aggregations["breakdown_values"] = json!({
            "terms": { "field": breakdown, "size": MAX_BREAKDOWN_VALUES },
            "aggs": { "over_time": over_time }
        });
    }
    aggregations
}

fn histogram_buckets(aggregations: SearchAggregations) -> Vec<HistogramBucket> {
    let Some(histogram) = aggregations.documents_over_time else {
        return Vec::new();
    };
    let breakdown = aggregations.breakdown_values.map(|terms| {
        terms
            .buckets
            .into_iter()
            .map(|bucket| {
                let key = bucket.key_as_string.unwrap_or_else(|| match bucket.key {
                    Value::String(key) => key,
                    other => other.to_string(),
                });
                let counts = bucket
                    .over_time
                    .buckets
                    .into_iter()
                    .map(|time| (time.key, time.doc_count))
                    .collect::<std::collections::HashMap<_, _>>();
                (key, counts)
            })
            .collect::<Vec<_>>()
    });
    histogram
        .buckets
        .into_iter()
        .map(|bucket| HistogramBucket {
            breakdown: breakdown.as_ref().map(|values| {
                let mut counts = values
                    .iter()
                    .map(|(key, counts)| BreakdownCount {
                        key: key.clone(),
                        count: counts.get(&bucket.key).copied().unwrap_or(0),
                    })
                    .collect::<Vec<_>>();
                let listed: u64 = counts.iter().map(|value| value.count).sum();
                counts.push(BreakdownCount {
                    key: BREAKDOWN_OTHER_KEY.to_owned(),
                    count: bucket.doc_count.saturating_sub(listed),
                });
                counts
            }),
            timestamp: bucket.key,
            label: bucket.key_as_string,
            count: bucket.doc_count,
        })
        .collect()
}

fn authorization_header(value: String) -> Result<header::HeaderMap, ElasticsearchError> {
    let authorization = value
        .parse::<header::HeaderValue>()
        .map_err(|_| ElasticsearchError::ClientConfiguration)?;
    let mut headers = header::HeaderMap::new();
    headers.insert(header::AUTHORIZATION, authorization);
    Ok(headers)
}

fn is_date_type(field_type: &str) -> bool {
    matches!(field_type, "date" | "date_nanos")
}

fn sort_family(field_type: &str) -> &str {
    match field_type {
        "long" | "integer" | "short" | "byte" | "double" | "float" | "half_float"
        | "scaled_float" | "unsigned_long" => "number",
        "date" | "date_nanos" => "date",
        "keyword" | "constant_keyword" | "wildcard" => "keyword",
        other => other,
    }
}

/// Restricts a pattern to indices whose time field is a date when other indices map it differently.
fn search_scope(
    request: &SearchRequest,
    capabilities: Option<&FieldCapabilities>,
) -> Result<String, ElasticsearchError> {
    let Some((field, types)) = request
        .histogram_field
        .as_deref()
        .and_then(|field| Some((field, capabilities?.get(field)?)))
    else {
        return Ok(request.index.clone());
    };
    if types
        .iter()
        .all(|capability| is_date_type(&capability.field_type) || capability.field_type == "unmapped")
    {
        return Ok(request.index.clone());
    }
    let indices = types
        .iter()
        .filter(|capability| is_date_type(&capability.field_type))
        .flat_map(|capability| capability.indices.iter().flatten().cloned())
        .collect::<BTreeSet<_>>();
    let scope = indices.iter().cloned().collect::<Vec<_>>().join(",");
    if indices.is_empty()
        || scope.len() > MAX_SCOPED_INDICES_BYTES
        || indices.iter().any(|index| validate_index(index).is_err())
    {
        return Err(ElasticsearchError::TimeFieldConflict(field.to_owned()));
    }
    Ok(scope)
}

fn sort_clause(
    request: &SearchRequest,
    capabilities: Option<&FieldCapabilities>,
) -> Result<Value, ElasticsearchError> {
    let Some(field) = request.sort_by.as_ref() else {
        return Ok(json!([{"_score": {"order": "desc"}}]));
    };
    let mut options = json!({"order": request.sort_order.as_deref().unwrap_or("asc")});
    if request.histogram_field.as_ref() == Some(field) {
        options["unmapped_type"] = json!("date");
    } else if let Some(types) = capabilities.and_then(|capabilities| capabilities.get(field)) {
        let mapped = types
            .iter()
            .filter(|capability| capability.field_type != "unmapped")
            .collect::<Vec<_>>();
        if mapped.iter().any(|capability| !capability.aggregatable) {
            return Err(ElasticsearchError::UnsortableField(field.clone()));
        }
        let families = mapped
            .iter()
            .map(|capability| sort_family(&capability.field_type))
            .collect::<BTreeSet<_>>();
        if families.len() > 1 {
            return Err(ElasticsearchError::IncompatibleField(field.clone()));
        }
        if let (Some(first), Some(family)) = (mapped.first(), families.first()) {
            let mixed = mapped.len() > 1;
            if mapped.len() < types.len() {
                options["unmapped_type"] = json!(match *family {
                    "number" if mixed => "double",
                    "date" => "date",
                    "keyword" => "keyword",
                    _ => first.field_type.as_str(),
                });
            }
            match *family {
                "number" if mixed => options["numeric_type"] = json!("double"),
                "date" if mixed => options["numeric_type"] = json!("date_nanos"),
                _ => {}
            }
        }
    }
    Ok(json!([{(field): options}]))
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
        || (request.cursor.is_none() && request.from > 10_000)
    {
        return Err(ElasticsearchError::InvalidIndex);
    }
    if let Some(token) = request.cursor.as_deref().filter(|token| !token.is_empty()) {
        SearchCursor::decode(token)?;
    }
    if request
        .sort_by
        .as_deref()
        .is_some_and(|field| !crate::search::safe_field_name(field))
    {
        return Err(ElasticsearchError::InvalidIndex);
    }
    if request
        .sort_order
        .as_deref()
        .is_some_and(|order| order != "asc" && order != "desc")
    {
        return Err(ElasticsearchError::InvalidIndex);
    }
    if request
        .histogram_field
        .as_deref()
        .is_some_and(|field| !crate::search::safe_field_name(field))
    {
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
        // Patterns can map a field differently per index; keep a date type so it remains a time field.
        fields
            .entry(field_name.clone())
            .and_modify(|existing: &mut String| {
                if is_date_type(&field_type) && !is_date_type(existing) {
                    existing.clone_from(&field_type);
                }
            })
            .or_insert(field_type);
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
        let error = serde_json::from_str::<Value>(&body).unwrap_or_default();
        let text = |pointers: [&str; 3]| {
            pointers
                .into_iter()
                .find_map(|pointer| error.pointer(pointer).and_then(Value::as_str))
                .unwrap_or_default()
                .chars()
                .take(MAX_ERROR_DETAIL_CHARS)
                .collect::<String>()
        };
        return Err(ElasticsearchError::Rejected {
            status: status.as_u16(),
            kind: text([
                "/error/failed_shards/0/reason/type",
                "/error/root_cause/0/type",
                "/error/type",
            ]),
            detail: text([
                "/error/failed_shards/0/reason/reason",
                "/error/root_cause/0/reason",
                "/error/reason",
            ]),
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
    use base64::Engine;
    use serde_json::{json, Value};
    use url::Url;
    use wiremock::{matchers, Mock, MockServer, ResponseTemplate};

    use super::ElasticsearchClient;
    use super::{ElasticsearchAuthentication, ElasticsearchSettings};

    fn cursor_request(token: &str) -> SearchRequest {
        serde_json::from_value(json!({
            "index": "logs", "cursor": token, "from": 50000, "size": 2,
            "sort_by": "created_at", "sort_order": "asc", "histogram_field": "created_at"
        }))
        .unwrap()
    }

    #[tokio::test]
    async fn all_time_pattern_breakdown_counts_every_match_on_one_pit() {
        let server = MockServer::start().await;
        Mock::given(matchers::method("POST"))
            .and(matchers::path("/logs-*/_pit"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({"id":"initial"})))
            .expect(1)
            .mount(&server)
            .await;
        Mock::given(matchers::method("POST"))
            .and(matchers::path("/_search"))
            .and(|request: &wiremock::Request| {
                let body: Value = serde_json::from_slice(&request.body).unwrap();
                body["size"] == 0
                    && body["pit"]["id"] == "initial"
                    && body["aggs"]["first_timestamp"]["min"]["field"] == "created_at"
                    && body["query"] == json!({"match_all":{}})
            })
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "timed_out":false,"_shards":{"failed":0},"pit_id":"rotated",
                "aggregations":{"first_timestamp":{"value":0},"last_timestamp":{"value":120000}}
            })))
            .expect(1)
            .mount(&server)
            .await;
        Mock::given(matchers::method("POST"))
            .and(matchers::path("/_search"))
            .and(matchers::query_param("allow_partial_search_results", "false"))
            .and(|request: &wiremock::Request| {
                let body: Value = serde_json::from_slice(&request.body).unwrap();
                body["track_total_hits"] == true && body["pit"]["id"] == "rotated"
                    && body["aggs"]["documents_over_time"]["date_histogram"]["fixed_interval"] == "5s"
                    && body["aggs"]["breakdown_values"]["terms"]["field"] == "service"
                    && body["aggs"]["documents_over_time"] == body["aggs"]["breakdown_values"]["aggs"]["over_time"]
                    && body["query"] == json!({"match_all":{}})
            })
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "timed_out":false,"_shards":{"failed":0},"pit_id":"finished",
                "hits":{"total":{"value":24000,"relation":"eq"},"hits":[]},
                "aggregations":{
                    "documents_over_time":{"buckets":[{"key":0,"doc_count":24000}]},
                    "breakdown_values":{"buckets":[{"key":"api","over_time":{"buckets":[{"key":0,"doc_count":20000}]}}]}
                }
            })))
            .expect(1).mount(&server).await;
        Mock::given(matchers::method("DELETE"))
            .and(matchers::path("/_pit"))
            .and(matchers::body_json(json!({"id":"finished"})))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({"succeeded":true})))
            .expect(1)
            .mount(&server)
            .await;
        let client = ElasticsearchClient::new(ElasticsearchSettings {
            url: Url::parse(&server.uri()).unwrap(),
            authentication: None,
            tls_insecure: false,
        })
        .unwrap();
        let mut request = cursor_request("");
        request.index = "logs-*".to_owned();
        request.histogram_breakdown_field = Some("service".to_owned());
        let result = client.search(request).await.unwrap();
        assert_eq!(result.total, 24000);
        assert_eq!(result.total_relation, "eq");
        assert_eq!(result.histogram_interval.as_deref(), Some("5s"));
        assert_eq!(
            serde_json::to_value(&result.histogram[0]).unwrap()["breakdown"],
            json!([{"key":"api","count":20000},{"key":"__other__","count":4000}])
        );
    }

    #[tokio::test]
    async fn pattern_cursor_search_tolerates_unmapped_time_fields() {
        for pattern in ["logs-*", "logs-2026.10.15,logs-2026.10.16"] {
            let server = MockServer::start().await;
            Mock::given(matchers::method("POST"))
                .and(matchers::path(format!("/{pattern}/_pit")))
                .respond_with(ResponseTemplate::new(200).set_body_json(json!({"id":"pattern-pit"})))
                .expect(1)
                .mount(&server)
                .await;
            Mock::given(matchers::method("POST"))
                .and(matchers::path("/_search"))
                .and(matchers::query_param(
                    "allow_partial_search_results",
                    "false",
                ))
                .and(|request: &wiremock::Request| {
                    let body: Value = serde_json::from_slice(&request.body).unwrap();
                    body["sort"][0]["created_at"]["unmapped_type"] == "date"
                        && body["sort"][1] == json!({"_shard_doc":"asc"})
                })
                .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                    "timed_out":false,"_shards":{"failed":0},
                    "hits":{"total":0,"hits":[]}
                })))
                .expect(1)
                .mount(&server)
                .await;
            Mock::given(matchers::method("DELETE"))
                .and(matchers::path("/_pit"))
                .and(matchers::body_json(json!({"id":"pattern-pit"})))
                .respond_with(ResponseTemplate::new(200).set_body_json(json!({"succeeded":true})))
                .expect(1)
                .mount(&server)
                .await;
            let client = ElasticsearchClient::new(ElasticsearchSettings {
                url: Url::parse(&server.uri()).unwrap(),
                authentication: None,
                tls_insecure: false,
            })
            .unwrap();
            let mut request = cursor_request("");
            request.index = pattern.to_owned();
            assert!(client.search(request).await.unwrap().next_cursor.is_none());
        }
    }

    #[tokio::test]
    async fn cursor_search_opens_pit_rotates_id_and_closes_on_exhaustion() {
        let server = MockServer::start().await;
        Mock::given(matchers::method("POST"))
            .and(matchers::path("/logs/_pit"))
            .and(matchers::query_param("keep_alive", "1m"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({"id":"pit-start"})))
            .expect(1)
            .mount(&server)
            .await;
        Mock::given(matchers::method("POST"))
            .and(matchers::path("/_search"))
            .and(matchers::query_param(
                "allow_partial_search_results",
                "false",
            ))
            .and(|request: &wiremock::Request| {
                let body: Value = serde_json::from_slice(&request.body).unwrap();
                body.get("from").is_none()
                    && body.get("search_after").is_none()
                    && body["size"] == 2
                    && body["pit"] == json!({"id":"pit-start","keep_alive":"1m"})
                    && body["sort"] == json!([{"created_at":{"order":"asc","unmapped_type":"date"}}, {"_shard_doc":"asc"}])
                    && body.get("aggs").is_some()
            })
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "timed_out":false,"_shards":{"failed":0},"pit_id":"pit-page-1",
                "hits":{"total":{"value":3,"relation":"eq"},"hits":[
                    {"_index":"logs","_id":"a","sort":[1000,1]},
                    {"_index":"logs","_id":"b","sort":[1000,2]}
                ]},"aggregations":{"documents_over_time":{"buckets":[]}}
            })))
            .expect(1)
            .mount(&server)
            .await;
        Mock::given(matchers::method("POST"))
            .and(matchers::path("/_search"))
            .and(|request: &wiremock::Request| {
                let body: Value = serde_json::from_slice(&request.body).unwrap();
                body.get("from").is_none()
                    && body.get("aggs").is_none()
                    && body["search_after"] == json!([1000, 2])
                    && body["pit"] == json!({"id":"pit-page-1","keep_alive":"1m"})
            })
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "timed_out":false,"_shards":{"failed":0},"pit_id":"pit-end",
                "hits":{"total":3,"hits":[{"_index":"logs","_id":"c","sort":[1001,3]}]}
            })))
            .expect(1)
            .mount(&server)
            .await;
        Mock::given(matchers::method("DELETE"))
            .and(matchers::path("/_pit"))
            .and(matchers::body_json(json!({"id":"pit-end"})))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({"succeeded":true})))
            .expect(1)
            .mount(&server)
            .await;
        let client = ElasticsearchClient::new(ElasticsearchSettings {
            url: Url::parse(&server.uri()).unwrap(),
            authentication: Some(ElasticsearchAuthentication::ApiKey(
                "test-secret".to_owned(),
            )),
            tls_insecure: false,
        })
        .unwrap();
        let first = client.search(cursor_request("")).await.unwrap();
        assert_eq!(first.hits.len(), 2);
        let token = first.next_cursor.unwrap();
        let decoded = super::SearchCursor::decode(&token).unwrap();
        assert_eq!(decoded.pit_id, "pit-page-1");
        assert_eq!(decoded.sort, vec![json!(1000), json!(2)]);
        assert!(!String::from_utf8(super::BASE64.decode(&token).unwrap())
            .unwrap()
            .contains("test-secret"));
        let last = client.search(cursor_request(&token)).await.unwrap();
        assert_eq!(last.hits[0].id, "c");
        assert!(last.next_cursor.is_none());
        assert!(serde_json::to_value(last)
            .unwrap()
            .get("next_cursor")
            .is_none());
    }

    #[tokio::test]
    async fn cursor_search_rejects_partial_pages_and_closes_empty_pages() {
        for (timed_out, failed, succeeds) in [
            (false, Some(0), true),
            (true, Some(0), true),
            (false, Some(1), false),
            (false, None, false),
        ] {
            let server = MockServer::start().await;
            let token = super::SearchCursor {
                pit_id: "pit".to_owned(),
                sort: vec![json!(1000), json!(1)],
            }
            .encode()
            .unwrap();
            Mock::given(matchers::method("POST"))
                .and(matchers::path("/_search"))
                .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                    "timed_out":timed_out,"_shards":{"failed":failed},
                    "hits":{"total":2,"hits":[]}
                })))
                .expect(1)
                .mount(&server)
                .await;
            Mock::given(matchers::method("DELETE"))
                .and(matchers::path("/_pit"))
                .respond_with(ResponseTemplate::new(200))
                .expect(1)
                .mount(&server)
                .await;
            let client = ElasticsearchClient::new(ElasticsearchSettings {
                url: Url::parse(&server.uri()).unwrap(),
                authentication: None,
                tls_insecure: false,
            })
            .unwrap();
            let result = client.search(cursor_request(&token)).await;
            assert_eq!(result.is_ok(), succeeds);
            if let Ok(result) = result {
                assert!(result.next_cursor.is_none());
                assert_eq!(result.timed_out, timed_out);
            }
        }
    }

    #[tokio::test]
    async fn mixed_pattern_scopes_time_field_and_adds_sort_fallbacks() {
        let server = MockServer::start().await;
        Mock::given(matchers::method("GET"))
            .and(matchers::path("/mix-*/_field_caps"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({"fields":{
                "@timestamp":{
                    "date":{"type":"date","aggregatable":true,"indices":["mix-a","mix-b"]},
                    "date_nanos":{"type":"date_nanos","aggregatable":true,"indices":["mix-c"]},
                    "keyword":{"type":"keyword","aggregatable":true,"indices":["mix-d"]}
                },
                "duration_ms":{
                    "long":{"type":"long","aggregatable":true,"indices":["mix-a"]},
                    "integer":{"type":"integer","aggregatable":true,"indices":["mix-c"]},
                    "unmapped":{"type":"unmapped","aggregatable":false,"indices":["mix-b","mix-d"]}
                }
            }})))
            .expect(1)
            .mount(&server)
            .await;
        Mock::given(matchers::method("POST"))
            .and(matchers::path("/mix-a,mix-b,mix-c/_search"))
            .and(matchers::query_param("allow_partial_search_results", "false"))
            .and(|request: &wiremock::Request| {
                let body: Value = serde_json::from_slice(&request.body).unwrap();
                body["sort"]
                    == json!([{"duration_ms":{"order":"desc","unmapped_type":"double","numeric_type":"double"}}])
            })
            .respond_with(
                ResponseTemplate::new(200).set_body_json(json!({"timed_out":false,"_shards":{"failed":0},"hits":{"total":0,"hits":[]}})),
            )
            .expect(1)
            .mount(&server)
            .await;
        let client = ElasticsearchClient::new(ElasticsearchSettings {
            url: Url::parse(&server.uri()).unwrap(),
            authentication: None,
            tls_insecure: false,
        })
        .unwrap();
        let request: SearchRequest = serde_json::from_value(json!({
            "index": "mix-*", "from": 0, "size": 2, "include_histogram": false,
            "sort_by": "duration_ms", "sort_order": "desc", "histogram_field": "@timestamp"
        }))
        .unwrap();
        client.search(request).await.unwrap();
    }

    #[tokio::test]
    async fn mixed_pattern_rejects_unsortable_and_incompatible_fields_before_searching() {
        for (types, unsortable) in [
            (json!({"text":{"type":"text","aggregatable":false}}), true),
            (
                json!({
                    "keyword":{"type":"keyword","aggregatable":true,"indices":["mix-a"]},
                    "boolean":{"type":"boolean","aggregatable":true,"indices":["mix-b"]}
                }),
                false,
            ),
        ] {
            let server = MockServer::start().await;
            Mock::given(matchers::method("GET"))
                .and(matchers::path("/mix-*/_field_caps"))
                .respond_with(
                    ResponseTemplate::new(200)
                        .set_body_json(json!({"fields":{"service":types}})),
                )
                .mount(&server)
                .await;
            let client = ElasticsearchClient::new(ElasticsearchSettings {
                url: Url::parse(&server.uri()).unwrap(),
                authentication: None,
                tls_insecure: false,
            })
            .unwrap();
            let request: SearchRequest = serde_json::from_value(json!({
                "index": "mix-*", "from": 0, "size": 2, "sort_by": "service"
            }))
            .unwrap();
            let error = client.search(request).await.unwrap_err();
            assert_eq!(
                matches!(error, super::ElasticsearchError::UnsortableField(_)),
                unsortable
            );
            assert_eq!(
                matches!(error, super::ElasticsearchError::IncompatibleField(_)),
                !unsortable
            );
        }
    }

    #[tokio::test]
    async fn time_field_without_date_mapping_is_a_clear_error() {
        let server = MockServer::start().await;
        Mock::given(matchers::method("GET"))
            .and(matchers::path("/logs/_field_caps"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({"fields":{
                "created_at":{"keyword":{"type":"keyword","aggregatable":true}}
            }})))
            .mount(&server)
            .await;
        let client = ElasticsearchClient::new(ElasticsearchSettings {
            url: Url::parse(&server.uri()).unwrap(),
            authentication: None,
            tls_insecure: false,
        })
        .unwrap();
        assert!(matches!(
            client.search(cursor_request("")).await,
            Err(super::ElasticsearchError::TimeFieldConflict(_))
        ));
    }

    #[test]
    fn cursor_tokens_reject_oversized_or_invalid_payloads() {
        use base64::Engine;
        for token in ["!".to_owned(), "a".repeat(32769)] {
            assert!(super::SearchCursor::decode(&token).is_err());
        }
        for payload in [
            json!({"pit_id":"","sort":[1]}),
            json!({"pit_id":1,"sort":[1]}),
            json!({"pit_id":"pit","sort":[]}),
            json!({"pit_id":"pit","sort":[{},1]}),
            json!({"pit_id":"pit","sort":[1,"bad-tie"]}),
            json!({"pit_id":"pit","sort":[1],"password":"secret"}),
        ] {
            assert!(
                super::SearchCursor::decode(&super::BASE64.encode(payload.to_string())).is_err()
            );
        }
        let mut request = cursor_request("");
        request.size = 101;
        assert!(validate_search(&request).is_err());
    }

    #[tokio::test]
    async fn include_histogram_false_skips_legacy_aggregations() {
        let server = MockServer::start().await;
        Mock::given(matchers::method("POST"))
            .and(matchers::path("/logs/_search"))
            .and(|request: &wiremock::Request| {
                let body: Value = serde_json::from_slice(&request.body).unwrap();
                body.get("aggs").is_none() && body["from"] == 0
            })
            .respond_with(
                ResponseTemplate::new(200).set_body_json(json!({"hits":{"total":0,"hits":[]}})),
            )
            .expect(1)
            .mount(&server)
            .await;
        let client = ElasticsearchClient::new(ElasticsearchSettings {
            url: Url::parse(&server.uri()).unwrap(),
            authentication: None,
            tls_insecure: false,
        })
        .unwrap();
        let mut request = cursor_request("");
        request.cursor = None;
        request.from = 0;
        request.include_histogram = Some(false);
        assert!(client.search(request).await.unwrap().histogram.is_empty());
    }

    #[tokio::test]
    async fn cursor_start_can_skip_histogram_and_missing_sort_cannot_advance() {
        for valid_sort in [true, false] {
            let server = MockServer::start().await;
            Mock::given(matchers::method("POST"))
                .and(matchers::path("/logs/_pit"))
                .respond_with(ResponseTemplate::new(200).set_body_json(json!({"id":"pit"})))
                .expect(1)
                .mount(&server)
                .await;
            let hits = if valid_sort {
                json!([])
            } else {
                json!([{"_index":"logs","_id":"a"},{"_index":"logs","_id":"b"}])
            };
            Mock::given(matchers::method("POST"))
                .and(matchers::path("/_search"))
                .and(|request: &wiremock::Request| {
                    let body: Value = serde_json::from_slice(&request.body).unwrap();
                    body.get("aggs").is_none() && body.get("from").is_none()
                })
                .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                    "timed_out":false,"_shards":{"failed":0},"hits":{"total":2,"hits":hits}
                })))
                .expect(1)
                .mount(&server)
                .await;
            Mock::given(matchers::method("DELETE"))
                .and(matchers::path("/_pit"))
                .respond_with(ResponseTemplate::new(200))
                .expect(1)
                .mount(&server)
                .await;
            let client = ElasticsearchClient::new(ElasticsearchSettings {
                url: Url::parse(&server.uri()).unwrap(),
                authentication: None,
                tls_insecure: false,
            })
            .unwrap();
            let mut request = cursor_request("");
            request.include_histogram = Some(false);
            assert_eq!(client.search(request).await.is_ok(), valid_sort);
        }
    }

    #[test]
    fn validate_index_accepts_patterns_and_rejects_path_segments() {
        assert!(validate_index("logs-*,metrics_2025.01").is_ok());
        assert!(validate_index("logs/../secrets").is_err());
    }

    #[test]
    fn validate_search_enforces_result_and_query_bounds() {
        let valid = SearchRequest {
            cursor: None,
            include_histogram: None,
            index: "logs-*".to_owned(),
            query: "service.name:api".to_owned(),
            from: 0,
            size: 50,
            sort_by: Some("@timestamp".to_owned()),
            sort_order: Some("desc".to_owned()),
            histogram_field: Some("@timestamp".to_owned()),
            time_range: None,
            ..Default::default()
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
            cursor: None,
            include_histogram: None,
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
            ..Default::default()
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
    async fn fixed_interval_histograms_use_timezone_bounds_and_break_down_top_values() {
        let server = MockServer::start().await;
        Mock::given(matchers::method("POST"))
            .and(|request: &wiremock::Request| {
                let body: serde_json::Value =
                    serde_json::from_slice(&request.body).expect("JSON search body");
                let over_time = &body["aggs"]["documents_over_time"]["date_histogram"];
                over_time["fixed_interval"] == "1m"
                    && over_time["time_zone"] == "Europe/Berlin"
                    && over_time["min_doc_count"] == 0
                    && over_time["extended_bounds"] == json!({"min": 0, "max": 120000})
                    && body["aggs"]["breakdown_values"]["terms"]
                        == json!({"field": "service", "size": 3})
                    && body["aggs"]["breakdown_values"]["aggs"]["over_time"]
                        == body["aggs"]["documents_over_time"]
            })
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "hits": {"total": {"value": 7, "relation": "eq"}, "hits": []},
                "aggregations": {
                    "documents_over_time": {"buckets": [
                        {"key": 0, "doc_count": 5}, {"key": 60000, "doc_count": 2}
                    ]},
                    "breakdown_values": {"buckets": [
                        {"key": "api", "doc_count": 4, "over_time": {"buckets": [{"key": 0, "doc_count": 3}, {"key": 60000, "doc_count": 1}]}},
                        {"key": 500, "doc_count": 1, "over_time": {"buckets": [{"key": 0, "doc_count": 1}]}}
                    ]}
                }
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
        let request: SearchRequest = serde_json::from_value(json!({
            "index": "logs", "histogram_field": "@timestamp", "histogram_interval": "1m",
            "histogram_breakdown_field": "service", "time_zone": "Europe/Berlin",
            "time_range": {"start": 0, "end": 120000}
        }))
        .unwrap();
        let result = client
            .search(request)
            .await
            .expect("histogram search succeeds");
        assert_eq!(result.histogram_interval.as_deref(), Some("1m"));
        let first = serde_json::to_value(&result.histogram[0]).unwrap();
        assert_eq!(
            first["breakdown"],
            json!([{"key":"api","count":3},{"key":"500","count":1},{"key":"__other__","count":1}])
        );
        let second = serde_json::to_value(&result.histogram[1]).unwrap();
        assert_eq!(
            second["breakdown"],
            json!([{"key":"api","count":1},{"key":"500","count":0},{"key":"__other__","count":1}])
        );
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
                        "lenient": true
                    }
                },
                "from": 0,
                "size": 50,
                "sort": [{"_score": {"order": "desc"}}],
                "timeout": "10s",
                "track_total_hits": true,
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
                cursor: None,
                include_histogram: None,
                index: "logs-*".to_owned(),
                query: "service.name:gateway".to_owned(),
                from: 0,
                size: 50,
                sort_by: None,
                sort_order: None,
                histogram_field: Some("@timestamp".to_owned()),
                time_range: None,
                ..Default::default()
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
