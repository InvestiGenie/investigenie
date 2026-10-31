//! Lucene `query_string` for Elasticsearch.
//!
//! The connector sends `default_field: "*"`, `allow_leading_wildcard: false`, and the default OR
//! operator. Leading wildcards stay disabled, so `contains`, `suffix`, and regexes that start
//! with an unbounded `.*` are not offered. Lucene has no aggregations, so computations cannot
//! target Elasticsearch.

use crate::investigation_dialect::{
    attribute_key, first_present, is_message_field, literal_value, word_tokens, Cmp, Expr, Field,
    FilterOperator, FilterPredicate, FilterValue, InvestigationDialect, Leaf, Op, PortableRegex, Query, Schema,
    TranslationError, WriteContext,
};

struct LuceneDialect;

pub fn dialect() -> &'static dyn InvestigationDialect {
    &LuceneDialect
}

/// Lucene `query_string` syntax characters; a backslash makes each one literal.
const SPECIAL: &str = "\\+-=&|><!(){}[]^\"~*?:/";
const UNSUPPORTED: TranslationError = TranslationError(
    "only field:value, field:\"phrase\", prefix*, /regex/, ranges, _exists_, free text, AND/OR/NOT, and parentheses can be translated",
);

impl InvestigationDialect for LuceneDialect {
    fn language(&self) -> &'static str {
        "Lucene"
    }

    fn example(&self) -> &'static str {
        "service:\"checkout\" AND status:[500 TO *] AND NOT level:debug"
    }

    /// No `contains`: its translation `field:*text*` needs a leading wildcard, which the
    /// connector disables (`allow_leading_wildcard: false`) to keep searches cheap.
    fn operators(&self) -> &'static [FilterOperator] {
        &[
            FilterOperator::Eq,
            FilterOperator::Gt,
            FilterOperator::Gte,
            FilterOperator::Lt,
            FilterOperator::Lte,
        ]
    }

    /// Analyzed `text` fields compare per token, so they keep equality (a phrase match) alone.
    fn field_operators(&self, field_type: &str) -> &'static [FilterOperator] {
        match field_type.trim().to_ascii_lowercase().as_str() {
            "text" | "match_only_text" => &[FilterOperator::Eq],
            _ => self.operators(),
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

    /// Exact names, then OpenTelemetry spellings (`resource.attributes.*`, `attributes.*`), then
    /// common message fields for `_msg`. Analyzed text fields with a `.keyword` sub-field use it
    /// for exact conditions.
    fn resolve_field(&self, name: &str, schema: &Schema) -> Option<Field> {
        let key = attribute_key(name);
        let mut candidates = vec![
            name.to_owned(),
            key.to_owned(),
            format!("resource.attributes.{key}"),
            format!("attributes.{key}"),
        ];
        if is_message_field(name) {
            candidates.extend(["message", "msg", "log", "body", "event.original"].map(str::to_owned));
        }
        let mut field = first_present(schema, candidates)?;
        if is_text(&field.field_type) {
            let exact = format!("{}.keyword", field.name);
            if schema.get(&exact).is_some_and(|field_type| field_type == "keyword") {
                field.exact = Some(exact);
            }
        }
        Some(field)
    }

    /// The planner's equality on an analyzed text field is a phrase match, which is a words
    /// condition rather than exact equality.
    fn planner_leaf(&self, filter: &FilterPredicate) -> Leaf {
        let mut leaf = filter.to_leaf();
        if is_text(&filter.field_type) && filter.operator == FilterOperator::Eq {
            leaf.op = Op::Words(filter.value.text());
        }
        leaf
    }

    fn check_leaf(&self, leaf: &Leaf) -> Result<(), TranslationError> {
        if leaf.case_insensitive && !matches!(leaf.op, Op::Words(_)) {
            return Err(TranslationError(
                "Elasticsearch query_string has no case-insensitive matching for exact values",
            ));
        }
        let analyzed = leaf
            .field
            .as_ref()
            .is_some_and(|field| is_text(&field.field_type) && field.exact.is_none());
        match &leaf.op {
            Op::Contains(_) | Op::Suffix(_) => Err(TranslationError(
                "Elasticsearch disables leading wildcards, so contains and suffix matches are not available",
            )),
            Op::Regex(regex) if regex.starts_unbounded() => Err(TranslationError(
                "Elasticsearch disables leading wildcards, so a regex cannot start with .* or .+",
            )),
            Op::Eq(_) | Op::Prefix(_) | Op::Regex(_) | Op::Compare(..) | Op::In(_) if analyzed => Err(TranslationError(
                "this is an analyzed text field without a .keyword sub-field, so only word matches are exact",
            )),
            Op::Words(_) if leaf.field.as_ref().is_some_and(|field| !is_text(&field.field_type)) => {
                Err(TranslationError("word matching needs an analyzed text field"))
            }
            Op::In(values) if values.is_empty() || values.len() > 256 => {
                Err(TranslationError("a value list needs between 1 and 256 values"))
            }
            Op::Words(text) => word_tokens(text).map(|_| ()),
            _ => Ok(()),
        }
    }

    fn read(&self, query: &str) -> Result<Query, TranslationError> {
        let tokens = tokenize(query)?;
        let mut parser = Reader { tokens, position: 0, field: None };
        let expr = parser.clauses(0)?;
        if parser.position != parser.tokens.len() {
            return Err(UNSUPPORTED);
        }
        Ok(Query::Filter(expr.simplify().merge_equalities()))
    }

    fn write(&self, query: &Query, _: &WriteContext) -> Result<String, TranslationError> {
        match query {
            Query::Filter(expr) => {
                let expr = expr.clone().simplify();
                if expr == Expr::all() {
                    return Ok(String::new());
                }
                write_expr(&expr, false)
            }
            Query::Metric(_) => Err(TranslationError(
                "Lucene has no aggregations, so computations cannot run on Elasticsearch",
            )),
        }
    }
}

fn is_text(field_type: &str) -> bool {
    matches!(field_type, "text" | "match_only_text")
}

fn write_expr(expr: &Expr, nested: bool) -> Result<String, TranslationError> {
    Ok(match expr {
        Expr::And(items) if items.is_empty() => "*:*".to_owned(),
        Expr::Or(items) if items.is_empty() => "NOT *:*".to_owned(),
        Expr::And(items) => group(items, " AND ", nested)?,
        Expr::Or(items) => group(items, " OR ", nested)?,
        Expr::Not(inner) => format!("NOT {}", write_expr(inner, true)?),
        Expr::Leaf(leaf) => write_leaf(leaf)?,
    })
}

fn group(items: &[Expr], separator: &str, nested: bool) -> Result<String, TranslationError> {
    let parts = items
        .iter()
        .map(|item| write_expr(item, true))
        .collect::<Result<Vec<_>, _>>()?;
    let joined = parts.join(separator);
    Ok(if nested { format!("({joined})") } else { joined })
}

fn write_leaf(leaf: &Leaf) -> Result<String, TranslationError> {
    let Some(field) = &leaf.field else {
        let Op::Words(text) = &leaf.op else {
            return Err(TranslationError("only free-text words can omit a field"));
        };
        return Ok(format!("\"{}\"", word_tokens(text)?.join(" ")));
    };
    // Exact conditions on an analyzed field use its `.keyword` sub-field.
    let name = escape(match (&leaf.op, &field.exact) {
        (Op::Words(_), _) | (_, None) => &field.name,
        (_, Some(exact)) => exact,
    }, false);
    // Phrases on message fields read back as word matches, so exact values there are regexes.
    if is_message_field(&name) {
        let values = match &leaf.op {
            Op::Eq(value) => Some(vec![value.text()]),
            Op::In(values) => Some(values.iter().map(FilterValue::text).collect()),
            _ => None,
        };
        if let Some(values) = values {
            let regex = PortableRegex::alternatives(&values).to_lucene().ok_or(UNSUPPORTED)?;
            return Ok(format!("{name}:/{regex}/"));
        }
    }
    Ok(match &leaf.op {
        Op::Eq(value) => format!("{name}:\"{}\"", escape(&value.text(), false)),
        Op::In(values) => format!(
            "{name}:({})",
            values
                .iter()
                .map(|value| format!("\"{}\"", escape(&value.text(), false)))
                .collect::<Vec<_>>()
                .join(" OR ")
        ),
        Op::Prefix(text) => format!("{name}:{}*", escape(text, true)),
        Op::Regex(regex) => format!(
            "{name}:/{}/",
            regex
                .to_lucene()
                .ok_or(TranslationError("Lucene regexes cannot be case-insensitive"))?
        ),
        Op::Words(text) => format!("{name}:\"{}\"", word_tokens(text)?.join(" ")),
        Op::Exists => format!("_exists_:{name}"),
        Op::Compare(cmp, value) => match cmp {
            Cmp::Gt => format!("{name}:{{{} TO *]", bound(value)),
            Cmp::Gte => format!("{name}:[{} TO *]", bound(value)),
            Cmp::Lt => format!("{name}:[* TO {}}}", bound(value)),
            Cmp::Lte => format!("{name}:[* TO {}]", bound(value)),
        },
        Op::Contains(_) | Op::Suffix(_) => {
            return Err(TranslationError("Elasticsearch disables leading wildcards"))
        }
    })
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

#[derive(Clone, Debug, PartialEq)]
enum Token {
    Open,
    Close,
    And,
    Or,
    Not,
    Must,
    MustNot,
    /// A `field:` prefix followed by `(`, applying the field to a group.
    FieldGroup(String),
    Term(String),
}

/// Splits a query into terms and operators, keeping quotes, ranges, regexes, and escapes whole.
fn tokenize(query: &str) -> Result<Vec<Token>, TranslationError> {
    let characters = query.chars().collect::<Vec<_>>();
    let mut tokens = Vec::new();
    let mut index = 0;
    while index < characters.len() {
        let character = characters[index];
        if character.is_whitespace() {
            index += 1;
            continue;
        }
        match character {
            '(' => {
                tokens.push(Token::Open);
                index += 1;
                continue;
            }
            ')' => {
                tokens.push(Token::Close);
                index += 1;
                continue;
            }
            '+' => {
                tokens.push(Token::Must);
                index += 1;
                continue;
            }
            '-' | '!' if index + 1 < characters.len() && !characters[index + 1].is_whitespace() => {
                tokens.push(Token::MustNot);
                index += 1;
                continue;
            }
            _ => {}
        }
        if characters[index..].starts_with(&['&', '&']) {
            tokens.push(Token::And);
            index += 2;
            continue;
        }
        if characters[index..].starts_with(&['|', '|']) {
            tokens.push(Token::Or);
            index += 2;
            continue;
        }
        // A term: everything up to unquoted, unescaped whitespace or a closing parenthesis; a
        // `field:(` prefix starts a field group instead.
        let start = index;
        let (mut quoted, mut escaped, mut depth, mut regex, mut field_group) = (false, false, 0usize, false, false);
        while index < characters.len() {
            let character = characters[index];
            if escaped {
                escaped = false;
            } else if character == '\\' {
                escaped = true;
            } else if regex {
                if character == '/' {
                    regex = false;
                }
            } else if character == '"' {
                quoted = !quoted;
            } else if !quoted {
                if character == '/' && index > start && characters[index - 1] == ':' {
                    regex = true;
                } else if character == '/' && index == start {
                    return Err(TranslationError("a regex needs a field, such as field:/regex/"));
                } else if matches!(character, '[' | '{') {
                    depth += 1;
                } else if matches!(character, ']' | '}') {
                    depth = depth.checked_sub(1).ok_or(UNSUPPORTED)?;
                } else if depth == 0 && (character.is_whitespace() || character == ')') {
                    break;
                } else if depth == 0 && character == '(' {
                    field_group = true;
                    break;
                }
            }
            index += 1;
        }
        if field_group {
            let prefix = characters[start..index].iter().collect::<String>();
            let field = prefix.strip_suffix(':').filter(|field| !field.is_empty()).ok_or(UNSUPPORTED)?;
            tokens.push(Token::FieldGroup(field.to_owned()));
            tokens.push(Token::Open);
            index += 1;
            continue;
        }
        if quoted || escaped || depth != 0 || regex {
            return Err(TranslationError("unterminated quote, escape, range, or regex"));
        }
        let term = characters[start..index].iter().collect::<String>();
        tokens.push(match term.as_str() {
            "AND" => Token::And,
            "OR" => Token::Or,
            "NOT" => Token::Not,
            _ => Token::Term(term),
        });
    }
    Ok(tokens)
}

struct Reader {
    tokens: Vec<Token>,
    position: usize,
    /// The field a `field:(…)` group applies to its bare terms.
    field: Option<String>,
}

#[derive(PartialEq)]
enum Occur {
    Must,
    Should,
    MustNot,
}

impl Reader {
    fn peek(&self) -> Option<&Token> {
        self.tokens.get(self.position)
    }

    /// One level of clauses with Lucene's semantics: with any required clause, optional clauses
    /// only affect scoring; otherwise at least one optional clause must match; prohibited
    /// clauses always exclude.
    fn clauses(&mut self, depth: usize) -> Result<Expr, TranslationError> {
        if depth > 32 {
            return Err(TranslationError("the query nests too deeply"));
        }
        let mut clauses: Vec<(Occur, Expr)> = Vec::new();
        let (mut saw_and, mut saw_or) = (false, false);
        let mut pending_and = false;
        while let Some(token) = self.peek() {
            match token {
                Token::Close => break,
                Token::And => {
                    if clauses.is_empty() || pending_and {
                        return Err(UNSUPPORTED);
                    }
                    saw_and = true;
                    pending_and = true;
                    // `a AND b` makes the previous clause required too.
                    if let Some((occur @ Occur::Should, _)) = clauses.last_mut() {
                        *occur = Occur::Must;
                    }
                    self.position += 1;
                    continue;
                }
                Token::Or => {
                    if clauses.is_empty() || pending_and {
                        return Err(UNSUPPORTED);
                    }
                    saw_or = true;
                    self.position += 1;
                    continue;
                }
                _ => {}
            }
            let (occur, expr) = self.clause(depth)?;
            let occur = match occur {
                Occur::Should if pending_and => Occur::Must,
                other => other,
            };
            pending_and = false;
            clauses.push((occur, expr));
        }
        if pending_and {
            return Err(TranslationError("the query ends with AND"));
        }
        if saw_and && saw_or {
            return Err(TranslationError(
                "Lucene does not apply AND/OR precedence; add parentheses when combining AND with OR",
            ));
        }
        let (mut must, mut should, mut must_not) = (Vec::new(), Vec::new(), Vec::new());
        for (occur, expr) in clauses {
            match occur {
                Occur::Must => must.push(expr),
                Occur::Should => should.push(expr),
                Occur::MustNot => must_not.push(expr.negate()),
            }
        }
        if !must.is_empty() && !should.is_empty() {
            return Err(TranslationError(
                "optional terms beside required ones only affect scoring in Lucene; use AND or OR explicitly",
            ));
        }
        let mut items = must;
        if !should.is_empty() {
            items.push(Expr::Or(should));
        }
        items.extend(must_not);
        Ok(Expr::And(items).simplify())
    }

    fn clause(&mut self, depth: usize) -> Result<(Occur, Expr), TranslationError> {
        let occur = match self.peek() {
            Some(Token::Must) => {
                self.position += 1;
                Occur::Must
            }
            Some(Token::MustNot) | Some(Token::Not) => {
                self.position += 1;
                Occur::MustNot
            }
            _ => Occur::Should,
        };
        if matches!(self.peek(), Some(Token::Must | Token::MustNot | Token::Not)) {
            return Err(UNSUPPORTED);
        }
        let expr = match self.tokens.get(self.position).cloned() {
            Some(Token::Open) => {
                self.position += 1;
                let inner = self.clauses(depth + 1)?;
                if self.peek() != Some(&Token::Close) {
                    return Err(UNSUPPORTED);
                }
                self.position += 1;
                inner
            }
            Some(Token::FieldGroup(field)) => {
                if self.field.is_some() {
                    return Err(UNSUPPORTED);
                }
                self.position += 2;
                self.field = Some(unescape(&field));
                let inner = self.clauses(depth + 1);
                self.field = None;
                let inner = inner?;
                if self.peek() != Some(&Token::Close) {
                    return Err(UNSUPPORTED);
                }
                self.position += 1;
                inner
            }
            Some(Token::Term(term)) => {
                self.position += 1;
                read_term(&term, self.field.as_deref())?
            }
            _ => return Err(UNSUPPORTED),
        };
        Ok((occur, expr))
    }
}

fn read_term(term: &str, group_field: Option<&str>) -> Result<Expr, TranslationError> {
    if term == "*:*" {
        return Ok(Expr::all());
    }
    let colon = unescaped(term).find(|(_, character)| *character == ':').map(|(index, _)| index);
    let (field, value) = match (colon, group_field) {
        (Some(colon), None) => {
            let raw = &term[..colon];
            if raw.is_empty() || has_unescaped(raw, SPECIAL) {
                return Err(UNSUPPORTED);
            }
            (Some(unescape(raw)), &term[colon + 1..])
        }
        (Some(_), Some(_)) => return Err(UNSUPPORTED),
        (None, field) => (field.map(str::to_owned), term),
    };
    if fuzzy_or_boost(value) {
        return Err(TranslationError("fuzzy, proximity, and boost operators cannot be translated"));
    }
    let Some(field) = field else {
        // Bare terms search every field's words.
        let text = match value.strip_prefix('"') {
            Some(phrase) => unescape(phrase.strip_suffix('"').ok_or(UNSUPPORTED)?),
            None if !has_unescaped(value, SPECIAL) => unescape(value),
            None => return Err(TranslationError("wildcards and operators need a field")),
        };
        word_tokens(&text)?;
        return Ok(Expr::Leaf(Leaf::text(text)));
    };
    if field == "_exists_" {
        return Ok(Expr::Leaf(Leaf::new(unescape(value), Op::Exists)));
    }
    let leaf = |op| Expr::Leaf(Leaf::new(field.clone(), op));
    // Message fields are analyzed text, where a phrase or a bare word matches words.
    let words = |text: String| match word_tokens(&text) {
        Ok(_) if is_message_field(&field) => Expr::Leaf(Leaf::new(field.clone(), Op::Words(text))),
        _ => leaf(Op::Eq(FilterValue::String(text))),
    };
    if let Some(phrase) = value.strip_prefix('"') {
        let phrase = phrase.strip_suffix('"').filter(|inner| !ends_escaped(inner)).ok_or(UNSUPPORTED)?;
        return Ok(words(unescape(phrase)));
    }
    if let Some(body) = value.strip_prefix('/') {
        let body = body.strip_suffix('/').ok_or(UNSUPPORTED)?;
        return Ok(leaf(Op::Regex(PortableRegex::from_lucene(body)?)));
    }
    if let Some(range) = value.strip_prefix(['[', '{']) {
        return read_range(&field, value.starts_with('['), range);
    }
    for (symbol, cmp) in [(">=", Cmp::Gte), ("<=", Cmp::Lte), (">", Cmp::Gt), ("<", Cmp::Lt)] {
        if let Some(bound) = value.strip_prefix(symbol) {
            let bound = read_bound(bound)?.ok_or(UNSUPPORTED)?;
            return Ok(leaf(Op::Compare(cmp, bound)));
        }
    }
    if value.is_empty() {
        return Err(UNSUPPORTED);
    }
    // Wildcards: `x*` is a prefix; `*x*` and `*x` are kept for reading (other targets support
    // them); any other `*`/`?` becomes a regex.
    let wildcards = unescaped(value).filter(|(_, character)| matches!(character, '*' | '?')).count();
    if wildcards > 0 {
        if has_unescaped(value, "\\+-=&|><!(){}[]^\"~:/") {
            return Err(UNSUPPORTED);
        }
        let inner = value.strip_prefix('*').and_then(|rest| rest.strip_suffix('*'));
        if let Some(inner) = inner.filter(|inner| !inner.is_empty() && !has_unescaped(inner, "*?") && !ends_escaped(inner)) {
            return Ok(leaf(Op::Contains(unescape(inner))));
        }
        if let Some(prefix) = value.strip_suffix('*').filter(|prefix| !prefix.is_empty() && !has_unescaped(prefix, "*?") && !ends_escaped(prefix)) {
            return Ok(leaf(Op::Prefix(unescape(prefix))));
        }
        if let Some(suffix) = value.strip_prefix('*').filter(|suffix| !suffix.is_empty() && !has_unescaped(suffix, "*?")) {
            return Ok(leaf(Op::Suffix(unescape(suffix))));
        }
        return Ok(leaf(Op::Regex(wildcard_regex(value)?)));
    }
    if has_unescaped(value, SPECIAL) {
        return Err(UNSUPPORTED);
    }
    let text = unescape(value);
    if is_message_field(&field) {
        return Ok(words(text));
    }
    Ok(leaf(Op::Eq(if value == text {
        literal_value(&text)
    } else {
        FilterValue::String(text)
    })))
}

/// `term~2`, `"a b"~3`, and `term^2`: an unescaped `~` or `^` outside a phrase or regex.
fn fuzzy_or_boost(value: &str) -> bool {
    if value.starts_with('/') {
        return false;
    }
    let (mut quoted, mut escaped) = (false, false);
    for character in value.chars() {
        match character {
            _ if escaped => escaped = false,
            '\\' => escaped = true,
            '"' => quoted = !quoted,
            '~' | '^' if !quoted => return true,
            _ => {}
        }
    }
    false
}

/// Lucene wildcards (`*` any run, `?` one character) as a portable regex.
fn wildcard_regex(value: &str) -> Result<PortableRegex, TranslationError> {
    let mut pattern = String::new();
    let mut escaped = false;
    for character in value.chars() {
        if escaped {
            pattern.push_str(&crate::investigation_dialect::quote_meta(&character.to_string()));
            escaped = false;
        } else if character == '\\' {
            escaped = true;
        } else if character == '*' {
            pattern.push_str(".*");
        } else if character == '?' {
            pattern.push('.');
        } else {
            pattern.push_str(&crate::investigation_dialect::quote_meta(&character.to_string()));
        }
    }
    PortableRegex::from_re2(&pattern)
}

fn read_range(field: &str, inclusive_low: bool, range: &str) -> Result<Expr, TranslationError> {
    let inclusive_high = range.ends_with(']');
    let inner = range.strip_suffix([']', '}']).ok_or(UNSUPPORTED)?;
    let parts = split_on_to(inner)?;
    let [low, high] = parts.as_slice() else {
        return Err(UNSUPPORTED);
    };
    let (low, high) = (read_bound(low)?, read_bound(high)?);
    let mut leaves = Vec::new();
    if let Some(low) = low {
        let cmp = if inclusive_low { Cmp::Gte } else { Cmp::Gt };
        leaves.push(Expr::Leaf(Leaf::new(field, Op::Compare(cmp, low))));
    }
    if let Some(high) = high {
        let cmp = if inclusive_high { Cmp::Lte } else { Cmp::Lt };
        leaves.push(Expr::Leaf(Leaf::new(field, Op::Compare(cmp, high))));
    }
    if leaves.is_empty() {
        // `field:[* TO *]` matches any value, which is the same as the field existing.
        return Ok(Expr::Leaf(Leaf::new(field, Op::Exists)));
    }
    Ok(Expr::And(leaves).simplify())
}

/// Splits a range body on ` TO ` outside quotes and escapes.
fn split_on_to(inner: &str) -> Result<Vec<String>, TranslationError> {
    let characters = inner.chars().collect::<Vec<_>>();
    let mut parts = Vec::new();
    let mut current = String::new();
    let (mut quoted, mut escaped, mut index) = (false, false, 0);
    while index < characters.len() {
        let character = characters[index];
        if !quoted && !escaped && characters[index..].starts_with(&[' ', 'T', 'O', ' ']) {
            parts.push(std::mem::take(&mut current));
            index += 4;
            continue;
        }
        if escaped {
            escaped = false;
        } else if character == '\\' {
            escaped = true;
        } else if character == '"' {
            quoted = !quoted;
        }
        current.push(character);
        index += 1;
    }
    if quoted {
        return Err(UNSUPPORTED);
    }
    parts.push(current);
    Ok(parts.into_iter().map(|part| part.trim().to_owned()).collect())
}

fn read_bound(bound: &str) -> Result<Option<FilterValue>, TranslationError> {
    if bound == "*" {
        return Ok(None);
    }
    if let Some(quoted) = bound.strip_prefix('"') {
        let inner = quoted.strip_suffix('"').filter(|inner| !ends_escaped(inner)).ok_or(UNSUPPORTED)?;
        return Ok(Some(FilterValue::String(unescape(inner))));
    }
    if bound.is_empty() || has_unescaped(bound, SPECIAL) {
        return Err(UNSUPPORTED);
    }
    let text = unescape(bound);
    if text.starts_with("now") {
        return Err(TranslationError("relative date math such as now-1h cannot be translated; use the time range"));
    }
    Ok(Some(if bound == text { literal_value(&text) } else { FilterValue::String(text) }))
}

/// Characters with their byte offsets, skipping backslash-escaped ones.
fn unescaped(text: &str) -> impl Iterator<Item = (usize, char)> + '_ {
    let mut escaped = false;
    text.char_indices().filter(move |(_, character)| {
        if escaped {
            escaped = false;
            false
        } else if *character == '\\' {
            escaped = true;
            false
        } else {
            true
        }
    })
}

fn has_unescaped(text: &str, characters: &str) -> bool {
    unescaped(text).any(|(_, character)| characters.contains(character) || character.is_whitespace())
}

fn ends_escaped(text: &str) -> bool {
    text.chars().rev().take_while(|character| *character == '\\').count() % 2 == 1
}

fn unescape(text: &str) -> String {
    let mut unescaped = String::with_capacity(text.len());
    let mut escaped = false;
    for character in text.chars() {
        if !escaped && character == '\\' {
            escaped = true;
        } else {
            unescaped.push(character);
            escaped = false;
        }
    }
    unescaped
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::investigation_dialect::{build_query, FilterPredicate};

    fn eq(field: &str, value: &str) -> Expr {
        Expr::Leaf(Leaf::new(field, Op::Eq(FilterValue::String(value.to_owned()))))
    }

    fn read(query: &str) -> Expr {
        match dialect().read(query) {
            Ok(Query::Filter(expr)) => expr,
            other => panic!("{query}: {other:?}"),
        }
    }

    fn write(expr: Expr) -> String {
        dialect()
            .write(
                &Query::Filter(expr),
                &WriteContext { index: "logs", time_field: None, time_range: None, schema: &Schema::new() },
            )
            .unwrap()
    }

    #[test]
    fn ai_filters_translate_to_escaped_lucene() {
        let filters = [
            FilterPredicate { field: "status".to_owned(), field_type: "keyword".to_owned(), operator: FilterOperator::Eq, value: FilterValue::String("500 OR *".to_owned()) },
            FilterPredicate { field: "latency".to_owned(), field_type: "long".to_owned(), operator: FilterOperator::Gte, value: FilterValue::Number(250.into()) },
            FilterPredicate { field: "service".to_owned(), field_type: "keyword".to_owned(), operator: FilterOperator::Lt, value: FilterValue::String("say \"hi\" b".to_owned()) },
        ];
        assert_eq!(
            build_query(dialect(), "logs-*", &filters).unwrap(),
            r#"status:"500 OR \*" AND latency:[250 TO *] AND service:[* TO "say \"hi\" b"}"#
        );
        assert_eq!(build_query(dialect(), "logs-*", &[]).unwrap(), "");
    }

    #[test]
    fn boolean_logic_and_every_condition_round_trip() {
        let expr = Expr::Or(vec![
            Expr::And(vec![eq("service", "checkout"), Expr::Not(Box::new(eq("level", "debug")))]),
            Expr::Leaf(Leaf::new("path", Op::Prefix("/api v1".to_owned()))),
            Expr::Leaf(Leaf::new("code", Op::In(vec![FilterValue::String("500".to_owned()), FilterValue::String("503".to_owned())]))),
            Expr::Leaf(Leaf::new("route", Op::Regex(PortableRegex::from_re2("/v[0-9]+/x").unwrap()))),
            Expr::Leaf(Leaf::new("trace", Op::Exists)),
            Expr::Leaf(Leaf::text("gateway timeout")),
        ]);
        let written = write(expr.clone());
        assert_eq!(
            written,
            r#"(service:"checkout" AND NOT level:"debug") OR path:\/api\ v1* OR code:("500" OR "503") OR route:/\/v[0-9]+\/x/ OR _exists_:trace OR "gateway timeout""#
        );
        assert_eq!(read(&written), expr);
    }

    #[test]
    fn lucene_clause_semantics_are_read_faithfully() {
        assert_eq!(read("a:1 b:2"), Expr::Or(vec![eq_number("a", 1), eq_number("b", 2)]));
        assert_eq!(read("+a:1 -b:2"), Expr::And(vec![eq_number("a", 1), Expr::Not(Box::new(eq_number("b", 2)))]));
        assert_eq!(read("a:1 -b:2"), Expr::And(vec![eq_number("a", 1), Expr::Not(Box::new(eq_number("b", 2)))]));
        assert_eq!(read("NOT a:1"), Expr::Not(Box::new(eq_number("a", 1))));
        assert_eq!(read("a:(x OR y)"), Expr::Leaf(Leaf::new("a", Op::In(vec![FilterValue::String("x".to_owned()), FilterValue::String("y".to_owned())]))));
        assert_eq!(read("status:>=500"), Expr::Leaf(Leaf::new("status", Op::Compare(Cmp::Gte, FilterValue::Number(500.into())))));
        assert_eq!(read("status:[* TO *]"), Expr::Leaf(Leaf::new("status", Op::Exists)));
        assert_eq!(read("path:a?c*d"), Expr::Leaf(Leaf::new("path", Op::Regex(PortableRegex::from_re2("a.c.*d").unwrap()))));
        assert_eq!(read("path:*v1*"), Expr::Leaf(Leaf::new("path", Op::Contains("v1".to_owned()))));
        for rejected in ["a:1 AND b:2 OR c:3", "+a:1 b:2", "a:foo~2", "a:foo^3", "/x/", "a:[now-1h TO *]", "a:1 AND", "(a:1", "a:\"x"] {
            assert!(dialect().read(rejected).is_err(), "{rejected}");
        }
    }

    fn eq_number(field: &str, value: i64) -> Expr {
        Expr::Leaf(Leaf::new(field, Op::Eq(FilterValue::Number(value.into()))))
    }

    #[test]
    fn leading_wildcards_case_and_analyzed_fields_are_refused() {
        let text_field = |op| Leaf { field: Some(Field::typed("message", "text")), op, case_insensitive: false };
        for leaf in [
            Leaf::new("path", Op::Contains("x".to_owned())),
            Leaf::new("path", Op::Suffix("x".to_owned())),
            Leaf::new("path", Op::Regex(PortableRegex::from_re2(".*x").unwrap())),
            Leaf::new("path", Op::Eq(FilterValue::String("x".to_owned()))).insensitive(),
            text_field(Op::Prefix("x".to_owned())),
            text_field(Op::Eq(FilterValue::String("x".to_owned()))),
        ] {
            assert!(dialect().check_leaf(&leaf).is_err(), "{leaf:?}");
        }
        assert!(dialect().check_leaf(&text_field(Op::Words("x y".to_owned()))).is_ok());
        let planner = FilterPredicate { field: "message".to_owned(), field_type: "text".to_owned(), operator: FilterOperator::Eq, value: FilterValue::String("gateway timeout".to_owned()) };
        assert_eq!(crate::investigation_dialect::build_query(dialect(), "logs", &[planner]).unwrap(), r#"message:"gateway timeout""#);
        let schema = Schema::from([
            ("message".to_owned(), "text".to_owned()),
            ("message.keyword".to_owned(), "keyword".to_owned()),
            ("resource.attributes.service.name".to_owned(), "keyword".to_owned()),
        ]);
        let resolved = dialect().resolve_field("_msg", &schema).unwrap();
        assert_eq!((resolved.name.as_str(), resolved.exact.as_deref()), ("message", Some("message.keyword")));
        assert_eq!(dialect().resolve_field("service_name", &schema), None);
        assert_eq!(dialect().resolve_field("ResourceAttributes['service.name']", &schema).unwrap().name, "resource.attributes.service.name");
        let leaf = Leaf { field: Some(resolved), op: Op::Prefix("Gate".to_owned()), case_insensitive: false };
        assert!(dialect().check_leaf(&leaf).is_ok());
        assert_eq!(write(Expr::Leaf(leaf)), "message.keyword:Gate*");
    }
}
