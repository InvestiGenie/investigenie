use crate::investigation_dialect::{
    FilterOperator, FilterPredicate, InvestigationDialect, TranslationError,
};

struct PromQlDialect;

pub fn dialect() -> &'static dyn InvestigationDialect {
    &PromQlDialect
}

const LABEL_OPERATORS: &[FilterOperator] = &[FilterOperator::Eq, FilterOperator::Contains];
const VALUE_OPERATORS: &[FilterOperator] = &[
    FilterOperator::Eq,
    FilterOperator::Gt,
    FilterOperator::Gte,
    FilterOperator::Lt,
    FilterOperator::Lte,
];

impl InvestigationDialect for PromQlDialect {
    fn operators(&self) -> &'static [FilterOperator] {
        &FilterOperator::ALL
    }

    /// Field types come from `PrometheusClient::fields`: series labels, `_value`, and `_time`.
    fn field_operators(&self, field_type: &str) -> &'static [FilterOperator] {
        match field_type {
            "label" => LABEL_OPERATORS,
            "sample value" => VALUE_OPERATORS,
            _ => &[],
        }
    }

    fn aggregatable(&self, field_type: &str) -> bool {
        field_type == "label"
    }

    fn check(&self, filter: &FilterPredicate) -> Result<(), TranslationError> {
        match filter.field_type.as_str() {
            "label" if valid_label_name(&filter.field) => Ok(()),
            "sample value" if filter.field == "_value" => {
                number_literal(&filter.value.text()).map(|_| ())
            }
            _ => Err(TranslationError("Unsupported Mimir field")),
        }
    }

    /// Label filters form one selector; `_value` comparisons then keep only matching samples.
    /// PromQL rejects selectors whose matchers all accept an empty value, so those get the
    /// match-all `__name__!=""` anchor that the Explore default query also uses.
    fn translate(&self, _: &str, filters: &[FilterPredicate]) -> Result<String, TranslationError> {
        let mut matchers = Vec::new();
        let mut comparisons = Vec::new();
        let mut anchored = false;
        for filter in filters {
            let value = filter.value.text();
            match (filter.field_type.as_str(), filter.operator) {
                ("label", FilterOperator::Eq) => {
                    matchers.push(format!("{}={}", filter.field, string_literal(&value)));
                }
                ("label", FilterOperator::Contains) => matchers.push(format!(
                    "{}=~{}",
                    filter.field,
                    string_literal(&format!(".*{}.*", quote_meta(&value)))
                )),
                ("sample value", operator) => {
                    comparisons.push((comparison(operator)?, number_literal(&value)?));
                    continue;
                }
                _ => return Err(TranslationError("Unsupported Mimir field")),
            }
            anchored |= !value.is_empty();
        }
        if !anchored {
            matchers.push("__name__!=\"\"".to_owned());
        }
        let mut query = format!("{{{}}}", matchers.join(", "));
        for (operator, literal) in comparisons {
            query = format!("({query}) {operator} {literal}");
        }
        Ok(query)
    }
}

fn comparison(operator: FilterOperator) -> Result<&'static str, TranslationError> {
    Ok(match operator {
        FilterOperator::Eq => "==",
        FilterOperator::Gt => ">",
        FilterOperator::Gte => ">=",
        FilterOperator::Lt => "<",
        FilterOperator::Lte => "<=",
        FilterOperator::Contains => return Err(TranslationError("Unsupported filter operator")),
    })
}

fn number_literal(text: &str) -> Result<String, TranslationError> {
    let text = text.trim();
    if !text.is_empty()
        && text.len() <= 32
        && text
            .bytes()
            .all(|byte| byte.is_ascii_digit() || b".-+eE".contains(&byte))
        && text.parse::<f64>().is_ok_and(f64::is_finite)
    {
        Ok(text.to_owned())
    } else {
        Err(TranslationError("Sample value filters need a number"))
    }
}

fn valid_label_name(name: &str) -> bool {
    let mut bytes = name.bytes();
    !matches!(name, "_time" | "_value")
        && bytes
            .next()
            .is_some_and(|first| first.is_ascii_alphabetic() || first == b'_')
        && bytes.all(|byte| byte.is_ascii_alphanumeric() || byte == b'_')
}

/// A PromQL double-quoted string; JSON escaping is a subset of the Go escapes PromQL accepts.
fn string_literal(value: &str) -> String {
    serde_json::Value::from(value).to_string()
}

/// Escapes RE2 metacharacters, matching Go's `regexp.QuoteMeta`.
fn quote_meta(value: &str) -> String {
    let mut quoted = String::with_capacity(value.len());
    for character in value.chars() {
        if r"\.+*?()|[]{}^$".contains(character) {
            quoted.push('\\');
        }
        quoted.push(character);
    }
    quoted
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::investigation_dialect::{build_query, FilterValue};

    fn filter(
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

    fn text(value: &str) -> FilterValue {
        FilterValue::String(value.to_owned())
    }

    #[test]
    fn labels_select_series_and_value_comparisons_filter_samples() {
        let filters = [
            filter("__name__", "label", FilterOperator::Eq, text("http_requests_total")),
            filter("handler", "label", FilterOperator::Contains, text("api/v1 (x)")),
            filter("_value", "sample value", FilterOperator::Gt, FilterValue::Number(1.into())),
            filter("_value", "sample value", FilterOperator::Lte, text("2.5e3")),
        ];
        assert_eq!(
            build_query(dialect(), "metrics", &filters).unwrap(),
            r#"(({__name__="http_requests_total", handler=~".*api/v1 \\(x\\).*"}) > 1) <= 2.5e3"#
        );
    }

    #[test]
    fn selectors_that_only_accept_empty_values_are_anchored() {
        assert_eq!(build_query(dialect(), "metrics", &[]).unwrap(), r#"{__name__!=""}"#);
        let filters = [
            filter("job", "label", FilterOperator::Eq, text("")),
            filter("_value", "sample value", FilterOperator::Eq, text("0")),
        ];
        assert_eq!(
            build_query(dialect(), "metrics", &filters).unwrap(),
            r#"({job="", __name__!=""}) == 0"#
        );
        let quoted = [filter("job", "label", FilterOperator::Eq, text("say \"hi\" \\"))];
        assert_eq!(
            build_query(dialect(), "metrics", &quoted).unwrap(),
            r#"{job="say \"hi\" \\"}"#
        );
    }

    #[test]
    fn unsupported_fields_and_values_are_rejected() {
        for rejected in [
            filter("_time", "timestamp", FilterOperator::Gt, text("1")),
            filter("_value", "sample value", FilterOperator::Gt, text("NaN")),
            filter("_value", "sample value", FilterOperator::Gt, text("1 or vector(1)")),
            filter("_value", "sample value", FilterOperator::Contains, text("1")),
            filter("job", "label", FilterOperator::Gt, text("1")),
            filter("_value", "label", FilterOperator::Eq, text("1")),
            filter("service.name", "label", FilterOperator::Eq, text("x")),
        ] {
            assert!(build_query(dialect(), "metrics", &[rejected]).is_err());
        }
        assert!(dialect().aggregatable("label"));
        assert!(!dialect().aggregatable("sample value"));
    }
}
