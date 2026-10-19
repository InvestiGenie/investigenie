use super::*;
use crate::storage::tests::TestDatabase;
use tower::ServiceExt;
use wiremock::{matchers, Mock, MockServer, ResponseTemplate};

fn headers(revision: &str) -> HeaderMap {
    let mut headers = HeaderMap::new();
    headers.insert(header::IF_MATCH, format!("\"{revision}\"").parse().unwrap());
    headers
}
fn state(storage: Storage) -> AppState {
    AppState {
        storage,
        upstream_slots: Arc::new(Semaphore::new(4)),
        ai_slots: Arc::new(Semaphore::new(4)),
        investigations: Arc::default(),
    }
}

fn json_request(path: &str, body: Value) -> axum::http::Request<axum::body::Body> {
    axum::http::Request::builder()
        .method("POST")
        .uri(path)
        .header(header::CONTENT_TYPE, "application/json")
        .body(axum::body::Body::from(body.to_string()))
        .unwrap()
}

fn model_request(
    method: Method,
    path: &str,
    body: Option<Value>,
    revision: Option<&str>,
) -> axum::http::Request<axum::body::Body> {
    let mut builder = axum::http::Request::builder().method(method).uri(path);
    if body.is_some() {
        builder = builder.header(header::CONTENT_TYPE, "application/json");
    }
    if let Some(revision) = revision {
        builder = builder.header(header::IF_MATCH, format!("\"{revision}\""));
    }
    builder
        .body(
            body.map(|value| axum::body::Body::from(value.to_string()))
                .unwrap_or_else(axum::body::Body::empty),
        )
        .unwrap()
}

#[tokio::test]
async fn filesystem_ai_conversation_routes_read_continue_and_delete_history() {
    let (_directory, storage) = status_storage().await;
    let provider = MockServer::start().await;
    Mock::given(matchers::method("POST"))
        .and(matchers::path("/chat/completions"))
        .and(|request: &wiremock::Request| {
            let body: Value = serde_json::from_slice(&request.body).unwrap();
            body.get("response_format").is_none()
                && body["messages"].as_array().is_some_and(|messages| {
                    messages.iter().any(|message| {
                        message["content"]
                            .as_str()
                            .is_some_and(|content| content.contains("checkout errors"))
                    })
                })
        })
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "choices":[{"message":{"content":"Check the database connection pool."},"finish_reason":"stop"}]
        })))
        .expect(1)
        .mount(&provider)
        .await;
    let model = storage
        .create_ai_model(StoredAiModel {
            id: String::new(),
            revision: String::new(),
            name: "History model".to_owned(),
            api_url: provider.uri(),
            model: "test-model".to_owned(),
            api_key: "private-key".to_owned(),
        })
        .await
        .unwrap();
    let conversation = StoredAiConversation::new(
        "chat-1".to_owned(),
        model.id,
        vec!["view-1".to_owned()],
        "Why did checkout fail?".to_owned(),
    );
    let conversation = storage.create_ai_conversation(conversation).await.unwrap();
    let conversation = storage
        .append_ai_conversation_messages(
            &conversation.id,
            &conversation.revision,
            vec![StoredAiConversationMessage {
                role: "assistant".to_owned(),
                content: "The investigation found checkout errors.".to_owned(),
                result: Some(json!({"summary":"checkout errors"})),
            }],
        )
        .await
        .unwrap();
    let app = router("http://localhost:5173".parse().unwrap(), storage.clone());

    let stale = app
        .clone()
        .oneshot(model_request(
            Method::POST,
            "/api/ai/conversations/chat-1/messages",
            Some(json!({"message":"What next?"})),
            Some("stale-revision"),
        ))
        .await
        .unwrap();
    assert_eq!(stale.status(), StatusCode::CONFLICT);

    let continued = app
        .clone()
        .oneshot(model_request(
            Method::POST,
            "/api/ai/conversations/chat-1/messages",
            Some(json!({"message":"What should I check next?"})),
            Some(&conversation.revision),
        ))
        .await
        .unwrap();
    assert_eq!(continued.status(), StatusCode::OK);
    let body: Value = serde_json::from_slice(
        &axum::body::to_bytes(continued.into_body(), 65_536)
            .await
            .unwrap(),
    )
    .unwrap();
    assert_eq!(
        body["conversation"]["messages"].as_array().unwrap().len(),
        4
    );
    assert_eq!(
        body["conversation"]["messages"][3]["content"],
        "Check the database connection pool."
    );
    let updated_revision = body["conversation"]["revision"].as_str().unwrap();

    let listed = app
        .clone()
        .oneshot(model_request(
            Method::GET,
            "/api/ai/conversations",
            None,
            None,
        ))
        .await
        .unwrap();
    let listed: Value = serde_json::from_slice(
        &axum::body::to_bytes(listed.into_body(), 65_536)
            .await
            .unwrap(),
    )
    .unwrap();
    assert_eq!(listed["conversations"][0]["id"], "chat-1");

    let deleted = app
        .oneshot(model_request(
            Method::DELETE,
            "/api/ai/conversations/chat-1",
            None,
            Some(updated_revision),
        ))
        .await
        .unwrap();
    assert_eq!(deleted.status(), StatusCode::NO_CONTENT);
    provider.verify().await;
}

#[tokio::test]
async fn ai_model_api_tests_provider_before_save_and_keeps_keys_private() {
    let (_directory, storage) = status_storage().await;
    let provider = MockServer::start().await;
    Mock::given(matchers::method("POST"))
        .and(matchers::path("/v1/chat/completions"))
        .and(matchers::header("authorization", "Bearer provider-secret"))
        .and(matchers::body_partial_json(
            json!({"response_format":{"type":"json_object"},"max_tokens":32}),
        ))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_json(json!({"choices":[{"message":{"content":"{\"ok\":true}"}}]})),
        )
        .expect(3)
        .mount(&provider)
        .await;
    let app = router("http://localhost:5173".parse().unwrap(), storage.clone());
    let response = app.clone().oneshot(model_request(Method::POST, "/api/ai/models", Some(json!({
        "name":"Primary", "api_url":format!("{}/v1/", provider.uri()), "model":"test-model", "api_key":"provider-secret"
    })), None)).await.unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let body: Value = serde_json::from_slice(
        &axum::body::to_bytes(response.into_body(), 4096)
            .await
            .unwrap(),
    )
    .unwrap();
    assert_eq!(body["model"]["name"], "Primary");
    assert_eq!(body["model"]["api_url"], format!("{}/v1", provider.uri()));
    assert_eq!(body["model"]["has_api_key"], true);
    assert!(body["model"].get("api_key").is_none());
    let id = body["model"]["id"].as_str().unwrap();
    let revision = body["model"]["revision"].as_str().unwrap();

    let listed = app
        .clone()
        .oneshot(model_request(Method::GET, "/api/ai/models", None, None))
        .await
        .unwrap();
    assert_eq!(listed.status(), StatusCode::OK);
    let listed: Value = serde_json::from_slice(
        &axum::body::to_bytes(listed.into_body(), 4096)
            .await
            .unwrap(),
    )
    .unwrap();
    assert_eq!(listed["models"].as_array().unwrap().len(), 1);
    assert!(!listed.to_string().contains("provider-secret"));
    assert!(listed["models"][0].get("api_key").is_none());

    let stale = app
        .clone()
        .oneshot(model_request(
            Method::PUT,
            &format!("/api/ai/models/{id}"),
            Some(json!({
                "name":"Renamed", "api_url":provider.uri()+"/v1", "model":"test-model"
            })),
            Some("stale"),
        ))
        .await
        .unwrap();
    assert_eq!(stale.status(), StatusCode::CONFLICT);
    let updated = app
        .clone()
        .oneshot(model_request(
            Method::PUT,
            &format!("/api/ai/models/{id}"),
            Some(json!({
                "name":"Renamed", "api_url":provider.uri()+"/v1", "model":"test-model"
            })),
            Some(revision),
        ))
        .await
        .unwrap();
    assert_eq!(updated.status(), StatusCode::OK);
    let updated: Value = serde_json::from_slice(
        &axum::body::to_bytes(updated.into_body(), 4096)
            .await
            .unwrap(),
    )
    .unwrap();
    assert!(updated["model"].get("api_key").is_none());
    let updated_revision = updated["model"]["revision"].as_str().unwrap();
    assert_eq!(
        storage.ai_model(id).await.unwrap().api_key,
        "provider-secret"
    );

    let duplicate = app.clone().oneshot(model_request(Method::POST, "/api/ai/models", Some(json!({
        "name":"renamed", "api_url":provider.uri()+"/v1", "model":"test-model", "api_key":"provider-secret"
    })), None)).await.unwrap();
    assert_eq!(duplicate.status(), StatusCode::CONFLICT);
    let body: Value = serde_json::from_slice(
        &axum::body::to_bytes(duplicate.into_body(), 4096)
            .await
            .unwrap(),
    )
    .unwrap();
    assert_eq!(body["error"]["code"], "duplicate");

    let missing_revision = app
        .clone()
        .oneshot(model_request(
            Method::DELETE,
            &format!("/api/ai/models/{id}"),
            None,
            None,
        ))
        .await
        .unwrap();
    assert_eq!(missing_revision.status(), StatusCode::PRECONDITION_REQUIRED);
    let stale_delete = app
        .clone()
        .oneshot(model_request(
            Method::DELETE,
            &format!("/api/ai/models/{id}"),
            None,
            Some(revision),
        ))
        .await
        .unwrap();
    assert_eq!(stale_delete.status(), StatusCode::CONFLICT);

    let deleted = app
        .clone()
        .oneshot(model_request(
            Method::DELETE,
            &format!("/api/ai/models/{id}"),
            None,
            Some(updated_revision),
        ))
        .await
        .unwrap();
    assert_eq!(deleted.status(), StatusCode::NO_CONTENT);
    assert!(matches!(
        storage.ai_model(id).await,
        Err(StorageError::AiModelNotFound)
    ));
    provider.verify().await;
}

#[tokio::test]
async fn failed_ai_model_probe_is_sanitized_and_does_not_save_profile() {
    let (_directory, storage) = status_storage().await;
    let provider = MockServer::start().await;
    Mock::given(matchers::method("POST"))
        .and(matchers::path("/chat/completions"))
        .respond_with(ResponseTemplate::new(401).set_body_string("provider-private-detail"))
        .expect(1)
        .mount(&provider)
        .await;
    let app = router("http://localhost:5173".parse().unwrap(), storage.clone());
    let response = app
        .oneshot(model_request(
            Method::POST,
            "/api/ai/models",
            Some(json!({
                "name":"Rejected", "api_url":provider.uri(), "model":"test-model", "api_key":"private-key"
            })),
            None,
        ))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::BAD_GATEWAY);
    let body = axum::body::to_bytes(response.into_body(), 4096)
        .await
        .unwrap();
    let body = String::from_utf8(body.to_vec()).unwrap();
    assert!(!body.contains("provider-private-detail"));
    assert!(!body.contains("private-key"));
    assert_eq!(storage.ai_models().await.unwrap().len(), 0);
    provider.verify().await;
}

#[tokio::test]
async fn ai_model_list_is_empty_when_no_profiles_are_saved() {
    let (_directory, storage) = status_storage().await;
    let app = router("http://localhost:5173".parse().unwrap(), storage);
    let response = app
        .oneshot(
            axum::http::Request::builder()
                .method("GET")
                .uri("/api/ai/models")
                .body(axum::body::Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let body = axum::body::to_bytes(response.into_body(), 1024)
        .await
        .unwrap();
    let body: Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(body, json!({"models":[]}));
}

#[tokio::test]
async fn ai_investigation_plans_queries_and_returns_only_cited_evidence() {
    assert_ai_investigation_cursor_batches(false).await;
}

#[tokio::test]
async fn bundled_web_ui_is_served_same_origin_without_shadowing_api_routes() {
    let (_directory, storage) = status_storage().await;
    let web = tempfile::tempdir().unwrap();
    std::fs::write(
        web.path().join("index.html"),
        "<html>InvestiGenie UI</html>",
    )
    .unwrap();
    std::fs::create_dir(web.path().join("assets")).unwrap();
    std::fs::write(
        web.path().join("assets").join("app.js"),
        "console.log('ui')",
    )
    .unwrap();
    let app = with_web(
        router("http://localhost:5173".parse().unwrap(), storage),
        web.path(),
    );
    let fetch = |uri: &'static str| {
        let app = app.clone();
        async move {
            let response = app
                .oneshot(
                    axum::http::Request::builder()
                        .uri(uri)
                        .body(axum::body::Body::empty())
                        .unwrap(),
                )
                .await
                .unwrap();
            let status = response.status();
            let body = axum::body::to_bytes(response.into_body(), 64 * 1024)
                .await
                .unwrap();
            (status, String::from_utf8_lossy(&body).into_owned())
        }
    };
    let (status, body) = fetch("/").await;
    assert_eq!(status, StatusCode::OK);
    assert!(body.contains("InvestiGenie UI"));
    assert_eq!(
        fetch("/assets/app.js").await,
        (StatusCode::OK, "console.log('ui')".to_owned())
    );
    assert!(fetch("/explore/anything")
        .await
        .1
        .contains("InvestiGenie UI"));
    assert!(!fetch("/../Cargo.toml").await.1.contains("[package]"));
    let (status, body) = fetch("/api/not-a-route").await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert!(body.contains("not_found"));
    assert_eq!(fetch("/api/health/live").await.0, StatusCode::OK);
}

#[tokio::test]
async fn background_investigations_validate_controls_poll_status_and_reject_unknown_ids() {
    let (_directory, storage) = status_storage().await;
    let model = storage
        .create_ai_model(StoredAiModel {
            id: String::new(),
            revision: String::new(),
            name: "Background".to_owned(),
            api_url: "http://127.0.0.1:9/v1".to_owned(),
            api_key: "unused".to_owned(),
            model: "test-model".to_owned(),
        })
        .await
        .unwrap();
    let app = router("http://localhost:5173".parse().unwrap(), storage);
    let get = |uri: String| {
        axum::http::Request::builder()
            .method("GET")
            .uri(uri)
            .body(axum::body::Body::empty())
            .unwrap()
    };
    async fn body(response: Response) -> Value {
        serde_json::from_slice(
            &axum::body::to_bytes(response.into_body(), 64 * 1024)
                .await
                .unwrap(),
        )
        .unwrap()
    }

    for controls in [
        json!({"ai_timeout_seconds": 1}),
        json!({"time_budget_seconds": 5}),
    ] {
        let mut request =
            json!({"question":"Errors?","data_view_ids":["missing-view"],"ai_model_id":model.id});
        request
            .as_object_mut()
            .unwrap()
            .extend(controls.as_object().unwrap().clone());
        let response = app
            .clone()
            .oneshot(json_request("/api/ai/investigations", request))
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    }

    let response = app
        .clone()
        .oneshot(json_request(
            "/api/ai/investigations",
            json!({"question":"Errors?","data_view_ids":["missing-view"],"ai_model_id":model.id,"ai_timeout_seconds":30,"time_budget_seconds":60}),
        ))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::ACCEPTED);
    let started = body(response).await;
    let id = started["investigation"]["id"].as_str().unwrap().to_owned();
    assert_eq!(started["investigation"]["time_budget_seconds"], 60);
    let mut snapshot = Value::Null;
    for _ in 0..200 {
        snapshot = body(
            app.clone()
                .oneshot(get(format!("/api/ai/investigations/{id}")))
                .await
                .unwrap(),
        )
        .await;
        if snapshot["investigation"]["status"] == "failed" {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(5)).await;
    }
    assert_eq!(snapshot["investigation"]["status"], "failed", "{snapshot}");
    assert_eq!(
        snapshot["investigation"]["error"]["code"],
        "invalid_investigation"
    );
    let late = app
        .clone()
        .oneshot(json_request(
            &format!("/api/ai/investigations/{id}/decision"),
            json!({"action":"continue"}),
        ))
        .await
        .unwrap();
    assert_eq!(late.status(), StatusCode::CONFLICT);

    let missing = app
        .clone()
        .oneshot(get("/api/ai/investigations/unknown".to_owned()))
        .await
        .unwrap();
    assert_eq!(missing.status(), StatusCode::NOT_FOUND);
    let missing = app
        .oneshot(json_request(
            "/api/ai/investigations/unknown/decision",
            json!({"action":"terminate"}),
        ))
        .await
        .unwrap();
    assert_eq!(missing.status(), StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn ai_investigation_scans_all_cursor_batches_and_retains_later_evidence() {
    assert_ai_investigation_cursor_batches(true).await;
}

async fn assert_ai_investigation_cursor_batches(multiple_pages: bool) {
    let (_directory, storage) = status_storage().await;
    let source = MockServer::start().await;
    let provider = MockServer::start().await;
    let first_page_size = if multiple_pages { 100 } else { 2 };
    let records_examined = if multiple_pages { 102 } else { 2 };
    let first_hits = (1..=first_page_size)
        .map(|record_number| json!({
            "_index":"events","_id":format!("event-{record_number}"),"_score":null,
            "sort":[1791630000000i64,record_number],
            "_source":{"@timestamp":"2026-10-10T11:00:00Z","service":"checkout","message":"inventory timeout"}
        }))
        .collect::<Vec<_>>();
    Mock::given(matchers::method("GET"))
        .and(matchers::path("/events/_mapping"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "events":{"mappings":{"properties":{
                "@timestamp":{"type":"date"},
                "service":{"type":"keyword"},
                "message":{"type":"text"}
            }}}
        })))
        .expect(1)
        .mount(&source)
        .await;
    Mock::given(matchers::method("POST"))
        .and(matchers::path("/events/_pit"))
        .and(matchers::query_param("keep_alive", "1m"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"id":"test-pit"})))
        .expect(1)
        .mount(&source)
        .await;
    Mock::given(matchers::method("DELETE"))
        .and(matchers::path("/_pit"))
        .and(matchers::body_partial_json(json!({"id":"test-pit"})))
        .respond_with(ResponseTemplate::new(503))
        .expect(1)
        .mount(&source)
        .await;
    let search_body = json!({
        "size":100,"pit":{"id":"test-pit","keep_alive":"1m"},
        "sort":[{"@timestamp":{"order":"asc"}},{"_shard_doc":"asc"}],
        "query":{"bool":{"must":[{"query_string":{"query":"service:\"checkout\""}}],"filter":[{"range":{"@timestamp":{"gte":1791630000000i64,"lte":1791633600000i64,"format":"epoch_millis"}}}]}}
    });
    Mock::given(matchers::method("POST"))
        .and(matchers::path("/_search"))
        .and(matchers::header("content-type", "application/json"))
        .and(matchers::body_partial_json(search_body.clone()))
        .and(|request: &wiremock::Request| {
            let body: Value = serde_json::from_slice(&request.body).unwrap();
            body.get("search_after").is_none() && body.get("from").is_none() && body.get("aggs").is_none()
        })
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "took":7,"timed_out":false,"_shards":{"failed":0},"hits":{"total":{"value":records_examined,"relation":"eq"},"hits":first_hits}
        })))
        .expect(1)
        .mount(&source)
        .await;
    if multiple_pages {
        Mock::given(matchers::method("POST"))
            .and(matchers::path("/_search"))
            .and(matchers::body_partial_json(search_body))
            .and(matchers::body_partial_json(json!({"search_after":[1791630000000i64,100]})))
            .and(|request: &wiremock::Request| {
                let body: Value = serde_json::from_slice(&request.body).unwrap();
                body.get("from").is_none() && body.get("aggs").is_none()
            })
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "took":3,"timed_out":false,"_shards":{"failed":0},"hits":{"total":{"value":102,"relation":"eq"},"hits":[
                    {"_index":"events","_id":"event-101","sort":[1791630001000i64,101],"_source":{"@timestamp":"2026-10-10T11:00:01Z","service":"checkout","message":"payment authorization rejected"}},
                    {"_index":"events","_id":"event-102","sort":[1791630002000i64,102],"_source":{"@timestamp":"2026-10-10T11:00:02Z","service":"checkout","message":"payment authorization rejected"}}
                ]}
            })))
            .expect(1)
            .mount(&source)
            .await;
    }

    let plan_content = json!({"views":[{"data_view_id":"checkout-view","filters":[{"field":"service","operator":"eq","value":"checkout"}]}]}).to_string();
    Mock::given(matchers::method("POST"))
        .and(matchers::path("/v1/chat/completions"))
        .and(matchers::header(
            "authorization",
            "Bearer test-provider-secret",
        ))
        .and(matchers::body_partial_json(
            json!({"model":"test-model","response_format":{"type":"json_object"}}),
        ))
        .and(|request: &wiremock::Request| {
            !String::from_utf8_lossy(&request.body).contains("Analyze only the provided records.")
        })
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_json(json!({"choices":[{"message":{"content":plan_content}}],"usage":{"prompt_tokens":210,"completion_tokens":44,"total_tokens":254}})),
        )
        .expect(1)
        .mount(&provider)
        .await;
    let findings_content = json!({
        "summary":"Checkout errors correlate with inventory timeouts.",
        "findings":[{"title":"Inventory timeout","detail":"Two checkout events report inventory timeouts.","severity":"high","confidence":"medium","evidence":[{"data_view_id":"checkout-view","data_view_name":"Checkout logs","record_ids":["event-1","event-2"],"count":2}]}],
        "caveats":[]
    }).to_string();
    let findings_mock = Mock::given(matchers::method("POST"))
        .and(matchers::path("/v1/chat/completions"))
        .and(matchers::body_string_contains(
            "Analyze only the provided records.",
        ))
        .and(move |request: &wiremock::Request| {
            let body: Value = serde_json::from_slice(&request.body).unwrap();
            let input: Value = serde_json::from_str(body["messages"][1]["content"].as_str().unwrap()).unwrap();
            input["sources"][0]["records_examined"] == first_page_size
        })
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_json(json!({"choices":[{"message":{"content":findings_content}}],"usage":{"prompt_tokens":420,"completion_tokens":96,"total_tokens":516}})),
        )
        .expect(if multiple_pages { 0 } else { 1 })
        .with_priority(1);
    findings_mock.mount(&provider).await;
    if multiple_pages {
        let later_findings = json!({
            "summary":"Inventory timeouts and later payment authorization failures affect checkout.",
            "findings":[
                {"title":"Inventory timeout","detail":"The first batch contains inventory timeouts.","severity":"high","confidence":"medium","evidence":[{"data_view_id":"checkout-view","data_view_name":"Checkout logs","record_ids":["event-1","event-2"],"count":100}]},
                {"title":"Payment authorization rejected","detail":"The later batch reveals payment authorization failures.","severity":"high","confidence":"high","evidence":[{"data_view_id":"checkout-view","data_view_name":"Checkout logs","record_ids":["event-101","event-102"],"count":2}]}
            ],"caveats":[]
        }).to_string();
        Mock::given(matchers::method("POST"))
            .and(matchers::path("/v1/chat/completions"))
            .and(matchers::body_string_contains(
                "Analyze only the provided records.",
            ))
            .and(|request: &wiremock::Request| {
                let body: Value = serde_json::from_slice(&request.body).unwrap();
                let input: Value =
                    serde_json::from_str(body["messages"][1]["content"].as_str().unwrap()).unwrap();
                input["sources"][0]["records_examined"] == 102
            })
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "choices":[{"message":{"content":later_findings}}],
                "usage":{"prompt_tokens":420,"completion_tokens":96,"total_tokens":516}
            })))
            .with_priority(1)
            .expect(1)
            .mount(&provider)
            .await;
    }

    storage.create_connection(StoredConnection {
        id:"checkout".to_owned(), name:"Checkout source".to_owned(), integration_id:"elasticsearch".to_owned(),
        config:serde_json::from_value(json!({"url":source.uri(),"username":"source-private-user","password":"source-private-password","auth_type":"basic"})).unwrap(), revision:String::new()
    }).await.unwrap();
    let connection = storage.connection("checkout").await.unwrap();
    let revision = connection.record.revision.clone();
    storage
        .save_view(
            DataView {
                id: "checkout-view".to_owned(),
                name: "Checkout logs".to_owned(),
                connection_id: "checkout".to_owned(),
                index: "events".to_owned(),
                time_field: "@timestamp".to_owned(),
                revision: String::new(),
            },
            &revision,
            None,
        )
        .await
        .unwrap();
    let saved_model = storage
        .create_ai_model(StoredAiModel {
            id: String::new(),
            revision: String::new(),
            name: "Test model".to_owned(),
            api_url: provider.uri() + "/v1",
            api_key: "test-provider-secret".to_owned(),
            model: "test-model".to_owned(),
        })
        .await
        .unwrap();
    let app = router("http://localhost:5173".parse().unwrap(), storage);
    let response = app.oneshot(json_request("/api/ai/investigate",json!({"question":"Why are checkouts failing?","data_view_ids":["checkout-view"],"ai_model_id":saved_model.id,"time_range":{"start":1791630000000i64,"end":1791633600000i64}}))).await.unwrap();
    let status = response.status();
    let body = axum::body::to_bytes(response.into_body(), 32 * 1024)
        .await
        .unwrap();
    let body: Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(
        body["findings"][0]["evidence"][0]["record_ids"],
        json!(["event-1", "event-2"])
    );
    assert_eq!(body["sources"][0]["records_examined"], records_examined);
    assert_eq!(
        body["sources"][0]["batches_processed"],
        if multiple_pages { 2 } else { 1 }
    );
    assert!(body["sources"][0]["query_summary"]
        .as_str()
        .unwrap()
        .contains("1791630000000 to 1791633600000"));
    assert_eq!(
        body["token_usage"][0],
        json!({"stage":"planning","prompt_tokens":210,"completion_tokens":44,"total_tokens":254})
    );
    let analysis_calls = 1;
    assert_eq!(body["token_usage"].as_array().unwrap().len(), 2);
    assert_eq!(
        body["token_usage"][1],
        json!({"stage":"analysis","prompt_tokens":420 * analysis_calls,"completion_tokens":96 * analysis_calls,"total_tokens":516 * analysis_calls})
    );
    for secret in [
        "source-private-user",
        "source-private-password",
        "test-provider-secret",
    ] {
        assert!(!body.to_string().contains(secret));
    }
    let provider_requests = provider.received_requests().await.unwrap();
    assert_eq!(provider_requests.len(), 1 + analysis_calls as usize);
    let analysis_inputs = provider_requests
        .iter()
        .map(|request| serde_json::from_slice::<Value>(&request.body).unwrap())
        .filter(|request| {
            request["messages"][0]["content"]
                .as_str()
                .unwrap()
                .contains("Analyze only the provided records.")
        })
        .map(|request| {
            serde_json::from_str::<Value>(request["messages"][1]["content"].as_str().unwrap())
                .unwrap()
        })
        .collect::<Vec<_>>();
    assert_eq!(analysis_inputs.len(), analysis_calls as usize);
    assert!(analysis_inputs[0]["previous_findings"].is_null());
    let first_examples = analysis_inputs[0]["sources"][0]["examples"]
        .as_array()
        .unwrap();
    assert!(first_examples
        .iter()
        .any(|example| example["record_id"] == "event-1"));
    assert!(first_examples
        .iter()
        .any(|example| example["record_id"] == "event-2"));
    assert!(first_examples
        .iter()
        .filter(|example| ["event-1", "event-2", "event-3"]
            .contains(&example["record_id"].as_str().unwrap()))
        .all(|example| example["observed_duplicate_count"] == first_page_size));
    if multiple_pages {
        assert!(body["sources"][0]["records_examined"].as_u64().unwrap() > 100);
        assert_eq!(
            body["findings"][1]["title"],
            "Payment authorization rejected"
        );
        assert_eq!(
            body["findings"][1]["evidence"][0]["record_ids"],
            json!(["event-101", "event-102"])
        );
        let later_ids = first_examples
            .iter()
            .filter(|example| example["observed_duplicate_count"] == 2)
            .map(|example| example["record_id"].as_str().unwrap())
            .collect::<Vec<_>>();
        assert_eq!(later_ids, vec!["event-101", "event-102"]);
    }
    let all_provider_input = provider_requests
        .iter()
        .filter_map(|request| std::str::from_utf8(&request.body).ok())
        .collect::<String>();
    assert!(!all_provider_input.contains("source-private-user"));
    assert!(!all_provider_input.contains("source-private-password"));
    assert!(!all_provider_input.contains("test-provider-secret"));
    source.verify().await;
    provider.verify().await;
}

async fn status_storage() -> (tempfile::TempDir, Storage) {
    use base64::{engine::general_purpose::STANDARD, Engine};
    let directory = tempfile::tempdir().unwrap();
    let storage = Storage::open(crate::storage::PersistenceOptions::Filesystem {
        path: directory.path().to_owned(),
        key: STANDARD.encode([9u8; 32]),
        key_id: "v1".to_owned(),
    })
    .await
    .unwrap();
    (directory, storage)
}

async fn save_status_connection(storage: &Storage, integration_id: &str, url: &str) {
    storage
        .create_connection(StoredConnection {
            id: integration_id.to_owned(),
            name: "Saved source".to_owned(),
            integration_id: integration_id.to_owned(),
            config: serde_json::from_value(json!({
                "url": url, "username": "private-status-user", "password": "private-status-password"
            }))
            .unwrap(),
            revision: String::new(),
        })
        .await
        .unwrap();
}

async fn status_response(app: &Router, id: &str) -> (StatusCode, Value) {
    let response = app
        .clone()
        .oneshot(
            axum::http::Request::builder()
                .uri(format!("/api/connections/{id}/status"))
                .body(axum::body::Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    let code = response.status();
    let body = axum::body::to_bytes(response.into_body(), 4096)
        .await
        .unwrap();
    let body: Value = serde_json::from_slice(&body).unwrap();
    for secret in [
        "private-status-user",
        "private-status-password",
        "upstream-private-error",
    ] {
        assert!(!body.to_string().contains(secret));
    }
    (code, body)
}

async fn mount_status_probe(server: &MockServer, integration_id: &str, response: ResponseTemplate) {
    let probe = if integration_id == "elasticsearch" {
        Mock::given(matchers::method("GET")).and(matchers::path("/_cat/indices"))
    } else {
        Mock::given(matchers::method("POST")).and(matchers::body_string("SELECT 1 FORMAT JSON"))
    };
    probe.respond_with(response).expect(1).mount(server).await;
}

#[tokio::test]
async fn filesystem_api_status_routes_check_both_connectors_and_recover_without_record_changes() {
    let (_directory, storage) = status_storage().await;
    let server = MockServer::start().await;
    let app = router("http://localhost:5173".parse().unwrap(), storage.clone());
    for integration_id in ["elasticsearch", "clickhouse"] {
        save_status_connection(&storage, integration_id, &server.uri()).await;
        let before = storage.connection(integration_id).await.unwrap().record;
        for (upstream_code, expected) in [
            (200, "connected"),
            (503, "not_connected"),
            (200, "connected"),
        ] {
            let response = if upstream_code == 200 {
                ResponseTemplate::new(200).set_body_json(if integration_id == "elasticsearch" {
                    json!([])
                } else {
                    json!({"meta":[], "data":[{"1":1}], "rows":1})
                })
            } else {
                ResponseTemplate::new(upstream_code).set_body_string("upstream-private-error")
            };
            mount_status_probe(&server, integration_id, response).await;
            assert_eq!(
                status_response(&app, integration_id).await,
                (StatusCode::OK, json!({"status": expected}))
            );
            server.verify().await;
            server.reset().await;
        }
        assert_eq!(
            status_response(&app, integration_id).await,
            (StatusCode::OK, json!({"status":"not_connected"}))
        );
        let after = storage.connection(integration_id).await.unwrap().record;
        assert_eq!(after.config, before.config);
        assert_eq!(after.revision, before.revision);
    }
    assert_eq!(storage.connections().await.unwrap().len(), 2);
    assert_eq!(
        status_response(&app, "missing").await.0,
        StatusCode::NOT_FOUND
    );
}

#[tokio::test]
async fn filesystem_api_status_routes_bound_stalled_connector_probes_to_five_seconds() {
    let (_directory, storage) = status_storage().await;
    let server = MockServer::start().await;
    let app = router("http://localhost:5173".parse().unwrap(), storage.clone());
    for integration_id in ["elasticsearch", "clickhouse"] {
        save_status_connection(&storage, integration_id, &server.uri()).await;
        mount_status_probe(
            &server,
            integration_id,
            ResponseTemplate::new(200)
                .set_body_json(if integration_id == "elasticsearch" {
                    json!([])
                } else {
                    json!({"meta":[],"data":[],"rows":0})
                })
                .set_delay(std::time::Duration::from_secs(6)),
        )
        .await;
        let started = std::time::Instant::now();
        assert_eq!(
            status_response(&app, integration_id).await,
            (StatusCode::OK, json!({"status":"not_connected"}))
        );
        assert!(started.elapsed() >= std::time::Duration::from_secs(5));
        assert!(started.elapsed() < std::time::Duration::from_secs(6));
        server.verify().await;
        server.reset().await;
    }
}

#[tokio::test]
async fn filesystem_api_status_routes_report_refused_connections_without_removing_saved_records() {
    let (_directory, storage) = status_storage().await;
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let endpoint = format!("http://{}", listener.local_addr().unwrap());
    drop(listener);
    let app = router("http://localhost:5173".parse().unwrap(), storage.clone());
    for integration_id in ["elasticsearch", "clickhouse"] {
        save_status_connection(&storage, integration_id, &endpoint).await;
        assert_eq!(
            status_response(&app, integration_id).await,
            (StatusCode::OK, json!({"status":"not_connected"}))
        );
        assert!(storage.connection(integration_id).await.is_ok());
    }
}

#[tokio::test]
async fn filesystem_api_status_capacity_errors_remain_errors_and_hide_credentials() {
    let (_directory, storage) = status_storage().await;
    save_status_connection(&storage, "elasticsearch", "http://127.0.0.1:1").await;
    let state = state(storage);
    let _permits = state
        .upstream_slots
        .clone()
        .acquire_many_owned(4)
        .await
        .unwrap();
    let error = connection_status(State(state), Path("elasticsearch".to_owned()))
        .await
        .unwrap_err();
    let response = error.into_response();
    assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);
    let body = axum::body::to_bytes(response.into_body(), 4096)
        .await
        .unwrap();
    let body: Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(body["error"]["code"], "upstream_capacity_unavailable");
    assert!(!body.to_string().contains("private-status"));
}

#[tokio::test]
async fn filesystem_api_retains_and_replaces_credentials_on_edit_and_reopen() {
    use crate::storage::PersistenceOptions;
    use base64::{engine::general_purpose::STANDARD, Engine};
    let directory = tempfile::tempdir().unwrap();
    let options = PersistenceOptions::Filesystem {
        path: directory.path().to_owned(),
        key: STANDARD.encode([7u8; 32]),
        key_id: "v1".to_owned(),
    };
    let storage = Storage::open(options.clone()).await.unwrap();
    let state = state(storage.clone());
    let server = MockServer::start().await;
    for (id, password, authorization) in [
        ("blank", "", "Basic ZXhwbG9yZXI6"),
        (
            "secret",
            "private-test-password",
            "Basic ZXhwbG9yZXI6cHJpdmF0ZS10ZXN0LXBhc3N3b3Jk",
        ),
    ] {
        Mock::given(matchers::method("POST"))
            .and(matchers::body_string("SELECT 1 FORMAT JSON"))
            .and(matchers::header("authorization", authorization))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_json(json!({"meta":[],"data":[{"1":1}],"rows":1})),
            )
            .expect(2)
            .mount(&server)
            .await;
        let added = add_connection(State(state.clone()), Json(serde_json::from_value(json!({"name":id,"connection_type":"clickhouse","url":server.uri(),"username":"explorer","password":password})).unwrap())).await.unwrap();
        let retained = update_connection(
            State(state.clone()),
            Path(id.to_owned()),
            headers(added.0["revision"].as_str().unwrap()),
            Json(
                serde_json::from_value(
                    json!({"name":id,"url":server.uri(),"connection_type":"elasticsearch"}),
                )
                .unwrap(),
            ),
        )
        .await
        .unwrap();
        let saved = storage.connection(id).await.unwrap();
        assert_eq!(saved.record.integration_id, "clickhouse");
        assert_eq!(saved.record.config["username"], "explorer");
        assert_eq!(saved.record.config["password"], password);
        let replacement = if id == "blank" { " replacement " } else { "" };
        Mock::given(matchers::method("POST"))
            .and(matchers::body_string("SELECT 1 FORMAT JSON"))
            .and(matchers::header(
                "authorization",
                format!(
                    "Basic {}",
                    STANDARD.encode(format!("replacement:{replacement}"))
                ),
            ))
            .respond_with(
                ResponseTemplate::new(200).set_body_json(json!({"meta":[],"data":[],"rows":0})),
            )
            .expect(1)
            .mount(&server)
            .await;
        let _ = update_connection(State(state.clone()), Path(id.to_owned()), headers(retained.0["revision"].as_str().unwrap()),
            Json(serde_json::from_value(json!({"name":id,"url":server.uri(),"username":"replacement","password":replacement,"database":null})).unwrap())).await.unwrap();
        let saved = storage.connection(id).await.unwrap();
        assert_eq!(saved.record.config["username"], "replacement");
        assert_eq!(saved.record.config["password"], replacement);
        assert_eq!(saved.record.config["database"], Value::Null);
        for malformed in [Value::Null, json!(false), json!(1), json!([]), json!({})] {
            for secret in ["username", "password"] {
                let mut request = json!({"name":id,"url":server.uri()})
                    .as_object()
                    .unwrap()
                    .clone();
                request.insert(secret.to_owned(), malformed.clone());
                let error = update_connection(
                    State(state.clone()),
                    Path(id.to_owned()),
                    headers(&saved.record.revision),
                    Json(serde_json::from_value(Value::Object(request)).unwrap()),
                )
                .await
                .unwrap_err();
                assert_eq!(error.into_response().status(), StatusCode::BAD_REQUEST);
                let unchanged = storage.connection(id).await.unwrap().record;
                assert_eq!(unchanged.config, saved.record.config);
                assert_eq!(unchanged.revision, saved.record.revision);
            }
        }
    }
    let listed = connections(State(state.clone())).await.unwrap().0;
    assert!(!listed.to_string().contains("private-test-password"));
    assert!(!listed.to_string().contains("explorer"));
    for connection in listed["connections"].as_array().unwrap() {
        assert!(connection["configuration"].get("username").is_none());
        assert!(connection["configuration"].get("password").is_none());
    }
    drop(state);
    drop(storage);
    let reopened = Storage::open(options).await.unwrap();
    assert_eq!(
        reopened.connection("blank").await.unwrap().record.config["password"],
        " replacement "
    );
    assert_eq!(
        reopened.connection("secret").await.unwrap().record.config["password"],
        ""
    );
}
#[test]
fn preconditions_are_single_quoted_opaque_tokens() {
    assert!(matches!(
        if_match(&HeaderMap::new()),
        Err(ApiError::PreconditionRequired)
    ));
    assert_eq!(
        ApiError::PreconditionRequired.into_response().status(),
        StatusCode::PRECONDITION_REQUIRED
    );
    assert_eq!(
        ApiError::Storage(StorageError::Conflict)
            .into_response()
            .status(),
        StatusCode::CONFLICT
    );
    assert_eq!(if_match(&headers("opaque-v1")).unwrap(), "opaque-v1");
    for value in ["*", "W/\"token\"", "token", "\"\"", "\"a\", \"b\""] {
        let mut headers = HeaderMap::new();
        headers.insert(header::IF_MATCH, value.parse().unwrap());
        assert!(if_match(&headers).is_err());
    }
    let mut duplicate = headers("one");
    duplicate.append(header::IF_MATCH, "\"two\"".parse().unwrap());
    assert!(if_match(&duplicate).is_err());
}

#[tokio::test]
async fn filesystem_api_infers_legacy_auth_and_switches_credentials_safely() {
    use crate::storage::PersistenceOptions;
    use base64::{engine::general_purpose::STANDARD, Engine};
    let directory = tempfile::tempdir().unwrap();
    let options = PersistenceOptions::Filesystem {
        path: directory.path().to_owned(),
        key: STANDARD.encode([8u8; 32]),
        key_id: "v1".to_owned(),
    };
    let storage = Storage::open(options.clone()).await.unwrap();
    let state = state(storage.clone());
    let server = MockServer::start().await;
    for (id, credentials) in [
        (
            "basic",
            json!({"username":"reader","password":" original "}),
        ),
        ("none", json!({})),
        ("api_key", json!({"api_key":"legacy-key"})),
    ] {
        let mut config = credentials.as_object().unwrap().clone();
        config.insert("url".to_owned(), json!(server.uri()));
        storage
            .create_connection(StoredConnection {
                id: id.to_owned(),
                name: id.to_owned(),
                integration_id: "elasticsearch".to_owned(),
                config,
                revision: String::new(),
            })
            .await
            .unwrap();
    }
    let listed = connections(State(state.clone())).await.unwrap().0;
    for connection in listed["connections"].as_array().unwrap() {
        assert_eq!(connection["configuration"]["auth_type"], connection["id"]);
        for secret in ["username", "password", "api_key"] {
            assert!(connection["configuration"].get(secret).is_none());
        }
    }
    assert!(server.received_requests().await.unwrap().is_empty());
    let basic_header = |credentials: &str| format!("Basic {}", STANDARD.encode(credentials));
    for (updates, authorization, mode) in [
        (
            json!({"auth_type":"basic"}),
            Some(basic_header("reader: original ")),
            "basic",
        ),
        (
            json!({"auth_type":"basic","username":"replacement"}),
            Some(basic_header("replacement: original ")),
            "basic",
        ),
        (
            json!({"auth_type":"basic","password":" rotated "}),
            Some(basic_header("replacement: rotated ")),
            "basic",
        ),
        (
            json!({"auth_type":"basic","password":"   "}),
            Some(basic_header("replacement:   ")),
            "basic",
        ),
        (
            json!({"auth_type":"api_key","api_key":"first-key"}),
            Some("ApiKey first-key".to_owned()),
            "api_key",
        ),
        (
            json!({"auth_type":"api_key"}),
            Some("ApiKey first-key".to_owned()),
            "api_key",
        ),
        (
            json!({"api_key":"second-key"}),
            Some("ApiKey second-key".to_owned()),
            "api_key",
        ),
        (json!({"auth_type":"none"}), None, "none"),
        (
            json!({"auth_type":"basic","username":"reader","password":"again"}),
            Some(basic_header("reader:again")),
            "basic",
        ),
        (json!({"auth_type":"none"}), None, "none"),
        (
            json!({"auth_type":"api_key","api_key":"third-key"}),
            Some("ApiKey third-key".to_owned()),
            "api_key",
        ),
        (
            json!({"auth_type":"basic","username":"reader","password":"final"}),
            Some(basic_header("reader:final")),
            "basic",
        ),
    ] {
        let mock = Mock::given(matchers::method("GET")).and(matchers::path("/_cat/indices"));
        let mock = if let Some(authorization) = authorization {
            mock.and(matchers::header("authorization", authorization))
        } else {
            mock.and(|request: &wiremock::Request| !request.headers.contains_key("authorization"))
        };
        mock.respond_with(ResponseTemplate::new(200).set_body_json(json!([])))
            .expect(1)
            .mount(&server)
            .await;
        let before = storage.connection("basic").await.unwrap().record;
        let mut request = updates.as_object().unwrap().clone();
        request.insert("name".to_owned(), json!("basic"));
        request.insert("url".to_owned(), json!(server.uri()));
        let _ = update_connection(
            State(state.clone()),
            Path("basic".to_owned()),
            headers(&before.revision),
            Json(serde_json::from_value(Value::Object(request)).unwrap()),
        )
        .await
        .unwrap();
        let saved = storage.connection("basic").await.unwrap().record;
        assert_ne!(saved.revision, before.revision);
        assert_eq!(saved.config["auth_type"], mode);
        for (secret, active) in [
            ("username", "basic"),
            ("password", "basic"),
            ("api_key", "api_key"),
        ] {
            assert_eq!(saved.config.contains_key(secret), mode == active);
        }
        if mode == "none" {
            for credentials in [
                json!({}),
                json!({"username":"reader"}),
                json!({"password":"secret"}),
            ] {
                let mut request = credentials.as_object().unwrap().clone();
                request.insert("name".to_owned(), json!("basic"));
                request.insert("url".to_owned(), json!(server.uri()));
                request.insert("auth_type".to_owned(), json!("basic"));
                let error = update_connection(
                    State(state.clone()),
                    Path("basic".to_owned()),
                    headers(&saved.revision),
                    Json(serde_json::from_value(Value::Object(request)).unwrap()),
                )
                .await
                .unwrap_err();
                assert_eq!(error.into_response().status(), StatusCode::BAD_REQUEST);
                let unchanged = storage.connection("basic").await.unwrap().record;
                assert_eq!(unchanged.config, saved.config);
                assert_eq!(unchanged.revision, saved.revision);
            }
        }
        server.verify().await;
        server.reset().await;
    }
    let original = storage.connection("basic").await.unwrap().record;
    for changes in [
        json!({"password":null}),
        json!({"username":false}),
        json!({"api_key":42}),
        json!({"password":[]}),
        json!({"password":{}}),
        json!({"auth_type":null}),
        json!({"auth_type":"unknown"}),
        json!({"auth_type":"basic","password":""}),
        json!({"auth_type":"api_key"}),
    ] {
        let mut request = changes.as_object().unwrap().clone();
        request.insert("name".to_owned(), json!("basic"));
        request.insert("url".to_owned(), json!(server.uri()));
        let error = update_connection(
            State(state.clone()),
            Path("basic".to_owned()),
            headers(&original.revision),
            Json(serde_json::from_value(Value::Object(request)).unwrap()),
        )
        .await
        .unwrap_err();
        assert_eq!(error.into_response().status(), StatusCode::BAD_REQUEST);
        let saved = storage.connection("basic").await.unwrap().record;
        assert_eq!(saved.config, original.config);
        assert_eq!(saved.revision, original.revision);
    }
    assert!(server.received_requests().await.unwrap().is_empty());
    Mock::given(matchers::method("GET"))
        .and(matchers::path("/_cat/indices"))
        .and(matchers::header("authorization", "ApiKey rejected-key"))
        .respond_with(ResponseTemplate::new(401).set_body_string("rejected-key"))
        .expect(1)
        .mount(&server)
        .await;
    let error = update_connection(State(state.clone()), Path("basic".to_owned()), headers(&original.revision),
        Json(serde_json::from_value(json!({"name":"basic","url":server.uri(),"auth_type":"api_key","api_key":"rejected-key"})).unwrap())).await.unwrap_err();
    assert_eq!(error.into_response().status(), StatusCode::BAD_GATEWAY);
    assert_eq!(
        storage.connection("basic").await.unwrap().record.config,
        original.config
    );
    server.verify().await;
    server.reset().await;
    Mock::given(matchers::method("GET"))
        .and(matchers::path("/_cat/indices"))
        .and(matchers::header(
            "authorization",
            basic_header("reader:duplicate"),
        ))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!([])))
        .expect(1)
        .mount(&server)
        .await;
    assert!(matches!(
        update_connection(
            State(state.clone()),
            Path("basic".to_owned()),
            headers(&original.revision),
            Json(
                serde_json::from_value(
                    json!({"name":"none","url":server.uri(),"password":"duplicate"})
                )
                .unwrap()
            )
        )
        .await,
        Err(ApiError::Storage(StorageError::Duplicate))
    ));
    assert_eq!(
        storage.connection("basic").await.unwrap().record.config,
        original.config
    );
    assert_eq!(
        storage.connection("basic").await.unwrap().record.revision,
        original.revision
    );
    server.verify().await;
    server.reset().await;
    for password in ["winner-one", "winner-two"] {
        Mock::given(matchers::method("GET"))
            .and(matchers::path("/_cat/indices"))
            .and(matchers::header(
                "authorization",
                basic_header(&format!("reader:{password}")),
            ))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_json(json!([]))
                    .set_delay(std::time::Duration::from_millis(30)),
            )
            .expect(1)
            .mount(&server)
            .await;
    }
    let update = |password| {
        update_connection(State(state.clone()), Path("basic".to_owned()), headers(&original.revision),
        Json(serde_json::from_value(json!({"name":"basic","url":server.uri(),"auth_type":"basic","password":password})).unwrap()))
    };
    let (first, second) = tokio::join!(update("winner-one"), update("winner-two"));
    let (winner, loser, password) = if first.is_ok() {
        (first, second, "winner-one")
    } else {
        (second, first, "winner-two")
    };
    let winner = winner.unwrap();
    assert!(matches!(
        loser,
        Err(ApiError::Storage(StorageError::Conflict))
    ));
    let saved = storage.connection("basic").await.unwrap().record;
    assert_eq!(saved.config["password"], password);
    assert_eq!(saved.config["username"], "reader");
    assert_eq!(json!(saved.revision), winner.0["revision"]);
    server.verify().await;
    let listed = connections(State(state.clone())).await.unwrap().0;
    for connection in listed["connections"].as_array().unwrap() {
        for secret in ["username", "password", "api_key"] {
            assert!(connection["configuration"].get(secret).is_none());
        }
    }
    drop(state);
    drop(storage);
    let reopened = Storage::open(options).await.unwrap();
    let persisted = reopened.connection("basic").await.unwrap().record;
    assert_eq!(persisted.config, saved.config);
    assert_eq!(persisted.revision, saved.revision);
}
#[test]
fn names_endpoints_catalog_and_time_ranges_preserve_validation() {
    assert_eq!(connection_id("My Analytics").unwrap(), "my-analytics");
    assert!(connection_id("../secret").is_err());
    assert_eq!(
        safe_endpoint("https://user:secret@example.com/path?key=secret#secret"),
        "https://example.com/path"
    );
    assert_eq!(safe_endpoint("private-invalid-url"), "");
    let ids: Vec<_> = registered().into_iter().map(|entry| entry.id).collect();
    assert!(ids.contains(&"elasticsearch") && ids.contains(&"clickhouse"));
    assert!(serde_json::from_value::<AddConnectionRequest>(
        json!({"name":"CH","connection_type":"clickhouse","url":"http://localhost:8123"})
    )
    .is_ok());
    for (field, start, end) in [
        (Some("created_at"), 2000, 1000),
        (None, 1000, 2000),
        (Some(""), 1000, 2000),
        (Some("created_at"), i64::MIN, 0),
    ] {
        let request: SearchRequest = serde_json::from_value(json!({"index":"events","histogram_field":field,"time_range":{"start":start,"end":end}})).unwrap();
        assert!(matches!(
            validate_search(&request),
            Err(ApiError::InvalidTimeRange)
        ));
    }
}
struct CapabilityConnector;
impl Connector for CapabilityConnector {
    fn test_connection(&self) -> crate::data_connectors::ConnectorFuture<'_, ()> {
        Box::pin(async { Ok(()) })
    }
    fn indices(
        &self,
    ) -> crate::data_connectors::ConnectorFuture<'_, Vec<crate::data_connectors::IndexSummary>>
    {
        Box::pin(async {
            Ok(vec![crate::data_connectors::IndexSummary {
                name: "events".to_owned(),
                document_count: None,
                health: None,
            }])
        })
    }
    fn fields<'a>(
        &'a self,
        index: &'a str,
    ) -> crate::data_connectors::ConnectorFuture<'a, Vec<crate::data_connectors::FieldSummary>>
    {
        Box::pin(async move {
            if !matches!(index, "events" | "events-*") {
                return Ok(Vec::new());
            }
            Ok(vec![
                crate::data_connectors::FieldSummary {
                    name: "clock".to_owned(),
                    field_type: "source_specific_clock".to_owned(),
                    is_time_field: true,
                },
                crate::data_connectors::FieldSummary {
                    name: "not_clock".to_owned(),
                    field_type: "date".to_owned(),
                    is_time_field: false,
                },
            ])
        })
    }
    fn search(&self, request: SearchRequest) -> crate::data_connectors::ConnectorFuture<'_, Value> {
        Box::pin(async move { Ok(json!({"query":request.query,"from":request.from})) })
    }
    fn document<'a>(
        &'a self,
        index: &'a str,
        id: &'a str,
    ) -> crate::data_connectors::ConnectorFuture<'a, Value> {
        Box::pin(async move { Ok(json!({"index":index,"id":id,"found":true})) })
    }
}
#[tokio::test]
async fn source_validation_uses_capabilities_not_type_names() {
    assert_eq!(
        validate_source(&CapabilityConnector, "events", "clock")
            .await
            .unwrap(),
        "clock"
    );
    assert_eq!(
        validate_source(&CapabilityConnector, "events-*", "clock")
            .await
            .unwrap(),
        "clock"
    );
    assert!(validate_source(&CapabilityConnector, "events", "not_clock")
        .await
        .is_err());
    assert!(validate_source(&CapabilityConnector, "missing", "clock")
        .await
        .is_err());
}
#[tokio::test]
#[ignore = "requires TEST_DATABASE_URL and CONFIG_ENCRYPTION_KEY"]
async fn api_preserves_redaction_authentication_preconditions_and_capacity() {
    let database = TestDatabase::new().await;
    let state = state(database.storage.clone());
    let server = MockServer::start().await;
    Mock::given(matchers::method("GET"))
        .and(matchers::path("/_cat/indices"))
        .and(matchers::header("authorization", "ApiKey private-api-key"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!([])))
        .expect(1)
        .mount(&server)
        .await;
    Mock::given(matchers::method("GET"))
        .and(matchers::path("/_cat/indices"))
        .and(matchers::header(
            "authorization",
            "Basic cmVwbGFjZW1lbnQ6cmVwbGFjZW1lbnQ=",
        ))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!([])))
        .expect(1)
        .mount(&server)
        .await;
    assert!(matches!(
        add_connection(
            State(state.clone()),
            Json(
                serde_json::from_value(
                    json!({"name":"Unknown","connection_type":"unknown","url":server.uri()})
                )
                .unwrap()
            )
        )
        .await,
        Err(ApiError::InvalidConnection)
    ));
    let added = add_connection(State(state.clone()), Json(serde_json::from_value(json!({"name":"ES","connection_type":"elasticsearch","url":server.uri(),
        "auth_type":"api_key","api_key":"private-api-key","tls_insecure":false,"unlisted_secret":"private-unlisted"})).unwrap())).await.unwrap();
    let original = added.0["revision"].as_str().unwrap();
    let edit = || {
        Json(serde_json::from_value(json!({"name":"ES Renamed","url":server.uri(),"api_key":"replacement","username":"replacement","password":"replacement",
        "auth_type":"basic","tls_insecure":true,"connection_type":"clickhouse","unlisted_secret":"replacement"})).unwrap())
    };
    assert!(matches!(
        update_connection(
            State(state.clone()),
            Path("es".to_owned()),
            HeaderMap::new(),
            edit()
        )
        .await,
        Err(ApiError::PreconditionRequired)
    ));
    let updated = update_connection(
        State(state.clone()),
        Path("es".to_owned()),
        headers(original),
        edit(),
    )
    .await
    .unwrap();
    let saved = get_connection(&state, "es-renamed").await.unwrap();
    assert!(!saved.record.config.contains_key("api_key"));
    assert_eq!(saved.record.config["auth_type"], "basic");
    assert_eq!(saved.record.config["username"], "replacement");
    assert_eq!(saved.record.config["password"], "replacement");
    assert_eq!(saved.record.config["tls_insecure"], false);
    assert_eq!(saved.record.config["unlisted_secret"], "private-unlisted");
    let listed = connections(State(state.clone())).await.unwrap().0;
    assert!(!listed.to_string().contains("private-api-key"));
    assert!(!listed.to_string().contains("private-unlisted"));
    assert!(listed["connections"][0]["configuration"]
        .get("username")
        .is_none());
    assert!(listed["connections"][0]["configuration"]
        .get("password")
        .is_none());
    assert_eq!(listed["connections"][0]["revision"], updated.0["revision"]);
    assert!(matches!(
        remove_connection(
            State(state.clone()),
            Path("es-renamed".to_owned()),
            headers(original)
        )
        .await,
        Err(ApiError::Storage(StorageError::Conflict))
    ));
    let single = AppState {
        storage: database.storage.clone(),
        upstream_slots: Arc::new(Semaphore::new(1)),
        ai_slots: Arc::new(Semaphore::new(1)),
        investigations: Arc::default(),
    };
    let permit = acquire(&single).await.unwrap();
    assert!(matches!(
        connection_indices(State(single.clone()), Path("missing".to_owned())).await,
        Err(ApiError::Unavailable)
    ));
    drop(permit);
    remove_connection(
        State(state),
        Path("es-renamed".to_owned()),
        headers(updated.0["revision"].as_str().unwrap()),
    )
    .await
    .unwrap();
    database.finish().await;
}
#[tokio::test]
#[ignore = "requires TEST_DATABASE_URL and CONFIG_ENCRYPTION_KEY"]
async fn api_views_validate_before_writes_and_cascade_delete() {
    let database = TestDatabase::new().await;
    let state = state(database.storage.clone());
    let server = MockServer::start().await;
    Mock::given(matchers::method("GET"))
        .and(matchers::path("/_cat/indices"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_json(json!([{"index":"logs","docs.count":"10","health":"green"}])),
        )
        .mount(&server)
        .await;
    Mock::given(matchers::method("GET")).and(matchers::path("/logs/_mapping"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"logs":{"mappings":{"properties":{"timestamp":{"type":"date"},"message":{"type":"keyword"}}}}}))).mount(&server).await;
    let source_revision = database
        .storage
        .create_connection(StoredConnection {
            id: "source".to_owned(),
            name: "Source".to_owned(),
            integration_id: "elasticsearch".to_owned(),
            config: serde_json::from_value(json!({"url":server.uri()})).unwrap(),
            revision: String::new(),
        })
        .await
        .unwrap();
    for (source, index, field) in [
        ("missing", "logs", "timestamp"),
        ("source", "missing", "timestamp"),
        ("source", "logs", "message"),
    ] {
        assert!(create_data_view(
            State(state.clone()),
            Json(CreateDataViewRequest {
                name: "Logs view".to_owned(),
                connection_id: source.to_owned(),
                index: index.to_owned(),
                time_field: field.to_owned()
            })
        )
        .await
        .is_err());
        assert!(database.storage.views().await.unwrap().is_empty());
    }
    let request = || {
        Json(CreateDataViewRequest {
            name: "Logs view".to_owned(),
            connection_id: "source".to_owned(),
            index: "logs".to_owned(),
            time_field: "timestamp".to_owned(),
        })
    };
    let created = create_data_view(State(state.clone()), request())
        .await
        .unwrap();
    let revision = created.0["revision"].as_str().unwrap();
    assert!(create_data_view(State(state.clone()), request())
        .await
        .is_err());
    let updated = update_data_view(
        State(state.clone()),
        Path("logs-view".to_owned()),
        headers(revision),
        request(),
    )
    .await
    .unwrap();
    assert!(matches!(
        remove_data_view(
            State(state.clone()),
            Path("logs-view".to_owned()),
            headers(revision)
        )
        .await,
        Err(ApiError::Storage(StorageError::Conflict))
    ));
    remove_data_view(
        State(state.clone()),
        Path("logs-view".to_owned()),
        headers(updated.0["revision"].as_str().unwrap()),
    )
    .await
    .unwrap();
    let _ = create_data_view(State(state.clone()), request())
        .await
        .unwrap();
    remove_connection(
        State(state),
        Path("source".to_owned()),
        headers(&source_revision),
    )
    .await
    .unwrap();
    assert!(database.storage.views().await.unwrap().is_empty());
    database.finish().await;
}
#[tokio::test]
#[ignore = "requires TEST_DATABASE_URL and CONFIG_ENCRYPTION_KEY"]
async fn clickhouse_edits_retain_credentials_and_allow_blank_passwords() {
    let database = TestDatabase::new().await;
    let state = state(database.storage.clone());
    let server = MockServer::start().await;
    Mock::given(matchers::method("POST"))
        .and(matchers::body_string("SELECT 1 FORMAT JSON"))
        .and(matchers::header("authorization", "Basic ZXhwbG9yZXI6"))
        .respond_with(
            ResponseTemplate::new(200).set_body_json(json!({"meta":[],"data":[{"1":1}],"rows":1})),
        )
        .expect(3)
        .mount(&server)
        .await;
    Mock::given(matchers::method("POST"))
        .and(matchers::body_string("SELECT 1 FORMAT JSON"))
        .and(matchers::header(
            "authorization",
            "Basic b3RoZXI6Y2hhbmdlZA==",
        ))
        .respond_with(
            ResponseTemplate::new(200).set_body_json(json!({"meta":[],"data":[],"rows":0})),
        )
        .expect(1)
        .mount(&server)
        .await;
    for password in [Some(""), None] {
        let name = if password.is_some() {
            "Blank"
        } else {
            "Omitted"
        };
        let _ = add_connection(State(state.clone()),Json(serde_json::from_value(json!({"name":name,"connection_type":"clickhouse","url":server.uri(),"username":"explorer","password":password,"database":"analytics"})).unwrap())).await.unwrap();
    }
    for (changes, expected) in [
        (json!({}), json!("analytics")),
        (
            json!({"database":null,"username":"other","password":"changed"}),
            Value::Null,
        ),
    ] {
        let saved = get_connection(&state, "blank").await.unwrap();
        let mut request = changes.as_object().unwrap().clone();
        request.insert("name".to_owned(), json!("Blank"));
        request.insert("url".to_owned(), json!(server.uri()));
        let _ = update_connection(
            State(state.clone()),
            Path("blank".to_owned()),
            headers(&saved.record.revision),
            Json(serde_json::from_value(Value::Object(request)).unwrap()),
        )
        .await
        .unwrap();
        let saved = get_connection(&state, "blank").await.unwrap();
        assert_eq!(saved.record.config["database"], expected);
        assert_eq!(
            saved.record.config["username"],
            if expected.is_null() {
                "other"
            } else {
                "explorer"
            }
        );
        assert_eq!(
            saved.record.config["password"],
            if expected.is_null() { "changed" } else { "" }
        );
    }
    let listed = connections(State(state)).await.unwrap().0;
    assert!(listed["connections"][0]["configuration"]
        .get("password")
        .is_none());
    database.finish().await;
}

#[tokio::test]
#[ignore = "requires TEST_DATABASE_URL and CONFIG_ENCRYPTION_KEY"]
async fn http_routes_enforce_preconditions_cors_settings_and_health() {
    let database = TestDatabase::new().await;
    let app = router(
        "http://localhost:5173".parse().unwrap(),
        database.storage.clone(),
    );
    for path in ["/api/connections/missing", "/api/data-views/missing"] {
        let request = axum::http::Request::builder()
            .method("DELETE")
            .uri(path)
            .body(axum::body::Body::empty())
            .unwrap();
        assert_eq!(
            app.clone().oneshot(request).await.unwrap().status(),
            StatusCode::PRECONDITION_REQUIRED
        );
    }
    let request = axum::http::Request::builder()
        .method("OPTIONS")
        .uri("/api/settings")
        .header("origin", "http://localhost:5173")
        .header("access-control-request-method", "PUT")
        .header("access-control-request-headers", "content-type,if-match")
        .body(axum::body::Body::empty())
        .unwrap();
    let response = app.clone().oneshot(request).await.unwrap();
    assert!(response.headers()["access-control-allow-headers"]
        .to_str()
        .unwrap()
        .contains("if-match"));
    let settings = database.storage.settings().await.unwrap();
    let request = axum::http::Request::builder()
        .method("PUT")
        .uri("/api/settings")
        .header("content-type", "application/json")
        .header("if-match", format!("\"{}\"", settings.revision))
        .body(axum::body::Body::from(
            r#"{"settings":{"theme":"dark","default_timezone":"UTC"}}"#,
        ))
        .unwrap();
    assert_eq!(
        app.clone().oneshot(request).await.unwrap().status(),
        StatusCode::OK
    );
    let request = axum::http::Request::builder()
        .method("PUT")
        .uri("/api/settings")
        .header("content-type", "application/json")
        .header("if-match", format!("\"{}\"", settings.revision))
        .body(axum::body::Body::from(r#"{"settings":{"theme":"light"}}"#))
        .unwrap();
    assert_eq!(
        app.clone().oneshot(request).await.unwrap().status(),
        StatusCode::CONFLICT
    );
    let request = axum::http::Request::builder()
        .uri("/api/health/ready")
        .body(axum::body::Body::empty())
        .unwrap();
    assert_eq!(
        app.clone().oneshot(request).await.unwrap().status(),
        StatusCode::OK
    );
    database.storage.close().await;
    for path in ["/api/health", "/api/health/live", "/api/health/ready"] {
        let request = axum::http::Request::builder()
            .uri(path)
            .body(axum::body::Body::empty())
            .unwrap();
        assert_eq!(
            app.clone().oneshot(request).await.unwrap().status(),
            if path.ends_with("ready") {
                StatusCode::SERVICE_UNAVAILABLE
            } else {
                StatusCode::OK
            }
        );
    }
    database.finish().await;
}
