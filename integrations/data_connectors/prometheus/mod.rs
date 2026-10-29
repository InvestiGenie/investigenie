use std::{
    cmp::Ordering,
    collections::{BTreeMap, HashMap},
    hash::{Hash, Hasher},
    time::{Duration, Instant},
};

use crate::connector_auth::HttpAuth;
use crate::data_connectors::IntegrationDescriptor;
use crate::search::{
    BreakdownCount, HistogramInterval, TimeRange, BREAKDOWN_OTHER_KEY, MAX_BREAKDOWN_VALUES,
};
use chrono::Utc;
use reqwest::{header::HeaderValue, redirect::Policy, Client, Response, Url};
use serde_json::{json, Map, Value};
use thiserror::Error;

mod connector;
pub use connector::{connect, connector_error};

const REQUEST_TIMEOUT: Duration = Duration::from_secs(15);
const QUERY_TIMEOUT: &str = "10s";
const MAX_QUERY_BYTES: usize = 8192;
const MAX_RESPONSE_BYTES: usize = 16 * 1024 * 1024;
const MAX_ERROR_BYTES: usize = 64 * 1024;
const MAX_SAMPLES: usize = 50_000;
const MAX_PAGE_SIZE: usize = 100;

pub fn descriptor() -> IntegrationDescriptor {
    IntegrationDescriptor {
        id: "prometheus",
        display_name: "Prometheus",
        icon: "prometheus",
        query_language: "PromQL",
        description: "Read-only PromQL range queries over time series",
        connection_fields: vec![
            json!({"name":"url","label":"URL","input_type":"url","required":true,"secret":false,"editable":true,"placeholder":"http://localhost:9090"}),
            json!({"name":"username","label":"Username","input_type":"text","required":false,"secret":true,"editable":true,"group":"credentials","placeholder":""}),
            json!({"name":"password","label":"Password","input_type":"password","required":false,"secret":true,"editable":true,"group":"credentials","placeholder":""}),
        ],
        query: json!({"placeholder":"Enter a PromQL expression, e.g. rate(http_requests_total[5m])","default_query":"{__name__!=\"\"}","field_query":"{query}","identifier_quote":"","help":"PromQL range queries return evaluated samples at a bounded step; missing samples may be filled by Prometheus lookback semantics","document_mode":"inline","sort_requires_projection":false,"histogram_total_from_buckets":true,"schema_label":"Prometheus labels"}),
        ..Default::default()
    }
}

/// Names and error codes for a server that speaks the Prometheus HTTP API, so compatible
/// backends such as Mimir reuse this client without reporting themselves as Prometheus.
pub struct ApiFlavor {
    pub name: &'static str,
    /// Whether the descriptor offers authentication modes and a tenant, which are then public.
    pub configurable_auth: bool,
    pub codes: ErrorCodes,
}

pub struct ErrorCodes {
    pub invalid_connection: &'static str,
    pub invalid_request: &'static str,
    pub result_limit_exceeded: &'static str,
    pub partial_response: &'static str,
    pub client_error: &'static str,
    pub unreachable: &'static str,
    pub authentication_failed: &'static str,
    pub rejected_request: &'static str,
    pub response_error: &'static str,
}

pub const PROMETHEUS: ApiFlavor = ApiFlavor {
    name: "Prometheus",
    configurable_auth: false,
    codes: ErrorCodes {
        invalid_connection: "invalid_prometheus_connection",
        invalid_request: "invalid_prometheus_request",
        result_limit_exceeded: "prometheus_result_limit_exceeded",
        partial_response: "prometheus_partial_response",
        client_error: "prometheus_client_error",
        unreachable: "prometheus_unreachable",
        authentication_failed: "prometheus_authentication_failed",
        rejected_request: "prometheus_rejected_request",
        response_error: "prometheus_response_error",
    },
};

pub struct PrometheusSettings {
    pub url: Url,
    pub auth: HttpAuth,
    /// Sent as `X-Scope-OrgID` by multi-tenant backends.
    pub tenant_id: Option<String>,
    pub flavor: &'static ApiFlavor,
}

pub struct PrometheusClient {
    base_url: Url,
    client: Client,
    auth: HttpAuth,
    tenant_id: Option<String>,
    flavor: &'static ApiFlavor,
}

#[derive(Debug, Error)]
pub enum PrometheusError {
    #[error("Prometheus URL must be an absolute HTTP or HTTPS URL without embedded credentials, query, or fragment")]
    InvalidUrl,
    #[error("Prometheus connection settings are invalid")]
    InvalidConfiguration,
    #[error("PromQL query or search parameters are invalid or exceed configured limits")]
    InvalidQuery,
    #[error("Prometheus returned more samples than this connector can safely read at once")]
    TooManyResults,
    #[error("Sorting by a label or value needs every sample, which exceeds the connector limit")]
    SortTooLarge,
    #[error("Prometheus response exceeded the configured size limit")]
    ResponseTooLarge,
    #[error("Prometheus returned warnings, so the result may be incomplete")]
    PartialResponse,
    #[error("Could not configure the Prometheus HTTP client")]
    ClientConfiguration,
    #[error("Prometheus request could not be completed")]
    Request,
    #[error("Could not obtain an OAuth 2.0 access token")]
    Authentication,
    #[error("Prometheus rejected the request with HTTP {status}")]
    Rejected { status: u16, error_type: String },
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

/// Number of evaluated samples at one query step, optionally for one breakdown label value.
struct StepCount {
    timestamp: i64,
    key: Option<String>,
    count: u64,
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
        let tenant_id = settings.tenant_id.filter(|value| !value.trim().is_empty());
        if matches!(&settings.auth, HttpAuth::Basic { username, .. } if username.trim().is_empty())
            || tenant_id
                .as_deref()
                .is_some_and(|value| value.len() > 256 || HeaderValue::from_str(value).is_err())
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
            auth: settings.auth,
            tenant_id,
            flavor: settings.flavor,
        })
    }

    pub fn auth(&self) -> &HttpAuth {
        &self.auth
    }

    pub fn tenant_id(&self) -> Option<&str> {
        self.tenant_id.as_deref()
    }

    pub fn flavor(&self) -> &'static ApiFlavor {
        self.flavor
    }

    pub async fn test_connection(&self) -> Result<(), PrometheusError> {
        let response = self
            .request("api/v1/query")
            .await?
            .form(&[("query", "vector(1)"), ("timeout", QUERY_TIMEOUT)])
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
            .request("api/v1/labels")
            .await?
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
        if step_millis <= 0 {
            return Err(PrometheusError::InvalidQuery);
        }

        let started = Instant::now();
        let first_page = request.cursor.as_deref().is_none_or(str::is_empty);
        let wants_histogram = first_page
            && request.histogram_field.is_some()
            && request.include_histogram.unwrap_or(true);
        let breakdown_field = options
            .breakdown_field
            .as_deref()
            .filter(|_| wants_histogram);
        let time_sorted = request.sort_by.as_deref().is_none_or(|field| field == "_time");
        let descending =
            request.sort_by.is_none() || request.sort_order.as_deref() == Some("desc");

        // Count evaluated samples per step on the server so totals and histograms never need every row.
        let counts = match breakdown_field {
            Some(field) if !is_label_name(field) => None,
            _ => {
                let count_query = match breakdown_field {
                    Some(label) => format!("count by ({label}) ({query})"),
                    None => format!("count({query})"),
                };
                match self.query_range(&count_query, range, step_millis).await {
                    Ok(series) => Some(parse_step_counts(&series, breakdown_field)?),
                    // Expressions that are not instant vectors, such as scalars, cannot be counted.
                    Err(PrometheusError::Rejected { status: 400, .. }) => None,
                    Err(error) => return Err(error),
                }
            }
        };

        let mut histogram = Vec::new();
        let (total, page) = match counts {
            Some(counts) => {
                let total = counts.iter().map(|entry| entry.count).sum::<u64>();
                if wants_histogram {
                    histogram = weighted_histogram(
                        range,
                        step_millis,
                        breakdown_field.is_some(),
                        counts
                            .iter()
                            .map(|entry| (entry.timestamp, entry.key.clone(), entry.count)),
                    )?;
                }
                let page = if offset >= total {
                    Vec::new()
                } else if time_sorted {
                    self.time_window_page(query, &counts, step_millis, offset, descending, &request)
                        .await?
                } else if total > MAX_SAMPLES as u64 {
                    return Err(PrometheusError::SortTooLarge);
                } else {
                    let samples = self
                        .evaluated_samples(query, range, step_millis, &request)
                        .await?;
                    page_of(samples, offset, request.size)
                };
                (total, page)
            }
            None => {
                let samples = self
                    .evaluated_samples(query, range, step_millis, &request)
                    .await?;
                if wants_histogram {
                    histogram = sample_histogram(&samples, range, step_millis, breakdown_field)?;
                }
                (samples.len() as u64, page_of(samples, offset, request.size))
            }
        };
        let end_offset = offset
            .checked_add(request.size as u64)
            .ok_or(PrometheusError::InvalidQuery)?;
        let next_cursor = (end_offset < total).then(|| end_offset.to_string());
        let hits = page
            .into_iter()
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
        if wants_histogram {
            result["histogram_interval"] = json!(format!("{}{}", interval.count, interval.suffix()));
        }
        if let Some(cursor) = next_cursor {
            result["next_cursor"] = json!(cursor);
        }
        Ok(result)
    }

    /// Fetches only the query steps that contain the requested time-sorted page.
    async fn time_window_page(
        &self,
        query: &str,
        counts: &[StepCount],
        step_millis: i64,
        offset: u64,
        descending: bool,
        request: &crate::search::SearchRequest,
    ) -> Result<Vec<EvaluatedSample>, PrometheusError> {
        let mut per_step = BTreeMap::<i64, u64>::new();
        for entry in counts {
            *per_step.entry(entry.timestamp).or_default() += entry.count;
        }
        let steps: Vec<(i64, u64)> = if descending {
            per_step.into_iter().rev().collect()
        } else {
            per_step.into_iter().collect()
        };
        let end_offset = offset.saturating_add(request.size as u64);
        let mut seen = 0u64;
        let mut before = 0u64;
        let mut window: Option<(i64, i64)> = None;
        for (timestamp, count) in steps {
            let next = seen.saturating_add(count);
            if next > offset && seen < end_offset {
                window = Some(match window {
                    None => {
                        before = seen;
                        (timestamp, timestamp)
                    }
                    Some((low, high)) => (low.min(timestamp), high.max(timestamp)),
                });
            }
            if next >= end_offset {
                break;
            }
            seen = next;
        }
        let Some((start, end)) = window else {
            return Ok(Vec::new());
        };
        let samples = self
            .evaluated_samples(query, TimeRange { start, end }, step_millis, request)
            .await?;
        Ok(page_of(samples, offset - before, request.size))
    }

    async fn evaluated_samples(
        &self,
        query: &str,
        range: TimeRange,
        step_millis: i64,
        request: &crate::search::SearchRequest,
    ) -> Result<Vec<EvaluatedSample>, PrometheusError> {
        let series = self.query_range(query, range, step_millis).await?;
        let mut samples = parse_matrix(&series)?;
        samples.retain(|sample| sample.timestamp >= range.start && sample.timestamp <= range.end);
        samples.sort_by(|left, right| compare_samples(left, right, request));
        Ok(samples)
    }

    async fn query_range(
        &self,
        query: &str,
        range: TimeRange,
        step_millis: i64,
    ) -> Result<Vec<Value>, PrometheusError> {
        let response = self
            .request("api/v1/query_range")
            .await?
            .form(&[
                ("query", query.to_owned()),
                ("start", seconds(range.start)),
                ("end", seconds(range.end)),
                ("step", format!("{step_millis}ms")),
                ("timeout", QUERY_TIMEOUT.to_owned()),
            ])
            .send()
            .await
            .map_err(|_| PrometheusError::Request)?;
        let body = read_bounded(response).await?;
        let mut response: Value =
            serde_json::from_slice(&body).map_err(|_| PrometheusError::InvalidResponse)?;
        validate_success(&response)?;
        let data = response
            .get_mut("data")
            .ok_or(PrometheusError::InvalidResponse)?;
        if data.get("resultType").and_then(Value::as_str) != Some("matrix") {
            return Err(PrometheusError::InvalidResponse);
        }
        match data.get_mut("result").map(Value::take) {
            Some(Value::Array(series)) => Ok(series),
            _ => Err(PrometheusError::InvalidResponse),
        }
    }

    async fn request(&self, path: &str) -> Result<reqwest::RequestBuilder, PrometheusError> {
        let mut endpoint = self.base_url.clone();
        let mut segments = endpoint
            .path_segments_mut()
            .map_err(|_| PrometheusError::InvalidUrl)?;
        segments.pop_if_empty();
        for segment in path.split('/') {
            segments.push(segment);
        }
        drop(segments);
        let mut request = self
            .auth
            .authorize(self.client.post(endpoint))
            .await
            .map_err(|_| PrometheusError::Authentication)?;
        if let Some(tenant_id) = self.tenant_id.as_deref() {
            let value =
                HeaderValue::from_str(tenant_id).map_err(|_| PrometheusError::InvalidConfiguration)?;
            request = request.header("X-Scope-OrgID", value);
        }
        Ok(request)
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
        Some(token) if token.len() <= 15 && token.bytes().all(|byte| byte.is_ascii_digit()) => {
            token.parse::<u64>().map_err(|_| PrometheusError::InvalidQuery)
        }
        _ => Err(PrometheusError::InvalidQuery),
    }
}

fn seconds(millis: i64) -> String {
    format!("{}.{:03}", millis.div_euclid(1000), millis.rem_euclid(1000))
}

fn is_label_name(name: &str) -> bool {
    let mut chars = name.chars();
    !matches!(name, "_time" | "_value")
        && chars
            .next()
            .is_some_and(|first| first.is_ascii_alphabetic() || first == '_')
        && chars.all(|next| next.is_ascii_alphanumeric() || next == '_')
}

fn page_of(samples: Vec<EvaluatedSample>, offset: u64, size: usize) -> Vec<EvaluatedSample> {
    samples
        .into_iter()
        .skip(usize::try_from(offset).unwrap_or(usize::MAX))
        .take(size)
        .collect()
}

fn parse_step_counts(
    series: &[Value],
    label: Option<&str>,
) -> Result<Vec<StepCount>, PrometheusError> {
    let mut counts = Vec::new();
    for series in series {
        let key = label
            .and_then(|label| series.get("metric")?.get(label)?.as_str())
            .map(str::to_owned);
        let values = series
            .get("values")
            .and_then(Value::as_array)
            .ok_or(PrometheusError::InvalidResponse)?;
        for point in values {
            let point = point
                .as_array()
                .filter(|point| point.len() == 2)
                .ok_or(PrometheusError::InvalidResponse)?;
            let count = point[1]
                .as_str()
                .and_then(|value| value.parse::<f64>().ok())
                .filter(|value| value.is_finite() && *value >= 0.0)
                .ok_or(PrometheusError::InvalidResponse)?;
            counts.push(StepCount {
                timestamp: timestamp_millis(&point[0])?,
                key: key.clone(),
                count: count as u64,
            });
        }
    }
    Ok(counts)
}

fn parse_matrix(series: &[Value]) -> Result<Vec<EvaluatedSample>, PrometheusError> {
    let mut samples = Vec::new();
    for series in series {
        let labels = series
            .get("metric")
            .and_then(Value::as_object)
            .ok_or(PrometheusError::InvalidResponse)?;
        // Native-histogram-only series carry `histograms` without `values`.
        let values = match series.get("values") {
            Some(values) => values
                .as_array()
                .ok_or(PrometheusError::InvalidResponse)?
                .as_slice(),
            None => &[],
        };
        for point in values {
            let point = point.as_array().ok_or(PrometheusError::InvalidResponse)?;
            if point.len() != 2 {
                return Err(PrometheusError::InvalidResponse);
            }
            let timestamp = timestamp_millis(&point[0])?;
            let value = parse_sample_value(&point[1])?;
            samples.push(make_sample(labels, timestamp, value));
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
                ));
                if samples.len() > MAX_SAMPLES {
                    return Err(PrometheusError::TooManyResults);
                }
            }
        }
    }
    Ok(samples)
}

/// Sample ids depend only on labels, time, and value so paging windows sort identically.
fn make_sample(labels: &Map<String, Value>, timestamp: i64, value: Value) -> EvaluatedSample {
    let mut source = labels.clone();
    source.insert("_time".to_owned(), json!(timestamp));
    source.insert("_value".to_owned(), value);
    let source = Value::Object(source);
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    source.to_string().hash(&mut hasher);
    EvaluatedSample {
        id: format!("sample-{:016x}", hasher.finish()),
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
    weighted_histogram(
        range,
        step_millis,
        breakdown_field.is_some(),
        samples.iter().map(|sample| {
            let key = breakdown_field
                .and_then(|field| sample.source.get(field))
                .filter(|value| !value.is_null())
                .map(|value| match value {
                    Value::String(value) => value.clone(),
                    other => other.to_string(),
                });
            (sample.timestamp, key, 1)
        }),
    )
}

fn weighted_histogram(
    range: TimeRange,
    step_millis: i64,
    with_breakdown: bool,
    entries: impl IntoIterator<Item = (i64, Option<String>, u64)>,
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
    for (timestamp, key, weight) in entries {
        let offset = timestamp.saturating_sub(range.start).max(0);
        let timestamp = range
            .start
            .saturating_add(offset.div_euclid(step_millis).saturating_mul(step_millis));
        let bucket = buckets.entry(timestamp).or_default();
        bucket.count += weight;
        if let Some(key) = key.filter(|_| with_breakdown) {
            *bucket.breakdown.entry(key.clone()).or_default() += weight;
            *key_totals.entry(key).or_default() += weight;
        }
    }
    let mut top_keys = key_totals.into_iter().collect::<Vec<_>>();
    top_keys.sort_by(|left, right| right.1.cmp(&left.1).then_with(|| left.0.cmp(&right.0)));
    top_keys.truncate(MAX_BREAKDOWN_VALUES);
    Ok(buckets
        .into_iter()
        .map(|(timestamp, bucket)| {
            let breakdown = with_breakdown.then(|| {
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
        // Only Prometheus's error category is kept; upstream error text is never returned to clients.
        let mut response = response;
        let mut body = Vec::new();
        while body.len() < MAX_ERROR_BYTES {
            match response.chunk().await {
                Ok(Some(chunk)) => body.extend_from_slice(&chunk),
                _ => break,
            }
        }
        let error_type = serde_json::from_slice::<Value>(&body)
            .ok()
            .and_then(|error| error.get("errorType")?.as_str().map(str::to_owned))
            .unwrap_or_default();
        return Err(PrometheusError::Rejected {
            status: status.as_u16(),
            error_type,
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
    use axum::http::StatusCode;
    use wiremock::{matchers, Mock, MockServer, ResponseTemplate};

    fn client(url: &str) -> PrometheusClient {
        PrometheusClient::new(PrometheusSettings {
            url: Url::parse(url).unwrap(),
            auth: HttpAuth::None,
            tenant_id: None,
            flavor: &PROMETHEUS,
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
                auth: HttpAuth::None,
                tenant_id: None,
                flavor: &PROMETHEUS,
            })
            .is_err());
        }
        assert!(PrometheusClient::new(PrometheusSettings {
            url: Url::parse("http://localhost:9090").unwrap(),
            auth: HttpAuth::Basic {
                username: " ".to_owned(),
                password: None,
            },
            tenant_id: None,
            flavor: &PROMETHEUS,
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
        }, {
            "metric":{"__name__":"latency"},
            "histograms":[[86400.0,{"count":"2","sum":"3"}]]
        }]);
        let samples = parse_matrix(matrix.as_array().unwrap()).unwrap();
        assert_eq!(samples.len(), 3);
        assert_eq!(samples[2].source["_value"]["count"], "2");
        assert_eq!(samples[0].timestamp, 86_400_000);
        assert_eq!(samples[0].source["_value"], 1.0);
        assert_eq!(samples[0].source["job"], "api");
        assert_eq!(samples[1].source["_value"], "NaN");
        assert_ne!(samples[0].id, samples[1].id);
    }

    fn form_contains(parts: &'static [&'static str]) -> impl Fn(&wiremock::Request) -> bool {
        move |request: &wiremock::Request| {
            let body = String::from_utf8_lossy(&request.body);
            parts.iter().all(|part| body.contains(part)) && !body.contains("limit=")
        }
    }

    #[tokio::test]
    async fn search_counts_on_the_server_and_fetches_only_the_page_window() {
        let server = MockServer::start().await;
        Mock::given(matchers::method("POST"))
            .and(matchers::path("/api/v1/query_range"))
            .and(form_contains(&["query=count+by+%28job%29+%28up%29", "step=1000ms"]))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "status":"success","data":{"resultType":"matrix","result":[
                    {"metric":{"job":"api"},"values":[[86400.0,"1"],[86401.0,"1"],[86402.0,"1"]]}
                ]}
            })))
            .expect(1)
            .mount(&server)
            .await;
        Mock::given(matchers::method("POST"))
            .and(matchers::path("/api/v1/query_range"))
            .and(form_contains(&["query=count%28up%29"]))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "status":"success","data":{"resultType":"matrix","result":[
                    {"metric":{},"values":[[86400.0,"1"],[86401.0,"1"],[86402.0,"1"]]}
                ]}
            })))
            .expect(1)
            .mount(&server)
            .await;
        Mock::given(matchers::method("POST"))
            .and(matchers::path("/api/v1/query_range"))
            .and(form_contains(&["query=up&", "start=86401.000", "end=86402.000"]))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "status":"success","data":{"resultType":"matrix","result":[
                    {"metric":{"__name__":"up","job":"api"},"values":[[86401.0,"0"],[86402.0,"1"]]}
                ]}
            })))
            .expect(1)
            .mount(&server)
            .await;
        Mock::given(matchers::method("POST"))
            .and(matchers::path("/api/v1/query_range"))
            .and(form_contains(&["query=up&", "start=86400.000", "end=86400.000"]))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "status":"success","data":{"resultType":"matrix","result":[
                    {"metric":{"__name__":"up","job":"api"},"values":[[86400.0,"1"]]}
                ]}
            })))
            .expect(1)
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
        assert_eq!(first["hits"][1]["source"]["_time"], 86401000);
        assert_eq!(first["next_cursor"], "2");
        assert_eq!(first["histogram"].as_array().unwrap().len(), 3);
        assert_eq!(first["histogram"][0]["count"], 1);
        assert_eq!(first["histogram"][0]["breakdown"][0]["key"], "api");
        assert_eq!(first["histogram_interval"], "1s");

        let mut next_request = request();
        next_request.cursor = Some(first["next_cursor"].as_str().unwrap().to_owned());
        let second = Connector::search(&client, next_request).await.unwrap();
        assert_eq!(second["total"], 3);
        assert_eq!(second["hits"].as_array().unwrap().len(), 1);
        assert_eq!(second["hits"][0]["source"]["_time"], 86400000);
        assert_eq!(second["histogram"], json!([]));
        assert!(second.get("next_cursor").is_none());
    }

    #[tokio::test]
    async fn uncountable_expressions_fall_back_and_large_value_sorts_fail_clearly() {
        let server = MockServer::start().await;
        Mock::given(matchers::method("POST"))
            .and(matchers::path("/api/v1/query_range"))
            .and(form_contains(&["query=count%28time%28%29%29"]))
            .respond_with(ResponseTemplate::new(400).set_body_json(
                json!({"status":"error","errorType":"bad_data","error":"expected instant vector"}),
            ))
            .expect(1)
            .mount(&server)
            .await;
        Mock::given(matchers::method("POST"))
            .and(matchers::path("/api/v1/query_range"))
            .and(form_contains(&["query=time%28%29&"]))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "status":"success","data":{"resultType":"matrix","result":[
                    {"metric":{},"values":[[86400.0,"86400"],[86401.0,"86401"]]}
                ]}
            })))
            .expect(1)
            .mount(&server)
            .await;
        Mock::given(matchers::method("POST"))
            .and(matchers::path("/api/v1/query_range"))
            .and(form_contains(&["query=count%28up%29"]))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "status":"success","data":{"resultType":"matrix","result":[
                    {"metric":{},"values":[[86400.0,"30000"],[86401.0,"30000"]]}
                ]}
            })))
            .expect(1)
            .mount(&server)
            .await;
        let client = client(&server.uri());
        let request = |query: &str, sort: Option<&str>| {
            serde_json::from_value(json!({
                "index":"metrics","query":query,"size":10,"cursor":"","sort_by":sort,
                "histogram_field":"_time","histogram_interval":"1s",
                "time_range":{"start":86400000i64,"end":86401000i64}
            }))
            .unwrap()
        };
        let scalar = client.search(request("time()", None)).await.unwrap();
        assert_eq!(scalar["total"], 2);
        assert_eq!(scalar["histogram"][1]["count"], 1);
        assert!(matches!(
            client.search(request("up", Some("_value"))).await,
            Err(PrometheusError::SortTooLarge)
        ));
    }

    #[test]
    fn rejections_use_curated_messages_by_prometheus_error_type() {
        for (status, error_type, expected_status, expected) in [
            (400, "bad_data", StatusCode::BAD_REQUEST, "could not parse"),
            (422, "execution", StatusCode::BAD_REQUEST, "within its limits"),
            (503, "timeout", StatusCode::GATEWAY_TIMEOUT, "timed out"),
            (401, "", StatusCode::BAD_GATEWAY, "credentials"),
        ] {
            let error: crate::data_connectors::ConnectorError = PrometheusError::Rejected {
                status,
                error_type: error_type.to_owned(),
            }
            .into();
            assert_eq!(error.status, expected_status);
            assert!(error.message.contains(expected), "{}", error.message);
        }
    }

    #[tokio::test]
    async fn connection_test_evaluates_a_constant_query() {
        let server = MockServer::start().await;
        Mock::given(matchers::method("POST"))
            .and(matchers::path("/api/v1/query"))
            .and(form_contains(&["query=vector%281%29"]))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "status":"success","data":{"resultType":"vector","result":[]}
            })))
            .expect(1)
            .mount(&server)
            .await;
        client(&server.uri()).test_connection().await.unwrap();
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
