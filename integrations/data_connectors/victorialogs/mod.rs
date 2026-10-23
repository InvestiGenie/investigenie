use std::{collections::BTreeMap, time::Duration};

use crate::data_connectors::IntegrationDescriptor;
use crate::search::{BreakdownCount, HistogramInterval, HistogramOptions, MAX_BREAKDOWN_VALUES};
use chrono::{DateTime, SecondsFormat, Utc};
use reqwest::{redirect::Policy, Client, Response, Url};
use serde_json::{json, Value};
use thiserror::Error;

mod connector;
pub use connector::connect;

const REQUEST_TIMEOUT: Duration = Duration::from_secs(15);
const SEARCH_TIMEOUT: &str = "10s";
const MAX_QUERY_BYTES: usize = 8192;
const MAX_RESPONSE_BYTES: usize = 16 * 1024 * 1024;
const MAX_PAGE_SIZE: usize = 100;
const MAX_CURSOR_OFFSET: u64 = i64::MAX as u64 - MAX_PAGE_SIZE as u64;

pub fn descriptor() -> IntegrationDescriptor {
    IntegrationDescriptor {
        id: "victorialogs",
        display_name: "VictoriaLogs",
        query_language: "LogsQL",
        description: "LogsQL queries over VictoriaLogs log streams",
        connection_fields: vec![
            json!({"name":"url","label":"URL","input_type":"url","required":true,"secret":false,"editable":true,"placeholder":"http://localhost:9428"}),
            json!({"name":"username","label":"Username","input_type":"text","required":false,"secret":true,"editable":true,"group":"credentials","placeholder":""}),
            json!({"name":"password","label":"Password","input_type":"password","required":false,"secret":true,"editable":true,"group":"credentials","placeholder":""}),
            json!({"name":"account_id","label":"Account ID","input_type":"text","required":false,"secret":false,"editable":true,"placeholder":"0"}),
            json!({"name":"project_id","label":"Project ID","input_type":"text","required":false,"secret":false,"editable":true,"placeholder":"0"}),
        ],
        query: json!({"placeholder":"Enter a LogsQL filter, e.g. error AND app:api","default_query":"","field_query":"{query} AND {field}:*","identifier_quote":"`","help":"LogsQL filters run against the selected VictoriaLogs source","document_mode":"inline","sort_requires_projection":false,"histogram_total_from_buckets":false,"schema_label":"VictoriaLogs fields"}),
    }
}

#[derive(Clone)]
pub struct VictoriaLogsSettings {
    pub url: Url,
    pub username: Option<String>,
    pub password: Option<String>,
    pub account_id: Option<String>,
    pub project_id: Option<String>,
}

#[derive(Clone)]
pub struct VictoriaLogsClient {
    base_url: Url,
    client: Client,
    username: Option<String>,
    password: Option<String>,
    account_id: Option<String>,
    project_id: Option<String>,
}

#[derive(Debug, Error)]
pub enum VictoriaLogsError {
    #[error("VictoriaLogs URL must be an absolute HTTP or HTTPS URL without embedded credentials, query, or fragment")]
    InvalidUrl,
    #[error("VictoriaLogs connection settings are invalid")]
    InvalidConfiguration,
    #[error("VictoriaLogs query or search parameters are invalid or exceed configured limits")]
    InvalidQuery,
    #[error("Could not configure the VictoriaLogs HTTP client")]
    ClientConfiguration,
    #[error("VictoriaLogs request could not be completed")]
    Request,
    #[error("VictoriaLogs rejected the request with HTTP {status}")]
    Rejected { status: u16 },
    #[error("VictoriaLogs returned an invalid response")]
    InvalidResponse,
    #[error("VictoriaLogs response exceeded the configured size limit")]
    ResponseTooLarge,
}

#[derive(Debug, serde::Serialize)]
struct HistogramBucket {
    timestamp: i64,
    label: Option<String>,
    count: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    breakdown: Option<Vec<BreakdownCount>>,
}

#[derive(Default)]
struct BucketCounts {
    count: u64,
    breakdown: BTreeMap<String, u64>,
    other: u64,
}

impl VictoriaLogsClient {
    pub fn new(settings: VictoriaLogsSettings) -> Result<Self, VictoriaLogsError> {
        if !matches!(settings.url.scheme(), "http" | "https")
            || settings.url.host_str().is_none()
            || !settings.url.username().is_empty()
            || settings.url.password().is_some()
            || settings.url.query().is_some()
            || settings.url.fragment().is_some()
            || settings
                .username
                .as_deref()
                .is_some_and(|username| username.trim().is_empty())
            || settings.username.is_some() != settings.password.is_some()
        {
            return Err(VictoriaLogsError::InvalidUrl);
        }
        for tenant in [&settings.account_id, &settings.project_id]
            .into_iter()
            .flatten()
        {
            if tenant.parse::<u32>().is_err() {
                return Err(VictoriaLogsError::InvalidConfiguration);
            }
        }

        let client = Client::builder()
            .timeout(REQUEST_TIMEOUT)
            .redirect(Policy::none())
            .build()
            .map_err(|_| VictoriaLogsError::ClientConfiguration)?;
        Ok(Self {
            base_url: settings.url,
            client,
            username: settings.username,
            password: settings.password,
            account_id: settings.account_id,
            project_id: settings.project_id,
        })
    }

    pub async fn test_connection(&self) -> Result<(), VictoriaLogsError> {
        let mut params = vec![
            ("query".to_owned(), "*".to_owned()),
            ("start".to_owned(), "now-1m".to_owned()),
            ("end".to_owned(), "now".to_owned()),
            ("limit".to_owned(), "1".to_owned()),
            ("timeout".to_owned(), SEARCH_TIMEOUT.to_owned()),
        ];
        let body = self.post_form("select/logsql/field_names", &params).await?;
        let _: Value = serde_json::from_slice(&body).map_err(|_| VictoriaLogsError::InvalidResponse)?;
        params.clear();
        Ok(())
    }

    pub async fn fields(&self, index: &str) -> Result<Vec<crate::data_connectors::FieldSummary>, VictoriaLogsError> {
        validate_index(index)?;
        let response = self
            .post_json(
                "select/logsql/field_names",
                &[
                    ("query".to_owned(), "*".to_owned()),
                    ("timeout".to_owned(), SEARCH_TIMEOUT.to_owned()),
                ],
            )
            .await?;
        let values = response
            .get("values")
            .and_then(Value::as_array)
            .ok_or(VictoriaLogsError::InvalidResponse)?;
        let mut fields = values
            .iter()
            .filter_map(|item| item.get("value").and_then(Value::as_str))
            .filter(|name| !name.is_empty() && name.len() <= 256)
            .map(|name| crate::data_connectors::FieldSummary {
                name: name.to_owned(),
                field_type: match name {
                    "_time" => "timestamp",
                    "_msg" => "text",
                    "_stream" => "stream",
                    "_stream_id" => "keyword",
                    _ => "unknown",
                }
                .to_owned(),
                is_time_field: name == "_time",
            })
            .collect::<Vec<_>>();
        fields.sort_by(|left, right| left.name.cmp(&right.name));
        fields.dedup_by(|left, right| left.name == right.name);
        Ok(fields)
    }

    pub async fn search(&self, request: crate::search::SearchRequest) -> Result<Value, VictoriaLogsError> {
        validate_index(&request.index)?;
        if request.size == 0 || request.size > MAX_PAGE_SIZE {
            return Err(VictoriaLogsError::InvalidQuery);
        }
        if request
            .time_range
            .is_some_and(|range| !range.is_valid())
        {
            return Err(VictoriaLogsError::InvalidQuery);
        }
        let offset = cursor_offset(request.cursor.as_deref(), request.from)?;
        let options = request
            .histogram_options()
            .map_err(|_| VictoriaLogsError::InvalidQuery)?;
        if request.sort_order.as_deref().is_some_and(|order| order != "asc" && order != "desc") {
            return Err(VictoriaLogsError::InvalidQuery);
        }
        if request
            .sort_by
            .as_deref()
            .is_some_and(|field| !crate::search::safe_field_name(field))
        {
            return Err(VictoriaLogsError::InvalidQuery);
        }
        if request
            .histogram_field
            .as_deref()
            .is_some_and(|field| field != "_time")
        {
            return Err(VictoriaLogsError::InvalidQuery);
        }

        let query = request.query.trim();
        let query = if query.is_empty() { "*" } else { query };
        if query.len() > MAX_QUERY_BYTES {
            return Err(VictoriaLogsError::InvalidQuery);
        }
        let page_query = match request.sort_by.as_deref() {
            Some(field) => format!(
                "{query} | sort by ({field} {})",
                request.sort_order.as_deref().unwrap_or("desc")
            ),
            None => query.to_owned(),
        };
        if page_query.len() > MAX_QUERY_BYTES {
            return Err(VictoriaLogsError::InvalidQuery);
        }

        let started = std::time::Instant::now();
        let mut page_params = vec![
            ("query".to_owned(), page_query),
            ("limit".to_owned(), (request.size + 1).to_string()),
            ("offset".to_owned(), offset.to_string()),
            ("timeout".to_owned(), SEARCH_TIMEOUT.to_owned()),
        ];
        add_time_range(&mut page_params, request.time_range)?;
        let body = self.post_form("select/logsql/query", &page_params).await?;
        let mut rows = parse_json_lines(&body)?;
        let has_more = rows.len() > request.size;
        if has_more {
            rows.truncate(request.size);
        }

        let first_page = request.cursor.as_deref().is_none_or(str::is_empty);
        let mut total = offset.saturating_add(rows.len() as u64);
        let mut total_relation = if has_more { "gte" } else { "eq" };
        let mut histogram = Vec::new();
        let mut histogram_interval = None;
        let mut histogram_unavailable_reason = None;
        if first_page {
            if request.histogram_field.is_some()
                && request.include_histogram.unwrap_or(true)
            {
                match self
                    .histogram_and_total(query, request.time_range, &options)
                    .await
                {
                    Ok((count, buckets, interval)) => {
                        total = count;
                        total_relation = "eq";
                        histogram = buckets;
                        histogram_interval = interval;
                    }
                    Err(error) => {
                        histogram_unavailable_reason = Some(histogram_error_reason(&error));
                        match self.count_query(query, request.time_range).await {
                            Ok(count) => {
                                total = count;
                                total_relation = "eq";
                            }
                            Err(_) => total_relation = "gte",
                        }
                    }
                }
            } else {
                match self.count_query(query, request.time_range).await {
                    Ok(count) => {
                        total = count;
                        total_relation = "eq";
                    }
                    Err(_) => total_relation = "gte",
                }
            }
        }

        let hits = rows
            .into_iter()
            .enumerate()
            .map(|(index, source)| {
                json!({
                    "index": request.index,
                    "id": format!("row-{}", offset.saturating_add(index as u64 + 1)),
                    "score": null,
                    "source": source
                })
            })
            .collect::<Vec<_>>();
        let next_cursor = has_more
            .then(|| offset.checked_add(request.size as u64))
            .flatten()
            .filter(|next| *next <= MAX_CURSOR_OFFSET)
            .map(|next| next.to_string());
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

    async fn histogram_and_total(
        &self,
        query: &str,
        time_range: Option<crate::search::TimeRange>,
        options: &HistogramOptions,
    ) -> Result<(u64, Vec<HistogramBucket>, Option<String>), VictoriaLogsError> {
        if options.time_zone != "UTC" {
            return Err(VictoriaLogsError::InvalidQuery);
        }
        let (range, known_total) = match time_range {
            Some(range) => (range, None),
            None => {
                let stats = self.all_time_stats(query).await?;
                let total = stats
                    .get("_ig_total")
                    .map(parse_count)
                    .transpose()?
                    .unwrap_or(0);
                if total == 0 {
                    return Ok((0, Vec::new(), None));
                }
                let start = parse_timestamp(&stats["_ig_min_time"])?;
                let end = parse_timestamp(&stats["_ig_max_time"])?;
                (crate::search::TimeRange { start, end }, Some(total))
            }
        };
        let span = range.end.saturating_sub(range.start).max(1);
        let interval = options
            .interval
            .or_else(|| HistogramInterval::automatic(span as f64))
            .ok_or(VictoriaLogsError::InvalidQuery)?;
        let step = format!("{}{}", interval.count, interval.suffix());
        let mut params = vec![
            ("query".to_owned(), query.to_owned()),
            ("step".to_owned(), step.clone()),
            ("offset".to_owned(), "0s".to_owned()),
            ("timeout".to_owned(), SEARCH_TIMEOUT.to_owned()),
        ];
        add_time_range(&mut params, Some(range))?;
        if let Some(field) = options.breakdown_field.as_deref() {
            params.push(("field".to_owned(), field.to_owned()));
            params.push(("fields_limit".to_owned(), MAX_BREAKDOWN_VALUES.to_string()));
        }
        let response = self
            .post_json("select/logsql/hits", &params)
            .await?;
        let (total, buckets) = parse_hits(&response, options.breakdown_field.as_deref())?;
        Ok((known_total.unwrap_or(total), buckets, Some(step)))
    }

    async fn count_query(
        &self,
        query: &str,
        time_range: Option<crate::search::TimeRange>,
    ) -> Result<u64, VictoriaLogsError> {
        let count_query = format!("{query} | stats count() as _ig_count");
        if count_query.len() > MAX_QUERY_BYTES {
            return Err(VictoriaLogsError::InvalidQuery);
        }
        let mut params = vec![
            ("query".to_owned(), count_query),
            ("timeout".to_owned(), SEARCH_TIMEOUT.to_owned()),
        ];
        add_time_range(&mut params, time_range)?;
        let body = self.post_form("select/logsql/query", &params).await?;
        let rows = parse_json_lines(&body)?;
        match rows.first().and_then(|row| row.get("_ig_count")) {
            Some(value) => parse_count(value),
            None => Ok(0),
        }
    }

    async fn all_time_stats(&self, query: &str) -> Result<Value, VictoriaLogsError> {
        let stats_query = format!(
            "{query} | stats count() as _ig_total, min(_time) as _ig_min_time, max(_time) as _ig_max_time"
        );
        if stats_query.len() > MAX_QUERY_BYTES {
            return Err(VictoriaLogsError::InvalidQuery);
        }
        let body = self
            .post_form(
                "select/logsql/query",
                &[
                    ("query".to_owned(), stats_query),
                    ("timeout".to_owned(), SEARCH_TIMEOUT.to_owned()),
                ],
            )
            .await?;
        parse_json_lines(&body)?
            .into_iter()
            .next()
            .ok_or(VictoriaLogsError::InvalidResponse)
    }

    async fn post_json(
        &self,
        path: &str,
        params: &[(String, String)],
    ) -> Result<Value, VictoriaLogsError> {
        let body = self.post_form(path, params).await?;
        serde_json::from_slice(&body).map_err(|_| VictoriaLogsError::InvalidResponse)
    }

    async fn post_form(
        &self,
        path: &str,
        params: &[(String, String)],
    ) -> Result<Vec<u8>, VictoriaLogsError> {
        let mut request = self
            .client
            .post(self.endpoint(path)?)
            .form(params)
            .header(reqwest::header::ACCEPT, "application/json");
        if let (Some(username), Some(password)) =
            (self.username.as_deref(), self.password.as_deref())
        {
            request = request.basic_auth(username, Some(password));
        }
        if let Some(account_id) = self.account_id.as_deref() {
            request = request.header("AccountID", account_id);
        }
        if let Some(project_id) = self.project_id.as_deref() {
            request = request.header("ProjectID", project_id);
        }
        let response = request.send().await.map_err(|_| VictoriaLogsError::Request)?;
        read_bounded(response).await
    }

    fn endpoint(&self, path: &str) -> Result<Url, VictoriaLogsError> {
        let mut endpoint = self.base_url.clone();
        let mut segments = endpoint
            .path_segments_mut()
            .map_err(|_| VictoriaLogsError::InvalidUrl)?;
        segments.pop_if_empty();
        for segment in path.split('/') {
            segments.push(segment);
        }
        drop(segments);
        Ok(endpoint)
    }
}

fn validate_index(index: &str) -> Result<(), VictoriaLogsError> {
    if index == "logs" {
        Ok(())
    } else {
        Err(VictoriaLogsError::InvalidQuery)
    }
}

fn cursor_offset(cursor: Option<&str>, from: usize) -> Result<u64, VictoriaLogsError> {
    match cursor {
        None if from <= 10_000 => Ok(from as u64),
        Some("") => Ok(0),
        Some(token) if token.len() <= 19 && token.bytes().all(|byte| byte.is_ascii_digit()) => {
            token
                .parse::<u64>()
                .ok()
                .filter(|offset| *offset <= MAX_CURSOR_OFFSET)
                .ok_or(VictoriaLogsError::InvalidQuery)
        }
        _ => Err(VictoriaLogsError::InvalidQuery),
    }
}

fn add_time_range(
    params: &mut Vec<(String, String)>,
    range: Option<crate::search::TimeRange>,
) -> Result<(), VictoriaLogsError> {
    if let Some(range) = range {
        if !range.is_valid() {
            return Err(VictoriaLogsError::InvalidQuery);
        }
        let exclusive_end = range
            .end
            .checked_add(1)
            .ok_or(VictoriaLogsError::InvalidQuery)?;
        params.push(("start".to_owned(), format_timestamp(range.start)?));
        params.push(("end".to_owned(), format_timestamp(exclusive_end)?));
    }
    Ok(())
}

fn format_timestamp(timestamp: i64) -> Result<String, VictoriaLogsError> {
    DateTime::<Utc>::from_timestamp_millis(timestamp)
        .map(|value| value.to_rfc3339_opts(SecondsFormat::Millis, true))
        .ok_or(VictoriaLogsError::InvalidQuery)
}

async fn read_bounded(response: Response) -> Result<Vec<u8>, VictoriaLogsError> {
    let status = response.status();
    if !status.is_success() {
        return Err(VictoriaLogsError::Rejected {
            status: status.as_u16(),
        });
    }
    if response
        .content_length()
        .is_some_and(|length| length > MAX_RESPONSE_BYTES as u64)
    {
        return Err(VictoriaLogsError::ResponseTooLarge);
    }
    let mut response = response;
    let mut body = Vec::new();
    while let Some(chunk) = response
        .chunk()
        .await
        .map_err(|_| VictoriaLogsError::Request)?
    {
        if body.len().saturating_add(chunk.len()) > MAX_RESPONSE_BYTES {
            return Err(VictoriaLogsError::ResponseTooLarge);
        }
        body.extend_from_slice(&chunk);
    }
    Ok(body)
}

fn parse_json_lines(body: &[u8]) -> Result<Vec<Value>, VictoriaLogsError> {
    serde_json::Deserializer::from_slice(body)
        .into_iter::<Value>()
        .collect::<Result<Vec<_>, _>>()
        .map_err(|_| VictoriaLogsError::InvalidResponse)
}

fn parse_count(value: &Value) -> Result<u64, VictoriaLogsError> {
    value
        .as_u64()
        .or_else(|| value.as_str()?.parse::<u64>().ok())
        .ok_or(VictoriaLogsError::InvalidResponse)
}

fn parse_timestamp(value: &Value) -> Result<i64, VictoriaLogsError> {
    let timestamp = value.as_str().ok_or(VictoriaLogsError::InvalidResponse)?;
    DateTime::parse_from_rfc3339(timestamp)
        .map(|time| time.timestamp_millis())
        .map_err(|_| VictoriaLogsError::InvalidResponse)
}

fn parse_hits(
    response: &Value,
    breakdown_field: Option<&str>,
) -> Result<(u64, Vec<HistogramBucket>), VictoriaLogsError> {
    let series = response
        .get("hits")
        .and_then(Value::as_array)
        .ok_or(VictoriaLogsError::InvalidResponse)?;
    let mut total = 0u64;
    let mut buckets = BTreeMap::<i64, BucketCounts>::new();
    let mut key_totals = BTreeMap::<String, u64>::new();
    for series in series {
        if let Some(count) = series.get("total") {
            total = total.saturating_add(parse_count(count)?);
        }
        let timestamps = series
            .get("timestamps")
            .and_then(Value::as_array)
            .ok_or(VictoriaLogsError::InvalidResponse)?;
        let values = series
            .get("values")
            .and_then(Value::as_array)
            .ok_or(VictoriaLogsError::InvalidResponse)?;
        if timestamps.len() != values.len() {
            return Err(VictoriaLogsError::InvalidResponse);
        }
        let fields = series.get("fields").and_then(Value::as_object);
        let key = breakdown_field.and_then(|field| fields.and_then(|fields| fields.get(field)));
        let key = key.filter(|value| !value.is_null()).map(|value| match value {
            Value::String(text) => text.clone(),
            other => other.to_string(),
        });
        for (timestamp, value) in timestamps.iter().zip(values) {
            let timestamp = parse_timestamp(timestamp)?;
            let count = parse_count(value)?;
            let bucket = buckets.entry(timestamp).or_default();
            bucket.count = bucket.count.saturating_add(count);
            if breakdown_field.is_some() {
                if let Some(key) = key.as_ref() {
                    *bucket.breakdown.entry(key.clone()).or_default() += count;
                    *key_totals.entry(key.clone()).or_default() += count;
                } else {
                    bucket.other = bucket.other.saturating_add(count);
                }
            }
        }
    }

    let mut top_keys = key_totals.into_iter().collect::<Vec<_>>();
    top_keys.sort_by(|left, right| right.1.cmp(&left.1).then_with(|| left.0.cmp(&right.0)));
    top_keys.truncate(MAX_BREAKDOWN_VALUES);
    let buckets = buckets
        .into_iter()
        .map(|(timestamp, counts)| {
            let breakdown = breakdown_field.map(|_| {
                let mut values = top_keys
                    .iter()
                    .map(|(key, _)| BreakdownCount {
                        key: key.clone(),
                        count: counts.breakdown.get(key).copied().unwrap_or(0),
                    })
                    .collect::<Vec<_>>();
                let listed = values.iter().map(|value| value.count).sum::<u64>();
                values.push(BreakdownCount {
                    key: "__other__".to_owned(),
                    count: counts.count.saturating_sub(listed),
                });
                values
            });
            HistogramBucket {
                timestamp,
                label: None,
                count: counts.count,
                breakdown,
            }
        })
        .collect();
    Ok((total, buckets))
}

fn histogram_error_reason(error: &VictoriaLogsError) -> &'static str {
    match error {
        VictoriaLogsError::InvalidQuery => "VictoriaLogs histograms currently support UTC only.",
        _ => "VictoriaLogs could not compute the histogram for this query.",
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::data_connectors::Connector;
    use wiremock::{matchers, Mock, MockServer, ResponseTemplate};

    fn client(url: &str) -> VictoriaLogsClient {
        VictoriaLogsClient::new(VictoriaLogsSettings {
            url: Url::parse(url).unwrap(),
            username: None,
            password: None,
            account_id: None,
            project_id: None,
        })
        .unwrap()
    }

    #[test]
    fn connection_settings_reject_unsafe_urls_and_malformed_tenants() {
        for raw_url in [
            "http://user:secret@localhost:9428",
            "http://localhost:9428?secret=value",
            "http://localhost:9428#fragment",
            "file:///tmp/vlogs",
        ] {
            let settings = VictoriaLogsSettings {
                url: Url::parse(raw_url).unwrap(),
                username: None,
                password: None,
                account_id: None,
                project_id: None,
            };
            assert!(VictoriaLogsClient::new(settings).is_err());
        }
        let mut settings = VictoriaLogsSettings {
            url: Url::parse("http://localhost:9428").unwrap(),
            username: Some("user".to_owned()),
            password: None,
            account_id: None,
            project_id: None,
        };
        assert!(VictoriaLogsClient::new(settings.clone()).is_err());
        settings.username = None;
        settings.project_id = Some("-1".to_owned());
        assert!(VictoriaLogsClient::new(settings).is_err());
    }

    #[test]
    fn descriptor_uses_inline_logs_and_shared_explore_contract() {
        let descriptor = descriptor();
        assert_eq!(descriptor.id, "victorialogs");
        assert_eq!(descriptor.query["document_mode"], "inline");
        assert_eq!(descriptor.query["schema_label"], "VictoriaLogs fields");
        assert!(descriptor
            .connection_fields
            .iter()
            .any(|field| field["name"] == "url" && field["required"] == true));
    }

    #[test]
    fn hits_response_maps_totals_buckets_and_breakdown_to_shared_shape() {
        let response = json!({"hits":[
            {"fields":{"level":"error"},"timestamps":["2026-10-22T10:00:00Z"],"values":[3],"total":3},
            {"fields":{"level":"info"},"timestamps":["2026-10-22T10:00:00Z"],"values":[4],"total":4},
            {"fields":{},"timestamps":["2026-10-22T10:00:00Z"],"values":[2],"total":2}
        ]});
        let (total, buckets) = parse_hits(&response, Some("level")).unwrap();
        assert_eq!(total, 9);
        assert_eq!(buckets.len(), 1);
        assert_eq!(buckets[0].count, 9);
        assert_eq!(
            serde_json::to_value(&buckets[0]).unwrap()["breakdown"],
            json!([
                {"key":"info","count":4},
                {"key":"error","count":3},
                {"key":"__other__","count":2}
            ])
        );
    }

    #[test]
    fn time_ranges_use_rfc3339_and_preserve_inclusive_millisecond_endpoints() {
        let mut params = Vec::new();
        add_time_range(
            &mut params,
            Some(crate::search::TimeRange { start: 0, end: 0 }),
        )
        .unwrap();
        assert_eq!(
            params,
            [
                (
                    "start".to_owned(),
                    "1970-01-01T00:00:00.000Z".to_owned()
                ),
                (
                    "end".to_owned(),
                    "1970-01-01T00:00:00.001Z".to_owned()
                )
            ]
        );
    }

    #[tokio::test]
    async fn search_returns_bounded_jsonl_hits_exact_total_and_histogram() {
        let server = MockServer::start().await;
        Mock::given(matchers::method("POST"))
            .and(matchers::path("/select/logsql/query"))
            .and(|request: &wiremock::Request| {
                String::from_utf8_lossy(&request.body).contains("limit=3")
            })
            .respond_with(ResponseTemplate::new(200).set_body_raw(
                "{\"_time\":\"2026-10-22T10:00:00Z\",\"_msg\":\"first\"}\n",
                "application/x-ndjson",
            ))
            .expect(1)
            .mount(&server)
            .await;
        Mock::given(matchers::method("POST"))
            .and(matchers::path("/select/logsql/hits"))
            .and(|request: &wiremock::Request| {
                String::from_utf8_lossy(&request.body).contains("step=30s")
            })
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({"hits":[{
                "fields":{},"timestamps":["2026-10-22T10:00:00Z"],"values":[1],"total":1
            }]})))
            .expect(1)
            .mount(&server)
            .await;

        let client = client(&server.uri());
        let request = serde_json::from_value(json!({
            "index":"logs","query":"error","size":2,"cursor":"",
            "histogram_field":"_time","time_range":{"start":1792663200000i64,"end":1792663500000i64}
        }))
        .unwrap();
        let result = Connector::search(&client, request).await.unwrap();
        assert_eq!(result["total"], 1);
        assert_eq!(result["total_relation"], "eq");
        assert_eq!(result["hits"].as_array().unwrap().len(), 1);
        assert_eq!(result["hits"][0]["index"], "logs");
        assert_eq!(result["hits"][0]["id"], "row-1");
        assert_eq!(result["histogram"][0]["count"], 1);
        assert_eq!(result["histogram_interval"], "30s");
        assert!(result.get("next_cursor").is_none());
    }

    #[tokio::test]
    async fn continuation_uses_offset_and_does_not_repeat_histogram() {
        let server = MockServer::start().await;
        Mock::given(matchers::method("POST"))
            .and(matchers::path("/select/logsql/query"))
            .and(|request: &wiremock::Request| {
                String::from_utf8_lossy(&request.body).contains("offset=2")
            })
            .respond_with(ResponseTemplate::new(200).set_body_raw(
                "{\"_time\":\"2026-10-22T09:00:00Z\",\"_msg\":\"last\"}\n",
                "application/x-ndjson",
            ))
            .expect(1)
            .mount(&server)
            .await;
        let client = client(&server.uri());
        let request = serde_json::from_value(json!({
            "index":"logs","query":"*","size":2,"cursor":"2",
            "histogram_field":"_time"
        }))
        .unwrap();
        let result = Connector::search(&client, request).await.unwrap();
        assert_eq!(result["total"], 3);
        assert_eq!(result["total_relation"], "eq");
        assert_eq!(result["hits"][0]["id"], "row-3");
        assert_eq!(result["histogram"], json!([]));
        assert!(server.received_requests().await.unwrap().iter().all(|request| {
            !request.url.path().ends_with("/hits")
        }));
    }

    #[tokio::test]
    async fn failed_histogram_and_count_keep_page_total_as_lower_bound() {
        let server = MockServer::start().await;
        Mock::given(matchers::method("POST"))
            .and(matchers::path("/select/logsql/query"))
            .and(|request: &wiremock::Request| {
                String::from_utf8_lossy(&request.body).contains("limit=3")
            })
            .respond_with(ResponseTemplate::new(200).set_body_raw(
                "{\"_time\":\"2026-10-22T10:00:00Z\",\"_msg\":\"first\"}\n",
                "application/x-ndjson",
            ))
            .expect(1)
            .mount(&server)
            .await;
        Mock::given(matchers::method("POST"))
            .and(matchers::path("/select/logsql/hits"))
            .respond_with(ResponseTemplate::new(500))
            .expect(1)
            .mount(&server)
            .await;
        Mock::given(matchers::method("POST"))
            .and(matchers::path("/select/logsql/query"))
            .and(|request: &wiremock::Request| {
                String::from_utf8_lossy(&request.body).contains("_ig_count")
            })
            .respond_with(ResponseTemplate::new(500))
            .expect(1)
            .mount(&server)
            .await;

        let client = client(&server.uri());
        let request = serde_json::from_value(json!({
            "index":"logs","query":"error","size":2,"cursor":"",
            "histogram_field":"_time","time_range":{"start":1792663200000i64,"end":1792663500000i64}
        }))
        .unwrap();
        let result = Connector::search(&client, request).await.unwrap();
        assert_eq!(result["total"], 1);
        assert_eq!(result["total_relation"], "gte");
        assert_eq!(result["histogram"], json!([]));
        assert!(result["histogram_unavailable_reason"].is_string());
    }
}
