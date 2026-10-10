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
    }
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
    fn test_connection(&self) -> crate::integrations::ConnectorFuture<'_, ()> {
        Box::pin(async { Ok(()) })
    }
    fn indices(
        &self,
    ) -> crate::integrations::ConnectorFuture<'_, Vec<crate::integrations::IndexSummary>> {
        Box::pin(async {
            Ok(vec![crate::integrations::IndexSummary {
                name: "events".to_owned(),
                document_count: None,
                health: None,
            }])
        })
    }
    fn fields<'a>(
        &'a self,
        _: &'a str,
    ) -> crate::integrations::ConnectorFuture<'a, Vec<crate::integrations::FieldSummary>> {
        Box::pin(async {
            Ok(vec![
                crate::integrations::FieldSummary {
                    name: "clock".to_owned(),
                    field_type: "source_specific_clock".to_owned(),
                    is_time_field: true,
                },
                crate::integrations::FieldSummary {
                    name: "not_clock".to_owned(),
                    field_type: "date".to_owned(),
                    is_time_field: false,
                },
            ])
        })
    }
    fn search(&self, request: SearchRequest) -> crate::integrations::ConnectorFuture<'_, Value> {
        Box::pin(async move { Ok(json!({"query":request.query,"from":request.from})) })
    }
    fn document<'a>(
        &'a self,
        index: &'a str,
        id: &'a str,
    ) -> crate::integrations::ConnectorFuture<'a, Value> {
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
