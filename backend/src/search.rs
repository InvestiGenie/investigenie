use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine};
use serde::{Deserialize, Serialize};
use serde_json::Value;

pub const MAX_HISTOGRAM_BUCKETS: i64 = 1_000;
pub const MAX_BREAKDOWN_VALUES: usize = 3;
pub const BREAKDOWN_OTHER_KEY: &str = "__other__";
pub const SEARCH_PAGE_SIZE: usize = 50;
pub const MAX_SEARCH_RECORDS: usize = 100;
const MAX_SEARCH_CURSOR_BYTES: usize = 1024 * 1024;

#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct SearchPageCursor {
    offset: usize,
    native: String,
}

pub struct SearchWindow {
    offset: usize,
    size: usize,
}

impl SearchWindow {
    pub fn prepare(request: &mut SearchRequest) -> Result<Self, &'static str> {
        if request.size == 0 {
            return Err("Search page size must be positive");
        }
        let offset = match request
            .cursor
            .as_deref()
            .filter(|cursor| !cursor.is_empty())
        {
            Some(cursor) => {
                if cursor.len() > MAX_SEARCH_CURSOR_BYTES {
                    return Err("Invalid search cursor; start a new search");
                }
                let bytes = URL_SAFE_NO_PAD
                    .decode(cursor)
                    .map_err(|_| "Invalid search cursor; start a new search")?;
                let cursor: SearchPageCursor = serde_json::from_slice(&bytes)
                    .map_err(|_| "Invalid search cursor; start a new search")?;
                if cursor.offset == 0 || cursor.native.is_empty() {
                    return Err("Invalid search cursor; start a new search");
                }
                request.cursor = Some(cursor.native);
                request.from = 0;
                cursor.offset
            }
            None => {
                if request.cursor.is_some() && request.from != 0 {
                    return Err("A new cursor search must start at offset zero");
                }
                request.from
            }
        };
        if offset >= MAX_SEARCH_RECORDS {
            return Err("Only the first 100 records are available; narrow the search time range");
        }
        request.size = request
            .size
            .min(SEARCH_PAGE_SIZE)
            .min(MAX_SEARCH_RECORDS - offset);
        Ok(Self {
            offset,
            size: request.size,
        })
    }

    pub fn finish(self, result: &mut Value) {
        let returned = match result.get_mut("hits").and_then(Value::as_array_mut) {
            Some(hits) => {
                hits.truncate(self.size);
                hits.len()
            }
            None => 0,
        };
        let offset = self.offset + returned;
        result["next_cursor"] = match result.get("next_cursor").and_then(Value::as_str) {
            Some(native) if !native.is_empty() && returned > 0 && offset < MAX_SEARCH_RECORDS => {
                let cursor = SearchPageCursor {
                    offset,
                    native: native.to_owned(),
                };
                serde_json::to_vec(&cursor)
                    .map(|bytes| Value::String(URL_SAFE_NO_PAD.encode(bytes)))
                    .unwrap_or(Value::Null)
            }
            _ => Value::Null,
        };
    }
}

#[derive(Debug, Default, Deserialize)]
pub struct SearchRequest {
    pub index: String,
    #[serde(default)]
    pub query: String,
    #[serde(default)]
    pub from: usize,
    #[serde(default = "default_page_size")]
    pub size: usize,
    #[serde(default)]
    pub cursor: Option<String>,
    #[serde(default)]
    pub include_histogram: Option<bool>,
    pub sort_by: Option<String>,
    pub sort_order: Option<String>,
    pub histogram_field: Option<String>,
    pub time_range: Option<TimeRange>,
    /// Fixed (`30s`, `5m`, `1d`) or calendar (`1w`, `1M`, `1y`) bucket size; omitted means automatic.
    #[serde(default)]
    pub histogram_interval: Option<String>,
    #[serde(default)]
    pub histogram_breakdown_field: Option<String>,
    #[serde(default)]
    pub time_zone: Option<String>,
    #[serde(skip)]
    pub track_total_hits: Option<bool>,
}

fn default_page_size() -> usize {
    SEARCH_PAGE_SIZE
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum IntervalUnit {
    Millisecond,
    Second,
    Minute,
    Hour,
    Day,
    Week,
    Month,
    Year,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct HistogramInterval {
    pub count: u32,
    pub unit: IntervalUnit,
}

impl HistogramInterval {
    pub fn automatic(span_millis: f64) -> Option<Self> {
        if !span_millis.is_finite() || span_millis < 0.0 {
            return None;
        }
        [
            "1ms", "1s", "5s", "30s", "1m", "5m", "30m", "1h", "3h", "12h", "1d", "1w", "1M", "1y",
            "3650d", "36500d",
        ]
        .into_iter()
        .filter_map(Self::parse)
        .find(|interval| span_millis.max(1.0) / interval.approximate_millis() as f64 <= 50.0)
    }

    pub fn parse(value: &str) -> Option<Self> {
        let split = value.find(|character: char| !character.is_ascii_digit())?;
        let (digits, suffix) = value.split_at(split);
        let count: u32 = digits
            .parse()
            .ok()
            .filter(|count| (1..=100_000).contains(count))?;
        let unit = match suffix {
            "ms" => IntervalUnit::Millisecond,
            "s" => IntervalUnit::Second,
            "m" => IntervalUnit::Minute,
            "h" => IntervalUnit::Hour,
            "d" => IntervalUnit::Day,
            "w" => IntervalUnit::Week,
            "M" => IntervalUnit::Month,
            "y" => IntervalUnit::Year,
            _ => return None,
        };
        // Elasticsearch only supports single calendar units.
        if matches!(
            unit,
            IntervalUnit::Week | IntervalUnit::Month | IntervalUnit::Year
        ) && count != 1
        {
            return None;
        }
        Some(Self { count, unit })
    }

    pub fn is_calendar(&self) -> bool {
        matches!(
            self.unit,
            IntervalUnit::Week | IntervalUnit::Month | IntervalUnit::Year
        )
    }

    pub fn approximate_millis(&self) -> i64 {
        let unit = match self.unit {
            IntervalUnit::Millisecond => 1,
            IntervalUnit::Second => 1_000,
            IntervalUnit::Minute => 60_000,
            IntervalUnit::Hour => 3_600_000,
            IntervalUnit::Day => 86_400_000,
            IntervalUnit::Week => 604_800_000,
            IntervalUnit::Month => 2_592_000_000,
            IntervalUnit::Year => 31_536_000_000,
        };
        unit * i64::from(self.count)
    }

    pub fn suffix(&self) -> &'static str {
        match self.unit {
            IntervalUnit::Millisecond => "ms",
            IntervalUnit::Second => "s",
            IntervalUnit::Minute => "m",
            IntervalUnit::Hour => "h",
            IntervalUnit::Day => "d",
            IntervalUnit::Week => "w",
            IntervalUnit::Month => "M",
            IntervalUnit::Year => "y",
        }
    }

    pub fn clickhouse_unit(&self) -> &'static str {
        match self.unit {
            IntervalUnit::Millisecond => "MILLISECOND",
            IntervalUnit::Second => "SECOND",
            IntervalUnit::Minute => "MINUTE",
            IntervalUnit::Hour => "HOUR",
            IntervalUnit::Day => "DAY",
            IntervalUnit::Week => "WEEK",
            IntervalUnit::Month => "MONTH",
            IntervalUnit::Year => "YEAR",
        }
    }
}

#[derive(Clone, Debug, Default)]
pub struct HistogramOptions {
    pub interval: Option<HistogramInterval>,
    pub breakdown_field: Option<String>,
    pub time_zone: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct BreakdownCount {
    pub key: String,
    pub count: u64,
}

pub fn safe_field_name(field: &str) -> bool {
    !field.is_empty()
        && field.len() <= 256
        && field
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || b"._@-".contains(&byte))
}

fn safe_time_zone(zone: &str) -> bool {
    !zone.is_empty()
        && zone.len() <= 64
        && zone
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || b"/_+-".contains(&byte))
}

impl SearchRequest {
    /// Validates the optional histogram controls; an explicit interval needs a bounded time range.
    pub fn histogram_options(&self) -> Result<HistogramOptions, &'static str> {
        let time_zone = match self.time_zone.as_deref() {
            None => "UTC".to_owned(),
            Some(zone) if safe_time_zone(zone) => zone.to_owned(),
            Some(_) => return Err("Unsupported histogram timezone"),
        };
        let interval = match self.histogram_interval.as_deref() {
            None | Some("") | Some("auto") => None,
            Some(value) => {
                let interval =
                    HistogramInterval::parse(value).ok_or("Unsupported histogram interval")?;
                let range = self
                    .time_range
                    .filter(TimeRange::is_valid)
                    .ok_or("A fixed histogram interval requires a time range")?;
                let span = range.end.saturating_sub(range.start).max(1);
                if span / interval.approximate_millis() > MAX_HISTOGRAM_BUCKETS {
                    return Err("The histogram interval creates too many buckets");
                }
                Some(interval)
            }
        };
        let breakdown_field = match self.histogram_breakdown_field.as_deref() {
            None | Some("") => None,
            Some(field) if safe_field_name(field) => Some(field.to_owned()),
            Some(_) => return Err("Unsupported breakdown field"),
        };
        Ok(HistogramOptions {
            interval,
            breakdown_field,
            time_zone,
        })
    }
}

#[derive(Clone, Copy, Debug, Deserialize, PartialEq, Eq)]
pub struct TimeRange {
    pub start: i64,
    pub end: i64,
}

impl TimeRange {
    pub fn is_valid(&self) -> bool {
        let supported = -62_135_596_800_000..=253_402_300_799_999;
        self.start <= self.end && supported.contains(&self.start) && supported.contains(&self.end)
    }
}

#[cfg(test)]
mod tests {
    use super::{HistogramInterval, IntervalUnit, SearchRequest, TimeRange};
    use base64::Engine;

    #[test]
    fn search_window_caps_records_without_changing_totals_or_histograms() {
        let mut request = SearchRequest {
            size: 100,
            cursor: Some(String::new()),
            ..Default::default()
        };
        let first = super::SearchWindow::prepare(&mut request).unwrap();
        assert_eq!(request.size, 50);
        let hits = vec![serde_json::json!({"id":"sample"}); 50];
        let histogram = serde_json::json!([{"timestamp":1,"count":20_000}]);
        let mut result = serde_json::json!({"hits":hits,"total":20_000,"total_relation":"gte","histogram":histogram,"next_cursor":"native-page-two"});
        first.finish(&mut result);
        let cursor = result["next_cursor"].as_str().unwrap().to_owned();
        request.cursor = Some(cursor);
        let second = super::SearchWindow::prepare(&mut request).unwrap();
        assert_eq!(request.cursor.as_deref(), Some("native-page-two"));
        assert_eq!(request.size, 50);
        result["next_cursor"] = serde_json::json!("native-page-three");
        second.finish(&mut result);
        assert!(result["next_cursor"].is_null());
        assert_eq!(result["hits"].as_array().unwrap().len(), 50);
        assert_eq!(result["total"], 20_000);
        assert_eq!(result["total_relation"], "gte");
        assert_eq!(result["histogram"], histogram);
    }

    #[test]
    fn search_window_rejects_exhausted_or_invalid_requests_and_caps_offsets() {
        for request in [
            SearchRequest {
                size: 0,
                ..Default::default()
            },
            SearchRequest {
                size: 50,
                from: 100,
                ..Default::default()
            },
            SearchRequest {
                size: 50,
                cursor: Some("not-a-cursor".to_owned()),
                ..Default::default()
            },
            SearchRequest {
                size: 50,
                cursor: Some("a".repeat(super::MAX_SEARCH_CURSOR_BYTES + 1)),
                ..Default::default()
            },
            SearchRequest {
                size: 50,
                from: 50,
                cursor: Some(String::new()),
                ..Default::default()
            },
        ] {
            let mut request = request;
            assert!(super::SearchWindow::prepare(&mut request).is_err());
        }
        let cursor = super::SearchPageCursor {
            offset: 100,
            native: "third-page".to_owned(),
        };
        let mut request = SearchRequest {
            size: 50,
            cursor: Some(super::URL_SAFE_NO_PAD.encode(serde_json::to_vec(&cursor).unwrap())),
            ..Default::default()
        };
        assert!(super::SearchWindow::prepare(&mut request).is_err());
        request.cursor = None;
        request.from = 75;
        let window = super::SearchWindow::prepare(&mut request).unwrap();
        assert_eq!(request.size, 25);
        let mut result =
            serde_json::json!({"hits":vec![serde_json::json!({}); 50],"next_cursor":"more"});
        window.finish(&mut result);
        assert_eq!(result["hits"].as_array().unwrap().len(), 25);
        assert!(result["next_cursor"].is_null());
    }

    #[test]
    fn automatic_histogram_intervals_are_shared_bounded_and_valid() {
        for (span, expected) in [
            (0.0, "1ms"),
            (50_000.0, "1s"),
            (50_001.0, "5s"),
            (120_000.0, "5s"),
        ] {
            assert_eq!(
                HistogramInterval::automatic(span),
                HistogramInterval::parse(expected)
            );
        }
        for span in [-1.0, f64::NAN, f64::INFINITY, f64::MAX] {
            assert!(HistogramInterval::automatic(span).is_none());
        }
    }

    #[test]
    fn histogram_controls_validate_interval_range_and_identifiers() {
        assert_eq!(
            HistogramInterval::parse("30s"),
            Some(HistogramInterval {
                count: 30,
                unit: IntervalUnit::Second
            })
        );
        assert!(HistogramInterval::parse("1M").unwrap().is_calendar());
        assert!(HistogramInterval::parse("2w").is_none());
        assert!(HistogramInterval::parse("5x").is_none());
        assert!(HistogramInterval::parse("s").is_none());

        let request: SearchRequest = serde_json::from_value(serde_json::json!({
            "index": "logs", "histogram_interval": "1m", "time_range": {"start": 0, "end": 3_600_000},
            "histogram_breakdown_field": "service.name", "time_zone": "Europe/Berlin"
        }))
        .unwrap();
        let options = request.histogram_options().unwrap();
        assert_eq!(options.interval.unwrap().approximate_millis(), 60_000);
        assert_eq!(options.breakdown_field.as_deref(), Some("service.name"));
        assert_eq!(options.time_zone, "Europe/Berlin");

        for invalid in [
            serde_json::json!({"index":"logs","histogram_interval":"1m"}),
            serde_json::json!({"index":"logs","histogram_interval":"1ms","time_range":{"start":0,"end":86_400_000}}),
            serde_json::json!({"index":"logs","histogram_breakdown_field":"a` OR 1"}),
            serde_json::json!({"index":"logs","time_zone":"UTC'; DROP"}),
        ] {
            let request: SearchRequest = serde_json::from_value(invalid).unwrap();
            assert!(request.histogram_options().is_err());
        }
    }

    #[test]
    fn omitted_pagination_fields_preserve_legacy_defaults() {
        let request: SearchRequest = serde_json::from_str(r#"{"index":"logs"}"#).unwrap();
        assert!(request.cursor.is_none());
        assert_eq!(request.size, 50);
        assert!(request.include_histogram.unwrap_or(true));
        let request: SearchRequest =
            serde_json::from_str(r#"{"index":"logs","cursor":"","include_histogram":false}"#)
                .unwrap();
        assert_eq!(request.cursor.as_deref(), Some(""));
        assert_eq!(request.include_histogram, Some(false));
    }

    #[test]
    fn time_ranges_require_ordered_supported_timestamps() {
        assert!(TimeRange { start: 0, end: 1 }.is_valid());
        assert!(TimeRange { start: 0, end: 0 }.is_valid());
        assert!(!TimeRange { start: 1, end: 0 }.is_valid());
        assert!(!TimeRange {
            start: i64::MIN,
            end: 0
        }
        .is_valid());
        assert!(!TimeRange {
            start: 0,
            end: i64::MAX
        }
        .is_valid());
    }
}
