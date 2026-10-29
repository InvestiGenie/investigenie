//! Contract for translating AI-planned investigation filters into a datasource's native query.
//!
//! A data connector supports AI investigations by adding `investigation.rs` beside its `mod.rs`
//! with `pub fn dialect() -> &'static dyn InvestigationDialect`. `backend/build.rs` discovers the
//! file, registers the dialect, and derives the descriptor's `ai_investigation` flag from it.

use serde_json::{Number, Value};

use crate::search::safe_field_name;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FilterOperator {
    Eq,
    Contains,
    Gt,
    Gte,
    Lt,
    Lte,
}

impl FilterOperator {
    pub const ALL: [Self; 6] = [
        Self::Eq,
        Self::Contains,
        Self::Gt,
        Self::Gte,
        Self::Lt,
        Self::Lte,
    ];

    /// Parses the operator spellings that models commonly produce.
    pub fn from_label(label: &str) -> Option<Self> {
        Some(match label.trim().to_ascii_lowercase().as_str() {
            "eq" | "=" | "==" | "equals" | "equal" | "is" | "term" | "match" => Self::Eq,
            "contains" | "like" | "includes" | "substring" => Self::Contains,
            "gt" | ">" | "greater_than" | "after" => Self::Gt,
            "gte" | ">=" | "greater_than_or_equal" | "at_least" => Self::Gte,
            "lt" | "<" | "less_than" | "before" => Self::Lt,
            "lte" | "<=" | "less_than_or_equal" | "at_most" => Self::Lte,
            _ => return None,
        })
    }

    /// The canonical label offered to the planner.
    pub fn label(self) -> &'static str {
        match self {
            Self::Eq => "eq",
            Self::Contains => "contains",
            Self::Gt => "gt",
            Self::Gte => "gte",
            Self::Lt => "lt",
            Self::Lte => "lte",
        }
    }
}

/// A scalar filter value; arrays, objects, and null never reach a translator.
#[derive(Clone, Debug, PartialEq)]
pub enum FilterValue {
    String(String),
    Number(Number),
    Bool(bool),
}

impl FilterValue {
    pub fn from_json(value: &Value) -> Option<Self> {
        match value {
            Value::String(value) => Some(Self::String(value.clone())),
            Value::Number(value) => Some(Self::Number(value.clone())),
            Value::Bool(value) => Some(Self::Bool(*value)),
            _ => None,
        }
    }

    /// The raw text: strings unquoted, numbers and booleans in their JSON form.
    pub fn text(&self) -> String {
        match self {
            Self::String(value) => value.clone(),
            Self::Number(value) => value.to_string(),
            Self::Bool(value) => value.to_string(),
        }
    }
}

#[derive(Debug)]
pub struct FilterPredicate {
    pub field: String,
    /// The type the source reported for `field`, as listed in the planner catalog.
    pub field_type: String,
    pub operator: FilterOperator,
    pub value: FilterValue,
}

/// A sanitized translation failure that is safe to show to the user.
#[derive(Debug, PartialEq, Eq)]
pub struct TranslationError(pub &'static str);

pub trait InvestigationDialect: Send + Sync {
    /// Operators the planner may use, in the order they are offered to the model.
    fn operators(&self) -> &'static [FilterOperator];
    /// Operators valid for one source-reported field type; a subset of `operators`.
    /// Sources whose fields all filter alike keep the default.
    fn field_operators(&self, _field_type: &str) -> &'static [FilterOperator] {
        self.operators()
    }
    /// Whether a field of this source-reported type can back a top-values breakdown.
    fn aggregatable(&self, field_type: &str) -> bool;
    /// Source-specific checks of one filter, such as a numeric value for a numeric comparison.
    /// Plan sanitization drops a filter that fails here, so `translate` can then rely on it.
    fn check(&self, _filter: &FilterPredicate) -> Result<(), TranslationError> {
        Ok(())
    }
    /// Translates AND-combined filters that `validate` has already accepted.
    fn translate(
        &self,
        index: &str,
        filters: &[FilterPredicate],
    ) -> Result<String, TranslationError>;
}

/// The checks every dialect gets: a safe field name, an operator valid for the field's type, and
/// the dialect's own `check`.
pub fn validate(
    dialect: &dyn InvestigationDialect,
    filter: &FilterPredicate,
) -> Result<(), TranslationError> {
    if !safe_field_name(&filter.field) {
        return Err(TranslationError("Unsupported filter field"));
    }
    if !dialect
        .field_operators(&filter.field_type)
        .contains(&filter.operator)
    {
        return Err(TranslationError("Unsupported filter operator"));
    }
    dialect.check(filter)
}

/// Validates every filter, then translates.
pub fn build_query(
    dialect: &dyn InvestigationDialect,
    index: &str,
    filters: &[FilterPredicate],
) -> Result<String, TranslationError> {
    for filter in filters {
        validate(dialect, filter)?;
    }
    dialect.translate(index, filters)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::data_connectors::{investigation_dialect, registered};

    fn predicate(
        field: &str,
        field_type: &str,
        operator: FilterOperator,
        value: FilterValue,
    ) -> FilterPredicate {
        FilterPredicate {
            field: field.to_owned(),
            field_type: field_type.to_owned(),
            operator,
            value,
        }
    }

    /// Field types the built-in sources report; dialects that ignore types treat them alike.
    const FIELD_TYPES: [&str; 13] = [
        "keyword",
        "long",
        "String",
        "UInt16",
        "label",
        "text",
        "timestamp",
        "sample value",
        "json string",
        "json int",
        "logfmt float",
        "json duration",
        "",
    ];

    /// Source-independent guarantees every registered dialect must keep.
    #[test]
    fn every_registered_dialect_meets_the_shared_contract() {
        let mut dialects = 0;
        for descriptor in registered() {
            let dialect = investigation_dialect(descriptor.id);
            assert_eq!(
                descriptor.ai_investigation,
                dialect.is_some(),
                "{}",
                descriptor.id
            );
            let Some(dialect) = dialect else { continue };
            dialects += 1;
            let id = descriptor.id;
            assert!(!dialect.operators().is_empty(), "{id}");
            assert!(build_query(dialect, "events", &[]).is_ok(), "{id}");
            for field_type in FIELD_TYPES {
                let operators = dialect.field_operators(field_type);
                assert!(
                    operators
                        .iter()
                        .all(|operator| dialect.operators().contains(operator)),
                    "{id} {field_type}"
                );
                for field in [
                    "", "a b", "a`b", "a'b", "a\"b", "a:b", "a
b",
                ] {
                    for &operator in operators {
                        let filter =
                            predicate(field, field_type, operator, FilterValue::Bool(true));
                        assert!(build_query(dialect, "events", &[filter]).is_err(), "{id}");
                    }
                }
                for operator in FilterOperator::ALL {
                    let values = [
                        "' OR 1=1 --",
                        "\" OR *",
                        "}) OR {",
                        "`; DROP",
                        "\\",
                        "%_*?",
                        "a
b",
                        "500",
                        "1.5s",
                    ]
                    .map(|value| FilterValue::String(value.to_owned()))
                    .into_iter()
                    .chain([FilterValue::Number(500.into()), FilterValue::Bool(true)]);
                    for value in values {
                        let filter = predicate("status", field_type, operator, value);
                        let valid = validate(dialect, &filter);
                        if !operators.contains(&operator) {
                            assert!(valid.is_err(), "{id} {field_type} {operator:?}");
                        }
                        // A filter that validates must always translate.
                        assert_eq!(
                            build_query(dialect, "events", &[filter]).is_ok(),
                            valid.is_ok(),
                            "{id} {field_type} {operator:?}"
                        );
                    }
                }
            }
        }
        assert!(dialects > 0);
    }

    #[test]
    fn operator_labels_round_trip() {
        for operator in FilterOperator::ALL {
            assert_eq!(FilterOperator::from_label(operator.label()), Some(operator));
        }
        assert_eq!(
            FilterOperator::from_label(" LIKE "),
            Some(FilterOperator::Contains)
        );
        assert_eq!(FilterOperator::from_label("between"), None);
    }

    #[test]
    fn filter_values_are_scalar() {
        assert_eq!(
            FilterValue::from_json(&serde_json::json!(500)).map(|value| value.text()),
            Some("500".to_owned())
        );
        for value in [Value::Null, serde_json::json!([1]), serde_json::json!({})] {
            assert!(FilterValue::from_json(&value).is_none());
        }
    }
}
