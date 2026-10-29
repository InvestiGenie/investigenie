use crate::investigation_dialect::{
    contains_regex, is_number, literal_value, read_contains_regex, read_string_literal,
    string_literal, FilterOperator, FilterPredicate, FilterValue, InvestigationDialect,
    ParsedFilter, TranslationError,
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

const UNSUPPORTED: TranslationError = TranslationError(
    "only metric{label=\"…\"} selectors with optional _value comparisons such as > 5 can be translated",
);

impl InvestigationDialect for PromQlDialect {
    fn language(&self) -> &'static str {
        "PromQL"
    }

    fn example(&self) -> &'static str {
        "http_requests_total{service=\"checkout\", code=~\".*5.*\"} > 0"
    }

    fn read(&self, query: &str) -> Result<Vec<ParsedFilter>, TranslationError> {
        let mut filters = Vec::new();
        if query.trim().is_empty() {
            return Ok(filters);
        }
        let rest = read_expression(query, &mut filters)?;
        if !rest.trim().is_empty() {
            return Err(UNSUPPORTED);
        }
        Ok(filters)
    }

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
                    string_literal(&contains_regex(&value))
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
    if is_number(text) {
        Ok(text.to_owned())
    } else {
        Err(TranslationError("Sample value filters need a number"))
    }
}

/// `expression := ( '(' expression ')' | selector ) { comparison number }`, where each
/// comparison filters samples by `_value`.
fn read_expression<'a>(
    input: &'a str,
    filters: &mut Vec<ParsedFilter>,
) -> Result<&'a str, TranslationError> {
    let input = input.trim_start();
    let mut rest = if let Some(inner) = input.strip_prefix('(') {
        read_expression(inner, filters)?
            .trim_start()
            .strip_prefix(')')
            .ok_or(UNSUPPORTED)?
    } else {
        read_selector(input, filters)?
    };
    loop {
        let trimmed = rest.trim_start();
        let Some((operator, after)) = ["==", ">=", "<=", "!=", ">", "<"]
            .iter()
            .find_map(|operator| trimmed.strip_prefix(operator).map(|after| (*operator, after)))
        else {
            return Ok(rest);
        };
        let operator = match operator {
            "==" => FilterOperator::Eq,
            ">" => FilterOperator::Gt,
            ">=" => FilterOperator::Gte,
            "<" => FilterOperator::Lt,
            "<=" => FilterOperator::Lte,
            _ => return Err(TranslationError("not-equal comparisons cannot be translated")),
        };
        let after = after.trim_start();
        let end = after
            .find(|character: char| !(character.is_ascii_alphanumeric() || matches!(character, '.' | '-' | '+')))
            .unwrap_or(after.len());
        let literal = &after[..end];
        if literal == "bool" || !is_number(literal) {
            return Err(TranslationError("comparisons are translated only against a number"));
        }
        filters.push(ParsedFilter::new("_value", operator, literal_value(literal)));
        rest = &after[end..];
    }
}

fn read_selector<'a>(
    input: &'a str,
    filters: &mut Vec<ParsedFilter>,
) -> Result<&'a str, TranslationError> {
    let name_end = input
        .find(|character: char| !(character.is_ascii_alphanumeric() || matches!(character, '_' | ':')))
        .unwrap_or(input.len());
    let metric = &input[..name_end];
    let mut rest = &input[name_end..];
    if !metric.is_empty() {
        if rest.trim_start().starts_with('(') || metric.starts_with(|character: char| character.is_ascii_digit()) {
            return Err(TranslationError("functions and aggregations cannot be translated"));
        }
        filters.push(ParsedFilter::new("__name__", FilterOperator::Eq, FilterValue::String(metric.to_owned())));
    }
    let Some(mut body) = rest.trim_start().strip_prefix('{') else {
        return if metric.is_empty() { Err(UNSUPPORTED) } else { Ok(rest) };
    };
    let mut first = true;
    loop {
        body = body.trim_start();
        if let Some(after) = body.strip_prefix('}') {
            rest = after;
            break;
        }
        if !first {
            body = body.strip_prefix(',').ok_or(UNSUPPORTED)?.trim_start();
            if let Some(after) = body.strip_prefix('}') {
                rest = after;
                break;
            }
        }
        first = false;
        let end = body
            .find(|character: char| !(character.is_ascii_alphanumeric() || character == '_'))
            .unwrap_or(body.len());
        let label = &body[..end];
        if !valid_label_name(label) && label != "__name__" {
            return Err(UNSUPPORTED);
        }
        let after = body[end..].trim_start();
        let (operator, after) = ["=~", "!=", "!~", "="]
            .iter()
            .find_map(|operator| after.strip_prefix(operator).map(|rest| (*operator, rest)))
            .ok_or(UNSUPPORTED)?;
        let (value, after) = read_string_literal(after.trim_start())
            .ok_or(TranslationError("label values must be double-quoted strings"))?;
        body = after;
        match operator {
            "=" => filters.push(ParsedFilter::new(label, FilterOperator::Eq, FilterValue::String(value))),
            "=~" => {
                let text = read_contains_regex(&value)
                    .ok_or(TranslationError("only \".*text.*\" regexes (contains) can be translated"))?;
                filters.push(ParsedFilter::new(label, FilterOperator::Contains, FilterValue::String(text)));
            }
            // The match-all anchor this dialect writes; it filters nothing.
            "!=" if label == "__name__" && value.is_empty() => {}
            _ => return Err(TranslationError("negative matchers cannot be translated")),
        }
    }
    Ok(rest)
}

fn valid_label_name(name: &str) -> bool {
    let mut bytes = name.bytes();
    !matches!(name, "_time" | "_value")
        && bytes
            .next()
            .is_some_and(|first| first.is_ascii_alphabetic() || first == b'_')
        && bytes.all(|byte| byte.is_ascii_alphanumeric() || byte == b'_')
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

    #[test]
    fn reading_accepts_the_portable_subset_and_the_anchor() {
        let read = dialect()
            .read(r#"((http_requests_total{job="api", path=~".*v1 \\(x\\).*", __name__!=""}) > 1) <= 2.5e3"#)
            .unwrap();
        assert_eq!(
            read,
            [
                ParsedFilter::new("__name__", FilterOperator::Eq, text("http_requests_total")),
                ParsedFilter::new("job", FilterOperator::Eq, text("api")),
                ParsedFilter::new("path", FilterOperator::Contains, text("v1 (x)")),
                ParsedFilter::new("_value", FilterOperator::Gt, FilterValue::Number(1.into())),
                ParsedFilter::new("_value", FilterOperator::Lte, literal_value("2.5e3")),
            ]
        );
        assert_eq!(
            dialect().read("up == 1").unwrap(),
            [
                ParsedFilter::new("__name__", FilterOperator::Eq, text("up")),
                ParsedFilter::new("_value", FilterOperator::Eq, FilterValue::Number(1.into())),
            ]
        );
        assert_eq!(dialect().read(r#"{__name__!=""}"#).unwrap(), []);
    }

    #[test]
    fn reading_rejects_anything_it_cannot_translate_exactly() {
        for query in [
            "rate(http_requests_total[5m])",
            "sum by (job) (up)",
            "up[5m]",
            "up offset 5m",
            "up > bool 1",
            "up != 1",
            "up > 1 and down",
            r#"up{job!="api"}"#,
            r#"up{job=~"api|web"}"#,
            "up{job='api'}",
            r#"up{job="api""#,
            "up > NaN",
            "(up",
            "1 + up",
        ] {
            assert!(dialect().read(query).is_err(), "{query}");
        }
    }
}
