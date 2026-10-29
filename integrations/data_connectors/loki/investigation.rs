use super::{valid_label_name, FieldKind, Parser, ValueType};
use crate::investigation_dialect::{
    contains_regex, is_number, literal_value, read_contains_regex, read_string_literal,
    string_literal, FilterOperator, FilterPredicate, FilterValue, InvestigationDialect,
    ParsedFilter, TranslationError,
};

struct LogQlDialect;

pub fn dialect() -> &'static dyn InvestigationDialect {
    &LogQlDialect
}

const STRING_OPERATORS: &[FilterOperator] = &[FilterOperator::Eq, FilterOperator::Contains];
const LINE_OPERATORS: &[FilterOperator] = &[FilterOperator::Contains];
const NUMBER_OPERATORS: &[FilterOperator] = &[
    FilterOperator::Eq,
    FilterOperator::Gt,
    FilterOperator::Gte,
    FilterOperator::Lt,
    FilterOperator::Lte,
];
const QUANTITY_OPERATORS: &[FilterOperator] = &[
    FilterOperator::Gt,
    FilterOperator::Gte,
    FilterOperator::Lt,
    FilterOperator::Lte,
];
const BOOLEAN_OPERATORS: &[FilterOperator] = &[FilterOperator::Eq];

const UNSUPPORTED: TranslationError = TranslationError(
    "only {label=\"…\"} selectors, |= \"…\" line filters, | json, | logfmt, and | field op value label filters can be translated",
);

impl InvestigationDialect for LogQlDialect {
    fn language(&self) -> &'static str {
        "LogQL"
    }

    fn example(&self) -> &'static str {
        "{service=\"checkout\"} |= \"timeout\" | json | status >= 500"
    }

    fn read(&self, query: &str) -> Result<Vec<ParsedFilter>, TranslationError> {
        let mut rest = query.trim();
        let mut filters = Vec::new();
        if rest.is_empty() {
            return Ok(filters);
        }
        rest = rest
            .strip_prefix('{')
            .ok_or(TranslationError("a LogQL query starts with a {stream selector}"))?;
        let mut first = true;
        loop {
            rest = rest.trim_start();
            if let Some(after) = rest.strip_prefix('}') {
                rest = after;
                break;
            }
            if !first {
                rest = rest.strip_prefix(',').ok_or(UNSUPPORTED)?.trim_start();
            }
            first = false;
            let (name, after) = read_name(rest)?;
            let (operator, after) = read_operator(after.trim_start(), &["=~", "!=", "!~", "="])?;
            let (value, after) = read_string_literal(after.trim_start()).ok_or(UNSUPPORTED)?;
            rest = after;
            match operator {
                "=" => filters.push(ParsedFilter::new(name, FilterOperator::Eq, FilterValue::String(value))),
                // The connector's match-all selector for an empty `{}`.
                "=~" if name == "service_name" && value == ".+" => {}
                "=~" => filters.push(contains(name, &value)?),
                _ => return Err(TranslationError("negative matchers cannot be translated")),
            }
        }
        loop {
            rest = rest.trim_start();
            if rest.is_empty() {
                return Ok(filters);
            }
            if let Some(after) = rest.strip_prefix("|=") {
                let (value, after) = read_string_literal(after.trim_start()).ok_or(UNSUPPORTED)?;
                filters.push(ParsedFilter::new("_msg", FilterOperator::Contains, FilterValue::String(value)));
                rest = after;
                continue;
            }
            if rest.starts_with("|~") || rest.starts_with("!=") || rest.starts_with("!~") {
                return Err(TranslationError("only |= line filters can be translated"));
            }
            let stage = rest.strip_prefix('|').ok_or(UNSUPPORTED)?.trim_start();
            let (name, after) = read_name(stage)?;
            if matches!(name, "json" | "logfmt") {
                rest = after.trim_start();
                if !(rest.is_empty() || rest.starts_with('|')) {
                    return Err(TranslationError("parser stages with parameters cannot be translated"));
                }
                continue;
            }
            let (operator, after) =
                read_operator(after.trim_start(), &["==", ">=", "<=", "=~", "!=", "!~", "=", ">", "<"])?;
            let after = after.trim_start();
            match operator {
                "=" | "=~" => {
                    let (value, after) = read_string_literal(after).ok_or(UNSUPPORTED)?;
                    rest = after;
                    if name == "__error__" && operator == "=" && value.is_empty() {
                        continue; // Written after numeric comparisons; implied by the field type.
                    }
                    filters.push(if operator == "=" {
                        ParsedFilter::new(name, FilterOperator::Eq, FilterValue::String(value))
                    } else {
                        contains(name, &value)?
                    });
                }
                "!=" | "!~" => return Err(TranslationError("negative label filters cannot be translated")),
                comparison => {
                    let end = after
                        .find(|character: char| character.is_whitespace() || character == '|')
                        .unwrap_or(after.len());
                    let literal = &after[..end];
                    if literal.is_empty()
                        || !literal
                            .chars()
                            .all(|character| character.is_ascii_alphanumeric() || matches!(character, '.' | '-' | '+' | 'µ'))
                    {
                        return Err(UNSUPPORTED);
                    }
                    let operator = match comparison {
                        "==" => FilterOperator::Eq,
                        ">" => FilterOperator::Gt,
                        ">=" => FilterOperator::Gte,
                        "<" => FilterOperator::Lt,
                        _ => FilterOperator::Lte,
                    };
                    let value = if is_number(literal) {
                        literal_value(literal)
                    } else {
                        FilterValue::String(literal.to_owned())
                    };
                    filters.push(ParsedFilter::new(name, operator, value));
                    rest = &after[end..];
                }
            }
        }
    }

    fn operators(&self) -> &'static [FilterOperator] {
        &FilterOperator::ALL
    }

    /// Field types come from `LokiClient::fields`: stream labels, the `_msg` line, `_time`, and
    /// fields Loki detects in log lines or structured metadata.
    fn field_operators(&self, field_type: &str) -> &'static [FilterOperator] {
        match FieldKind::parse(field_type) {
            Some(FieldKind::Label) => STRING_OPERATORS,
            Some(FieldKind::Line) => LINE_OPERATORS,
            Some(FieldKind::Parsed(_, value_type)) => match value_type {
                ValueType::String => STRING_OPERATORS,
                ValueType::Int | ValueType::Float => NUMBER_OPERATORS,
                ValueType::Duration | ValueType::Bytes => QUANTITY_OPERATORS,
                ValueType::Boolean => BOOLEAN_OPERATORS,
            },
            Some(FieldKind::Time) | None => &[],
        }
    }

    /// Stream labels only: parsed fields exist in rows only when the query parses lines.
    fn aggregatable(&self, field_type: &str) -> bool {
        matches!(FieldKind::parse(field_type), Some(FieldKind::Label))
    }

    fn check(&self, filter: &FilterPredicate) -> Result<(), TranslationError> {
        let kind = FieldKind::parse(&filter.field_type)
            .ok_or(TranslationError("Unsupported Loki field type"))?;
        if !matches!(kind, FieldKind::Line | FieldKind::Time) && !valid_label_name(&filter.field) {
            return Err(TranslationError("Unsupported Loki label name"));
        }
        match kind {
            FieldKind::Line if filter.field != "_msg" => {
                Err(TranslationError("Only _msg is the Loki log line"))
            }
            FieldKind::Parsed(_, value_type) => comparison_literal(value_type, filter).map(|_| ()),
            _ => Ok(()),
        }
    }

    fn translate(&self, _: &str, filters: &[FilterPredicate]) -> Result<String, TranslationError> {
        let mut query = LogQlQuery::default();
        for filter in filters {
            query.add(filter)?;
        }
        Ok(query.render())
    }
}

/// Collects each filter into the LogQL position that can express it, then renders
/// `{matchers} |= line | parser | label filter | __error__=""`. Only matchers that cannot match an
/// empty value go in the stream selector, because Loki rejects selectors without one; with none,
/// the selector stays `{}` and the connector resolves it to every stream.
#[derive(Default)]
struct LogQlQuery {
    matchers: Vec<String>,
    line_filters: Vec<String>,
    parsers: Vec<Parser>,
    label_filters: Vec<String>,
    /// Loki keeps lines whose value fails a numeric conversion, marked with `__error__`.
    drop_conversion_errors: bool,
}

impl LogQlQuery {
    fn add(&mut self, filter: &FilterPredicate) -> Result<(), TranslationError> {
        let label = filter.field.as_str();
        match FieldKind::parse(&filter.field_type) {
            Some(FieldKind::Line) => {
                self.line_filters
                    .push(format!("|= {}", string_literal(&filter.value.text())));
            }
            Some(FieldKind::Label) => {
                let matcher = string_matcher(label, filter)?;
                if filter.value.text().is_empty() {
                    self.label_filters.push(format!("| {matcher}"));
                } else {
                    self.matchers.push(matcher);
                }
            }
            Some(FieldKind::Parsed(parser, value_type)) => {
                if let Some(parser) = parser.filter(|parser| !self.parsers.contains(parser)) {
                    self.parsers.push(parser);
                }
                let expression = match comparison_literal(value_type, filter)? {
                    Some(literal) => {
                        self.drop_conversion_errors = true;
                        format!("{label} {} {literal}", comparison(filter.operator)?)
                    }
                    None => string_matcher(label, filter)?,
                };
                self.label_filters.push(format!("| {expression}"));
            }
            Some(FieldKind::Time) | None => {
                return Err(TranslationError(
                    "Loki time is filtered by the investigation time range",
                ))
            }
        }
        Ok(())
    }

    fn render(&self) -> String {
        let mut query = format!("{{{}}}", self.matchers.join(", "));
        let parsers = self.parsers.iter().map(|parser| match parser {
            Parser::Json => "| json".to_owned(),
            Parser::Logfmt => "| logfmt".to_owned(),
        });
        let errors = self
            .drop_conversion_errors
            .then(|| "| __error__=\"\"".to_owned());
        for stage in self
            .line_filters
            .iter()
            .cloned()
            .chain(parsers)
            .chain(self.label_filters.iter().cloned())
            .chain(errors)
        {
            query.push(' ');
            query.push_str(&stage);
        }
        query
    }
}

fn string_matcher(label: &str, filter: &FilterPredicate) -> Result<String, TranslationError> {
    let value = filter.value.text();
    match filter.operator {
        FilterOperator::Eq => Ok(format!("{label}={}", string_literal(&value))),
        FilterOperator::Contains => Ok(format!("{label}=~{}", string_literal(&contains_regex(&value)))),
        _ => Err(TranslationError("Unsupported filter operator")),
    }
}

/// The unquoted literal for a numeric, duration, or bytes comparison, or `None` when the field
/// compares as a string.
fn comparison_literal(
    value_type: ValueType,
    filter: &FilterPredicate,
) -> Result<Option<String>, TranslationError> {
    let text = filter.value.text();
    let text = text.trim();
    let valid = match value_type {
        ValueType::String => return Ok(None),
        ValueType::Boolean => {
            return match &filter.value {
                FilterValue::Bool(_) => Ok(None),
                FilterValue::String(value) if matches!(value.as_str(), "true" | "false") => Ok(None),
                _ => Err(TranslationError("Boolean filters need true or false")),
            }
        }
        ValueType::Int | ValueType::Float => is_number(text),
        ValueType::Duration => is_quantity(text, &["ns", "us", "µs", "ms", "s", "m", "h"], false),
        ValueType::Bytes => is_quantity(
            text,
            &["B", "kB", "KB", "MB", "GB", "TB", "PB", "KiB", "MiB", "GiB", "TiB", "PiB"],
            true,
        ),
    };
    if valid {
        Ok(Some(text.to_owned()))
    } else {
        Err(TranslationError("Comparison value does not match the field type"))
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

/// A non-negative quantity such as `1.5s`, `2m30s`, or `10MB`; bytes take a single unit.
fn is_quantity(text: &str, units: &[&str], single: bool) -> bool {
    let mut rest = text;
    let mut parts = 0;
    while !rest.is_empty() {
        let digits = rest
            .find(|character: char| !(character.is_ascii_digit() || character == '.'))
            .unwrap_or(rest.len());
        if digits == 0 || rest[..digits].parse::<f64>().is_err() {
            return false;
        }
        rest = &rest[digits..];
        // Longest unit first, so `ms` is not read as `m`.
        let Some(unit) = units
            .iter()
            .filter(|unit| rest.starts_with(**unit))
            .max_by_key(|unit| unit.len())
        else {
            return false;
        };
        rest = &rest[unit.len()..];
        parts += 1;
    }
    parts > 0 && text.len() <= 32 && (!single || parts == 1)
}

fn read_name(input: &str) -> Result<(&str, &str), TranslationError> {
    let end = input
        .find(|character: char| !(character.is_ascii_alphanumeric() || character == '_'))
        .unwrap_or(input.len());
    let name = &input[..end];
    if !valid_label_name(name) {
        return Err(UNSUPPORTED);
    }
    Ok((name, &input[end..]))
}

/// The first of `operators` that `input` starts with; longer spellings are listed first.
fn read_operator<'a>(
    input: &'a str,
    operators: &[&'static str],
) -> Result<(&'static str, &'a str), TranslationError> {
    operators
        .iter()
        .find_map(|operator| input.strip_prefix(operator).map(|rest| (*operator, rest)))
        .ok_or(UNSUPPORTED)
}

fn contains(field: &str, regex: &str) -> Result<ParsedFilter, TranslationError> {
    let text = read_contains_regex(regex)
        .ok_or(TranslationError("only \".*text.*\" regexes (contains) can be translated"))?;
    Ok(ParsedFilter::new(field, FilterOperator::Contains, FilterValue::String(text)))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::investigation_dialect::build_query;

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
    fn labels_select_streams_and_messages_filter_lines() {
        let filters = [
            filter("app", "label", FilterOperator::Eq, text("api")),
            filter("_msg", "text", FilterOperator::Contains, text("say \"hi\" \\ now")),
            filter("pod", "label", FilterOperator::Contains, text("web.v2(a)")),
        ];
        assert_eq!(
            build_query(dialect(), "logs", &filters).unwrap(),
            r#"{app="api", pod=~".*web\\.v2\\(a\\).*"} |= "say \"hi\" \\ now""#
        );
    }

    #[test]
    fn parsed_fields_add_their_parser_and_numeric_comparisons_drop_conversion_errors() {
        let filters = [
            filter("app", "label", FilterOperator::Eq, text("api")),
            filter("status", "json int", FilterOperator::Gte, FilterValue::Number(500.into())),
            filter("latency", "json float", FilterOperator::Lt, text("0.25")),
            filter("user_tier", "json string", FilterOperator::Contains, text("gold")),
            filter("duration", "logfmt duration", FilterOperator::Gt, text("1.5s")),
            filter("cached", "json boolean", FilterOperator::Eq, FilterValue::Bool(true)),
            filter("trace_id", "metadata string", FilterOperator::Eq, text("abc")),
        ];
        assert_eq!(
            build_query(dialect(), "logs", &filters).unwrap(),
            r#"{app="api"} | json | logfmt | status >= 500 | latency < 0.25 | user_tier=~".*gold.*" | duration > 1.5s | cached="true" | trace_id="abc" | __error__="""#
        );
        let strings_only = [filter("msg", "json string", FilterOperator::Eq, text("ok"))];
        assert_eq!(
            build_query(dialect(), "logs", &strings_only).unwrap(),
            r#"{} | json | msg="ok""#
        );
    }

    #[test]
    fn selectors_without_a_non_empty_matcher_are_left_for_the_connector() {
        assert_eq!(build_query(dialect(), "logs", &[]).unwrap(), "{}");
        let filters = [
            filter("_msg", "text", FilterOperator::Contains, text("timeout")),
            filter("level", "label", FilterOperator::Eq, text("")),
        ];
        assert_eq!(
            build_query(dialect(), "logs", &filters).unwrap(),
            r#"{} |= "timeout" | level="""#
        );
    }

    #[test]
    fn values_that_do_not_fit_the_field_type_are_rejected() {
        for rejected in [
            filter("_time", "timestamp", FilterOperator::Eq, text("x")),
            filter("_msg", "text", FilterOperator::Eq, text("x")),
            filter("app", "label", FilterOperator::Gt, text("5")),
            filter("status", "json int", FilterOperator::Gt, text("500 or 1")),
            filter("status", "json int", FilterOperator::Contains, text("5")),
            filter("latency", "json float", FilterOperator::Gt, text("NaN")),
            filter("duration", "logfmt duration", FilterOperator::Gt, text("1.5")),
            filter("duration", "logfmt duration", FilterOperator::Gt, text("1s; drop")),
            filter("size", "json bytes", FilterOperator::Gt, text("1MB2KB")),
            filter("cached", "json boolean", FilterOperator::Eq, text("yes")),
            filter("service.name", "label", FilterOperator::Eq, text("x")),
            filter("status", "yaml int", FilterOperator::Eq, text("1")),
        ] {
            assert!(
                build_query(dialect(), "logs", &[rejected]).is_err(),
                "{:?}",
                build_query(dialect(), "logs", &[])
            );
        }
        let filters = [
            filter("duration", "json duration", FilterOperator::Lte, text("2m30s")),
            filter("size", "json bytes", FilterOperator::Gt, text("10MiB")),
        ];
        assert_eq!(
            build_query(dialect(), "logs", &filters).unwrap(),
            r#"{} | json | duration <= 2m30s | size > 10MiB | __error__="""#
        );
    }

    #[test]
    fn reading_accepts_the_portable_subset_and_the_connector_scaffolding() {
        let read = dialect()
            .read(r#"{service_name=~".+", app="api", pod=~".*web\\.v2.*"} |= "time out" | json | logfmt | status >= 500 | duration > 1.5s | level=~".*err.*" | trace="" | __error__="""#)
            .unwrap();
        assert_eq!(
            read,
            [
                ParsedFilter::new("app", FilterOperator::Eq, text("api")),
                ParsedFilter::new("pod", FilterOperator::Contains, text("web.v2")),
                ParsedFilter::new("_msg", FilterOperator::Contains, text("time out")),
                ParsedFilter::new("status", FilterOperator::Gte, FilterValue::Number(500.into())),
                ParsedFilter::new("duration", FilterOperator::Gt, text("1.5s")),
                ParsedFilter::new("level", FilterOperator::Contains, text("err")),
                ParsedFilter::new("trace", FilterOperator::Eq, text("")),
            ]
        );
        assert_eq!(dialect().read("{}").unwrap(), []);
        assert_eq!(dialect().read("").unwrap(), []);
    }

    #[test]
    fn reading_rejects_anything_it_cannot_translate_exactly() {
        for query in [
            r#"app="api""#,
            r#"{app!="api"}"#,
            r#"{app=~"api|web"}"#,
            r#"{app="api"} |~ "time.*out""#,
            r#"{app="api"} != "debug""#,
            r#"{app="api"} | json status="code""#,
            r#"{app="api"} | line_format "{{.msg}}""#,
            r#"{app="api"} | status != 500"#,
            r#"{app="api"} | status > 500 or status < 100"#,
            r#"sum(count_over_time({app="api"}[5m]))"#,
            r#"{app="api" | json"#,
            r#"{app="api"} | status > "500""#,
        ] {
            assert!(dialect().read(query).is_err(), "{query}");
        }
    }

    #[test]
    fn operators_and_breakdowns_follow_the_field_type() {
        assert_eq!(dialect().field_operators("label"), STRING_OPERATORS);
        assert_eq!(dialect().field_operators("text"), LINE_OPERATORS);
        assert_eq!(dialect().field_operators("json int"), NUMBER_OPERATORS);
        assert_eq!(dialect().field_operators("logfmt bytes"), QUANTITY_OPERATORS);
        assert!(dialect().field_operators("timestamp").is_empty());
        assert!(dialect().aggregatable("label"));
        assert!(!dialect().aggregatable("json string"));
    }
}
