//! The language-neutral computation every metric-capable dialect reads and writes.
//!
//! Records matching `filter` are grouped by `group_by` into time buckets `(t - range, t]`,
//! labelled by their end `t` as PromQL and LogQL evaluate range functions, and reduced by
//! `function`. Records without a group field fall into a group without that field. Value
//! functions use only records whose field is a number; `Avg` is the exact mean of those records,
//! not a mean of per-series means.

use super::{Expr, Field, TranslationError};
use crate::search::TimeRange;

#[derive(Clone, Debug, PartialEq)]
pub enum RangeFunction {
    Count,
    /// Records per second: `Count` divided by the range in seconds.
    Rate,
    Sum(Field),
    Avg(Field),
    Min(Field),
    Max(Field),
}

impl RangeFunction {
    pub fn value_field(&self) -> Option<&Field> {
        match self {
            Self::Count | Self::Rate => None,
            Self::Sum(field) | Self::Avg(field) | Self::Min(field) | Self::Max(field) => {
                Some(field)
            }
        }
    }

    pub fn value_field_mut(&mut self) -> Option<&mut Field> {
        match self {
            Self::Count | Self::Rate => None,
            Self::Sum(field) | Self::Avg(field) | Self::Min(field) | Self::Max(field) => {
                Some(field)
            }
        }
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct MetricQuery {
    pub function: RangeFunction,
    pub range_ms: i64,
    pub group_by: Vec<Field>,
    pub filter: Expr,
}

/// The bucket ends a computation evaluates inside `range`: multiples of `step` since the epoch,
/// from the first to the last one inside it. `None` when the range holds no bucket end.
pub fn bucket_ends(range: TimeRange, step: i64) -> Option<TimeRange> {
    if step <= 0 {
        return None;
    }
    let first = range.start.div_euclid(step) * step
        + if range.start.rem_euclid(step) == 0 {
            0
        } else {
            step
        };
    let last = range.end.div_euclid(step) * step;
    (first <= last).then_some(TimeRange {
        start: first,
        end: last,
    })
}

/// Ranges from one second to one week keep buckets meaningful and queries bounded.
pub const MIN_RANGE_MS: i64 = 1_000;
pub const MAX_RANGE_MS: i64 = 7 * 24 * 3_600_000;
pub const MAX_GROUP_FIELDS: usize = 8;

impl MetricQuery {
    pub fn validate(&self) -> Result<(), TranslationError> {
        if !(MIN_RANGE_MS..=MAX_RANGE_MS).contains(&self.range_ms) || self.range_ms % 1_000 != 0 {
            return Err(TranslationError(
                "the range must be whole seconds between 1s and 1w",
            ));
        }
        if self.group_by.len() > MAX_GROUP_FIELDS {
            return Err(TranslationError("too many group-by fields"));
        }
        self.filter.check_size()
    }
}

/// A Prometheus-style duration such as `5m`, `1h30m`, or `90s`, in milliseconds.
pub fn parse_duration(text: &str) -> Option<i64> {
    let mut rest = text;
    let mut total = 0i64;
    if rest.is_empty() {
        return None;
    }
    while !rest.is_empty() {
        let digits = rest.find(|character: char| !character.is_ascii_digit())?;
        if digits == 0 {
            return None;
        }
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
    Some(total)
}

/// The shortest exact spelling, such as `1h30m` or `45s`.
pub fn format_duration(millis: i64) -> String {
    let mut rest = millis;
    let mut text = String::new();
    for (unit, size) in [
        ("d", 86_400_000),
        ("h", 3_600_000),
        ("m", 60_000),
        ("s", 1_000),
        ("ms", 1),
    ] {
        if rest >= size {
            text.push_str(&format!("{}{unit}", rest / size));
            rest %= size;
        }
    }
    if text.is_empty() {
        "0s".to_owned()
    } else {
        text
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn durations_round_trip() {
        for (text, millis) in [
            ("5m", 300_000),
            ("1h30m", 5_400_000),
            ("90s", 90_000),
            ("1d", 86_400_000),
            ("1w", 604_800_000),
        ] {
            assert_eq!(parse_duration(text), Some(millis), "{text}");
            assert_eq!(
                parse_duration(&format_duration(millis)),
                Some(millis),
                "{text}"
            );
        }
        assert_eq!(format_duration(90_000), "1m30s");
        for text in ["", "m", "5", "1.5m", "5x"] {
            assert_eq!(parse_duration(text), None, "{text}");
        }
    }
}
