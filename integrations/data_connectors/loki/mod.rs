use std::{
    cmp::Ordering,
    collections::{BTreeMap, HashMap},
    hash::{Hash, Hasher},
    time::{Duration, Instant},
};

use crate::data_connectors::IntegrationDescriptor;
use crate::search::{
    BreakdownCount, HistogramInterval, HistogramOptions, TimeRange, BREAKDOWN_OTHER_KEY,
    MAX_BREAKDOWN_VALUES,
};
use chrono::{DateTime, SecondsFormat, Utc};
use reqwest::{header, redirect::Policy, Client, Response, Url};
use serde_json::{json, Value};
use thiserror::Error;

mod connector;
pub use connector::connect;

const REQUEST_TIMEOUT: Duration = Duration::from_secs(15);
const QUERY_TIMEOUT: &str = "10s";
const MAX_QUERY_BYTES: usize = 8192;
const MAX_RESPONSE_BYTES: usize = 16 * 1024 * 1024;
const MAX_RETURNED_ENTRIES: usize = 5000;
const MAX_PAGE_SIZE: usize = 100;
const MAX_CURSOR_OFFSET: u64 = MAX_RETURNED_ENTRIES as u64;
const NANOS_PER_MILLI: i64 = 1_000_000;

pub fn descriptor() -> IntegrationDescriptor {
    IntegrationDescriptor {
        id: "loki",
        display_name: "Grafana Loki",
        query_language: "LogQL",
        description: "Read-only log queries over Loki streams",
        connection_fields: vec![
            json!({"name":"url","label":"URL","input_type":"url","required":true,"secret":false,"editable":true,"placeholder":"http://localhost:3100"}),
            json!({"name":"auth_type","label":"Authentication","input_type":"select","required":false,"secret":false,"editable":true,"group":"authentication","placeholder":"","options":[{"value":"none","label":"None"},{"value":"basic","label":"Basic"},{"value":"bearer","label":"Bearer token"}]}),
            json!({"name":"username","label":"Username","input_type":"text","required":true,"secret":true,"editable":true,"group":"credentials","placeholder":"","visible_when":{"field":"auth_type","value":"basic"}}),
            json!({"name":"password","label":"Password","input_type":"password","required":true,"secret":true,"editable":true,"group":"credentials","placeholder":"","visible_when":{"field":"auth_type","value":"basic"}}),
            json!({"name":"bearer_token","label":"Bearer token","input_type":"password","required":true,"secret":true,"editable":true,"group":"credentials","placeholder":"","visible_when":{"field":"auth_type","value":"bearer"}}),
            json!({"name":"tenant_id","label":"Tenant ID (optional)","input_type":"text","required":false,"secret":false,"editable":true,"placeholder":"X-Scope-OrgID"}),
        ],
        query: json!({"placeholder":"Enter a LogQL query, e.g. {app=\"api\"} |= \"error\"","default_query":"{}","field_query":"{query}","identifier_quote":"","help":"LogQL query runs against the selected Loki log dataset","document_mode":"inline","sort_requires_projection":false,"histogram_total_from_buckets":false,"schema_label":"Loki stream labels"}),
    }
}

#[derive(Clone)]
pub struct LokiSettings {
    pub url: Url,
    pub auth_type: String,
    pub username: Option<String>,
    pub password: Option<String>,
    pub bearer_token: Option<String>,
    pub tenant_id: Option<String>,
}

#[derive(Clone)]
pub struct LokiClient {
    base_url: Url,
    client: Client,
    auth_type: &'static str,
    username: Option<String>,
    password: Option<String>,
    bearer_token: Option<String>,
    tenant_id: Option<String>,
}

#[derive(Debug, Error)]
pub enum LokiError {
    #[error("Loki URL must be an absolute HTTP or HTTPS URL without embedded credentials, query, or fragment")]
    InvalidUrl,
    #[error("Loki authentication or tenant settings are invalid")]
    InvalidConfiguration,
    #[error("LogQL query or search parameters are invalid or exceed configured limits")]
    InvalidQuery,
    #[error("Loki query returned its configured entry limit; narrow the query or time range")]
    TooManyEntries,
    #[error("Loki response exceeded the configured size limit")]
    ResponseTooLarge,
    #[error("Could not configure the Loki HTTP client")]
    ClientConfiguration,
    #[error("Loki request could not be completed")]
    Request,
    #[error("Loki rejected the request with HTTP {status}")]
    Rejected { status: u16 },
    #[error("Loki returned an invalid response")]
    InvalidResponse,
}

#[derive(Clone)]
struct LogEntry {
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

impl LokiClient {
    pub fn new(settings: LokiSettings) -> Result<Self, LokiError> {
        if !matches!(settings.url.scheme(), "http" | "https")
            || settings.url.host_str().is_none()
            || !settings.url.username().is_empty()
            || settings.url.password().is_some()
            || settings.url.query().is_some()
            || settings.url.fragment().is_some()
        {
            return Err(LokiError::InvalidUrl);
        }
        let (auth_type, username, password, bearer_token) = match settings.auth_type.as_str() {
            "none" if settings.username.is_none()
                && settings.password.is_none()
                && settings.bearer_token.is_none() => ("none", None, None, None),
            "basic"
                if settings.username.as_deref().is_some_and(|value| !value.trim().is_empty())
                    && settings.password.as_deref().is_some_and(|value| !value.is_empty())
                    && settings.bearer_token.is_none() =>
            {
                (
                    "basic",
                    settings.username,
                    settings.password,
                    None,
                )
            }
            "bearer"
                if settings.username.is_none()
                    && settings.password.is_none()
                    && settings
                        .bearer_token
                        .as_deref()
                        .is_some_and(|value| !value.trim().is_empty()) =>
            {
                ("bearer", None, None, settings.bearer_token)
            }
            _ => return Err(LokiError::InvalidConfiguration),
        };
        let tenant_id = settings.tenant_id.filter(|value| !value.trim().is_empty());
        if tenant_id
            .as_deref()
            .is_some_and(|value| value.len() > 256 || value.contains(['\r', '\n']))
        {
            return Err(LokiError::InvalidConfiguration);
        }
        let client = Client::builder()
            .timeout(REQUEST_TIMEOUT)
            .redirect(Policy::none())
            .build()
            .map_err(|_| LokiError::ClientConfiguration)?;
        Ok(Self {
            base_url: settings.url,
            client,
            auth_type,
            username,
            password,
            bearer_token,
            tenant_id,
        })
    }

    pub async fn test_connection(&self) -> Result<(), LokiError> {
        let response = self
            .request("loki/api/v1/labels")?
            .query(&[("limit", "1")])
            .send()
            .await
            .map_err(|_| LokiError::Request)?;
        let body = read_bounded(response).await?;
        let response: Value =
            serde_json::from_slice(&body).map_err(|_| LokiError::InvalidResponse)?;
        validate_success(&response)?;
        Ok(())
    }

    pub async fn fields(
        &self,
        index: &str,
    ) -> Result<Vec<crate::data_connectors::FieldSummary>, LokiError> {
        validate_index(index)?;
        let response = self
            .request("loki/api/v1/labels")?
            .send()
            .await
            .map_err(|_| LokiError::Request)?;
        let body = read_bounded(response).await?;
        let response: Value =
            serde_json::from_slice(&body).map_err(|_| LokiError::InvalidResponse)?;
        validate_success(&response)?;
        let labels = response
            .get("data")
            .and_then(Value::as_array)
            .ok_or(LokiError::InvalidResponse)?;
        let mut fields = vec![
            crate::data_connectors::FieldSummary {
                name: "_time".to_owned(),
                field_type: "timestamp".to_owned(),
                is_time_field: true,
            },
            crate::data_connectors::FieldSummary {
                name: "_msg".to_owned(),
                field_type: "text".to_owned(),
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
    ) -> Result<Value, LokiError> {
        validate_index(&request.index)?;
        if request.size == 0 || request.size > MAX_PAGE_SIZE {
            return Err(LokiError::InvalidQuery);
        }
        if request.time_range.is_some_and(|range| {
            !range.is_valid() || range.start < 0 || range.end > i64::MAX / NANOS_PER_MILLI - 1
        }) {
            return Err(LokiError::InvalidQuery);
        }
        let offset = cursor_offset(request.cursor.as_deref(), request.from)?;
        if request.sort_order.as_deref().is_some_and(|order| order != "asc" && order != "desc")
            || request
                .sort_by
                .as_deref()
                .is_some_and(|field| !crate::search::safe_field_name(field))
            || request
                .histogram_field
                .as_deref()
                .is_some_and(|field| field != "_time")
        {
            return Err(LokiError::InvalidQuery);
        }
        let options = request
            .histogram_options()
            .map_err(|_| LokiError::InvalidQuery)?;
        let query = request.query.trim();
        let query = if query.is_empty() { "{}" } else { query };
        if query.len() > MAX_QUERY_BYTES {
            return Err(LokiError::InvalidQuery);
        }
        if offset
            .checked_add(request.size as u64)
            .is_none_or(|end| end > MAX_RETURNED_ENTRIES as u64)
        {
            return Err(LokiError::TooManyEntries);
        }
        let requested = MAX_RETURNED_ENTRIES;
        let (start, end) = time_bounds(request.time_range)?;
        let started = Instant::now();
        let params = vec![
            ("query".to_owned(), query.to_owned()),
            ("limit".to_owned(), requested.to_string()),
            ("direction".to_owned(), "backward".to_owned()),
            ("start".to_owned(), start),
            ("end".to_owned(), end),
            ("timeout".to_owned(), QUERY_TIMEOUT.to_owned()),
        ];
        let response = self
            .request("loki/api/v1/query_range")?
            .query(&params)
            .send()
            .await
            .map_err(|_| LokiError::Request)?;
        let body = read_bounded(response).await?;
        let response: Value = serde_json::from_slice(&body).map_err(|_| LokiError::InvalidResponse)?;
        validate_success(&response)?;
        let data = response.get("data").ok_or(LokiError::InvalidResponse)?;
        if data.get("resultType").and_then(Value::as_str) != Some("streams") {
            return Err(LokiError::InvalidResponse);
        }
        let mut entries = parse_streams(data.get("result").and_then(Value::as_array).ok_or(LokiError::InvalidResponse)?)?;
        if entries.len() >= MAX_RETURNED_ENTRIES {
            return Err(LokiError::TooManyEntries);
        }
        if let Some(range) = request.time_range {
            entries.retain(|entry| entry.timestamp >= range.start && entry.timestamp <= range.end);
        }
        entries.sort_by(|left, right| compare_entries(left, right, &request));
        let total = entries.len() as u64;
        let end_offset = offset
            .checked_add(request.size as u64)
            .ok_or(LokiError::InvalidQuery)?;
        let next_cursor = (end_offset < total).then(|| end_offset.to_string());
        let first_page = request.cursor.as_deref().is_none_or(str::is_empty);
        let mut histogram = Vec::new();
        let mut histogram_interval = None;
        let mut histogram_unavailable_reason = None;
        if first_page
            && request.histogram_field.is_some()
            && request.include_histogram.unwrap_or(true)
        {
            if options.time_zone != "UTC" {
                histogram_unavailable_reason = Some("Loki histograms currently support UTC only.");
            } else {
                let range = request.time_range.unwrap_or(TimeRange {
                    start: entries.iter().map(|entry| entry.timestamp).min().unwrap_or(0),
                    end: entries.iter().map(|entry| entry.timestamp).max().unwrap_or(0),
                });
                let (buckets, interval) = sample_histogram(&entries, range, &options)?;
                histogram = buckets;
                histogram_interval = Some(interval);
            }
        }
        let hits = entries
            .into_iter()
            .skip(offset as usize)
            .take(request.size)
            .map(|entry| {
                json!({
                    "index": request.index,
                    "id": entry.id,
                    "score": null,
                    "source": entry.source
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
            "histogram_unavailable_reason": histogram_unavailable_reason,
        });
        if let Some(interval) = histogram_interval {
            result["histogram_interval"] = json!(interval);
        }
        if let Some(cursor) = next_cursor {
            result["next_cursor"] = json!(cursor);
        }
        Ok(result)
    }

    fn request(&self, path: &str) -> Result<reqwest::RequestBuilder, LokiError> {
        let mut endpoint = self.base_url.clone();
        let mut segments = endpoint
            .path_segments_mut()
            .map_err(|_| LokiError::InvalidUrl)?;
        segments.pop_if_empty();
        for segment in path.split('/') {
            segments.push(segment);
        }
        drop(segments);
        let mut request = self.client.get(endpoint);
        request = match self.auth_type {
            "basic" => request.basic_auth(
                self.username.as_deref().unwrap_or_default(),
                self.password.as_deref(),
            ),
            "bearer" => {
                let token = self.bearer_token.as_deref().ok_or(LokiError::InvalidConfiguration)?;
                let mut value = header::HeaderValue::from_str(&format!("Bearer {token}"))
                    .map_err(|_| LokiError::InvalidConfiguration)?;
                value.set_sensitive(true);
                request.header(header::AUTHORIZATION, value)
            }
            _ => request,
        };
        if let Some(tenant_id) = self.tenant_id.as_deref() {
            let value = header::HeaderValue::from_str(tenant_id)
                .map_err(|_| LokiError::InvalidConfiguration)?;
            request = request.header("X-Scope-OrgID", value);
        }
        Ok(request)
    }
}

fn validate_index(index: &str) -> Result<(), LokiError> {
    if index == "logs" {
        Ok(())
    } else {
        Err(LokiError::InvalidQuery)
    }
}

fn time_bounds(range: Option<TimeRange>) -> Result<(String, String), LokiError> {
    let (start, end) = range.map_or_else(
        || (0, Utc::now().timestamp_millis()),
        |range| (range.start, range.end),
    );
    let end = end
        .checked_add(1)
        .and_then(|value| value.checked_mul(NANOS_PER_MILLI))
        .ok_or(LokiError::InvalidQuery)?;
    let start = start
        .checked_mul(NANOS_PER_MILLI)
        .ok_or(LokiError::InvalidQuery)?;
    Ok((start.to_string(), end.to_string()))
}

fn parse_streams(streams: &[Value]) -> Result<Vec<LogEntry>, LokiError> {
    let mut entries = Vec::new();
    for stream in streams {
        let labels = stream
            .get("stream")
            .and_then(Value::as_object)
            .ok_or(LokiError::InvalidResponse)?;
        let values = stream
            .get("values")
            .and_then(Value::as_array)
            .ok_or(LokiError::InvalidResponse)?;
        for value in values {
            let value = value.as_array().ok_or(LokiError::InvalidResponse)?;
            if value.len() < 2 {
                return Err(LokiError::InvalidResponse);
            }
            let timestamp = value[0]
                .as_str()
                .and_then(|value| value.parse::<i64>().ok())
                .ok_or(LokiError::InvalidResponse)?;
            let line = value[1].as_str().ok_or(LokiError::InvalidResponse)?;
            let millis = timestamp.div_euclid(NANOS_PER_MILLI);
            let time = DateTime::<Utc>::from_timestamp(
                timestamp.div_euclid(1_000_000_000),
                timestamp.rem_euclid(1_000_000_000) as u32,
            )
                .ok_or(LokiError::InvalidResponse)?
                .to_rfc3339_opts(SecondsFormat::Nanos, true);
            let mut source = labels.clone();
            source.insert("_time".to_owned(), json!(time));
            source.insert("_msg".to_owned(), json!(line));
            let source = Value::Object(source);
            let mut hasher = std::collections::hash_map::DefaultHasher::new();
            source.to_string().hash(&mut hasher);
            entries.push(LogEntry {
                id: format!("log-{:016x}-{}", hasher.finish(), entries.len()),
                timestamp: millis,
                source,
            });
            if entries.len() >= MAX_RETURNED_ENTRIES {
                return Err(LokiError::TooManyEntries);
            }
        }
    }
    Ok(entries)
}

fn compare_entries(left: &LogEntry, right: &LogEntry, request: &crate::search::SearchRequest) -> Ordering {
    let field = request.sort_by.as_deref().unwrap_or("_time");
    let mut ordering = match (left.source.get(field), right.source.get(field)) {
        (Some(Value::String(left)), Some(Value::String(right))) if field == "_time" => {
            left.cmp(right)
        }
        (Some(left), Some(right)) => left.to_string().cmp(&right.to_string()),
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

fn sample_histogram(
    entries: &[LogEntry],
    range: TimeRange,
    options: &HistogramOptions,
) -> Result<(Vec<HistogramBucket>, String), LokiError> {
    let interval = options
        .interval
        .or_else(|| {
            HistogramInterval::automatic(
                range.end.saturating_sub(range.start).max(1) as f64,
            )
        })
        .ok_or(LokiError::InvalidQuery)?;
    let millis = interval.approximate_millis();
    let start = range.start.div_euclid(millis) * millis;
    let final_bucket = range.end.div_euclid(millis) * millis;
    let count = final_bucket.saturating_sub(start).div_euclid(millis) + 1;
    if count > crate::search::MAX_HISTOGRAM_BUCKETS {
        return Err(LokiError::InvalidQuery);
    }
    let mut buckets = BTreeMap::<i64, BucketCounts>::new();
    for index in 0..count {
        buckets.insert(start.saturating_add(index.saturating_mul(millis)), BucketCounts::default());
    }
    let mut key_totals = HashMap::<String, u64>::new();
    for entry in entries {
        let timestamp = entry.timestamp.div_euclid(millis) * millis;
        let bucket = buckets.entry(timestamp).or_default();
        bucket.count += 1;
        if let Some(field) = options.breakdown_field.as_deref() {
            if let Some(value) = entry.source.get(field).filter(|value| !value.is_null()) {
                let key = value
                    .as_str()
                    .map(str::to_owned)
                    .unwrap_or_else(|| value.to_string());
                *bucket.breakdown.entry(key.clone()).or_default() += 1;
                *key_totals.entry(key).or_default() += 1;
            }
        }
    }
    let mut top_keys = key_totals.into_iter().collect::<Vec<_>>();
    top_keys.sort_by(|left, right| right.1.cmp(&left.1).then_with(|| left.0.cmp(&right.0)));
    top_keys.truncate(MAX_BREAKDOWN_VALUES);
    let histogram = buckets
        .into_iter()
        .map(|(timestamp, bucket)| {
            let breakdown = options.breakdown_field.as_ref().map(|_| {
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
        .collect();
    Ok((histogram, format!("{}{}", interval.count, interval.suffix())))
}

fn validate_success(response: &Value) -> Result<(), LokiError> {
    if response.get("status").and_then(Value::as_str) == Some("success") {
        Ok(())
    } else {
        Err(LokiError::InvalidResponse)
    }
}

async fn read_bounded(response: Response) -> Result<Vec<u8>, LokiError> {
    let status = response.status();
    if !status.is_success() {
        return Err(LokiError::Rejected {
            status: status.as_u16(),
        });
    }
    if response
        .content_length()
        .is_some_and(|length| length > MAX_RESPONSE_BYTES as u64)
    {
        return Err(LokiError::ResponseTooLarge);
    }
    let mut response = response;
    let mut body = Vec::new();
    while let Some(chunk) = response.chunk().await.map_err(|_| LokiError::Request)? {
        if body.len().saturating_add(chunk.len()) > MAX_RESPONSE_BYTES {
            return Err(LokiError::ResponseTooLarge);
        }
        body.extend_from_slice(&chunk);
    }
    Ok(body)
}

fn cursor_offset(cursor: Option<&str>, from: usize) -> Result<u64, LokiError> {
    match cursor {
        None if from <= 10_000 => Ok(from as u64),
        Some("") => Ok(0),
        Some(token) if token.len() <= 4 && token.bytes().all(|byte| byte.is_ascii_digit()) => {
            token
                .parse::<u64>()
                .ok()
                .filter(|offset| *offset <= MAX_CURSOR_OFFSET)
                .ok_or(LokiError::InvalidQuery)
        }
        _ => Err(LokiError::InvalidQuery),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::data_connectors::Connector;
    use wiremock::{matchers, Mock, MockServer, ResponseTemplate};

    fn client(url: &str) -> LokiClient {
        LokiClient::new(LokiSettings {
            url: Url::parse(url).unwrap(),
            auth_type: "none".to_owned(),
            username: None,
            password: None,
            bearer_token: None,
            tenant_id: None,
        })
        .unwrap()
    }

    #[test]
    fn auth_configuration_rejects_mixed_or_partial_credentials() {
        for (auth_type, username, password, bearer_token) in [
            ("basic", Some("user".to_owned()), None, None),
            ("bearer", None, None, Some("   ".to_owned())),
            ("none", None, None, Some("token".to_owned())),
        ] {
            assert!(LokiClient::new(LokiSettings {
                url: Url::parse("http://localhost:3100").unwrap(),
                auth_type: auth_type.to_owned(),
                username,
                password,
                bearer_token,
                tenant_id: None,
            })
            .is_err());
        }
    }

    #[test]
    fn descriptor_uses_inline_logql_rows() {
        let descriptor = descriptor();
        assert_eq!(descriptor.id, "loki");
        assert_eq!(descriptor.query_language, "LogQL");
        assert_eq!(descriptor.query["document_mode"], "inline");
    }

    #[test]
    fn stream_response_flattens_labels_message_and_nanosecond_time() {
        let streams = json!([{
            "stream":{"app":"api","level":"error"},
            "values":[["86400000000000000","failed"]]
        }]);
        let entries = parse_streams(streams.as_array().unwrap()).unwrap();
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].source["app"], "api");
        assert_eq!(entries[0].source["_msg"], "failed");
        assert_eq!(entries[0].source["_time"], "1972-09-27T00:00:00.000000000Z");
    }

    #[tokio::test]
    async fn range_search_returns_shared_hits_and_histogram() {
        let server = MockServer::start().await;
        Mock::given(matchers::method("GET"))
            .and(matchers::path("/loki/api/v1/query_range"))
            .and(matchers::query_param("limit", "5000"))
            .and(matchers::query_param("start", "86400000000000"))
            .and(matchers::query_param("end", "86402001000000"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "status":"success","data":{"resultType":"streams","result":[{
                    "stream":{"app":"api","level":"error"},
                    "values":[["86402000000000","latest"],["86401000000000","middle"],["86400000000000","oldest"]]
                }]}
            })))
            .expect(1)
            .mount(&server)
            .await;
        let request = serde_json::from_value(json!({
            "index":"logs","query":"{app=\"api\"}","size":2,"cursor":"",
            "histogram_field":"_time","histogram_interval":"1s",
            "histogram_breakdown_field":"level",
            "time_range":{"start":86400000i64,"end":86402000i64}
        }))
        .unwrap();
        let result = Connector::search(&client(&server.uri()), request).await.unwrap();
        assert_eq!(result["total"], 3);
        assert_eq!(result["total_relation"], "eq");
        assert_eq!(result["hits"].as_array().unwrap().len(), 2);
        assert_eq!(result["hits"][0]["source"]["_msg"], "latest");
        assert_eq!(result["hits"][0]["source"]["app"], "api");
        assert_eq!(result["next_cursor"], "2");
        assert_eq!(result["histogram"].as_array().unwrap().len(), 3);
        assert_eq!(result["histogram"][0]["breakdown"][0]["key"], "error");
    }
}