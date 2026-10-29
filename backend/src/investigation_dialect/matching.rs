//! String conditions as RE2 patterns, and back.
//!
//! Label matchers (LogQL, PromQL) match the whole value; line filters (`|~`) and ClickHouse
//! `match` search within it. Writers and readers both go through here, so each pattern a writer
//! produces reads back as the condition it came from.

use super::regex::Shape;
use super::{
    read_words_regex, words_regex, words_value_regex, FilterValue, Op, PortableRegex,
    TranslationError,
};

/// The whole-value pattern for a string condition. `Eq` without case-insensitivity is written
/// with `=` instead, so it has no pattern.
pub fn value_pattern(op: &Op, case_insensitive: bool) -> Result<Option<String>, TranslationError> {
    let regex = match op {
        Op::Eq(value) if case_insensitive => PortableRegex::literal(&value.text()),
        Op::In(values) => PortableRegex::alternatives(&texts(values)),
        Op::Contains(text) => PortableRegex::contains(text),
        Op::Prefix(text) => PortableRegex::prefix(text),
        Op::Suffix(text) => PortableRegex::suffix(text),
        Op::Regex(regex) => return Ok(Some(regex.to_re2())),
        Op::Words(text) => return words_value_regex(text).map(Some),
        Op::Eq(_) | Op::Exists | Op::Compare(..) => return Ok(None),
    };
    Ok(Some(regex.with_case_insensitive(case_insensitive).to_re2()))
}

/// The pattern that searches a value (or log line) for a string condition.
pub fn search_pattern(op: &Op, case_insensitive: bool) -> Result<String, TranslationError> {
    let flags = if case_insensitive { "(?i)" } else { "" };
    Ok(match op {
        Op::Contains(text) => format!("{flags}{}", super::quote_meta(text)),
        Op::Prefix(text) => format!("{flags}^{}", super::quote_meta(text)),
        Op::Suffix(text) => format!("{flags}{}$", super::quote_meta(text)),
        Op::Eq(value) => format!("{flags}^{}$", super::quote_meta(&value.text())),
        Op::In(values) => PortableRegex::alternatives(&texts(values))
            .with_case_insensitive(case_insensitive)
            .to_anchored_re2(),
        Op::Regex(regex) => regex.to_anchored_re2(),
        Op::Words(text) => words_regex(text)?,
        Op::Exists | Op::Compare(..) => {
            return Err(TranslationError("this condition cannot search text"))
        }
    })
}

/// The condition a whole-value pattern expresses, and whether it is case-insensitive.
pub fn classify_value_pattern(pattern: &str) -> Result<(Op, bool), TranslationError> {
    if let Some(words) = read_words_regex(pattern) {
        return Ok((Op::Words(words), true));
    }
    let regex = PortableRegex::from_re2(pattern)?;
    let case_insensitive = regex.case_insensitive;
    let op = match regex.shape() {
        Shape::Literal(text) => Op::Eq(FilterValue::String(text)),
        Shape::Contains(text) => Op::Contains(text),
        Shape::Prefix(text) => Op::Prefix(text),
        Shape::Suffix(text) => Op::Suffix(text),
        Shape::Alternatives(values) => {
            Op::In(values.into_iter().map(FilterValue::String).collect())
        }
        Shape::Other => return Ok((Op::Regex(regex), false)),
    };
    Ok((op, case_insensitive))
}

/// The condition a search pattern expresses: unanchored text is `contains`, `^…$` is the
/// whole value.
pub fn classify_search_pattern(pattern: &str) -> Result<(Op, bool), TranslationError> {
    if read_words_regex(pattern).is_some() {
        return classify_value_pattern(pattern);
    }
    let (flags, core) = match pattern.strip_prefix("(?") {
        Some(rest) => match rest.split_once(')') {
            Some((flags, core))
                if !flags.is_empty() && flags.chars().all(|flag| matches!(flag, 'i' | 's')) =>
            {
                (format!("(?{flags})"), core)
            }
            _ => (String::new(), pattern),
        },
        None => (String::new(), pattern),
    };
    let anchored_start = core.starts_with('^');
    let trimmed = core.strip_prefix('^').unwrap_or(core);
    let anchored_end = super::regex::strip_end_anchor(trimmed).is_some();
    let trimmed = super::regex::strip_end_anchor(trimmed).unwrap_or(trimmed);
    let whole = format!(
        "{flags}{}(?:{trimmed}){}",
        if anchored_start { "" } else { ".*" },
        if anchored_end { "" } else { ".*" }
    );
    classify_value_pattern(&whole)
}

fn texts(values: &[FilterValue]) -> Vec<String> {
    values.iter().map(FilterValue::text).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn string(text: &str) -> FilterValue {
        FilterValue::String(text.to_owned())
    }

    #[test]
    fn value_and_search_patterns_read_back_as_their_conditions() {
        let ops = [
            Op::Eq(string("a.b")),
            Op::In(vec![string("x|y"), string("z")]),
            Op::Contains("it's \"x\"".to_owned()),
            Op::Prefix("/api".to_owned()),
            Op::Suffix(".log".to_owned()),
            Op::Words("gateway timeout".to_owned()),
            Op::Regex(PortableRegex::from_re2("v[0-9]+\\.x").unwrap()),
        ];
        for op in ops {
            for case_insensitive in [false, true] {
                if matches!(op, Op::Words(_) | Op::Regex(_)) && case_insensitive {
                    continue;
                }
                let expected_case = case_insensitive || matches!(op, Op::Words(_));
                if let Some(pattern) = value_pattern(&op, case_insensitive).unwrap() {
                    assert_eq!(
                        classify_value_pattern(&pattern).unwrap(),
                        (op.clone(), expected_case),
                        "{pattern}"
                    );
                }
                let pattern = search_pattern(&op, case_insensitive).unwrap();
                assert_eq!(
                    classify_search_pattern(&pattern).unwrap(),
                    (op.clone(), expected_case),
                    "{pattern}"
                );
            }
        }
    }

    #[test]
    fn plain_search_text_is_contains() {
        assert_eq!(
            classify_search_pattern("timeout").unwrap(),
            (Op::Contains("timeout".to_owned()), false)
        );
        assert_eq!(
            classify_search_pattern("(?i)^GET ").unwrap(),
            (Op::Prefix("GET ".to_owned()), true)
        );
        assert!(matches!(
            classify_search_pattern("time.*out").unwrap().0,
            Op::Regex(_)
        ));
    }
}
