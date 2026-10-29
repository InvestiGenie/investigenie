//! LogQL for Loki: log queries (filters) and metric queries (computations).
//!
//! A filter becomes `{stream matchers} |= line filters | parsers | label filter`. Only
//! conditions that select streams by a non-empty value can anchor the stream selector; with none,
//! it stays `{}` and the connector resolves it to every stream. LogQL has no `not`, so negation is
//! pushed to each condition; numeric comparisons are guarded with a number pattern so values that
//! are not numbers neither match nor raise errors (`(f=~NUM and f >= 5)`, negated
//! `(f!~NUM or f < 5)`). Free text inside OR/NOT copies the line into the `ig_line` label first.

use super::{valid_label_name, FieldKind, Parser, ValueType};
use crate::investigation_dialect::{
    attribute_key, format_duration, is_message_field, is_number, label_name, literal_value,
    matching::{classify_search_pattern, classify_value_pattern, search_pattern, value_pattern},
    parse_duration, read_string_literal, string_literal, word_tokens, words_value_regex, Cmp,
    Field, Expr, FilterOperator, FilterValue, InvestigationDialect, Leaf, MetricQuery, Op,
    PortableRegex, Query, RangeFunction, Schema, TranslationError, WriteContext,
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

/// Values Go's `strconv.ParseFloat` reads, except NaN and infinities.
const NUMBER_GUARD: &str = r"[-+]?([0-9]+(\.[0-9]*)?|\.[0-9]+)([eE][-+]?[0-9]+)?";
const DURATION_GUARD: &str = r"([0-9]+(\.[0-9]*)?(ns|us|µs|ms|s|m|h))+";
const BYTES_GUARD: &str = r"[0-9]+(\.[0-9]*)? ?([KMGTPE]i?B|[kKMGTPE]B|B)";
const LINE_LABEL: &str = "ig_line";
const UNSUPPORTED: TranslationError = TranslationError(
    "only stream selectors, line filters, | json, | logfmt, label filters, and sum/min/max of count_over_time, rate, sum_over_time, min_over_time, or max_over_time can be translated",
);

impl InvestigationDialect for LogQlDialect {
    fn language(&self) -> &'static str {
        "LogQL"
    }

    fn example(&self) -> &'static str {
        "{service=\"checkout\"} |= \"timeout\" | json | status >= 500"
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

    /// Exact names, then label spellings of attributes (`service.name` → `service_name`), and
    /// `_msg` for common message field names.
    fn resolve_field(&self, name: &str, schema: &Schema) -> Option<Field> {
        let key = attribute_key(name);
        let mut candidates = vec![name.to_owned(), label_name(key), label_name(name)];
        if is_message_field(name) {
            candidates.push("_msg".to_owned());
        }
        candidates
            .into_iter()
            .find_map(|candidate| schema.get(&candidate).map(|field_type| Field::typed(candidate, field_type.clone())))
    }

    fn check_leaf(&self, leaf: &Leaf) -> Result<(), TranslationError> {
        let Some(field) = &leaf.field else {
            return match &leaf.op {
                Op::Words(text) => word_tokens(text).map(|_| ()),
                _ => Err(TranslationError("only free-text words can omit a field")),
            };
        };
        let kind = FieldKind::parse(&field.field_type)
            .ok_or(TranslationError("Unsupported Loki field type"))?;
        if matches!(kind, FieldKind::Label | FieldKind::Parsed(..)) && !valid_label_name(&field.name) {
            return Err(TranslationError("Unsupported Loki label name"));
        }
        if kind == FieldKind::Line && field.name != "_msg" {
            return Err(TranslationError("Only _msg is the Loki log line"));
        }
        let string_op = matches!(leaf.op, Op::Eq(_) | Op::In(_) | Op::Contains(_) | Op::Prefix(_) | Op::Suffix(_) | Op::Regex(_) | Op::Words(_) | Op::Exists);
        match (kind, &leaf.op) {
            (FieldKind::Time, _) => Err(TranslationError("Loki time is filtered by the time range")),
            (FieldKind::Line, Op::Exists) => Ok(()),
            (FieldKind::Line, Op::Compare(..)) => Err(TranslationError("the log line cannot be compared as a number")),
            (FieldKind::Line | FieldKind::Label, _) if string_op => text_values(&leaf.op),
            (FieldKind::Label, Op::Compare(_, value)) => number_literal(value).map(|_| ()),
            (FieldKind::Parsed(_, value_type), op) => match (value_type, op) {
                (ValueType::String, _) if string_op => text_values(op),
                (ValueType::Int | ValueType::Float, Op::Eq(value) | Op::Compare(_, value)) => number_literal(value).map(|_| ()),
                (ValueType::Int | ValueType::Float, Op::In(values)) => values.iter().try_for_each(|value| number_literal(value).map(|_| ())),
                (ValueType::Duration, Op::Compare(_, value)) => quantity_literal(value, DURATION_GUARD).map(|_| ()),
                (ValueType::Bytes, Op::Compare(_, value)) => quantity_literal(value, BYTES_GUARD).map(|_| ()),
                (ValueType::Boolean, Op::Eq(value)) => match value {
                    FilterValue::Bool(_) => Ok(()),
                    FilterValue::String(text) if matches!(text.as_str(), "true" | "false") => Ok(()),
                    _ => Err(TranslationError("Boolean filters need true or false")),
                },
                (_, Op::Exists) => Ok(()),
                (ValueType::Duration | ValueType::Bytes, _) => Err(TranslationError("duration and size fields support comparisons only")),
                _ => Err(TranslationError("this condition does not fit the field type")),
            },
            _ => Err(TranslationError("this condition does not fit the field type")),
        }
    }

    fn read(&self, query: &str) -> Result<Query, TranslationError> {
        let query = query.trim();
        if query.is_empty() {
            return Ok(Query::Filter(Expr::all()));
        }
        if query.starts_with('{') {
            let mut reader = Reader::new(query);
            let pipeline = reader.pipeline()?;
            if !reader.rest().trim().is_empty() {
                return Err(UNSUPPORTED);
            }
            if pipeline.unwrap.is_some() {
                return Err(TranslationError("unwrap belongs inside a metric query"));
            }
            return Ok(Query::Filter(pipeline.filter));
        }
        let mut reader = Reader::new(query);
        let metric = reader.metric()?;
        if !reader.rest().trim().is_empty() {
            return Err(UNSUPPORTED);
        }
        Ok(Query::Metric(metric))
    }

    fn write(&self, query: &Query, _: &WriteContext) -> Result<String, TranslationError> {
        match query {
            Query::Filter(expr) => Ok(Pipeline::build(expr, &[])?.render(false, None, false)),
            Query::Metric(metric) => write_metric(metric),
        }
    }
}

fn text_values(op: &Op) -> Result<(), TranslationError> {
    match op {
        Op::In(values) if values.is_empty() || values.len() > 256 => {
            Err(TranslationError("a value list needs between 1 and 256 values"))
        }
        Op::Words(text) => word_tokens(text).map(|_| ()),
        _ => Ok(()),
    }
}

fn number_literal(value: &FilterValue) -> Result<String, TranslationError> {
    let text = value.text();
    let text = text.trim();
    if is_number(text) {
        Ok(text.to_owned())
    } else {
        Err(TranslationError("Comparison value does not match the field type"))
    }
}

/// A duration (`1.5s`, `2m30s`) or byte size (`10MiB`) literal.
fn quantity_literal(value: &FilterValue, guard: &str) -> Result<String, TranslationError> {
    let text = value.text();
    let text = text.trim();
    let valid = !text.is_empty()
        && text.len() <= 32
        && if guard == DURATION_GUARD { is_duration(text) } else { is_bytes(text) };
    if valid {
        Ok(text.to_owned())
    } else {
        Err(TranslationError("Comparison value does not match the field type"))
    }
}

fn is_duration(text: &str) -> bool {
    let mut rest = text;
    while !rest.is_empty() {
        let digits = rest.find(|character: char| !(character.is_ascii_digit() || character == '.')).unwrap_or(rest.len());
        if digits == 0 || rest[..digits].parse::<f64>().is_err() {
            return false;
        }
        rest = &rest[digits..];
        let Some(unit) = ["ns", "us", "µs", "ms", "s", "m", "h"].iter().filter(|unit| rest.starts_with(**unit)).max_by_key(|unit| unit.len()) else {
            return false;
        };
        rest = &rest[unit.len()..];
    }
    true
}

fn is_bytes(text: &str) -> bool {
    let digits = text.find(|character: char| !(character.is_ascii_digit() || character == '.')).unwrap_or(text.len());
    if digits == 0 || text[..digits].parse::<f64>().is_err() {
        return false;
    }
    let unit = text[digits..].trim_start();
    ["B", "kB", "KB", "MB", "GB", "TB", "PB", "EB", "KiB", "MiB", "GiB", "TiB", "PiB", "EiB"].contains(&unit)
}

fn kind(leaf: &Leaf) -> FieldKind {
    match &leaf.field {
        None => FieldKind::Line,
        Some(field) => FieldKind::parse(&field.field_type).unwrap_or(FieldKind::Label),
    }
}

/// The literal a parsed numeric, duration, or bytes field compares with; `None` for strings.
fn comparison_literal(leaf: &Leaf, value: &FilterValue) -> Result<Option<(String, &'static str)>, TranslationError> {
    Ok(match kind(leaf) {
        FieldKind::Parsed(_, ValueType::Int | ValueType::Float) | FieldKind::Label => Some((number_literal(value)?, NUMBER_GUARD)),
        FieldKind::Parsed(_, ValueType::Duration) => Some((quantity_literal(value, DURATION_GUARD)?, DURATION_GUARD)),
        FieldKind::Parsed(_, ValueType::Bytes) => Some((quantity_literal(value, BYTES_GUARD)?, BYTES_GUARD)),
        _ => None,
    })
}

#[derive(Default)]
struct Pipeline {
    matchers: Vec<String>,
    line_filters: Vec<String>,
    parsers: Vec<Parser>,
    capture_line: bool,
    label_filter: Option<String>,
}

impl Pipeline {
    /// `extra` fields (group-by and value fields of a computation) also need their parsers.
    fn build(expr: &Expr, extra: &[&Field]) -> Result<Self, TranslationError> {
        let mut pipeline = Self::default();
        let expr = expr.clone().nnf().merge_equalities();
        for leaf in expr.leaves() {
            if let Some(field) = &leaf.field {
                pipeline.add_parser(field);
            }
        }
        for field in extra {
            pipeline.add_parser(field);
        }
        let mut selector = Vec::new();
        let mut rest = Vec::new();
        for conjunct in expr.conjuncts() {
            let literal = as_literal(&conjunct);
            match literal {
                Some((leaf, _)) if kind(leaf) == FieldKind::Label && selector_op(&leaf.op) => selector.push(conjunct),
                Some((leaf, false)) if kind(leaf) == FieldKind::Line && leaf.op == Op::Exists => {}
                Some((leaf, negated)) if kind(leaf) == FieldKind::Line && leaf.op != Op::Exists => {
                    pipeline.line_filters.push(line_filter(leaf, negated)?)
                }
                _ => match &conjunct {
                    Expr::Or(items) if items.iter().all(|item| matches!(as_literal(item), Some((leaf, false)) if kind(leaf) == FieldKind::Line && leaf.op != Op::Exists)) => {
                        let patterns = items
                            .iter()
                            .filter_map(as_literal)
                            .map(|(leaf, _)| search_pattern(&leaf.op, leaf.case_insensitive).map(|pattern| format!("(?:{pattern})")))
                            .collect::<Result<Vec<_>, _>>()?;
                        pipeline.line_filters.push(format!("|~ {}", string_literal(&patterns.join("|"))));
                    }
                    _ => rest.push(conjunct),
                },
            }
        }
        let anchored = selector
            .iter()
            .any(|item| matches!(as_literal(item), Some((leaf, false)) if anchors(leaf)));
        if anchored {
            for item in &selector {
                let (leaf, negated) = as_literal(item).ok_or(UNSUPPORTED)?;
                pipeline.matchers.push(string_condition(leaf, negated)?);
            }
        } else {
            rest.splice(0..0, selector);
        }
        if !rest.is_empty() {
            let mut capture = false;
            pipeline.label_filter = Some(label_expression(&Expr::And(rest).simplify(), &mut capture, true)?);
            pipeline.capture_line = capture;
        }
        Ok(pipeline)
    }

    fn add_parser(&mut self, field: &Field) {
        if let Some(FieldKind::Parsed(Some(parser), _)) = FieldKind::parse(&field.field_type) {
            if !self.parsers.contains(&parser) {
                self.parsers.push(parser);
            }
        }
    }

    /// A computation adds `__error__` handling (`metric`), the value field's number `guard`,
    /// and `unwrap`.
    fn render(&self, metric: bool, guard: Option<&Field>, unwrap: bool) -> String {
        let mut query = format!("{{{}}}", self.matchers.join(", "));
        let mut stage = |text: &str| {
            query.push(' ');
            query.push_str(text);
        };
        for filter in &self.line_filters {
            stage(filter);
        }
        for parser in &self.parsers {
            stage(match parser {
                Parser::Json => "| json",
                Parser::Logfmt => "| logfmt",
            });
        }
        if metric && !self.parsers.is_empty() {
            // Metric queries fail on lines a parser could not read, so those lines are dropped.
            stage("| __error__=\"\"");
        }
        if self.capture_line {
            stage(&format!("| label_format {LINE_LABEL}=\"{{{{__line__}}}}\""));
        }
        if let Some(filter) = &self.label_filter {
            stage(&format!("| {filter}"));
        }
        if self.capture_line {
            stage(&format!("| drop {LINE_LABEL}"));
        }
        if let Some(field) = guard {
            stage(&format!("| {}=~{}", field.name, string_literal(NUMBER_GUARD)));
            if unwrap {
                stage(&format!("| unwrap {}", field.name));
            }
        }
        query
    }
}

/// A condition or a negated condition.
fn as_literal(expr: &Expr) -> Option<(&Leaf, bool)> {
    match expr {
        Expr::Leaf(leaf) => Some((leaf, false)),
        Expr::Not(inner) => match &**inner {
            Expr::Leaf(leaf) => Some((leaf, true)),
            _ => None,
        },
        _ => None,
    }
}

fn selector_op(op: &Op) -> bool {
    !matches!(op, Op::Compare(..))
}

/// Whether a positive stream condition selects streams on its own (matches no empty value).
fn anchors(leaf: &Leaf) -> bool {
    match &leaf.op {
        Op::Eq(value) => !value.text().is_empty(),
        Op::Exists => true,
        op => value_pattern(op, leaf.case_insensitive)
            .ok()
            .flatten()
            .and_then(|pattern| PortableRegex::from_re2(&pattern).ok())
            .is_some_and(|regex| !regex.matches_empty())
            || matches!(op, Op::Words(_)),
    }
}

/// A string condition as a matcher or label filter: `l="v"`, `l!~"p"`, `l!=""`.
fn string_condition(leaf: &Leaf, negated: bool) -> Result<String, TranslationError> {
    let name = &leaf.field.as_ref().ok_or(UNSUPPORTED)?.name;
    Ok(match &leaf.op {
        Op::Exists => format!("{name}{}\"\"", if negated { "=" } else { "!=" }),
        Op::Eq(value) if !leaf.case_insensitive => {
            format!("{name}{}{}", if negated { "!=" } else { "=" }, string_literal(&value.text()))
        }
        op => {
            let pattern = value_pattern(op, leaf.case_insensitive)?.ok_or(UNSUPPORTED)?;
            format!("{name}{}{}", if negated { "!~" } else { "=~" }, string_literal(&pattern))
        }
    })
}

fn line_filter(leaf: &Leaf, negated: bool) -> Result<String, TranslationError> {
    Ok(match &leaf.op {
        Op::Contains(text) if !leaf.case_insensitive => {
            format!("{} {}", if negated { "!=" } else { "|=" }, string_literal(text))
        }
        op => format!(
            "{} {}",
            if negated { "!~" } else { "|~" },
            string_literal(&search_pattern(op, leaf.case_insensitive)?)
        ),
    })
}

/// A label filter expression for a negation-normal-form tree.
fn label_expression(expr: &Expr, capture: &mut bool, top: bool) -> Result<String, TranslationError> {
    Ok(match expr {
        Expr::And(items) | Expr::Or(items) => {
            if items.is_empty() {
                return Err(TranslationError("an empty group cannot be written in LogQL"));
            }
            let separator = if matches!(expr, Expr::And(_)) { " and " } else { " or " };
            let parts = items
                .iter()
                .map(|item| label_expression(item, capture, false))
                .collect::<Result<Vec<_>, _>>()?;
            let joined = parts.join(separator);
            if top { joined } else { format!("({joined})") }
        }
        Expr::Leaf(leaf) => label_leaf(leaf, false, capture)?,
        Expr::Not(inner) => match &**inner {
            Expr::Leaf(leaf) => label_leaf(leaf, true, capture)?,
            _ => return Err(UNSUPPORTED),
        },
    })
}

fn label_leaf(leaf: &Leaf, negated: bool, capture: &mut bool) -> Result<String, TranslationError> {
    if kind(leaf) == FieldKind::Line {
        // Line conditions inside OR/NOT test the line copied into a label.
        *capture = true;
        if let Op::Words(text) = &leaf.op {
            let pattern = words_value_regex(text)?;
            return Ok(format!("{LINE_LABEL}{}{}", if negated { "!~" } else { "=~" }, string_literal(&pattern)));
        }
        let line = Leaf { field: Some(Field::typed(LINE_LABEL, "label")), op: leaf.op.clone(), case_insensitive: leaf.case_insensitive };
        return string_condition(&line, negated);
    }
    let name = &leaf.field.as_ref().ok_or(UNSUPPORTED)?.name;
    match &leaf.op {
        Op::Compare(cmp, value) => {
            let (literal, guard) = comparison_literal(leaf, value)?.ok_or(TranslationError("string fields cannot be compared in LogQL"))?;
            let guard = string_literal(guard);
            Ok(if negated {
                format!("({name}!~{guard} or {name} {} {literal})", cmp.complement().symbol())
            } else {
                format!("({name}=~{guard} and {name} {} {literal})", cmp.symbol())
            })
        }
        Op::Eq(value) if matches!(kind(leaf), FieldKind::Parsed(_, ValueType::Int | ValueType::Float)) => {
            let literal = number_literal(value)?;
            let guard = string_literal(NUMBER_GUARD);
            Ok(if negated {
                format!("({name}!~{guard} or {name} != {literal})")
            } else {
                format!("({name}=~{guard} and {name} == {literal})")
            })
        }
        Op::In(values) if matches!(kind(leaf), FieldKind::Parsed(_, ValueType::Int | ValueType::Float)) => {
            let items = values
                .iter()
                .map(|value| Ok(Expr::Leaf(Leaf { field: leaf.field.clone(), op: Op::Eq(value.clone()), case_insensitive: false })))
                .collect::<Result<Vec<_>, TranslationError>>()?;
            let expr = if negated {
                Expr::And(items.into_iter().map(Expr::negate).collect())
            } else {
                Expr::Or(items)
            };
            label_expression(&expr, &mut false, false)
        }
        Op::Eq(FilterValue::Bool(flag)) => Ok(format!("{name}{}\"{flag}\"", if negated { "!=" } else { "=" })),
        _ => string_condition(leaf, negated),
    }
}

fn write_metric(metric: &MetricQuery) -> Result<String, TranslationError> {
    let value = metric.function.value_field();
    if let Some(field) = value {
        match FieldKind::parse(&field.field_type) {
            Some(FieldKind::Parsed(_, ValueType::Int | ValueType::Float) | FieldKind::Label) => {}
            _ => return Err(TranslationError("Loki computes values only from numeric fields")),
        }
    }
    for field in &metric.group_by {
        if !matches!(FieldKind::parse(&field.field_type), Some(FieldKind::Label | FieldKind::Parsed(..))) || !valid_label_name(&field.name) {
            return Err(TranslationError("Loki groups only by labels and parsed fields"));
        }
    }
    let mut extra = metric.group_by.iter().collect::<Vec<_>>();
    extra.extend(value);
    let pipeline = Pipeline::build(&metric.filter, &extra)?;
    let range = format_duration(metric.range_ms);
    let by = if metric.group_by.is_empty() {
        String::new()
    } else {
        format!(" by ({})", metric.group_by.iter().map(|field| field.name.as_str()).collect::<Vec<_>>().join(", "))
    };
    let plain = pipeline.render(true, None, false);
    Ok(match &metric.function {
        RangeFunction::Count => format!("sum{by} (count_over_time({plain} [{range}]))"),
        RangeFunction::Rate => format!("sum{by} (rate({plain} [{range}]))"),
        function => {
            let field = value.expect("value functions have a field");
            let unwrapped = pipeline.render(true, Some(field), true);
            let (outer, inner) = match function {
                RangeFunction::Sum(_) => ("sum", "sum_over_time"),
                RangeFunction::Min(_) => ("min", "min_over_time"),
                RangeFunction::Max(_) => ("max", "max_over_time"),
                RangeFunction::Avg(_) => {
                    let guarded = pipeline.render(true, Some(field), false);
                    return Ok(format!(
                        "sum{by} (sum_over_time({unwrapped} [{range}])) / sum{by} (count_over_time({guarded} [{range}]))"
                    ));
                }
                _ => unreachable!(),
            };
            format!("{outer}{by} ({inner}({unwrapped} [{range}]))")
        }
    })
}

// ---------------------------------------------------------------------------------------------
// Reading

struct ReadPipeline {
    filter: Expr,
    unwrap: Option<String>,
}

struct Reader<'a> {
    input: &'a str,
    position: usize,
}

impl<'a> Reader<'a> {
    fn new(input: &'a str) -> Self {
        Self { input, position: 0 }
    }

    fn rest(&self) -> &'a str {
        &self.input[self.position..]
    }

    fn skip_space(&mut self) {
        let trimmed = self.rest().trim_start();
        self.position = self.input.len() - trimmed.len();
    }

    fn eat(&mut self, text: &str) -> bool {
        self.skip_space();
        if self.rest().starts_with(text) {
            self.position += text.len();
            true
        } else {
            false
        }
    }

    fn word(&mut self) -> Option<&'a str> {
        self.skip_space();
        let rest = self.rest();
        let end = rest
            .find(|character: char| !(character.is_ascii_alphanumeric() || character == '_'))
            .unwrap_or(rest.len());
        if end == 0 {
            return None;
        }
        self.position += end;
        Some(&rest[..end])
    }

    fn string(&mut self) -> Result<String, TranslationError> {
        self.skip_space();
        let (value, rest) = read_string_literal(self.rest()).ok_or(TranslationError("expected a quoted string"))?;
        self.position = self.input.len() - rest.len();
        Ok(value)
    }

    /// An unquoted number, duration, or size literal.
    fn literal(&mut self) -> Result<String, TranslationError> {
        self.skip_space();
        let rest = self.rest();
        let end = rest
            .find(|character: char| !(character.is_ascii_alphanumeric() || matches!(character, '.' | '-' | '+' | 'µ')))
            .unwrap_or(rest.len());
        if end == 0 {
            return Err(UNSUPPORTED);
        }
        self.position += end;
        Ok(rest[..end].to_owned())
    }

    fn pipeline(&mut self) -> Result<ReadPipeline, TranslationError> {
        if !self.eat("{") {
            return Err(TranslationError("a LogQL query starts with a {stream selector}"));
        }
        let mut items = Vec::new();
        let mut first = true;
        loop {
            if self.eat("}") {
                break;
            }
            if !first && !self.eat(",") {
                return Err(UNSUPPORTED);
            }
            first = false;
            let name = self.word().ok_or(UNSUPPORTED)?.to_owned();
            let operator = self.operator(&["=~", "!=", "!~", "="])?;
            let value = self.string()?;
            // The connector's match-all selector for an empty `{}`.
            if name == "service_name" && operator == "=~" && value == ".+" {
                continue;
            }
            items.push(string_read(&name, operator, &value)?);
        }
        let mut unwrap = None;
        loop {
            self.skip_space();
            if self.rest().is_empty() || self.rest().starts_with('[') || self.rest().starts_with(')') {
                break;
            }
            if unwrap.is_some() {
                return Err(TranslationError("unwrap must be the last stage"));
            }
            if let Some(operator) = ["|=", "!=", "|~", "!~"].iter().find(|operator| self.rest().starts_with(**operator)) {
                self.position += operator.len();
                let mut alternatives = vec![self.line_read(operator)?];
                // Loki 3: `|= "a" or "b"`.
                while self.peek_keyword("or") {
                    self.word();
                    alternatives.push(self.line_read(operator)?);
                }
                items.push(if alternatives.len() == 1 {
                    alternatives.pop().unwrap()
                } else if operator.starts_with('|') {
                    Expr::Or(alternatives)
                } else {
                    Expr::And(alternatives)
                });
                continue;
            }
            if !self.eat("|") {
                return Err(UNSUPPORTED);
            }
            self.skip_space();
            let checkpoint = self.position;
            match self.word() {
                Some("json" | "logfmt") if self.stage_ended() => continue,
                Some("label_format") => {
                    let expected = format!("{LINE_LABEL}=\"{{{{__line__}}}}\"");
                    if self.eat(&expected) {
                        continue;
                    }
                    return Err(TranslationError("label_format cannot be translated"));
                }
                Some("drop") if self.eat(LINE_LABEL) => continue,
                Some("unwrap") => {
                    let field = self.word().ok_or(UNSUPPORTED)?.to_owned();
                    if self.rest().trim_start().starts_with('(') {
                        return Err(TranslationError("unwrap conversion functions cannot be translated"));
                    }
                    unwrap = Some(field);
                    continue;
                }
                Some(stage @ ("json" | "logfmt" | "regexp" | "pattern" | "unpack" | "line_format" | "drop" | "keep" | "decolorize" | "distinct")) => {
                    let _ = stage;
                    return Err(TranslationError("parser and formatting stages other than | json and | logfmt cannot be translated"));
                }
                _ => {
                    self.position = checkpoint;
                    items.push(self.label_or(0)?);
                }
            }
        }
        // Guards pair with their comparison inside each stage, so collapse before flattening.
        let filter = Expr::And(items.into_iter().map(collapse_guards).collect()).simplify().merge_equalities();
        Ok(ReadPipeline { filter, unwrap })
    }

    fn stage_ended(&mut self) -> bool {
        let rest = self.rest().trim_start();
        rest.is_empty() || rest.starts_with('|') || rest.starts_with('[') || rest.starts_with(')')
    }

    fn peek_keyword(&mut self, keyword: &str) -> bool {
        self.skip_space();
        let rest = self.rest();
        rest.starts_with(keyword)
            && rest[keyword.len()..].starts_with(|character: char| character.is_whitespace() || character == '"' || character == '(')
    }

    fn operator(&mut self, operators: &[&'static str]) -> Result<&'static str, TranslationError> {
        self.skip_space();
        let found = operators.iter().find(|operator| self.rest().starts_with(**operator)).ok_or(UNSUPPORTED)?;
        self.position += found.len();
        Ok(found)
    }

    fn line_read(&mut self, operator: &str) -> Result<Expr, TranslationError> {
        let value = self.string()?;
        let leaf = if operator.ends_with('=') {
            Leaf::new("_msg", Op::Contains(value))
        } else {
            let (op, insensitive) = classify_search_pattern(&value)?;
            match op {
                Op::Words(words) => Leaf::text(words),
                op => Leaf { field: Some(Field::named("_msg")), op, case_insensitive: insensitive },
            }
        };
        Ok(if operator.starts_with('!') { Expr::Leaf(leaf).negate() } else { Expr::Leaf(leaf) })
    }

    fn label_or(&mut self, depth: usize) -> Result<Expr, TranslationError> {
        if depth > 32 {
            return Err(TranslationError("the query nests too deeply"));
        }
        let mut items = vec![self.label_and(depth)?];
        while self.peek_keyword("or") {
            self.word();
            items.push(self.label_and(depth)?);
        }
        Ok(if items.len() == 1 { items.pop().unwrap() } else { Expr::Or(items) })
    }

    fn label_and(&mut self, depth: usize) -> Result<Expr, TranslationError> {
        let mut items = vec![self.label_unary(depth)?];
        loop {
            if self.peek_keyword("and") {
                self.word();
            } else if self.eat(",") {
            } else {
                break;
            }
            items.push(self.label_unary(depth)?);
        }
        Ok(if items.len() == 1 { items.pop().unwrap() } else { Expr::And(items) })
    }

    fn label_unary(&mut self, depth: usize) -> Result<Expr, TranslationError> {
        if self.eat("(") {
            let inner = self.label_or(depth + 1)?;
            if !self.eat(")") {
                return Err(UNSUPPORTED);
            }
            return Ok(inner);
        }
        let name = self.word().ok_or(UNSUPPORTED)?.to_owned();
        if name == "not" {
            return Err(TranslationError("LogQL has no not keyword; use != or !~"));
        }
        let operator = self.operator(&["==", "!=", ">=", "<=", "=~", "!~", "=", ">", "<"])?;
        self.skip_space();
        let quoted = self.rest().starts_with('"') || self.rest().starts_with('`');
        if quoted {
            if !matches!(operator, "=" | "!=" | "=~" | "!~") {
                return Err(TranslationError("compare numbers, durations, and sizes without quotes"));
            }
            let value = self.string()?;
            if name == "__error__" && operator == "=" && value.is_empty() {
                return Ok(Expr::all());
            }
            return string_read(&name, operator, &value);
        }
        let literal = self.literal()?;
        let value = if is_number(&literal) { literal_value(&literal) } else { FilterValue::String(literal) };
        let leaf = |op| Expr::Leaf(Leaf::new(name.clone(), op));
        Ok(match operator {
            "==" | "=" => leaf(Op::Eq(value)),
            "!=" => leaf(Op::Eq(value)).negate(),
            other => leaf(Op::Compare(Cmp::from_symbol(other).ok_or(UNSUPPORTED)?, value)),
        })
    }

    fn metric(&mut self) -> Result<MetricQuery, TranslationError> {
        let left = self.aggregation()?;
        if !self.eat("/") {
            return left.into_metric(None);
        }
        let right = self.aggregation()?;
        left.into_metric(Some(right))
    }

    /// `agg [by (labels)] (fn(logq [range]))`, with `by` before or after the parentheses.
    fn aggregation(&mut self) -> Result<Aggregation, TranslationError> {
        let outer = self.word().ok_or(UNSUPPORTED)?.to_owned();
        if !matches!(outer.as_str(), "sum" | "min" | "max") {
            return Err(TranslationError("only sum, min, and max aggregations can be translated"));
        }
        let mut group = self.group()?;
        if !self.eat("(") {
            return Err(UNSUPPORTED);
        }
        let function = self.word().ok_or(UNSUPPORTED)?.to_owned();
        if !self.eat("(") {
            return Err(UNSUPPORTED);
        }
        let pipeline = self.pipeline()?;
        if !self.eat("[") {
            return Err(UNSUPPORTED);
        }
        let end = self.rest().find(']').ok_or(UNSUPPORTED)?;
        let range = parse_duration(self.rest()[..end].trim()).ok_or(TranslationError("invalid range duration"))?;
        self.position += end + 1;
        if !self.eat(")") || !self.eat(")") {
            return Err(UNSUPPORTED);
        }
        if group.is_none() {
            group = self.group()?;
        }
        Ok(Aggregation { outer, function, pipeline, range, group: group.unwrap_or_default() })
    }

    fn group(&mut self) -> Result<Option<Vec<String>>, TranslationError> {
        if !self.peek_keyword("by") {
            if self.peek_keyword("without") {
                return Err(TranslationError("without (...) cannot be translated; use by (...)"));
            }
            return Ok(None);
        }
        self.word();
        if !self.eat("(") {
            return Err(UNSUPPORTED);
        }
        let mut labels = Vec::new();
        if !self.eat(")") {
            loop {
                labels.push(self.word().ok_or(UNSUPPORTED)?.to_owned());
                if self.eat(")") {
                    break;
                }
                if !self.eat(",") {
                    return Err(UNSUPPORTED);
                }
            }
        }
        Ok(Some(labels))
    }
}

struct Aggregation {
    outer: String,
    function: String,
    pipeline: ReadPipeline,
    range: i64,
    group: Vec<String>,
}

impl Aggregation {
    /// The filter without the value field's number guard, which the value function implies.
    fn filter(&self, value: Option<&str>) -> Expr {
        let Some(value) = value else {
            return self.pipeline.filter.clone();
        };
        let items = self
            .pipeline
            .filter
            .clone()
            .conjuncts()
            .into_iter()
            .filter(|item| !is_guard(item, value))
            .collect();
        Expr::And(items).simplify()
    }

    fn into_metric(self, divisor: Option<Aggregation>) -> Result<MetricQuery, TranslationError> {
        let group_by = self.group.iter().map(Field::named).collect::<Vec<_>>();
        let value = self.pipeline.unwrap.clone();
        let function = match (self.outer.as_str(), self.function.as_str(), &value, &divisor) {
            ("sum", "count_over_time", None, None) => RangeFunction::Count,
            ("sum", "rate", None, None) => RangeFunction::Rate,
            ("sum", "sum_over_time", Some(field), None) => RangeFunction::Sum(Field::named(field)),
            ("min", "min_over_time", Some(field), None) => RangeFunction::Min(Field::named(field)),
            ("max", "max_over_time", Some(field), None) => RangeFunction::Max(Field::named(field)),
            ("sum", "sum_over_time", Some(field), Some(count)) => {
                let same = count.outer == "sum"
                    && count.function == "count_over_time"
                    && count.pipeline.unwrap.is_none()
                    && count.range == self.range
                    && count.group == self.group
                    && count.filter(Some(field)) == self.filter(Some(field));
                if !same {
                    return Err(TranslationError("only sum_over_time / count_over_time of the same records is an exact average"));
                }
                RangeFunction::Avg(Field::named(field))
            }
            (_, "avg_over_time" | "quantile_over_time" | "stddev_over_time" | "stdvar_over_time" | "bytes_over_time" | "bytes_rate" | "absent_over_time" | "first_over_time" | "last_over_time", _, _) => {
                return Err(TranslationError("this range function has no equivalent in the other languages"))
            }
            _ => return Err(TranslationError("the aggregation does not match its range function (use sum for counts, rates, and sums; min and max for min_over_time and max_over_time)")),
        };
        let filter = self.filter(value.as_deref());
        Ok(MetricQuery { function, range_ms: self.range, group_by, filter })
    }
}

fn is_guard(expr: &Expr, field: &str) -> bool {
    matches!(expr, Expr::Leaf(Leaf { field: Some(found), op: Op::Regex(regex), .. })
        if found.name == field && Some(regex) == PortableRegex::from_re2(NUMBER_GUARD).ok().as_ref())
}

/// A string matcher or label filter: `=`, `!=`, `=~`, `!~`. An empty value matches streams or
/// lines without the label, so `l=""` is "missing" and `l!=""` is "exists".
fn string_read(name: &str, operator: &str, value: &str) -> Result<Expr, TranslationError> {
    let (field, line) = if name == LINE_LABEL { ("_msg", true) } else { (name, false) };
    Ok(match (operator, value.is_empty()) {
        ("=", true) => Expr::Leaf(Leaf::new(field, Op::Exists)).negate(),
        ("!=", true) => Expr::Leaf(Leaf::new(field, Op::Exists)),
        ("=", false) => Expr::Leaf(Leaf::new(field, Op::Eq(FilterValue::String(value.to_owned())))),
        ("!=", false) => Expr::Leaf(Leaf::new(field, Op::Eq(FilterValue::String(value.to_owned())))).negate(),
        (pattern_operator, _) => {
            let (op, insensitive) = classify_value_pattern(value)?;
            let leaf = match op {
                Op::Words(words) if line => Leaf::text(words),
                op => Leaf { field: Some(Field::named(field)), op, case_insensitive: insensitive },
            };
            if pattern_operator == "!~" { Expr::Leaf(leaf).negate() } else { Expr::Leaf(leaf) }
        }
    })
}

/// Turns the writer's guarded comparisons back into plain ones:
/// `(f=~NUM and f op v)` → `f op v`; `(f!~NUM or f op v)` → `not (f complement(op) v)`.
fn collapse_guards(expr: Expr) -> Expr {
    let guards = [NUMBER_GUARD, DURATION_GUARD, BYTES_GUARD]
        .iter()
        .filter_map(|guard| PortableRegex::from_re2(guard).ok())
        .collect::<Vec<_>>();
    let guard_of = |expr: &Expr| -> Option<String> {
        match expr {
            Expr::Leaf(Leaf { field: Some(field), op: Op::Regex(regex), .. }) if guards.contains(regex) => Some(field.name.clone()),
            _ => None,
        }
    };
    fn compared(expr: &Expr) -> Option<&str> {
        match expr {
            Expr::Leaf(Leaf { field: Some(field), op: Op::Compare(..) | Op::Eq(_), .. }) => Some(&field.name),
            Expr::Not(inner) => compared(inner),
            _ => None,
        }
    }
    match expr {
        Expr::And(items) => {
            if let [first, second] = items.as_slice() {
                if guard_of(first).is_some() && guard_of(first).as_deref() == compared(second) && !matches!(second, Expr::Not(_)) {
                    return second.clone();
                }
            }
            Expr::And(items.into_iter().map(collapse_guards).collect())
        }
        Expr::Or(items) => {
            if let [Expr::Not(guard), second] = items.as_slice() {
                if guard_of(guard).is_some() && guard_of(guard).as_deref() == compared(second) {
                    return match second {
                        Expr::Leaf(Leaf { field, op: Op::Compare(cmp, value), case_insensitive }) => Expr::Leaf(Leaf {
                            field: field.clone(),
                            op: Op::Compare(cmp.complement(), value.clone()),
                            case_insensitive: *case_insensitive,
                        })
                        .negate(),
                        // `f != v` (a negated equality) keeps its negation.
                        other => other.clone(),
                    };
                }
            }
            Expr::Or(items.into_iter().map(collapse_guards).collect())
        }
        Expr::Not(inner) => Expr::Not(Box::new(collapse_guards(*inner))),
        leaf => leaf,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::investigation_dialect::{build_query, FilterPredicate};

    fn text(value: &str) -> FilterValue {
        FilterValue::String(value.to_owned())
    }

    fn leaf(field: &str, field_type: &str, op: Op) -> Leaf {
        Leaf { field: Some(Field::typed(field, field_type)), op, case_insensitive: false }
    }

    fn write(expr: Expr) -> String {
        dialect()
            .write(&Query::Filter(expr), &WriteContext { index: "logs", time_field: None, time_range: None, schema: &Schema::new() })
            .unwrap()
    }

    fn read(query: &str) -> Expr {
        match dialect().read(query).unwrap() {
            Query::Filter(expr) => expr,
            other => panic!("{other:?}"),
        }
    }

    /// Drops field types so written and read trees compare.
    fn untyped(expr: Expr) -> Expr {
        let mut expr = expr;
        for leaf in expr.leaves_mut() {
            if let Some(field) = leaf.field.as_mut() {
                field.field_type.clear();
            }
        }
        expr
    }

    #[test]
    fn ai_filters_keep_their_queries() {
        let filters = [
            FilterPredicate { field: "app".to_owned(), field_type: "label".to_owned(), operator: FilterOperator::Eq, value: text("api") },
            FilterPredicate { field: "status".to_owned(), field_type: "json int".to_owned(), operator: FilterOperator::Gte, value: FilterValue::Number(500.into()) },
            FilterPredicate { field: "_msg".to_owned(), field_type: "text".to_owned(), operator: FilterOperator::Contains, value: text("say \"hi\"") },
        ];
        assert_eq!(
            build_query(dialect(), "logs", &filters).unwrap(),
            format!(r#"{{app="api"}} |= "say \"hi\"" | json | (status=~{} and status >= 500)"#, string_literal(NUMBER_GUARD))
        );
    }

    #[test]
    fn boolean_logic_round_trips_through_selector_line_and_label_filters() {
        let expr = Expr::And(vec![
            Expr::Leaf(leaf("app", "label", Op::Eq(text("api")))),
            Expr::Leaf(leaf("env", "label", Op::Eq(text("dev")))).negate(),
            Expr::Leaf(leaf("_msg", "text", Op::Contains("timeout".to_owned()))),
            Expr::Or(vec![
                Expr::Leaf(leaf("status", "json int", Op::Compare(Cmp::Gte, FilterValue::Number(500.into())))),
                Expr::Leaf(leaf("level", "json string", Op::In(vec![text("warn"), text("error")]))),
                Expr::Leaf(Leaf::text("gateway down")),
            ]),
            Expr::Leaf(leaf("latency", "json float", Op::Compare(Cmp::Gt, FilterValue::Number(1.into())))).negate(),
        ]);
        let written = write(expr.clone());
        assert_eq!(
            written,
            format!(
                r#"{{app="api", env!="dev"}} |= "timeout" | json | label_format ig_line="{{{{__line__}}}}" | ((status=~{num} and status >= 500) or level=~"warn|error" or ig_line=~"(?is).*\\bgateway\\W+down\\b.*") and (latency!~{num} or latency <= 1) | drop ig_line"#,
                num = string_literal(NUMBER_GUARD)
            )
        );
        assert_eq!(read(&written), untyped(expr));
    }

    #[test]
    fn unanchored_selectors_move_to_label_filters() {
        let expr = Expr::And(vec![
            Expr::Leaf(leaf("env", "label", Op::Eq(text("dev")))).negate(),
            Expr::Leaf(leaf("_msg", "text", Op::Words("disk full".to_owned()))),
        ]);
        let written = write(expr.clone());
        assert_eq!(written, r#"{} |~ "(?i)\\bdisk\\W+full\\b" | env!="dev""#);
        assert_eq!(read(&written), untyped(Expr::And(vec![Expr::Leaf(Leaf::text("disk full")), Expr::Leaf(leaf("env", "", Op::Eq(text("dev")))).negate()])));
    }

    #[test]
    fn computations_write_exact_aggregates_and_read_back() {
        let metric = MetricQuery {
            function: RangeFunction::Avg(Field::typed("latency", "json float")),
            range_ms: 60_000,
            group_by: vec![Field::typed("service", "label")],
            filter: Expr::Leaf(leaf("level", "json string", Op::Eq(text("error")))),
        };
        let written = dialect().write(&Query::Metric(metric.clone()), &WriteContext { index: "logs", time_field: None, time_range: None, schema: &Schema::new() }).unwrap();
        let num = string_literal(NUMBER_GUARD);
        assert_eq!(
            written,
            format!(r#"sum by (service) (sum_over_time({{}} | json | __error__="" | level="error" | latency=~{num} | unwrap latency [1m])) / sum by (service) (count_over_time({{}} | json | __error__="" | level="error" | latency=~{num} [1m]))"#)
        );
        let Query::Metric(read) = dialect().read(&written).unwrap() else { panic!() };
        assert_eq!(read.function, RangeFunction::Avg(Field::named("latency")));
        assert_eq!(read.group_by, vec![Field::named("service")]);
        assert_eq!(read.filter, untyped(metric.filter.clone()));
        let count = MetricQuery { function: RangeFunction::Rate, range_ms: 300_000, group_by: vec![], filter: Expr::Leaf(leaf("app", "label", Op::Eq(text("api")))) };
        let written = dialect().write(&Query::Metric(count), &WriteContext { index: "logs", time_field: None, time_range: None, schema: &Schema::new() }).unwrap();
        assert_eq!(written, r#"sum (rate({app="api"} [5m]))"#);
        for rejected in [
            r#"avg by (service) (avg_over_time({app="x"} | unwrap latency [1m]))"#,
            r#"count_over_time({app="x"} [1m])"#,
            r#"sum by (service) (quantile_over_time(0.9, {app="x"} | unwrap latency [1m]))"#,
            r#"max by (service) (sum_over_time({app="x"} | unwrap latency [1m]))"#,
        ] {
            assert!(dialect().read(rejected).is_err(), "{rejected}");
        }
    }

    #[test]
    fn values_that_do_not_fit_the_field_type_are_rejected() {
        for rejected in [
            leaf("_time", "timestamp", Op::Eq(text("x"))),
            leaf("app", "label", Op::Compare(Cmp::Gt, text("abc"))),
            leaf("status", "json int", Op::Compare(Cmp::Gt, text("500 or 1"))),
            leaf("status", "json int", Op::Contains("5".to_owned())),
            leaf("duration", "logfmt duration", Op::Compare(Cmp::Gt, text("1.5"))),
            leaf("size", "json bytes", Op::Compare(Cmp::Gt, text("1MB2KB"))),
            leaf("cached", "json boolean", Op::Eq(text("yes"))),
            leaf("service.name", "label", Op::Eq(text("x"))),
        ] {
            assert!(dialect().check_leaf(&rejected).is_err(), "{rejected:?}");
        }
        assert!(dialect().check_leaf(&leaf("duration", "json duration", Op::Compare(Cmp::Lte, text("2m30s")))).is_ok());
        let schema = Schema::from([("service_name".to_owned(), "label".to_owned()), ("_msg".to_owned(), "text".to_owned())]);
        assert_eq!(dialect().resolve_field("service.name", &schema).unwrap().name, "service_name");
        assert_eq!(dialect().resolve_field("message", &schema).unwrap().name, "_msg");
    }

    #[test]
    fn reading_rejects_what_has_no_language_neutral_meaning() {
        for query in [
            r#"app="api""#,
            r#"{app="api"} | json status="code""#,
            r#"{app="api"} | line_format "{{.msg}}""#,
            r#"{app="api"} | pattern "<ip> <_>""#,
            r#"{app="api"} | not status > 5"#,
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
