//! Mapping field names between sources: the same attribute is `service.name` in Elasticsearch and
//! OpenTelemetry, `service_name` as a Loki or Prometheus label, and `ServiceName` or
//! `ResourceAttributes['service.name']` in ClickHouse tables from the OpenTelemetry exporter.

use std::collections::BTreeMap;

use super::Field;

/// A target Data View's fields and the types its source reports.
pub type Schema = BTreeMap<String, String>;

/// Names commonly used for the log message; `_msg` is Loki's log line.
pub const MESSAGE_FIELDS: &[&str] = &["_msg", "message", "msg", "body", "Body", "log", "line"];

/// OpenTelemetry resource-attribute namespaces; others are record attributes.
const RESOURCE_NAMESPACES: &[&str] = &[
    "service.",
    "host.",
    "k8s.",
    "cloud.",
    "container.",
    "process.",
    "os.",
    "deployment.",
    "telemetry.",
    "faas.",
    "device.",
    "browser.",
];

/// The attribute key behind map access and OpenTelemetry prefixes: `ResourceAttributes['a.b']`,
/// `resource.attributes.a.b`, and `attributes.a.b` all name `a.b`.
pub fn attribute_key(name: &str) -> &str {
    if let Some((_, rest)) = name.split_once("['") {
        if let Some(key) = rest.strip_suffix("']") {
            return key;
        }
    }
    for prefix in [
        "resource.attributes.",
        "attributes.",
        "resource.",
        "labels.",
    ] {
        if let Some(key) = name.strip_prefix(prefix).filter(|key| !key.is_empty()) {
            return key;
        }
    }
    name
}

pub fn is_message_field(name: &str) -> bool {
    MESSAGE_FIELDS.contains(&name)
}

pub fn is_resource_attribute(key: &str) -> bool {
    RESOURCE_NAMESPACES
        .iter()
        .any(|namespace| key.starts_with(namespace))
}

/// A label-safe spelling: dots and dashes become underscores, as Prometheus and Loki store
/// OpenTelemetry attributes.
pub fn label_name(key: &str) -> String {
    key.chars()
        .map(|character| {
            if character.is_ascii_alphanumeric() || character == '_' {
                character
            } else {
                '_'
            }
        })
        .collect()
}

/// The first candidate present in `schema`.
pub fn first_present(
    schema: &Schema,
    candidates: impl IntoIterator<Item = String>,
) -> Option<Field> {
    candidates.into_iter().find_map(|name| {
        schema
            .get(&name)
            .map(|field_type| Field::typed(name, field_type.clone()))
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn attribute_keys_and_label_names_normalize_across_sources() {
        for (name, key) in [
            ("ResourceAttributes['service.name']", "service.name"),
            ("resource.attributes.service.name", "service.name"),
            ("attributes.http.route", "http.route"),
            ("service.name", "service.name"),
            ("plain", "plain"),
        ] {
            assert_eq!(attribute_key(name), key, "{name}");
        }
        assert_eq!(label_name("k8s.pod-name"), "k8s_pod_name");
        assert!(is_resource_attribute("service.name") && !is_resource_attribute("http.route"));
    }
}
