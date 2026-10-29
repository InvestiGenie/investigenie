use crate::investigation_dialect::{
    literal_value, FilterOperator, FilterPredicate, FilterValue, InvestigationDialect,
    ParsedFilter, TranslationError,
};

struct LuceneDialect;

pub fn dialect() -> &'static dyn InvestigationDialect {
    &LuceneDialect
}

/// Lucene `query_string` syntax characters; a backslash makes each one literal.
const SPECIAL: &str = "\\+-=&|><!(){}[]^\"~*?:/";
const UNSUPPORTED: TranslationError = TranslationError(
    "only field:value, field:\"phrase\", field:*text*, and field:[low TO high] terms joined by AND can be translated",
);

impl InvestigationDialect for LuceneDialect {
    fn language(&self) -> &'static str {
        "Lucene"
    }

    fn example(&self) -> &'static str {
        "service:\"checkout\" AND status:[500 TO *] AND message:*timeout*"
    }

    /// No `contains`: its translation `field:*text*` needs a leading wildcard, which the
    /// connector disables (`allow_leading_wildcard: false`) to keep searches cheap. Reading
    /// `field:*text*` still works, for queries translated to other sources.
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

    fn translate(&self, _: &str, filters: &[FilterPredicate]) -> Result<String, TranslationError> {
        Ok(filters
            .iter()
            .map(lucene_filter)
            .collect::<Vec<_>>()
            .join(" AND "))
    }

    fn read(&self, query: &str) -> Result<Vec<ParsedFilter>, TranslationError> {
        let mut filters = Vec::new();
        for term in split_terms(query)? {
            read_term(&term, &mut filters)?;
        }
        Ok(filters)
    }
}

fn lucene_filter(filter: &FilterPredicate) -> String {
    let field = escape(&filter.field, false);
    let text = filter.value.text();
    match filter.operator {
        FilterOperator::Eq => format!("{field}:\"{}\"", escape(&text, false)),
        // Unquoted, so whitespace is escaped too; otherwise `*a b*` becomes two terms.
        FilterOperator::Contains => format!("{field}:*{}*", escape(&text, true)),
        FilterOperator::Gt => format!("{field}:{{{} TO *]", bound(&filter.value)),
        FilterOperator::Gte => format!("{field}:[{} TO *]", bound(&filter.value)),
        FilterOperator::Lt => format!("{field}:[* TO {}}}", bound(&filter.value)),
        FilterOperator::Lte => format!("{field}:[* TO {}]", bound(&filter.value)),
    }
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

/// Splits on `AND` outside quotes, ranges, and escapes. Lucene's default operator is OR, so
/// terms without `AND` between them, and explicit `OR`/`NOT`, are rejected.
fn split_terms(query: &str) -> Result<Vec<String>, TranslationError> {
    let mut words = Vec::new();
    let mut word = String::new();
    let (mut quoted, mut escaped, mut depth) = (false, false, 0usize);
    for character in query.chars() {
        if escaped {
            escaped = false;
        } else if character == '\\' {
            escaped = true;
        } else if character == '"' && depth == 0 {
            quoted = !quoted;
        } else if !quoted && matches!(character, '[' | '{') {
            depth += 1;
        } else if !quoted && matches!(character, ']' | '}') {
            depth = depth.checked_sub(1).ok_or(UNSUPPORTED)?;
        } else if !quoted && depth == 0 && character.is_whitespace() {
            if !word.is_empty() {
                words.push(std::mem::take(&mut word));
            }
            continue;
        }
        word.push(character);
    }
    if quoted || escaped || depth != 0 {
        return Err(TranslationError("unterminated quote, escape, or range"));
    }
    if !word.is_empty() {
        words.push(word);
    }
    let mut terms = Vec::new();
    let mut expect_term = true;
    for word in words {
        match (word.as_str(), expect_term) {
            ("AND" | "&&", false) => expect_term = true,
            ("OR" | "||" | "NOT" | "!", _) => {
                return Err(TranslationError("only AND can join terms; OR and NOT cannot be translated"))
            }
            (_, true) => {
                terms.push(word);
                expect_term = false;
            }
            (_, false) => return Err(TranslationError("join terms with AND")),
        }
    }
    if expect_term && !terms.is_empty() {
        return Err(TranslationError("the query ends with AND"));
    }
    Ok(terms)
}

fn read_term(term: &str, filters: &mut Vec<ParsedFilter>) -> Result<(), TranslationError> {
    let colon = unescaped(term).find(|(_, character)| *character == ':').map(|(index, _)| index);
    let Some(colon) = colon else {
        return Err(TranslationError("every term needs a field, such as field:value"));
    };
    let raw_field = &term[..colon];
    let value = &term[colon + 1..];
    if raw_field.is_empty() || has_unescaped(raw_field, SPECIAL) {
        return Err(UNSUPPORTED);
    }
    let field = &unescape(raw_field);
    if let Some(phrase) = value.strip_prefix('"') {
        let phrase = phrase.strip_suffix('"').filter(|inner| !ends_escaped(inner)).ok_or(UNSUPPORTED)?;
        filters.push(ParsedFilter::new(field, FilterOperator::Eq, FilterValue::String(unescape(phrase))));
    } else if let Some(range) = value.strip_prefix(['[', '{']) {
        read_range(field, value.starts_with('['), range, filters)?;
    } else if let Some(text) = value.strip_prefix('*').and_then(|rest| rest.strip_suffix('*')) {
        if text.is_empty() || ends_escaped(text) || has_unescaped(text, "*?") {
            return Err(UNSUPPORTED);
        }
        filters.push(ParsedFilter::new(field, FilterOperator::Contains, FilterValue::String(unescape(text))));
    } else if !value.is_empty() && !has_unescaped(value, SPECIAL) {
        let text = unescape(value);
        let parsed = if value == text { literal_value(&text) } else { FilterValue::String(text) };
        filters.push(ParsedFilter::new(field, FilterOperator::Eq, parsed));
    } else {
        return Err(UNSUPPORTED);
    }
    Ok(())
}

fn read_range(
    field: &str,
    inclusive_low: bool,
    range: &str,
    filters: &mut Vec<ParsedFilter>,
) -> Result<(), TranslationError> {
    let inclusive_high = range.ends_with(']');
    let inner = range.strip_suffix([']', '}']).ok_or(UNSUPPORTED)?;
    let parts = split_terms_on_to(inner)?;
    let [low, high] = parts.as_slice() else {
        return Err(UNSUPPORTED);
    };
    let (low, high) = (read_bound(low)?, read_bound(high)?);
    if low.is_none() && high.is_none() {
        return Err(TranslationError("a range needs at least one bound"));
    }
    if let Some(low) = low {
        let operator = if inclusive_low { FilterOperator::Gte } else { FilterOperator::Gt };
        filters.push(ParsedFilter::new(field, operator, low));
    }
    if let Some(high) = high {
        let operator = if inclusive_high { FilterOperator::Lte } else { FilterOperator::Lt };
        filters.push(ParsedFilter::new(field, operator, high));
    }
    Ok(())
}

/// Splits a range body on ` TO ` outside quotes and escapes.
fn split_terms_on_to(inner: &str) -> Result<Vec<String>, TranslationError> {
    let characters = inner.chars().collect::<Vec<_>>();
    let mut parts = Vec::new();
    let mut current = String::new();
    let (mut quoted, mut escaped, mut index) = (false, false, 0);
    while index < characters.len() {
        let character = characters[index];
        if !quoted && !escaped && inner[byte_offset(&characters, index)..].starts_with(" TO ") {
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

fn byte_offset(characters: &[char], index: usize) -> usize {
    characters[..index].iter().map(|character| character.len_utf8()).sum()
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
    use crate::investigation_dialect::build_query;

    fn predicate(field: &str, operator: FilterOperator, value: FilterValue) -> FilterPredicate {
        FilterPredicate {
            field: field.to_owned(),
            field_type: "keyword".to_owned(),
            operator,
            value,
        }
    }

    fn text(value: &str) -> FilterValue {
        FilterValue::String(value.to_owned())
    }

    #[test]
    fn filters_translate_to_escaped_lucene() {
        let filters = [
            predicate("status", FilterOperator::Eq, text("500 OR *")),
            predicate("latency", FilterOperator::Gte, FilterValue::Number(250.into())),
            predicate("service", FilterOperator::Lt, text("say \"hi\" b")),
        ];
        assert_eq!(
            build_query(dialect(), "logs-*", &filters).unwrap(),
            r#"status:"500 OR \*" AND latency:[250 TO *] AND service:[* TO "say \"hi\" b"}"#
        );
        let contains = predicate("message", FilterOperator::Contains, text("hello world"));
        assert!(build_query(dialect(), "logs-*", &[contains]).is_err());
        assert_eq!(build_query(dialect(), "logs-*", &[]).unwrap(), "");
    }

    #[test]
    fn reading_accepts_the_portable_subset() {
        let read = dialect()
            .read(r#"service:"check out" AND status:[500 TO 599} && message:*time\ out* AND code:42 AND ok:true AND host:web\-1"#)
            .unwrap();
        assert_eq!(
            read,
            [
                ParsedFilter::new("service", FilterOperator::Eq, text("check out")),
                ParsedFilter::new("status", FilterOperator::Gte, FilterValue::Number(500.into())),
                ParsedFilter::new("status", FilterOperator::Lt, FilterValue::Number(599.into())),
                ParsedFilter::new("message", FilterOperator::Contains, text("time out")),
                ParsedFilter::new("code", FilterOperator::Eq, FilterValue::Number(42.into())),
                ParsedFilter::new("ok", FilterOperator::Eq, FilterValue::Bool(true)),
                ParsedFilter::new("host", FilterOperator::Eq, text("web-1")),
            ]
        );
        assert_eq!(dialect().read("  ").unwrap(), []);
        assert_eq!(
            dialect().read(r#"name:{"a b" TO *]"#).unwrap(),
            [ParsedFilter::new("name", FilterOperator::Gt, text("a b"))]
        );
    }

    #[test]
    fn field_names_are_escaped_and_operators_follow_the_field_type() {
        let filter = predicate("user-agent", FilterOperator::Eq, text("curl"));
        assert_eq!(build_query(dialect(), "logs", &[filter]).unwrap(), r#"user\-agent:"curl""#);
        assert_eq!(
            dialect().read(r#"user\-agent:"curl""#).unwrap(),
            [ParsedFilter::new("user-agent", FilterOperator::Eq, text("curl"))]
        );
        assert_eq!(dialect().field_operators("text"), [FilterOperator::Eq]);
        for field_type in ["keyword", "long", "date", ""] {
            assert!(!dialect().field_operators(field_type).contains(&FilterOperator::Contains));
            assert!(dialect().field_operators(field_type).contains(&FilterOperator::Gte));
        }
    }

    #[test]
    fn reading_rejects_anything_it_cannot_translate_exactly() {
        for query in [
            "error",
            "status:500 OR status:503",
            "status:500 service:api",
            "NOT status:500",
            "status:500 AND",
            "(status:500)",
            "status:50?",
            "message:te*xt",
            "status:500^2",
            "status:[* TO *]",
            "status:[1 TO 2 TO 3]",
            "status:\"open",
            "message:/regex/",
            "-status:500",
            "status:>500",
        ] {
            assert!(dialect().read(query).is_err(), "{query}");
        }
    }
}
