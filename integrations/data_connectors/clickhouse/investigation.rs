use super::{quote_identifier, valid_identifier};
use crate::investigation_dialect::{
    literal_value, FilterOperator, FilterPredicate, FilterValue, InvestigationDialect,
    ParsedFilter, TranslationError,
};

struct SqlDialect;

pub fn dialect() -> &'static dyn InvestigationDialect {
    &SqlDialect
}

const UNSUPPORTED: TranslationError = TranslationError(
    "only `[SELECT * FROM table] [WHERE] field op value AND …` with =, >, >=, <, <=, and LIKE '%text%' can be translated",
);

impl InvestigationDialect for SqlDialect {
    fn language(&self) -> &'static str {
        "SQL"
    }

    fn example(&self) -> &'static str {
        "service = 'checkout' AND status >= 500 AND message LIKE '%timeout%'"
    }

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

    fn read(&self, query: &str) -> Result<Vec<ParsedFilter>, TranslationError> {
        let mut tokens = tokenize(query)?.into_iter().peekable();
        if tokens.peek().is_some_and(|token| token.is_keyword("SELECT")) {
            tokens.next();
            if tokens.next() != Some(Token::Star)
                || !tokens.next().is_some_and(|token| token.is_keyword("FROM"))
            {
                return Err(TranslationError("only SELECT * FROM table can be translated"));
            }
            // The table is the target Data View's, so the source table name is not used.
            loop {
                match tokens.next() {
                    Some(Token::Word(_) | Token::Quoted(_)) => {}
                    _ => return Err(UNSUPPORTED),
                }
                if tokens.peek() != Some(&Token::Dot) {
                    break;
                }
                tokens.next();
            }
        }
        if tokens.peek().is_some_and(|token| token.is_keyword("WHERE")) {
            tokens.next();
        }
        let mut filters = Vec::new();
        while tokens.peek().is_some() {
            if !filters.is_empty() {
                match tokens.next() {
                    Some(token) if token.is_keyword("AND") => {}
                    Some(token) if token.is_keyword("OR") || token.is_keyword("NOT") => {
                        return Err(TranslationError(
                            "only AND can join conditions; OR and NOT cannot be translated",
                        ))
                    }
                    _ => return Err(UNSUPPORTED),
                }
            }
            filters.push(read_condition(&mut tokens)?);
        }
        Ok(filters)
    }
}

/// ClickHouse processes backslash escapes inside string literals, so both `\` and `'` are
/// escaped; doubling `'` alone lets a trailing `\` end the string early.
fn string_literal(value: &str) -> String {
    format!("'{}'", value.replace('\\', "\\\\").replace('\'', "\\'"))
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
            // LIKE escapes first, then string-literal escapes on top of them.
            let pattern = text
                .replace('\\', "\\\\")
                .replace('%', "\\%")
                .replace('_', "\\_");
            format!("{field} LIKE {}", string_literal(&format!("%{pattern}%")))
        }
        FilterOperator::Gt => format!("{field} > {value}"),
        FilterOperator::Gte => format!("{field} >= {value}"),
        FilterOperator::Lt => format!("{field} < {value}"),
        FilterOperator::Lte => format!("{field} <= {value}"),
    })
}

#[derive(Debug, PartialEq)]
enum Token {
    Word(String),
    Quoted(String),
    Str(String),
    Number(String),
    Operator(&'static str),
    Star,
    Dot,
}

impl Token {
    fn is_keyword(&self, keyword: &str) -> bool {
        matches!(self, Self::Word(word) if word.eq_ignore_ascii_case(keyword))
    }
}

fn tokenize(query: &str) -> Result<Vec<Token>, TranslationError> {
    let mut tokens = Vec::new();
    let mut characters = query.char_indices().peekable();
    while let Some(&(start, character)) = characters.peek() {
        match character {
            _ if character.is_whitespace() => {
                characters.next();
            }
            '\'' => {
                characters.next();
                tokens.push(Token::Str(read_string(&mut characters)?));
            }
            '`' => {
                characters.next();
                let mut identifier = String::new();
                loop {
                    match characters.next() {
                        Some((_, '`')) if characters.peek().is_some_and(|(_, next)| *next == '`') => {
                            characters.next();
                            identifier.push('`');
                        }
                        Some((_, '`')) => break,
                        Some((_, '\\')) | None => return Err(UNSUPPORTED),
                        Some((_, other)) => identifier.push(other),
                    }
                }
                tokens.push(Token::Quoted(identifier));
            }
            '*' => {
                characters.next();
                tokens.push(Token::Star);
            }
            '.' if !characters.clone().nth(1).is_some_and(|(_, next)| next.is_ascii_digit()) => {
                characters.next();
                tokens.push(Token::Dot);
            }
            '=' | '!' | '<' | '>' => {
                let mut operator = String::from(character);
                characters.next();
                if let Some(&(_, next)) = characters.peek() {
                    if matches!((character, next), ('=', '=') | ('!' | '<' | '>', '=') | ('<', '>')) {
                        operator.push(next);
                        characters.next();
                    }
                }
                tokens.push(Token::Operator(match operator.as_str() {
                    "=" | "==" => "=",
                    ">" => ">",
                    ">=" => ">=",
                    "<" => "<",
                    "<=" => "<=",
                    "!=" | "<>" => {
                        return Err(TranslationError("not-equal conditions cannot be translated"))
                    }
                    _ => return Err(UNSUPPORTED),
                }));
            }
            _ if character.is_ascii_digit() || matches!(character, '-' | '+' | '.') => {
                let mut end = start;
                while let Some(&(index, next)) = characters.peek() {
                    let exponent_sign = matches!(next, '-' | '+')
                        && query[..index].ends_with(['e', 'E'])
                        && index > start;
                    if next.is_ascii_digit() || matches!(next, '.' | 'e' | 'E') || exponent_sign || index == start {
                        end = index + next.len_utf8();
                        characters.next();
                    } else {
                        break;
                    }
                }
                let number = &query[start..end];
                if !crate::investigation_dialect::is_number(number) {
                    return Err(UNSUPPORTED);
                }
                tokens.push(Token::Number(number.to_owned()));
            }
            _ if character.is_ascii_alphabetic() || character == '_' => {
                let mut end = start;
                while let Some(&(index, next)) = characters.peek() {
                    if next.is_ascii_alphanumeric() || matches!(next, '_' | '.') {
                        end = index + next.len_utf8();
                        characters.next();
                    } else {
                        break;
                    }
                }
                tokens.push(Token::Word(query[start..end].to_owned()));
            }
            ';' => return Err(TranslationError("only one statement can be translated")),
            '(' | ')' => {
                return Err(TranslationError(
                    "parentheses, functions, and subqueries cannot be translated",
                ))
            }
            _ => return Err(UNSUPPORTED),
        }
    }
    Ok(tokens)
}

/// Decodes a single-quoted literal as ClickHouse does: `''`, known backslash escapes, and any
/// other `\c` kept as written (which is how LIKE escapes such as `\%` survive).
fn read_string(
    characters: &mut std::iter::Peekable<std::str::CharIndices<'_>>,
) -> Result<String, TranslationError> {
    let mut value = String::new();
    loop {
        match characters.next() {
            None => return Err(TranslationError("unterminated string")),
            Some((_, '\'')) if characters.peek().is_some_and(|(_, next)| *next == '\'') => {
                characters.next();
                value.push('\'');
            }
            Some((_, '\'')) => return Ok(value),
            Some((_, '\\')) => match characters.next() {
                None => return Err(TranslationError("unterminated string")),
                Some((_, escaped)) => match escaped {
                    '\\' => value.push('\\'),
                    '\'' => value.push('\''),
                    'n' => value.push('\n'),
                    't' => value.push('\t'),
                    'r' => value.push('\r'),
                    '0' => value.push('\0'),
                    other => {
                        value.push('\\');
                        value.push(other);
                    }
                },
            },
            Some((_, other)) => value.push(other),
        }
    }
}

fn read_condition(
    tokens: &mut std::iter::Peekable<std::vec::IntoIter<Token>>,
) -> Result<ParsedFilter, TranslationError> {
    let field = match tokens.next() {
        Some(Token::Word(word))
            if !["AND", "OR", "NOT", "LIKE", "WHERE", "SELECT", "FROM"]
                .iter()
                .any(|keyword| word.eq_ignore_ascii_case(keyword)) =>
        {
            word
        }
        Some(Token::Quoted(identifier)) => identifier,
        _ => return Err(UNSUPPORTED),
    };
    match tokens.next() {
        Some(token) if token.is_keyword("LIKE") => {
            let Some(Token::Str(pattern)) = tokens.next() else {
                return Err(UNSUPPORTED);
            };
            Ok(ParsedFilter::new(
                field,
                FilterOperator::Contains,
                FilterValue::String(read_like(&pattern)?),
            ))
        }
        Some(Token::Operator(operator)) => {
            let operator = match operator {
                "=" => FilterOperator::Eq,
                ">" => FilterOperator::Gt,
                ">=" => FilterOperator::Gte,
                "<" => FilterOperator::Lt,
                _ => FilterOperator::Lte,
            };
            let value = match tokens.next() {
                Some(Token::Str(text)) => FilterValue::String(text),
                Some(Token::Number(number)) => literal_value(&number),
                Some(token) if token.is_keyword("TRUE") => FilterValue::Bool(true),
                Some(token) if token.is_keyword("FALSE") => FilterValue::Bool(false),
                _ => return Err(UNSUPPORTED),
            };
            Ok(ParsedFilter::new(field, operator, value))
        }
        _ => Err(UNSUPPORTED),
    }
}

/// The text of a `'%text%'` pattern, undoing LIKE escapes; other wildcards are rejected.
fn read_like(pattern: &str) -> Result<String, TranslationError> {
    let not_contains = TranslationError("LIKE is translated only as '%text%'");
    let inner = pattern
        .strip_prefix('%')
        .and_then(|rest| rest.strip_suffix('%'))
        .ok_or(not_contains)?;
    let mut text = String::new();
    let mut characters = inner.chars();
    while let Some(character) = characters.next() {
        match character {
            '\\' => match characters.next() {
                Some(escaped @ ('\\' | '%' | '_')) => text.push(escaped),
                _ => return Err(TranslationError("LIKE is translated only as '%text%'")),
            },
            '%' | '_' => return Err(TranslationError("LIKE is translated only as '%text%'")),
            other => text.push(other),
        }
    }
    if text.is_empty() {
        return Err(TranslationError("LIKE is translated only as '%text%'"));
    }
    Ok(text)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::investigation_dialect::build_query;

    fn predicate(field: &str, operator: FilterOperator, value: FilterValue) -> FilterPredicate {
        FilterPredicate {
            field: field.to_owned(),
            field_type: String::new(),
            operator,
            value,
        }
    }

    fn text(value: &str) -> FilterValue {
        FilterValue::String(value.to_owned())
    }

    #[test]
    fn filters_translate_to_escaped_sql() {
        let filters = [predicate(
            "message",
            FilterOperator::Contains,
            text("error%' OR 1=1 --"),
        )];
        assert_eq!(
            build_query(dialect(), "database.events", &filters).unwrap(),
            r"SELECT * FROM `database`.`events` WHERE `message` LIKE '%error\\%\' OR 1=1 --%'"
        );
        let filters = [predicate(
            "status",
            FilterOperator::Contains,
            FilterValue::Number(500.into()),
        )];
        assert!(build_query(dialect(), "events", &filters).is_err());
        for table in ["", "events; DROP", "db.`events`"] {
            assert!(build_query(dialect(), table, &[]).is_err(), "{table}");
        }
    }

    /// Verified against ClickHouse 24.8: `x\' OR 1=1 --` matched every row with `''` escaping.
    #[test]
    fn backslashes_cannot_end_a_string_early() {
        let filters = [predicate("s", FilterOperator::Eq, text(r"x\' OR 1=1 --"))];
        assert_eq!(
            build_query(dialect(), "probe", &filters).unwrap(),
            r"SELECT * FROM `probe` WHERE `s` = 'x\\\' OR 1=1 --'"
        );
        let filters = [predicate("s", FilterOperator::Contains, text(r"a\b_100%"))];
        assert_eq!(
            build_query(dialect(), "probe", &filters).unwrap(),
            r"SELECT * FROM `probe` WHERE `s` LIKE '%a\\\\b\\_100\\%%'"
        );
    }

    #[test]
    fn reading_accepts_the_portable_subset() {
        let read = dialect()
            .read("SELECT * FROM `db`.`events` WHERE service = 'it''s' AND `user-agent` == 'a\\\\b' AND status >= 500 AND latency < -0.5 AND ok = TRUE AND message LIKE '%50\\\\%\\_off%'")
            .unwrap();
        assert_eq!(
            read,
            [
                ParsedFilter::new("service", FilterOperator::Eq, text("it's")),
                ParsedFilter::new("user-agent", FilterOperator::Eq, text(r"a\b")),
                ParsedFilter::new("status", FilterOperator::Gte, FilterValue::Number(500.into())),
                ParsedFilter::new("latency", FilterOperator::Lt, literal_value("-0.5")),
                ParsedFilter::new("ok", FilterOperator::Eq, FilterValue::Bool(true)),
                ParsedFilter::new("message", FilterOperator::Contains, text("50%_off")),
            ]
        );
        assert_eq!(
            dialect().read("where status > 1").unwrap(),
            [ParsedFilter::new("status", FilterOperator::Gt, FilterValue::Number(1.into()))]
        );
        assert_eq!(dialect().read("SELECT * FROM events").unwrap(), []);
        assert_eq!(dialect().read("").unwrap(), []);
    }

    #[test]
    fn reading_rejects_anything_it_cannot_translate_exactly() {
        for query in [
            "status = 500 OR status = 503",
            "NOT status = 500",
            "status != 500",
            "status <> 500",
            "(status = 500)",
            "count(status) > 1",
            "SELECT status FROM events",
            "SELECT * FROM events WHERE status = 500 ORDER BY status",
            "SELECT * FROM events LIMIT 5",
            "status = 500; DROP TABLE events",
            "message LIKE 'timeout'",
            "message LIKE '%a_b%'",
            "message LIKE '%%'",
            "status IN (500, 503)",
            "status = NULL",
            "status = 'open",
            "status 500",
            "status = 500 AND",
        ] {
            assert!(dialect().read(query).is_err(), "{query}");
        }
    }
}
