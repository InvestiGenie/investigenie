use super::*;

pub struct TestDatabase {
    pub options: StorageOptions,
    pub schema: String,
    pub storage: Storage,
    admin: PgPool,
}
impl TestDatabase {
    pub async fn new() -> Self {
        let options = StorageOptions {
            database_url: std::env::var("TEST_DATABASE_URL").expect(
                "ignored PostgreSQL tests require TEST_DATABASE_URL; no tests are silently skipped",
            ),
            encryption_key: std::env::var("CONFIG_ENCRYPTION_KEY")
                .expect("ignored PostgreSQL tests require CONFIG_ENCRYPTION_KEY"),
            key_id: "test-v1".to_owned(),
            max_connections: 4,
        };
        let schema = format!("investigenie_test_{}", Uuid::new_v4().simple());
        let admin = Storage::pool(&options, None).await.unwrap();
        sqlx::query(&format!("CREATE SCHEMA {schema}"))
            .execute(&admin)
            .await
            .unwrap();
        let pool = Storage::pool(&options, Some(&schema)).await.unwrap();
        MIGRATOR.run(&pool).await.unwrap();
        let storage = Storage::from_pool(
            pool,
            Encryption::new(&options.encryption_key, &options.key_id).unwrap(),
        )
        .await
        .unwrap();
        Self {
            options,
            schema,
            storage,
            admin,
        }
    }
    pub async fn replica(&self) -> Storage {
        Storage::from_pool(
            Storage::pool(&self.options, Some(&self.schema))
                .await
                .unwrap(),
            Encryption::new(&self.options.encryption_key, &self.options.key_id).unwrap(),
        )
        .await
        .unwrap()
    }
    pub async fn finish(self) {
        self.storage.close().await;
        sqlx::query(&format!("DROP SCHEMA {} CASCADE", self.schema))
            .execute(&self.admin)
            .await
            .unwrap();
        self.admin.close().await;
    }
}
fn record(id: &str) -> StoredConnection {
    StoredConnection { id: id.to_owned(), name: id.to_owned(), integration_id: "clickhouse".to_owned(),
        config: serde_json::from_value(json!({"url":"http://127.0.0.1:1", "username":"explorer", "password":"private-test-password"})).unwrap(), revision: String::new() }
}
fn view(id: &str, source: &str) -> DataView {
    DataView {
        id: id.to_owned(),
        name: id.to_owned(),
        connection_id: source.to_owned(),
        index: "events".to_owned(),
        time_field: "timestamp".to_owned(),
        revision: String::new(),
    }
}
#[test]
fn encryption_authenticates_nonce_key_id_integration_and_ciphertext() {
    let encryption = Encryption::new(&STANDARD.encode([7u8; 32]), "v1").unwrap();
    let first = encryption.encrypt("clickhouse", b"private-secret").unwrap();
    let second = encryption.encrypt("clickhouse", b"private-secret").unwrap();
    assert_ne!(first, second);
    assert!(!String::from_utf8_lossy(&first).contains("private-secret"));
    assert_eq!(
        encryption.decrypt("clickhouse", &first).unwrap(),
        b"private-secret"
    );
    let legacy = encryption.encrypt_legacy_fixture("clickhouse", b"legacy-private-secret");
    assert_eq!(
        encryption.decrypt("clickhouse", &legacy).unwrap(),
        b"legacy-private-secret"
    );
    let legacy_verifier_ciphertext =
        encryption.encrypt_legacy_fixture(VERIFIER_ID, &legacy_verifier());
    assert_eq!(
        encryption
            .decrypt(VERIFIER_ID, &legacy_verifier_ciphertext)
            .unwrap(),
        legacy_verifier()
    );
    assert!(encryption.decrypt("elasticsearch", &first).is_err());
    assert!(Encryption::new(&STANDARD.encode([8u8; 32]), "v1")
        .unwrap()
        .decrypt("clickhouse", &first)
        .is_err());
    assert!(Encryption::new(&STANDARD.encode([7u8; 32]), "v2")
        .unwrap()
        .decrypt("clickhouse", &first)
        .is_err());
    let mut envelope: Envelope = serde_json::from_slice(&first).unwrap();
    envelope.ciphertext[0] ^= 1;
    assert!(encryption
        .decrypt("clickhouse", &serde_json::to_vec(&envelope).unwrap())
        .is_err());
    envelope.nonce.clear();
    assert!(encryption
        .decrypt("clickhouse", &serde_json::to_vec(&envelope).unwrap())
        .is_err());
    assert!(Encryption::new("not-base64", "v1").is_err());
    assert!(Encryption::new(&STANDARD.encode([1u8; 31]), "v1").is_err());
}
#[test]
fn settings_allow_only_bounded_public_options() {
    for value in [
        json!({}),
        json!({"theme":"dark","default_timezone":"America/New_York"}),
    ] {
        assert!(validate_settings(&value).is_ok());
    }
    for value in [
        json!([]),
        json!({"database_url":"private"}),
        json!({"theme":{"password":"private"}}),
        json!({"CONFIG_ENCRYPTION_KEY":"private"}),
        json!({"default_timezone":"unknown"}),
        json!({"theme":"invalid"}),
        json!({"theme":null}),
    ] {
        assert!(validate_settings(&value).is_err());
    }
    assert!(validate_settings(&json!({"theme":"a".repeat(65536)})).is_err());
}
#[tokio::test]
#[ignore = "requires isolated PostgreSQL schema via TEST_DATABASE_URL and CONFIG_ENCRYPTION_KEY"]
async fn replicas_restart_visibility_revision_cascades_and_aba() {
    let database = TestDatabase::new().await;
    let first = &database.storage;
    let second = database.replica().await;
    let original = first.create_connection(record("source")).await.unwrap();
    assert_eq!(
        second.connection("source").await.unwrap().record.revision,
        original
    );
    let view_revision = first
        .save_view(view("events", "source"), &original, None)
        .await
        .unwrap();
    let mut changed = record("source");
    changed
        .config
        .insert("database".to_owned(), json!("analytics"));
    let changed_revision = second
        .update_connection("source", &original, changed)
        .await
        .unwrap();
    assert_eq!(
        first.connection("source").await.unwrap().record.config["database"],
        "analytics"
    );
    assert!(matches!(
        first
            .update_connection("source", &original, record("renamed"))
            .await,
        Err(StorageError::Conflict)
    ));
    assert!(matches!(
        first.remove_connection("source", &original).await,
        Err(StorageError::Conflict)
    ));
    assert!(matches!(
        first
            .save_view(view("stale", "source"), &original, None)
            .await,
        Err(StorageError::Conflict)
    ));
    let renamed_revision = first
        .update_connection("source", &changed_revision, record("renamed"))
        .await
        .unwrap();
    let views = second.views().await.unwrap();
    assert_eq!(views[0].connection_id, "renamed");
    assert_ne!(views[0].revision, view_revision);
    assert!(matches!(
        second.remove_view("events", &view_revision).await,
        Err(StorageError::Conflict)
    ));
    let edited = first
        .save_view(
            view("events", "renamed"),
            &renamed_revision,
            Some(&views[0].revision),
        )
        .await
        .unwrap();
    assert!(matches!(
        second
            .save_view(
                view("events", "renamed"),
                &renamed_revision,
                Some(&views[0].revision)
            )
            .await,
        Err(StorageError::Conflict)
    ));
    second.remove_view("events", &edited).await.unwrap();
    assert!(first.views().await.unwrap().is_empty());
    first
        .save_view(view("again", "renamed"), &renamed_revision, None)
        .await
        .unwrap();
    second
        .remove_connection("renamed", &renamed_revision)
        .await
        .unwrap();
    assert!(first.views().await.unwrap().is_empty());
    second.create_connection(record("renamed")).await.unwrap();
    assert!(matches!(
        first.remove_connection("renamed", &renamed_revision).await,
        Err(StorageError::Conflict)
    ));
    second.close().await;
    let restarted = database.replica().await;
    assert_eq!(restarted.connections().await.unwrap().len(), 1);
    restarted.close().await;
    database.finish().await;
}
#[tokio::test]
#[ignore = "requires isolated PostgreSQL schema via TEST_DATABASE_URL and CONFIG_ENCRYPTION_KEY"]
async fn encrypted_at_rest_wrong_key_empty_database_and_tampering_fail_closed() {
    let database = TestDatabase::new().await;
    let wrong = Encryption::new(&STANDARD.encode([99u8; 32]), &database.options.key_id).unwrap();
    assert!(matches!(
        Storage::from_pool(
            Storage::pool(&database.options, Some(&database.schema))
                .await
                .unwrap(),
            wrong.clone()
        )
        .await,
        Err(StorageError::Encryption)
    ));
    database
        .storage
        .create_connection(record("source"))
        .await
        .unwrap();
    let bytes: Vec<u8> =
        sqlx::query_scalar("SELECT config_encrypted FROM connections WHERE id='source'")
            .fetch_one(&database.storage.pool)
            .await
            .unwrap();
    assert!(!String::from_utf8_lossy(&bytes).contains("private-test-password"));
    assert!(matches!(
        Storage::from_pool(
            Storage::pool(&database.options, Some(&database.schema))
                .await
                .unwrap(),
            wrong
        )
        .await,
        Err(StorageError::Encryption)
    ));
    let mut envelope: Envelope = serde_json::from_slice(&bytes).unwrap();
    envelope.ciphertext[0] ^= 1;
    sqlx::query("UPDATE connections SET config_encrypted=$1 WHERE id='source'")
        .bind(serde_json::to_vec(&envelope).unwrap())
        .execute(&database.storage.pool)
        .await
        .unwrap();
    assert!(matches!(
        database.storage.connection("source").await,
        Err(StorageError::Encryption)
    ));
    database.finish().await;
}
#[tokio::test]
#[ignore = "requires isolated PostgreSQL schema via TEST_DATABASE_URL and CONFIG_ENCRYPTION_KEY"]
async fn concurrent_unique_writes_settings_and_rollback_are_atomic() {
    let database = TestDatabase::new().await;
    let other = database.replica().await;
    let (first, second) = tokio::join!(
        database.storage.create_connection(record("source")),
        other.create_connection(record("source"))
    );
    assert_eq!(usize::from(first.is_ok()) + usize::from(second.is_ok()), 1);
    assert!(
        matches!(first, Err(StorageError::Duplicate))
            || matches!(second, Err(StorageError::Duplicate))
    );
    let revision = database
        .storage
        .connection("source")
        .await
        .unwrap()
        .record
        .revision;
    let view_revision = database
        .storage
        .save_view(view("events", "source"), &revision, None)
        .await
        .unwrap();
    database
        .storage
        .create_connection(record("occupied"))
        .await
        .unwrap();
    assert!(matches!(
        database
            .storage
            .update_connection("source", &revision, record("occupied"))
            .await,
        Err(StorageError::Duplicate)
    ));
    assert_eq!(
        database.storage.views().await.unwrap()[0].revision,
        view_revision
    );
    assert_eq!(
        database
            .storage
            .connection("source")
            .await
            .unwrap()
            .record
            .revision,
        revision
    );
    let mut duplicate = view("other", "source");
    duplicate.name = "EVENTS".to_owned();
    assert!(matches!(
        other.save_view(duplicate, &revision, None).await,
        Err(StorageError::Duplicate)
    ));
    let settings = database.storage.settings().await.unwrap();
    let (first, second) = tokio::join!(
        database
            .storage
            .save_settings(json!({"theme":"dark"}), &settings.revision),
        other.save_settings(json!({"theme":"light"}), &settings.revision)
    );
    assert_eq!(usize::from(first.is_ok()) + usize::from(second.is_ok()), 1);
    assert!(
        matches!(first, Err(StorageError::Conflict))
            || matches!(second, Err(StorageError::Conflict))
    );
    assert_eq!(
        database.storage.settings().await.unwrap(),
        other.settings().await.unwrap()
    );
    assert!(database.storage.ready().await.is_ok());
    sqlx::query("UPDATE _sqlx_migrations SET success=false")
        .execute(&other.pool)
        .await
        .unwrap();
    assert!(matches!(
        database.storage.ready().await,
        Err(StorageError::Schema)
    ));
    other.close().await;
    assert!(other.ready().await.is_err());
    database.finish().await;
}
