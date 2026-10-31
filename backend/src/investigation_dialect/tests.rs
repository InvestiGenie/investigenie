//! Guarantees every dialect keeps, checked against a reference evaluator of the language-neutral
//! semantics rather than against query text, so equivalent spellings in different languages pass.

use std::collections::BTreeMap;

use serde_json::{json, Number, Value};

use super::*;
use crate::data_connectors::{investigation_dialect, investigation_dialects, registered};

type Record = BTreeMap<&'static str, Value>;

/// The canonical meaning of a filter on one record: a missing field never matches, `Not` is the
/// full complement, regexes match the whole value, and free text searches the message.
fn matches(expr: &Expr, record: &Record) -> bool {
    match expr {
        Expr::And(items) => items.iter().all(|item| matches(item, record)),
        Expr::Or(items) => items.iter().any(|item| matches(item, record)),
        Expr::Not(inner) => !matches(inner, record),
        Expr::Leaf(leaf) => leaf_matches(leaf, record),
    }
}

/// Field names as the evaluator's records spell them, whichever source wrote the query.
fn record_field(name: &str) -> String {
    if is_message_field(name) || name == "ig_line" {
        return "message".to_owned();
    }
    label_name(attribute_key(name))
}

fn leaf_matches(leaf: &Leaf, record: &Record) -> bool {
    let name = leaf
        .field
        .as_ref()
        .map_or_else(|| "message".to_owned(), |field| record_field(&field.name));
    let Some(value) = record
        .iter()
        .find(|(key, _)| **key == name)
        .map(|(_, value)| value)
    else {
        return false;
    };
    let text = match value {
        Value::String(text) => text.clone(),
        other => other.to_string(),
    };
    let fold = |value: &str| {
        if leaf.case_insensitive {
            value.to_lowercase()
        } else {
            value.to_owned()
        }
    };
    let equal = |expected: &FilterValue| match (
        FilterValue::String(text.clone()).as_number(),
        expected.as_number(),
    ) {
        (Some(actual), Some(expected)) if value.is_number() => actual == expected,
        _ => fold(&text) == fold(&expected.text()),
    };
    match &leaf.op {
        Op::Eq(expected) => equal(expected),
        Op::In(values) => values.iter().any(equal),
        Op::Contains(part) => fold(&text).contains(&fold(part)),
        Op::Prefix(part) => fold(&text).starts_with(&fold(part)),
        Op::Suffix(part) => fold(&text).ends_with(&fold(part)),
        Op::Regex(regex) => {
            let pattern = regex
                .clone()
                .with_case_insensitive(leaf.case_insensitive || regex.case_insensitive)
                .to_anchored_re2();
            ::regex::Regex::new(&pattern).unwrap().is_match(&text)
        }
        Op::Words(words) => ::regex::Regex::new(&words_regex(words).unwrap())
            .unwrap()
            .is_match(&text),
        Op::Exists => true,
        Op::Compare(cmp, expected) => match (value.as_f64(), expected.as_number()) {
            (Some(actual), Some(expected)) => match cmp {
                Cmp::Gt => actual > expected,
                Cmp::Gte => actual >= expected,
                Cmp::Lt => actual < expected,
                Cmp::Lte => actual <= expected,
            },
            _ => false,
        },
    }
}

fn record(fields: &[(&'static str, Value)]) -> Record {
    fields.iter().cloned().collect()
}

fn records() -> Vec<Record> {
    let s = |text: &str| Value::String(text.to_owned());
    let n = |number: i64| Value::Number(number.into());
    vec![
        record(&[
            ("service", s("check out")),
            ("status", n(503)),
            ("path", s("/api/v1/pay")),
            ("env", s("prod")),
            ("message", s("upstream timeout after 5s")),
            ("value", n(503)),
        ]),
        record(&[
            ("service", s("check out")),
            ("status", n(200)),
            ("path", s("/api/v1/cart")),
            ("env", s("dev")),
            ("message", s("ok")),
            ("value", n(200)),
        ]),
        record(&[
            ("service", s("search")),
            ("status", n(504)),
            ("path", s("/find")),
            ("message", s("Upstream  TIMEOUT")),
            ("value", n(504)),
        ]),
        record(&[
            ("service", s("CHECKOUT")),
            ("status", n(500)),
            ("path", s("/api v1/it's \"x\"")),
            ("env", s("prod")),
            ("message", s("boom \\ 50%_off")),
            ("value", n(500)),
        ]),
        record(&[
            ("service", s("checkout")),
            ("status", n(301)),
            ("env", s("staging")),
            ("message", s("timeouts are fine")),
            ("value", n(301)),
        ]),
        record(&[
            ("service", s("web")),
            ("path", s("/pay")),
            ("message", s("line\nbreak timeout")),
            ("value", n(-2)),
        ]),
    ]
}

/// Records whose `field` holds each value, plus one without the field when the source can
/// store such a record.
fn value_records(field: &'static str, values: &[FilterValue], missing: bool) -> Vec<Record> {
    let mut records = values
        .iter()
        .map(|value| {
            let json = match value {
                FilterValue::String(text) => Value::String(text.clone()),
                FilterValue::Number(number) => Value::Number(number.clone()),
                FilterValue::Bool(flag) => Value::String(flag.to_string()),
            };
            record(&[(field, json)])
        })
        .collect::<Vec<_>>();
    if missing {
        records.push(Record::new());
    }
    records
}

fn equivalent(left: &Expr, right: &Expr, records: &[Record]) -> bool {
    records
        .iter()
        .all(|record| matches(left, record) == matches(right, record))
}

struct Target {
    id: &'static str,
    schema: Schema,
}

fn schema(fields: &[(&str, &str)]) -> Schema {
    fields
        .iter()
        .map(|(name, field_type)| ((*name).to_owned(), (*field_type).to_owned()))
        .collect()
}

fn targets() -> Vec<Target> {
    vec![
        Target {
            id: "elasticsearch",
            schema: schema(&[
                ("service", "keyword"),
                ("status", "long"),
                ("path", "keyword"),
                ("env", "keyword"),
                ("message", "text"),
                ("_value", "double"),
            ]),
        },
        Target {
            id: "clickhouse",
            schema: schema(&[
                ("service", "String"),
                ("status", "UInt16"),
                ("path", "Nullable(String)"),
                ("env", "Nullable(String)"),
                ("message", "String"),
                ("_value", "Float64"),
                ("timestamp", "DateTime64(3)"),
            ]),
        },
        Target {
            id: "loki",
            schema: schema(&[
                ("service", "label"),
                ("status", "json int"),
                ("path", "json string"),
                ("env", "label"),
                ("_msg", "text"),
                ("_value", "json float"),
            ]),
        },
        Target {
            id: "prometheus",
            schema: schema(&[
                ("service", "label"),
                ("path", "label"),
                ("env", "label"),
                ("_value", "sample value"),
            ]),
        },
        Target {
            id: "mimir",
            schema: schema(&[
                ("service", "label"),
                ("path", "label"),
                ("env", "label"),
                ("_value", "sample value"),
            ]),
        },
    ]
}

fn context<'a>(schema: &'a Schema) -> WriteContext<'a> {
    WriteContext {
        index: "events",
        time_field: Some("timestamp"),
        time_range: Some(TimeRange {
            start: 0,
            end: 3_600_000,
        }),
        schema,
    }
}

fn read_filter(dialect: &dyn InvestigationDialect, query: &str) -> Expr {
    match dialect.read(query) {
        Ok(Query::Filter(expr)) => expr,
        other => panic!("{} cannot read {query}: {other:?}", dialect.language()),
    }
}

fn text(value: &str) -> FilterValue {
    FilterValue::String(value.to_owned())
}

fn leaf(field: &str, op: Op) -> Expr {
    Expr::Leaf(Leaf::new(field, op))
}

/// Filters covering every construct of the language-neutral model.
fn filters() -> Vec<Expr> {
    vec![
        Expr::And(vec![
            leaf("service", Op::Eq(text("check out"))),
            Expr::Or(vec![
                leaf(
                    "status",
                    Op::Compare(Cmp::Gte, FilterValue::Number(500.into())),
                ),
                leaf("path", Op::Prefix("/api".to_owned())),
            ]),
            leaf("env", Op::Eq(text("dev"))).negate(),
        ]),
        Expr::Or(vec![
            leaf("service", Op::In(vec![text("search"), text("web")])),
            leaf("env", Op::Exists).negate(),
        ]),
        leaf(
            "service",
            Op::Regex(PortableRegex::from_re2("che.*|w[a-z]b").unwrap()),
        )
        .negate(),
        Expr::And(vec![
            leaf("path", Op::Contains("v1".to_owned())),
            leaf("env", Op::Exists),
        ]),
        Expr::Leaf(Leaf::text("upstream timeout")),
        Expr::Or(vec![
            Expr::Leaf(Leaf::new("service", Op::Eq(text("CHECKOUT"))).insensitive()),
            leaf("path", Op::Suffix("pay".to_owned())),
        ]),
        leaf(
            "status",
            Op::Compare(Cmp::Lt, FilterValue::Number(300.into())),
        )
        .negate(),
        leaf("message", Op::Contains("50%_off".to_owned())),
        Expr::And(vec![
            leaf("service", Op::Prefix("check".to_owned())),
            leaf(
                "_value",
                Op::Compare(Cmp::Gt, FilterValue::Number(300.into())),
            ),
        ]),
        Expr::all(),
    ]
}

/// Source-independent guarantees every registered dialect must keep.
#[test]
fn every_registered_dialect_meets_the_shared_contract() {
    const FIELD_TYPES: [&str; 13] = [
        "keyword",
        "long",
        "text",
        "String",
        "UInt16",
        "Nullable(String)",
        "label",
        "timestamp",
        "sample value",
        "json string",
        "json int",
        "logfmt float",
        "",
    ];
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
        assert!(build_query(dialect, "events", &[]).is_ok(), "{id}");
        for field_type in FIELD_TYPES {
            let operators = dialect.field_operators(field_type);
            assert!(
                operators
                    .iter()
                    .all(|operator| dialect.operators().contains(operator)),
                "{id} {field_type}"
            );
            for field in ["", "a b", "a`b", "a'b", "a\"b", "a:b", "a\nb", "a{b}"] {
                for &operator in operators {
                    let filter = FilterPredicate {
                        field: field.to_owned(),
                        field_type: field_type.to_owned(),
                        operator,
                        value: FilterValue::Bool(true),
                    };
                    assert!(
                        build_query(dialect, "events", &[filter]).is_err(),
                        "{id} {field:?}"
                    );
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
                    "a\nb",
                    "500",
                    "1.5s",
                ]
                .map(text)
                .into_iter()
                .chain([FilterValue::Number(500.into()), FilterValue::Bool(true)]);
                for value in values {
                    let filter = FilterPredicate {
                        field: "status".to_owned(),
                        field_type: field_type.to_owned(),
                        operator,
                        value,
                    };
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
    assert!(dialects >= 5, "{dialects}");
}

/// Every dialect reads back what it writes, for each type and condition it accepts, with values
/// chosen to break quoting and escaping; the read-back must match exactly the same records.
#[test]
fn every_dialect_reads_back_what_it_writes() {
    let values = [
        text("checkout"),
        text("say \"hi\" \\ now"),
        text("a b\tc"),
        text("50%_off"),
        text("web.v2(x)|y*?"),
        text("it's; DROP"),
        text("ends with \\"),
        text("{}[]:/"),
        text("AND"),
        text("OR NOT"),
        text("ünïcødé ✓"),
        text("line\nbreak"),
        FilterValue::Number(500.into()),
        FilterValue::Number(Number::from_f64(-2.5).unwrap()),
    ];
    let mut round_trips = 0;
    for target in targets() {
        let dialect = investigation_dialect(target.id).unwrap();
        for (field, field_type) in &target.schema {
            let ops = |value: &FilterValue| {
                let text = value.text();
                vec![
                    Op::Eq(value.clone()),
                    Op::In(vec![value.clone(), FilterValue::String("other".to_owned())]),
                    Op::Contains(text.clone()),
                    Op::Prefix(text.clone()),
                    Op::Suffix(text.clone()),
                    Op::Regex(PortableRegex::literal(&text)),
                    Op::Words(text.clone()),
                    Op::Exists,
                    Op::Compare(Cmp::Gte, value.clone()),
                    Op::Compare(Cmp::Lt, value.clone()),
                ]
            };
            for value in &values {
                for op in ops(value) {
                    for case_insensitive in [false, true] {
                        let leaf = Leaf {
                            field: Some(Field::typed(field.clone(), field_type.clone())),
                            op: op.clone(),
                            case_insensitive,
                        };
                        if dialect.check_leaf(&leaf).is_err() {
                            continue;
                        }
                        for expr in [Expr::Leaf(leaf.clone()), Expr::Leaf(leaf.clone()).negate()] {
                            let query = Query::Filter(expr.clone());
                            let Ok(written) = dialect.write(&query, &context(&target.schema))
                            else {
                                continue;
                            };
                            let read = read_filter(dialect, &written);
                            let field = record_field(field);
                            // Columns that are not Nullable, Loki's log line, and sample values are
                            // never missing.
                            let missing = match target.id {
                                "clickhouse" => field_type.starts_with("Nullable("),
                                "loki" => field != "message",
                                _ => field != "_value",
                            };
                            // Prometheus sample values are always numbers.
                            let possible = values
                                .iter()
                                .filter(|value| {
                                    missing
                                        || field != "_value"
                                        || matches!(value, FilterValue::Number(_))
                                })
                                .cloned()
                                .collect::<Vec<_>>();
                            let records = value_records(
                                Box::leak(field.into_boxed_str()),
                                &possible,
                                missing,
                            );
                            assert!(
                                equivalent(&expr, &read, &records),
                                "{}: {expr:?}\n{written}\n{read:?}",
                                target.id
                            );
                            round_trips += 1;
                        }
                    }
                }
            }
        }
    }
    assert!(round_trips > 1_000, "{round_trips}");
}

/// The same filter written in each language translates into every other language, and the
/// translation matches exactly the records the original does, or is refused with a reason when
/// the target cannot express it.
#[test]
fn filters_translate_between_every_pair_of_languages() {
    let records = records();
    let targets = targets();
    let mut translations = 0;
    for filter in filters() {
        for source in &targets {
            let source_dialect = investigation_dialect(source.id).unwrap();
            let Ok(written) = translate_parsed(
                Query::Filter(filter.clone()),
                source_dialect,
                &context(&source.schema),
            ) else {
                continue;
            };
            assert!(
                equivalent(
                    &filter,
                    &read_filter(source_dialect, &written.query),
                    &records
                ),
                "{} wrote {}",
                source.id,
                written.query
            );
            for target in &targets {
                let target_dialect = investigation_dialect(target.id).unwrap();
                let direct = translate_parsed(
                    Query::Filter(filter.clone()),
                    target_dialect,
                    &context(&target.schema),
                );
                let translated = translate_query(
                    source_dialect.language(),
                    &written.query,
                    target_dialect,
                    &context(&target.schema),
                );
                match (&direct, &translated) {
                    (Ok(_), Ok(translated)) => {
                        let read = read_filter(target_dialect, &translated.query);
                        assert!(
                            equivalent(&filter, &read, &records),
                            "{} → {}: {}\n→ {}\n{read:?}",
                            source.id,
                            target.id,
                            written.query,
                            translated.query
                        );
                        translations += 1;
                    }
                    (Err(_), Err(_)) => {}
                    _ => panic!(
                        "{} → {}: {filter:?}\n{}\ndirect {direct:?}\ntranslated {translated:?}",
                        source.id, target.id, written.query
                    ),
                }
            }
        }
    }
    assert!(translations > 60, "{translations}");
}

#[test]
fn computations_translate_between_metric_languages() {
    let metric = |function| MetricQuery {
        function,
        range_ms: 300_000,
        group_by: vec![Field::named("service")],
        filter: Expr::And(vec![
            leaf("env", Op::Eq(text("prod"))),
            leaf("path", Op::Prefix("/api".to_owned())),
        ]),
    };
    let mut translations = 0;
    for function in [
        RangeFunction::Count,
        RangeFunction::Rate,
        RangeFunction::Sum(Field::named("_value")),
        RangeFunction::Avg(Field::named("_value")),
        RangeFunction::Max(Field::named("_value")),
    ] {
        let canonical = metric(function);
        for source in targets() {
            let source_dialect = investigation_dialect(source.id).unwrap();
            let Ok(written) = translate_parsed(
                Query::Metric(canonical.clone()),
                source_dialect,
                &context(&source.schema),
            ) else {
                assert_eq!(source.id, "elasticsearch", "{canonical:?}");
                continue;
            };
            assert_eq!(written.range_ms, Some(300_000));
            for target in targets()
                .into_iter()
                .filter(|target| target.id != "elasticsearch")
            {
                let target_dialect = investigation_dialect(target.id).unwrap();
                let translated = translate_query(
                    source_dialect.language(),
                    &written.query,
                    target_dialect,
                    &context(&target.schema),
                )
                .unwrap_or_else(|error| {
                    panic!("{} → {}: {}: {error}", source.id, target.id, written.query)
                });
                let Ok(Query::Metric(read)) = target_dialect.read(&translated.query) else {
                    panic!("{} cannot read {}", target.id, translated.query);
                };
                assert_eq!(
                    std::mem::discriminant(&read.function),
                    std::mem::discriminant(&canonical.function),
                    "{}",
                    translated.query
                );
                assert_eq!(read.range_ms, canonical.range_ms);
                assert_eq!(
                    read.group_by
                        .iter()
                        .map(|field| record_field(&field.name))
                        .collect::<Vec<_>>(),
                    ["service"]
                );
                assert!(
                    equivalent(&read.filter, &canonical.filter, &records()),
                    "{}",
                    translated.query
                );
                translations += 1;
            }
        }
    }
    assert!(translations >= 40, "{translations}");
    let elasticsearch = investigation_dialect("elasticsearch").unwrap();
    let schema = targets().remove(0).schema;
    assert!(translate_parsed(
        Query::Metric(metric(RangeFunction::Count)),
        elasticsearch,
        &context(&schema)
    )
    .is_err());
}

#[test]
fn untranslatable_queries_explain_why() {
    let schema = schema(&[("status", "label"), ("_msg", "text")]);
    let loki = investigation_dialect("loki").unwrap();
    let error = |language: &str, query: &str| {
        translate_query(language, query, loki, &context(&schema))
            .unwrap_err()
            .to_string()
    };
    assert_eq!(
        error("Cobol", "x"),
        "This query language cannot be translated"
    );
    assert_eq!(
        error("sql", "service = 'api'"),
        "Field \"service\" is not in this Data View"
    );
    assert_eq!(
        error("SQL", "message > 5"),
        "Field \"_msg\": the log line cannot be compared as a number"
    );
    assert!(error("PromQL", "rate(up[5m])").starts_with("PromQL query: "));
    assert!(dialect_for_language(" lucene ").is_some());
}

#[test]
fn every_dialect_names_its_descriptor_language_and_lists_the_others() {
    let mut languages = investigation_dialects()
        .into_iter()
        .map(|dialect| dialect.language())
        .collect::<Vec<_>>();
    languages.sort_unstable();
    languages.dedup();
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
        let listed = descriptor
            .query_languages
            .iter()
            .map(|language| language["language"].as_str().unwrap())
            .collect::<Vec<_>>();
        assert_eq!(listed.len(), languages.len(), "{listed:?}");
        for language in listed {
            let dialect = dialect_for_language(language).unwrap();
            assert!(
                dialect.read(dialect.example()).is_ok(),
                "{language}: {}",
                dialect.example()
            );
        }
    }
}

#[test]
fn operator_labels_and_values_are_tolerant_but_scalar() {
    for operator in FilterOperator::ALL {
        assert_eq!(FilterOperator::from_label(operator.label()), Some(operator));
    }
    assert_eq!(
        FilterOperator::from_label(" >= "),
        Some(FilterOperator::Gte)
    );
    assert!(FilterValue::from_json(&Value::Null).is_none());
    assert!(FilterValue::from_json(&serde_json::json!([1])).is_none());
}

// ---------------------------------------------------------------------------------------------
// Live engine matrix

/// One record of the live dataset. Prometheus holds it as the series
/// `ig_matrix{rid, service, env, path}` with `status` as its value.
struct Row {
    rid: u64,
    service: &'static str,
    env: Option<&'static str>,
    path: Option<&'static str>,
    status: i64,
    latency: f64,
    message: &'static str,
}

const ROWS: [Row; 6] = [
    Row {
        rid: 1,
        service: "check out",
        env: Some("prod"),
        path: Some("/api/v1/pay"),
        status: 503,
        latency: 2.5,
        message: "upstream timeout after 5s",
    },
    Row {
        rid: 2,
        service: "check out",
        env: Some("dev"),
        path: Some("/api/v1/cart"),
        status: 200,
        latency: 0.1,
        message: "all good",
    },
    Row {
        rid: 3,
        service: "search",
        env: None,
        path: Some("/find"),
        status: 504,
        latency: 0.4,
        message: "Upstream TIMEOUT",
    },
    Row {
        rid: 4,
        service: "CHECKOUT",
        env: Some("prod"),
        path: Some("/api v1/it's \"x\""),
        status: 500,
        latency: 1.25,
        message: "boom \\ 50%_off",
    },
    Row {
        rid: 5,
        service: "checkout",
        env: Some("staging"),
        path: None,
        status: 301,
        latency: 0.2,
        message: "timeouts are fine",
    },
    Row {
        rid: 6,
        service: "web",
        env: None,
        path: Some("/pay"),
        status: 404,
        latency: 3.0,
        message: "second line timeout",
    },
];

const METRIC_NAME: &str = "ig_matrix";

/// The Prometheus exposition the engine script serves for scraping.
fn exposition() -> String {
    let escape = |value: &str| {
        value
            .replace('\\', "\\\\")
            .replace('"', "\\\"")
            .replace('\n', "\\n")
    };
    let mut text = format!("# HELP {METRIC_NAME} Translation matrix fixture; see investigation_dialect/tests.rs.\n# TYPE {METRIC_NAME} gauge\n");
    for row in &ROWS {
        let mut labels = vec![
            format!("rid=\"{}\"", row.rid),
            format!("service=\"{}\"", escape(row.service)),
        ];
        labels.extend(row.env.map(|env| format!("env=\"{}\"", escape(env))));
        labels.extend(row.path.map(|path| format!("path=\"{}\"", escape(path))));
        text.push_str(&format!(
            "{METRIC_NAME}{{{}}} {}\n",
            labels.join(","),
            row.status
        ));
    }
    text
}

#[test]
fn the_prometheus_fixture_holds_the_live_dataset() {
    assert_eq!(
        include_str!("../../../scripts/translation-engines/metrics").replace("\r\n", "\n"),
        exposition()
    );
}

/// How each engine holds the dataset, for the reference evaluator: logs engines hold every
/// field; metrics engines hold the series labels and the value.
fn engine_records(id: &str, run: &str) -> Vec<(u64, Record)> {
    let s = |text: &str| Value::String(text.to_owned());
    ROWS.iter()
        .map(|row| {
            let mut fields = vec![("service", s(row.service))];
            fields.extend(row.env.map(|env| ("env", s(env))));
            fields.extend(row.path.map(|path| ("path", s(path))));
            if matches!(id, "prometheus" | "mimir") {
                fields.push(("__name__", s(METRIC_NAME)));
                fields.push(("_value", Value::Number(row.status.into())));
            } else {
                fields.push(("run", s(run)));
                fields.push(("status", Value::Number(row.status.into())));
                fields.push((
                    "latency",
                    Value::Number(Number::from_f64(row.latency).unwrap()),
                ));
                fields.push(("message", s(row.message)));
            }
            (row.rid, record(&fields))
        })
        .collect()
}

/// Each engine's records for this run, so runs never see each other's data.
fn scope(id: &str, run: &str) -> Expr {
    match id {
        "prometheus" | "mimir" => leaf("__name__", Op::Eq(text(METRIC_NAME))),
        _ => leaf("run", Op::Eq(text(run))),
    }
}

struct Engine {
    id: &'static str,
    connector: std::sync::Arc<dyn crate::data_connectors::Connector>,
    index: String,
    time_field: &'static str,
    schema: Schema,
}

async fn seed_engines(run: &str, now: i64) -> Vec<Engine> {
    use crate::data_connectors::connect;
    let client = reqwest::Client::new();
    let connector =
        |id: &str, url: &str| connect(id, json!({"url": url}).as_object().unwrap()).unwrap();
    let mut engines = Vec::new();
    if let Ok(url) = std::env::var("TEST_ELASTICSEARCH_URL") {
        let index = format!("ig-matrix-{run}");
        client
            .put(format!("{url}/{index}"))
            .json(&json!({"mappings": {"properties": {
                "run": {"type": "keyword"}, "rid": {"type": "long"}, "service": {"type": "keyword"},
                "env": {"type": "keyword"}, "path": {"type": "keyword"}, "status": {"type": "long"},
                "latency": {"type": "double"}, "@timestamp": {"type": "date"},
                "message": {"type": "text", "fields": {"keyword": {"type": "keyword"}}}}}}))
            .send()
            .await
            .unwrap()
            .error_for_status()
            .unwrap();
        let mut bulk = String::new();
        for row in &ROWS {
            let mut document = json!({"run": run, "rid": row.rid, "service": row.service, "status": row.status,
                "latency": row.latency, "message": row.message, "@timestamp": now});
            if let Some(env) = row.env {
                document["env"] = json!(env);
            }
            if let Some(path) = row.path {
                document["path"] = json!(path);
            }
            bulk.push_str(&format!("{}\n{document}\n", json!({"index": {}})));
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
        engines.push(Engine {
            id: "elasticsearch",
            connector: connector("elasticsearch", &url),
            index,
            time_field: "@timestamp",
            schema: Schema::new(),
        });
    }
    if let Ok(url) = std::env::var("TEST_CLICKHOUSE_URL") {
        let table = format!("ig_matrix_{run}");
        let time = chrono::DateTime::from_timestamp_millis(now)
            .unwrap()
            .format("%Y-%m-%d %H:%M:%S%.3f")
            .to_string();
        let rows = ROWS
            .iter()
            .map(|row| json!({"run": run, "rid": row.rid, "service": row.service, "env": row.env, "path": row.path,
                "status": row.status, "latency": row.latency, "message": row.message, "ts": time}).to_string())
            .collect::<Vec<_>>()
            .join("\n");
        for statement in [
            format!("CREATE TABLE {table} (run String, rid UInt8, service String, env Nullable(String), path Nullable(String), status UInt16, latency Float64, message String, ts DateTime64(3, 'UTC')) ENGINE = MergeTree ORDER BY rid"),
            format!("INSERT INTO {table} FORMAT JSONEachRow\n{rows}"),
        ] {
            client.post(&url).body(statement).send().await.unwrap().error_for_status().unwrap();
        }
        engines.push(Engine {
            id: "clickhouse",
            connector: connector("clickhouse", &url),
            index: table,
            time_field: "ts",
            schema: Schema::new(),
        });
    }
    if let Ok(url) = std::env::var("TEST_LOKI_URL") {
        let streams = ROWS
            .iter()
            .map(|row| {
                let mut stream = json!({"run": run, "service": row.service});
                if let Some(env) = row.env {
                    stream["env"] = json!(env);
                }
                let mut line = json!({"rid": row.rid, "status": row.status, "latency": row.latency, "message": row.message});
                if let Some(path) = row.path {
                    line["path"] = json!(path);
                }
                json!({"stream": stream, "values": [[(now * 1_000_000 + row.rid as i64).to_string(), line.to_string()]]})
            })
            .collect::<Vec<_>>();
        client
            .post(format!("{url}/loki/api/v1/push"))
            .json(&json!({"streams": streams}))
            .send()
            .await
            .unwrap()
            .error_for_status()
            .unwrap();
        engines.push(Engine {
            id: "loki",
            connector: connector("loki", &url),
            index: "logs".to_owned(),
            time_field: "_time",
            schema: Schema::new(),
        });
    }
    for (id, variable) in [
        ("prometheus", "TEST_PROMETHEUS_URL"),
        ("mimir", "TEST_MIMIR_URL"),
    ] {
        if let Ok(url) = std::env::var(variable) {
            engines.push(Engine {
                id,
                connector: connector(id, &url),
                index: "metrics".to_owned(),
                time_field: "_time",
                schema: Schema::new(),
            });
        }
    }
    // Wait until each engine reports this dataset's fields (Loki detects them from recent lines;
    // Prometheus and Mimir from scraped series).
    for engine in &mut engines {
        let wanted: &[&str] = match engine.id {
            "prometheus" | "mimir" => &["service", "env", "path", "rid", "_value"],
            _ => &["service", "env", "path", "status", "latency", "message"],
        };
        for _ in 0..120 {
            engine.schema = engine
                .connector
                .fields(&engine.index)
                .await
                .unwrap_or_else(|error| panic!("{}: {}", engine.id, error.message))
                .into_iter()
                .map(|field| (field.name, field.field_type))
                .collect();
            if wanted
                .iter()
                .all(|field| engine.schema.contains_key(*field))
            {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(500)).await;
        }
        assert!(
            wanted
                .iter()
                .all(|field| engine.schema.contains_key(*field)),
            "{}: {:?}",
            engine.id,
            engine.schema
        );
    }
    engines
}

async fn search(engine: &Engine, query: &str, range: TimeRange, interval: Option<String>) -> Value {
    let request: crate::search::SearchRequest = serde_json::from_value(json!({
        "index": engine.index, "query": query, "size": 100, "include_histogram": false,
        "histogram_field": engine.time_field, "histogram_interval": interval,
        "time_range": {"start": range.start, "end": range.end}
    }))
    .unwrap();
    engine
        .connector
        .search(request)
        .await
        .unwrap_or_else(|error| panic!("{}: {} for {query}", engine.id, error.message))
}

fn hit_rids(result: &Value) -> std::collections::BTreeSet<u64> {
    result["hits"]
        .as_array()
        .unwrap()
        .iter()
        .map(|hit| {
            // Loki rows carry the raw JSON line unless the query parses it.
            let line = hit["source"]["_msg"]
                .as_str()
                .and_then(|line| serde_json::from_str::<Value>(line).ok());
            let rid = hit["source"]
                .get("rid")
                .or_else(|| line.as_ref()?.get("rid"))
                .unwrap_or(&Value::Null);
            rid.as_u64()
                .or_else(|| rid.as_str()?.parse().ok())
                .unwrap_or_else(|| panic!("no rid in {hit}"))
        })
        .collect()
}

/// Filters in every language, run against every engine. Each engine must return exactly the
/// records the reference evaluator selects from its copy of the dataset, or refuse the query
/// with a reason when it cannot express it.
const LIVE_FILTERS: &[(&str, &str)] = &[
    (
        "Lucene",
        r#"service:"check out" AND (status:[500 TO *] OR path:\/api*) AND NOT env:dev"#,
    ),
    ("Lucene", "service:(search OR web) OR NOT _exists_:env"),
    ("Lucene", "NOT service:/che.*|w[a-z]b/"),
    ("Lucene", r#""upstream timeout""#),
    ("Lucene", "status:{300 TO 504] AND NOT latency:[* TO 0.3]"),
    (
        "Lucene",
        r#"message:"upstream timeout" OR path:\/api\ v1\/it\'s\ \"x\""#,
    ),
    (
        "SQL",
        "service = 'check out' AND (status >= 500 OR path LIKE '/api%') AND NOT env = 'dev'",
    ),
    ("SQL", r#"path LIKE '%it\'s "x"%' OR env IS NULL"#),
    ("SQL", "service IN ('search', 'web') AND latency > 0.3"),
    (
        "SQL",
        "lower(service) = 'checkout' OR message ILIKE '%TIMEOUT%'",
    ),
    ("SQL", "status BETWEEN 300 AND 504 AND path IS NOT NULL"),
    (
        "LogQL",
        r#"{service=~"check.*"} | json | status >= 500 or latency < 0.3"#,
    ),
    ("LogQL", r#"{env!="dev"} |= "timeout""#),
    (
        "LogQL",
        r#"{service=~"(?i)checkout"} | json | path=~"/api.*""#,
    ),
    ("LogQL", r#"{} |~ "(?i)\\bupstream\\W+timeout\\b" | env="""#),
    ("PromQL", r#"{service=~"(?i)checkout|search", env!="prod"}"#),
    ("PromQL", r#"{path=~"/api.*"} > 400"#),
    ("PromQL", r#"{service="web"} or {env="staging"}"#),
    ("PromQL", r#"{env=~"prod|dev", path!~".*cart"}"#),
];

/// Computations read from LogQL and PromQL, run against every engine that computes.
const LIVE_COMPUTATIONS: &[(&str, &str)] = &[
    ("LogQL", r#"sum by (service) (count_over_time({env="prod"} [5m]))"#),
    ("PromQL", r#"sum by (service) (count_over_time({env=~"prod|dev"}[5m])) / 300"#),
    ("LogQL", "sum by (service) (sum_over_time({} | json | unwrap latency [5m]))"),
    ("LogQL", "max by (service) (max_over_time({} | json | unwrap status [5m]))"),
    ("LogQL", "sum by (service) (sum_over_time({} | json | unwrap latency [5m])) / sum by (service) (count_over_time({} | json [5m]))"),
];

#[tokio::test]
#[ignore = "requires TEST_ELASTICSEARCH_URL, TEST_CLICKHOUSE_URL, TEST_LOKI_URL, TEST_PROMETHEUS_URL, and/or TEST_MIMIR_URL; see scripts/translation-engines/start.sh"]
async fn live_translations_return_the_same_records_in_every_engine() {
    let configured = [
        "TEST_ELASTICSEARCH_URL",
        "TEST_CLICKHOUSE_URL",
        "TEST_LOKI_URL",
        "TEST_PROMETHEUS_URL",
        "TEST_MIMIR_URL",
    ]
    .iter()
    .any(|variable| std::env::var(variable).is_ok());
    if !configured {
        eprintln!("skipped: no TEST_*_URL engine is configured");
        return;
    }
    let run = format!("r{}", chrono::Utc::now().timestamp_millis());
    let now = chrono::Utc::now().timestamp_millis() - 30_000;
    let engines = seed_engines(&run, now).await;
    let logs_range = TimeRange {
        start: now - 600_000,
        end: now + 400_000,
    };
    let mut checked = BTreeMap::<&str, usize>::new();
    let mut refused = Vec::new();
    for (language, query) in LIVE_FILTERS {
        let source = dialect_for_language(language).unwrap();
        let canonical = read_filter(source, query);
        for engine in &engines {
            let target = investigation_dialect(engine.id).unwrap();
            let scoped = Query::Filter(Expr::And(vec![scope(engine.id, &run), canonical.clone()]));
            let context = WriteContext {
                index: &engine.index,
                time_field: Some(engine.time_field),
                time_range: Some(logs_range),
                schema: &engine.schema,
            };
            let translation = match translate_parsed(scoped, target, &context) {
                Ok(translation) => translation,
                Err(error) => {
                    refused.push(format!("{language} → {}: {error}", engine.id));
                    continue;
                }
            };
            let expected = engine_records(engine.id, &run)
                .into_iter()
                .filter(|(_, record)| matches(&canonical, record))
                .map(|(rid, _)| rid)
                .collect::<std::collections::BTreeSet<_>>();
            // Metrics engines sample the scraped series over the last half minute.
            let (range, interval) = match engine.id {
                "prometheus" | "mimir" => {
                    let end = chrono::Utc::now().timestamp_millis();
                    (
                        TimeRange {
                            start: end - 30_000,
                            end,
                        },
                        Some("15s".to_owned()),
                    )
                }
                _ => (logs_range, None),
            };
            let mut found = Default::default();
            for _ in 0..20 {
                found =
                    hit_rids(&search(engine, &translation.query, range, interval.clone()).await);
                if found == expected {
                    break;
                }
                tokio::time::sleep(std::time::Duration::from_millis(500)).await;
            }
            assert_eq!(
                found, expected,
                "{language} → {}\n{query}\n{}",
                engine.id, translation.query
            );
            *checked.entry(engine.id).or_default() += 1;
        }
    }
    for (language, query) in LIVE_COMPUTATIONS {
        let Ok(Query::Metric(canonical)) = dialect_for_language(language).unwrap().read(query)
        else {
            panic!("{language} cannot read {query}");
        };
        for engine in &engines {
            let target = investigation_dialect(engine.id).unwrap();
            let mut scoped = canonical.clone();
            scoped.filter = Expr::And(vec![scope(engine.id, &run), scoped.filter]);
            let context = WriteContext {
                index: &engine.index,
                time_field: Some(engine.time_field),
                time_range: Some(logs_range),
                schema: &engine.schema,
            };
            let translation = match translate_parsed(Query::Metric(scoped), target, &context) {
                Ok(translation) => translation,
                Err(error) => {
                    refused.push(format!("{language} → {}: {error}", engine.id));
                    continue;
                }
            };
            let range_ms = translation.range_ms.unwrap();
            let interval = Some(format_duration(range_ms));
            if matches!(engine.id, "prometheus" | "mimir") {
                // Prometheus counts raw samples, which depend on the scrape interval; check that
                // the computation runs and groups by the requested label.
                let end = chrono::Utc::now().timestamp_millis();
                let window = bucket_ends(
                    TimeRange {
                        start: end - 2 * range_ms,
                        end,
                    },
                    range_ms,
                )
                .unwrap();
                let result = search(engine, &translation.query, window, interval).await;
                let hits = result["hits"].as_array().unwrap();
                assert!(!hits.is_empty(), "{}: {}", engine.id, translation.query);
                assert!(
                    hits.iter().all(|hit| hit["source"]["service"].is_string()),
                    "{}",
                    translation.query
                );
                *checked.entry(engine.id).or_default() += 1;
                continue;
            }
            // Every record sits in one bucket, so each group's bucket holds the whole answer.
            let mut expected = BTreeMap::<String, Vec<f64>>::new();
            for (_, record) in engine_records(engine.id, &run) {
                if !matches(&canonical.filter, &record) {
                    continue;
                }
                let service = record["service"].as_str().unwrap().to_owned();
                let value = canonical
                    .function
                    .value_field()
                    .map_or(1.0, |field| record[field.name.as_str()].as_f64().unwrap());
                expected.entry(service).or_default().push(value);
            }
            let expected = expected
                .into_iter()
                .map(|(service, values)| {
                    let sum = values.iter().sum::<f64>();
                    let value = match canonical.function {
                        RangeFunction::Count | RangeFunction::Sum(_) => sum,
                        RangeFunction::Rate => sum / (range_ms / 1_000) as f64,
                        RangeFunction::Avg(_) => sum / values.len() as f64,
                        RangeFunction::Min(_) => {
                            values.iter().copied().fold(f64::INFINITY, f64::min)
                        }
                        RangeFunction::Max(_) => {
                            values.iter().copied().fold(f64::NEG_INFINITY, f64::max)
                        }
                    };
                    (service, value)
                })
                .collect::<BTreeMap<_, _>>();
            let window = bucket_ends(logs_range, range_ms).unwrap();
            let result = search(engine, &translation.query, window, interval).await;
            let mut found = BTreeMap::new();
            for hit in result["hits"].as_array().unwrap() {
                let service = hit["source"]["service"].as_str().unwrap().to_owned();
                let value = &hit["source"]["_value"];
                let value = value
                    .as_f64()
                    .or_else(|| value.as_str()?.parse().ok())
                    .unwrap();
                assert!(
                    found.insert(service, value).is_none(),
                    "{}: more than one bucket\n{}",
                    engine.id,
                    translation.query
                );
            }
            assert_eq!(
                found.keys().collect::<Vec<_>>(),
                expected.keys().collect::<Vec<_>>(),
                "{language} → {}\n{}",
                engine.id,
                translation.query
            );
            for (service, value) in &expected {
                assert!(
                    (found[service] - value).abs() < 1e-9,
                    "{language} → {}: {service} {} != {value}\n{}",
                    engine.id,
                    found[service],
                    translation.query
                );
            }
            *checked.entry(engine.id).or_default() += 1;
        }
    }
    eprintln!(
        "checked per engine: {checked:?}\nrefused (cannot be expressed exactly):\n{}",
        refused.join("\n")
    );
    for engine in &engines {
        assert!(
            checked.get(engine.id).copied().unwrap_or_default() >= 5,
            "{}: {checked:?}\n{}",
            engine.id,
            refused.join("\n")
        );
    }
}
