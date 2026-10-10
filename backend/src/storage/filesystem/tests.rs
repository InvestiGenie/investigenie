use super::*;

fn options(path: &Path) -> PersistenceOptions {
    PersistenceOptions::Filesystem {
        path: path.to_owned(),
        key: STANDARD.encode([7u8; 32]),
        key_id: "v1".to_owned(),
    }
}
fn record(id: &str) -> StoredConnection {
    StoredConnection { id: id.to_owned(), name: id.to_owned(), integration_id: "clickhouse".to_owned(),
        config: serde_json::from_value(json!({"url":"http://127.0.0.1:1", "username":"explorer", "password":"private-test-password"})).unwrap(), revision: String::new() }
}
fn view(id: &str, connection_id: &str) -> DataView {
    DataView {
        id: id.to_owned(),
        name: id.to_owned(),
        connection_id: connection_id.to_owned(),
        index: "events".to_owned(),
        time_field: "timestamp".to_owned(),
        revision: String::new(),
    }
}

#[tokio::test]
async fn lifecycle_reopen_custom_path_settings_and_encrypted_bytes() {
    let directory = tempfile::tempdir().unwrap();
    let root = directory.path().join("custom/nested");
    Storage::migrate(options(&root)).await.unwrap();
    let storage = Storage::open(options(&root)).await.unwrap();
    let original_bytes = fs::read(root.join(SNAPSHOT_NAME)).unwrap();
    let connection_revision = storage.create_connection(record("source")).await.unwrap();
    let view_revision = storage
        .save_view(view("events", "source"), &connection_revision, None)
        .await
        .unwrap();
    let settings = storage.settings().await.unwrap();
    let settings_revision = storage
        .save_settings(
            json!({"theme":"dark", "default_timezone":"UTC"}),
            &settings.revision,
        )
        .await
        .unwrap();
    assert!(matches!(
        storage
            .save_settings(json!({"theme":"light"}), &settings.revision)
            .await,
        Err(StorageError::Conflict)
    ));
    assert!(matches!(
        storage
            .save_settings(json!({"password":"secret"}), &settings_revision)
            .await,
        Err(StorageError::Invalid)
    ));
    storage.ready().await.unwrap();
    let bytes = fs::read(root.join(SNAPSHOT_NAME)).unwrap();
    assert_ne!(original_bytes, bytes);
    for secret in [
        "private-test-password",
        "explorer",
        "events",
        "theme",
        "127.0.0.1",
    ] {
        assert!(!String::from_utf8_lossy(&bytes).contains(secret));
    }
    assert_eq!(fs::read_dir(&root).unwrap().count(), 2);
    drop(storage);
    let storage = Storage::open(options(&root)).await.unwrap();
    assert_eq!(
        storage.connection("source").await.unwrap().record.revision,
        connection_revision
    );
    assert_eq!(storage.views().await.unwrap()[0].revision, view_revision);
    assert_eq!(
        storage.settings().await.unwrap().revision,
        settings_revision
    );
    storage.remove_view("events", &view_revision).await.unwrap();
    storage
        .save_view(view("again", "source"), &connection_revision, None)
        .await
        .unwrap();
    storage
        .remove_connection("source", &connection_revision)
        .await
        .unwrap();
    assert!(storage.views().await.unwrap().is_empty());
    drop(storage);
    let storage = Storage::open(options(&root)).await.unwrap();
    assert!(storage.connections().await.unwrap().is_empty());
}

#[tokio::test]
async fn ai_model_secrets_are_encrypted_and_profiles_survive_reopen() {
    let directory = tempfile::tempdir().unwrap();
    let storage = Storage::open(options(directory.path())).await.unwrap();
    let model = storage
        .create_ai_model(StoredAiModel {
            id: String::new(),
            revision: String::new(),
            name: "Private provider".to_owned(),
            api_url: "https://provider.example/v1".to_owned(),
            model: "private-model".to_owned(),
            api_key: "filesystem-provider-secret".to_owned(),
        })
        .await
        .unwrap();
    let path = directory.path().join(SNAPSHOT_NAME);
    let bytes = fs::read(&path).unwrap();
    let stored = String::from_utf8_lossy(&bytes);
    for secret in [
        "filesystem-provider-secret",
        "Private provider",
        "provider.example",
        "private-model",
    ] {
        assert!(!stored.contains(secret));
    }
    drop(storage);
    let reopened = Storage::open(options(directory.path())).await.unwrap();
    let restored = reopened.ai_model(&model.id).await.unwrap();
    assert_eq!(restored.api_key, "filesystem-provider-secret");
    assert_eq!(restored.revision, model.revision);
}

#[test]
fn version_one_snapshots_without_ai_models_remain_readable() {
    let mut snapshot = serde_json::to_value(Snapshot::empty()).unwrap();
    snapshot.as_object_mut().unwrap().remove("ai_models");
    let snapshot: Snapshot = serde_json::from_value(snapshot).unwrap();
    assert!(snapshot.ai_models.is_empty());
    assert!(snapshot.validate().is_ok());
}

#[tokio::test]
async fn legacy_encrypted_snapshots_are_migrated_to_current_identifiers() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("legacy-store");
    fs::create_dir_all(&path).unwrap();
    let encryption = Encryption::new(&STANDARD.encode([7u8; 32]), "v1").unwrap();
    let mut snapshot = Snapshot::empty();
    snapshot.verifier = String::from_utf8(legacy_verifier()).unwrap();
    let bytes = encryption.encrypt_legacy_fixture(
        SNAPSHOT_ID,
        &serde_json::to_vec(&snapshot).unwrap(),
    );
    fs::write(path.join(SNAPSHOT_NAME), bytes).unwrap();

    let storage = Storage::open(options(&path)).await.unwrap();
    let migrated_bytes = fs::read(path.join(SNAPSHOT_NAME)).unwrap();
    let plaintext = encryption.decrypt(SNAPSHOT_ID, &migrated_bytes).unwrap();
    let migrated: Snapshot = serde_json::from_slice(&plaintext).unwrap();
    assert_eq!(migrated.verifier.as_bytes(), VERIFIER);
    assert_eq!(storage.connections().await.unwrap().len(), 0);
}

#[tokio::test]
async fn locks_survive_close_and_clones_until_last_drop() {
    let directory = tempfile::tempdir().unwrap();
    let storage = Storage::open(options(directory.path())).await.unwrap();
    let clone = storage.clone();
    storage.close().await;
    drop(storage);
    assert!(matches!(
        Storage::open(options(directory.path())).await,
        Err(StorageError::Unavailable)
    ));
    drop(clone);
    Storage::open(options(directory.path())).await.unwrap();
}

#[tokio::test]
async fn conflicts_aba_rename_cascade_duplicates_and_rollback() {
    let directory = tempfile::tempdir().unwrap();
    let storage = Storage::open(options(directory.path())).await.unwrap();
    let original = storage.create_connection(record("source")).await.unwrap();
    let view_revision = storage
        .save_view(view("events", "source"), &original, None)
        .await
        .unwrap();
    storage.create_connection(record("occupied")).await.unwrap();
    let bytes = fs::read(directory.path().join(SNAPSHOT_NAME)).unwrap();
    assert!(matches!(
        storage
            .update_connection("source", &original, record("occupied"))
            .await,
        Err(StorageError::Duplicate)
    ));
    assert_eq!(storage.views().await.unwrap()[0].revision, view_revision);
    let mut duplicate = view("duplicate", "source");
    duplicate.name = "EVENTS".to_owned();
    assert!(matches!(
        storage.save_view(duplicate, &original, None).await,
        Err(StorageError::Duplicate)
    ));
    assert!(matches!(
        storage
            .save_view(view("events", "source"), &original, None)
            .await,
        Err(StorageError::Duplicate)
    ));
    assert_eq!(
        fs::read(directory.path().join(SNAPSHOT_NAME)).unwrap(),
        bytes
    );
    let updated = storage
        .update_connection("source", &original, record("renamed"))
        .await
        .unwrap();
    let views = storage.views().await.unwrap();
    assert_eq!(views[0].connection_id, "renamed");
    assert_ne!(views[0].revision, view_revision);
    assert!(matches!(
        storage.remove_view("events", &view_revision).await,
        Err(StorageError::Conflict)
    ));
    assert!(matches!(
        storage
            .save_view(view("new", "renamed"), &original, None)
            .await,
        Err(StorageError::Conflict)
    ));
    assert!(matches!(
        storage.remove_connection("renamed", &original).await,
        Err(StorageError::Conflict)
    ));
    let edited = storage
        .save_view(
            view("events", "renamed"),
            &updated,
            Some(&views[0].revision),
        )
        .await
        .unwrap();
    assert_ne!(edited, views[0].revision);
    storage.remove_view("events", &edited).await.unwrap();
    let recreated_view = storage
        .save_view(view("events", "renamed"), &updated, None)
        .await
        .unwrap();
    assert_ne!(edited, recreated_view);
    assert!(matches!(
        storage.remove_view("events", &edited).await,
        Err(StorageError::Conflict)
    ));
    assert!(matches!(
        storage
            .save_view(view("events", "renamed"), &updated, Some(&edited))
            .await,
        Err(StorageError::Conflict)
    ));
    storage
        .remove_connection("renamed", &updated)
        .await
        .unwrap();
    let recreated = storage.create_connection(record("renamed")).await.unwrap();
    assert_ne!(updated, recreated);
    assert!(matches!(
        storage.remove_connection("renamed", &updated).await,
        Err(StorageError::Conflict)
    ));
}

#[tokio::test]
async fn failed_replacement_does_not_publish_memory_or_leave_temporary_files() {
    let directory = tempfile::tempdir().unwrap();
    let storage = Storage::open(options(directory.path())).await.unwrap();
    let path = directory.path().join(SNAPSHOT_NAME);
    let original = fs::read(&path).unwrap();
    fs::remove_file(&path).unwrap();
    fs::create_dir(&path).unwrap();
    assert!(matches!(
        storage.create_connection(record("source")).await,
        Err(StorageError::Unavailable)
    ));
    assert!(storage.connections().await.unwrap().is_empty());
    assert_eq!(fs::read_dir(directory.path()).unwrap().count(), 2);
    fs::remove_dir(&path).unwrap();
    fs::write(&path, original).unwrap();
    storage.create_connection(record("source")).await.unwrap();
    drop(storage);
    assert_eq!(
        Storage::open(options(directory.path()))
            .await
            .unwrap()
            .connections()
            .await
            .unwrap()
            .len(),
        1
    );
}

#[tokio::test]
async fn wrong_key_and_key_id_fail_even_for_empty_snapshots_without_overwrite() {
    let directory = tempfile::tempdir().unwrap();
    drop(Storage::open(options(directory.path())).await.unwrap());
    let path = directory.path().join(SNAPSHOT_NAME);
    let original = fs::read(&path).unwrap();
    for (key, key_id) in [
        (STANDARD.encode([8u8; 32]), "v1"),
        (STANDARD.encode([7u8; 32]), "v2"),
    ] {
        let wrong = PersistenceOptions::Filesystem {
            path: directory.path().to_owned(),
            key,
            key_id: key_id.to_owned(),
        };
        assert!(matches!(
            Storage::open(wrong).await,
            Err(StorageError::Encryption)
        ));
        assert_eq!(fs::read(&path).unwrap(), original);
    }
    Storage::open(options(directory.path())).await.unwrap();
}

#[tokio::test]
async fn tampering_and_truncation_fail_closed_on_open_and_mutation() {
    let directory = tempfile::tempdir().unwrap();
    let storage = Storage::open(options(directory.path())).await.unwrap();
    let path = directory.path().join(SNAPSHOT_NAME);
    let original = fs::read(&path).unwrap();
    let mut envelope: Envelope = serde_json::from_slice(&original).unwrap();
    envelope.ciphertext[0] ^= 1;
    let tampered = serde_json::to_vec(&envelope).unwrap();
    fs::write(&path, &tampered).unwrap();
    assert!(matches!(
        storage.ready().await,
        Err(StorageError::Encryption)
    ));
    assert!(matches!(
        storage.create_connection(record("source")).await,
        Err(StorageError::Encryption)
    ));
    assert!(storage.connections().await.unwrap().is_empty());
    assert_eq!(fs::read(&path).unwrap(), tampered);
    drop(storage);
    for bytes in [
        tampered,
        original[..original.len() / 2].to_vec(),
        Vec::new(),
    ] {
        fs::write(&path, &bytes).unwrap();
        assert!(matches!(
            Storage::open(options(directory.path())).await,
            Err(StorageError::Encryption)
        ));
        assert_eq!(fs::read(&path).unwrap(), bytes);
    }
}

#[tokio::test]
async fn invalid_snapshot_schema_records_references_names_and_settings_are_not_overwritten() {
    let directory = tempfile::tempdir().unwrap();
    let encryption = Encryption::new(&STANDARD.encode([7u8; 32]), "v1").unwrap();
    drop(Storage::open(options(directory.path())).await.unwrap());
    let mut snapshot = Snapshot::empty();
    let mut connection = record("source");
    connection.revision = revision();
    snapshot
        .connections
        .insert(connection.id.clone(), connection);
    let mut events = view("events", "source");
    events.revision = revision();
    snapshot.views.insert(events.id.clone(), events);
    let mut invalid = Vec::new();
    let mut changed = snapshot.clone();
    changed.version = 2;
    invalid.push(changed);
    let mut changed = snapshot.clone();
    changed.verifier.clear();
    invalid.push(changed);
    let mut changed = snapshot.clone();
    changed.connections.get_mut("source").unwrap().id = "../secret".to_owned();
    invalid.push(changed);
    let mut changed = snapshot.clone();
    changed.connections.get_mut("source").unwrap().revision = "not-a-revision".to_owned();
    invalid.push(changed);
    let mut changed = snapshot.clone();
    changed
        .connections
        .get_mut("source")
        .unwrap()
        .integration_id = "unknown".to_owned();
    invalid.push(changed);
    let mut changed = snapshot.clone();
    changed
        .connections
        .get_mut("source")
        .unwrap()
        .config
        .insert("url".to_owned(), json!("invalid-secret-url"));
    invalid.push(changed);
    let mut changed = snapshot.clone();
    changed.views.get_mut("events").unwrap().connection_id = "missing".to_owned();
    invalid.push(changed);
    let mut changed = snapshot.clone();
    changed.views.get_mut("events").unwrap().revision = "invalid".to_owned();
    invalid.push(changed);
    let mut changed = snapshot.clone();
    changed.views.get_mut("events").unwrap().index = " ".to_owned();
    invalid.push(changed);
    let mut changed = snapshot.clone();
    let mut duplicate = changed.views["events"].clone();
    duplicate.id = "other".to_owned();
    duplicate.name = "EVENTS".to_owned();
    changed.views.insert(duplicate.id.clone(), duplicate);
    invalid.push(changed);
    let mut changed = snapshot.clone();
    changed.settings.settings = json!({"password":"secret"});
    invalid.push(changed);
    let mut changed = snapshot.clone();
    changed.settings.revision = "invalid".to_owned();
    invalid.push(changed);
    let path = directory.path().join(SNAPSHOT_NAME);
    for snapshot in invalid {
        let bytes = encryption
            .encrypt(SNAPSHOT_ID, &serde_json::to_vec(&snapshot).unwrap())
            .unwrap();
        fs::write(&path, &bytes).unwrap();
        let error = match Storage::open(options(directory.path())).await {
            Ok(_) => panic!("invalid snapshot accepted"),
            Err(error) => error,
        };
        assert!(!error.to_string().contains("secret"));
        assert_eq!(fs::read(&path).unwrap(), bytes);
    }
    for plaintext in [
        b"not-json".as_slice(),
        b"{\"version\":1,\"private\":true}".as_slice(),
    ] {
        let bytes = encryption.encrypt(SNAPSHOT_ID, plaintext).unwrap();
        fs::write(&path, &bytes).unwrap();
        assert!(matches!(
            Storage::open(options(directory.path())).await,
            Err(StorageError::Schema)
        ));
        assert_eq!(fs::read(&path).unwrap(), bytes);
    }
}

#[tokio::test]
async fn oversized_snapshots_are_rejected_before_decryption() {
    let directory = tempfile::tempdir().unwrap();
    drop(Storage::open(options(directory.path())).await.unwrap());
    let path = directory.path().join(SNAPSHOT_NAME);
    let file = OpenOptions::new().write(true).open(&path).unwrap();
    file.set_len(MAX_BYTES + 1).unwrap();
    drop(file);
    assert!(matches!(
        Storage::open(options(directory.path())).await,
        Err(StorageError::Invalid)
    ));
    assert_eq!(fs::metadata(&path).unwrap().len(), MAX_BYTES + 1);
}

#[tokio::test]
async fn concurrent_writers_have_one_winner_and_revision_checks_are_serialized() {
    let directory = tempfile::tempdir().unwrap();
    let storage = Storage::open(options(directory.path())).await.unwrap();
    let clone = storage.clone();
    let (first, second) = tokio::join!(
        storage.create_connection(record("source")),
        clone.create_connection(record("source"))
    );
    assert_eq!(usize::from(first.is_ok()) + usize::from(second.is_ok()), 1);
    assert!(
        matches!(first, Err(StorageError::Duplicate))
            || matches!(second, Err(StorageError::Duplicate))
    );
    let settings = storage.settings().await.unwrap();
    let (first, second) = tokio::join!(
        storage.save_settings(json!({"theme":"dark"}), &settings.revision),
        clone.save_settings(json!({"theme":"light"}), &settings.revision)
    );
    assert_eq!(usize::from(first.is_ok()) + usize::from(second.is_ok()), 1);
    assert!(
        matches!(first, Err(StorageError::Conflict))
            || matches!(second, Err(StorageError::Conflict))
    );
    assert_eq!(
        storage.settings().await.unwrap(),
        clone.settings().await.unwrap()
    );
}

#[tokio::test]
async fn connection_type_changes_and_invalid_views_do_not_commit() {
    let directory = tempfile::tempdir().unwrap();
    let storage = Storage::open(options(directory.path())).await.unwrap();
    let original = storage.create_connection(record("source")).await.unwrap();
    let bytes = fs::read(directory.path().join(SNAPSHOT_NAME)).unwrap();
    let mut changed = record("source");
    changed.integration_id = "elasticsearch".to_owned();
    assert!(matches!(
        storage
            .update_connection("source", &original, changed)
            .await,
        Err(StorageError::Invalid)
    ));
    let mut invalid = view("events", "source");
    invalid.time_field = "x".repeat(1025);
    assert!(matches!(
        storage.save_view(invalid, &original, None).await,
        Err(StorageError::Invalid)
    ));
    assert_eq!(
        fs::read(directory.path().join(SNAPSHOT_NAME)).unwrap(),
        bytes
    );
}

#[cfg(unix)]
#[tokio::test]
async fn symlinks_are_rejected_and_unix_permissions_are_restricted() {
    use std::os::unix::fs::{symlink, PermissionsExt};
    let directory = tempfile::tempdir().unwrap();
    let root = directory.path().join("store");
    drop(Storage::open(options(&root)).await.unwrap());
    assert_eq!(
        fs::metadata(&root).unwrap().permissions().mode() & 0o777,
        0o700
    );
    assert_eq!(
        fs::metadata(root.join(SNAPSHOT_NAME))
            .unwrap()
            .permissions()
            .mode()
            & 0o777,
        0o600
    );
    let alias = directory.path().join("alias");
    symlink(&root, &alias).unwrap();
    assert!(matches!(
        Storage::open(options(&alias)).await,
        Err(StorageError::Unavailable)
    ));
    for name in [SNAPSHOT_NAME, LOCK_NAME] {
        let path = root.join(name);
        let backup = root.join(format!("{name}.backup"));
        fs::rename(&path, &backup).unwrap();
        symlink(&backup, &path).unwrap();
        assert!(matches!(
            Storage::open(options(&root)).await,
            Err(StorageError::Unavailable)
        ));
        fs::remove_file(&path).unwrap();
        fs::rename(&backup, &path).unwrap();
    }
}
