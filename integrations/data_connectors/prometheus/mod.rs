use std::{
    cmp::Ordering,
    collections::{BTreeMap, HashMap},
    hash::{Hash, Hasher},
    time::{Duration, Instant},
};

use crate::data_connectors::IntegrationDescriptor;
use crate::search::{
    BreakdownCount, HistogramInterval, TimeRange, BREAKDOWN_OTHER_KEY, MAX_BREAKDOWN_VALUES,
};
use chrono::Utc;
use reqwest::{redirect::Policy, Client, Response, Url};
use serde_json::{json, Map, Value};
use thiserror::Error;

mod connector;
pub use connector::connect;

const REQUEST_TIMEOUT: Duration = Duration::from_secs(15);
const QUERY_TIMEOUT: &str = "10s";
const MAX_QUERY_BYTES: usize = 8192;
const MAX_RESPONSE_BYTES: usize = 16 * 1024 * 1024;
const MAX_SERIES: usize = 500;
const MAX_SAMPLES: usize = 50_000;
const MAX_PAGE_SIZE: usize = 100;

pub fn descriptor() -> IntegrationDescriptor {
    IntegrationDescriptor {
        id: "prometheus",
        display_name: "Prometheus",
        query_language: "PromQL",
        description: "Read-only PromQL range queries over time series",
        connection_fields: vec![
            json!({"name":"url","label":"URL","input_type":"url","required":true,"secret":false,"editable":true,"placeholder":"http://localhost:9090"}),
            json!({"name":"username","label":"Username","input_type":"text","required":false,"secret":true,"editable":true,"group":"credentials","placeholder":""}),
            json!({"name":"password","label":"Password","input_type":"password","required":false,"secret":true,"editable":true,"group":"credentials","placeholder":""}),
        ],
        query: json!({"placeholder":"Enter a PromQL expression, e.g. rate(http_requests_total[5m])","default_query":"{__name__!=\"\"}","field_query":"{query}","identifier_quote":"","help":"PromQL range queries return evaluated samples at a bounded step; missing samples may be filled by Prometheus lookback semantics","document_mode":"inline","sort_requires_projection":false,"histogram_total_from_buckets":true,"schema_label":"Prometheus labels"}),
    }
}

#[derive(Clone)]
pub struct PrometheusSettings {
    pub url: Url,
    pub username: Option<String>,
    pub password: Option<String>,
}

#[derive(Clone)]
pub struct PrometheusClient {
    base_url: Url,
    client: Client,
    username: Option<String>,
    password: Option<String>,
}

#[derive(Debug, Error)]
pub enum PrometheusError {
    #[error("Prometheus URL must be an absolute HTTP or HTTPS URL without embedded credentials, query, or fragment")]
    InvalidUrl,
    #[error("Prometheus connection settings are invalid")]
    InvalidConfiguration,
    #[error("PromQL query or search parameters are invalid or exceed configured limits")]
    InvalidQuery,
    #[error("Prometheus returned more series or samples than this connector can safely display")]
    TooManyResults,
    #[error("Prometheus response exceeded the configured size limit")]
    ResponseTooLarge,
    #[error("Prometheus returned warnings, so the result may be incomplete")]
    PartialResponse,
    #[error("Could not configure the Prometheus HTTP client")]
    ClientConfiguration,
    #[error("Prometheus request could not be completed")]
    Request,
    #[error("Prometheus rejected the request with HTTP {status}")]
    Rejected { status: u16 },
    #[error("Prometheus returned an invalid response")]
    InvalidResponse,
}

#[derive(Clone)]
struct EvaluatedSample {
    id: String,
    timestamp: i64,
    source: Value,
}

#[derive(Default)]
struct BucketCounts {
    count: u64,
    breakdown: BTreeMap<String, u64>,
}

#[derive(Debug, serde::Serialize)]
struct HistogramBucket {
    timestamp: i64,
    label: Option<String>,
    count: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    breakdown: Option<Vec<BreakdownCount>>,
}

impl PrometheusClient {
    pub fn new(settings: PrometheusSettings) -> Result<Self, PrometheusError> {
        if !matches!(settings.url.scheme(), "http" | "https")
            || settings.url.host_str().is_none()
            || !settings.url.username().is_empty()
            || settings.url.password().is_some()
            || settings.url.query().is_some()
            || settings.url.fragment().is_some()
        {
            return Err(PrometheusError::InvalidUrl);
        }
        if settings
            .username
            .as_deref()
            .is_some_and(|value| value.trim().is_empty())
            || settings.username.is_some() != settings.password.is_some()
        {
            return Err(PrometheusError::InvalidConfiguration);
        }
        let client = Client::builder()
            .timeout(REQUEST_TIMEOUT)
            .redirect(Policy::none())
            .build()
            .map_err(|_| PrometheusError::ClientConfiguration)?;
        Ok(Self {
            base_url: settings.url,
            client,
            username: settings.username,
            password: settings.password,
        })
    }

    pub async fn test_connection(&self) -> Result<(), PrometheusError> {
        let response = self
            .request("api/v1/labels")?
            .query(&[("limit", "1"), ("timeout", QUERY_TIMEOUT)])
            .send()
            .await
            .map_err(|_| PrometheusError::Request)?;
        let body = read_bounded(response).await?;
        let response: Value =
            serde_json::from_slice(&body).map_err(|_| PrometheusError::InvalidResponse)?;
        validate_success(&response)?;
        Ok(())
    }

    pub async fn fields(
        &self,
        index: &str,
    ) -> Result<Vec<crate::data_connectors::FieldSummary>, PrometheusError> {
        validate_index(index)?;
        let response = self
            .request("api/v1/labels")?
            .query(&[("timeout", QUERY_TIMEOUT)])
            .send()
            .await
            .map_err(|_| PrometheusError::Request)?;
        let body = read_bounded(response).await?;
        let response: Value =
            serde_json::from_slice(&body).map_err(|_| PrometheusError::InvalidResponse)?;
        validate_success(&response)?;
        let labels = response
            .get("data")
            .and_then(Value::as_array)
            .ok_or(PrometheusError::InvalidResponse)?;
        let mut fields = vec![
            crate::data_connectors::FieldSummary {
                name: "_time".to_owned(),
                field_type: "timestamp".to_owned(),
                is_time_field: true,
            },
            crate::data_connectors::FieldSummary {
                name: "_value".to_owned(),
                field_type: "sample value".to_owned(),
                is_time_field: false,
            },
        ];
        fields.extend(labels.iter().filter_map(Value::as_str).map(|name| {
            crate::data_connectors::FieldSummary {
                name: name.to_owned(),
                field_type: "label".to_owned(),
                is_time_field: false,
            }
        }));
        fields.sort_by(|left, right| left.name.cmp(&right.name));
        fields.dedup_by(|left, right| left.name == right.name);
        Ok(fields)
    }

    pub async fn search(
        &self,
        request: crate::search::SearchRequest,
    ) -> Result<Value, PrometheusError> {
        validate_index(&request.index)?;
        if request.size == 0 || request.size > MAX_PAGE_SIZE {
            return Err(PrometheusError::InvalidQuery);
        }
        if request.time_range.is_some_and(|range| !range.is_valid() || range.start < 0) {
            return Err(PrometheusError::InvalidQuery);
        }
        let offset = cursor_offset(request.cursor.as_deref(), request.from)?;
        let options = request
            .histogram_options()
            .map_err(|_| PrometheusError::InvalidQuery)?;
        if request.sort_order.as_deref().is_some_and(|order| order != "asc" && order != "desc")
            || request
                .sort_by
                .as_deref()
                .is_some_and(|field| !crate::search::safe_field_name(field))
        {
            return Err(PrometheusError::InvalidQuery);
        }
        if request
            .histogram_field
            .as_deref()
            .is_some_and(|field| field != "_time")
        {
            return Err(PrometheusError::InvalidQuery);
        }
        let query = request.query.trim();
        let query = if query.is_empty() {
            "{__name__!=\"\"}"
        } else {
            query
        };
        if query.len() > MAX_QUERY_BYTES || query.contains(';') {
            return Err(PrometheusError::InvalidQuery);
        }

        let range = request.time_range.unwrap_or(TimeRange {
            start: 0,
            end: Utc::now().timestamp_millis(),
        });
        let span = range.end.saturating_sub(range.start).max(1);
        let interval = options
            .interval
            .or_else(|| HistogramInterval::automatic(span as f64))
            .ok_or(PrometheusError::InvalidQuery)?;
        let step_millis = interval.approximate_millis();
        let step = format!("{step_millis}ms");
        let start = range.start as f64 / 1000.0;
        let end = range.end as f64 / 1000.0;

        let started = Instant::now();
        let response = self
            .request("api/v1/query_range")?
            .form(&[
                ("query", query.to_owned()),
                ("start", start.to_string()),
                ("end", end.to_string()),
                ("step", step),
                ("timeout", QUERY_TIMEOUT.to_owned()),
                ("limit", (MAX_SERIES + 1).to_string()),
            ])
            .send()
            .await
            .map_err(|_| PrometheusError::Request)?;
        let body = read_bounded(response).await?;
        let response: Value =
            serde_json::from_slice(&body).map_err(|_| PrometheusError::InvalidResponse)?;
        validate_success(&response)?;
        if response
            .get("warnings")
            .and_then(Value::as_array)
            .is_some_and(|warnings| !warnings.is_empty())
        {
            return Err(PrometheusError::PartialResponse);
        }
        let data = response
            .get("data")
            .ok_or(PrometheusError::InvalidResponse)?;
        if data.get("resultType").and_then(Value::as_str) != Some("matrix") {
            return Err(PrometheusError::InvalidResponse);
        }
        let series = data
            .get("result")
            .and_then(Value::as_array)
            .ok_or(PrometheusError::InvalidResponse)?;
        if series.len() > MAX_SERIES {
            return Err(PrometheusError::TooManyResults);
        }
        let mut samples = parse_matrix(series)?;
        if samples.len() > MAX_SAMPLES {
            return Err(PrometheusError::TooManyResults);
        }
        samples.retain(|sample| sample.timestamp >= range.start && sample.timestamp <= range.end);
        samples.sort_by(|left, right| compare_samples(left, right, &request));
        let total = samples.len() as u64;
        let end_offset = offset
            .checked_add(request.size as u64)
            .ok_or(PrometheusError::InvalidQuery)?;
        let next_cursor = (end_offset < total).then(|| end_offset.to_string());
        let first_page = request.cursor.as_deref().is_none_or(str::is_empty);
        let mut histogram = Vec::new();
        let mut histogram_interval = None;
        if first_page
            && request.histogram_field.is_some()
            && request.include_histogram.unwrap_or(true)
        {
            histogram = sample_histogram(&samples, range, step_millis, options.breakdown_field.as_deref())?;
            histogram_interval = Some(format!("{}{}", interval.count, interval.suffix()));
        }
        let hits = samples
            .into_iter()
            .skip(offset as usize)
            .take(request.size)
            .map(|sample| {
                json!({
                    "index": request.index,
                    "id": sample.id,
                    "score": null,
                    "source": sample.source
                })
            })
            .collect::<Vec<_>>();
        let took = started.elapsed().as_millis().min(u64::MAX as u128) as u64;
        let mut result = json!({
            "took": took,
            "timed_out": false,
            "total": total,
            "total_relation": "eq",
            "hits": hits,
            "histogram": histogram,
        });
        if let Some(interval) = histogram_interval {
            result["histogram_interval"] = json!(interval);
        }
        if let Some(cursor) = next_cursor {
            result["next_cursor"] = json!(cursor);
        }
        Ok(result)
    }

    fn request(&self, path: &str) -> Result<reqwest::RequestBuilder, PrometheusError> {
        let mut endpoint = self.base_url.clone();
        let mut segments = endpoint
            .path_segments_mut()
            .map_err(|_| PrometheusError::InvalidUrl)?;
        segments.pop_if_empty();
        for segment in path.split('/') {
            segments.push(segment);
        }
        drop(segments);
        let request = self.client.post(endpoint);
        Ok(match (self.username.as_deref(), self.password.as_deref()) {
            (Some(username), Some(password)) => request.basic_auth(username, Some(password)),
            _ => request,
        })
    }
}

fn validate_index(index: &str) -> Result<(), PrometheusError> {
    if index == "metrics" {
        Ok(())
    } else {
        Err(PrometheusError::InvalidQuery)
    }
}

fn cursor_offset(cursor: Option<&str>, from: usize) -> Result<u64, PrometheusError> {
    match cursor {
        None if from <= 10_000 => Ok(from as u64),
        Some("") => Ok(0),
        Some(token) if token.len() <= 6 && token.bytes().all(|byte| byte.is_ascii_digit()) => {
            token
                .parse::<u64>()
                .ok()
                .filter(|offset| *offset <= MAX_SAMPLES as u64)
                .ok_or(PrometheusError::InvalidQuery)
        }
        _ => Err(PrometheusError::InvalidQuery),
    }
}

fn parse_matrix(series: &[Value]) -> Result<Vec<EvaluatedSample>, PrometheusError> {
    let mut samples = Vec::new();
    for series in series {
        let labels = series
            .get("metric")
            .and_then(Value::as_object)
            .ok_or(PrometheusError::InvalidResponse)?;
        let values = series
            .get("values")
            .and_then(Value::as_array)
            .ok_or(PrometheusError::InvalidResponse)?;
        for point in values {
            let point = point.as_array().ok_or(PrometheusError::InvalidResponse)?;
            if point.len() != 2 {
                return Err(PrometheusError::InvalidResponse);
            }
            let timestamp = timestamp_millis(&point[0])?;
            let value = parse_sample_value(&point[1])?;
            samples.push(make_sample(labels, timestamp, value, samples.len()));
            if samples.len() > MAX_SAMPLES {
                return Err(PrometheusError::TooManyResults);
            }
        }
        if let Some(histograms) = series.get("histograms").and_then(Value::as_array) {
            for point in histograms {
                let point = point.as_array().ok_or(PrometheusError::InvalidResponse)?;
                if point.len() != 2 {
                    return Err(PrometheusError::InvalidResponse);
                }
                samples.push(make_sample(
                    labels,
                    timestamp_millis(&point[0])?,
                    point[1].clone(),
                    samples.len(),
                ));
                if samples.len() > MAX_SAMPLES {
                    return Err(PrometheusError::TooManyResults);
                }
            }
        }
    }
    Ok(samples)
}

fn make_sample(labels: &Map<String, Value>, timestamp: i64, value: Value, ordinal: usize) -> EvaluatedSample {
    let mut source = labels.clone();
    source.insert("_time".to_owned(), json!(timestamp));
    source.insert("_value".to_owned(), value);
    let source = Value::Object(source);
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    source.to_string().hash(&mut hasher);
    EvaluatedSample {
        id: format!("sample-{:016x}-{ordinal}", hasher.finish()),
        timestamp,
        source,
    }
}

fn timestamp_millis(value: &Value) -> Result<i64, PrometheusError> {
    let seconds = value.as_f64().ok_or(PrometheusError::InvalidResponse)?;
    if !seconds.is_finite() {
        return Err(PrometheusError::InvalidResponse);
    }
    let milliseconds = (seconds * 1000.0).round();
    if milliseconds < i64::MIN as f64 || milliseconds > i64::MAX as f64 {
        return Err(PrometheusError::InvalidResponse);
    }
    Ok(milliseconds as i64)
}

fn parse_sample_value(value: &Value) -> Result<Value, PrometheusError> {
    let value = value.as_str().ok_or(PrometheusError::InvalidResponse)?;
    if let Ok(number) = value.parse::<f64>() {
        if number.is_finite() {
            return serde_json::Number::from_f64(number)
                .map(Value::Number)
                .ok_or(PrometheusError::InvalidResponse);
        }
    }
    Ok(Value::String(value.to_owned()))
}

fn compare_samples(
    left: &EvaluatedSample,
    right: &EvaluatedSample,
    request: &crate::search::SearchRequest,
) -> Ordering {
    let field = request.sort_by.as_deref().unwrap_or("_time");
    let mut ordering = match (left.source.get(field), right.source.get(field)) {
        (Some(left), Some(right)) => compare_values(left, right),
        (None, None) => Ordering::Equal,
        (None, Some(_)) => Ordering::Less,
        (Some(_), None) => Ordering::Greater,
    };
    let descending = request.sort_by.is_none() || request.sort_order.as_deref() == Some("desc");
    if descending {
        ordering = ordering.reverse();
    }
    ordering.then_with(|| left.id.cmp(&right.id))
}

fn compare_values(left: &Value, right: &Value) -> Ordering {
    match (left, right) {
        (Value::Number(left), Value::Number(right)) => left
            .as_f64()
            .partial_cmp(&right.as_f64())
            .unwrap_or(Ordering::Equal),
        (Value::String(left), Value::String(right)) => left.cmp(right),
        _ => left.to_string().cmp(&right.to_string()),
    }
}

fn sample_histogram(
    samples: &[EvaluatedSample],
    range: TimeRange,
    step_millis: i64,
    breakdown_field: Option<&str>,
) -> Result<Vec<HistogramBucket>, PrometheusError> {
    if step_millis <= 0 {
        return Err(PrometheusError::InvalidQuery);
    }
    let bucket_count = range.end.saturating_sub(range.start) / step_millis + 1;
    if bucket_count > crate::search::MAX_HISTOGRAM_BUCKETS {
        return Err(PrometheusError::InvalidQuery);
    }
    let mut buckets = BTreeMap::<i64, BucketCounts>::new();
    for index in 0..bucket_count {
        let timestamp = range.start.saturating_add(index.saturating_mul(step_millis));
        buckets.insert(timestamp, BucketCounts::default());
    }
    let mut key_totals = HashMap::<String, u64>::new();
    for sample in samples {
        let offset = sample.timestamp.saturating_sub(range.start).max(0);
        let timestamp = range
            .start
            .saturating_add(offset.div_euclid(step_millis).saturating_mul(step_millis));
        let bucket = buckets.entry(timestamp).or_default();
        bucket.count += 1;
        if let Some(field) = breakdown_field {
            if let Some(value) = sample.source.get(field).filter(|value| !value.is_null()) {
                let key = match value {
                    Value::String(value) => value.clone(),
                    other => other.to_string(),
                };
                *bucket.breakdown.entry(key.clone()).or_default() += 1;
                *key_totals.entry(key).or_default() += 1;
            }
        }
    }
    let mut top_keys = key_totals.into_iter().collect::<Vec<_>>();
    top_keys.sort_by(|left, right| right.1.cmp(&left.1).then_with(|| left.0.cmp(&right.0)));
    top_keys.truncate(MAX_BREAKDOWN_VALUES);
    Ok(buckets
        .into_iter()
        .map(|(timestamp, bucket)| {
            let breakdown = breakdown_field.map(|_| {
                let mut values = top_keys
                    .iter()
                    .map(|(key, _)| BreakdownCount {
                        key: key.clone(),
                        count: bucket.breakdown.get(key).copied().unwrap_or(0),
                    })
                    .collect::<Vec<_>>();
                let listed = values.iter().map(|value| value.count).sum::<u64>();
                values.push(BreakdownCount {
                    key: BREAKDOWN_OTHER_KEY.to_owned(),
                    count: bucket.count.saturating_sub(listed),
                });
                values
            });
            HistogramBucket {
                timestamp,
                label: None,
                count: bucket.count,
                breakdown,
            }
        })
        .collect())
}

fn validate_success(response: &Value) -> Result<(), PrometheusError> {
    if response.get("status").and_then(Value::as_str) != Some("success") {
        return Err(PrometheusError::InvalidResponse);
    }
    if response
        .get("warnings")
        .and_then(Value::as_array)
        .is_some_and(|warnings| !warnings.is_empty())
    {
        return Err(PrometheusError::PartialResponse);
    }
    Ok(())
}

async fn read_bounded(response: Response) -> Result<Vec<u8>, PrometheusError> {
    let status = response.status();
    if !status.is_success() {
        return Err(PrometheusError::Rejected {
            status: status.as_u16(),
        });
    }
    if response
        .content_length()
        .is_some_and(|length| length > MAX_RESPONSE_BYTES as u64)
    {
        return Err(PrometheusError::ResponseTooLarge);
    }
    let mut response = response;
    let mut body = Vec::new();
    while let Some(chunk) = response
        .chunk()
        .await
        .map_err(|_| PrometheusError::Request)?
    {
        if body.len().saturating_add(chunk.len()) > MAX_RESPONSE_BYTES {
            return Err(PrometheusError::ResponseTooLarge);
        }
        body.extend_from_slice(&chunk);
    }
    Ok(body)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::data_connectors::Connector;
    use wiremock::{matchers, Mock, MockServer, ResponseTemplate};

    fn client(url: &str) -> PrometheusClient {
        PrometheusClient::new(PrometheusSettings {
            url: Url::parse(url).unwrap(),
            username: None,
            password: None,
        })
        .unwrap()
    }

    #[test]
    fn connection_settings_reject_unsafe_urls_and_partial_basic_auth() {
        for raw_url in [
            "http://user:secret@localhost:9090",
            "http://localhost:9090?secret=value",
            "http://localhost:9090#fragment",
            "file:///tmp/prometheus",
        ] {
            assert!(PrometheusClient::new(PrometheusSettings {
                url: Url::parse(raw_url).unwrap(),
                username: None,
                password: None,
            })
            .is_err());
        }
        assert!(PrometheusClient::new(PrometheusSettings {
            url: Url::parse("http://localhost:9090").unwrap(),
            username: Some("user".to_owned()),
            password: None,
        })
        .is_err());
    }

    #[test]
    fn descriptor_uses_promql_and_inline_evaluated_samples() {
        let descriptor = descriptor();
        assert_eq!(descriptor.id, "prometheus");
        assert_eq!(descriptor.query_language, "PromQL");
        assert_eq!(descriptor.query["document_mode"], "inline");
        assert!(descriptor.query["help"].as_str().unwrap().contains("lookback"));
    }

    #[test]
    fn matrix_samples_map_labels_value_and_seconds_to_shared_rows() {
        let matrix = json!([{
            "metric":{"__name__":"up","job":"api"},
            "values":[[86400.0,"1"],[86401.25,"NaN"]]
        }]);
        let samples = parse_matrix(matrix.as_array().unwrap()).unwrap();
        assert_eq!(samples.len(), 2);
        assert_eq!(samples[0].timestamp, 86_400_000);
        assert_eq!(samples[0].source["_value"], 1.0);
        assert_eq!(samples[0].source["job"], "api");
        assert_eq!(samples[1].source["_value"], "NaN");
        assert_ne!(samples[0].id, samples[1].id);
    }

    #[tokio::test]
    async fn search_returns_bounded_pages_and_histogram_with_prometheus_shape() {
        let server = MockServer::start().await;
        Mock::given(matchers::method("POST"))
            .and(matchers::path("/api/v1/query_range"))
            .and(|request: &wiremock::Request| {
                let body = String::from_utf8_lossy(&request.body);
                body.contains("query=up") && body.contains("step=1000ms") && body.contains("limit=501")
            })
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "status":"success","data":{"resultType":"matrix","result":[
                    {"metric":{"__name__":"up","job":"api"},"values":[
                        [86400.0,"1"],[86401.0,"0"],[86402.0,"1"]
                    ]}
                ]}
            })))
            .expect(2)
            .mount(&server)
            .await;
        let client = client(&server.uri());
        let request = || {
            serde_json::from_value(json!({
                "index":"metrics","query":"up","size":2,"cursor":"",
                "histogram_field":"_time","histogram_interval":"1s",
                "histogram_breakdown_field":"job",
                "time_range":{"start":86400000i64,"end":86402000i64}
            }))
            .unwrap()
        };
        let first = Connector::search(&client, request()).await.unwrap();
        assert_eq!(first["total"], 3);
        assert_eq!(first["total_relation"], "eq");
        assert_eq!(first["hits"].as_array().unwrap().len(), 2);
        assert_eq!(first["hits"][0]["source"]["_time"], 86402000);
        assert_eq!(first["hits"][0]["source"]["_value"], 1.0);
        assert_eq!(first["next_cursor"], "2");
        assert_eq!(first["histogram"].as_array().unwrap().len(), 3);
        assert_eq!(first["histogram"][0]["breakdown"][0]["key"], "api");

        let mut next_request = request();
        next_request.cursor = Some(first["next_cursor"].as_str().unwrap().to_owned());
        let second = Connector::search(&client, next_request).await.unwrap();
        assert_eq!(second["hits"].as_array().unwrap().len(), 1);
        assert_eq!(second["hits"][0]["source"]["_time"], 86400000);
        assert_eq!(second["histogram"], json!([]));
        assert!(second.get("next_cursor").is_none());
    }

    #[tokio::test]
    async fn fields_include_virtual_time_and_sample_value_fields() {
        let server = MockServer::start().await;
        Mock::given(matchers::method("POST"))
            .and(matchers::path("/api/v1/labels"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "status":"success","data":["__name__","job","instance"]
            })))
            .expect(1)
            .mount(&server)
            .await;
        let fields = client(&server.uri()).fields("metrics").await.unwrap();
        assert!(fields.iter().any(|field| field.name == "_time" && field.is_time_field));
        assert!(fields.iter().any(|field| field.name == "_value"));
        assert!(fields.iter().any(|field| field.name == "job"));
    }
}