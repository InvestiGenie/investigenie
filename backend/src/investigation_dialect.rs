//! Contract for translating filters between datasource query languages.
//!
//! A data connector supports AI investigations, and cross-language Explore queries, by adding
//! `investigation.rs` beside its `mod.rs` with `pub fn dialect() -> &'static dyn
//! InvestigationDialect`. `backend/build.rs` discovers the file, registers the dialect, and
//! derives the descriptor's `ai_investigation` flag and `query_languages` list from it.
//!
//! Every dialect writes (`translate`) and reads (`read`) the same portable subset of its
//! language: field comparisons joined by AND. Explore translates a query from one language to
//! another by reading it with the source dialect and writing it with the target's. Anything
//! outside the subset is rejected, never approximated.

use std::collections::BTreeMap;

use serde_json::{json, Number, Value};

use crate::data_connectors::investigation_dialects;
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

/// A filter read from a query, before the target Data View supplies its field type.
#[derive(Clone, Debug, PartialEq)]
pub struct ParsedFilter {
    pub field: String,
    pub operator: FilterOperator,
    pub value: FilterValue,
}

impl ParsedFilter {
    pub fn new(field: impl Into<String>, operator: FilterOperator, value: FilterValue) -> Self {
        Self {
            field: field.into(),
            operator,
            value,
        }
    }
}

/// A sanitized translation failure that is safe to show to the user.
#[derive(Debug, PartialEq, Eq)]
pub struct TranslationError(pub &'static str);

pub trait InvestigationDialect: Send + Sync {
    /// The query language `translate` writes; equal to the descriptor's `query_language`.
    fn language(&self) -> &'static str;
    /// A query in the portable subset `read` accepts, offered as Explore's placeholder.
    fn example(&self) -> &'static str;
    /// Reads a query back into filters: the inverse of `translate` for AND-combined field
    /// comparisons. Anything outside that subset is an error, never an approximation.
    fn read(&self, query: &str) -> Result<Vec<ParsedFilter>, TranslationError>;
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
        return Err(TranslationError(
            "this operator is not supported for this field",
        ));
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

/// Reads `query` in `language`, types each field from the target Data View, validates the
/// filters against the target dialect, and writes them in the target's language.
pub fn translate_query(
    language: &str,
    query: &str,
    target: &dyn InvestigationDialect,
    index: &str,
    field_types: &BTreeMap<String, String>,
) -> Result<String, QueryTranslationError> {
    let source = dialect_for_language(language).ok_or(QueryTranslationError::UnknownLanguage)?;
    let filters = source
        .read(query)
        .map_err(|error| QueryTranslationError::Read(source.language(), error))?;
    let mut predicates = Vec::with_capacity(filters.len());
    for filter in filters {
        let field_type = field_types
            .get(&filter.field)
            .ok_or_else(|| QueryTranslationError::UnknownField(truncated(&filter.field)))?;
        let predicate = FilterPredicate {
            field: filter.field,
            field_type: field_type.clone(),
            operator: filter.operator,
            value: filter.value,
        };
        validate(target, &predicate).map_err(|error| {
            QueryTranslationError::Unsupported(truncated(&predicate.field), error)
        })?;
        predicates.push(predicate);
    }
    target
        .translate(index, &predicates)
        .map_err(|error| QueryTranslationError::Unsupported(String::new(), error))
}

fn truncated(field: &str) -> String {
    field.chars().take(64).collect()
}

/// A double-quoted literal for LogQL and PromQL; JSON escaping is a subset of Go's escapes.
pub fn string_literal(value: &str) -> String {
    Value::from(value).to_string()
}

/// Reads a double-quoted literal at the start of `input`, returning it and the rest.
pub fn read_string_literal(input: &str) -> Option<(String, &str)> {
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
        if REGEX_METACHARACTERS.contains(character) {
            quoted.push('\\');
        }
        quoted.push(character);
    }
    quoted
}

const REGEX_METACHARACTERS: &str = r"\.+*?()|[]{}^$";

/// The fully anchored regex LogQL and PromQL use for `contains`.
pub fn contains_regex(value: &str) -> String {
    format!(".*{}.*", quote_meta(value))
}

/// The text of a regex written by `contains_regex`, or `None` for any other regex.
pub fn read_contains_regex(regex: &str) -> Option<String> {
    let inner = regex.strip_prefix(".*")?.strip_suffix(".*")?;
    let mut text = String::with_capacity(inner.len());
    let mut characters = inner.chars();
    while let Some(character) = characters.next() {
        match character {
            '\\' => {
                let escaped = characters.next()?;
                if !REGEX_METACHARACTERS.contains(escaped) {
                    return None;
                }
                text.push(escaped);
            }
            _ if REGEX_METACHARACTERS.contains(character) => return None,
            _ => text.push(character),
        }
    }
    // `.*.*` cannot be told apart from a broader regex, so an empty `contains` is rejected.
    (!text.is_empty()).then_some(text)
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
mod tests {
    use super::*;
    use crate::data_connectors::{investigation_dialect, investigation_dialects, registered};

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

    /// Every dialect reads back exactly what it writes, for each field type and operator it
    /// allows, with values chosen to break quoting and escaping.
    #[test]
    fn every_dialect_reads_back_what_it_writes() {
        let values = [
            FilterValue::String("checkout".to_owned()),
            FilterValue::String("say \"hi\" \\ now".to_owned()),
            FilterValue::String("a b\tc".to_owned()),
            FilterValue::String("50%_off".to_owned()),
            FilterValue::String("web.v2(x)|y*?".to_owned()),
            FilterValue::String("it's; DROP".to_owned()),
            FilterValue::String("ends with \\".to_owned()),
            FilterValue::String("{}[]:/".to_owned()),
            FilterValue::String("AND".to_owned()),
            FilterValue::String("1.5s".to_owned()),
            FilterValue::Number(500.into()),
            FilterValue::Number(Number::from_f64(-2.5).unwrap()),
            FilterValue::Bool(true),
        ];
        let mut round_trips = 0;
        for dialect in investigation_dialects() {
            let language = dialect.language();
            for field_type in FIELD_TYPES {
                let field = match (language, field_type) {
                    ("LogQL", "text") => "_msg",
                    (_, "sample value") => "_value",
                    _ => "status",
                };
                for &operator in dialect.field_operators(field_type) {
                    for value in &values {
                        let filter = predicate(field, field_type, operator, value.clone());
                        if validate(dialect, &filter).is_err() {
                            continue;
                        }
                        let query = dialect.translate("events", &[filter]).unwrap();
                        let read = dialect.read(&query).unwrap_or_else(|error| {
                            panic!("{language} cannot read {query}: {}", error.0)
                        });
                        assert_eq!(read.len(), 1, "{language}: {query}");
                        assert_eq!(read[0].field, field, "{language}: {query}");
                        assert_eq!(read[0].operator, operator, "{language}: {query}");
                        assert_eq!(read[0].value.text(), value.text(), "{language}: {query}");
                        round_trips += 1;
                    }
                }
            }
        }
        assert!(round_trips > 200, "{round_trips}");
    }

    /// The same filter written in each language reaches the same target query.
    #[test]
    fn queries_translate_between_every_pair_of_languages() {
        let sources = [
            (
                "Lucene",
                r#"service:"check out" AND status:[500 TO *] AND path:*api\ v1*"#,
            ),
            (
                "SQL",
                "service = 'check out' AND status >= 500 AND path LIKE '%api v1%'",
            ),
            (
                "LogQL",
                r#"{service="check out", path=~".*api v1.*"} | json | status >= 500"#,
            ),
        ];
        let es_types = BTreeMap::from([
            ("service".to_owned(), "keyword".to_owned()),
            ("status".to_owned(), "long".to_owned()),
            ("path".to_owned(), "keyword".to_owned()),
        ]);
        let ch_types = BTreeMap::from([
            ("service".to_owned(), "String".to_owned()),
            ("status".to_owned(), "UInt16".to_owned()),
            ("path".to_owned(), "String".to_owned()),
        ]);
        let loki_types = BTreeMap::from([
            ("service".to_owned(), "label".to_owned()),
            ("status".to_owned(), "json int".to_owned()),
            ("path".to_owned(), "label".to_owned()),
        ]);
        for (language, query) in sources {
            let to = |id: &str, types: &BTreeMap<String, String>| {
                translate_query(
                    language,
                    query,
                    investigation_dialect(id).unwrap(),
                    "events",
                    types,
                )
                .unwrap_or_else(|error| panic!("{language} to {id}: {error}"))
            };
            // Elasticsearch disables leading wildcards, so `contains` is refused, not approximated.
            let refused = translate_query(
                language,
                query,
                investigation_dialect("elasticsearch").unwrap(),
                "events",
                &es_types,
            )
            .unwrap_err();
            assert_eq!(
                refused.to_string(),
                "Field \"path\": this operator is not supported for this field",
                "{language}"
            );
            // Filter order follows the source query, so compare the order-free sets of clauses.
            let sql = to("clickhouse", &ch_types);
            let mut clauses = sql
                .strip_prefix("SELECT * FROM `events` WHERE ")
                .unwrap()
                .split(" AND ")
                .collect::<Vec<_>>();
            clauses.sort_unstable();
            assert_eq!(
                clauses,
                [
                    "`path` LIKE '%api v1%'",
                    "`service` = 'check out'",
                    "`status` >= 500"
                ],
                "{language}"
            );
            let logql = to("loki", &loki_types);
            assert!(
                logql.contains(r#"service="check out""#) && logql.contains(r#"path=~".*api v1.*""#),
                "{logql}"
            );
            assert!(
                logql.ends_with(r#"| json | status >= 500 | __error__="""#),
                "{logql}"
            );
        }
        let promql = translate_query(
            "Lucene",
            r#"__name__:"up" AND job:*prom* AND _value:[1 TO *]"#,
            investigation_dialect("mimir").unwrap(),
            "metrics",
            &BTreeMap::from([
                ("__name__".to_owned(), "label".to_owned()),
                ("job".to_owned(), "label".to_owned()),
                ("_value".to_owned(), "sample value".to_owned()),
            ]),
        )
        .unwrap();
        assert_eq!(promql, r#"({__name__="up", job=~".*prom.*"}) >= 1"#);
    }

    #[test]
    fn untranslatable_queries_explain_why() {
        let types = BTreeMap::from([("status".to_owned(), "label".to_owned())]);
        let loki = investigation_dialect("loki").unwrap();
        let error = |language: &str, query: &str| {
            translate_query(language, query, loki, "logs", &types)
                .unwrap_err()
                .to_string()
        };
        assert_eq!(
            error("Cobol", "x"),
            "This query language cannot be translated"
        );
        assert!(error("SQL", "status = 1 OR status = 2").starts_with("SQL query: only AND"));
        assert_eq!(
            error("sql", "service = 'api'"),
            "Field \"service\" is not in this Data View"
        );
        assert_eq!(
            error("SQL", "status > 500"),
            "Field \"status\": this operator is not supported for this field"
        );
        assert!(dialect_for_language(" lucene ").is_some());
    }

    #[test]
    fn every_dialect_names_its_descriptor_language_and_lists_the_others() {
        for descriptor in registered() {
            let Some(dialect) = investigation_dialect(descriptor.id) else {
                assert!(descriptor.query_languages.is_empty(), "{}", descriptor.id);
                continue;
            };
            assert_eq!(
                dialect.language(),
                descriptor.query_language,
                "{}",
                descriptor.id
            );
            assert_eq!(
                descriptor.query_languages[0]["language"],
                descriptor.query_language
            );
            let languages = descriptor
                .query_languages
                .iter()
                .map(|language| language["language"].as_str().unwrap())
                .collect::<Vec<_>>();
            assert_eq!(languages.len(), 4, "{languages:?}");
            for language in &languages {
                let example = dialect_for_language(language).unwrap().example();
                assert!(
                    dialect_for_language(language)
                        .unwrap()
                        .read(example)
                        .is_ok(),
                    "{language}: {example}"
                );
            }
        }
    }

    /// Seeds one dataset into each configured engine, then runs the same filters written in
    /// Lucene, SQL, and LogQL against every engine and expects identical records back.
    #[tokio::test]
    #[ignore = "requires TEST_ELASTICSEARCH_URL, TEST_CLICKHOUSE_URL, TEST_LOKI_URL, and/or TEST_MIMIR_URL"]
    async fn live_translated_queries_return_the_same_records_in_every_engine() {
        use crate::data_connectors::connect;
        use crate::search::SearchRequest;
        use std::collections::BTreeSet;

        let run = format!("x{}", chrono::Utc::now().timestamp_millis());
        let now = chrono::Utc::now().timestamp_millis();
        let records = [
            (1, "checkout", 503, 2.5, "/api/v1/pay", "upstream timeout"),
            (2, "checkout", 200, 0.1, "/api/v1/cart", "ok"),
            (3, "search", 504, 0.4, "/api/v2/find", "gateway timeout"),
            (
                4,
                "checkout",
                500,
                1.2,
                "/api v1/it's \"x\"",
                "boom \\ 50%_off",
            ),
            (5, "search", 200, 0.2, "/health", "ok"),
        ];
        // Every query is scoped to this run, since Loki streams are shared across runs.
        // Expected record IDs, whether the query uses `contains`, and its three spellings.
        type CrossQuery = (Vec<u64>, bool, [(&'static str, String); 3]);
        let queries: Vec<CrossQuery> = vec![
            (
                vec![1, 4],
                false,
                [
                    (
                        "Lucene",
                        format!(r#"run:"{run}" AND service:"checkout" AND status:[500 TO *]"#),
                    ),
                    (
                        "SQL",
                        format!("run = '{run}' AND service = 'checkout' AND status >= 500"),
                    ),
                    (
                        "LogQL",
                        format!(r#"{{run="{run}", service="checkout"}} | json | status >= 500"#),
                    ),
                ],
            ),
            (
                vec![4],
                true,
                [
                    ("Lucene", format!(r#"run:"{run}" AND path:*it's\ \"x\"*"#)),
                    (
                        "SQL",
                        format!(r#"run = '{run}' AND path LIKE '%it\'s "x"%'"#),
                    ),
                    (
                        "LogQL",
                        format!(r#"{{run="{run}"}} | json | path=~".*it's \"x\".*""#),
                    ),
                ],
            ),
            (
                vec![1, 4],
                false,
                [
                    (
                        "Lucene",
                        format!(r#"run:"{run}" AND latency:{{1 TO *] AND status:[* TO 600}}"#),
                    ),
                    (
                        "SQL",
                        format!("run = '{run}' AND latency > 1 AND status < 600"),
                    ),
                    (
                        "LogQL",
                        format!(r#"{{run="{run}"}} | json | latency > 1 | status < 600"#),
                    ),
                ],
            ),
            (
                vec![4],
                false,
                [
                    (
                        "Lucene",
                        format!(r#"run:"{run}" AND message:"boom \\ 50%_off""#),
                    ),
                    (
                        "SQL",
                        format!(r#"run = '{run}' AND message = 'boom \\ 50%_off'"#),
                    ),
                    (
                        "LogQL",
                        format!(r#"{{run="{run}"}} | json | message="boom \\ 50%_off""#),
                    ),
                ],
            ),
            (
                vec![1, 3],
                true,
                [
                    ("Lucene", format!(r#"run:"{run}" AND message:*timeout*"#)),
                    ("SQL", format!("run = '{run}' AND message LIKE '%timeout%'")),
                    (
                        "LogQL",
                        format!(r#"{{run="{run}"}} | json | message=~".*timeout.*""#),
                    ),
                ],
            ),
        ];
        let client = reqwest::Client::new();
        let mut engines = Vec::new();
        if let Ok(url) = std::env::var("TEST_ELASTICSEARCH_URL") {
            let index = format!("ig-cross-{run}");
            client
                .put(format!("{url}/{index}"))
                .json(&json!({"mappings":{"properties":{
                    "run":{"type":"keyword"},"rid":{"type":"long"},"service":{"type":"keyword"},
                    "status":{"type":"long"},"latency":{"type":"double"},"path":{"type":"keyword"},
                    "message":{"type":"keyword"},"@timestamp":{"type":"date"}}}}))
                .send()
                .await
                .unwrap()
                .error_for_status()
                .unwrap();
            let mut bulk = String::new();
            for (rid, service, status, latency, path, message) in records {
                bulk.push_str(&format!(
                    "{}\n{}\n",
                    json!({"index":{}}),
                    json!({
                    "run":run,"rid":rid,"service":service,"status":status,"latency":latency,
                    "path":path,"message":message,"@timestamp":now})
                ));
            }
            client
                .post(format!("{url}/{index}/_bulk?refresh=true"))
                .header("content-type", "application/x-ndjson")
                .body(bulk)
                .send()
                .await
                .unwrap()
                .error_for_status()
                .unwrap();
            engines.push(("elasticsearch", url, index, "@timestamp"));
        }
        if let Ok(url) = std::env::var("TEST_CLICKHOUSE_URL") {
            let table = format!("ig_cross_{run}");
            for statement in [
                format!("CREATE TABLE {table} (run String, rid UInt8, service String, status UInt16, latency Float64, path String, message String, ts DateTime64(3)) ENGINE = MergeTree ORDER BY rid"),
                format!("INSERT INTO {table} FORMAT JSONEachRow\n{}", records.iter().map(|(rid, service, status, latency, path, message)| json!({
                    "run":run,"rid":rid,"service":service,"status":status,"latency":latency,
                    "path":path,"message":message,"ts":chrono::DateTime::from_timestamp_millis(now).unwrap().format("%Y-%m-%d %H:%M:%S%.3f").to_string()}).to_string()).collect::<Vec<_>>().join("\n")),
            ] {
                client.post(&url).body(statement).send().await.unwrap().error_for_status().unwrap();
            }
            engines.push(("clickhouse", url, table, "ts"));
        }
        if let Ok(url) = std::env::var("TEST_LOKI_URL") {
            let streams = records
                .iter()
                .map(|(rid, service, status, latency, path, message)| json!({
                    "stream":{"run":run,"service":service},
                    "values":[[((now - 1_000) * 1_000_000 + *rid as i64).to_string(), json!({
                        "rid":rid,"status":status,"latency":latency,"path":path,"message":message}).to_string()]]
                }))
                .collect::<Vec<_>>();
            client
                .post(format!("{url}/loki/api/v1/push"))
                .json(&json!({"streams":streams}))
                .send()
                .await
                .unwrap()
                .error_for_status()
                .unwrap();
            engines.push(("loki", url, "logs".to_owned(), "_time"));
        }
        for (id, url, index, time_field) in &engines {
            let connector = connect(id, json!({"url":url}).as_object().unwrap()).unwrap();
            let target = crate::data_connectors::investigation_dialect(id).unwrap();
            // Loki detects parsed fields from recent lines, so wait until this run's fields appear.
            let mut field_types = BTreeMap::new();
            for _ in 0..60 {
                field_types = connector
                    .fields(index)
                    .await
                    .unwrap()
                    .into_iter()
                    .map(|field| (field.name, field.field_type))
                    .collect::<BTreeMap<_, _>>();
                if ["rid", "status", "latency", "path", "message"]
                    .iter()
                    .all(|field| field_types.contains_key(*field))
                {
                    break;
                }
                tokio::time::sleep(std::time::Duration::from_millis(500)).await;
            }
            for (expected, contains, sources) in &queries {
                for (language, query) in sources {
                    let translated = translate_query(language, query, target, index, &field_types);
                    if *id == "elasticsearch" && *contains {
                        // Contains needs a leading wildcard, which the connector disables.
                        let error = translated.unwrap_err().to_string();
                        assert!(error.contains("not supported"), "{language}: {error}");
                        continue;
                    }
                    let native = translated
                        .unwrap_or_else(|error| panic!("{language} to {id}: {error}\n{query}"));
                    let request: SearchRequest = serde_json::from_value(json!({
                        "index":index,"query":native,"size":50,"include_histogram":false,
                        "histogram_field":time_field,
                        "time_range":{"start":now - 600_000,"end":now + 60_000}
                    }))
                    .unwrap();
                    let mut found = BTreeSet::new();
                    for _ in 0..20 {
                        let result = connector
                            .search(request_clone(&request))
                            .await
                            .unwrap_or_else(|error| {
                                panic!("{language} to {id}: {} for {native}", error.message)
                            });
                        found = result["hits"]
                            .as_array()
                            .unwrap()
                            .iter()
                            .map(|hit| {
                                let rid = &hit["source"]["rid"];
                                rid.as_u64().or_else(|| rid.as_str()?.parse().ok()).unwrap()
                            })
                            .collect();
                        if found.iter().copied().eq(expected.iter().copied()) {
                            break;
                        }
                        tokio::time::sleep(std::time::Duration::from_millis(250)).await;
                    }
                    assert_eq!(
                        found.into_iter().collect::<Vec<_>>(),
                        *expected,
                        "{language} to {id}: {native}"
                    );
                }
            }
        }
        if let Ok(url) = std::env::var("TEST_MIMIR_URL") {
            let connector = connect("mimir", json!({"url":url}).as_object().unwrap()).unwrap();
            let field_types = connector
                .fields("metrics")
                .await
                .unwrap()
                .into_iter()
                .map(|field| (field.name, field.field_type))
                .collect::<BTreeMap<_, _>>();
            for (language, query) in [
                (
                    "SQL",
                    "__name__ = 'up' AND job LIKE '%prom%' AND _value >= 1",
                ),
                (
                    "Lucene",
                    r#"__name__:"up" AND job:*prom* AND _value:[1 TO *]"#,
                ),
                ("LogQL", r#"{__name__="up", job=~".*prom.*"}"#),
            ] {
                let native = translate_query(
                    language,
                    query,
                    crate::data_connectors::investigation_dialect("mimir").unwrap(),
                    "metrics",
                    &field_types,
                )
                .unwrap_or_else(|error| panic!("{language} to mimir: {error}"));
                let request: SearchRequest = serde_json::from_value(json!({
                    "index":"metrics","query":native,"size":10,"histogram_field":"_time",
                    "time_range":{"start":now - 120_000,"end":now}
                }))
                .unwrap();
                let result = connector.search(request).await.unwrap();
                assert!(
                    result["total"].as_u64().unwrap() > 0,
                    "{language}: {native}"
                );
                for hit in result["hits"].as_array().unwrap() {
                    assert_eq!(hit["source"]["__name__"], "up", "{native}");
                }
            }
        }
    }

    fn request_clone(request: &crate::search::SearchRequest) -> crate::search::SearchRequest {
        serde_json::from_value(json!({
            "index":request.index,"query":request.query,"size":request.size,
            "include_histogram":request.include_histogram,"histogram_field":request.histogram_field,
            "time_range":request.time_range.map(|range| json!({"start":range.start,"end":range.end}))
        }))
        .unwrap()
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
