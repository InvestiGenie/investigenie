//! Grafana Mimir serves the Prometheus HTTP API (under `/prometheus` by default) with tenants and
//! gateway authentication, so it reuses the Prometheus client and adds only those settings.

use std::sync::Arc;

use super::prometheus::{
    connector_error, ApiFlavor, ErrorCodes, PrometheusClient, PrometheusSettings,
};
use crate::connector_auth::HttpAuth;
use crate::data_connectors::{
    string_setting, ConnectionConfig, Connector, ConnectorError, IntegrationDescriptor,
};
use serde_json::json;

pub mod investigation;

pub static MIMIR: ApiFlavor = ApiFlavor {
    name: "Mimir",
    configurable_auth: true,
    codes: ErrorCodes {
        invalid_connection: "invalid_mimir_connection",
        invalid_request: "invalid_mimir_request",
        result_limit_exceeded: "mimir_result_limit_exceeded",
        partial_response: "mimir_partial_response",
        client_error: "mimir_client_error",
        unreachable: "mimir_unreachable",
        authentication_failed: "mimir_authentication_failed",
        rejected_request: "mimir_rejected_request",
        response_error: "mimir_response_error",
    },
};

pub fn descriptor() -> IntegrationDescriptor {
    IntegrationDescriptor {
        id: "mimir",
        display_name: "Grafana Mimir",
        icon: "mimir",
        query_language: "PromQL",
        description: "Read-only PromQL range queries over Mimir tenants",
        connection_fields: [json!({"name":"url","label":"Prometheus API URL","input_type":"url","required":true,"secret":false,"editable":true,"placeholder":"http://localhost:9009/prometheus"})]
            .into_iter()
            .chain(crate::connector_auth::connection_fields())
            .chain([json!({"name":"tenant_id","label":"Tenant ID (optional)","input_type":"text","required":false,"secret":false,"editable":true,"placeholder":"X-Scope-OrgID"})])
            .collect(),
        query: json!({"placeholder":"Enter a PromQL expression, e.g. rate(http_requests_total[5m])","default_query":"{__name__!=\"\"}","field_query":"{query}","identifier_quote":"","help":"PromQL range queries against Mimir return evaluated samples at a bounded step; missing samples may be filled by lookback semantics","document_mode":"inline","sort_requires_projection":false,"histogram_total_from_buckets":true,"schema_label":"Mimir labels"}),
        ..Default::default()
    }
}

pub fn connect(config: &ConnectionConfig) -> Result<Arc<dyn Connector>, ConnectorError> {
    let raw_url = string_setting(config, "url")?.ok_or_else(ConnectorError::invalid_connection)?;
    let url = url::Url::parse(&raw_url).map_err(|_| ConnectorError::invalid_connection())?;
    let client = PrometheusClient::new(PrometheusSettings {
        url,
        auth: HttpAuth::from_config(config)?,
        tenant_id: string_setting(config, "tenant_id")?,
        flavor: &MIMIR,
    })
    .map_err(|error| connector_error(error, &MIMIR))?;
    Ok(Arc::new(client))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::Value;
    use wiremock::{matchers, Mock, MockServer, ResponseTemplate};

    fn connector(server: &MockServer, extra: Value) -> Arc<dyn Connector> {
        let mut config = json!({"url":format!("{}/prometheus", server.uri())});
        config
            .as_object_mut()
            .unwrap()
            .extend(extra.as_object().unwrap().clone());
        connect(config.as_object().unwrap()).unwrap()
    }

    #[tokio::test]
    async fn requests_use_the_prometheus_prefix_tenant_and_oauth_token() {
        let server = MockServer::start().await;
        Mock::given(matchers::method("POST"))
            .and(matchers::path("/token"))
            .respond_with(ResponseTemplate::new(200).set_body_json(
                json!({"access_token":"mimir-token","token_type":"Bearer","expires_in":300}),
            ))
            .expect(1)
            .mount(&server)
            .await;
        Mock::given(matchers::method("POST"))
            .and(matchers::path("/prometheus/api/v1/query"))
            .and(matchers::header("authorization", "Bearer mimir-token"))
            .and(matchers::header("x-scope-orgid", "team-a"))
            .respond_with(ResponseTemplate::new(200).set_body_json(
                json!({"status":"success","data":{"resultType":"vector","result":[]}}),
            ))
            .expect(1)
            .mount(&server)
            .await;
        Mock::given(matchers::path("/prometheus/api/v1/labels"))
            .and(matchers::header("authorization", "Bearer mimir-token"))
            .respond_with(ResponseTemplate::new(200).set_body_json(
                json!({"status":"success","data":["__name__","job"]}),
            ))
            .expect(1)
            .mount(&server)
            .await;
        let mimir = connector(
            &server,
            json!({"auth_type":"oauth2","oauth_token_url":format!("{}/token", server.uri()),
                   "oauth_client_id":"mimir-reader","oauth_client_secret":"mimir-secret","tenant_id":"team-a"}),
        );
        mimir.test_connection().await.unwrap();
        let fields = mimir.fields("metrics").await.unwrap();
        assert!(fields
            .iter()
            .any(|field| field.name == "job" && field.field_type == "label"));
        let public = Value::Object(mimir.public_configuration()).to_string();
        assert!(public.contains("oauth2") && public.contains("team-a"));
        assert!(!public.contains("mimir-secret") && !public.contains("mimir-reader"));
    }

    #[tokio::test]
    async fn errors_name_mimir_and_rejected_tokens_are_refetched() {
        let server = MockServer::start().await;
        Mock::given(matchers::path("/token"))
            .respond_with(ResponseTemplate::new(200).set_body_json(
                json!({"access_token":"stale","token_type":"Bearer","expires_in":300}),
            ))
            .expect(2)
            .mount(&server)
            .await;
        Mock::given(matchers::path("/prometheus/api/v1/query"))
            .respond_with(ResponseTemplate::new(401))
            .mount(&server)
            .await;
        let config = json!({"auth_type":"oauth2","oauth_token_url":format!("{}/token", server.uri()),
                            "oauth_client_id":"id","oauth_client_secret":"rotated"});
        let error = connector(&server, config.clone())
            .test_connection()
            .await
            .unwrap_err();
        assert_eq!(error.code, "mimir_rejected_request");
        assert_eq!(error.message, "Mimir rejected the configured credentials");
        // The 401 dropped the cached token, so a rebuilt connector fetches a fresh one.
        connector(&server, config).test_connection().await.unwrap_err();

        let error = match connect(json!({"url":"http://localhost:9009/prometheus","auth_type":"oauth2"}).as_object().unwrap()) {
            Err(error) => error,
            Ok(_) => panic!("OAuth without a token URL must be rejected"),
        };
        assert_eq!(error.code, "invalid_connection");
    }

    #[test]
    fn descriptor_offers_shared_authentication_and_a_tenant() {
        let descriptor = descriptor();
        let names = descriptor
            .connection_fields
            .iter()
            .filter_map(|field| field["name"].as_str())
            .collect::<Vec<_>>();
        for name in ["url", "auth_type", "oauth_token_url", "oauth_client_secret", "tenant_id"] {
            assert!(names.contains(&name), "{name}");
        }
        assert_eq!(descriptor.query_language, "PromQL");
    }

    /// Translated PromQL on a real Mimir fed by Prometheus remote write.
    #[tokio::test]
    #[ignore = "requires TEST_MIMIR_URL, a Mimir Prometheus API URL with an `up` series"]
    async fn live_mimir_runs_translated_label_and_value_filters() {
        use crate::investigation_dialect::{build_query, FilterOperator, FilterPredicate, FilterValue};

        // CI runs ignored tests with --include-ignored; without a server this test has nothing to check.
        let Ok(url) = std::env::var("TEST_MIMIR_URL") else {
            eprintln!("skipped: TEST_MIMIR_URL is not set");
            return;
        };
        let mimir = connect(json!({"url":url}).as_object().unwrap()).unwrap();
        mimir.test_connection().await.unwrap();
        let fields = mimir.fields("metrics").await.unwrap();
        assert!(fields.iter().any(|field| field.name == "job" && field.field_type == "label"));
        assert!(fields.iter().any(|field| field.name == "_value" && field.field_type == "sample value"));

        let predicate = |field: &str, field_type: &str, operator, value| FilterPredicate {
            field: field.to_owned(),
            field_type: field_type.to_owned(),
            operator,
            value,
        };
        let up = |value_operator| {
            build_query(
                investigation::dialect(),
                "metrics",
                &[
                    predicate("__name__", "label", FilterOperator::Eq, FilterValue::String("up".to_owned())),
                    predicate("job", "label", FilterOperator::Contains, FilterValue::String("prom".to_owned())),
                    predicate("_value", "sample value", value_operator, FilterValue::Number(1.into())),
                ],
            )
            .unwrap()
        };
        let end = chrono::Utc::now().timestamp_millis();
        let search = |query: String| {
            serde_json::from_value::<crate::search::SearchRequest>(json!({
                "index":"metrics","query":query,"size":10,"histogram_field":"_time",
                "time_range":{"start":end - 120_000,"end":end}
            }))
            .unwrap()
        };
        let matched = mimir.search(search(up(FilterOperator::Eq))).await.unwrap();
        assert!(matched["total"].as_u64().unwrap() > 0, "{matched}");
        for hit in matched["hits"].as_array().unwrap() {
            assert_eq!(hit["source"]["__name__"], "up");
            assert_eq!(hit["source"]["_value"].as_f64(), Some(1.0));
        }
        let none = mimir.search(search(up(FilterOperator::Gt))).await.unwrap();
        assert_eq!(none["total"], 0);
    }
}
