//! ClickHouse SQL.
//!
//! Filters become `SELECT * FROM table WHERE …`; computations group end-labelled time buckets in
//! an outer query over an inner one that filters and renames the time column, so the
//! connector's own time filter still applies to the outer rows. Negated conditions include rows
//! where the value is NULL (`(NOT (c) OR x IS NULL)`), matching the other languages; a bare SQL
//! `NOT`/`!=` keeps SQL's meaning, which excludes NULLs.

use super::{quote_identifier, valid_identifier};
use crate::investigation_dialect::{
    bucket_ends,
    attribute_key, is_message_field, is_number, is_resource_attribute, literal_value, map_access,
    matching::{classify_search_pattern, search_pattern},
    Cmp, Expr, Field, FilterOperator, FilterValue, InvestigationDialect, Leaf, MetricQuery, Op,
    PortableRegex, Query, RangeFunction, Schema, TranslationError, WriteContext,
};

struct SqlDialect;

pub fn dialect() -> &'static dyn InvestigationDialect {
    &SqlDialect
}

const UNSUPPORTED: TranslationError = TranslationError(
    "only SELECT * FROM table WHERE conditions, or SELECT time buckets, group fields, and count/sum/avg/min/max … GROUP BY, can be translated",
);
const TIME_ALIAS: &str = "__ig_time";
const VALUE_ALIAS: &str = "__ig_value";
/// Columns the OpenTelemetry ClickHouse exporter uses for well-known attributes.
const OTEL_COLUMNS: &[(&str, &str)] = &[
    ("service.name", "ServiceName"),
    ("severity_text", "SeverityText"),
    ("severity", "SeverityText"),
    ("level", "SeverityText"),
    ("severity_number", "SeverityNumber"),
    ("trace_id", "TraceId"),
    ("span_id", "SpanId"),
    ("trace_flags", "TraceFlags"),
];
const MAP_COLUMNS: &[&str] = &["LogAttributes", "SpanAttributes", "Attributes", "ScopeAttributes", "ResourceAttributes"];
const MESSAGE_COLUMNS: &[&str] = &["Body", "message", "msg", "log", "line", "_msg"];

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

    /// Exact names, then OpenTelemetry exporter columns, then attribute maps such as
    /// `ResourceAttributes['service.name']`; Loki-style `service_name` also tries `service.name`.
    fn resolve_field(&self, name: &str, schema: &Schema) -> Option<Field> {
        if let Some((column, key)) = map_access(name) {
            if let Some(value_type) = schema.get(column).and_then(|field_type| map_value_type(field_type)) {
                return Some(Field::typed(format!("{column}['{key}']"), value_type));
            }
        }
        let key = attribute_key(name);
        let dotted = (!key.contains('.')).then(|| key.replace('_', "."));
        let keys = std::iter::once(key.to_owned()).chain(dotted).collect::<Vec<_>>();
        let mut candidates = vec![name.to_owned()];
        candidates.extend(keys.iter().cloned());
        for key in &keys {
            candidates.extend(OTEL_COLUMNS.iter().filter(|(attribute, _)| attribute == key).map(|(_, column)| (*column).to_owned()));
        }
        if is_message_field(name) {
            candidates.extend(MESSAGE_COLUMNS.iter().map(|column| (*column).to_owned()));
        }
        if let Some(field) = candidates
            .into_iter()
            .find_map(|candidate| schema.get(&candidate).map(|field_type| Field::typed(candidate, field_type.clone())))
        {
            return Some(field);
        }
        // Attribute maps: resource namespaces in ResourceAttributes first, others in record maps.
        for key in keys.iter().filter(|key| key.contains('.') || key.chars().all(|character| character.is_ascii_alphanumeric() || character == '_')) {
            let mut order = MAP_COLUMNS.to_vec();
            if is_resource_attribute(key) {
                order.rotate_right(1);
            }
            for column in order {
                if let Some(value_type) = schema.get(column).and_then(|field_type| map_value_type(field_type)) {
                    if map_access(&format!("{column}['{key}']")).is_some() {
                        return Some(Field::typed(format!("{column}['{key}']"), value_type));
                    }
                }
            }
        }
        None
    }

    fn check_leaf(&self, leaf: &Leaf) -> Result<(), TranslationError> {
        let Some(field) = &leaf.field else {
            return Ok(());
        };
        let kind = Kind::of(&field.field_type);
        if kind == Kind::Unsupported {
            return Err(TranslationError("arrays, tuples, maps, and JSON columns cannot be compared directly"));
        }
        if leaf.case_insensitive && kind != Kind::String {
            return Err(TranslationError("case-insensitive matching needs a string column"));
        }
        match &leaf.op {
            Op::Contains(_) | Op::Prefix(_) | Op::Suffix(_) | Op::Regex(_) | Op::Words(_) if kind != Kind::String => {
                Err(TranslationError("text matching needs a string column"))
            }
            Op::Eq(value) | Op::Compare(_, value) if kind == Kind::Number && value.as_number().is_none() => {
                Err(TranslationError("a numeric column compares only with numbers"))
            }
            Op::In(values) if values.is_empty() || values.len() > 256 => {
                Err(TranslationError("a value list needs between 1 and 256 values"))
            }
            Op::In(values) if kind == Kind::Number && values.iter().any(|value| value.as_number().is_none()) => {
                Err(TranslationError("a numeric column compares only with numbers"))
            }
            Op::Words(text) => crate::investigation_dialect::word_tokens(text).map(|_| ()),
            _ => Ok(()),
        }
    }

    fn read(&self, query: &str) -> Result<Query, TranslationError> {
        let tokens = tokenize(query)?;
        let mut parser = Parser { tokens, position: 0 };
        let query = parser.statement()?;
        if parser.position != parser.tokens.len() {
            return Err(UNSUPPORTED);
        }
        Ok(query)
    }

    fn write(&self, query: &Query, context: &WriteContext) -> Result<String, TranslationError> {
        if !valid_identifier(context.index) {
            return Err(TranslationError(
                "The selected data view has an unsupported table name",
            ));
        }
        let table = context
            .index
            .split('.')
            .map(quote_identifier)
            .collect::<Vec<_>>()
            .join(".");
        match query {
            Query::Filter(expr) => {
                let expr = expr.clone().nnf();
                let mut condition = condition(&expr, context.schema)?;
                // The top-level group needs no parentheses.
                if matches!(&expr, Expr::And(items) | Expr::Or(items) if !items.is_empty()) {
                    condition = condition[1..condition.len() - 1].to_owned();
                }
                Ok(if condition == "1" {
                    format!("SELECT * FROM {table}")
                } else {
                    format!("SELECT * FROM {table} WHERE {condition}")
                })
            }
            Query::Metric(metric) => write_metric(metric, &table, context),
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Kind {
    String,
    Number,
    Date,
    Bool,
    Unsupported,
}

impl Kind {
    fn of(field_type: &str) -> Self {
        let base = base_type(field_type);
        let lower = base.to_ascii_lowercase();
        if lower == "string" || lower.starts_with("fixedstring") || lower.starts_with("enum") || lower == "uuid" || lower.starts_with("ipv") {
            Self::String
        } else if lower.starts_with("int") || lower.starts_with("uint") || lower.starts_with("float") || lower.starts_with("decimal") {
            Self::Number
        } else if lower.starts_with("date") {
            Self::Date
        } else if lower == "bool" || lower == "boolean" {
            Self::Bool
        } else if lower.is_empty() {
            // Untyped (AI catalog without types): treat as a string column.
            Self::String
        } else {
            Self::Unsupported
        }
    }
}

/// The type without `Nullable(…)` and `LowCardinality(…)` wrappers.
fn base_type(field_type: &str) -> &str {
    let mut current = field_type.trim();
    loop {
        let stripped = ["Nullable(", "LowCardinality("]
            .iter()
            .find_map(|wrapper| current.strip_prefix(wrapper).and_then(|rest| rest.strip_suffix(')')));
        match stripped {
            Some(inner) => current = inner.trim(),
            None => return current,
        }
    }
}

fn is_nullable(field_type: &str) -> bool {
    field_type.contains("Nullable(")
}

/// `Map(K, V)`'s value type `V`.
fn map_value_type(field_type: &str) -> Option<String> {
    let inner = base_type(field_type).strip_prefix("Map(")?.strip_suffix(')')?;
    let mut depth = 0usize;
    for (index, character) in inner.char_indices() {
        match character {
            '(' => depth += 1,
            ')' => depth = depth.saturating_sub(1),
            ',' if depth == 0 => return Some(inner[index + 1..].trim().to_owned()),
            _ => {}
        }
    }
    None
}

/// ClickHouse processes backslash escapes inside string literals, so both `\` and `'` are
/// escaped; doubling `'` alone lets a trailing `\` end the string early.
fn string_literal(value: &str) -> String {
    format!("'{}'", value.replace('\\', "\\\\").replace('\'', "\\'"))
}

fn column(field: &Field) -> String {
    match map_access(&field.name) {
        Some((map, key)) => format!("{}[{}]", quote_identifier(map), string_literal(key)),
        None => quote_identifier(&field.name),
    }
}

fn value_sql(value: &FilterValue, kind: Kind) -> String {
    match (value, kind) {
        (FilterValue::Bool(flag), _) => flag.to_string(),
        (_, Kind::Number) => value.as_number().map_or_else(|| string_literal(&value.text()), |_| value.text().trim().to_owned()),
        (_, Kind::Bool) => value.text(),
        _ => string_literal(&value.text()),
    }
}

fn like_pattern(text: &str) -> String {
    text.replace('\\', "\\\\").replace('%', "\\%").replace('_', "\\_")
}

fn message_column(schema: &Schema) -> Result<Field, TranslationError> {
    MESSAGE_COLUMNS
        .iter()
        .find_map(|name| schema.get(*name).filter(|field_type| Kind::of(field_type) == Kind::String).map(|field_type| Field::typed(*name, field_type.clone())))
        .ok_or(TranslationError("free-text search needs a message column such as Body or message"))
}

/// A positive condition, and the expression whose NULL makes it unknown.
fn leaf_condition(leaf: &Leaf, schema: &Schema) -> Result<(String, Option<String>), TranslationError> {
    let field = match &leaf.field {
        Some(field) => field.clone(),
        None => message_column(schema)?,
    };
    let kind = Kind::of(&field.field_type);
    let target = column(&field);
    // Non-Nullable columns fold `IS NULL` to 0, so the guard is always the value itself.
    let nullable = Some(target.clone());
    let lower = |expr: &str| if leaf.case_insensitive { format!("lower({expr})") } else { expr.to_owned() };
    let like = if leaf.case_insensitive { "ILIKE" } else { "LIKE" };
    Ok(match &leaf.op {
        Op::Eq(value) => (format!("{} = {}", lower(&target), lower(&value_sql(value, kind))), nullable),
        Op::In(values) => (
            format!(
                "{} IN ({})",
                lower(&target),
                values.iter().map(|value| lower(&value_sql(value, kind))).collect::<Vec<_>>().join(", ")
            ),
            nullable,
        ),
        Op::Contains(text) => (format!("{target} {like} {}", string_literal(&format!("%{}%", like_pattern(text)))), nullable),
        Op::Prefix(text) => (format!("{target} {like} {}", string_literal(&format!("{}%", like_pattern(text)))), nullable),
        Op::Suffix(text) => (format!("{target} {like} {}", string_literal(&format!("%{}", like_pattern(text)))), nullable),
        Op::Regex(_) | Op::Words(_) => (
            format!("match({target}, {})", string_literal(&search_pattern(&leaf.op, leaf.case_insensitive)?)),
            nullable,
        ),
        Op::Exists => {
            let condition = match (map_access(&field.name), is_nullable(&field.field_type)) {
                (Some((map, key)), _) => format!("mapContains({}, {})", quote_identifier(map), string_literal(key)),
                (None, true) => format!("{target} IS NOT NULL"),
                (None, false) => "1".to_owned(),
            };
            (condition, None)
        }
        Op::Compare(cmp, value) => match (kind, value.as_number()) {
            // Numbers in string columns compare numerically; other values stay NULL.
            (Kind::String, Some(_)) => {
                let numeric = format!("toFloat64OrNull({target})");
                (format!("{numeric} {} {}", cmp.symbol(), value.text().trim()), Some(numeric))
            }
            _ => (format!("{target} {} {}", cmp.symbol(), value_sql(value, kind)), nullable),
        },
    })
}

/// SQL for a negation-normal-form expression.
fn condition(expr: &Expr, schema: &Schema) -> Result<String, TranslationError> {
    Ok(match expr {
        Expr::And(items) if items.is_empty() => "1".to_owned(),
        Expr::Or(items) if items.is_empty() => "0".to_owned(),
        Expr::And(items) | Expr::Or(items) => {
            let separator = if matches!(expr, Expr::And(_)) { " AND " } else { " OR " };
            let parts = items.iter().map(|item| condition(item, schema)).collect::<Result<Vec<_>, _>>()?;
            format!("({})", parts.join(separator))
        }
        Expr::Leaf(leaf) => leaf_condition(leaf, schema)?.0,
        Expr::Not(inner) => {
            let Expr::Leaf(leaf) = &**inner else {
                return Err(TranslationError("negation must be on a single condition"));
            };
            let (positive, nullable) = leaf_condition(leaf, schema)?;
            match (&leaf.op, nullable) {
                (Op::Exists, _) => match positive.strip_suffix(" IS NOT NULL") {
                    Some(target) => format!("{target} IS NULL"),
                    None if positive == "1" => "0".to_owned(),
                    None => format!("NOT {positive}"),
                },
                // The complement includes rows where the value is NULL, as in the other languages.
                (_, Some(value)) => format!("(NOT ({positive}) OR {value} IS NULL)"),
                (_, None) => format!("NOT ({positive})"),
            }
        }
    })
}

fn write_metric(metric: &MetricQuery, table: &str, context: &WriteContext) -> Result<String, TranslationError> {
    let time_name = context
        .time_field
        .ok_or(TranslationError("computations need the Data View's time field"))?;
    let time_type = context.schema.get(time_name).map(String::as_str).unwrap_or("DateTime64(3)");
    if Kind::of(time_type) != Kind::Date {
        return Err(TranslationError("the Data View's time field is not a date column"));
    }
    let time = quote_identifier(time_name);
    let seconds = metric.range_ms / 1_000;
    let mut inner = vec![format!("{time} AS `{TIME_ALIAS}`")];
    let mut outer = vec![format!(
        "toStartOfInterval(`{TIME_ALIAS}` - toIntervalNanosecond(1), INTERVAL {seconds} SECOND) + INTERVAL {seconds} SECOND AS {time}"
    )];
    let mut group = vec![time.clone()];
    for field in &metric.group_by {
        let alias = quote_identifier(&display_name(field));
        let expression = match Kind::of(&field.field_type) {
            // Empty strings (missing map keys) count as missing, as in the other languages.
            Kind::String => format!("nullIf({}, '')", column(field)),
            Kind::Unsupported => return Err(TranslationError("arrays, tuples, maps, and JSON columns cannot be grouped")),
            _ => column(field),
        };
        inner.push(format!("{expression} AS {alias}"));
        outer.push(alias.clone());
        group.push(alias);
    }
    let value = metric.function.value_field();
    if let Some(field) = value {
        let expression = match Kind::of(&field.field_type) {
            Kind::Number => column(field),
            Kind::String => format!("toFloat64OrNull({})", column(field)),
            _ => return Err(TranslationError("the value field must be numeric")),
        };
        inner.push(format!("{expression} AS `{VALUE_ALIAS}`"));
    }
    outer.push(format!(
        "{} AS `_value`",
        match &metric.function {
            RangeFunction::Count => "count()".to_owned(),
            RangeFunction::Rate => format!("count() / {seconds}"),
            RangeFunction::Sum(_) => format!("sum(`{VALUE_ALIAS}`)"),
            RangeFunction::Avg(_) => format!("avg(`{VALUE_ALIAS}`)"),
            RangeFunction::Min(_) => format!("min(`{VALUE_ALIAS}`)"),
            RangeFunction::Max(_) => format!("max(`{VALUE_ALIAS}`)"),
        }
    ));
    let mut filters = Vec::new();
    let filter = condition(&metric.filter.clone().nnf(), context.schema)?;
    if filter != "1" {
        filters.push(filter);
    }
    // Scan only the records that fall in the evaluated buckets: (first - range, last].
    if let Some(range) = context.time_range {
        let (start, end) = bucket_ends(range, metric.range_ms).map_or((0, 0), |ends| (ends.start - metric.range_ms, ends.end));
        filters.push(format!(
            "{time} > fromUnixTimestamp64Milli(toInt64({start}), 'UTC') AND {time} <= fromUnixTimestamp64Milli(toInt64({end}), 'UTC')"
        ));
    }
    let filter = if filters.is_empty() { String::new() } else { format!(" WHERE {}", filters.join(" AND ")) };
    let present = if value.is_some() { format!(" WHERE `{VALUE_ALIAS}` IS NOT NULL") } else { String::new() };
    Ok(format!(
        "SELECT {} FROM (SELECT {} FROM {table}{filter}){present} GROUP BY {} ORDER BY {time}",
        outer.join(", "),
        inner.join(", "),
        group.join(", ")
    ))
}

/// The column alias for a group field: a map key stands for its attribute.
fn display_name(field: &Field) -> String {
    map_access(&field.name).map_or_else(|| field.name.clone(), |(_, key)| key.to_owned())
}

// ---------------------------------------------------------------------------------------------
// Reading

#[derive(Clone, Debug, PartialEq)]
enum Token {
    Word(String),
    Quoted(String),
    Str(String),
    Number(String),
    Operator(&'static str),
    Star,
    Dot,
    Comma,
    Open,
    Close,
    OpenBracket,
    CloseBracket,
}

impl Token {
    fn is_keyword(&self, keyword: &str) -> bool {
        matches!(self, Self::Word(word) if word.eq_ignore_ascii_case(keyword))
    }
}

fn tokenize(query: &str) -> Result<Vec<Token>, TranslationError> {
    let characters = query.chars().collect::<Vec<_>>();
    let mut tokens = Vec::new();
    let mut index = 0;
    while index < characters.len() {
        let character = characters[index];
        let next = characters.get(index + 1).copied();
        match character {
            _ if character.is_whitespace() => index += 1,
            '\'' => {
                index += 1;
                let mut value = String::new();
                loop {
                    let Some(&character) = characters.get(index) else {
                        return Err(TranslationError("unterminated string"));
                    };
                    index += 1;
                    match character {
                        '\'' if characters.get(index) == Some(&'\'') => {
                            index += 1;
                            value.push('\'');
                        }
                        '\'' => break,
                        '\\' => {
                            let escaped = *characters.get(index).ok_or(TranslationError("unterminated string"))?;
                            index += 1;
                            match escaped {
                                '\\' | '\'' => value.push(escaped),
                                'n' => value.push('\n'),
                                't' => value.push('\t'),
                                'r' => value.push('\r'),
                                '0' => value.push('\0'),
                                // ClickHouse keeps other escapes as written, which is how LIKE
                                // escapes such as `\%` survive.
                                other => {
                                    value.push('\\');
                                    value.push(other);
                                }
                            }
                        }
                        other => value.push(other),
                    }
                }
                tokens.push(Token::Str(value));
            }
            '`' | '"' => {
                let quote = character;
                index += 1;
                let mut identifier = String::new();
                loop {
                    match characters.get(index) {
                        Some(&found) if found == quote && characters.get(index + 1) == Some(&quote) => {
                            identifier.push(quote);
                            index += 2;
                        }
                        Some(&found) if found == quote => {
                            index += 1;
                            break;
                        }
                        Some('\\') | None => return Err(UNSUPPORTED),
                        Some(&other) => {
                            identifier.push(other);
                            index += 1;
                        }
                    }
                }
                tokens.push(Token::Quoted(identifier));
            }
            '*' => {
                tokens.push(Token::Star);
                index += 1;
            }
            ',' => {
                tokens.push(Token::Comma);
                index += 1;
            }
            '(' => {
                tokens.push(Token::Open);
                index += 1;
            }
            ')' => {
                tokens.push(Token::Close);
                index += 1;
            }
            '[' => {
                tokens.push(Token::OpenBracket);
                index += 1;
            }
            ']' => {
                tokens.push(Token::CloseBracket);
                index += 1;
            }
            '.' if !next.is_some_and(|next| next.is_ascii_digit()) => {
                tokens.push(Token::Dot);
                index += 1;
            }
            '=' | '!' | '<' | '>' | '+' | '/' => {
                let pair = next.map(|next| format!("{character}{next}"));
                let (operator, width) = match pair.as_deref() {
                    Some("==") => ("=", 2),
                    Some("!=") | Some("<>") => ("!=", 2),
                    Some(">=") => (">=", 2),
                    Some("<=") => ("<=", 2),
                    _ => match character {
                        '=' => ("=", 1),
                        '<' => ("<", 1),
                        '>' => (">", 1),
                        '+' => ("+", 1),
                        '/' => ("/", 1),
                        _ => return Err(UNSUPPORTED),
                    },
                };
                tokens.push(Token::Operator(operator));
                index += width;
            }
            '-' if next.is_some_and(|next| next.is_ascii_digit() || next == '.')
                && !matches!(tokens.last(), Some(Token::Number(_) | Token::Word(_) | Token::Quoted(_) | Token::Close | Token::Str(_))) =>
            {
                let start = index;
                index += 1;
                while index < characters.len() && (characters[index].is_ascii_digit() || matches!(characters[index], '.' | 'e' | 'E') || (matches!(characters[index], '+' | '-') && matches!(characters[index - 1], 'e' | 'E'))) {
                    index += 1;
                }
                let number = characters[start..index].iter().collect::<String>();
                if !is_number(&number) {
                    return Err(UNSUPPORTED);
                }
                tokens.push(Token::Number(number));
            }
            '-' => {
                tokens.push(Token::Operator("-"));
                index += 1;
            }
            _ if character.is_ascii_digit() || character == '.' => {
                let start = index;
                while index < characters.len() && (characters[index].is_ascii_digit() || matches!(characters[index], '.' | 'e' | 'E') || (matches!(characters[index], '+' | '-') && matches!(characters[index - 1], 'e' | 'E'))) {
                    index += 1;
                }
                let number = characters[start..index].iter().collect::<String>();
                if !is_number(&number) {
                    return Err(UNSUPPORTED);
                }
                tokens.push(Token::Number(number));
            }
            _ if character.is_ascii_alphabetic() || character == '_' => {
                let start = index;
                while index < characters.len() && (characters[index].is_ascii_alphanumeric() || characters[index] == '_') {
                    index += 1;
                }
                tokens.push(Token::Word(characters[start..index].iter().collect()));
            }
            ';' => return Err(TranslationError("only one statement can be translated")),
            _ => return Err(UNSUPPORTED),
        }
    }
    Ok(tokens)
}

#[derive(Clone, Debug, PartialEq)]
enum Sql {
    Column(String),
    Str(String),
    Number(String),
    Bool(bool),
    Null,
    Call(String, Vec<Sql>),
    Interval(i64),
    Binary(&'static str, Box<Sql>, Box<Sql>),
    And(Vec<Sql>),
    Or(Vec<Sql>),
    Not(Box<Sql>),
    IsNull(Box<Sql>, bool),
    In(Box<Sql>, Vec<Sql>, bool),
    Like(Box<Sql>, String, bool, bool),
    Between(Box<Sql>, Box<Sql>, Box<Sql>, bool),
}

struct Parser {
    tokens: Vec<Token>,
    position: usize,
}

impl Parser {
    fn peek(&self) -> Option<&Token> {
        self.tokens.get(self.position)
    }

    fn keyword(&mut self, keyword: &str) -> bool {
        if self.peek().is_some_and(|token| token.is_keyword(keyword)) {
            self.position += 1;
            true
        } else {
            false
        }
    }

    fn expect(&mut self, token: Token) -> Result<(), TranslationError> {
        if self.peek() == Some(&token) {
            self.position += 1;
            Ok(())
        } else {
            Err(UNSUPPORTED)
        }
    }

    fn statement(&mut self) -> Result<Query, TranslationError> {
        if self.tokens.is_empty() {
            return Ok(Query::Filter(Expr::all()));
        }
        if !self.peek().is_some_and(|token| token.is_keyword("SELECT")) {
            self.keyword("WHERE");
            return Ok(Query::Filter(to_filter(&self.expression(0)?)?.simplify().merge_equalities()));
        }
        let select = self.select()?;
        select.into_query()
    }

    fn select(&mut self) -> Result<Select, TranslationError> {
        if !self.keyword("SELECT") {
            return Err(UNSUPPORTED);
        }
        let mut items = Vec::new();
        if self.peek() == Some(&Token::Star) {
            self.position += 1;
        } else {
            loop {
                let expression = self.expression(0)?;
                let alias = if self.keyword("AS") { Some(self.identifier()?) } else { None };
                items.push((expression, alias));
                if self.peek() != Some(&Token::Comma) {
                    break;
                }
                self.position += 1;
            }
        }
        if !self.keyword("FROM") {
            return Err(UNSUPPORTED);
        }
        let source = if self.peek() == Some(&Token::Open) {
            self.position += 1;
            let inner = self.select()?;
            self.expect(Token::Close)?;
            Source::Subquery(Box::new(inner))
        } else {
            let mut name = self.identifier()?;
            while self.peek() == Some(&Token::Dot) {
                self.position += 1;
                name = format!("{name}.{}", self.identifier()?);
            }
            Source::Table(name)
        };
        let filter = if self.keyword("WHERE") { Some(self.expression(0)?) } else { None };
        let mut group = Vec::new();
        if self.keyword("GROUP") {
            if !self.keyword("BY") {
                return Err(UNSUPPORTED);
            }
            loop {
                group.push(self.expression(0)?);
                if self.peek() != Some(&Token::Comma) {
                    break;
                }
                self.position += 1;
            }
        }
        if self.keyword("ORDER") {
            if !self.keyword("BY") {
                return Err(UNSUPPORTED);
            }
            // Ordering does not change which rows match; it is accepted on the time bucket only.
            self.expression(0)?;
            self.keyword("ASC");
        }
        if self.peek().is_some_and(|token| token.is_keyword("LIMIT") || token.is_keyword("HAVING") || token.is_keyword("JOIN") || token.is_keyword("UNION")) {
            return Err(TranslationError("LIMIT, HAVING, JOIN, and UNION cannot be translated"));
        }
        Ok(Select { items, source, filter, group })
    }

    fn identifier(&mut self) -> Result<String, TranslationError> {
        match self.tokens.get(self.position).cloned() {
            Some(Token::Word(word)) | Some(Token::Quoted(word)) => {
                self.position += 1;
                Ok(word)
            }
            _ => Err(UNSUPPORTED),
        }
    }

    /// Precedence climbing: OR < AND < NOT < comparisons < + - < * / < unary.
    fn expression(&mut self, depth: usize) -> Result<Sql, TranslationError> {
        if depth > 48 {
            return Err(TranslationError("the query nests too deeply"));
        }
        let mut items = vec![self.and(depth)?];
        while self.keyword("OR") {
            items.push(self.and(depth)?);
        }
        Ok(if items.len() == 1 { items.pop().unwrap() } else { Sql::Or(items) })
    }

    fn and(&mut self, depth: usize) -> Result<Sql, TranslationError> {
        let mut items = vec![self.not(depth)?];
        while self.keyword("AND") {
            items.push(self.not(depth)?);
        }
        Ok(if items.len() == 1 { items.pop().unwrap() } else { Sql::And(items) })
    }

    fn not(&mut self, depth: usize) -> Result<Sql, TranslationError> {
        if self.keyword("NOT") {
            return Ok(Sql::Not(Box::new(self.not(depth)?)));
        }
        self.comparison(depth)
    }

    fn comparison(&mut self, depth: usize) -> Result<Sql, TranslationError> {
        let left = self.additive(depth)?;
        if let Some(Token::Operator(operator @ ("=" | "!=" | "<" | "<=" | ">" | ">="))) = self.peek().cloned() {
            self.position += 1;
            let right = self.additive(depth)?;
            return Ok(Sql::Binary(operator, Box::new(left), Box::new(right)));
        }
        if self.keyword("IS") {
            let negated = self.keyword("NOT");
            if !self.keyword("NULL") {
                return Err(UNSUPPORTED);
            }
            return Ok(Sql::IsNull(Box::new(left), negated));
        }
        let negated = self.keyword("NOT");
        if self.keyword("IN") {
            self.expect(Token::Open)?;
            let mut values = Vec::new();
            loop {
                values.push(self.additive(depth)?);
                if self.peek() != Some(&Token::Comma) {
                    break;
                }
                self.position += 1;
            }
            self.expect(Token::Close)?;
            return Ok(Sql::In(Box::new(left), values, negated));
        }
        let insensitive = self.peek().is_some_and(|token| token.is_keyword("ILIKE"));
        if self.keyword("LIKE") || self.keyword("ILIKE") {
            let Some(Token::Str(pattern)) = self.tokens.get(self.position).cloned() else {
                return Err(UNSUPPORTED);
            };
            self.position += 1;
            return Ok(Sql::Like(Box::new(left), pattern, insensitive, negated));
        }
        if self.keyword("BETWEEN") {
            let low = self.additive(depth)?;
            if !self.keyword("AND") {
                return Err(UNSUPPORTED);
            }
            let high = self.additive(depth)?;
            return Ok(Sql::Between(Box::new(left), Box::new(low), Box::new(high), negated));
        }
        if negated {
            return Err(UNSUPPORTED);
        }
        Ok(left)
    }

    fn additive(&mut self, depth: usize) -> Result<Sql, TranslationError> {
        let mut left = self.multiplicative(depth)?;
        while let Some(Token::Operator(operator @ ("+" | "-"))) = self.peek().cloned() {
            self.position += 1;
            let right = self.multiplicative(depth)?;
            left = Sql::Binary(operator, Box::new(left), Box::new(right));
        }
        Ok(left)
    }

    fn multiplicative(&mut self, depth: usize) -> Result<Sql, TranslationError> {
        let mut left = self.primary(depth)?;
        while let Some(token @ (Token::Star | Token::Operator("/"))) = self.peek().cloned() {
            self.position += 1;
            let right = self.primary(depth)?;
            let operator = if token == Token::Star { "*" } else { "/" };
            left = Sql::Binary(operator, Box::new(left), Box::new(right));
        }
        Ok(left)
    }

    fn primary(&mut self, depth: usize) -> Result<Sql, TranslationError> {
        let token = self.tokens.get(self.position).cloned().ok_or(UNSUPPORTED)?;
        self.position += 1;
        let mut expression = match token {
            Token::Open => {
                if self.peek().is_some_and(|token| token.is_keyword("SELECT")) {
                    return Err(TranslationError("subqueries in conditions cannot be translated"));
                }
                let inner = self.expression(depth + 1)?;
                self.expect(Token::Close)?;
                inner
            }
            Token::Str(text) => Sql::Str(text),
            Token::Number(number) => Sql::Number(number),
            Token::Quoted(name) => Sql::Column(name),
            Token::Word(word) if word.eq_ignore_ascii_case("NULL") => Sql::Null,
            Token::Word(word) if word.eq_ignore_ascii_case("TRUE") => Sql::Bool(true),
            Token::Word(word) if word.eq_ignore_ascii_case("FALSE") => Sql::Bool(false),
            Token::Word(word) if word.eq_ignore_ascii_case("INTERVAL") => {
                let Some(Token::Number(amount)) = self.tokens.get(self.position).cloned() else {
                    return Err(UNSUPPORTED);
                };
                self.position += 1;
                let unit = self.identifier()?.to_ascii_uppercase();
                let seconds = match unit.trim_end_matches('S') {
                    "SECOND" => 1,
                    "MINUTE" => 60,
                    "HOUR" => 3_600,
                    "DAY" => 86_400,
                    "WEEK" => 604_800,
                    _ => return Err(TranslationError("only SECOND to WEEK intervals can be translated")),
                };
                let amount = amount.parse::<i64>().map_err(|_| UNSUPPORTED)?;
                Sql::Interval(amount.checked_mul(seconds * 1_000).ok_or(UNSUPPORTED)?)
            }
            Token::Word(word) if ["AND", "OR", "NOT", "SELECT", "FROM", "WHERE", "GROUP", "ORDER", "BY", "LIKE", "IN", "IS"].iter().any(|keyword| word.eq_ignore_ascii_case(keyword)) => {
                return Err(UNSUPPORTED)
            }
            Token::Word(word) if self.peek() == Some(&Token::Open) => {
                self.position += 1;
                let mut arguments = Vec::new();
                if self.peek() == Some(&Token::Star) {
                    self.position += 1;
                } else if self.peek() != Some(&Token::Close) {
                    loop {
                        arguments.push(self.expression(depth + 1)?);
                        if self.peek() != Some(&Token::Comma) {
                            break;
                        }
                        self.position += 1;
                    }
                }
                self.expect(Token::Close)?;
                Sql::Call(word.to_ascii_lowercase(), arguments)
            }
            Token::Word(word) => {
                let mut name = word;
                while self.peek() == Some(&Token::Dot) {
                    self.position += 1;
                    name = format!("{name}.{}", self.identifier()?);
                }
                Sql::Column(name)
            }
            _ => return Err(UNSUPPORTED),
        };
        if self.peek() == Some(&Token::OpenBracket) {
            let Sql::Column(map) = expression else {
                return Err(UNSUPPORTED);
            };
            self.position += 1;
            let Some(Token::Str(key)) = self.tokens.get(self.position).cloned() else {
                return Err(UNSUPPORTED);
            };
            self.position += 1;
            self.expect(Token::CloseBracket)?;
            expression = Sql::Column(format!("{map}['{key}']"));
            if map_access(match &expression { Sql::Column(name) => name, _ => unreachable!() }).is_none() {
                return Err(UNSUPPORTED);
            }
        }
        Ok(expression)
    }
}

enum Source {
    /// The table read; translation targets the Data View's own table instead.
    #[allow(dead_code)]
    Table(String),
    Subquery(Box<Select>),
}

struct Select {
    items: Vec<(Sql, Option<String>)>,
    source: Source,
    filter: Option<Sql>,
    group: Vec<Sql>,
}

impl Select {
    fn into_query(self) -> Result<Query, TranslationError> {
        if self.items.is_empty() {
            if !self.group.is_empty() || matches!(self.source, Source::Subquery(_)) {
                return Err(UNSUPPORTED);
            }
            let filter = self.filter.as_ref().map(to_filter).transpose()?.unwrap_or_else(Expr::all);
            return Ok(Query::Filter(filter.simplify().merge_equalities()));
        }
        // A computation: one time bucket, group fields, and one aggregate.
        let (inner_items, inner_filter) = match self.source {
            Source::Table(_) => (Vec::new(), None),
            Source::Subquery(inner) => {
                if !inner.group.is_empty() || !matches!(inner.source, Source::Table(_)) {
                    return Err(UNSUPPORTED);
                }
                (inner.items, inner.filter)
            }
        };
        // Inner aliases (writer form) map back to their source expressions.
        let resolve_alias = |expression: &Sql| -> Sql {
            if let Sql::Column(name) = expression {
                if let Some((source, _)) = inner_items.iter().find(|(_, alias)| alias.as_deref() == Some(name.as_str())) {
                    return source.clone();
                }
            }
            expression.clone()
        };
        let mut range_ms = None;
        let mut time_column = None;
        let mut group_by = Vec::new();
        let mut function = None;
        for (expression, _) in &self.items {
            if let Some((column, range)) = bucket(expression) {
                range_ms = Some(range);
                time_column = Some(resolve_alias(&Sql::Column(column)));
                continue;
            }
            if let Some(found) = aggregate(expression, &resolve_alias)? {
                if function.replace(found).is_some() {
                    return Err(TranslationError("only one aggregate can be translated"));
                }
                continue;
            }
            group_by.push(Field::named(group_field(&resolve_alias(expression))?));
        }
        let range_ms = range_ms.ok_or(TranslationError("a computation needs a toStartOfInterval(time, INTERVAL …) bucket"))?;
        let function = function.ok_or(TranslationError("a computation needs count(), sum(), avg(), min(), or max()"))?;
        let function = match function {
            Aggregate::Rate(divisor) if divisor * 1_000 == range_ms => RangeFunction::Rate,
            Aggregate::Rate(_) => return Err(TranslationError("a rate must divide count() by the bucket length in seconds")),
            Aggregate::Function(function) => function,
        };
        let time_column = match time_column {
            Some(Sql::Column(name)) => name,
            _ => return Err(UNSUPPORTED),
        };
        let mut filters = Vec::new();
        for filter in [inner_filter, self.filter].into_iter().flatten() {
            filters.extend(conjuncts(filter));
        }
        // The writer's scan window and value-presence checks are execution details.
        let filters = filters
            .into_iter()
            .filter(|filter| !is_time_window(filter, &time_column) && !is_value_presence(filter))
            .map(|filter| to_filter(&filter))
            .collect::<Result<Vec<_>, _>>()?;
        Ok(Query::Metric(MetricQuery {
            function,
            range_ms,
            group_by,
            filter: Expr::And(filters).simplify().merge_equalities(),
        }))
    }
}

enum Aggregate {
    Function(RangeFunction),
    /// `count() / seconds`.
    Rate(i64),
}

/// `toStartOfInterval(time, INTERVAL n UNIT)`, optionally minus a nanosecond and plus the
/// interval (the writer's end-labelled form).
fn bucket(expression: &Sql) -> Option<(String, i64)> {
    let start: &Sql = match expression {
        Sql::Binary("+", start, interval) if matches!(**interval, Sql::Interval(_)) => start,
        other => other,
    };
    let Sql::Call(name, arguments) = start else {
        return None;
    };
    if name != "tostartofinterval" || arguments.len() != 2 {
        return None;
    }
    let Sql::Interval(range) = arguments[1] else {
        return None;
    };
    let column = match &arguments[0] {
        Sql::Column(name) => name.clone(),
        Sql::Binary("-", inner, nanosecond)
            if matches!(&**nanosecond, Sql::Call(function, _) if function == "tointervalnanosecond") =>
        {
            match &**inner {
                Sql::Column(name) => name.clone(),
                _ => return None,
            }
        }
        _ => return None,
    };
    Some((column, range))
}

fn aggregate(expression: &Sql, resolve: &dyn Fn(&Sql) -> Sql) -> Result<Option<Aggregate>, TranslationError> {
    if let Sql::Binary("/", count, divisor) = expression {
        if matches!(&**count, Sql::Call(name, arguments) if name == "count" && arguments.is_empty()) {
            let Sql::Number(seconds) = &**divisor else {
                return Err(TranslationError("a rate must divide count() by a number of seconds"));
            };
            return Ok(Some(Aggregate::Rate(seconds.parse().map_err(|_| UNSUPPORTED)?)));
        }
    }
    let Sql::Call(name, arguments) = expression else {
        return Ok(None);
    };
    let value = |arguments: &[Sql]| -> Result<Field, TranslationError> {
        let [argument] = arguments else {
            return Err(UNSUPPORTED);
        };
        let argument = resolve(argument);
        let argument = match &argument {
            Sql::Call(function, inner) if function == "tofloat64ornull" && inner.len() == 1 => inner[0].clone(),
            other => other.clone(),
        };
        match argument {
            Sql::Column(name) => Ok(Field::named(name)),
            _ => Err(TranslationError("aggregates are translated only over a column")),
        }
    };
    Ok(Some(Aggregate::Function(match name.as_str() {
        "count" if arguments.is_empty() => RangeFunction::Count,
        "sum" => RangeFunction::Sum(value(arguments)?),
        "avg" => RangeFunction::Avg(value(arguments)?),
        "min" => RangeFunction::Min(value(arguments)?),
        "max" => RangeFunction::Max(value(arguments)?),
        "count" | "uniq" | "quantile" | "median" | "any" => {
            return Err(TranslationError("only count(), sum(), avg(), min(), and max() can be translated"))
        }
        _ => return Ok(None),
    })))
}

fn group_field(expression: &Sql) -> Result<String, TranslationError> {
    match expression {
        Sql::Column(name) => Ok(name.clone()),
        Sql::Call(function, arguments) if function == "nullif" && arguments.len() == 2 && arguments[1] == Sql::Str(String::new()) => {
            group_field(&arguments[0])
        }
        _ => Err(TranslationError("group fields must be columns")),
    }
}

fn conjuncts(expression: Sql) -> Vec<Sql> {
    match expression {
        Sql::And(items) => items.into_iter().flat_map(conjuncts).collect(),
        other => vec![other],
    }
}

fn is_time_window(expression: &Sql, time_column: &str) -> bool {
    matches!(expression, Sql::Binary(">" | "<=", column, bound)
        if matches!(&**column, Sql::Column(name) if name == time_column)
            && matches!(&**bound, Sql::Call(function, _) if function == "fromunixtimestamp64milli"))
}

fn is_value_presence(expression: &Sql) -> bool {
    matches!(expression, Sql::IsNull(column, true) if matches!(&**column, Sql::Column(name) if name == VALUE_ALIAS))
}

/// A condition as a filter tree.
fn to_filter(expression: &Sql) -> Result<Expr, TranslationError> {
    Ok(match expression {
        Sql::And(items) => Expr::And(items.iter().map(to_filter).collect::<Result<_, _>>()?),
        Sql::Or(items) => {
            // The writer's complement form `(NOT (c) OR x IS NULL)` is the plain negation.
            if let [Sql::Not(inner), Sql::IsNull(_, false)] = items.as_slice() {
                return Ok(to_filter(inner)?.negate());
            }
            Expr::Or(items.iter().map(to_filter).collect::<Result<_, _>>()?)
        }
        // A bare SQL NOT is false for NULLs, so it also requires the value to exist.
        Sql::Not(inner) => sql_negation(to_filter(inner)?),
        Sql::Number(number) if number == "1" => Expr::all(),
        Sql::Number(number) if number == "0" => Expr::Or(Vec::new()),
        Sql::Binary(operator, left, right) => comparison(operator, left, right)?,
        Sql::IsNull(column, negated) => {
            let leaf = Expr::Leaf(Leaf::new(column_name(column)?, Op::Exists));
            if *negated { leaf } else { leaf.negate() }
        }
        Sql::In(column, values, negated) => {
            let (name, insensitive) = column_reference(column)?;
            let values = values.iter().map(|value| literal(value, insensitive)).collect::<Result<Vec<_>, _>>()?;
            let leaf = Leaf { field: Some(Field::named(name)), op: Op::In(values), case_insensitive: insensitive };
            if *negated { sql_negation(Expr::Leaf(leaf)) } else { Expr::Leaf(leaf) }
        }
        Sql::Like(column, pattern, insensitive, negated) => {
            let leaf = Leaf { field: Some(Field::named(column_name(column)?)), op: like(pattern)?, case_insensitive: *insensitive };
            if *negated { sql_negation(Expr::Leaf(leaf)) } else { Expr::Leaf(leaf) }
        }
        Sql::Between(column, low, high, negated) => {
            let name = column_name(column)?;
            let range = Expr::And(vec![
                Expr::Leaf(Leaf::new(name.clone(), Op::Compare(Cmp::Gte, literal(low, false)?))),
                Expr::Leaf(Leaf::new(name, Op::Compare(Cmp::Lte, literal(high, false)?))),
            ]);
            if *negated { sql_negation(range) } else { range }
        }
        Sql::Call(function, arguments) => match (function.as_str(), arguments.as_slice()) {
            ("match", [column, Sql::Str(pattern)]) => {
                let (op, insensitive) = classify_search_pattern(pattern)?;
                let field = column_name(column)?;
                Expr::Leaf(Leaf { field: Some(Field::named(field)), op, case_insensitive: insensitive })
            }
            ("mapcontains", [Sql::Column(map), Sql::Str(key)]) => Expr::Leaf(Leaf::new(format!("{map}['{key}']"), Op::Exists)),
            _ => return Err(TranslationError("this function cannot be translated")),
        },
        _ => return Err(UNSUPPORTED),
    })
}

/// SQL's three-valued NOT: the negated condition holds only where the value exists.
fn sql_negation(expr: Expr) -> Expr {
    let fields = expr
        .leaves()
        .into_iter()
        .filter_map(|leaf| leaf.field.clone())
        .map(|field| field.name)
        .collect::<std::collections::BTreeSet<_>>();
    let mut items = fields
        .into_iter()
        .map(|name| Expr::Leaf(Leaf::new(name, Op::Exists)))
        .collect::<Vec<_>>();
    items.push(expr.negate());
    Expr::And(items).simplify()
}

fn comparison(operator: &str, left: &Sql, right: &Sql) -> Result<Expr, TranslationError> {
    // Literal on the left: flip the comparison.
    let (operator, left, right) = if is_literal(left) && !is_literal(right) {
        let flipped = match operator {
            "<" => ">",
            "<=" => ">=",
            ">" => "<",
            ">=" => "<=",
            other => other,
        };
        (flipped, right, left)
    } else {
        (operator, left, right)
    };
    let (name, insensitive) = column_reference(left)?;
    let value = literal(right, insensitive)?;
    let field = Some(Field::named(name));
    Ok(match operator {
        "=" => Expr::Leaf(Leaf { field, op: Op::Eq(value), case_insensitive: insensitive }),
        "!=" => sql_negation(Expr::Leaf(Leaf { field, op: Op::Eq(value), case_insensitive: insensitive })),
        other => {
            let cmp = Cmp::from_symbol(other).ok_or(UNSUPPORTED)?;
            if insensitive {
                return Err(UNSUPPORTED);
            }
            Expr::Leaf(Leaf { field, op: Op::Compare(cmp, value), case_insensitive: false })
        }
    })
}

fn is_literal(expression: &Sql) -> bool {
    matches!(expression, Sql::Str(_) | Sql::Number(_) | Sql::Bool(_))
        || matches!(expression, Sql::Call(function, arguments) if function == "lower" && arguments.len() == 1 && is_literal(&arguments[0]))
}

/// A column, `toFloat64OrNull(column)` (numeric comparison of a string), or `lower(column)`
/// (case-insensitive), and whether it is case-insensitive.
fn column_reference(expression: &Sql) -> Result<(String, bool), TranslationError> {
    match expression {
        Sql::Call(function, arguments) if function == "lower" && arguments.len() == 1 => Ok((column_name(&arguments[0])?, true)),
        Sql::Call(function, arguments) if function == "tofloat64ornull" && arguments.len() == 1 => Ok((column_name(&arguments[0])?, false)),
        other => Ok((column_name(other)?, false)),
    }
}

fn column_name(expression: &Sql) -> Result<String, TranslationError> {
    match expression {
        Sql::Column(name) => Ok(name.clone()),
        Sql::Call(..) => Err(TranslationError("functions on columns cannot be translated")),
        _ => Err(UNSUPPORTED),
    }
}

fn literal(expression: &Sql, insensitive: bool) -> Result<FilterValue, TranslationError> {
    match expression {
        Sql::Str(text) => Ok(FilterValue::String(text.clone())),
        Sql::Number(number) => Ok(literal_value(number)),
        Sql::Bool(flag) => Ok(FilterValue::Bool(*flag)),
        Sql::Call(function, arguments) if insensitive && function == "lower" && arguments.len() == 1 => literal(&arguments[0], false),
        Sql::Null => Err(TranslationError("compare with NULL using IS NULL")),
        _ => Err(TranslationError("conditions compare a column with a literal value")),
    }
}

/// A LIKE pattern as the simplest equivalent condition.
fn like(pattern: &str) -> Result<Op, TranslationError> {
    // Split into literal runs and wildcards, honouring `\%`, `\_`, and `\\`.
    let mut parts: Vec<Result<String, char>> = Vec::new();
    let mut characters = pattern.chars();
    let mut literal_run = String::new();
    while let Some(character) = characters.next() {
        match character {
            '\\' => match characters.next() {
                Some(escaped) => literal_run.push(escaped),
                None => return Err(UNSUPPORTED),
            },
            '%' | '_' => {
                if !literal_run.is_empty() {
                    parts.push(Ok(std::mem::take(&mut literal_run)));
                }
                parts.push(Err(character));
            }
            other => literal_run.push(other),
        }
    }
    if !literal_run.is_empty() {
        parts.push(Ok(literal_run));
    }
    Ok(match parts.as_slice() {
        [] => Op::Eq(FilterValue::String(String::new())),
        [Ok(text)] => Op::Eq(FilterValue::String(text.clone())),
        [Err('%'), Ok(text), Err('%')] => Op::Contains(text.clone()),
        [Ok(text), Err('%')] => Op::Prefix(text.clone()),
        [Err('%'), Ok(text)] => Op::Suffix(text.clone()),
        _ => {
            let mut regex = String::new();
            for part in &parts {
                match part {
                    Ok(text) => regex.push_str(&crate::investigation_dialect::quote_meta(text)),
                    Err('%') => regex.push_str(".*"),
                    Err(_) => regex.push('.'),
                }
            }
            Op::Regex(PortableRegex::from_re2(&regex)?)
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::investigation_dialect::{build_query, FilterPredicate};
    use crate::search::TimeRange;

    fn string(text: &str) -> FilterValue {
        FilterValue::String(text.to_owned())
    }

    fn schema() -> Schema {
        Schema::from([
            ("service".to_owned(), "String".to_owned()),
            ("status".to_owned(), "Nullable(Int32)".to_owned()),
            ("status_text".to_owned(), "String".to_owned()),
            ("level".to_owned(), "LowCardinality(Nullable(String))".to_owned()),
            ("Body".to_owned(), "String".to_owned()),
            ("ResourceAttributes".to_owned(), "Map(LowCardinality(String), String)".to_owned()),
            ("LogAttributes".to_owned(), "Map(LowCardinality(String), String)".to_owned()),
            ("ServiceName".to_owned(), "LowCardinality(String)".to_owned()),
            ("ts".to_owned(), "DateTime64(3)".to_owned()),
        ])
    }

    fn context(schema: &Schema) -> WriteContext<'_> {
        WriteContext { index: "logs", time_field: Some("ts"), time_range: None, schema }
    }

    fn typed(name: &str, op: Op) -> Leaf {
        dialect().resolve_field(name, &schema()).map(|field| Leaf { field: Some(field), op, case_insensitive: false }).unwrap()
    }

    #[test]
    fn ai_filters_keep_their_escaping() {
        let filters = [FilterPredicate { field: "message".to_owned(), field_type: String::new(), operator: FilterOperator::Contains, value: string("error%' OR 1=1 --") }];
        assert_eq!(
            build_query(dialect(), "database.events", &filters).unwrap(),
            r"SELECT * FROM `database`.`events` WHERE `message` LIKE '%error\\%\' OR 1=1 --%'"
        );
        let filters = [FilterPredicate { field: "s".to_owned(), field_type: String::new(), operator: FilterOperator::Eq, value: string(r"x\' OR 1=1 --") }];
        assert_eq!(build_query(dialect(), "probe", &filters).unwrap(), r"SELECT * FROM `probe` WHERE `s` = 'x\\\' OR 1=1 --'");
        for table in ["", "events; DROP", "db.`events`"] {
            assert!(build_query(dialect(), table, &[]).is_err(), "{table}");
        }
    }

    #[test]
    fn negation_includes_nulls_and_numbers_in_strings_compare_numerically() {
        let schema = schema();
        let expr = Expr::Or(vec![
            Expr::Not(Box::new(Expr::Leaf(typed("level", Op::Eq(string("error")))))),
            Expr::Leaf(typed("status_text", Op::Compare(Cmp::Gte, FilterValue::Number(500.into())))),
            Expr::Not(Box::new(Expr::Leaf(typed("level", Op::Exists)))),
            Expr::Leaf(typed("http.route", Op::Prefix("/api_v1".to_owned()))),
            Expr::Leaf(Leaf::text("gateway timeout")),
        ]);
        let written = dialect().write(&Query::Filter(expr.clone()), &context(&schema)).unwrap();
        assert_eq!(
            written,
            r"SELECT * FROM `logs` WHERE (NOT (`level` = 'error') OR `level` IS NULL) OR toFloat64OrNull(`status_text`) >= 500 OR `level` IS NULL OR `LogAttributes`['http.route'] LIKE '/api\\_v1%' OR match(`Body`, '(?i)\\bgateway\\W+timeout\\b')"
        );
        let Query::Filter(read) = dialect().read(&written).unwrap() else { panic!() };
        let names = |expr: &Expr| expr.leaves().iter().map(|leaf| leaf.op.clone()).collect::<Vec<_>>();
        assert_eq!(names(&read), names(&expr));
    }

    #[test]
    fn user_sql_keeps_sql_null_semantics() {
        let Query::Filter(read) = dialect().read("level != 'error' AND NOT (status >= 500) AND service IN ('a', 'b') AND Body ILIKE '%Time%' AND x BETWEEN 1 AND 5").unwrap() else { panic!() };
        let exists = read.leaves().iter().filter(|leaf| leaf.op == Op::Exists).count();
        assert_eq!(exists, 2, "{read:?}");
        for rejected in ["a = 1 OR", "count(a) > 1", "a = NULL", "SELECT a FROM t", "SELECT * FROM t LIMIT 5", "a = 1; DROP", "a IN (SELECT 1)", "lower(a) > 'x'"] {
            assert!(dialect().read(rejected).is_err(), "{rejected}");
        }
    }

    #[test]
    fn computations_bucket_by_end_time_and_read_back() {
        let schema = schema();
        let metric = MetricQuery {
            function: RangeFunction::Avg(dialect().resolve_field("status_text", &schema).unwrap()),
            range_ms: 60_000,
            group_by: vec![dialect().resolve_field("service.name", &schema).unwrap(), dialect().resolve_field("k8s.pod.name", &schema).unwrap()],
            filter: Expr::Leaf(typed("level", Op::Eq(string("error")))),
        };
        let mut context = context(&schema);
        context.time_range = Some(TimeRange { start: 90_000, end: 300_000 });
        let written = dialect().write(&Query::Metric(metric.clone()), &context).unwrap();
        assert_eq!(
            written,
            "SELECT toStartOfInterval(`__ig_time` - toIntervalNanosecond(1), INTERVAL 60 SECOND) + INTERVAL 60 SECOND AS `ts`, `ServiceName`, `k8s.pod.name`, avg(`__ig_value`) AS `_value` FROM (SELECT `ts` AS `__ig_time`, nullIf(`ServiceName`, '') AS `ServiceName`, nullIf(`ResourceAttributes`['k8s.pod.name'], '') AS `k8s.pod.name`, toFloat64OrNull(`status_text`) AS `__ig_value` FROM `logs` WHERE `level` = 'error' AND `ts` > fromUnixTimestamp64Milli(toInt64(60000), 'UTC') AND `ts` <= fromUnixTimestamp64Milli(toInt64(300000), 'UTC')) WHERE `__ig_value` IS NOT NULL GROUP BY `ts`, `ServiceName`, `k8s.pod.name` ORDER BY `ts`"
        );
        let Query::Metric(read) = dialect().read(&written).unwrap() else { panic!() };
        assert_eq!(read.range_ms, 60_000);
        assert_eq!(read.function, RangeFunction::Avg(Field::named("status_text")));
        assert_eq!(read.group_by.iter().map(|field| field.name.as_str()).collect::<Vec<_>>(), ["ServiceName", "ResourceAttributes['k8s.pod.name']"]);
        let Query::Metric(simple) = dialect().read("SELECT toStartOfInterval(ts, INTERVAL 5 MINUTE) AS t, service, count() / 300 FROM logs WHERE status >= 500 GROUP BY t, service").unwrap() else { panic!() };
        assert_eq!((simple.function, simple.range_ms), (RangeFunction::Rate, 300_000));
    }

    #[test]
    fn otel_columns_and_attribute_maps_resolve() {
        let schema = schema();
        for (name, resolved) in [
            ("service.name", "ServiceName"),
            ("service_name", "ServiceName"),
            ("resource.attributes.k8s.pod.name", "ResourceAttributes['k8s.pod.name']"),
            ("http.route", "LogAttributes['http.route']"),
            ("_msg", "Body"),
            ("ResourceAttributes['host.name']", "ResourceAttributes['host.name']"),
        ] {
            assert_eq!(dialect().resolve_field(name, &schema).unwrap().name, resolved, "{name}");
        }
    }
}
