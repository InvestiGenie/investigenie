use std::{
    cmp::Ordering,
    collections::{BTreeMap, HashMap},
    hash::{Hash, Hasher},
    time::{Duration, Instant},
};

use crate::data_connectors::IntegrationDescriptor;
use crate::search::{
    BreakdownCount, HistogramInterval, HistogramOptions, IntervalUnit, TimeRange,
    BREAKDOWN_OTHER_KEY, MAX_BREAKDOWN_VALUES,
};
use chrono::{Datelike, NaiveDate, NaiveTime, TimeZone, Utc};
use reqwest::{redirect::Policy, Client, Response, Url};
use serde_json::{json, Map, Value};
use thiserror::Error;

mod connector;
pub use connector::connect;

const REQUEST_TIMEOUT: Duration = Duration::from_secs(15);
const QUERY_TIMEOUT: &str = "10s";
const MAX_QUERY_BYTES: usize = 8192;
const MAX_RESPONSE_BYTES: usize = 16 * 1024 * 1024;
const MAX_SAMPLES: usize = 50_000;
const MAX_PAGE_SIZE: usize = 100;
const MAX_GRID_CELLS: i64 = 10_000;
const DAY_MS: i64 = 86_400_000;
const MIN_TIMESTAMP_MS: i64 = 86_400_000;

pub fn descriptor() -> IntegrationDescriptor {
    IntegrationDescriptor {
        id: "victoriametrics",
        display_name: "VictoriaMetrics",
        icon: "victoriametrics",
        query_language: "MetricsQL selector",
        description: "Read-only raw metric samples from VictoriaMetrics",
        connection_fields: vec![
            json!({"name":"url","label":"URL","input_type":"url","required":true,"secret":false,"editable":true,"placeholder":"http://localhost:8428"}),
            json!({"name":"username","label":"Username","input_type":"text","required":false,"secret":true,"editable":true,"group":"credentials","placeholder":""}),
            json!({"name":"password","label":"Password","input_type":"password","required":false,"secret":true,"editable":true,"group":"credentials","placeholder":""}),
        ],
        query: json!({"placeholder":"Enter a metric selector, e.g. http_requests_total{job=\"api\"}","default_query":"{__name__!=\"\"}","field_query":"{query}","identifier_quote":"","help":"Enter a read-only MetricsQL series selector; Explore displays raw samples, not evaluated range-query values","document_mode":"inline","sort_requires_projection":false,"histogram_total_from_buckets":false,"schema_label":"VictoriaMetrics labels"}),
        ..Default::default()
    }
}

#[derive(Clone)]
pub struct VictoriaMetricsSettings {
    pub url: Url,
    pub username: Option<String>,
    pub password: Option<String>,
}

#[derive(Clone)]
pub struct VictoriaMetricsClient {
    base_url: Url,
    client: Client,
    username: Option<String>,
    password: Option<String>,
}

#[derive(Debug, Error)]
pub enum VictoriaMetricsError {
    #[error("VictoriaMetrics URL must be an absolute HTTP or HTTPS URL without embedded credentials, query, or fragment")]
    InvalidUrl,
    #[error("VictoriaMetrics connection settings are invalid")]
    InvalidConfiguration,
    #[error("VictoriaMetrics query or search parameters are invalid or exceed configured limits")]
    InvalidQuery,
    #[error("VictoriaMetrics search exceeded the connector's bounded sample limit")]
    TooManySamples,
    #[error("Sorting by a label or value needs every sample, which exceeds the connector limit")]
    SortTooLarge,
    #[error("VictoriaMetrics response exceeded the configured size limit")]
    ResponseTooLarge,
    #[error("Could not configure the VictoriaMetrics HTTP client")]
    ClientConfiguration,
    #[error("VictoriaMetrics request could not be completed")]
    Request,
    #[error("VictoriaMetrics rejected the request with HTTP {status}")]
    Rejected { status: u16 },
    #[error("VictoriaMetrics returned an invalid response")]
    InvalidResponse,
}

#[derive(Clone)]
struct MetricSample {
    id: String,
    timestamp: i64,
    source: Value,
}

#[derive(Default)]
struct BucketCounts {
    count: u64,
    breakdown: BTreeMap<String, u64>,
}

/// Raw samples counted by VictoriaMetrics in one grid cell, optionally for one breakdown label value.
struct CellCount {
    start: i64,
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

impl VictoriaMetricsClient {
    pub fn new(settings: VictoriaMetricsSettings) -> Result<Self, VictoriaMetricsError> {
        if !matches!(settings.url.scheme(), "http" | "https")
            || settings.url.host_str().is_none()
            || !settings.url.username().is_empty()
            || settings.url.password().is_some()
            || settings.url.query().is_some()
            || settings.url.fragment().is_some()
        {
            return Err(VictoriaMetricsError::InvalidUrl);
        }
        if settings
            .username
            .as_deref()
            .is_some_and(|value| value.trim().is_empty())
            || settings.username.is_some() != settings.password.is_some()
        {
            return Err(VictoriaMetricsError::InvalidConfiguration);
        }
        let client = Client::builder()
            .timeout(REQUEST_TIMEOUT)
            .redirect(Policy::none())
            .build()
            .map_err(|_| VictoriaMetricsError::ClientConfiguration)?;
        Ok(Self {
            base_url: settings.url,
            client,
            username: settings.username,
            password: settings.password,
        })
    }

    pub async fn test_connection(&self) -> Result<(), VictoriaMetricsError> {
        let response = self
            .request("api/v1/query")?
            .form(&[("query", "vector(1)"), ("timeout", QUERY_TIMEOUT)])
            .send()
            .await
            .map_err(|_| VictoriaMetricsError::Request)?;
        let body = read_bounded(response).await?;
        let response: Value =
            serde_json::from_slice(&body).map_err(|_| VictoriaMetricsError::InvalidResponse)?;
        validate_api_success(&response)
    }

    pub async fn fields(
        &self,
        index: &str,
    ) -> Result<Vec<crate::data_connectors::FieldSummary>, VictoriaMetricsError> {
        validate_index(index)?;
        let response = self
            .request("api/v1/labels")?
            .query(&[("timeout", QUERY_TIMEOUT)])
            .send()
            .await
            .map_err(|_| VictoriaMetricsError::Request)?;
        let body = read_bounded(response).await?;
        let response: Value =
            serde_json::from_slice(&body).map_err(|_| VictoriaMetricsError::InvalidResponse)?;
        validate_api_success(&response)?;
        let labels = response
            .get("data")
            .and_then(Value::as_array)
            .ok_or(VictoriaMetricsError::InvalidResponse)?;
        let mut fields = vec![
            crate::data_connectors::FieldSummary {
                name: "_time".to_owned(),
                field_type: "timestamp".to_owned(),
                is_time_field: true,
            },
            crate::data_connectors::FieldSummary {
                name: "_value".to_owned(),
                field_type: "float".to_owned(),
                is_time_field: false,
            },
            crate::data_connectors::FieldSummary {
                name: "__name__".to_owned(),
                field_type: "label".to_owned(),
                is_time_field: false,
            },
        ];
        for label in labels.iter().filter_map(Value::as_str) {
            if crate::search::safe_field_name(label) {
                fields.push(crate::data_connectors::FieldSummary {
                    name: label.to_owned(),
                    field_type: "label".to_owned(),
                    is_time_field: false,
                });
            }
        }
        fields.sort_by(|left, right| left.name.cmp(&right.name));
        fields.dedup_by(|left, right| left.name == right.name);
        Ok(fields)
    }

    pub async fn search(
        &self,
        request: crate::search::SearchRequest,
    ) -> Result<Value, VictoriaMetricsError> {
        validate_index(&request.index)?;
        if request.size == 0 || request.size > MAX_PAGE_SIZE {
            return Err(VictoriaMetricsError::InvalidQuery);
        }
        if request.time_range.is_some_and(|range| {
            !range.is_valid() || range.start < MIN_TIMESTAMP_MS
        }) {
            return Err(VictoriaMetricsError::InvalidQuery);
        }
        let offset = cursor_offset(request.cursor.as_deref(), request.from)?;
        let options = request
            .histogram_options()
            .map_err(|_| VictoriaMetricsError::InvalidQuery)?;
        if request.sort_order.as_deref().is_some_and(|order| order != "asc" && order != "desc")
            || request
                .sort_by
                .as_deref()
                .is_some_and(|field| !crate::search::safe_field_name(field))
        {
            return Err(VictoriaMetricsError::InvalidQuery);
        }
        if request
            .histogram_field
            .as_deref()
            .is_some_and(|field| field != "_time")
        {
            return Err(VictoriaMetricsError::InvalidQuery);
        }

        let selector = request.query.trim();
        let selector = if selector.is_empty() {
            "{__name__!=\"\"}"
        } else {
            selector
        };
        if selector.len() > MAX_QUERY_BYTES || selector.contains(';') {
            return Err(VictoriaMetricsError::InvalidQuery);
        }

        let first_page = request.cursor.as_deref().is_none_or(str::is_empty);
        let wants_histogram = first_page
            && request.histogram_field.is_some()
            && request.include_histogram.unwrap_or(true);
        let countable = options
            .breakdown_field
            .as_deref()
            .filter(|_| wants_histogram)
            .is_none_or(is_label_name);
        if countable {
            match self
                .counted_search(&request, selector, offset, &options, wants_histogram)
                .await
            {
                // Inputs that only the export API understands fall back to the bounded raw export.
                Err(VictoriaMetricsError::Rejected { status: 400 }) => {}
                result => return result,
            }
        }
        self.exported_search(&request, selector, offset, &options, wants_histogram)
            .await
    }

    /// Counts raw samples on the server, then exports only the time window holding the requested page.
    async fn counted_search(
        &self,
        request: &crate::search::SearchRequest,
        selector: &str,
        offset: u64,
        options: &HistogramOptions,
        wants_histogram: bool,
    ) -> Result<Value, VictoriaMetricsError> {
        let started = Instant::now();
        let range = match request.time_range {
            Some(range) => Some(range),
            None => self.extent(selector).await?,
        };
        let utc = options.time_zone == "UTC";
        let reason = (wants_histogram && !utc)
            .then_some("VictoriaMetrics histograms currently support UTC only.");
        let Some(range) = range else {
            return search_result(request, started, 0, Vec::new(), Vec::new(), None, reason, offset);
        };
        let span = range.end.saturating_sub(range.start).max(1);
        let interval = options
            .interval
            .or_else(|| HistogramInterval::automatic(span as f64))
            .ok_or(VictoriaMetricsError::InvalidQuery)?;
        let histogram_wanted = wants_histogram && utc;
        let breakdown = options
            .breakdown_field
            .as_deref()
            .filter(|_| histogram_wanted);
        let grid = if interval.is_calendar() {
            DAY_MS
        } else {
            interval.approximate_millis()
        };
        let cells = self.grid_counts(selector, range, grid, breakdown).await?;
        let total = cells.iter().map(|cell| cell.count).sum::<u64>();
        let mut histogram = Vec::new();
        let mut histogram_interval = None;
        if histogram_wanted {
            histogram = weighted_histogram(
                cells
                    .iter()
                    .map(|cell| (cell.start, cell.key.clone(), cell.count)),
                interval,
                breakdown.is_some(),
            )?;
            histogram_interval = Some(format!("{}{}", interval.count, interval.suffix()));
        }
        let time_sorted = request.sort_by.as_deref().is_none_or(|field| field == "_time");
        let page = if offset >= total {
            Vec::new()
        } else if time_sorted {
            let descending =
                request.sort_by.is_none() || request.sort_order.as_deref() == Some("desc");
            let mut cells = per_cell(&cells);
            let (mut grid, mut bounds, mut skip) = (grid, range, offset);
            loop {
                let Some((low, high, before, count)) =
                    page_window(&cells, descending, skip, request.size)
                else {
                    break Vec::new();
                };
                let window = TimeRange {
                    start: low.max(bounds.start),
                    end: high.saturating_add(grid - 1).min(bounds.end),
                };
                skip -= before;
                if count <= MAX_SAMPLES as u64 || grid == 1 {
                    let samples = self.export_samples(selector, Some(window), request).await?;
                    break page_of(samples, skip, request.size);
                }
                // Dense cells are re-counted on a finer grid so the export stays within MAX_SAMPLES.
                grid = (grid / 64).max(1);
                bounds = window;
                cells = per_cell(&self.grid_counts(selector, window, grid, None).await?);
            }
        } else if total > MAX_SAMPLES as u64 {
            return Err(VictoriaMetricsError::SortTooLarge);
        } else {
            let samples = self.export_samples(selector, Some(range), request).await?;
            page_of(samples, offset, request.size)
        };
        search_result(
            request,
            started,
            total,
            page,
            histogram,
            histogram_interval,
            reason,
            offset,
        )
    }

    /// Legacy path for breakdowns that cannot be counted server-side; bounded to MAX_SAMPLES.
    async fn exported_search(
        &self,
        request: &crate::search::SearchRequest,
        selector: &str,
        offset: u64,
        options: &HistogramOptions,
        wants_histogram: bool,
    ) -> Result<Value, VictoriaMetricsError> {
        let started = Instant::now();
        let samples = self
            .export_samples(selector, request.time_range, request)
            .await?;
        let mut histogram = Vec::new();
        let mut histogram_interval = None;
        let mut reason = None;
        if wants_histogram {
            if options.time_zone != "UTC" {
                reason = Some("VictoriaMetrics histograms currently support UTC only.");
            } else {
                let (buckets, interval) = sample_histogram(&samples, request.time_range, options)?;
                histogram = buckets;
                histogram_interval = Some(interval);
            }
        }
        let total = samples.len() as u64;
        search_result(
            request,
            started,
            total,
            page_of(samples, offset, request.size),
            histogram,
            histogram_interval,
            reason,
            offset,
        )
    }

    async fn export_samples(
        &self,
        selector: &str,
        range: Option<TimeRange>,
        request: &crate::search::SearchRequest,
    ) -> Result<Vec<MetricSample>, VictoriaMetricsError> {
        let mut params = vec![
            ("match[]".to_owned(), selector.to_owned()),
            ("timeout".to_owned(), QUERY_TIMEOUT.to_owned()),
        ];
        if let Some(range) = range {
            let end = range
                .end
                .checked_add(1)
                .ok_or(VictoriaMetricsError::InvalidQuery)?;
            params.push(("start".to_owned(), range.start.to_string()));
            params.push(("end".to_owned(), end.to_string()));
        }
        let response = self
            .request("api/v1/export")?
            .form(&params)
            .send()
            .await
            .map_err(|_| VictoriaMetricsError::Request)?;
        let body = read_bounded(response).await?;
        let mut samples = parse_export(&body)?;
        if let Some(range) = range {
            samples.retain(|sample| sample.timestamp >= range.start && sample.timestamp <= range.end);
        }
        samples.sort_by(|left, right| compare_samples(left, right, request));
        Ok(samples)
    }

    /// Exact per-cell sample counts for epoch-aligned cells, clipped to the requested range.
    async fn grid_counts(
        &self,
        selector: &str,
        range: TimeRange,
        grid: i64,
        breakdown: Option<&str>,
    ) -> Result<Vec<CellCount>, VictoriaMetricsError> {
        if grid <= 0 {
            return Err(VictoriaMetricsError::InvalidQuery);
        }
        let first = range.start.div_euclid(grid) * grid;
        let last = range.end.div_euclid(grid) * grid;
        if (last - first) / grid + 1 > MAX_GRID_CELLS {
            return Err(VictoriaMetricsError::InvalidQuery);
        }
        let partial = |cell: i64| cell < range.start || cell + grid - 1 > range.end;
        let full_first = if partial(first) { first + grid } else { first };
        let full_last = if partial(last) { last - grid } else { last };
        let mut cells = Vec::new();
        if full_first <= full_last {
            // A point at a cell's last millisecond counts that cell, because rollup windows are left-open.
            let series = self
                .range_query(
                    &count_expression(selector, grid, breakdown),
                    full_first + grid - 1,
                    full_last + grid - 1,
                    grid,
                )
                .await?;
            for series in &series {
                let key = series_key(series, breakdown);
                let values = series
                    .get("values")
                    .and_then(Value::as_array)
                    .ok_or(VictoriaMetricsError::InvalidResponse)?;
                for point in values {
                    let (timestamp, count) = count_point(point)?;
                    cells.push(CellCount {
                        start: timestamp - (grid - 1),
                        key: key.clone(),
                        count,
                    });
                }
            }
        }
        let mut edges = vec![first];
        if last != first {
            edges.push(last);
        }
        for cell in edges.into_iter().filter(|cell| partial(*cell)) {
            let low = cell.max(range.start);
            let high = (cell + grid - 1).min(range.end);
            let series = self
                .instant_query(&count_expression(selector, high - low + 1, breakdown), high)
                .await?;
            for series in &series {
                let point = series
                    .get("value")
                    .ok_or(VictoriaMetricsError::InvalidResponse)?;
                cells.push(CellCount {
                    start: cell,
                    key: series_key(series, breakdown),
                    count: count_point(point)?.1,
                });
            }
        }
        cells.retain(|cell| cell.count > 0);
        Ok(cells)
    }

    /// First and last raw-sample timestamps for all-time searches, or `None` when nothing matches.
    async fn extent(&self, selector: &str) -> Result<Option<TimeRange>, VictoriaMetricsError> {
        let now = Utc::now().timestamp_millis();
        let window = now.saturating_sub(MIN_TIMESTAMP_MS).max(1);
        let mut bounds = Vec::new();
        for function in ["min(tfirst_over_time", "max(tlast_over_time"] {
            let series = self
                .instant_query(&format!("{function}(({selector})[{window}ms]))"), now)
                .await?;
            let Some(point) = series.first().and_then(|series| series.get("value")) else {
                return Ok(None);
            };
            let seconds = point
                .get(1)
                .and_then(Value::as_str)
                .and_then(|value| value.parse::<f64>().ok())
                .filter(|value| value.is_finite())
                .ok_or(VictoriaMetricsError::InvalidResponse)?;
            bounds.push((seconds * 1000.0).round() as i64);
        }
        Ok(Some(TimeRange {
            start: bounds[0].max(MIN_TIMESTAMP_MS),
            end: bounds[1].max(bounds[0]),
        }))
    }

    async fn instant_query(
        &self,
        query: &str,
        time: i64,
    ) -> Result<Vec<Value>, VictoriaMetricsError> {
        self.query_api(
            "api/v1/query",
            "vector",
            vec![("query", query.to_owned()), ("time", seconds(time))],
        )
        .await
    }

    async fn range_query(
        &self,
        query: &str,
        start: i64,
        end: i64,
        step: i64,
    ) -> Result<Vec<Value>, VictoriaMetricsError> {
        self.query_api(
            "api/v1/query_range",
            "matrix",
            vec![
                ("query", query.to_owned()),
                ("start", seconds(start)),
                ("end", seconds(end)),
                ("step", format!("{step}ms")),
            ],
        )
        .await
    }

    async fn query_api(
        &self,
        path: &str,
        result_type: &str,
        mut params: Vec<(&str, String)>,
    ) -> Result<Vec<Value>, VictoriaMetricsError> {
        // nocache keeps exact evaluation timestamps; the minimum latency_offset includes the newest samples.
        params.extend([
            ("timeout", QUERY_TIMEOUT.to_owned()),
            ("nocache", "1".to_owned()),
            ("latency_offset", "1ms".to_owned()),
        ]);
        let response = self
            .request(path)?
            .form(&params)
            .send()
            .await
            .map_err(|_| VictoriaMetricsError::Request)?;
        let body = read_bounded(response).await?;
        let mut response: Value =
            serde_json::from_slice(&body).map_err(|_| VictoriaMetricsError::InvalidResponse)?;
        validate_api_success(&response)?;
        let data = response
            .get_mut("data")
            .ok_or(VictoriaMetricsError::InvalidResponse)?;
        if data.get("resultType").and_then(Value::as_str) != Some(result_type) {
            return Err(VictoriaMetricsError::InvalidResponse);
        }
        match data.get_mut("result").map(Value::take) {
            Some(Value::Array(series)) => Ok(series),
            _ => Err(VictoriaMetricsError::InvalidResponse),
        }
    }

    fn request(&self, path: &str) -> Result<reqwest::RequestBuilder, VictoriaMetricsError> {
        let mut endpoint = self.base_url.clone();
        let mut segments = endpoint
            .path_segments_mut()
            .map_err(|_| VictoriaMetricsError::InvalidUrl)?;
        segments.pop_if_empty();
        for segment in path.split('/') {
            segments.push(segment);
        }
        drop(segments);
        let request = self
            .client
            .post(endpoint)
            .header(reqwest::header::ACCEPT, "application/json");
        Ok(match (self.username.as_deref(), self.password.as_deref()) {
            (Some(username), Some(password)) => request.basic_auth(username, Some(password)),
            _ => request,
        })
    }
}

fn validate_index(index: &str) -> Result<(), VictoriaMetricsError> {
    if index == "metrics" {
        Ok(())
    } else {
        Err(VictoriaMetricsError::InvalidQuery)
    }
}

fn cursor_offset(cursor: Option<&str>, from: usize) -> Result<u64, VictoriaMetricsError> {
    match cursor {
        None if from <= 10_000 => Ok(from as u64),
        Some("") => Ok(0),
        Some(token) if token.len() <= 15 && token.bytes().all(|byte| byte.is_ascii_digit()) => {
            token.parse::<u64>().map_err(|_| VictoriaMetricsError::InvalidQuery)
        }
        _ => Err(VictoriaMetricsError::InvalidQuery),
    }
}

#[allow(clippy::too_many_arguments)]
fn search_result(
    request: &crate::search::SearchRequest,
    started: Instant,
    total: u64,
    page: Vec<MetricSample>,
    histogram: Vec<HistogramBucket>,
    histogram_interval: Option<String>,
    histogram_unavailable_reason: Option<&str>,
    offset: u64,
) -> Result<Value, VictoriaMetricsError> {
    let end = offset
        .checked_add(request.size as u64)
        .ok_or(VictoriaMetricsError::InvalidQuery)?;
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
        "histogram_unavailable_reason": histogram_unavailable_reason,
    });
    if let Some(interval) = histogram_interval {
        result["histogram_interval"] = json!(interval);
    }
    if end < total {
        result["next_cursor"] = json!(end.to_string());
    }
    Ok(result)
}

/// Total samples per cell start, ascending.
fn per_cell(cells: &[CellCount]) -> Vec<(i64, u64)> {
    let mut totals = BTreeMap::<i64, u64>::new();
    for cell in cells {
        *totals.entry(cell.start).or_default() += cell.count;
    }
    totals.into_iter().collect()
}

/// Contiguous cells holding sorted positions `[offset, offset + size)`:
/// returns the lowest and highest cell starts, samples before the window, and samples inside it.
fn page_window(
    cells: &[(i64, u64)],
    descending: bool,
    offset: u64,
    size: usize,
) -> Option<(i64, i64, u64, u64)> {
    let ordered: Box<dyn Iterator<Item = &(i64, u64)>> = if descending {
        Box::new(cells.iter().rev())
    } else {
        Box::new(cells.iter())
    };
    let end_offset = offset.saturating_add(size as u64);
    let (mut seen, mut window) = (0u64, None::<(i64, i64, u64, u64)>);
    for &(start, count) in ordered {
        let next = seen.saturating_add(count);
        if next > offset && seen < end_offset {
            window = Some(match window {
                None => (start, start, seen, count),
                Some((low, high, before, inside)) => {
                    (low.min(start), high.max(start), before, inside + count)
                }
            });
        }
        if next >= end_offset {
            break;
        }
        seen = next;
    }
    window
}

fn page_of(samples: Vec<MetricSample>, offset: u64, size: usize) -> Vec<MetricSample> {
    samples
        .into_iter()
        .skip(usize::try_from(offset).unwrap_or(usize::MAX))
        .take(size)
        .collect()
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

fn count_expression(selector: &str, window: i64, breakdown: Option<&str>) -> String {
    // Rollups drop the metric name unless asked to keep it, which a __name__ breakdown needs.
    let keep = if breakdown == Some("__name__") {
        " keep_metric_names"
    } else {
        ""
    };
    let counted = format!("count_over_time(({selector})[{window}ms]){keep}");
    match breakdown {
        Some(label) => format!("sum by ({label}) ({counted})"),
        None => format!("sum({counted})"),
    }
}

fn series_key(series: &Value, breakdown: Option<&str>) -> Option<String> {
    breakdown
        .and_then(|label| series.get("metric")?.get(label)?.as_str())
        .map(str::to_owned)
}

fn count_point(point: &Value) -> Result<(i64, u64), VictoriaMetricsError> {
    let point = point
        .as_array()
        .filter(|point| point.len() == 2)
        .ok_or(VictoriaMetricsError::InvalidResponse)?;
    let seconds = point[0]
        .as_f64()
        .filter(|value| value.is_finite())
        .ok_or(VictoriaMetricsError::InvalidResponse)?;
    let count = point[1]
        .as_str()
        .and_then(|value| value.parse::<f64>().ok())
        .filter(|value| value.is_finite() && *value >= 0.0)
        .ok_or(VictoriaMetricsError::InvalidResponse)?;
    Ok(((seconds * 1000.0).round() as i64, count as u64))
}

fn parse_export(body: &[u8]) -> Result<Vec<MetricSample>, VictoriaMetricsError> {
    let mut samples = Vec::new();
    let lines = serde_json::Deserializer::from_slice(body).into_iter::<Value>();
    for line in lines {
        let line = line.map_err(|_| VictoriaMetricsError::InvalidResponse)?;
        let metric = line
            .get("metric")
            .and_then(Value::as_object)
            .ok_or(VictoriaMetricsError::InvalidResponse)?;
        let values = line
            .get("values")
            .and_then(Value::as_array)
            .ok_or(VictoriaMetricsError::InvalidResponse)?;
        let timestamps = line
            .get("timestamps")
            .and_then(Value::as_array)
            .ok_or(VictoriaMetricsError::InvalidResponse)?;
        if values.len() != timestamps.len() {
            return Err(VictoriaMetricsError::InvalidResponse);
        }
        for (value, timestamp) in values.iter().zip(timestamps) {
            let timestamp = timestamp
                .as_i64()
                .or_else(|| timestamp.as_u64().and_then(|value| i64::try_from(value).ok()))
                .ok_or(VictoriaMetricsError::InvalidResponse)?;
            let mut source = Map::new();
            source.insert("_time".to_owned(), json!(timestamp));
            source.insert("_value".to_owned(), value.clone());
            for (name, value) in metric {
                if !value.is_string() {
                    return Err(VictoriaMetricsError::InvalidResponse);
                }
                source.insert(name.clone(), value.clone());
            }
            let source = Value::Object(source);
            // Ids depend only on labels, time, and value so paging windows sort identically.
            let mut hasher = std::collections::hash_map::DefaultHasher::new();
            source.to_string().hash(&mut hasher);
            samples.push(MetricSample {
                id: format!("sample-{:016x}", hasher.finish()),
                timestamp,
                source,
            });
            if samples.len() > MAX_SAMPLES {
                return Err(VictoriaMetricsError::TooManySamples);
            }
        }
    }
    Ok(samples)
}

fn compare_samples(
    left: &MetricSample,
    right: &MetricSample,
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
    samples: &[MetricSample],
    time_range: Option<TimeRange>,
    options: &HistogramOptions,
) -> Result<(Vec<HistogramBucket>, String), VictoriaMetricsError> {
    let span = time_range
        .map(|range| range.end.saturating_sub(range.start).max(1))
        .or_else(|| {
            let min = samples.iter().map(|sample| sample.timestamp).min()?;
            let max = samples.iter().map(|sample| sample.timestamp).max()?;
            Some(max.saturating_sub(min).max(1))
        })
        .unwrap_or(1);
    let interval = options
        .interval
        .or_else(|| HistogramInterval::automatic(span as f64))
        .ok_or(VictoriaMetricsError::InvalidQuery)?;
    let interval_text = format!("{}{}", interval.count, interval.suffix());
    let breakdown = options.breakdown_field.as_deref();
    let histogram = weighted_histogram(
        samples.iter().map(|sample| {
            let key = breakdown
                .and_then(|field| sample.source.get(field))
                .filter(|value| !value.is_null())
                .map(|value| match value {
                    Value::String(value) => value.clone(),
                    other => other.to_string(),
                });
            (sample.timestamp, key, 1)
        }),
        interval,
        breakdown.is_some(),
    )?;
    Ok((histogram, interval_text))
}

fn weighted_histogram(
    entries: impl IntoIterator<Item = (i64, Option<String>, u64)>,
    interval: HistogramInterval,
    with_breakdown: bool,
) -> Result<Vec<HistogramBucket>, VictoriaMetricsError> {
    let mut buckets = BTreeMap::<i64, BucketCounts>::new();
    let mut key_totals = HashMap::<String, u64>::new();
    for (timestamp, key, weight) in entries {
        let timestamp = bucket_timestamp(timestamp, interval)?;
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
    let histogram = buckets
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
        .collect();
    Ok(histogram)
}

fn bucket_timestamp(
    timestamp: i64,
    interval: HistogramInterval,
) -> Result<i64, VictoriaMetricsError> {
    if !interval.is_calendar() {
        let millis = interval.approximate_millis();
        return Ok(timestamp.div_euclid(millis) * millis);
    }
    let date = Utc
        .timestamp_millis_opt(timestamp)
        .single()
        .ok_or(VictoriaMetricsError::InvalidResponse)?
        .date_naive();
    let date = match interval.unit {
        IntervalUnit::Week => {
            date - chrono::Duration::days(date.weekday().num_days_from_monday() as i64)
        }
        IntervalUnit::Month => NaiveDate::from_ymd_opt(date.year(), date.month(), 1)
            .ok_or(VictoriaMetricsError::InvalidResponse)?,
        IntervalUnit::Year => NaiveDate::from_ymd_opt(date.year(), 1, 1)
            .ok_or(VictoriaMetricsError::InvalidResponse)?,
        _ => return Err(VictoriaMetricsError::InvalidQuery),
    };
    Ok(Utc
        .from_local_datetime(&date.and_time(NaiveTime::MIN))
        .single()
        .ok_or(VictoriaMetricsError::InvalidResponse)?
        .timestamp_millis())
}

fn validate_api_success(response: &Value) -> Result<(), VictoriaMetricsError> {
    if response.get("status").and_then(Value::as_str) == Some("success") {
        Ok(())
    } else {
        Err(VictoriaMetricsError::InvalidResponse)
    }
}

async fn read_bounded(response: Response) -> Result<Vec<u8>, VictoriaMetricsError> {
    let status = response.status();
    if !status.is_success() {
        return Err(VictoriaMetricsError::Rejected {
            status: status.as_u16(),
        });
    }
    if response
        .content_length()
        .is_some_and(|length| length > MAX_RESPONSE_BYTES as u64)
    {
        return Err(VictoriaMetricsError::ResponseTooLarge);
    }
    let mut response = response;
    let mut body = Vec::new();
    while let Some(chunk) = response
        .chunk()
        .await
        .map_err(|_| VictoriaMetricsError::Request)?
    {
        if body.len().saturating_add(chunk.len()) > MAX_RESPONSE_BYTES {
            return Err(VictoriaMetricsError::ResponseTooLarge);
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

    fn client(url: &str) -> VictoriaMetricsClient {
        VictoriaMetricsClient::new(VictoriaMetricsSettings {
            url: Url::parse(url).unwrap(),
            username: None,
            password: None,
        })
        .unwrap()
    }

    #[test]
    fn connection_settings_reject_unsafe_urls_and_partial_basic_auth() {
        for raw_url in [
            "http://user:secret@localhost:8428",
            "http://localhost:8428?secret=value",
            "http://localhost:8428#fragment",
            "file:///tmp/victoriametrics",
        ] {
            assert!(VictoriaMetricsClient::new(VictoriaMetricsSettings {
                url: Url::parse(raw_url).unwrap(),
                username: None,
                password: None,
            })
            .is_err());
        }
        assert!(VictoriaMetricsClient::new(VictoriaMetricsSettings {
            url: Url::parse("http://localhost:8428").unwrap(),
            username: Some("user".to_owned()),
            password: None,
        })
        .is_err());
    }

    #[test]
    fn descriptor_uses_inline_samples_and_selector_query() {
        let descriptor = descriptor();
        assert_eq!(descriptor.id, "victoriametrics");
        assert_eq!(descriptor.query["document_mode"], "inline");
        assert_eq!(descriptor.query["schema_label"], "VictoriaMetrics labels");
    }

    #[test]
    fn export_jsonl_flattens_samples_and_preserves_label_and_value_types() {
        let body = br#"{"metric":{"__name__":"up","job":"api"},"values":[1,2],"timestamps":[1000,2000]}"#;
        let samples = parse_export(body).unwrap();
        assert_eq!(samples.len(), 2);
        assert_eq!(samples[0].source["__name__"], "up");
        assert_eq!(samples[0].source["job"], "api");
        assert_eq!(samples[0].source["_time"], 1000);
        assert_eq!(samples[0].source["_value"], 1);
        assert_ne!(samples[0].id, samples[1].id);
    }

    fn form_contains(parts: &'static [&'static str]) -> impl Fn(&wiremock::Request) -> bool {
        move |request: &wiremock::Request| {
            let body = String::from_utf8_lossy(&request.body);
            parts.iter().all(|part| body.contains(part))
        }
    }

    #[tokio::test]
    async fn search_counts_on_the_server_and_exports_only_the_page_window() {
        let server = MockServer::start().await;
        for (path, parts, body) in [
            (
                "/api/v1/query_range",
                &["query=sum+by+%28job%29", "%5B1000ms%5D", "start=86400.999", "end=86401.999", "nocache=1"][..],
                json!({"status":"success","data":{"resultType":"matrix","result":[
                    {"metric":{"job":"api"},"values":[[86400.999,"1"],[86401.999,"1"]]}
                ]}}),
            ),
            (
                "/api/v1/query",
                &["query=sum+by+%28job%29", "%5B1ms%5D", "time=86402.000"][..],
                json!({"status":"success","data":{"resultType":"vector","result":[
                    {"metric":{"job":"api"},"value":[86402.0,"1"]}
                ]}}),
            ),
            (
                "/api/v1/query_range",
                &["query=sum%28count_over_time", "%5B1000ms%5D"][..],
                json!({"status":"success","data":{"resultType":"matrix","result":[
                    {"metric":{},"values":[[86400.999,"1"],[86401.999,"1"]]}
                ]}}),
            ),
            (
                "/api/v1/query",
                &["query=sum%28count_over_time", "%5B1ms%5D"][..],
                json!({"status":"success","data":{"resultType":"vector","result":[
                    {"metric":{},"value":[86402.0,"1"]}
                ]}}),
            ),
        ] {
            Mock::given(matchers::method("POST"))
                .and(matchers::path(path))
                .and(form_contains(parts))
                .respond_with(ResponseTemplate::new(200).set_body_json(body))
                .expect(1)
                .mount(&server)
                .await;
        }
        for (parts, body) in [
            (
                &["start=86401000", "end=86402001"][..],
                "{\"metric\":{\"__name__\":\"up\",\"job\":\"api\"},\"values\":[2,3],\"timestamps\":[86401000,86402000]}\n",
            ),
            (
                &["start=86400000", "end=86401000"][..],
                "{\"metric\":{\"__name__\":\"up\",\"job\":\"api\"},\"values\":[1],\"timestamps\":[86400000]}\n",
            ),
        ] {
            Mock::given(matchers::method("POST"))
                .and(matchers::path("/api/v1/export"))
                .and(form_contains(parts))
                .respond_with(ResponseTemplate::new(200).set_body_raw(body, "application/x-ndjson"))
                .expect(1)
                .mount(&server)
                .await;
        }
        let client = client(&server.uri());
        let request = || {
            serde_json::from_value(json!({
                "index":"metrics","query":"up","size":2,"cursor":"",
                "sort_by":"_time","sort_order":"desc","histogram_field":"_time",
                "histogram_interval":"1s","histogram_breakdown_field":"job",
                "time_range":{"start":86400000i64,"end":86402000i64}
            }))
            .unwrap()
        };
        let first = Connector::search(&client, request()).await.unwrap();
        assert_eq!(first["total"], 3);
        assert_eq!(first["hits"].as_array().unwrap().len(), 2);
        assert_eq!(first["hits"][0]["source"]["_time"], 86402000);
        assert_eq!(first["hits"][1]["source"]["_time"], 86401000);
        assert_eq!(first["next_cursor"], "2");
        assert_eq!(first["histogram"].as_array().unwrap().len(), 3);
        assert_eq!(first["histogram"][2]["timestamp"], 86402000);
        assert_eq!(first["histogram"][0]["breakdown"][0]["key"], "api");

        let mut second_request = request();
        second_request.cursor = Some(first["next_cursor"].as_str().unwrap().to_owned());
        let second = Connector::search(&client, second_request).await.unwrap();
        assert_eq!(second["total"], 3);
        assert_eq!(second["hits"].as_array().unwrap().len(), 1);
        assert_eq!(second["hits"][0]["source"]["_time"], 86400000);
        assert!(second.get("next_cursor").is_none());
    }

    #[test]
    fn page_windows_cover_only_cells_holding_the_requested_positions() {
        let cells = [(0, 5), (10, 0), (20, 3), (30, 4)];
        assert_eq!(page_window(&cells, true, 0, 2), Some((30, 30, 0, 4)));
        assert_eq!(page_window(&cells, true, 3, 3), Some((20, 30, 0, 7)));
        assert_eq!(page_window(&cells, false, 5, 3), Some((20, 20, 5, 3)));
        assert_eq!(page_window(&cells, false, 12, 3), None);
    }

    #[test]
    fn count_expressions_keep_metric_names_only_for_name_breakdowns() {
        assert_eq!(
            count_expression("up", 1000, None),
            "sum(count_over_time((up)[1000ms]))"
        );
        assert_eq!(
            count_expression("up", 5, Some("__name__")),
            "sum by (__name__) (count_over_time((up)[5ms]) keep_metric_names)"
        );
        assert!(is_label_name("job") && !is_label_name("_value") && !is_label_name("a-b"));
    }

    #[tokio::test]
    async fn uncountable_selectors_fall_back_to_bounded_export() {
        let server = MockServer::start().await;
        for path in ["/api/v1/query_range", "/api/v1/query"] {
            Mock::given(matchers::method("POST"))
                .and(matchers::path(path))
                .respond_with(ResponseTemplate::new(400))
                .mount(&server)
                .await;
        }
        Mock::given(matchers::method("POST"))
            .and(matchers::path("/api/v1/export"))
            .and(|request: &wiremock::Request| {
                let body = String::from_utf8_lossy(&request.body);
                body.contains("match%5B%5D=up")
                    && body.contains("start=86400000")
                    && body.contains("end=86402001")
            })
            .respond_with(ResponseTemplate::new(200).set_body_raw(
                "{\"metric\":{\"__name__\":\"up\",\"job\":\"api\"},\"values\":[1,2,3],\"timestamps\":[86400000,86401000,86402000]}\n",
                "application/x-ndjson",
            ))
            .expect(2)
            .mount(&server)
            .await;
        let client = client(&server.uri());
        let request = || {
            serde_json::from_value(json!({
                "index":"metrics","query":"up","size":2,"cursor":"",
                "sort_by":"_time","sort_order":"desc","histogram_field":"_time",
                "histogram_interval":"1s","histogram_breakdown_field":"job",
                "time_range":{"start":86400000i64,"end":86402000i64}
            }))
            .unwrap()
        };
        let first = Connector::search(&client, request()).await.unwrap();
        assert_eq!(first["total"], 3);
        assert_eq!(first["total_relation"], "eq");
        assert_eq!(first["hits"].as_array().unwrap().len(), 2);
        assert_eq!(first["hits"][0]["source"]["_time"], 86402000);
        assert_eq!(first["hits"][0]["source"]["_value"], 3);
        assert_eq!(first["next_cursor"], "2");
        assert_eq!(first["histogram"].as_array().unwrap().len(), 3);
        assert_eq!(first["histogram"][0]["breakdown"][0]["key"], "api");

        let mut second_request = request();
        second_request.cursor = Some(first["next_cursor"].as_str().unwrap().to_owned());
        let second = Connector::search(&client, second_request).await.unwrap();
        assert_eq!(second["hits"].as_array().unwrap().len(), 1);
        assert_eq!(second["hits"][0]["source"]["_time"], 86400000);
        assert_eq!(second["histogram"], json!([]));
        assert!(second.get("next_cursor").is_none());
    }

    #[tokio::test]
    async fn fields_include_synthetic_time_and_raw_value_fields() {
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

    #[test]
    fn non_utc_histograms_fail_explicitly_without_changing_the_shared_shape() {
        let request = serde_json::from_value::<crate::search::SearchRequest>(json!({
            "index":"metrics","time_zone":"America/Los_Angeles"
        }))
        .unwrap();
        assert_eq!(request.histogram_options().unwrap().time_zone, "America/Los_Angeles");
    }
}
