use super::{quote_identifier, valid_identifier};
use crate::connector_dialect::{
    FilterOperator, FilterPredicate, FilterValue, ConnectorDialect, TranslationError,
};

struct SqlDialect;

pub fn dialect() -> &'static dyn ConnectorDialect {
    &SqlDialect
}

impl ConnectorDialect for SqlDialect {
    fn operators(&self) -> &'static [FilterOperator] {
        &FilterOperator::ALL
    }

    fn aggregatable(&self, field_type: &str) -> bool {
        let normalized = field_type.trim().to_ascii_lowercase();
        let normalized = normalized
            .strip_prefix("nullable(")
            .and_then(|value| value.strip_suffix(')'))
            .unwrap_or(&normalized);
        ![
            "object", "nested", "array", "map", "tuple", "json", "text", "date", "datetime",
        ]
        .iter()
        .any(|prefix| normalized.starts_with(prefix))
    }

    fn check(&self, filter: &FilterPredicate) -> Result<(), TranslationError> {
        match (&filter.operator, &filter.value) {
            (FilterOperator::Contains, FilterValue::Number(_) | FilterValue::Bool(_)) => {
                Err(TranslationError("Contains filters require string values"))
            }
            _ => Ok(()),
        }
    }

    fn translate(
        &self,
        index: &str,
        filters: &[FilterPredicate],
    ) -> Result<String, TranslationError> {
        if !valid_identifier(index) {
            return Err(TranslationError(
                "The selected data view has an unsupported table name",
            ));
        }
        let table = index
            .split('.')
            .map(quote_identifier)
            .collect::<Vec<_>>()
            .join(".");
        let clauses = filters.iter().map(sql_filter).collect::<Result<Vec<_>, _>>()?;
        let suffix = if clauses.is_empty() {
            String::new()
        } else {
            format!(" WHERE {}", clauses.join(" AND "))
        };
        Ok(format!("SELECT * FROM {table}{suffix}"))
    }
}

fn sql_filter(filter: &FilterPredicate) -> Result<String, TranslationError> {
    let field = quote_identifier(&filter.field);
    let value = match &filter.value {
        FilterValue::String(value) => string_literal(value),
        FilterValue::Number(_) | FilterValue::Bool(_) => filter.value.text(),
    };
    Ok(match filter.operator {
        FilterOperator::Eq => format!("{field} = {value}"),
        FilterOperator::Contains => {
            let FilterValue::String(text) = &filter.value else {
                return Err(TranslationError("Contains filters require string values"));
            };
            format!(
                "{field} LIKE {}",
                string_literal(&format!("%{}%", like_pattern(text)))
            )
        }
        FilterOperator::Gt => format!("{field} > {value}"),
        FilterOperator::Gte => format!("{field} >= {value}"),
        FilterOperator::Lt => format!("{field} < {value}"),
        FilterOperator::Lte => format!("{field} <= {value}"),
    })
}

/// ClickHouse processes backslash escapes inside string literals, so both `\` and `'` are
/// escaped; doubling `'` alone lets a value ending in `\` close the string early.
fn string_literal(value: &str) -> String {
    format!("'{}'", value.replace('\\', "\\\\").replace('\'', "\\'"))
}

/// Escapes `LIKE` wildcards and the escape character; the result still needs `string_literal`.
fn like_pattern(text: &str) -> String {
    text.replace('\\', "\\\\")
        .replace('%', "\\%")
        .replace('_', "\\_")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::connector_dialect::build_query;

    #[test]
    fn filters_translate_to_escaped_sql() {
        let filters = [FilterPredicate {
            field: "message".to_owned(),
            field_type: String::new(),
            operator: FilterOperator::Contains,
            value: FilterValue::String("error%' OR 1=1 --".to_owned()),
        }];
        assert_eq!(
            build_query(dialect(), "database.events", &filters).unwrap(),
            r"SELECT * FROM `database`.`events` WHERE `message` LIKE '%error\\%\' OR 1=1 --%'"
        );
        // A backslash before the quote used to end the string early and match every row.
        let filters = [FilterPredicate {
            field: "s".to_owned(),
            field_type: String::new(),
            operator: FilterOperator::Eq,
            value: FilterValue::String(r"x\' OR 1=1 --".to_owned()),
        }];
        assert_eq!(
            build_query(dialect(), "probe", &filters).unwrap(),
            r"SELECT * FROM `probe` WHERE `s` = 'x\\\' OR 1=1 --'"
        );
        let filters = [FilterPredicate {
            field: "path".to_owned(),
            field_type: String::new(),
            operator: FilterOperator::Contains,
            value: FilterValue::String(r"a_b\".to_owned()),
        }];
        assert_eq!(
            build_query(dialect(), "probe", &filters).unwrap(),
            r"SELECT * FROM `probe` WHERE `path` LIKE '%a\\_b\\\\%'"
        );
        let filters = [FilterPredicate {
            field: "status".to_owned(),
            field_type: String::new(),
            operator: FilterOperator::Contains,
            value: FilterValue::Number(500.into()),
        }];
        assert!(build_query(dialect(), "events", &filters).is_err());
        for table in ["", "events; DROP", "db.`events`"] {
            assert!(build_query(dialect(), table, &[]).is_err(), "{table}");
        }
    }

    #[test]
    fn composite_and_temporal_columns_do_not_aggregate() {
        assert!(dialect().aggregatable("LowCardinality(String)"));
        assert!(dialect().aggregatable("Nullable(UInt16)"));
        assert!(!dialect().aggregatable("Nullable(DateTime64(3))"));
        assert!(!dialect().aggregatable("Array(String)"));
    }
}
