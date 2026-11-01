use crate::connector_dialect::{
    FilterOperator, FilterPredicate, FilterValue, ConnectorDialect, TranslationError,
};

struct LuceneDialect;

pub fn dialect() -> &'static dyn ConnectorDialect {
    &LuceneDialect
}

/// Lucene's reserved characters, escaped in field names and values.
const SPECIAL: &str = "\\+-=&|><!(){}[]^\"~*?:/";

impl ConnectorDialect for LuceneDialect {
    fn operators(&self) -> &'static [FilterOperator] {
        &[
            FilterOperator::Eq,
            FilterOperator::Gt,
            FilterOperator::Gte,
            FilterOperator::Lt,
            FilterOperator::Lte,
        ]
    }

    /// Analyzed text fields match phrases, so only `eq` keeps its meaning there.
    fn field_operators(&self, field_type: &str) -> &'static [FilterOperator] {
        if is_text(field_type) {
            &[FilterOperator::Eq]
        } else {
            self.operators()
        }
    }

    fn aggregatable(&self, field_type: &str) -> bool {
        matches!(
            field_type.trim().to_ascii_lowercase().as_str(),
            "keyword"
                | "constant_keyword"
                | "boolean"
                | "ip"
                | "long"
                | "integer"
                | "short"
                | "byte"
                | "double"
                | "float"
                | "half_float"
                | "scaled_float"
                | "unsigned_long"
                | "version"
        )
    }

    fn translate(&self, _: &str, filters: &[FilterPredicate]) -> Result<String, TranslationError> {
        Ok(filters
            .iter()
            .map(lucene_filter)
            .collect::<Vec<_>>()
            .join(" AND "))
    }
}

fn is_text(field_type: &str) -> bool {
    matches!(
        field_type.trim().to_ascii_lowercase().as_str(),
        "text" | "match_only_text"
    )
}

fn lucene_filter(filter: &FilterPredicate) -> String {
    // Field names may hold `-` or `@`, which Lucene reads as operators.
    let field = escape(&filter.field, false);
    let value = filter.value.text();
    match filter.operator {
        FilterOperator::Eq => format!("{field}:\"{}\"", escape(&value, false)),
        // Unquoted, so whitespace is escaped too; otherwise each word matches separately.
        FilterOperator::Contains => format!("{field}:*{}*", escape(&value, true)),
        FilterOperator::Gt => format!("{field}:{{{} TO *]", bound(&filter.value)),
        FilterOperator::Gte => format!("{field}:[{} TO *]", bound(&filter.value)),
        FilterOperator::Lt => format!("{field}:[* TO {}}}", bound(&filter.value)),
        FilterOperator::Lte => format!("{field}:[* TO {}]", bound(&filter.value)),
    }
}

fn escape(text: &str, whitespace: bool) -> String {
    let mut escaped = String::with_capacity(text.len());
    for character in text.chars() {
        if SPECIAL.contains(character) || (whitespace && character.is_whitespace()) {
            escaped.push('\\');
        }
        escaped.push(character);
    }
    escaped
}

/// Non-negative numbers stay bare; other bounds are quoted, since an unquoted bound cannot hold
/// whitespace and a leading `-` is a Lucene operator.
fn bound(value: &FilterValue) -> String {
    match value {
        FilterValue::Number(number) if !number.to_string().starts_with('-') => number.to_string(),
        _ => format!("\"{}\"", escape(&value.text(), false)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::connector_dialect::build_query;

    fn filter(field: &str, operator: FilterOperator, value: FilterValue) -> String {
        lucene_filter(&FilterPredicate {
            field: field.to_owned(),
            field_type: String::new(),
            operator,
            value,
        })
    }

    #[test]
    fn filters_translate_to_escaped_lucene() {
        let filters = [
            FilterPredicate {
                field: "status".to_owned(),
                field_type: String::new(),
                operator: FilterOperator::Eq,
                value: FilterValue::String("500 OR *".to_owned()),
            },
            FilterPredicate {
                field: "latency".to_owned(),
                field_type: String::new(),
                operator: FilterOperator::Gte,
                value: FilterValue::Number(250.into()),
            },
        ];
        assert_eq!(
            build_query(dialect(), "logs-*", &filters).unwrap(),
            "status:\"500 OR \\*\" AND latency:[250 TO *]"
        );
        assert_eq!(build_query(dialect(), "logs-*", &[]).unwrap(), "");
    }

    #[test]
    fn field_names_and_range_bounds_are_escaped() {
        let text = |value: &str| FilterValue::String(value.to_owned());
        assert_eq!(
            filter("user-agent", FilterOperator::Eq, text("curl")),
            r#"user\-agent:"curl""#
        );
        // Negative and textual bounds were unquoted: `-` negated the clause and whitespace split it.
        assert_eq!(
            filter("delta", FilterOperator::Lt, FilterValue::Number((-5).into())),
            r#"delta:[* TO "\-5"}"#
        );
        assert_eq!(
            filter("version", FilterOperator::Gt, text("1.2 beta")),
            r#"version:{"1.2 beta" TO *]"#
        );
        assert_eq!(
            filter("message", FilterOperator::Contains, text("hello world")),
            r"message:*hello\ world*"
        );
    }

    #[test]
    fn analyzed_text_fields_allow_only_phrase_matches() {
        assert_eq!(dialect().field_operators("text"), [FilterOperator::Eq]);
        assert_eq!(dialect().field_operators("keyword"), dialect().operators());
    }

    #[test]
    fn only_keyword_like_and_numeric_fields_aggregate() {
        assert!(dialect().aggregatable("keyword"));
        assert!(dialect().aggregatable(" Long "));
        assert!(!dialect().aggregatable("text"));
        assert!(!dialect().aggregatable("date"));
    }
}
