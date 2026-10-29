//! The language-neutral filter tree every dialect reads and writes.
//!
//! Semantics, which each writer reproduces on its engine (verified against real engines by the
//! live matrix test):
//! - A condition on a field is false when the record lacks the field (or, for numeric
//!   comparisons, when the value is not a number).
//! - `Not` is the exact complement, so it includes records that lack the field.
//! - String conditions are case-sensitive unless `case_insensitive` is set; `Words` is always
//!   case-insensitive and matches whole words.
//! - `Regex` matches the whole value; see `PortableRegex`.

use super::{FilterValue, PortableRegex, TranslationError};

/// A field as named by the source query, and once resolved, as it exists in the target.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Field {
    pub name: String,
    /// The type the target reported; empty until the field is resolved.
    pub field_type: String,
    /// A sibling field for exact matching, such as Elasticsearch's `message.keyword`.
    pub exact: Option<String>,
}

impl Field {
    pub fn named(name: impl Into<String>) -> Self {
        Self::typed(name, "")
    }

    pub fn typed(name: impl Into<String>, field_type: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            field_type: field_type.into(),
            exact: None,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Cmp {
    Gt,
    Gte,
    Lt,
    Lte,
}

impl Cmp {
    pub fn symbol(self) -> &'static str {
        match self {
            Self::Gt => ">",
            Self::Gte => ">=",
            Self::Lt => "<",
            Self::Lte => "<=",
        }
    }

    pub fn from_symbol(symbol: &str) -> Option<Self> {
        Some(match symbol {
            ">" => Self::Gt,
            ">=" => Self::Gte,
            "<" => Self::Lt,
            "<=" => Self::Lte,
            _ => return None,
        })
    }

    /// The comparison true exactly where this one is false, for values that are numbers.
    pub fn complement(self) -> Self {
        match self {
            Self::Gt => Self::Lte,
            Self::Gte => Self::Lt,
            Self::Lt => Self::Gte,
            Self::Lte => Self::Gt,
        }
    }
}

#[derive(Clone, Debug, PartialEq)]
pub enum Op {
    Eq(FilterValue),
    In(Vec<FilterValue>),
    Contains(String),
    Prefix(String),
    Suffix(String),
    Regex(PortableRegex),
    /// Whole words in order, case-insensitive, separated by non-word characters: the free-text
    /// match Elasticsearch's analyzed fields, `\b…\b` regexes, and ClickHouse `match` share.
    Words(String),
    Exists,
    Compare(Cmp, FilterValue),
}

#[derive(Clone, Debug, PartialEq)]
pub struct Leaf {
    /// `None` only for free-text `Words`, which searches the whole record or log line.
    pub field: Option<Field>,
    pub op: Op,
    pub case_insensitive: bool,
}

impl Leaf {
    pub fn new(field: impl Into<String>, op: Op) -> Self {
        Self {
            field: Some(Field::named(field)),
            op,
            case_insensitive: false,
        }
    }

    pub fn text(words: impl Into<String>) -> Self {
        Self {
            field: None,
            op: Op::Words(words.into()),
            case_insensitive: true,
        }
    }

    #[cfg(test)]
    pub fn insensitive(mut self) -> Self {
        self.case_insensitive = true;
        self
    }
}

#[derive(Clone, Debug, PartialEq)]
pub enum Expr {
    And(Vec<Expr>),
    Or(Vec<Expr>),
    Not(Box<Expr>),
    Leaf(Leaf),
}

/// Writers that expand into an OR of conjunctions (PromQL) refuse queries that would explode.
pub const MAX_CONJUNCTIONS: usize = 32;
const MAX_LEAVES: usize = 128;

impl Expr {
    /// Matches every record.
    pub fn all() -> Self {
        Self::And(Vec::new())
    }

    pub fn negate(self) -> Self {
        match self {
            Self::Not(inner) => *inner,
            other => Self::Not(Box::new(other)),
        }
    }

    pub fn leaves(&self) -> Vec<&Leaf> {
        let mut leaves = Vec::new();
        self.visit(&mut |leaf| leaves.push(leaf));
        leaves
    }

    fn visit<'a>(&'a self, visitor: &mut impl FnMut(&'a Leaf)) {
        match self {
            Self::And(items) | Self::Or(items) => items.iter().for_each(|item| item.visit(visitor)),
            Self::Not(inner) => inner.visit(visitor),
            Self::Leaf(leaf) => visitor(leaf),
        }
    }

    pub fn leaves_mut(&mut self) -> Vec<&mut Leaf> {
        let mut leaves = Vec::new();
        fn visit<'a>(expr: &'a mut Expr, leaves: &mut Vec<&'a mut Leaf>) {
            match expr {
                Expr::And(items) | Expr::Or(items) => {
                    items.iter_mut().for_each(|item| visit(item, leaves))
                }
                Expr::Not(inner) => visit(inner, leaves),
                Expr::Leaf(leaf) => leaves.push(leaf),
            }
        }
        visit(self, &mut leaves);
        leaves
    }

    /// Bounds the work every writer does on a query.
    pub fn check_size(&self) -> Result<(), TranslationError> {
        if self.leaves().len() > MAX_LEAVES {
            return Err(TranslationError("the query has too many conditions"));
        }
        Ok(())
    }

    /// Flattens nested AND/OR, unwraps single-item groups, and removes double negation.
    pub fn simplify(self) -> Self {
        match self {
            Self::And(items) => {
                let mut flat = Vec::new();
                for item in items.into_iter().map(Self::simplify) {
                    match item {
                        Self::And(inner) => flat.extend(inner),
                        other => flat.push(other),
                    }
                }
                if flat.len() == 1 {
                    flat.pop().unwrap()
                } else {
                    Self::And(flat)
                }
            }
            Self::Or(items) => {
                let mut flat = Vec::new();
                for item in items.into_iter().map(Self::simplify) {
                    match item {
                        Self::Or(inner) => flat.extend(inner),
                        other => flat.push(other),
                    }
                }
                if flat.len() == 1 {
                    flat.pop().unwrap()
                } else {
                    Self::Or(flat)
                }
            }
            Self::Not(inner) => match inner.simplify() {
                Self::Not(inner) => *inner,
                other => Self::Not(Box::new(other)),
            },
            leaf => leaf,
        }
    }

    /// Negation normal form: `Not` only wraps leaves (De Morgan), for writers without a general
    /// NOT (LogQL label filters, PromQL, SQL with NULLs).
    pub fn nnf(self) -> Self {
        fn push(expr: Expr, negated: bool) -> Expr {
            match (expr, negated) {
                (Expr::And(items), false) => {
                    Expr::And(items.into_iter().map(|item| push(item, false)).collect())
                }
                (Expr::Or(items), false) => {
                    Expr::Or(items.into_iter().map(|item| push(item, false)).collect())
                }
                (Expr::And(items), true) => {
                    Expr::Or(items.into_iter().map(|item| push(item, true)).collect())
                }
                (Expr::Or(items), true) => {
                    Expr::And(items.into_iter().map(|item| push(item, true)).collect())
                }
                (Expr::Not(inner), negated) => push(*inner, !negated),
                (leaf @ Expr::Leaf(_), false) => leaf,
                (leaf @ Expr::Leaf(_), true) => Expr::Not(Box::new(leaf)),
            }
        }
        push(self.simplify(), false).simplify()
    }

    /// Disjunctive normal form over `(negated, leaf)` literals; an empty outer list matches
    /// nothing and an empty conjunction matches everything.
    pub fn dnf(&self) -> Result<Vec<Vec<(bool, Leaf)>>, TranslationError> {
        fn expand(expr: &Expr) -> Result<Vec<Vec<(bool, Leaf)>>, TranslationError> {
            Ok(match expr {
                Expr::Leaf(leaf) => vec![vec![(false, leaf.clone())]],
                Expr::Not(inner) => match &**inner {
                    Expr::Leaf(leaf) => vec![vec![(true, leaf.clone())]],
                    _ => unreachable!("dnf runs on negation normal form"),
                },
                Expr::Or(items) => {
                    let mut terms = Vec::new();
                    for item in items {
                        terms.extend(expand(item)?);
                        if terms.len() > MAX_CONJUNCTIONS {
                            return Err(TOO_COMPLEX);
                        }
                    }
                    terms
                }
                Expr::And(items) => {
                    let mut terms = vec![Vec::new()];
                    for item in items {
                        let alternatives = expand(item)?;
                        let mut next = Vec::new();
                        for term in &terms {
                            for alternative in &alternatives {
                                let mut combined = term.clone();
                                combined.extend(alternative.iter().cloned());
                                next.push(combined);
                                if next.len() > MAX_CONJUNCTIONS {
                                    return Err(TOO_COMPLEX);
                                }
                            }
                        }
                        terms = next;
                    }
                    terms
                }
            })
        }
        const TOO_COMPLEX: TranslationError = TranslationError(
            "the boolean logic expands into too many alternatives for this language",
        );
        expand(&self.clone().nnf())
    }

    /// Merges positive equalities on the same field inside each OR into one `In`, which every
    /// language writes as a single value list or regex alternation.
    pub fn merge_equalities(self) -> Self {
        match self {
            Self::Or(items) => {
                let mut merged: Vec<Expr> = Vec::new();
                for item in items.into_iter().map(Self::merge_equalities) {
                    let values = match &item {
                        Self::Leaf(Leaf {
                            op: Op::Eq(value), ..
                        }) => Some(vec![value.clone()]),
                        Self::Leaf(Leaf {
                            op: Op::In(values), ..
                        }) => Some(values.clone()),
                        _ => None,
                    };
                    let target = match (&item, &values) {
                        (Self::Leaf(leaf), Some(_)) => merged.iter_mut().find(|existing| {
                            matches!(existing, Self::Leaf(other)
                                if other.field == leaf.field
                                    && other.case_insensitive == leaf.case_insensitive
                                    && matches!(other.op, Op::Eq(_) | Op::In(_)))
                        }),
                        _ => None,
                    };
                    match (target, values) {
                        (Some(Self::Leaf(existing)), Some(values)) => {
                            let mut all = match &existing.op {
                                Op::Eq(value) => vec![value.clone()],
                                Op::In(values) => values.clone(),
                                _ => unreachable!(),
                            };
                            for value in values {
                                if !all.contains(&value) {
                                    all.push(value);
                                }
                            }
                            existing.op = Op::In(all);
                        }
                        _ => merged.push(item),
                    }
                }
                Self::Or(merged).simplify()
            }
            Self::And(items) => Self::And(items.into_iter().map(Self::merge_equalities).collect()),
            Self::Not(inner) => Self::Not(Box::new(inner.merge_equalities())),
            leaf => leaf,
        }
    }

    /// The top-level AND items.
    pub fn conjuncts(self) -> Vec<Expr> {
        match self.simplify() {
            Self::And(items) => items,
            other => vec![other],
        }
    }
}

impl FilterValue {
    /// A finite number, from a number or numeric text.
    pub fn as_number(&self) -> Option<f64> {
        match self {
            Self::Number(number) => number.as_f64(),
            Self::String(text) if super::is_number(text.trim()) => text.trim().parse().ok(),
            _ => None,
        }
        .filter(|value: &f64| value.is_finite())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn eq(field: &str, value: &str) -> Expr {
        Expr::Leaf(Leaf::new(
            field,
            Op::Eq(FilterValue::String(value.to_owned())),
        ))
    }

    #[test]
    fn negation_moves_to_leaves_and_expands_to_alternatives() {
        let expr = Expr::Not(Box::new(Expr::Or(vec![
            eq("a", "1"),
            Expr::And(vec![eq("b", "2"), Expr::Not(Box::new(eq("c", "3")))]),
        ])));
        assert_eq!(
            expr.clone().nnf(),
            Expr::And(vec![
                Expr::Not(Box::new(eq("a", "1"))),
                Expr::Or(vec![Expr::Not(Box::new(eq("b", "2"))), eq("c", "3")]),
            ])
        );
        let dnf = expr.dnf().unwrap();
        assert_eq!(dnf.len(), 2);
        assert!(dnf.iter().all(|term| term.len() == 2 && term[0].0));
    }

    #[test]
    fn expansion_is_bounded() {
        let wide = Expr::And(
            (0..6)
                .map(|index| Expr::Or(vec![eq("a", &index.to_string()), eq("b", "x")]))
                .collect(),
        );
        assert!(wide.dnf().is_err());
        assert_eq!(Expr::all().dnf().unwrap(), vec![Vec::new()]);
    }

    #[test]
    fn equalities_on_one_field_merge_into_a_value_list() {
        let merged = Expr::Or(vec![eq("a", "1"), eq("b", "2"), eq("a", "3"), eq("a", "1")])
            .merge_equalities();
        assert_eq!(
            merged,
            Expr::Or(vec![
                Expr::Leaf(Leaf::new(
                    "a",
                    Op::In(vec![
                        FilterValue::String("1".to_owned()),
                        FilterValue::String("3".to_owned())
                    ])
                )),
                eq("b", "2"),
            ])
        );
    }

    #[test]
    fn simplification_flattens_and_unwraps() {
        let nested = Expr::And(vec![
            Expr::And(vec![eq("a", "1")]),
            Expr::Not(Box::new(Expr::Not(Box::new(eq("b", "2"))))),
        ]);
        assert_eq!(
            nested.simplify(),
            Expr::And(vec![eq("a", "1"), eq("b", "2")])
        );
    }
}
