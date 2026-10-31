//! PromQL for Prometheus and Mimir: series filters and computations over raw samples.
//!
//! A filter's records are the samples of matching series. Labels select series; `_value`
//! comparisons filter samples. PromQL has no `not` and a selector is one AND of matchers, so a
//! filter is written in disjunctive normal form, one `{selector} > n` per alternative, joined with
//! the `or` set operator. A missing label reads as the empty string in PromQL, so conditions are
//! written to keep "a missing field never matches" exact: a positive pattern that matches the empty
//! string also gets `l!=""`, and a negated one becomes `l="" or l!~"p"`. Selectors whose matchers
//! all accept the empty string get the match-all `__name__!=""` anchor.
//!
//! Computations reduce raw samples per bucket with the `*_over_time` functions; the filter must be
//! a single selector, since range functions take one.

use std::collections::HashSet;

use crate::investigation_dialect::{
    attribute_key, format_duration, is_number, label_name, literal_value,
    matching::{classify_value_pattern_and_empty, value_pattern},
    parse_duration, read_string_literal, string_literal, valid_field_name, word_tokens, Cmp, Expr,
    Field, FilterOperator, FilterValue, InvestigationDialect, Leaf, MetricQuery, Op, PortableRegex,
    Query, RangeFunction, Schema, TranslationError, WriteContext,
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

const VALUE_FIELD: &str = "_value";
const ANCHOR: &str = "__name__!=\"\"";
const UNSUPPORTED: TranslationError = TranslationError(
    "only selectors, _value comparisons such as > 5, or, and sum/min/max of count_over_time, sum_over_time, min_over_time, or max_over_time can be translated",
);

/// The field types `PrometheusClient::fields` reports.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Kind {
    Label,
    Value,
    Time,
}

fn kind(field_type: &str) -> Option<Kind> {
    match field_type {
        "label" => Some(Kind::Label),
        "sample value" => Some(Kind::Value),
        "timestamp" => Some(Kind::Time),
        _ => None,
    }
}

impl InvestigationDialect for PromQlDialect {
    fn language(&self) -> &'static str {
        "PromQL"
    }

    fn example(&self) -> &'static str {
        "http_requests_total{service=\"checkout\", code=~\"5..\"} > 0"
    }

    fn operators(&self) -> &'static [FilterOperator] {
        &FilterOperator::ALL
    }

    fn field_operators(&self, field_type: &str) -> &'static [FilterOperator] {
        match kind(field_type) {
            Some(Kind::Label) => LABEL_OPERATORS,
            Some(Kind::Value) => VALUE_OPERATORS,
            Some(Kind::Time) | None => &[],
        }
    }

    fn aggregatable(&self, field_type: &str) -> bool {
        kind(field_type) == Some(Kind::Label)
    }

    /// Exact names, then label spellings of attributes (`service.name` → `service_name`).
    fn resolve_field(&self, name: &str, schema: &Schema) -> Option<Field> {
        [name.to_owned(), label_name(attribute_key(name)), label_name(name)]
            .into_iter()
            .find_map(|candidate| schema.get(&candidate).map(|field_type| Field::typed(candidate, field_type.clone())))
    }

    fn check_leaf(&self, leaf: &Leaf) -> Result<(), TranslationError> {
        let field = leaf
            .field
            .as_ref()
            .ok_or(TranslationError("PromQL has no free-text search"))?;
        match kind(&field.field_type) {
            Some(Kind::Time) => Err(TranslationError("Prometheus time is filtered by the time range")),
            Some(Kind::Value) if field.name != VALUE_FIELD => Err(TranslationError("Unsupported Prometheus field")),
            Some(Kind::Value) => match &leaf.op {
                Op::Eq(value) | Op::Compare(_, value) => number_literal(value).map(|_| ()),
                Op::In(values) if (1..=256).contains(&values.len()) => {
                    values.iter().try_for_each(|value| number_literal(value).map(|_| ()))
                }
                _ => Err(TranslationError("sample values support numeric comparisons only")),
            },
            Some(Kind::Label) => {
                if !valid_field_name(&field.name) || field.name.contains('[') || field.name == VALUE_FIELD {
                    return Err(TranslationError("Unsupported Prometheus label name"));
                }
                match &leaf.op {
                    Op::Compare(..) => Err(TranslationError("PromQL cannot compare labels as numbers")),
                    Op::Eq(value) if value.text().is_empty() => Err(EMPTY_LABEL),
                    Op::In(values) if values.is_empty() || values.len() > 256 => {
                        Err(TranslationError("a value list needs between 1 and 256 values"))
                    }
                    Op::In(values) if values.iter().any(|value| value.text().is_empty()) => Err(EMPTY_LABEL),
                    Op::Words(text) => word_tokens(text).map(|_| ()),
                    _ => Ok(()),
                }
            }
            None => Err(TranslationError("Unsupported Prometheus field type")),
        }
    }

    fn read(&self, query: &str) -> Result<Query, TranslationError> {
        let query = query.trim();
        if query.is_empty() {
            return Ok(Query::Filter(Expr::all()));
        }
        let mut reader = Reader::new(query);
        let read = if reader.at_aggregation() {
            Query::Metric(reader.metric()?)
        } else {
            Query::Filter(reader.or_expression(0)?.simplify().merge_equalities())
        };
        if !reader.rest().trim().is_empty() {
            return Err(UNSUPPORTED);
        }
        Ok(read)
    }

    fn write(&self, query: &Query, _: &WriteContext) -> Result<String, TranslationError> {
        match query {
            Query::Filter(expr) => write_filter(expr),
            Query::Metric(metric) => write_metric(metric),
        }
    }
}

const EMPTY_LABEL: TranslationError =
    TranslationError("Prometheus labels are never empty; filter on exists or missing instead");

fn number_literal(value: &FilterValue) -> Result<String, TranslationError> {
    let text = value.text();
    let text = text.trim();
    if is_number(text) {
        Ok(text.to_owned())
    } else {
        Err(TranslationError("Sample value filters need a number"))
    }
}

fn is_value(leaf: &Leaf) -> bool {
    leaf.field.as_ref().is_some_and(|field| field.name == VALUE_FIELD)
}

/// A label written unquoted when it is a classic name, quoted (Prometheus 3) otherwise.
fn label(name: &str) -> String {
    if legacy_name(name, false) {
        name.to_owned()
    } else {
        string_literal(name)
    }
}

fn legacy_name(name: &str, metric: bool) -> bool {
    let mut bytes = name.bytes();
    let extra = |byte: u8| byte == b'_' || (metric && byte == b':');
    bytes
        .next()
        .is_some_and(|first| first.is_ascii_alphabetic() || extra(first))
        && bytes.all(|byte| byte.is_ascii_alphanumeric() || extra(byte))
}

/// A written matcher, and whether it accepts the empty value (a missing label).
struct Matcher {
    text: String,
    matches_empty: bool,
}

/// One selector alternative: matchers, then sample comparisons.
#[derive(Default)]
struct Selector {
    metric: Option<String>,
    matchers: Vec<Matcher>,
    comparisons: Vec<String>,
}

impl Selector {
    fn build(term: &[(bool, Leaf)]) -> Result<Self, TranslationError> {
        let mut selector = Self::default();
        for (negated, leaf) in term {
            if is_value(leaf) {
                selector.comparisons.push(comparison(leaf, *negated)?);
                continue;
            }
            let name = &leaf.field.as_ref().ok_or(UNSUPPORTED)?.name;
            match (&leaf.op, negated) {
                (Op::Eq(value), false)
                    if name == "__name__"
                        && !leaf.case_insensitive
                        && selector.metric.is_none()
                        && legacy_name(&value.text(), true) =>
                {
                    selector.metric = Some(value.text());
                }
                _ => selector.matchers.extend(matchers(name, leaf, *negated)?),
            }
        }
        Ok(selector)
    }

    fn render(&self) -> String {
        let anchored = self.metric.is_some() || self.matchers.iter().any(|matcher| !matcher.matches_empty);
        let mut matchers = self.matchers.iter().map(|matcher| matcher.text.as_str()).collect::<Vec<_>>();
        if !anchored {
            matchers.push(ANCHOR);
        }
        let mut query = self.metric.clone().unwrap_or_default();
        if !matchers.is_empty() {
            query.push_str(&format!("{{{}}}", matchers.join(", ")));
        }
        for comparison in &self.comparisons {
            query.push(' ');
            query.push_str(comparison);
        }
        query
    }
}

/// The matchers for one label condition.
fn matchers(name: &str, leaf: &Leaf, negated: bool) -> Result<Vec<Matcher>, TranslationError> {
    let name = label(name);
    let matcher = |operator: &str, value: &str, matches_empty: bool| Matcher {
        text: format!("{name}{operator}{}", string_literal(value)),
        matches_empty,
    };
    Ok(match (&leaf.op, negated) {
        (Op::Exists, false) => vec![matcher("!=", "", false)],
        (Op::Exists, true) => vec![matcher("=", "", true)],
        (Op::Eq(value), false) if !leaf.case_insensitive => vec![matcher("=", &value.text(), false)],
        (Op::Eq(value), true) if !leaf.case_insensitive => vec![matcher("!=", &value.text(), true)],
        (op, negated) => {
            let pattern = value_pattern(op, leaf.case_insensitive)?.ok_or(UNSUPPORTED)?;
            let empty = pattern_matches_empty(op, &pattern);
            match (negated, empty) {
                // `prepare` pairs this with a missing-label alternative, so here it means "present
                // and not matching".
                (true, true) => vec![matcher("!~", &pattern, false)],
                (true, false) => vec![matcher("!~", &pattern, true)],
                (false, true) => vec![matcher("=~", &pattern, true), matcher("!=", "", false)],
                (false, false) => vec![matcher("=~", &pattern, false)],
            }
        }
    })
}

/// Whether a whole-value pattern accepts the empty string; unknown patterns are assumed to.
fn pattern_matches_empty(op: &Op, pattern: &str) -> bool {
    !matches!(op, Op::Words(_)) && PortableRegex::from_re2(pattern).map_or(true, |regex| regex.matches_empty())
}

fn negated_pattern_matches_empty(leaf: &Leaf) -> bool {
    match &leaf.op {
        Op::Exists | Op::Compare(..) => false,
        Op::Eq(_) if !leaf.case_insensitive => false,
        op => value_pattern(op, leaf.case_insensitive)
            .ok()
            .flatten()
            .is_some_and(|pattern| pattern_matches_empty(op, &pattern)),
    }
}

fn comparison(leaf: &Leaf, negated: bool) -> Result<String, TranslationError> {
    Ok(match (&leaf.op, negated) {
        (Op::Eq(value), false) => format!("== {}", number_literal(value)?),
        (Op::Eq(value), true) => format!("!= {}", number_literal(value)?),
        (Op::Compare(cmp, value), false) => format!("{} {}", cmp.symbol(), number_literal(value)?),
        (Op::Compare(cmp, value), true) => format!("{} {}", cmp.complement().symbol(), number_literal(value)?),
        _ => return Err(TranslationError("sample values support numeric comparisons only")),
    })
}

/// Rewrites the tree so every alternative fits one selector: `_value` value lists become ORs of
/// equalities, and negated patterns that match the empty string keep missing labels explicitly.
fn prepare(expr: &Expr) -> Expr {
    fn rewrite(expr: Expr) -> Expr {
        match expr {
            Expr::And(items) => Expr::And(items.into_iter().map(rewrite).collect()),
            Expr::Or(items) => Expr::Or(items.into_iter().map(rewrite).collect()),
            Expr::Leaf(leaf) if is_value(&leaf) && matches!(leaf.op, Op::In(_)) => {
                let Op::In(values) = &leaf.op else { unreachable!() };
                Expr::Or(
                    values
                        .iter()
                        .map(|value| Expr::Leaf(Leaf { op: Op::Eq(value.clone()), ..leaf.clone() }))
                        .collect(),
                )
            }
            Expr::Not(inner) => match *inner {
                Expr::Leaf(leaf) if is_value(&leaf) && matches!(leaf.op, Op::In(_)) => {
                    rewrite(Expr::Leaf(leaf)).negate().nnf()
                }
                Expr::Leaf(leaf) if !is_value(&leaf) && negated_pattern_matches_empty(&leaf) => Expr::Or(vec![
                    Expr::Leaf(Leaf { op: Op::Exists, case_insensitive: false, ..leaf.clone() }).negate(),
                    Expr::Leaf(leaf).negate(),
                ]),
                other => Expr::Not(Box::new(other)),
            },
            leaf => leaf,
        }
    }
    rewrite(expr.clone().nnf().merge_equalities()).simplify()
}

fn write_filter(expr: &Expr) -> Result<String, TranslationError> {
    let terms = prepare(expr).dnf()?;
    if terms.is_empty() {
        return Err(TranslationError("the filter matches nothing"));
    }
    let selectors = terms
        .iter()
        .map(|term| Selector::build(term).map(|selector| selector.render()))
        .collect::<Result<Vec<_>, _>>()?;
    Ok(selectors.join(" or "))
}

fn write_metric(metric: &MetricQuery) -> Result<String, TranslationError> {
    if let Some(field) = metric.function.value_field() {
        if field.name != VALUE_FIELD {
            return Err(TranslationError("Prometheus computes values only from _value"));
        }
    }
    for field in &metric.group_by {
        if kind(&field.field_type) != Some(Kind::Label) || field.name == VALUE_FIELD {
            return Err(TranslationError("Prometheus groups only by labels"));
        }
    }
    let terms = prepare(&metric.filter).dnf()?;
    let [term] = terms.as_slice() else {
        return Err(TranslationError("a PromQL computation reads one selector, so its filter cannot use or"));
    };
    if term.iter().any(|(_, leaf)| is_value(leaf)) {
        return Err(TranslationError("a PromQL computation cannot filter samples by _value"));
    }
    let selector = format!("{}[{}]", Selector::build(term)?.render(), format_duration(metric.range_ms));
    let by = if metric.group_by.is_empty() {
        String::new()
    } else {
        format!(" by ({})", metric.group_by.iter().map(|field| label(&field.name)).collect::<Vec<_>>().join(", "))
    };
    let count = format!("sum{by} (count_over_time({selector}))");
    Ok(match &metric.function {
        RangeFunction::Count => count,
        RangeFunction::Rate => format!("{count} / {}", metric.range_ms / 1_000),
        RangeFunction::Sum(_) => format!("sum{by} (sum_over_time({selector}))"),
        RangeFunction::Avg(_) => format!("sum{by} (sum_over_time({selector})) / {count}"),
        RangeFunction::Min(_) => format!("min{by} (min_over_time({selector}))"),
        RangeFunction::Max(_) => format!("max{by} (max_over_time({selector}))"),
    })
}

// ---------------------------------------------------------------------------------------------
// Reading

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

    /// A metric or label name; metric names may contain colons.
    fn name(&mut self, metric: bool) -> Option<&'a str> {
        self.skip_space();
        let rest = self.rest();
        let end = rest
            .find(|character: char| !(character.is_ascii_alphanumeric() || character == '_' || (metric && character == ':')))
            .unwrap_or(rest.len());
        if end == 0 {
            return None;
        }
        self.position += end;
        Some(&rest[..end])
    }

    fn peek_keyword(&mut self, keyword: &str) -> bool {
        self.skip_space();
        let rest = self.rest();
        rest.starts_with(keyword)
            && !rest[keyword.len()..].starts_with(|character: char| character.is_ascii_alphanumeric() || character == '_' || character == ':')
    }

    /// A double-quoted, single-quoted, or backtick string.
    fn string(&mut self) -> Result<String, TranslationError> {
        self.skip_space();
        let rest = self.rest();
        let (value, after) = if rest.starts_with('\'') {
            read_single_quoted(rest)
        } else {
            read_string_literal(rest)
        }
        .ok_or(TranslationError("expected a quoted string"))?;
        self.position = self.input.len() - after.len();
        Ok(value)
    }

    fn at_aggregation(&mut self) -> bool {
        let checkpoint = self.position;
        let found = matches!(self.name(true), Some("sum" | "min" | "max" | "avg" | "count" | "stddev" | "stdvar" | "topk" | "bottomk" | "quantile" | "count_values" | "group"))
            && (self.peek_keyword("by") || self.peek_keyword("without") || self.rest().trim_start().starts_with('('));
        self.position = checkpoint;
        found
    }

    fn or_expression(&mut self, depth: usize) -> Result<Expr, TranslationError> {
        if depth > 32 {
            return Err(TranslationError("the query nests too deeply"));
        }
        let mut items = vec![self.compared(depth)?];
        loop {
            if self.peek_keyword("or") {
                self.position += 2;
                items.push(self.compared(depth)?);
            } else if self.peek_keyword("and") || self.peek_keyword("unless") {
                return Err(TranslationError("and and unless match series by labels and cannot be translated; use one selector"));
            } else {
                break;
            }
        }
        Ok(if items.len() == 1 { items.pop().unwrap() } else { Expr::Or(items) })
    }

    /// `primary { comparison number }`, comparisons filtering samples by `_value`.
    fn compared(&mut self, depth: usize) -> Result<Expr, TranslationError> {
        let mut items = vec![self.primary(depth)?];
        loop {
            self.skip_space();
            let Some(operator) = ["==", "!=", ">=", "<=", ">", "<"].into_iter().find(|operator| self.rest().starts_with(operator)) else {
                break;
            };
            self.position += operator.len();
            if self.peek_keyword("bool") {
                return Err(TranslationError("bool comparisons return 0 or 1 instead of filtering and cannot be translated"));
            }
            self.skip_space();
            let rest = self.rest();
            let end = rest
                .find(|character: char| !(character.is_ascii_alphanumeric() || matches!(character, '.' | '-' | '+')))
                .unwrap_or(rest.len());
            let literal = &rest[..end];
            if !is_number(literal) {
                return Err(TranslationError("comparisons are translated only against a number"));
            }
            self.position += end;
            let value = literal_value(literal);
            let leaf = |op| Expr::Leaf(Leaf::new(VALUE_FIELD, op));
            items.push(match operator {
                "==" => leaf(Op::Eq(value)),
                "!=" => leaf(Op::Eq(value)).negate(),
                other => leaf(Op::Compare(Cmp::from_symbol(other).ok_or(UNSUPPORTED)?, value)),
            });
        }
        Ok(if items.len() == 1 { items.pop().unwrap() } else { Expr::And(items) })
    }

    fn primary(&mut self, depth: usize) -> Result<Expr, TranslationError> {
        if self.eat("(") {
            let inner = self.or_expression(depth + 1)?;
            if !self.eat(")") {
                return Err(UNSUPPORTED);
            }
            self.check_modifiers()?;
            return Ok(inner);
        }
        self.skip_space();
        if self.rest().starts_with(|character: char| character.is_ascii_digit() || matches!(character, '.' | '-' | '+')) {
            return Err(TranslationError("scalar arithmetic cannot be translated; compare a selector with a number"));
        }
        let selector = self.selector()?;
        self.check_modifiers()?;
        Ok(selector)
    }

    /// Range selectors, offsets, and `@` change which samples a selector returns.
    fn check_modifiers(&mut self) -> Result<(), TranslationError> {
        self.skip_space();
        if self.rest().starts_with('[') || self.peek_keyword("offset") || self.rest().starts_with('@') {
            return Err(TranslationError("range selectors, offset, and @ cannot be translated"));
        }
        Ok(())
    }

    fn selector(&mut self) -> Result<Expr, TranslationError> {
        let mut raw = Vec::new();
        if let Some(metric) = self.name(true) {
            if self.rest().trim_start().starts_with('(') {
                return Err(TranslationError("functions cannot be translated"));
            }
            if matches!(metric, "or" | "and" | "unless" | "bool" | "by" | "without" | "offset") {
                return Err(UNSUPPORTED);
            }
            raw.push(("__name__".to_owned(), "=", metric.to_owned()));
        }
        if self.eat("{") {
            let mut first = true;
            loop {
                if self.eat("}") {
                    break;
                }
                if !first && !self.eat(",") {
                    return Err(UNSUPPORTED);
                }
                first = false;
                if self.eat("}") {
                    break;
                }
                self.skip_space();
                let quoted = self.rest().starts_with(['"', '\'', '`']);
                let name = if quoted { self.string()? } else { self.name(false).ok_or(UNSUPPORTED)?.to_owned() };
                self.skip_space();
                // Prometheus 3: a bare quoted string names the metric.
                if quoted && (self.rest().starts_with(',') || self.rest().starts_with('}')) {
                    raw.push(("__name__".to_owned(), "=", name));
                    continue;
                }
                let operator = ["=~", "!=", "!~", "="]
                    .into_iter()
                    .find(|operator| self.rest().starts_with(operator))
                    .ok_or(UNSUPPORTED)?;
                self.position += operator.len();
                let value = self.string()?;
                if !valid_field_name(&name) {
                    return Err(TranslationError("Unsupported Prometheus label name"));
                }
                raw.push((name, operator, value));
            }
        } else if raw.is_empty() {
            return Err(UNSUPPORTED);
        }
        read_matchers(raw)
    }

    fn metric(&mut self) -> Result<MetricQuery, TranslationError> {
        let left = self.aggregation()?;
        if !self.eat("/") {
            return left.into_metric(Divisor::None);
        }
        self.skip_space();
        if self.rest().starts_with(|character: char| character.is_ascii_digit()) {
            let rest = self.rest();
            let end = rest.find(|character: char| !(character.is_ascii_digit() || character == '.')).unwrap_or(rest.len());
            let seconds = rest[..end].parse::<f64>().map_err(|_| UNSUPPORTED)?;
            self.position += end;
            return left.into_metric(Divisor::Seconds(seconds));
        }
        let right = self.aggregation()?;
        left.into_metric(Divisor::Aggregation(right))
    }

    /// `agg [by (labels)] (fn(selector[range]))`, with `by` before or after the parentheses.
    fn aggregation(&mut self) -> Result<Aggregation, TranslationError> {
        let outer = self.name(true).ok_or(UNSUPPORTED)?.to_owned();
        if !matches!(outer.as_str(), "sum" | "min" | "max") {
            return Err(TranslationError("only sum, min, and max aggregations can be translated"));
        }
        let mut group = self.group()?;
        if !self.eat("(") {
            return Err(UNSUPPORTED);
        }
        let function = self.name(true).ok_or(UNSUPPORTED)?.to_owned();
        if !self.eat("(") {
            return Err(UNSUPPORTED);
        }
        let selector = self.selector()?;
        if !self.eat("[") {
            return Err(TranslationError("a range function needs a [range]"));
        }
        let end = self.rest().find(']').ok_or(UNSUPPORTED)?;
        let range = parse_duration(self.rest()[..end].trim()).ok_or(TranslationError("subqueries and invalid ranges cannot be translated"))?;
        self.position += end + 1;
        self.check_modifiers()?;
        if !self.eat(")") || !self.eat(")") {
            return Err(UNSUPPORTED);
        }
        if group.is_none() {
            group = self.group()?;
        }
        Ok(Aggregation { outer, function, selector, range, group: group.unwrap_or_default() })
    }

    fn group(&mut self) -> Result<Option<Vec<String>>, TranslationError> {
        if !self.peek_keyword("by") {
            if self.peek_keyword("without") {
                return Err(TranslationError("without (...) cannot be translated; use by (...)"));
            }
            return Ok(None);
        }
        self.position += 2;
        if !self.eat("(") {
            return Err(UNSUPPORTED);
        }
        let mut labels = Vec::new();
        if !self.eat(")") {
            loop {
                self.skip_space();
                let name = if self.rest().starts_with(['"', '\'', '`']) {
                    self.string()?
                } else {
                    self.name(false).ok_or(UNSUPPORTED)?.to_owned()
                };
                labels.push(name);
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

/// Reads a Go single-quoted string by re-quoting it for the double-quoted reader.
fn read_single_quoted(input: &str) -> Option<(String, &str)> {
    let mut quoted = String::from("\"");
    let mut escaped = false;
    for (index, character) in input.char_indices().skip(1) {
        match character {
            '\'' if escaped => {
                quoted.push('\'');
                escaped = false;
            }
            _ if escaped => {
                quoted.push('\\');
                quoted.push(character);
                escaped = false;
            }
            '\\' => escaped = true,
            '"' => quoted.push_str("\\\""),
            '\'' => {
                quoted.push('"');
                let (value, _) = read_string_literal(&quoted)?;
                return Some((value, &input[index + 1..]));
            }
            _ => quoted.push(character),
        }
    }
    None
}

/// One selector's matchers as a condition. A missing label reads as the empty string, so a
/// matcher that accepts the empty value also accepts series without the label.
fn read_matchers(raw: Vec<(String, &str, String)>) -> Result<Expr, TranslationError> {
    // Each pattern is parsed once, for its condition and whether it accepts the empty value.
    let patterns = raw
        .iter()
        .map(|(_, operator, value)| match *operator {
            "=" | "!=" => Ok(None),
            _ => classify_value_pattern_and_empty(value).map(Some),
        })
        .collect::<Result<Vec<_>, _>>()?;
    // Labels another matcher already requires to be present, labels with a `!=""` matcher, and
    // labels with a `=~` matcher, gathered in one pass so each matcher below is a set lookup.
    let (mut present, mut with_exists, mut with_pattern) = (HashSet::new(), HashSet::new(), HashSet::new());
    for ((name, operator, value), pattern) in raw.iter().zip(&patterns) {
        let matches_empty = match (*operator, pattern) {
            ("=", _) => value.is_empty(),
            ("!=", _) => !value.is_empty(),
            ("=~", Some((_, _, accepts))) => *accepts,
            (_, pattern) => !pattern.as_ref().is_some_and(|(_, _, accepts)| *accepts),
        };
        if *operator == "!=" && value.is_empty() {
            with_exists.insert(name.as_str());
        } else if !matches_empty {
            present.insert(name.as_str());
        }
        if *operator == "=~" {
            with_pattern.insert(name.as_str());
        }
    }
    let mut items = Vec::new();
    for ((name, operator, value), pattern) in raw.iter().zip(patterns) {
        let exists = || Expr::Leaf(Leaf::new(name.clone(), Op::Exists));
        match (*operator, value.is_empty()) {
            // A pattern matcher that accepts the empty value consumes the `!=""` beside it.
            ("!=", true) if name == "__name__" || present.contains(name.as_str()) || with_pattern.contains(name.as_str()) => {}
            ("!=", true) => items.push(exists()),
            ("=", true) => items.push(exists().negate()),
            ("=", false) => items.push(Expr::Leaf(Leaf::new(name.clone(), Op::Eq(FilterValue::String(value.clone()))))),
            ("!=", false) => items.push(Expr::Leaf(Leaf::new(name.clone(), Op::Eq(FilterValue::String(value.clone())))).negate()),
            (pattern_operator, _) => {
                let (op, insensitive, accepts_empty) = pattern.ok_or(UNSUPPORTED)?;
                let leaf = Expr::Leaf(Leaf { field: Some(Field::named(name.clone())), op, case_insensitive: insensitive });
                items.push(match (pattern_operator, accepts_empty) {
                    ("=~", true) if with_exists.contains(name.as_str()) => leaf,
                    ("=~", true) => Expr::Or(vec![exists().negate(), leaf]),
                    ("=~", false) => leaf,
                    (_, true) => Expr::And(vec![exists(), leaf.negate()]),
                    (_, false) => leaf.negate(),
                });
            }
        }
    }
    Ok(Expr::And(items).simplify())
}

enum Divisor {
    None,
    Seconds(f64),
    Aggregation(Aggregation),
}

struct Aggregation {
    outer: String,
    function: String,
    selector: Expr,
    range: i64,
    group: Vec<String>,
}

impl Aggregation {
    fn into_metric(self, divisor: Divisor) -> Result<MetricQuery, TranslationError> {
        let value = || Field::named(VALUE_FIELD);
        let function = match (self.outer.as_str(), self.function.as_str(), &divisor) {
            ("sum", "count_over_time", Divisor::None) => RangeFunction::Count,
            ("sum", "count_over_time", Divisor::Seconds(seconds)) if *seconds * 1_000.0 == self.range as f64 => RangeFunction::Rate,
            ("sum", "count_over_time", Divisor::Seconds(_)) => {
                return Err(TranslationError("a count is a rate only when divided by its range in seconds"))
            }
            ("sum", "sum_over_time", Divisor::None) => RangeFunction::Sum(value()),
            ("min", "min_over_time", Divisor::None) => RangeFunction::Min(value()),
            ("max", "max_over_time", Divisor::None) => RangeFunction::Max(value()),
            ("sum", "sum_over_time", Divisor::Aggregation(count)) => {
                let same = count.outer == "sum"
                    && count.function == "count_over_time"
                    && count.range == self.range
                    && count.group == self.group
                    && count.selector == self.selector;
                if !same {
                    return Err(TranslationError("only sum_over_time / count_over_time of the same series is an exact average"));
                }
                RangeFunction::Avg(value())
            }
            (_, "rate" | "irate" | "increase" | "delta" | "idelta" | "deriv", _) => {
                return Err(TranslationError("counter and gauge functions have no equivalent in the other languages; use count_over_time or sum_over_time"))
            }
            (_, "avg_over_time" | "quantile_over_time" | "stddev_over_time" | "stdvar_over_time" | "last_over_time" | "present_over_time" | "absent_over_time" | "changes" | "resets", _) => {
                return Err(TranslationError("this range function has no equivalent in the other languages"))
            }
            _ => return Err(TranslationError("the aggregation does not match its range function (use sum for counts and sums; min and max for min_over_time and max_over_time)")),
        };
        Ok(MetricQuery {
            function,
            range_ms: self.range,
            group_by: self.group.iter().map(Field::named).collect(),
            filter: self.selector.merge_equalities(),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::investigation_dialect::{build_query, FilterPredicate};

    fn text(value: &str) -> FilterValue {
        FilterValue::String(value.to_owned())
    }

    fn label_leaf(name: &str, op: Op) -> Leaf {
        Leaf { field: Some(Field::typed(name, "label")), op, case_insensitive: false }
    }

    fn value_leaf(op: Op) -> Leaf {
        Leaf { field: Some(Field::typed(VALUE_FIELD, "sample value")), op, case_insensitive: false }
    }

    fn context() -> WriteContext<'static> {
        static SCHEMA: std::sync::OnceLock<Schema> = std::sync::OnceLock::new();
        WriteContext { index: "metrics", time_field: None, time_range: None, schema: SCHEMA.get_or_init(Schema::new) }
    }

    fn write(expr: Expr) -> String {
        dialect().write(&Query::Filter(expr), &context()).unwrap()
    }

    fn read(query: &str) -> Expr {
        match dialect().read(query).unwrap() {
            Query::Filter(expr) => expr,
            other => panic!("{other:?}"),
        }
    }

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
    fn ai_filters_select_series_and_filter_samples() {
        let filters = [
            FilterPredicate { field: "__name__".to_owned(), field_type: "label".to_owned(), operator: FilterOperator::Eq, value: text("http_requests_total") },
            FilterPredicate { field: "handler".to_owned(), field_type: "label".to_owned(), operator: FilterOperator::Contains, value: text("api/v1 (x)") },
            FilterPredicate { field: "_value".to_owned(), field_type: "sample value".to_owned(), operator: FilterOperator::Gt, value: FilterValue::Number(1.into()) },
            FilterPredicate { field: "_value".to_owned(), field_type: "sample value".to_owned(), operator: FilterOperator::Lte, value: text("2.5e3") },
        ];
        assert_eq!(
            build_query(dialect(), "metrics", &filters).unwrap(),
            r#"http_requests_total{handler=~"(?s).*api/v1 \\(x\\).*"} > 1 <= 2.5e3"#
        );
        assert_eq!(build_query(dialect(), "metrics", &[]).unwrap(), r#"{__name__!=""}"#);
        let quoted = [FilterPredicate { field: "job".to_owned(), field_type: "label".to_owned(), operator: FilterOperator::Eq, value: text("say \"hi\" \\") }];
        assert_eq!(build_query(dialect(), "metrics", &quoted).unwrap(), r#"{job="say \"hi\" \\"}"#);
    }

    #[test]
    fn boolean_logic_expands_into_selector_alternatives() {
        let expr = Expr::And(vec![
            Expr::Leaf(label_leaf("job", Op::Eq(text("api")))),
            Expr::Or(vec![
                Expr::Leaf(label_leaf("code", Op::Prefix("5".to_owned()))),
                Expr::Leaf(value_leaf(Op::Compare(Cmp::Gt, FilterValue::Number(10.into())))),
            ]),
            Expr::Leaf(label_leaf("env", Op::Eq(text("dev")))).negate(),
        ]);
        let written = write(expr.clone());
        assert_eq!(written, r#"{job="api", code=~"(?s)5.*", env!="dev"} or {job="api", env!="dev"} > 10"#);
        let read_back = read(&written);
        assert_eq!(
            read_back,
            untyped(Expr::Or(vec![
                Expr::And(vec![
                    Expr::Leaf(label_leaf("job", Op::Eq(text("api")))),
                    Expr::Leaf(label_leaf("code", Op::Prefix("5".to_owned()))),
                    Expr::Leaf(label_leaf("env", Op::Eq(text("dev")))).negate(),
                ]),
                Expr::And(vec![
                    Expr::Leaf(label_leaf("job", Op::Eq(text("api")))),
                    Expr::Leaf(label_leaf("env", Op::Eq(text("dev")))).negate(),
                    Expr::Leaf(value_leaf(Op::Compare(Cmp::Gt, FilterValue::Number(10.into())))),
                ]),
            ]))
        );
    }

    #[test]
    fn missing_labels_never_match_and_negation_keeps_them() {
        // `.*` accepts the empty value, so the positive form also requires the label.
        let empty = || Op::Regex(PortableRegex::from_re2("a*").unwrap());
        assert_eq!(write(Expr::Leaf(label_leaf("zone", empty()))), r#"{zone=~"a*", zone!=""}"#);
        assert_eq!(read(r#"{zone=~"a*", zone!=""}"#), untyped(Expr::Leaf(label_leaf("zone", empty()))));
        assert_eq!(
            write(Expr::Leaf(label_leaf("zone", Op::Regex(PortableRegex::from_re2("a*").unwrap()))).negate()),
            r#"{zone="", __name__!=""} or {zone!~"a*"}"#
        );
        assert_eq!(write(Expr::Leaf(label_leaf("zone", Op::Exists)).negate()), r#"{zone="", __name__!=""}"#);
        assert_eq!(write(Expr::Leaf(label_leaf("zone", Op::Contains("x".to_owned()))).negate()), r#"{zone!~"(?s).*x.*", __name__!=""}"#);
        assert_eq!(read(r#"{zone=~"a*"}"#), untyped(Expr::Or(vec![
            Expr::Leaf(label_leaf("zone", Op::Exists)).negate(),
            Expr::Leaf(label_leaf("zone", Op::Regex(PortableRegex::from_re2("a*").unwrap()))),
        ])));
        assert_eq!(read(r#"{__name__!=""}"#), Expr::all());
    }

    #[test]
    fn value_lists_and_negated_comparisons_are_exact() {
        let expr = Expr::And(vec![
            Expr::Leaf(label_leaf("__name__", Op::Eq(text("up")))),
            Expr::Leaf(value_leaf(Op::In(vec![FilterValue::Number(0.into()), FilterValue::Number(1.into())]))).negate(),
            Expr::Leaf(value_leaf(Op::Compare(Cmp::Gte, FilterValue::Number(5.into())))).negate(),
        ]);
        assert_eq!(write(expr), "up != 0 != 1 < 5");
        assert_eq!(
            write(Expr::Leaf(value_leaf(Op::In(vec![FilterValue::Number(0.into()), FilterValue::Number(1.into())])))),
            r#"{__name__!=""} == 0 or {__name__!=""} == 1"#
        );
        assert_eq!(
            write(Expr::Leaf(Leaf { case_insensitive: true, ..label_leaf("job", Op::In(vec![text("API"), text("web")])) })),
            r#"{job=~"(?i)API|web"}"#
        );
        assert_eq!(write(Expr::Leaf(label_leaf("service.name", Op::Eq(text("x"))))), r#"{"service.name"="x"}"#);
    }

    #[test]
    fn computations_write_exact_aggregates_and_read_back() {
        let metric = MetricQuery {
            function: RangeFunction::Avg(Field::typed(VALUE_FIELD, "sample value")),
            range_ms: 60_000,
            group_by: vec![Field::typed("job", "label")],
            filter: Expr::Leaf(label_leaf("__name__", Op::Eq(text("latency_seconds")))),
        };
        let written = dialect().write(&Query::Metric(metric.clone()), &context()).unwrap();
        assert_eq!(written, "sum by (job) (sum_over_time(latency_seconds[1m])) / sum by (job) (count_over_time(latency_seconds[1m]))");
        let Query::Metric(read) = dialect().read(&written).unwrap() else { panic!() };
        assert_eq!(read.function, RangeFunction::Avg(Field::named(VALUE_FIELD)));
        assert_eq!(read.group_by, vec![Field::named("job")]);
        assert_eq!(read.filter, untyped(metric.filter.clone()));
        let rate = MetricQuery { function: RangeFunction::Rate, range_ms: 300_000, group_by: vec![], filter: Expr::Leaf(label_leaf("job", Op::Eq(text("api")))) };
        let written = dialect().write(&Query::Metric(rate), &context()).unwrap();
        assert_eq!(written, r#"sum (count_over_time({job="api"}[5m])) / 300"#);
        assert!(matches!(dialect().read(&written).unwrap(), Query::Metric(MetricQuery { function: RangeFunction::Rate, .. })));
        let or = MetricQuery { function: RangeFunction::Count, range_ms: 60_000, group_by: vec![], filter: Expr::Or(vec![Expr::Leaf(label_leaf("a", Op::Eq(text("1")))), Expr::Leaf(label_leaf("b", Op::Eq(text("2"))))]) };
        assert!(dialect().write(&Query::Metric(or), &context()).is_err());
        for rejected in [
            "sum(rate(http_requests_total[5m]))",
            "sum(count_over_time(up[5m])) / 60",
            "avg by (job) (avg_over_time(up[1m]))",
            "max by (job) (sum_over_time(up[1m]))",
            "sum without (job) (count_over_time(up[1m]))",
            "sum(count_over_time(up[5m:1m]))",
            "sum(count_over_time(up[5m] offset 1m))",
        ] {
            assert!(dialect().read(rejected).is_err(), "{rejected}");
        }
    }

    #[test]
    fn reading_accepts_prometheus_spellings() {
        assert_eq!(
            read(r#"(up{job='a\'b', "service.name"="x"}) >= 1 or {"up", zone!~"eu-.*"}"#),
            untyped(Expr::Or(vec![
                Expr::And(vec![
                    Expr::Leaf(label_leaf("__name__", Op::Eq(text("up")))),
                    Expr::Leaf(label_leaf("job", Op::Eq(text("a'b")))),
                    Expr::Leaf(label_leaf("service.name", Op::Eq(text("x")))),
                    Expr::Leaf(value_leaf(Op::Compare(Cmp::Gte, FilterValue::Number(1.into())))),
                ]),
                Expr::And(vec![
                    Expr::Leaf(label_leaf("__name__", Op::Eq(text("up")))),
                    Expr::Leaf(label_leaf("zone", Op::Prefix("eu-".to_owned()))).negate(),
                ]),
            ]))
        );
        assert_eq!(read("up != 1"), untyped(Expr::And(vec![
            Expr::Leaf(label_leaf("__name__", Op::Eq(text("up")))),
            Expr::Leaf(value_leaf(Op::Eq(FilterValue::Number(1.into())))).negate(),
        ])));
    }

    #[test]
    fn reading_rejects_anything_it_cannot_translate_exactly() {
        for query in [
            "rate(http_requests_total[5m])",
            "up[5m]",
            "up offset 5m",
            "up @ 1700000000",
            "up > bool 1",
            "up > 1 and down",
            "up unless down",
            r#"up{job="api""#,
            "up > NaN",
            "up > 0x1F",
            "(up",
            "1 + up",
            "1 < up",
            "up + 1",
        ] {
            assert!(dialect().read(query).is_err(), "{query}");
        }
    }

    #[test]
    fn fields_follow_the_prometheus_schema() {
        for rejected in [
            label_leaf("job", Op::Compare(Cmp::Gt, text("1"))),
            label_leaf("job", Op::Eq(text(""))),
            value_leaf(Op::Compare(Cmp::Gt, text("NaN"))),
            value_leaf(Op::Compare(Cmp::Gt, text("1 or vector(1)"))),
            value_leaf(Op::Contains("1".to_owned())),
            Leaf { field: Some(Field::typed("_time", "timestamp")), op: Op::Compare(Cmp::Gt, text("1")), case_insensitive: false },
            Leaf::text("timeout"),
        ] {
            assert!(dialect().check_leaf(&rejected).is_err(), "{rejected:?}");
        }
        let schema = Schema::from([("service_name".to_owned(), "label".to_owned())]);
        assert_eq!(dialect().resolve_field("resource.attributes.service.name", &schema).unwrap().name, "service_name");
        assert!(dialect().aggregatable("label") && !dialect().aggregatable("sample value"));
        assert_eq!(dialect().field_operators("sample value"), VALUE_OPERATORS);
    }
}
