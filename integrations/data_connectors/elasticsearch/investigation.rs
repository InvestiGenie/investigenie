use crate::investigation_dialect::{
    FilterOperator, FilterPredicate, InvestigationDialect, TranslationError,
};

struct LuceneDialect;

pub fn dialect() -> &'static dyn InvestigationDialect {
    &LuceneDialect
}

impl InvestigationDialect for LuceneDialect {
    fn operators(&self) -> &'static [FilterOperator] {
        &[
            FilterOperator::Eq,
            FilterOperator::Gt,
            FilterOperator::Gte,
            FilterOperator::Lt,
            FilterOperator::Lte,
        ]
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

fn lucene_filter(filter: &FilterPredicate) -> String {
    let escaped = filter
        .value
        .text()
        .chars()
        .flat_map(|character| {
            if "\\+-=&|><!(){}[]^\"~*?:/".contains(character) {
                vec!['\\', character]
            } else {
                vec![character]
            }
        })
        .collect::<String>();
    match filter.operator {
        FilterOperator::Eq => format!("{}:\"{}\"", filter.field, escaped),
        FilterOperator::Contains => format!("{}:*{}*", filter.field, escaped),
        FilterOperator::Gt => format!("{}:{{{} TO *]", filter.field, escaped),
        FilterOperator::Gte => format!("{}:[{} TO *]", filter.field, escaped),
        FilterOperator::Lt => format!("{}:[* TO {}}}", filter.field, escaped),
        FilterOperator::Lte => format!("{}:[* TO {}]", filter.field, escaped),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::investigation_dialect::{build_query, FilterValue};

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
    fn only_keyword_like_and_numeric_fields_aggregate() {
        assert!(dialect().aggregatable("keyword"));
        assert!(dialect().aggregatable(" Long "));
        assert!(!dialect().aggregatable("text"));
        assert!(!dialect().aggregatable("date"));
    }
}
