//! A portable subset of regular expressions, read from and written to both RE2 (LogQL, PromQL,
//! ClickHouse `match`) and Lucene.
//!
//! Patterns always match the whole value, as Lucene, LogQL label filters, and PromQL do; writers
//! that search within a value (ClickHouse `match`, LogQL line filters) add the anchoring. The
//! subset covers literals, `.`, character classes with ranges, groups, alternation, and the
//! quantifiers `* + ? {n} {n,} {n,m}`. RE2's `\d`, `\w`, and `\s` become equivalent classes;
//! anything without a Lucene equivalent (lookaround, backreferences, Unicode classes, anchors
//! inside the pattern) is rejected.

use super::TranslationError;

const UNSUPPORTED: TranslationError =
    TranslationError("this regex uses syntax that cannot be translated between query languages");

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PortableRegex {
    node: Node,
    /// Case-insensitive; RE2 writes it as a leading `(?i)`, and Lucene cannot express it.
    pub case_insensitive: bool,
}

#[derive(Clone, Debug, PartialEq, Eq)]
enum Node {
    Alternation(Vec<Node>),
    Concatenation(Vec<Node>),
    Repeat(Box<Node>, u32, Option<u32>),
    Group(Box<Node>),
    Literal(char),
    Any,
    Class(bool, Vec<ClassItem>),
}

#[derive(Clone, Debug, PartialEq, Eq)]
enum ClassItem {
    Char(char),
    Range(char, char),
}

const RE2_META: &str = r"\.+*?()|[]{}^$";
/// Lucene's reserved characters with all syntax flags enabled, as Elasticsearch uses them, plus
/// `/`, which ends the regex inside a `query_string`.
const LUCENE_META: &str = "\\.+*?()|[]{}\"#@&<>~/";
const MAX_PATTERN_CHARS: usize = 1024;
const MAX_REPEAT: u32 = 1000;

impl PortableRegex {
    /// A regex matching exactly `text`.
    pub fn literal(text: &str) -> Self {
        Self {
            node: literal_node(text),
            case_insensitive: false,
        }
    }

    pub fn contains(text: &str) -> Self {
        Self::sequence(vec![any_run(), literal_node(text), any_run()])
    }

    pub fn prefix(text: &str) -> Self {
        Self::sequence(vec![literal_node(text), any_run()])
    }

    pub fn suffix(text: &str) -> Self {
        Self::sequence(vec![any_run(), literal_node(text)])
    }

    /// One of several exact values.
    pub fn alternatives(values: &[String]) -> Self {
        Self {
            node: Node::Alternation(values.iter().map(|value| literal_node(value)).collect()),
            case_insensitive: false,
        }
    }

    fn sequence(items: Vec<Node>) -> Self {
        Self {
            node: Node::Concatenation(items),
            case_insensitive: false,
        }
    }

    pub fn with_case_insensitive(mut self, case_insensitive: bool) -> Self {
        self.case_insensitive = case_insensitive;
        self
    }

    /// Reads an RE2 pattern with whole-value semantics; a leading `^` and trailing `$` are
    /// redundant there and dropped. Leading `(?i)`, `(?s)`, and `(?is)` flags are accepted;
    /// `.` always matches newlines in the portable subset, as it does in Lucene.
    pub fn from_re2(pattern: &str) -> Result<Self, TranslationError> {
        let (case_insensitive, rest) = strip_flags(pattern)?;
        let rest = rest.strip_prefix('^').unwrap_or(rest);
        let rest = strip_end_anchor(rest).unwrap_or(rest);
        Parser::new(rest, Flavor::Re2).parse().map(|node| Self {
            node,
            case_insensitive,
        })
    }

    /// Reads a Lucene regex (the body between `/…/`).
    pub fn from_lucene(pattern: &str) -> Result<Self, TranslationError> {
        Parser::new(pattern, Flavor::Lucene)
            .parse()
            .map(|node| Self {
                node,
                case_insensitive: false,
            })
    }

    /// RE2 with whole-value semantics, as LogQL and PromQL label matchers apply it.
    pub fn to_re2(&self) -> String {
        let mut pattern = self.re2_flags();
        write_node(&self.node, Flavor::Re2, &mut pattern);
        pattern
    }

    /// RE2 anchored for engines that search within the value, such as ClickHouse `match` and
    /// LogQL line filters.
    pub fn to_anchored_re2(&self) -> String {
        let mut pattern = self.re2_flags();
        pattern.push_str("^(?:");
        write_node(&self.node, Flavor::Re2, &mut pattern);
        pattern.push_str(")$");
        pattern
    }

    /// `(?s)` makes RE2's `.` match newlines as Lucene's does; `(?i)` is case-insensitivity.
    fn re2_flags(&self) -> String {
        match (self.case_insensitive, self.has_any()) {
            (true, true) => "(?is)".to_owned(),
            (true, false) => "(?i)".to_owned(),
            (false, true) => "(?s)".to_owned(),
            (false, false) => String::new(),
        }
    }

    fn has_any(&self) -> bool {
        fn any(node: &Node) -> bool {
            match node {
                Node::Alternation(items) | Node::Concatenation(items) => items.iter().any(any),
                Node::Repeat(inner, ..) | Node::Group(inner) => any(inner),
                Node::Any => true,
                Node::Literal(_) | Node::Class(..) => false,
            }
        }
        any(&self.node)
    }

    /// Lucene syntax, or `None` when the pattern is case-insensitive.
    pub fn to_lucene(&self) -> Option<String> {
        if self.case_insensitive {
            return None;
        }
        let mut pattern = String::new();
        write_node(&self.node, Flavor::Lucene, &mut pattern);
        Some(pattern)
    }

    /// The simplest condition this pattern is equivalent to, so readers can turn `.*x.*` back
    /// into `contains` and `a|b` into `in`.
    pub fn shape(&self) -> Shape {
        /// Strips groups and one-item sequences, which do not change what matches.
        fn unwrap(node: &Node) -> &Node {
            match node {
                Node::Group(inner) => unwrap(inner),
                Node::Concatenation(items) if items.len() == 1 => unwrap(&items[0]),
                other => other,
            }
        }
        /// Literal characters through any nesting of groups and sequences.
        fn literal(node: &Node, text: &mut String) -> bool {
            match node {
                Node::Literal(character) => {
                    text.push(*character);
                    true
                }
                Node::Group(inner) => literal(inner, text),
                Node::Concatenation(items) => items.iter().all(|item| literal(item, text)),
                _ => false,
            }
        }
        fn literal_text(nodes: &[Node]) -> Option<String> {
            let mut text = String::new();
            nodes
                .iter()
                .all(|node| literal(node, &mut text))
                .then_some(text)
        }
        fn any_run(node: &Node) -> bool {
            matches!(unwrap(node), Node::Repeat(inner, 0, None) if **inner == Node::Any)
        }
        /// Top-level items with nested sequences spliced in.
        fn items(node: &Node, out: &mut Vec<Node>) {
            match node {
                Node::Concatenation(inner) => inner.iter().for_each(|item| items(item, out)),
                Node::Group(inner) if matches!(**inner, Node::Concatenation(_)) => {
                    items(inner, out)
                }
                other => out.push(other.clone()),
            }
        }
        let node = unwrap(&self.node);
        if let Node::Alternation(branches) = node {
            return branches
                .iter()
                .map(|branch| literal_text(std::slice::from_ref(branch)))
                .collect::<Option<Vec<_>>>()
                .map_or(Shape::Other, Shape::Alternatives);
        }
        let mut sequence = Vec::new();
        items(node, &mut sequence);
        let leading = sequence.first().is_some_and(any_run);
        let trailing =
            sequence.len() > usize::from(leading) && sequence.last().is_some_and(any_run);
        let core = &sequence[usize::from(leading)..sequence.len() - usize::from(trailing)];
        if !leading && !trailing && core.len() == 1 && !matches!(unwrap(&core[0]), Node::Literal(_))
        {
            return Shape::Other;
        }
        match (literal_text(core), leading, trailing) {
            (Some(text), false, false) => Shape::Literal(text),
            (Some(text), true, true) if !text.is_empty() => Shape::Contains(text),
            (Some(text), false, true) if !text.is_empty() => Shape::Prefix(text),
            (Some(text), true, false) if !text.is_empty() => Shape::Suffix(text),
            _ => Shape::Other,
        }
    }

    /// Whether the pattern matches the empty string, which makes a LogQL or PromQL matcher
    /// unable to select streams on its own.
    pub fn matches_empty(&self) -> bool {
        fn empty(node: &Node) -> bool {
            match node {
                Node::Alternation(branches) => branches.iter().any(empty),
                Node::Concatenation(items) => items.iter().all(empty),
                Node::Repeat(inner, min, _) => *min == 0 || empty(inner),
                Node::Group(inner) => empty(inner),
                Node::Literal(_) | Node::Any | Node::Class(..) => false,
            }
        }
        empty(&self.node)
    }

    /// Whether the pattern can begin with an unbounded run of any character, like `.*x`, which
    /// needs the same full scan as a leading wildcard.
    pub fn starts_unbounded(&self) -> bool {
        fn leading(node: &Node) -> bool {
            match node {
                Node::Alternation(branches) => branches.iter().any(leading),
                Node::Concatenation(items) => items.first().is_some_and(leading),
                Node::Group(inner) => leading(inner),
                Node::Repeat(inner, _, None) => {
                    matches!(**inner, Node::Any | Node::Class(true, _)) || leading(inner)
                }
                Node::Repeat(inner, _, _) => leading(inner),
                _ => false,
            }
        }
        leading(&self.node)
    }
}

/// `pattern` without a trailing `$` anchor, which an even run of backslashes before it leaves
/// unescaped.
pub fn strip_end_anchor(pattern: &str) -> Option<&str> {
    let inner = pattern.strip_suffix('$')?;
    let backslashes = inner.len() - inner.trim_end_matches('\\').len();
    (backslashes % 2 == 0).then_some(inner)
}

/// What a whole-value regex is equivalent to.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Shape {
    Literal(String),
    Contains(String),
    Prefix(String),
    Suffix(String),
    Alternatives(Vec<String>),
    Other,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Flavor {
    Re2,
    Lucene,
}

struct Parser<'a> {
    characters: std::iter::Peekable<std::str::Chars<'a>>,
    flavor: Flavor,
    length: usize,
}

impl<'a> Parser<'a> {
    fn new(pattern: &'a str, flavor: Flavor) -> Self {
        Self {
            characters: pattern.chars().peekable(),
            flavor,
            length: pattern.chars().count(),
        }
    }

    fn parse(mut self) -> Result<Node, TranslationError> {
        if self.length > MAX_PATTERN_CHARS {
            return Err(TranslationError("the regex is too long"));
        }
        let node = self.alternation(0)?;
        if self.characters.next().is_some() {
            return Err(UNSUPPORTED);
        }
        Ok(node)
    }

    fn alternation(&mut self, depth: usize) -> Result<Node, TranslationError> {
        if depth > 32 {
            return Err(TranslationError("the regex nests too deeply"));
        }
        let mut branches = vec![self.concatenation(depth)?];
        while self.characters.peek() == Some(&'|') {
            self.characters.next();
            branches.push(self.concatenation(depth)?);
        }
        Ok(if branches.len() == 1 {
            branches.pop().unwrap()
        } else {
            Node::Alternation(branches)
        })
    }

    fn concatenation(&mut self, depth: usize) -> Result<Node, TranslationError> {
        let mut items = Vec::new();
        while let Some(&character) = self.characters.peek() {
            if matches!(character, '|' | ')') {
                break;
            }
            let atom = self.atom(depth)?;
            // Groups without a quantifier only bracket; inline them so equivalent spellings
            // compare equal.
            match self.quantified(atom)? {
                Node::Group(inner) if !matches!(*inner, Node::Alternation(_)) => match *inner {
                    Node::Concatenation(inner) => items.extend(inner),
                    other => items.push(other),
                },
                other => items.push(other),
            }
        }
        if let [Node::Group(_)] = items.as_slice() {
            let Some(Node::Group(inner)) = items.pop() else {
                unreachable!()
            };
            return Ok(*inner);
        }
        Ok(Node::Concatenation(items))
    }

    fn quantified(&mut self, mut atom: Node) -> Result<Node, TranslationError> {
        loop {
            let (min, max) = match self.characters.peek() {
                Some('*') => (0, None),
                Some('+') => (1, None),
                Some('?') => (0, Some(1)),
                Some('{') => {
                    self.characters.next();
                    let (min, max) = self.bounds()?;
                    atom = Node::Repeat(Box::new(atom), min, max);
                    self.reject_lazy()?;
                    continue;
                }
                _ => return Ok(atom),
            };
            self.characters.next();
            self.reject_lazy()?;
            atom = Node::Repeat(Box::new(atom), min, max);
        }
    }

    /// Lazy quantifiers (`*?`) only change capture positions, not what matches, but RE2 and
    /// Lucene disagree on them; RE2 keeps them out of the subset.
    fn reject_lazy(&mut self) -> Result<(), TranslationError> {
        if self.flavor == Flavor::Re2 && self.characters.peek() == Some(&'?') {
            return Err(UNSUPPORTED);
        }
        Ok(())
    }

    fn bounds(&mut self) -> Result<(u32, Option<u32>), TranslationError> {
        let mut text = String::new();
        loop {
            match self.characters.next() {
                Some('}') => break,
                Some(character) if character.is_ascii_digit() || character == ',' => {
                    text.push(character)
                }
                _ => return Err(UNSUPPORTED),
            }
        }
        let number = |part: &str| {
            part.parse::<u32>()
                .ok()
                .filter(|value| *value <= MAX_REPEAT)
        };
        match text.split_once(',') {
            None => {
                let exact = number(&text).ok_or(UNSUPPORTED)?;
                Ok((exact, Some(exact)))
            }
            Some((min, "")) => Ok((number(min).ok_or(UNSUPPORTED)?, None)),
            Some((min, max)) => {
                let (min, max) = (
                    number(min).ok_or(UNSUPPORTED)?,
                    number(max).ok_or(UNSUPPORTED)?,
                );
                if min > max {
                    return Err(UNSUPPORTED);
                }
                Ok((min, Some(max)))
            }
        }
    }

    fn atom(&mut self, depth: usize) -> Result<Node, TranslationError> {
        let Some(character) = self.characters.next() else {
            return Err(UNSUPPORTED);
        };
        match character {
            '.' => Ok(Node::Any),
            '(' => {
                if self.characters.peek() == Some(&'?') {
                    self.characters.next();
                    // Only the non-capturing `(?:` form; flags and names are outside the subset.
                    if self.flavor == Flavor::Lucene || self.characters.next() != Some(':') {
                        return Err(UNSUPPORTED);
                    }
                }
                let inner = self.alternation(depth + 1)?;
                if self.characters.next() != Some(')') {
                    return Err(UNSUPPORTED);
                }
                Ok(Node::Group(Box::new(inner)))
            }
            '[' => self.class(),
            '\\' => self.escape(),
            '*' | '+' | '?' | '{' | ')' | ']' | '}' => Err(UNSUPPORTED),
            '^' | '$' if self.flavor == Flavor::Re2 => Err(UNSUPPORTED),
            _ if self.flavor == Flavor::Lucene && "\"#@&<>~^$".contains(character) => {
                Err(UNSUPPORTED)
            }
            other => Ok(Node::Literal(other)),
        }
    }

    fn escape(&mut self) -> Result<Node, TranslationError> {
        let escaped = self.characters.next().ok_or(UNSUPPORTED)?;
        if self.flavor == Flavor::Re2 {
            if let Some(class) = shorthand_class(escaped) {
                return Ok(class);
            }
            return match escaped {
                'n' => Ok(Node::Literal('\n')),
                't' => Ok(Node::Literal('\t')),
                'r' => Ok(Node::Literal('\r')),
                _ if RE2_META.contains(escaped) || (escaped.is_ascii_punctuation()) => {
                    Ok(Node::Literal(escaped))
                }
                _ => Err(UNSUPPORTED),
            };
        }
        // Lucene treats any escaped character as a literal.
        Ok(Node::Literal(escaped))
    }

    fn class(&mut self) -> Result<Node, TranslationError> {
        let negated = self.characters.peek() == Some(&'^');
        if negated {
            self.characters.next();
        }
        let mut items = Vec::new();
        let mut first = true;
        loop {
            let character = self.characters.next().ok_or(UNSUPPORTED)?;
            let item = match character {
                ']' if !first => break,
                '\\' => {
                    let escaped = self.characters.next().ok_or(UNSUPPORTED)?;
                    if self.flavor == Flavor::Re2 {
                        if let Some(Node::Class(false, shorthand)) = shorthand_class(escaped) {
                            items.extend(shorthand);
                            first = false;
                            continue;
                        }
                        if escaped.is_ascii_alphanumeric() && !matches!(escaped, 'n' | 't' | 'r') {
                            return Err(UNSUPPORTED);
                        }
                    }
                    match escaped {
                        'n' if self.flavor == Flavor::Re2 => '\n',
                        't' if self.flavor == Flavor::Re2 => '\t',
                        'r' if self.flavor == Flavor::Re2 => '\r',
                        other => other,
                    }
                }
                '[' if self.flavor == Flavor::Re2 && self.characters.peek() == Some(&':') => {
                    return Err(UNSUPPORTED)
                }
                other => other,
            };
            first = false;
            if self.characters.peek() == Some(&'-') {
                let mut lookahead = self.characters.clone();
                lookahead.next();
                if lookahead.peek().is_some_and(|next| *next != ']') {
                    self.characters.next();
                    let mut end = self.characters.next().ok_or(UNSUPPORTED)?;
                    if end == '\\' {
                        end = self.characters.next().ok_or(UNSUPPORTED)?;
                    }
                    if end < item {
                        return Err(UNSUPPORTED);
                    }
                    items.push(ClassItem::Range(item, end));
                    continue;
                }
            }
            items.push(ClassItem::Char(item));
        }
        if items.is_empty() {
            return Err(UNSUPPORTED);
        }
        Ok(Node::Class(negated, items))
    }
}

fn literal_node(text: &str) -> Node {
    Node::Concatenation(text.chars().map(Node::Literal).collect())
}

fn any_run() -> Node {
    Node::Repeat(Box::new(Node::Any), 0, None)
}

/// Splits leading RE2 flags; only `i` and `s` are portable.
fn strip_flags(pattern: &str) -> Result<(bool, &str), TranslationError> {
    let Some(rest) = pattern.strip_prefix("(?") else {
        return Ok((false, pattern));
    };
    let Some(end) = rest.find(')') else {
        return Err(UNSUPPORTED);
    };
    let flags = &rest[..end];
    if flags.is_empty()
        || flags.contains(':')
        || !flags.chars().all(|flag| matches!(flag, 'i' | 's'))
    {
        return Ok((false, pattern));
    }
    Ok((flags.contains('i'), &rest[end + 1..]))
}

/// RE2's `\d`, `\w`, and `\s` as explicit ASCII classes; negated forms only outside a class.
fn shorthand_class(escaped: char) -> Option<Node> {
    let items = match escaped.to_ascii_lowercase() {
        'd' => vec![ClassItem::Range('0', '9')],
        'w' => vec![
            ClassItem::Range('0', '9'),
            ClassItem::Range('A', 'Z'),
            ClassItem::Range('a', 'z'),
            ClassItem::Char('_'),
        ],
        's' => [' ', '\t', '\n', '\r', '\x0B', '\x0C']
            .into_iter()
            .map(ClassItem::Char)
            .collect(),
        _ => return None,
    };
    Some(Node::Class(escaped.is_ascii_uppercase(), items))
}

fn write_node(node: &Node, flavor: Flavor, out: &mut String) {
    match node {
        Node::Alternation(branches) => {
            for (index, branch) in branches.iter().enumerate() {
                if index > 0 {
                    out.push('|');
                }
                write_node(branch, flavor, out);
            }
        }
        Node::Concatenation(items) => {
            for item in items {
                // A bare alternation inside a concatenation needs its own group.
                if matches!(item, Node::Alternation(_)) {
                    write_group(item, flavor, out);
                } else {
                    write_node(item, flavor, out);
                }
            }
        }
        Node::Repeat(inner, min, max) => {
            match **inner {
                Node::Literal(_) | Node::Any | Node::Class(..) | Node::Group(_) => {
                    write_node(inner, flavor, out)
                }
                _ => write_group(inner, flavor, out),
            }
            match (min, max) {
                (0, None) => out.push('*'),
                (1, None) => out.push('+'),
                (0, Some(1)) => out.push('?'),
                (min, None) => out.push_str(&format!("{{{min},}}")),
                (min, Some(max)) if min == max => out.push_str(&format!("{{{min}}}")),
                (min, Some(max)) => out.push_str(&format!("{{{min},{max}}}")),
            }
        }
        Node::Group(inner) => write_group(inner, flavor, out),
        Node::Literal(character) => write_literal(*character, flavor, out),
        Node::Any => out.push('.'),
        Node::Class(negated, items) => {
            out.push('[');
            if *negated {
                out.push('^');
            }
            for item in items {
                match item {
                    ClassItem::Char(character) => write_class_char(*character, flavor, out),
                    ClassItem::Range(start, end) => {
                        write_class_char(*start, flavor, out);
                        out.push('-');
                        write_class_char(*end, flavor, out);
                    }
                }
            }
            out.push(']');
        }
    }
}

fn write_group(node: &Node, flavor: Flavor, out: &mut String) {
    out.push_str(if flavor == Flavor::Re2 { "(?:" } else { "(" });
    write_node(node, flavor, out);
    out.push(')');
}

fn write_literal(character: char, flavor: Flavor, out: &mut String) {
    match (flavor, character) {
        (Flavor::Re2, '\n') => out.push_str("\\n"),
        (Flavor::Re2, '\t') => out.push_str("\\t"),
        (Flavor::Re2, '\r') => out.push_str("\\r"),
        (Flavor::Re2, _) if RE2_META.contains(character) => {
            out.push('\\');
            out.push(character);
        }
        (Flavor::Lucene, _) if LUCENE_META.contains(character) => {
            out.push('\\');
            out.push(character);
        }
        _ => out.push(character),
    }
}

fn write_class_char(character: char, flavor: Flavor, out: &mut String) {
    match (flavor, character) {
        (Flavor::Re2, '\n') => out.push_str("\\n"),
        (Flavor::Re2, '\t') => out.push_str("\\t"),
        (Flavor::Re2, '\r') => out.push_str("\\r"),
        (_, '\\' | ']' | '[' | '^' | '-') => {
            out.push('\\');
            out.push(character);
        }
        (Flavor::Lucene, _) if LUCENE_META.contains(character) => {
            out.push('\\');
            out.push(character);
        }
        _ => out.push(character),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn patterns_round_trip_between_re2_and_lucene() {
        for (re2, lucene) in [
            ("/api/v[0-9]+/.*", "\\/api\\/v[0-9]+\\/.*"),
            ("(?:get|post)-[a-z_]{2,5}", "(get|post)-[a-z_]{2,5}"),
            ("a\\.b\\+c\\?", "a\\.b\\+c\\?"),
            ("[^\\]x-]+", "[^\\]x\\-]+"),
            ("say\"hi\"@home#1", "say\\\"hi\\\"\\@home\\#1"),
            ("x{3}y{1,}z?", "x{3}y+z?"),
        ] {
            let parsed = PortableRegex::from_re2(re2).unwrap();
            assert_eq!(parsed.to_lucene().unwrap(), lucene, "{re2}");
            assert_eq!(
                PortableRegex::from_lucene(lucene).unwrap(),
                parsed,
                "{lucene}"
            );
            assert_eq!(
                PortableRegex::from_re2(&parsed.to_re2()).unwrap(),
                parsed,
                "{re2}"
            );
        }
    }

    #[test]
    fn shorthand_classes_anchors_and_flags_are_normalized() {
        let parsed = PortableRegex::from_re2("^(?i)x").err();
        assert!(parsed.is_some());
        let parsed = PortableRegex::from_re2("(?i)^code-\\d+\\s\\w$").unwrap();
        assert!(parsed.case_insensitive);
        assert_eq!(
            parsed.to_re2(),
            "(?i)code-[0-9]+[ \\t\\n\\r\u{b}\u{c}][0-9A-Za-z_]"
        );
        assert_eq!(
            parsed.to_anchored_re2(),
            "(?i)^(?:code-[0-9]+[ \\t\\n\\r\u{b}\u{c}][0-9A-Za-z_])$"
        );
        assert_eq!(
            PortableRegex::from_re2("(?si)a.b").unwrap().to_re2(),
            "(?is)a.b"
        );
        assert_eq!(PortableRegex::contains("a.b").to_re2(), "(?s).*a\\.b.*");
        assert!(parsed.to_lucene().is_none());
        assert_eq!(PortableRegex::literal("a.b").to_re2(), "a\\.b");
        assert_eq!(PortableRegex::literal("a.b").to_lucene().unwrap(), "a\\.b");
    }

    #[test]
    fn shapes_recover_the_simplest_condition() {
        for (pattern, shape) in [
            ("abc", Shape::Literal("abc".to_owned())),
            (".*a\\.b.*", Shape::Contains("a.b".to_owned())),
            ("(?s).*(?:x y).*", Shape::Contains("x y".to_owned())),
            ("/api.*", Shape::Prefix("/api".to_owned())),
            (".*\\.log", Shape::Suffix(".log".to_owned())),
            (
                "a|b\\|c",
                Shape::Alternatives(vec!["a".to_owned(), "b|c".to_owned()]),
            ),
            (
                "(?:a|b)",
                Shape::Alternatives(vec!["a".to_owned(), "b".to_owned()]),
            ),
            ("a.c", Shape::Other),
            (".*", Shape::Other),
        ] {
            assert_eq!(
                PortableRegex::from_re2(pattern).unwrap().shape(),
                shape,
                "{pattern}"
            );
        }
        for constructed in [
            PortableRegex::contains("x"),
            PortableRegex::prefix("x"),
            PortableRegex::suffix("x"),
            PortableRegex::alternatives(&["a".to_owned(), "b".to_owned()]),
        ] {
            assert_eq!(
                PortableRegex::from_re2(&constructed.to_re2())
                    .unwrap()
                    .shape(),
                constructed.shape()
            );
        }
        assert!(PortableRegex::from_re2("a*").unwrap().matches_empty());
        assert!(!PortableRegex::contains("x").matches_empty());
    }

    #[test]
    fn unbounded_leading_patterns_are_detected() {
        for (pattern, unbounded) in [
            (".*x", true),
            ("(?:.+|a)x", true),
            ("[^/]*x", true),
            ("a.*", false),
            ("x{0,3}y", false),
            ("(a|.*)", true),
        ] {
            assert_eq!(
                PortableRegex::from_re2(pattern).unwrap().starts_unbounded(),
                unbounded,
                "{pattern}"
            );
        }
    }

    #[test]
    fn syntax_outside_the_subset_is_rejected() {
        for pattern in [
            "(?=x)",
            "(?P<n>x)",
            "\\1",
            "\\p{L}",
            "a^b",
            "a$b",
            "x*?",
            "[[:alpha:]]",
            "x{2000}",
            "x{3,1}",
            "(x",
            "x)",
            "*x",
            "\\bword",
        ] {
            assert!(PortableRegex::from_re2(pattern).is_err(), "{pattern}");
        }
        for pattern in ["a&b", "~a", "<1-5>", "a@", "#", "(?:x)"] {
            assert!(PortableRegex::from_lucene(pattern).is_err(), "{pattern}");
        }
    }
}
