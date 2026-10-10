use serde::Deserialize;

#[derive(Debug, Deserialize)]
pub struct SearchRequest {
    pub index: String,
    #[serde(default)]
    pub query: String,
    #[serde(default)]
    pub from: usize,
    #[serde(default = "default_page_size")]
    pub size: usize,
    pub sort_by: Option<String>,
    pub sort_order: Option<String>,
    pub histogram_field: Option<String>,
    pub time_range: Option<TimeRange>,
}

fn default_page_size() -> usize {
    50
}

#[derive(Clone, Copy, Debug, Deserialize)]
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
    use super::TimeRange;

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
