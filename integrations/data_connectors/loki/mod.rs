use std::{
    cmp::Ordering,
    collections::{BTreeMap, HashMap},
    hash::{Hash, Hasher},
    sync::Mutex,
    time::{Duration, Instant},
};

use crate::connector_auth::HttpAuth;
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
pub mod investigation;
pub use connector::connect;

const REQUEST_TIMEOUT: Duration = Duration::from_secs(15);
const QUERY_TIMEOUT: &str = "10s";
const MAX_QUERY_BYTES: usize = 8192;
const MAX_RESPONSE_BYTES: usize = 16 * 1024 * 1024;
const MAX_RETURNED_ENTRIES: usize = 5000;
const MAX_PAGE_SIZE: usize = 100;
const MAX_CURSOR_OFFSET: u64 = MAX_RETURNED_ENTRIES as u64;
const NANOS_PER_MILLI: i64 = 1_000_000;
const MAX_ERROR_BYTES: usize = 4096;
const MAX_CONFIG_BYTES: usize = 4 * 1024 * 1024;
const MAX_PARSED_FIELDS: usize = 200;
/// Loki refuses range queries with more points per series than this.
const MAX_METRIC_POINTS: i64 = 11_000;
/// The match-all stream selector for `{}`; Loki 3 labels every stream with `service_name`.
const ALL_STREAMS: &str = "{service_name=~\".+\"}";
/// Connection setting holding the discovered (or overridden) `max_query_length`; 0 means unlimited.
pub(super) const MAX_QUERY_LENGTH_SETTING: &str = "max_query_length_ms";

pub fn descriptor() -> IntegrationDescriptor {
    IntegrationDescriptor {
        id: "loki",
        display_name: "Grafana Loki",
        icon: "loki",
        query_language: "LogQL",
        description: "Read-only log queries over Loki streams",
        connection_fields: vec![
            json!({"name":"url","label":"URL","input_type":"url","required":true,"secret":false,"editable":true,"placeholder":"http://localhost:3100"}),
        ]
        .into_iter()
        .chain(crate::connector_auth::connection_fields())
        .chain([json!({"name":"tenant_id","label":"Tenant ID (optional)","input_type":"text","required":false,"secret":false,"editable":true,"placeholder":"X-Scope-OrgID"})])
        .collect(),
        query: json!({"placeholder":"Enter a LogQL query, e.g. {app=\"api\"} |= \"error\"","default_query":"{}","field_query":"{query}","identifier_quote":"","help":"LogQL query runs against the selected Loki log dataset","document_mode":"inline","sort_requires_projection":false,"histogram_total_from_buckets":false,"schema_label":"Loki stream labels"}),
        ..Default::default()
    }
}

pub struct LokiSettings {
    pub url: Url,
    pub auth: HttpAuth,
    pub tenant_id: Option<String>,
    /// Loki's `max_query_length` in milliseconds when known; 0 means unlimited.
    pub max_query_length: Option<i64>,
}

pub struct LokiClient {
    base_url: Url,
    client: Client,
    auth: HttpAuth,
    tenant_id: Option<String>,
    max_query_length: Option<i64>,
    /// Set by `test_connection` from Loki's `/config`, then saved with the connection.
    learned_max_query_length: Mutex<Option<i64>>,
}

#[derive(Debug, Error)]
pub enum LokiError {
    #[error("Loki URL must be an absolute HTTP or HTTPS URL without embedded credentials, query, or fragment")]
    InvalidUrl,
    #[error("Loki authentication or tenant settings are invalid")]
    InvalidConfiguration,
    #[error("LogQL query or search parameters are invalid or exceed configured limits")]
    InvalidQuery,
    #[error("Loki needs a stream selector with at least one non-empty label matcher")]
    SelectorRequired,
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
    #[error("Could not obtain an OAuth 2.0 access token for Loki")]
    Authentication,
    #[error("The time range exceeds Loki's max_query_length of {limit_millis} ms")]
    QueryTooLong { limit_millis: i64 },
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

impl BucketCounts {
    fn add(&mut self, key: Option<&str>, count: u64) {
        self.count += count;
        if let Some(key) = key {
            *self.breakdown.entry(key.to_owned()).or_default() += count;
        }
    }
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
        if settings.max_query_length.is_some_and(|limit| limit < 0) {
            return Err(LokiError::InvalidConfiguration);
        }
        Ok(Self {
            base_url: settings.url,
            client,
            auth: settings.auth,
            tenant_id,
            max_query_length: settings.max_query_length,
            learned_max_query_length: Mutex::new(None),
        })
    }

    pub fn auth(&self) -> &HttpAuth {
        &self.auth
    }

    pub fn tenant_id(&self) -> Option<&str> {
        self.tenant_id.as_deref()
    }

    pub fn max_query_length(&self) -> Option<i64> {
        self.max_query_length
    }

    pub fn learned_max_query_length(&self) -> Option<i64> {
        self.learned_max_query_length
            .lock()
            .ok()
            .and_then(|learned| *learned)
    }

    pub async fn test_connection(&self) -> Result<(), LokiError> {
        let response = self
            .request("loki/api/v1/labels")
            .await?
            .query(&[("limit", "1")])
            .send()
            .await
            .map_err(|_| LokiError::Request)?;
        let body = read_bounded(response).await?;
        let response: Value =
            serde_json::from_slice(&body).map_err(|_| LokiError::InvalidResponse)?;
        validate_success(&response)?;
        let learned = self.discover_max_query_length().await;
        if let Ok(mut slot) = self.learned_max_query_length.lock() {
            *slot = learned;
        }
        Ok(())
    }

    /// Reads `limits_config.max_query_length` from Loki's `/config`. It is an admin endpoint that
    /// gateways often block, and it shows defaults rather than per-tenant overrides, so a missing
    /// value is normal: searches then learn the limit from Loki's rejection instead.
    async fn discover_max_query_length(&self) -> Option<i64> {
        let response = self.request("config").await.ok()?.send().await.ok()?;
        let body = read_limited(response, MAX_CONFIG_BYTES).await.ok()?;
        configured_max_query_length(&String::from_utf8_lossy(&body))
    }

    /// Fields Loki detects in recent log lines (`| json`, `| logfmt`) or structured metadata.
    /// Older Loki versions lack the endpoint, so failures only omit these fields.
    async fn detected_fields(&self, labels: &[&str]) -> Vec<crate::data_connectors::FieldSummary> {
        let end = Utc::now().timestamp_millis();
        let start = end - 24 * 60 * 60 * 1000;
        let start = self
            .max_query_length
            .filter(|limit| *limit > 0)
            .map_or(start, |limit| start.max(end + 1 - limit));
        let Ok((start, end)) = time_bounds(Some(TimeRange { start, end })) else {
            return Vec::new();
        };
        let Ok(request) = self.request("loki/api/v1/detected_fields").await else {
            return Vec::new();
        };
        let response = request
            .query(&[
                ("query", "{service_name=~\".+\"}"),
                ("start", &start),
                ("end", &end),
                ("limit", &MAX_PARSED_FIELDS.to_string()),
            ])
            .send()
            .await;
        let Ok(response) = response else {
            return Vec::new();
        };
        let Ok(body) = read_bounded(response).await else {
            return Vec::new();
        };
        let Ok(response) = serde_json::from_slice::<Value>(&body) else {
            return Vec::new();
        };
        response
            .get("fields")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
            .filter_map(|field| {
                let name = field.get("label")?.as_str()?;
                if matches!(name, "_msg" | "_time") || labels.contains(&name) || !valid_label_name(name) {
                    return None;
                }
                let parsers = field.get("parsers").and_then(Value::as_array);
                let has = |parser: &str| parsers.is_some_and(|parsers| parsers.iter().any(|value| value == parser));
                // JSON wins when both parsers saw the field; chaining them can mis-parse lines.
                let source = if has("json") { "json" } else if has("logfmt") { "logfmt" } else { "metadata" };
                let value_type = field.get("type")?.as_str()?;
                ValueType::parse(value_type)?;
                Some(crate::data_connectors::FieldSummary {
                    name: name.to_owned(),
                    field_type: format!("{source} {value_type}"),
                    is_time_field: false,
                })
            })
            .take(MAX_PARSED_FIELDS)
            .collect()
    }

    pub async fn fields(
        &self,
        index: &str,
    ) -> Result<Vec<crate::data_connectors::FieldSummary>, LokiError> {
        validate_index(index)?;
        let response = self
            .request("loki/api/v1/labels")
            .await?
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
        let labels = labels.iter().filter_map(Value::as_str).collect::<Vec<_>>();
        fields.extend(labels.iter().map(|name| crate::data_connectors::FieldSummary {
            name: (*name).to_owned(),
            field_type: "label".to_owned(),
            is_time_field: false,
        }));
        fields.extend(self.detected_fields(&labels).await);
        fields.sort_by(|left, right| left.name.cmp(&right.name));
        fields.dedup_by(|left, right| left.name == right.name);
        Ok(fields)
    }

    /// Searches within the known `max_query_length`. When the limit was unknown or has changed,
    /// Loki's rejection states it, and the search is retried once within that limit.
    pub async fn search(
        &self,
        request: crate::search::SearchRequest,
    ) -> Result<Value, LokiError> {
        match self.search_within(&request, self.max_query_length).await {
            Err(LokiError::QueryTooLong { limit_millis })
                if limit_millis > 0 && self.max_query_length != Some(limit_millis) =>
            {
                self.search_within(&request, Some(limit_millis)).await
            }
            result => result,
        }
    }

    async fn search_within(
        &self,
        request: &crate::search::SearchRequest,
        max_query_length: Option<i64>,
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
        let (time_range, clamped) = within_query_length(request.time_range, max_query_length);
        let (start, end) = time_bounds(time_range)?;
        let started = Instant::now();
        let query = self.resolve_selector(query, &start, &end).await?;
        if !query.starts_with('{') {
            return self
                .search_metric(request, &query, time_range, clamped, options.interval, offset, started)
                .await;
        }
        let mut entries = self
            .entries(&query, &start, &end, MAX_RETURNED_ENTRIES, "backward")
            .await?;
        let saturated = entries.len() >= MAX_RETURNED_ENTRIES;
        let time_sorted = request.sort_by.as_deref().is_none_or(|field| field == "_time");
        if saturated && !time_sorted {
            return Err(LokiError::TooManyEntries);
        }
        let ascending = request.sort_by.is_some() && request.sort_order.as_deref() != Some("desc");
        if saturated && ascending {
            // The backward fetch holds the newest entries; an ascending page needs the oldest.
            let limit = (offset as usize).saturating_add(request.size);
            entries = self.entries(&query, &start, &end, limit, "forward").await?;
        }
        if let Some(range) = time_range {
            entries.retain(|entry| entry.timestamp >= range.start && entry.timestamp <= range.end);
        }
        entries.sort_by(|left, right| compare_entries(left, right, request));
        let first_page = request.cursor.as_deref().is_none_or(str::is_empty);
        let wants_histogram = first_page
            && request.histogram_field.is_some()
            && request.include_histogram.unwrap_or(true);
        let mut total = entries.len() as u64;
        let mut total_relation = "eq";
        let mut histogram = Vec::new();
        let mut histogram_interval = None;
        let mut histogram_unavailable_reason = None;
        if wants_histogram && options.time_zone != "UTC" {
            histogram_unavailable_reason = Some("Loki histograms currently support UTC only.");
        }
        let histogram_wanted = wants_histogram && histogram_unavailable_reason.is_none();
        if !saturated {
            if histogram_wanted {
                let range = time_range.unwrap_or(TimeRange {
                    start: entries.iter().map(|entry| entry.timestamp).min().unwrap_or(0),
                    end: entries.iter().map(|entry| entry.timestamp).max().unwrap_or(0),
                });
                let (buckets, interval) = sample_histogram(&entries, range, &options)?;
                histogram = buckets;
                histogram_interval = Some(interval);
            }
        } else {
            // Above the entry cap, Loki's metric engine counts every matching line; rows stay a bounded page.
            let breakdown = options.breakdown_field.as_deref().filter(|_| histogram_wanted);
            let counted = match (time_range, breakdown) {
                (None, _) => Err("Loki counts large results only for a bounded time range."),
                (Some(_), Some(field)) if matches!(field, "_msg" | "_time") || !valid_label_name(field) => {
                    Err("Loki breaks down large results by stream label only.")
                }
                (Some(range), _) => self
                    .count_lines(&query, range, histogram_wanted.then_some(&options), breakdown)
                    .await
                    .map_err(|_| "Loki could not count the full result; the total is a lower bound."),
            };
            match counted {
                Ok((count, buckets)) => {
                    total = count;
                    if let Some((buckets, interval)) = buckets {
                        histogram = buckets;
                        histogram_interval = Some(interval);
                    }
                }
                Err(reason) => {
                    total = MAX_RETURNED_ENTRIES as u64;
                    total_relation = "gte";
                    if wants_histogram {
                        histogram_unavailable_reason.get_or_insert(reason);
                    }
                }
            }
        }
        if clamped {
            // Matches older than Loki's max_query_length window were not searched.
            total_relation = "gte";
        }
        let end_offset = offset
            .checked_add(request.size as u64)
            .ok_or(LokiError::InvalidQuery)?;
        let next_cursor = (end_offset < total.min(MAX_RETURNED_ENTRIES as u64))
            .then(|| end_offset.to_string());
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
            "total_relation": total_relation,
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

    /// Metric queries (`sum by (service) (count_over_time({…} [5m]))`) return one row per series
    /// and evaluation step. Steps fall on multiples of the step since the epoch, so each value
    /// covers the bucket `(t - step, t]` that other sources' computations use for the same range.
    #[allow(clippy::too_many_arguments)]
    async fn search_metric(
        &self,
        request: &crate::search::SearchRequest,
        query: &str,
        time_range: Option<TimeRange>,
        clamped: bool,
        interval: Option<HistogramInterval>,
        offset: u64,
        started: Instant,
    ) -> Result<Value, LokiError> {
        let range = time_range.unwrap_or_else(|| {
            let end = Utc::now().timestamp_millis();
            TimeRange { start: end - 3_600_000, end }
        });
        let interval = interval
            .or_else(|| HistogramInterval::automatic(range.end.saturating_sub(range.start).max(1) as f64))
            .ok_or(LokiError::InvalidQuery)?;
        let step = interval.approximate_millis();
        if step <= 0 {
            return Err(LokiError::InvalidQuery);
        }
        let mut samples = Vec::new();
        if let Some(TimeRange { start: first, end: last }) = crate::investigation_dialect::bucket_ends(range, step) {
            if (last - first) / step + 1 > MAX_METRIC_POINTS {
                return Err(LokiError::InvalidQuery);
            }
            let start = first.checked_mul(NANOS_PER_MILLI).ok_or(LokiError::InvalidQuery)?.to_string();
            let end = last.checked_mul(NANOS_PER_MILLI).ok_or(LokiError::InvalidQuery)?.to_string();
            let step = format!("{step}ms");
            let params = [
                ("query", query),
                ("start", start.as_str()),
                ("end", end.as_str()),
                ("step", step.as_str()),
                ("timeout", QUERY_TIMEOUT),
            ];
            let data = self.query_data("loki/api/v1/query_range", "matrix", &params).await?;
            samples = parse_matrix(
                data.get("result").and_then(Value::as_array).ok_or(LokiError::InvalidResponse)?,
                MAX_RETURNED_ENTRIES,
            )?;
        }
        samples.sort_by(|left, right| compare_entries(left, right, request));
        let total = samples.len() as u64;
        let end_offset = offset.checked_add(request.size as u64).ok_or(LokiError::InvalidQuery)?;
        let hits = samples
            .into_iter()
            .skip(offset as usize)
            .take(request.size)
            .map(|entry| json!({"index": request.index, "id": entry.id, "score": null, "source": entry.source}))
            .collect::<Vec<_>>();
        let wants_histogram = request.cursor.as_deref().is_none_or(str::is_empty)
            && request.histogram_field.is_some()
            && request.include_histogram.unwrap_or(true);
        let mut result = json!({
            "took": started.elapsed().as_millis().min(u64::MAX as u128) as u64,
            "timed_out": false,
            "total": total,
            "total_relation": if clamped { "gte" } else { "eq" },
            "hits": hits,
            "histogram": [],
            "histogram_unavailable_reason": wants_histogram
                .then_some("Metric queries return their values as rows, one per series and step."),
        });
        if end_offset < total {
            result["next_cursor"] = json!(end_offset.to_string());
        }
        Ok(result)
    }

    /// Loki rejects `{}`. Loki 3 labels every stream with `service_name`, so a non-empty
    /// `service_name` matcher selects all streams; older servers need an explicit selector.
    /// Metric queries can hold several selectors, so each `{}` outside a string is replaced.
    async fn resolve_selector(&self, query: &str, start: &str, end: &str) -> Result<String, LokiError> {
        let empty = empty_selectors(query);
        if empty.is_empty() {
            return Ok(query.to_owned());
        }
        let response = self
            .request("loki/api/v1/labels")
            .await?
            .query(&[("start", start), ("end", end)])
            .send()
            .await
            .map_err(|_| LokiError::Request)?;
        let body = read_bounded(response).await?;
        let response: Value = serde_json::from_slice(&body).map_err(|_| LokiError::InvalidResponse)?;
        validate_success(&response)?;
        let labels = response
            .get("data")
            .and_then(Value::as_array)
            .ok_or(LokiError::InvalidResponse)?;
        // No labels means no streams in range, so the match-all selector simply returns nothing.
        if !labels.is_empty() && !labels.iter().any(|label| label == "service_name") {
            return Err(LokiError::SelectorRequired);
        }
        let mut resolved = String::with_capacity(query.len() + empty.len() * ALL_STREAMS.len());
        let mut copied = 0;
        for (start, end) in empty {
            resolved.push_str(&query[copied..start]);
            resolved.push_str(ALL_STREAMS);
            copied = end;
        }
        resolved.push_str(&query[copied..]);
        Ok(resolved)
    }

    async fn entries(
        &self,
        query: &str,
        start: &str,
        end: &str,
        limit: usize,
        direction: &str,
    ) -> Result<Vec<LogEntry>, LokiError> {
        let limit_text = limit.to_string();
        let params = [
            ("query", query),
            ("limit", limit_text.as_str()),
            ("direction", direction),
            ("start", start),
            ("end", end),
            ("timeout", QUERY_TIMEOUT),
        ];
        let data = self.query_data("loki/api/v1/query_range", "streams", &params).await?;
        parse_streams(
            data.get("result")
                .and_then(Value::as_array)
                .ok_or(LokiError::InvalidResponse)?,
            limit,
        )
    }

    /// Exact line counts over the inclusive range, plus a histogram when `options` is given.
    ///
    /// Range queries evaluate at step-aligned bucket ends and LogQL windows are left-open, so a
    /// line exactly on an interior bucket boundary lands in the earlier bucket. The last bucket is
    /// counted on its own and the first is the remainder of the exact total, so buckets always sum
    /// to the total.
    async fn count_lines(
        &self,
        query: &str,
        range: TimeRange,
        options: Option<&HistogramOptions>,
        breakdown: Option<&str>,
    ) -> Result<(u64, Option<(Vec<HistogramBucket>, String)>), LokiError> {
        // The inclusive millisecond end, as the last nanosecond Loki's left-open window must include.
        let last_nanos = range
            .end
            .checked_add(1)
            .and_then(|value| value.checked_mul(NANOS_PER_MILLI))
            .ok_or(LokiError::InvalidQuery)?
            - 1;
        let totals = self
            .instant_counts(query, range.end + 1 - range.start, last_nanos, breakdown)
            .await?;
        let Some(options) = options else {
            return Ok((totals.count, None));
        };
        let (interval, millis, mut buckets) = histogram_grid(range, options)?;
        let first = *buckets.keys().next().ok_or(LokiError::InvalidQuery)?;
        let last = *buckets.keys().next_back().ok_or(LokiError::InvalidQuery)?;
        if last > first {
            buckets.insert(
                last,
                self.instant_counts(query, range.end + 1 - last, last_nanos, breakdown)
                    .await?,
            );
        }
        if last - millis > first {
            let expression = count_expression(query, millis, breakdown);
            let start = (first + 2 * millis).saturating_mul(NANOS_PER_MILLI).to_string();
            let end = last.saturating_mul(NANOS_PER_MILLI).to_string();
            let step = format!("{millis}ms");
            let data = self
                .query_data(
                    "loki/api/v1/query_range",
                    "matrix",
                    &[
                        ("query", expression.as_str()),
                        ("start", start.as_str()),
                        ("end", end.as_str()),
                        ("step", step.as_str()),
                        ("timeout", QUERY_TIMEOUT),
                    ],
                )
                .await?;
            for series in data
                .get("result")
                .and_then(Value::as_array)
                .ok_or(LokiError::InvalidResponse)?
            {
                let key = series_key(series, breakdown);
                for point in series
                    .get("values")
                    .and_then(Value::as_array)
                    .ok_or(LokiError::InvalidResponse)?
                {
                    let (time, count) = count_point(point)?;
                    // Each point counts the bucket that ends at its evaluation time.
                    let bucket = (time + millis / 2).div_euclid(millis) * millis - millis;
                    buckets
                        .get_mut(&bucket)
                        .filter(|_| bucket > first && bucket < last)
                        .ok_or(LokiError::InvalidResponse)?
                        .add(key.as_deref(), count);
                }
            }
        }
        let mut remainder = totals;
        for counts in buckets.values().skip(1) {
            remainder.count = remainder.count.saturating_sub(counts.count);
            for (key, count) in &counts.breakdown {
                if let Some(total) = remainder.breakdown.get_mut(key) {
                    *total = total.saturating_sub(*count);
                }
            }
        }
        buckets.insert(first, remainder);
        let total = buckets.values().map(|counts| counts.count).sum();
        Ok((total, Some(finish_histogram(buckets, interval, breakdown.is_some()))))
    }

    async fn instant_counts(
        &self,
        query: &str,
        window_millis: i64,
        time_nanos: i64,
        breakdown: Option<&str>,
    ) -> Result<BucketCounts, LokiError> {
        let expression = count_expression(query, window_millis, breakdown);
        let time = time_nanos.to_string();
        let data = self
            .query_data(
                "loki/api/v1/query",
                "vector",
                &[
                    ("query", expression.as_str()),
                    ("time", time.as_str()),
                    ("timeout", QUERY_TIMEOUT),
                ],
            )
            .await?;
        let mut counts = BucketCounts::default();
        for series in data
            .get("result")
            .and_then(Value::as_array)
            .ok_or(LokiError::InvalidResponse)?
        {
            let point = series.get("value").ok_or(LokiError::InvalidResponse)?;
            counts.add(series_key(series, breakdown).as_deref(), count_point(point)?.1);
        }
        Ok(counts)
    }

    async fn query_data(
        &self,
        path: &str,
        result_type: &str,
        params: &[(&str, &str)],
    ) -> Result<Value, LokiError> {
        let response = self
            .request(path)
            .await?
            .query(params)
            .send()
            .await
            .map_err(|_| LokiError::Request)?;
        let body = read_bounded(response).await?;
        let mut response: Value =
            serde_json::from_slice(&body).map_err(|_| LokiError::InvalidResponse)?;
        validate_success(&response)?;
        let data = response
            .get_mut("data")
            .map(Value::take)
            .ok_or(LokiError::InvalidResponse)?;
        if data.get("resultType").and_then(Value::as_str) != Some(result_type) {
            return Err(LokiError::InvalidResponse);
        }
        Ok(data)
    }

    async fn request(&self, path: &str) -> Result<reqwest::RequestBuilder, LokiError> {
        let mut endpoint = self.base_url.clone();
        let mut segments = endpoint
            .path_segments_mut()
            .map_err(|_| LokiError::InvalidUrl)?;
        segments.pop_if_empty();
        for segment in path.split('/') {
            segments.push(segment);
        }
        drop(segments);
        let mut request = self
            .auth
            .authorize(self.client.get(endpoint))
            .await
            .map_err(|_| LokiError::Authentication)?;
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

/// Byte ranges of the `{}` stream selectors outside string literals.
fn empty_selectors(query: &str) -> Vec<(usize, usize)> {
    let mut found = Vec::new();
    let mut quote = None;
    let mut escaped = false;
    for (index, character) in query.char_indices() {
        match quote {
            Some(_) if escaped => escaped = false,
            Some(open) if character == '\\' && open != '`' => escaped = true,
            Some(open) if character == open => quote = None,
            Some(_) => {}
            None if matches!(character, '"' | '`') => quote = Some(character),
            None if character == '{' => {
                let rest = &query[index + 1..];
                let inner = rest.len() - rest.trim_start().len();
                if rest[inner..].starts_with('}') {
                    found.push((index, index + 1 + inner + 1));
                }
            }
            None => {}
        }
    }
    found
}

/// Metric query series as rows: the series labels, `_time`, and `_value`.
fn parse_matrix(series: &[Value], limit: usize) -> Result<Vec<LogEntry>, LokiError> {
    let mut entries = Vec::new();
    for item in series {
        let labels = item.get("metric").and_then(Value::as_object).ok_or(LokiError::InvalidResponse)?;
        let values = item.get("values").and_then(Value::as_array).ok_or(LokiError::InvalidResponse)?;
        for point in values {
            let (Some(seconds), Some(value)) = (
                point.get(0).and_then(Value::as_f64),
                point.get(1).and_then(Value::as_str),
            ) else {
                return Err(LokiError::InvalidResponse);
            };
            let millis = (seconds * 1_000.0).round() as i64;
            let time = DateTime::<Utc>::from_timestamp_millis(millis)
                .ok_or(LokiError::InvalidResponse)?
                .to_rfc3339_opts(SecondsFormat::Millis, true);
            let mut source = labels.clone();
            source.insert("_time".to_owned(), json!(time));
            source.insert(
                "_value".to_owned(),
                value
                    .parse::<f64>()
                    .ok()
                    .filter(|number| number.is_finite())
                    .map_or_else(|| json!(value), |number| json!(number)),
            );
            let source = Value::Object(source);
            let mut hasher = std::collections::hash_map::DefaultHasher::new();
            source.to_string().hash(&mut hasher);
            entries.push(LogEntry {
                id: format!("sample-{:016x}-{}", hasher.finish(), entries.len()),
                timestamp: millis,
                source,
            });
            if entries.len() > limit {
                return Err(LokiError::TooManyEntries);
            }
        }
    }
    Ok(entries)
}

fn parse_streams(streams: &[Value], limit: usize) -> Result<Vec<LogEntry>, LokiError> {
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
            if entries.len() > limit {
                return Err(LokiError::InvalidResponse);
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
    let (interval, millis, mut buckets) = histogram_grid(range, options)?;
    for entry in entries {
        let timestamp = entry.timestamp.div_euclid(millis) * millis;
        let key = options.breakdown_field.as_deref().and_then(|field| {
            let value = entry.source.get(field).filter(|value| !value.is_null())?;
            Some(
                value
                    .as_str()
                    .map(str::to_owned)
                    .unwrap_or_else(|| value.to_string()),
            )
        });
        buckets.entry(timestamp).or_default().add(key.as_deref(), 1);
    }
    Ok(finish_histogram(
        buckets,
        interval,
        options.breakdown_field.is_some(),
    ))
}

/// Empty epoch-aligned buckets covering `range`.
fn histogram_grid(
    range: TimeRange,
    options: &HistogramOptions,
) -> Result<(HistogramInterval, i64, BTreeMap<i64, BucketCounts>), LokiError> {
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
    Ok((interval, millis, buckets))
}

/// Keeps the top breakdown values across all buckets and folds the rest into the other key.
fn finish_histogram(
    buckets: BTreeMap<i64, BucketCounts>,
    interval: HistogramInterval,
    breakdown: bool,
) -> (Vec<HistogramBucket>, String) {
    let mut key_totals = HashMap::<&str, u64>::new();
    for bucket in buckets.values() {
        for (key, count) in &bucket.breakdown {
            *key_totals.entry(key).or_default() += count;
        }
    }
    let mut top_keys = key_totals.into_iter().collect::<Vec<_>>();
    top_keys.sort_by(|left, right| right.1.cmp(&left.1).then_with(|| left.0.cmp(right.0)));
    top_keys.truncate(MAX_BREAKDOWN_VALUES);
    let histogram = buckets
        .iter()
        .map(|(timestamp, bucket)| {
            let breakdown = breakdown.then(|| {
                let mut values = top_keys
                    .iter()
                    .map(|(key, _)| BreakdownCount {
                        key: (*key).to_owned(),
                        count: bucket.breakdown.get(*key).copied().unwrap_or(0),
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
                timestamp: *timestamp,
                label: None,
                count: bucket.count,
                breakdown,
            }
        })
        .collect();
    (histogram, format!("{}{}", interval.count, interval.suffix()))
}

/// `sum(count_over_time(...))` over `window` milliseconds; the newline ends any trailing comment.
fn count_expression(query: &str, window: i64, breakdown: Option<&str>) -> String {
    let counted = format!("count_over_time(({query}\n)[{window}ms])");
    match breakdown {
        Some(label) => format!("sum by ({label}) ({counted})"),
        None => format!("sum({counted})"),
    }
}

/// The breakdown value of a metric series; lines without the label count only toward totals.
fn series_key(series: &Value, breakdown: Option<&str>) -> Option<String> {
    let label = breakdown?;
    series
        .get("metric")?
        .get(label)?
        .as_str()
        .filter(|value| !value.is_empty())
        .map(str::to_owned)
}

/// A `[seconds, "count"]` sample as epoch milliseconds and a line count.
fn count_point(point: &Value) -> Result<(i64, u64), LokiError> {
    let seconds = point
        .get(0)
        .and_then(Value::as_f64)
        .filter(|value| value.is_finite())
        .ok_or(LokiError::InvalidResponse)?;
    let count = point
        .get(1)
        .and_then(Value::as_str)
        .and_then(|value| value.parse::<f64>().ok())
        .filter(|value| value.is_finite() && *value >= 0.0)
        .ok_or(LokiError::InvalidResponse)?;
    Ok(((seconds * 1000.0).round() as i64, count.round() as u64))
}

/// Loki label names, as accepted by stream selectors and `by` clauses.
pub(super) fn valid_label_name(name: &str) -> bool {
    let mut bytes = name.bytes();
    bytes
        .next()
        .is_some_and(|first| first.is_ascii_alphabetic() || first == b'_')
        && bytes.all(|byte| byte.is_ascii_alphanumeric() || byte == b'_')
}

fn validate_success(response: &Value) -> Result<(), LokiError> {
    if response.get("status").and_then(Value::as_str) == Some("success") {
        Ok(())
    } else {
        Err(LokiError::InvalidResponse)
    }
}

async fn read_bounded(response: Response) -> Result<Vec<u8>, LokiError> {
    read_limited(response, MAX_RESPONSE_BYTES).await
}

async fn read_limited(mut response: Response, limit: usize) -> Result<Vec<u8>, LokiError> {
    let status = response.status();
    if !status.is_success() {
        // Only the query-length limit is read from an error; upstream text is never returned.
        let mut body = Vec::new();
        while body.len() < MAX_ERROR_BYTES {
            match response.chunk().await {
                Ok(Some(chunk)) => body.extend_from_slice(&chunk),
                _ => break,
            }
        }
        if status.as_u16() == 400 {
            if let Some(limit_millis) = rejected_query_length(&String::from_utf8_lossy(&body)) {
                return Err(LokiError::QueryTooLong { limit_millis });
            }
        }
        return Err(LokiError::Rejected {
            status: status.as_u16(),
        });
    }
    if response
        .content_length()
        .is_some_and(|length| length > limit as u64)
    {
        return Err(LokiError::ResponseTooLarge);
    }
    let mut body = Vec::new();
    while let Some(chunk) = response.chunk().await.map_err(|_| LokiError::Request)? {
        if body.len().saturating_add(chunk.len()) > limit {
            return Err(LokiError::ResponseTooLarge);
        }
        body.extend_from_slice(&chunk);
    }
    Ok(body)
}

/// Narrows a search to the newest window Loki accepts. Returns the range to query and whether
/// it was narrowed; All time becomes a bounded window when the limit is known.
fn within_query_length(
    requested: Option<TimeRange>,
    max_query_length: Option<i64>,
) -> (Option<TimeRange>, bool) {
    let Some(limit) = max_query_length.filter(|limit| *limit > 0) else {
        return (requested, false);
    };
    let range = requested.unwrap_or(TimeRange {
        start: 0,
        end: Utc::now().timestamp_millis(),
    });
    // Loki measures the half-open nanosecond range that `time_bounds` sends: end + 1 ms - start.
    if range.end.saturating_add(1).saturating_sub(range.start) <= limit {
        return (requested, false);
    }
    (
        Some(TimeRange {
            start: range.end.saturating_add(1).saturating_sub(limit),
            end: range.end,
        }),
        true,
    )
}

/// `limits_config.max_query_length` from Loki's `/config` YAML, in milliseconds.
fn configured_max_query_length(config: &str) -> Option<i64> {
    let mut in_limits = false;
    for line in config.lines() {
        if !line.starts_with(' ') && !line.is_empty() {
            in_limits = line.trim_end() == "limits_config:";
            continue;
        }
        if in_limits {
            if let Some(value) = line.strip_prefix("  max_query_length:") {
                return parse_duration(value.trim());
            }
        }
    }
    None
}

/// The limit from Loki's rejection: "the query time range exceeds the limit (query length: ..., limit: 30d1h)".
fn rejected_query_length(error: &str) -> Option<i64> {
    let (_, rest) = error.split_once("the query time range exceeds the limit")?;
    let (_, limit) = rest.split_once("limit: ")?;
    parse_duration(limit.split(')').next()?.trim())
}

/// A Prometheus-style duration such as `30d1h`, `721h`, or `0s`, in milliseconds.
fn parse_duration(text: &str) -> Option<i64> {
    if text == "0" {
        return Some(0);
    }
    let mut rest = text;
    let mut total = 0i64;
    while !rest.is_empty() {
        let digits = rest.find(|character: char| !character.is_ascii_digit())?;
        let amount = rest[..digits].parse::<i64>().ok()?;
        rest = &rest[digits..];
        let (unit, millis) = [
            ("ms", 1),
            ("y", 365 * 86_400_000),
            ("w", 7 * 86_400_000),
            ("d", 86_400_000),
            ("h", 3_600_000),
            ("m", 60_000),
            ("s", 1_000),
        ]
        .into_iter()
        .find(|(unit, _)| rest.starts_with(unit))?;
        rest = &rest[unit.len()..];
        total = total.checked_add(amount.checked_mul(millis)?)?;
    }
    (!text.is_empty()).then_some(total)
}

/// How a Loki field is stored, as encoded in its `FieldSummary::field_type`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum FieldKind {
    Time,
    Line,
    Label,
    /// A field parsed from the line, or structured metadata when there is no parser.
    Parsed(Option<Parser>, ValueType),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Parser {
    Json,
    Logfmt,
}

/// The value types Loki's `detected_fields` reports.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ValueType {
    String,
    Int,
    Float,
    Duration,
    Bytes,
    Boolean,
}

impl FieldKind {
    fn parse(field_type: &str) -> Option<Self> {
        Some(match field_type {
            "timestamp" => Self::Time,
            "text" => Self::Line,
            "label" => Self::Label,
            other => {
                let (source, value_type) = other.split_once(' ')?;
                let parser = match source {
                    "json" => Some(Parser::Json),
                    "logfmt" => Some(Parser::Logfmt),
                    "metadata" => None,
                    _ => return None,
                };
                Self::Parsed(parser, ValueType::parse(value_type)?)
            }
        })
    }
}

impl ValueType {
    fn parse(value_type: &str) -> Option<Self> {
        Some(match value_type {
            "string" => Self::String,
            "int" => Self::Int,
            "float" => Self::Float,
            "duration" => Self::Duration,
            "bytes" => Self::Bytes,
            "boolean" => Self::Boolean,
            _ => return None,
        })
    }
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
        limited_client(url, None)
    }

    fn limited_client(url: &str, max_query_length: Option<i64>) -> LokiClient {
        LokiClient::new(LokiSettings {
            url: Url::parse(url).unwrap(),
            auth: HttpAuth::None,
            tenant_id: None,
            max_query_length,
        })
        .unwrap()
    }

    fn labels(data: Value) -> ResponseTemplate {
        ResponseTemplate::new(200).set_body_json(json!({"status":"success","data":data}))
    }

    #[test]
    fn query_length_limits_are_read_from_config_and_rejections() {
        let config = "server:\n  max_query_length: 1h\nlimits_config:\n  ingestion_rate_mb: 4\n  max_query_length: 30d1h\n  max_query_series: 500\nquerier:\n  max_query_length: 2h\n";
        let thirty_days_one_hour = 30 * 86_400_000 + 3_600_000;
        assert_eq!(configured_max_query_length(config), Some(thirty_days_one_hour));
        assert_eq!(configured_max_query_length("limits_config:\n  max_query_length: 0s\n"), Some(0));
        assert_eq!(configured_max_query_length("limits_config:\n  max_query_series: 1\n"), None);
        assert_eq!(
            rejected_query_length(
                "the query time range exceeds the limit (query length: 960h0m0s, limit: 30d1h)"
            ),
            Some(thirty_days_one_hour)
        );
        assert_eq!(rejected_query_length("parse error"), None);
        for (text, millis) in [
            ("721h", 721 * 3_600_000),
            ("1h30m", 5_400_000),
            ("90s", 90_000),
            ("250ms", 250),
            ("1w", 604_800_000),
        ] {
            assert_eq!(parse_duration(text), Some(millis), "{text}");
        }
        for text in ["", "h", "1.5h", "1x", "-1h"] {
            assert_eq!(parse_duration(text), None, "{text}");
        }
        let range = TimeRange { start: 0, end: 9_999 };
        assert_eq!(within_query_length(Some(range), None), (Some(range), false));
        assert_eq!(within_query_length(Some(range), Some(10_000)), (Some(range), false));
        assert_eq!(
            within_query_length(Some(range), Some(1_000)),
            (Some(TimeRange { start: 9_000, end: 9_999 }), true)
        );
        let (all_time, clamped) = within_query_length(None, Some(1_000));
        assert!(clamped && all_time.is_some_and(|range| range.end + 1 - range.start == 1_000));
    }

    #[tokio::test]
    async fn connection_tests_learn_max_query_length_when_config_is_readable() {
        let server = MockServer::start().await;
        Mock::given(matchers::path("/loki/api/v1/labels"))
            .respond_with(labels(json!([])))
            .mount(&server)
            .await;
        Mock::given(matchers::path("/config"))
            .respond_with(
                ResponseTemplate::new(200).set_body_string("limits_config:\n  max_query_length: 2d\n"),
            )
            .expect(1)
            .mount(&server)
            .await;
        let loki = client(&server.uri());
        loki.test_connection().await.unwrap();
        assert_eq!(
            Value::Object(Connector::learned_configuration(&loki)),
            json!({"max_query_length_ms":172_800_000})
        );

        let blocked = MockServer::start().await;
        Mock::given(matchers::path("/loki/api/v1/labels"))
            .respond_with(labels(json!([])))
            .mount(&blocked)
            .await;
        Mock::given(matchers::path("/config"))
            .respond_with(ResponseTemplate::new(403))
            .mount(&blocked)
            .await;
        let loki = client(&blocked.uri());
        loki.test_connection().await.unwrap();
        assert!(Connector::learned_configuration(&loki).is_empty());
    }

    #[tokio::test]
    async fn searches_stay_within_a_known_or_rejected_max_query_length() {
        let server = MockServer::start().await;
        // A one-second limit narrows the three-second range to its newest second.
        Mock::given(matchers::path("/loki/api/v1/query_range"))
            .and(matchers::query_param("start", "86402000000000"))
            .and(matchers::query_param("end", "86403000000000"))
            .respond_with(streams(vec![json!(["86402500000000", "newest"])]))
            .expect(2)
            .mount(&server)
            .await;
        let request =
            || saturated_request(json!({"histogram_field":"_time","histogram_interval":"1s"}));
        let result = Connector::search(&limited_client(&server.uri(), Some(1_000)), request())
            .await
            .unwrap();
        assert_eq!(result["total"], 1);
        assert_eq!(result["total_relation"], "gte");
        assert_eq!(result["histogram"].as_array().unwrap().len(), 1);

        // Without a stored limit, Loki's rejection states it and the search retries once within it.
        Mock::given(matchers::path("/loki/api/v1/query_range"))
            .and(matchers::query_param("start", "86400000000000"))
            .respond_with(ResponseTemplate::new(400).set_body_string(
                "the query time range exceeds the limit (query length: 3s, limit: 1s)",
            ))
            .expect(1)
            .mount(&server)
            .await;
        let result = Connector::search(&client(&server.uri()), request()).await.unwrap();
        assert_eq!(result["hits"][0]["source"]["_msg"], "newest");
        assert_eq!(result["total_relation"], "gte");
    }

    #[tokio::test]
    async fn fields_include_detected_parsed_fields_and_tolerate_older_loki() {
        let server = MockServer::start().await;
        Mock::given(matchers::path("/loki/api/v1/labels"))
            .respond_with(labels(json!(["app", "level"])))
            .mount(&server)
            .await;
        Mock::given(matchers::path("/loki/api/v1/detected_fields"))
            .and(matchers::query_param("query", "{service_name=~\".+\"}"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({"fields":[
                {"label":"status","type":"int","parsers":["logfmt","json"]},
                {"label":"duration","type":"duration","parsers":["logfmt"]},
                {"label":"trace_id","type":"string","parsers":null},
                {"label":"level","type":"string","parsers":["logfmt"]},
                {"label":"odd","type":"geo","parsers":["json"]},
                {"label":"bad.name","type":"string","parsers":["json"]}
            ]})))
            .mount(&server)
            .await;
        let fields = client(&server.uri()).fields("logs").await.unwrap();
        let fields = fields
            .iter()
            .map(|field| (field.name.as_str(), field.field_type.as_str()))
            .collect::<Vec<_>>();
        assert_eq!(
            fields,
            [
                ("_msg", "text"),
                ("_time", "timestamp"),
                ("app", "label"),
                ("duration", "logfmt duration"),
                ("level", "label"),
                ("status", "json int"),
                ("trace_id", "metadata string"),
            ]
        );

        let older = MockServer::start().await;
        Mock::given(matchers::path("/loki/api/v1/labels"))
            .respond_with(labels(json!(["app"])))
            .mount(&older)
            .await;
        assert_eq!(client(&older.uri()).fields("logs").await.unwrap().len(), 3);
    }

    #[tokio::test]
    async fn oauth_tokens_authorize_every_loki_request_without_exposing_secrets() {
        let server = MockServer::start().await;
        Mock::given(matchers::method("POST"))
            .and(matchers::path("/token"))
            .respond_with(ResponseTemplate::new(200).set_body_json(
                json!({"access_token":"loki-token","token_type":"Bearer","expires_in":300}),
            ))
            .expect(1)
            .mount(&server)
            .await;
        Mock::given(matchers::path("/loki/api/v1/labels"))
            .and(matchers::header("authorization", "Bearer loki-token"))
            .and(matchers::header("x-scope-orgid", "team-a"))
            .respond_with(labels(json!(["app"])))
            .expect(2)
            .mount(&server)
            .await;
        let config = json!({
            "url":server.uri(),"auth_type":"oauth2","oauth_token_url":format!("{}/token", server.uri()),
            "oauth_client_id":"loki-reader","oauth_client_secret":"loki-secret","tenant_id":"team-a"
        });
        let connector = connector::connect(config.as_object().unwrap()).unwrap();
        connector.test_connection().await.unwrap();
        connector.fields("logs").await.unwrap();
        let public = Value::Object(connector.public_configuration()).to_string();
        assert!(public.contains("oauth2") && public.contains("/token") && public.contains("team-a"));
        assert!(!public.contains("loki-secret") && !public.contains("loki-reader"));
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
        let entries = parse_streams(streams.as_array().unwrap(), 10).unwrap();
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

    fn streams(values: Vec<Value>) -> ResponseTemplate {
        ResponseTemplate::new(200).set_body_json(json!({
            "status":"success","data":{"resultType":"streams","result":[{
                "stream":{"app":"api","level":"error"},"values":values
            }]}
        }))
    }

    fn vector(series: Value) -> ResponseTemplate {
        ResponseTemplate::new(200)
            .set_body_json(json!({"status":"success","data":{"resultType":"vector","result":series}}))
    }

    /// 5,000 entries inside 86,400,000..=86,402,999 ms, newest first, as a backward query returns them.
    fn saturated_entries() -> Vec<Value> {
        (0..MAX_RETURNED_ENTRIES as i64)
            .rev()
            .map(|index| json!([((86_400_000 + index % 3_000) * NANOS_PER_MILLI).to_string(), format!("line {index}")]))
            .collect()
    }

    fn saturated_request(extra: Value) -> crate::search::SearchRequest {
        let mut request = json!({
            "index":"logs","query":"{app=\"api\"}","size":2,"cursor":"",
            "time_range":{"start":86_400_000i64,"end":86_402_999i64}
        });
        request
            .as_object_mut()
            .unwrap()
            .extend(extra.as_object().unwrap().clone());
        serde_json::from_value(request).unwrap()
    }

    async fn mount_backward(server: &MockServer) {
        Mock::given(matchers::path("/loki/api/v1/query_range"))
            .and(matchers::query_param("direction", "backward"))
            .respond_with(streams(saturated_entries()))
            .expect(1)
            .mount(server)
            .await;
    }

    #[tokio::test]
    async fn empty_selectors_resolve_to_every_service_or_explain_the_requirement() {
        let server = MockServer::start().await;
        Mock::given(matchers::path("/loki/api/v1/labels"))
            .and(matchers::query_param("start", "86400000000000"))
            .respond_with(ResponseTemplate::new(200).set_body_json(
                json!({"status":"success","data":["app","service_name"]}),
            ))
            .expect(1)
            .mount(&server)
            .await;
        Mock::given(matchers::path("/loki/api/v1/query_range"))
            .and(matchers::query_param(
                "query",
                "{service_name=~\".+\"} |= \"timeout\"",
            ))
            .respond_with(streams(vec![json!(["86400000000000", "timeout"])]))
            .expect(1)
            .mount(&server)
            .await;
        let request = saturated_request(json!({"query":"{ } |= \"timeout\""}));
        let result = Connector::search(&client(&server.uri()), request).await.unwrap();
        assert_eq!(result["total"], 1);

        let server = MockServer::start().await;
        Mock::given(matchers::path("/loki/api/v1/labels"))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_json(json!({"status":"success","data":["app"]})),
            )
            .mount(&server)
            .await;
        let error = Connector::search(&client(&server.uri()), saturated_request(json!({"query":""})))
            .await
            .unwrap_err();
        assert_eq!(error.code, "loki_selector_required");
    }

    #[tokio::test]
    async fn results_above_the_entry_cap_are_counted_exactly_on_the_server() {
        let server = MockServer::start().await;
        mount_backward(&server).await;
        let last_nanos = "86402999999999";
        Mock::given(matchers::path("/loki/api/v1/query"))
            .and(matchers::query_param(
                "query",
                "sum by (level) (count_over_time(({app=\"api\"}\n)[3000ms]))",
            ))
            .and(matchers::query_param("time", last_nanos))
            .respond_with(vector(json!([
                {"metric":{"level":"error"},"value":[86402.999,"7000"]},
                {"metric":{"level":"info"},"value":[86402.999,"5000"]},
                {"metric":{},"value":[86402.999,"500"]}
            ])))
            .expect(1)
            .mount(&server)
            .await;
        Mock::given(matchers::path("/loki/api/v1/query"))
            .and(matchers::query_param(
                "query",
                "sum by (level) (count_over_time(({app=\"api\"}\n)[1000ms]))",
            ))
            .and(matchers::query_param("time", last_nanos))
            .respond_with(vector(json!([
                {"metric":{"level":"error"},"value":[86402.999,"2000"]},
                {"metric":{"level":"info"},"value":[86402.999,"1000"]}
            ])))
            .expect(1)
            .mount(&server)
            .await;
        Mock::given(matchers::path("/loki/api/v1/query_range"))
            .and(matchers::query_param("step", "1000ms"))
            .and(matchers::query_param("start", "86402000000000"))
            .and(matchers::query_param("end", "86402000000000"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "status":"success","data":{"resultType":"matrix","result":[
                    {"metric":{"level":"error"},"values":[[86402,"3000"]]},
                    {"metric":{"level":"info"},"values":[[86402,"1000"]]}
                ]}
            })))
            .expect(1)
            .mount(&server)
            .await;
        let request = saturated_request(json!({
            "histogram_field":"_time","histogram_interval":"1s","histogram_breakdown_field":"level"
        }));
        let result = Connector::search(&client(&server.uri()), request).await.unwrap();
        assert_eq!(result["total"], 12_500);
        assert_eq!(result["total_relation"], "eq");
        assert!(result["histogram_unavailable_reason"].is_null());
        assert_eq!(result["hits"][0]["source"]["_msg"], "line 2999");
        assert_eq!(result["next_cursor"], "2");
        let histogram = result["histogram"].as_array().unwrap();
        let counts = histogram
            .iter()
            .map(|bucket| bucket["count"].as_u64().unwrap())
            .collect::<Vec<_>>();
        assert_eq!(counts, [5_500, 4_000, 3_000]);
        assert_eq!(
            histogram[0]["breakdown"],
            json!([{"key":"error","count":2000},{"key":"info","count":3000},{"key":BREAKDOWN_OTHER_KEY,"count":500}])
        );
    }

    #[tokio::test]
    async fn uncountable_large_results_keep_rows_with_a_lower_bound() {
        let server = MockServer::start().await;
        mount_backward(&server).await;
        Mock::given(matchers::path("/loki/api/v1/query"))
            .respond_with(ResponseTemplate::new(400))
            .expect(1)
            .mount(&server)
            .await;
        let request = saturated_request(json!({"histogram_field":"_time","histogram_interval":"1s"}));
        let result = Connector::search(&client(&server.uri()), request).await.unwrap();
        assert_eq!(result["total"], MAX_RETURNED_ENTRIES);
        assert_eq!(result["total_relation"], "gte");
        assert_eq!(result["hits"].as_array().unwrap().len(), 2);
        assert!(result["histogram_unavailable_reason"]
            .as_str()
            .unwrap()
            .contains("lower bound"));

        let server = MockServer::start().await;
        mount_backward(&server).await;
        let request = saturated_request(json!({
            "histogram_field":"_time","histogram_interval":"1s","histogram_breakdown_field":"_msg"
        }));
        let result = Connector::search(&client(&server.uri()), request).await.unwrap();
        assert_eq!(result["total_relation"], "gte");
        assert!(result["histogram_unavailable_reason"]
            .as_str()
            .unwrap()
            .contains("stream label"));
    }

    #[tokio::test]
    async fn large_ascending_pages_fetch_the_oldest_entries_and_other_sorts_stay_bounded() {
        let server = MockServer::start().await;
        mount_backward(&server).await;
        Mock::given(matchers::path("/loki/api/v1/query_range"))
            .and(matchers::query_param("direction", "forward"))
            .and(matchers::query_param("limit", "2"))
            .respond_with(streams(vec![
                json!(["86400000000000", "oldest"]),
                json!(["86400001000000", "next"]),
            ]))
            .expect(1)
            .mount(&server)
            .await;
        Mock::given(matchers::path("/loki/api/v1/query"))
            .and(matchers::query_param(
                "query",
                "sum(count_over_time(({app=\"api\"}\n)[3000ms]))",
            ))
            .respond_with(vector(json!([{"metric":{},"value":[86402.999,"9000"]}])))
            .expect(1)
            .mount(&server)
            .await;
        let request = saturated_request(json!({
            "sort_by":"_time","sort_order":"asc","include_histogram":false
        }));
        let result = Connector::search(&client(&server.uri()), request).await.unwrap();
        assert_eq!(result["total"], 9_000);
        assert_eq!(result["hits"][0]["source"]["_msg"], "oldest");

        let server = MockServer::start().await;
        mount_backward(&server).await;
        let request = saturated_request(json!({"sort_by":"level","sort_order":"asc"}));
        let error = Connector::search(&client(&server.uri()), request)
            .await
            .unwrap_err();
        assert_eq!(error.code, "loki_result_limit_exceeded");
    }

    /// Runs the connector and the LogQL dialect against a real Loki 3, e.g. the playground's.
    #[tokio::test]
    #[ignore = "requires TEST_LOKI_URL pointing at a disposable Loki 3"]
    async fn live_loki_counts_large_results_exactly_and_runs_translated_filters() {
        use crate::investigation_dialect::{build_query, FilterOperator, FilterPredicate, FilterValue};

        // CI runs ignored tests with --include-ignored; without a server this test has nothing to check.
        let Ok(url) = std::env::var("TEST_LOKI_URL") else {
            eprintln!("skipped: TEST_LOKI_URL is not set");
            return;
        };
        let run = format!("r{}", Utc::now().timestamp_nanos_opt().unwrap());
        // Millisecond-precision lines put one entry on every bucket boundary.
        let base = (Utc::now().timestamp_millis() / 1_000 - 3_600) * 1_000;
        let values = (0..6_000i64)
            .map(|index| {
                json!([((base + index) * NANOS_PER_MILLI).to_string(), format!("run {run} line {index}")])
            })
            .collect::<Vec<_>>();
        let streams = [0, 1, 2].map(|level| {
            json!({
                "stream":{"app":"ig_live","run":run,"level":if level == 0 { "error" } else { "info" }},
                "values":values.iter().enumerate().filter(|(index, _)| index % 3 == level).map(|(_, value)| value).collect::<Vec<_>>()
            })
        });
        Client::new()
            .post(format!("{url}/loki/api/v1/push"))
            .json(&json!({"streams":streams}))
            .send()
            .await
            .unwrap()
            .error_for_status()
            .unwrap();
        let client = client(&url);
        let (start, end) = (base + 250, base + 5_749);
        let search = |query: String, extra: Value| {
            let mut request = json!({
                "index":"logs","query":query,"size":2,"cursor":"",
                "time_range":{"start":start,"end":end}
            });
            request.as_object_mut().unwrap().extend(extra.as_object().unwrap().clone());
            serde_json::from_value::<crate::search::SearchRequest>(request).unwrap()
        };
        let selector = format!("{{run=\"{run}\"}}");
        let mut result = Value::Null;
        for _ in 0..40 {
            result = client
                .search(search(selector.clone(), json!({"histogram_field":"_time","histogram_interval":"1s","histogram_breakdown_field":"level"})))
                .await
                .unwrap();
            if result["total"] == 5_500 {
                break;
            }
            tokio::time::sleep(Duration::from_millis(250)).await;
        }
        assert_eq!(result["total"], 5_500, "{result}");
        assert_eq!(result["total_relation"], "eq");
        assert_eq!(result["hits"][0]["source"]["_msg"], format!("run {run} line 5749"));
        let histogram = result["histogram"].as_array().unwrap();
        let counts = histogram
            .iter()
            .map(|bucket| bucket["count"].as_i64().unwrap())
            .collect::<Vec<_>>();
        assert_eq!(counts.iter().sum::<i64>(), 5_500);
        for (count, expected) in counts.iter().zip([750, 1_000, 1_000, 1_000, 1_000, 750]) {
            assert!((count - expected).abs() <= 1, "{counts:?}");
        }
        let errors = histogram
            .iter()
            .flat_map(|bucket| bucket["breakdown"].as_array().unwrap())
            .filter(|value| value["key"] == "error")
            .map(|value| value["count"].as_u64().unwrap())
            .sum::<u64>();
        assert_eq!(errors, (250..=5_749).filter(|index| index % 3 == 0).count() as u64);

        let oldest = client
            .search(search(selector.clone(), json!({"sort_by":"_time","sort_order":"asc","include_histogram":false})))
            .await
            .unwrap();
        assert_eq!(oldest["hits"][0]["source"]["_msg"], format!("run {run} line 250"));
        assert_eq!(oldest["total"], 5_500);

        let filters = [
            FilterPredicate { field: "run".to_owned(), field_type: "label".to_owned(), operator: FilterOperator::Contains, value: FilterValue::String(run[1..].to_owned()) },
            FilterPredicate { field: "level".to_owned(), field_type: "label".to_owned(), operator: FilterOperator::Eq, value: FilterValue::String("error".to_owned()) },
            FilterPredicate { field: "_msg".to_owned(), field_type: "text".to_owned(), operator: FilterOperator::Contains, value: FilterValue::String("line 57".to_owned()) },
        ];
        let translated = build_query(investigation::dialect(), "logs", &filters).unwrap();
        let filtered = client.search(search(translated, json!({"size":50}))).await.unwrap();
        let expected = (250..=5_749)
            .filter(|index| index % 3 == 0 && format!("line {index}").contains("line 57"))
            .count();
        assert_eq!(filtered["total"], expected as u64, "{filtered}");
        assert!(filtered["hits"].as_array().unwrap().iter().all(|hit| hit["source"]["level"] == "error"));

        let message = format!("run {run} line 5749");
        let resolved = client
            .search(search(format!("{{}} |= {}", Value::from(message.as_str())), json!({})))
            .await
            .unwrap();
        assert_eq!(resolved["total"], 1);
    }

    /// Parsed JSON fields, numeric comparisons, and max_query_length on a real Loki 3.
    #[tokio::test]
    #[ignore = "requires TEST_LOKI_URL pointing at a disposable Loki 3"]
    async fn live_loki_filters_parsed_fields_and_respects_max_query_length() {
        use crate::investigation_dialect::{build_query, FilterOperator, FilterPredicate, FilterValue};

        // CI runs ignored tests with --include-ignored; without a server this test has nothing to check.
        let Ok(url) = std::env::var("TEST_LOKI_URL") else {
            eprintln!("skipped: TEST_LOKI_URL is not set");
            return;
        };
        let run = format!("p{}", Utc::now().timestamp_nanos_opt().unwrap());
        let base = (Utc::now().timestamp_millis() - 120_000) * NANOS_PER_MILLI;
        let lines = [
            json!({"status":200,"latency":0.12,"msg":"ok"}),
            json!({"status":503,"latency":2.5,"msg":"upstream timeout"}),
            json!({"status":"oops","msg":"unparseable status"}),
            json!({"msg":"no status"}),
            json!({"status":504,"latency":0.4,"msg":"gateway timeout"}),
        ];
        let values = lines
            .iter()
            .enumerate()
            .map(|(index, line)| json!([(base + index as i64 * 1_000_000_000).to_string(), line.to_string()]))
            .collect::<Vec<_>>();
        Client::new()
            .post(format!("{url}/loki/api/v1/push"))
            .json(&json!({"streams":[{"stream":{"app":"ig_parsed","run":run},"values":values}]}))
            .send()
            .await
            .unwrap()
            .error_for_status()
            .unwrap();

        let connector = connector::connect(json!({"url":url}).as_object().unwrap()).unwrap();
        connector.test_connection().await.unwrap();
        let learned = connector.learned_configuration();
        let limit = learned[MAX_QUERY_LENGTH_SETTING].as_i64().unwrap();
        assert!(limit > 0, "{learned:?}");

        let mut status_type = None;
        for _ in 0..40 {
            let fields = client(&url).fields("logs").await.unwrap();
            status_type = fields
                .iter()
                .find(|field| field.name == "status")
                .map(|field| field.field_type.clone());
            if status_type.is_some() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(250)).await;
        }
        assert_eq!(status_type.as_deref(), Some("json int"));

        let predicate = |field: &str, field_type: &str, operator, value| FilterPredicate {
            field: field.to_owned(),
            field_type: field_type.to_owned(),
            operator,
            value,
        };
        let query = build_query(
            investigation::dialect(),
            "logs",
            &[
                predicate("run", "label", FilterOperator::Eq, FilterValue::String(run.clone())),
                predicate("status", "json int", FilterOperator::Gte, FilterValue::Number(500.into())),
                predicate("latency", "json float", FilterOperator::Gt, FilterValue::String("1".to_owned())),
            ],
        )
        .unwrap();
        let range = json!({"start":base / NANOS_PER_MILLI - 1_000,"end":base / NANOS_PER_MILLI + 10_000});
        let request = |query: &str, range: Value| {
            serde_json::from_value::<crate::search::SearchRequest>(
                json!({"index":"logs","query":query,"size":50,"time_range":range}),
            )
            .unwrap()
        };
        let matched = client(&url).search(request(&query, range.clone())).await.unwrap();
        let messages = matched["hits"]
            .as_array()
            .unwrap()
            .iter()
            .map(|hit| hit["source"]["msg"].as_str().unwrap_or_default().to_owned())
            .collect::<Vec<_>>();
        assert_eq!(messages, ["upstream timeout"], "{query}");

        // All time exceeds Loki's limit: a stored limit narrows it up front, and without one
        // Loki's rejection supplies the limit for a single retry.
        let selector = format!("{{run=\"{run}\"}}");
        for loki in [limited_client(&url, Some(limit)), client(&url)] {
            let all_time = loki.search(request(&selector, Value::Null)).await.unwrap();
            assert_eq!(all_time["total"], 5);
            assert_eq!(all_time["total_relation"], "gte");
        }
    }
}
