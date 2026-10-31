//! Translation between datasource query languages.
//!
//! A data connector supports AI investigations, and cross-language Explore queries, by adding
//! `investigation.rs` beside its `mod.rs` with `pub fn dialect() -> &'static dyn
//! InvestigationDialect`. `backend/build.rs` discovers the file, registers the dialect, and
//! derives the descriptor's `ai_investigation` flag and `query_languages` list from it.
//!
//! Every dialect reads its language into a language-neutral [`Query`] (a filter tree, or a
//! computation over time buckets) and writes such a query back in its language. Explore
//! translates by reading with the source language's dialect and writing with the target's, after
//! mapping field names onto the target's schema. Anything a target cannot express exactly is
//! rejected with a reason, never approximated.

mod expr;
mod fields;
pub mod matching;
mod metric;
mod regex;

use std::collections::BTreeMap;

use serde_json::{json, Number, Value};

pub use expr::{Cmp, Expr, Field, Leaf, Op};
pub use fields::{
    attribute_key, first_present, is_message_field, is_resource_attribute, label_name, Schema,
};
pub use metric::{bucket_ends, format_duration, parse_duration, MetricQuery, RangeFunction};
pub use regex::PortableRegex;

use crate::data_connectors::investigation_dialects;
use crate::search::{safe_field_name, TimeRange};

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
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
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

/// A filter the AI planner proposed for one catalog field.
#[derive(Debug)]
pub struct FilterPredicate {
    pub field: String,
    /// The type the source reported for `field`, as listed in the planner catalog.
    pub field_type: String,
    pub operator: FilterOperator,
    pub value: FilterValue,
}

impl FilterPredicate {
    pub fn to_leaf(&self) -> Leaf {
        let op = match self.operator {
            FilterOperator::Eq => Op::Eq(self.value.clone()),
            FilterOperator::Contains => Op::Contains(self.value.text()),
            FilterOperator::Gt => Op::Compare(Cmp::Gt, self.value.clone()),
            FilterOperator::Gte => Op::Compare(Cmp::Gte, self.value.clone()),
            FilterOperator::Lt => Op::Compare(Cmp::Lt, self.value.clone()),
            FilterOperator::Lte => Op::Compare(Cmp::Lte, self.value.clone()),
        };
        Leaf {
            field: Some(Field::typed(&self.field, &self.field_type)),
            op,
            case_insensitive: false,
        }
    }
}

/// A query in a language-neutral form.
#[derive(Clone, Debug, PartialEq)]
pub enum Query {
    /// Records matching a filter.
    Filter(Expr),
    /// A computation over time buckets.
    Metric(MetricQuery),
}

/// What a writer needs to know about the target beyond the query.
pub struct WriteContext<'a> {
    /// The Data View's index, table, or dataset.
    pub index: &'a str,
    /// The Data View's time field, for computations in SQL.
    pub time_field: Option<&'a str>,
    /// The search's time range, so SQL computations scan only it.
    pub time_range: Option<TimeRange>,
    /// The target's fields and types, for choices such as which column holds the message.
    pub schema: &'a Schema,
}

/// A sanitized translation failure that is safe to show to the user.
#[derive(Debug, PartialEq, Eq)]
pub struct TranslationError(pub &'static str);

pub trait InvestigationDialect: Send + Sync {
    /// The query language this dialect reads and writes; equal to the descriptor's.
    fn language(&self) -> &'static str;
    /// A query `read` accepts, offered as Explore's placeholder.
    fn example(&self) -> &'static str;
    /// Operators the AI planner may use, in the order they are offered to the model.
    fn operators(&self) -> &'static [FilterOperator];
    /// Planner operators valid for one source-reported field type; a subset of `operators`.
    fn field_operators(&self, _field_type: &str) -> &'static [FilterOperator] {
        self.operators()
    }
    /// Whether a field of this source-reported type can back a top-values breakdown.
    fn aggregatable(&self, field_type: &str) -> bool;
    /// Maps a field name from any source onto this source's schema. The default accepts only
    /// exact names; dialects add their own aliases.
    fn resolve_field(&self, name: &str, schema: &Schema) -> Option<Field> {
        schema
            .get(name)
            .map(|field_type| Field::typed(name, field_type.clone()))
    }
    /// The condition an AI planner filter means on this source.
    fn planner_leaf(&self, filter: &FilterPredicate) -> Leaf {
        filter.to_leaf()
    }
    /// Whether this source can express one typed condition exactly; the reason is shown.
    fn check_leaf(&self, leaf: &Leaf) -> Result<(), TranslationError>;
    /// Reads a query, or explains which part has no language-neutral meaning.
    fn read(&self, query: &str) -> Result<Query, TranslationError>;
    /// Writes a query whose leaves `check_leaf` accepted.
    fn write(&self, query: &Query, context: &WriteContext) -> Result<String, TranslationError>;
}

/// Field names as datasources spell them, plus ClickHouse map access such as
/// `ResourceAttributes['service.name']`.
pub fn valid_field_name(name: &str) -> bool {
    safe_field_name(name) || map_access(name).is_some()
}

/// The column and key of `Column['key']`.
pub fn map_access(name: &str) -> Option<(&str, &str)> {
    let (column, rest) = name.split_once("['")?;
    let key = rest.strip_suffix("']")?;
    let column_ok = !column.is_empty()
        && column
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || byte == b'_');
    let key_ok = !key.is_empty()
        && key.len() <= 256
        && key
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || b"._-/:@".contains(&byte));
    (column_ok && key_ok).then_some((column, key))
}

/// The checks every AI filter gets: a safe field name, a planner operator valid for the field's
/// type, and the dialect's own `check_leaf`.
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
        return Err(TranslationError(
            "this operator is not supported for this field",
        ));
    }
    dialect.check_leaf(&dialect.planner_leaf(filter))
}

/// Validates AI filters, then writes them as one AND-combined filter.
pub fn build_query(
    dialect: &dyn InvestigationDialect,
    index: &str,
    filters: &[FilterPredicate],
) -> Result<String, TranslationError> {
    for filter in filters {
        validate(dialect, filter)?;
    }
    let expr = Expr::And(
        filters
            .iter()
            .map(|filter| Expr::Leaf(dialect.planner_leaf(filter)))
            .collect(),
    );
    dialect.write(
        &Query::Filter(expr),
        &WriteContext {
            index,
            time_field: None,
            time_range: None,
            schema: &Schema::new(),
        },
    )
}

/// The dialect that reads `language`, matched case-insensitively.
pub fn dialect_for_language(language: &str) -> Option<&'static dyn InvestigationDialect> {
    investigation_dialects()
        .into_iter()
        .find(|dialect| dialect.language().eq_ignore_ascii_case(language.trim()))
}

/// Languages Explore can translate into `native`'s language, native first, with examples.
pub fn input_languages(native: &dyn InvestigationDialect) -> Vec<Value> {
    let mut others = investigation_dialects()
        .into_iter()
        .filter(|dialect| dialect.language() != native.language())
        .map(|dialect| (dialect.language(), dialect.example()))
        .collect::<BTreeMap<_, _>>();
    others.remove(native.language());
    std::iter::once((native.language(), native.example()))
        .chain(others)
        .map(|(language, example)| json!({"language": language, "example": example}))
        .collect()
}

/// Why a cross-language query could not be translated; every message is safe to show.
#[derive(Debug, PartialEq, Eq)]
pub enum QueryTranslationError {
    UnknownLanguage,
    Read(&'static str, TranslationError),
    UnknownField(String),
    Unsupported(String, TranslationError),
}

impl std::fmt::Display for QueryTranslationError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::UnknownLanguage => {
                formatter.write_str("This query language cannot be translated")
            }
            Self::Read(language, error) => write!(formatter, "{language} query: {}", error.0),
            Self::UnknownField(field) => {
                write!(formatter, "Field \"{field}\" is not in this Data View")
            }
            Self::Unsupported(field, error) if field.is_empty() => formatter.write_str(error.0),
            Self::Unsupported(field, error) => write!(formatter, "Field \"{field}\": {}", error.0),
        }
    }
}

/// A translated query; computations also carry their bucket range, which the search must step by.
#[derive(Debug, PartialEq, Eq)]
pub struct Translation {
    pub query: String,
    pub range_ms: Option<i64>,
}

/// Reads `query` in `language`, maps its fields onto the target Data View, checks every
/// condition against the target, and writes it in the target's language.
pub fn translate_query(
    language: &str,
    query: &str,
    target: &dyn InvestigationDialect,
    context: &WriteContext,
) -> Result<Translation, QueryTranslationError> {
    let source = dialect_for_language(language).ok_or(QueryTranslationError::UnknownLanguage)?;
    let read = source
        .read(query)
        .map_err(|error| QueryTranslationError::Read(source.language(), error))?;
    translate_parsed(read, target, context)
}

/// Resolves, checks, and writes a query that a dialect has already read.
pub fn translate_parsed(
    mut query: Query,
    target: &dyn InvestigationDialect,
    context: &WriteContext,
) -> Result<Translation, QueryTranslationError> {
    let resolve = |field: &mut Field| -> Result<(), QueryTranslationError> {
        let resolved = target
            .resolve_field(&field.name, context.schema)
            .filter(|resolved| valid_field_name(&resolved.name))
            .ok_or_else(|| QueryTranslationError::UnknownField(truncated(&field.name)))?;
        *field = resolved;
        Ok(())
    };
    let unsupported = |error| QueryTranslationError::Unsupported(String::new(), error);
    let filter = match &mut query {
        Query::Filter(expr) => expr,
        Query::Metric(metric) => {
            metric.validate().map_err(unsupported)?;
            for field in metric.group_by.iter_mut() {
                resolve(field)?;
            }
            if let Some(field) = metric.function.value_field_mut() {
                resolve(field)?;
            }
            &mut metric.filter
        }
    };
    filter.check_size().map_err(unsupported)?;
    for leaf in filter.leaves_mut() {
        if let Some(field) = leaf.field.as_mut() {
            resolve(field)?;
        }
        target.check_leaf(leaf).map_err(|error| {
            QueryTranslationError::Unsupported(
                leaf.field
                    .as_ref()
                    .map_or_else(String::new, |field| truncated(&field.name)),
                error,
            )
        })?;
    }
    let range_ms = match &query {
        Query::Metric(metric) => Some(metric.range_ms),
        Query::Filter(_) => None,
    };
    let written = target.write(&query, context).map_err(unsupported)?;
    Ok(Translation {
        query: written,
        range_ms,
    })
}

fn truncated(field: &str) -> String {
    field.chars().take(64).collect()
}

/// A double-quoted literal for LogQL and PromQL; JSON escaping is a subset of Go's escapes.
pub fn string_literal(value: &str) -> String {
    Value::from(value).to_string()
}

/// Reads a double-quoted (JSON/Go escapes) or backtick (raw) literal at the start of `input`,
/// returning it and the rest.
pub fn read_string_literal(input: &str) -> Option<(String, &str)> {
    if let Some(raw) = input.strip_prefix('`') {
        let end = raw.find('`')?;
        return Some((raw[..end].to_owned(), &raw[end + 1..]));
    }
    if !input.starts_with('"') {
        return None;
    }
    let mut escaped = false;
    for (index, character) in input.char_indices().skip(1) {
        match character {
            _ if escaped => escaped = false,
            '\\' => escaped = true,
            '"' => {
                let literal = serde_json::from_str::<String>(&input[..=index]).ok()?;
                return Some((literal, &input[index + 1..]));
            }
            _ => {}
        }
    }
    None
}

/// Escapes RE2 metacharacters, matching Go's `regexp.QuoteMeta`.
pub fn quote_meta(value: &str) -> String {
    let mut quoted = String::with_capacity(value.len());
    for character in value.chars() {
        if r"\.+*?()|[]{}^$".contains(character) {
            quoted.push('\\');
        }
        quoted.push(character);
    }
    quoted
}

/// The words of a free-text or `Words` condition: runs of letters, digits, and underscores.
pub fn word_tokens(text: &str) -> Result<Vec<&str>, TranslationError> {
    let tokens = text
        .split(|character: char| !(character.is_alphanumeric() || character == '_'))
        .filter(|token| !token.is_empty())
        .collect::<Vec<_>>();
    if tokens.is_empty() || tokens.len() > 32 {
        return Err(TranslationError("text search needs between 1 and 32 words"));
    }
    Ok(tokens)
}

/// The RE2 search pattern (unanchored) for `Words`: `(?i)\bw1\W+w2\b`.
pub fn words_regex(text: &str) -> Result<String, TranslationError> {
    Ok(format!("(?i)\\b{}\\b", word_tokens(text)?.join("\\W+")))
}

/// `Words` as a whole-value RE2 pattern, for engines whose regexes match the entire value.
pub fn words_value_regex(text: &str) -> Result<String, TranslationError> {
    Ok(format!(
        "(?is).*\\b{}\\b.*",
        word_tokens(text)?.join("\\W+")
    ))
}

/// Reads a pattern written by `words_regex` or `words_value_regex` back into its words.
pub fn read_words_regex(pattern: &str) -> Option<String> {
    let inner = pattern
        .strip_prefix("(?is).*\\b")
        .and_then(|rest| rest.strip_suffix("\\b.*"))
        .or_else(|| {
            pattern
                .strip_prefix("(?i)\\b")
                .and_then(|rest| rest.strip_suffix("\\b"))
        })?;
    let words = inner.split("\\W+").collect::<Vec<_>>();
    words
        .iter()
        .all(|word| {
            !word.is_empty()
                && word
                    .chars()
                    .all(|character| character.is_alphanumeric() || character == '_')
        })
        .then(|| words.join(" "))
}

/// A finite decimal or scientific number literal, as SQL, LogQL, and PromQL write it.
pub fn is_number(text: &str) -> bool {
    !text.is_empty()
        && text.len() <= 32
        && text
            .bytes()
            .all(|byte| byte.is_ascii_digit() || b".-+eE".contains(&byte))
        && text.parse::<f64>().is_ok_and(f64::is_finite)
}

/// An unquoted literal: a boolean, a number, or otherwise text.
pub fn literal_value(text: &str) -> FilterValue {
    match text {
        "true" => FilterValue::Bool(true),
        "false" => FilterValue::Bool(false),
        _ => serde_json::from_str::<Number>(text)
            .ok()
            .filter(|_| is_number(text))
            .map_or_else(|| FilterValue::String(text.to_owned()), FilterValue::Number),
    }
}

#[cfg(test)]
mod tests;
