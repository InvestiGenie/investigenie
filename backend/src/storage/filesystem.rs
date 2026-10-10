use super::*;
use fs2::FileExt;
use std::{
    collections::{BTreeMap, BTreeSet},
    fs::{self, File, OpenOptions},
    io::{Read, Write},
    path::{Path, PathBuf},
    sync::Mutex,
};

const SNAPSHOT_ID: &str = "northstar-filesystem-snapshot";
const SNAPSHOT_NAME: &str = "configuration.snapshot";
const LOCK_NAME: &str = "configuration.lock";
const MAX_BYTES: u64 = 16 * 1024 * 1024;

#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct Snapshot {
    version: u8,
    verifier: String,
    pub connections: BTreeMap<String, StoredConnection>,
    pub views: BTreeMap<String, DataView>,
    #[serde(default)]
    pub ai_models: BTreeMap<String, StoredAiModel>,
    pub settings: PublicSettings,
}

impl Snapshot {
    fn empty() -> Self {
        Self {
            version: 1,
            verifier: String::from_utf8(VERIFIER.to_vec()).unwrap(),
            connections: BTreeMap::new(),
            views: BTreeMap::new(),
            ai_models: BTreeMap::new(),
            settings: PublicSettings {
                settings: json!({"theme":"system", "default_timezone":"UTC"}),
                revision: revision(),
            },
        }
    }

    fn validate(&self) -> Result<(), StorageError> {
        if self.version != 1 {
            return Err(StorageError::Schema);
        }
        if self.verifier.as_bytes() != VERIFIER {
            return Err(StorageError::Encryption);
        }
        for (id, record) in &self.connections {
            if id != &record.id || Uuid::parse_str(&record.revision).is_err() {
                return Err(StorageError::Invalid);
            }
            validate_record(record)?;
        }
        let mut names = BTreeSet::new();
        for (id, view) in &self.views {
            validate_view(view)?;
            if id != &view.id
                || Uuid::parse_str(&view.revision).is_err()
                || !self.connections.contains_key(&view.connection_id)
            {
                return Err(StorageError::Invalid);
            }
            if !names.insert(view.name.to_lowercase()) {
                return Err(StorageError::Duplicate);
            }
        }
        validate_settings(&self.settings.settings)?;
        let mut model_names = BTreeSet::new();
        for (id, model) in &self.ai_models {
            validate_ai_model(model)?;
            if id != &model.id
                || Uuid::parse_str(&model.revision).is_err()
                || !model_names.insert(model.name.to_lowercase())
            {
                return Err(StorageError::Invalid);
            }
        }
        if Uuid::parse_str(&self.settings.revision).is_err() {
            return Err(StorageError::Invalid);
        }
        Ok(())
    }

    fn checked_connection(
        &self,
        id: &str,
        expected: &str,
    ) -> Result<&StoredConnection, StorageError> {
        let record = self
            .connections
            .get(id)
            .ok_or(StorageError::ConnectionNotFound)?;
        if record.revision != expected {
            return Err(StorageError::Conflict);
        }
        Ok(record)
    }

    fn checked_view(&self, id: &str, expected: &str) -> Result<(), StorageError> {
        let view = self.views.get(id).ok_or(StorageError::ViewNotFound)?;
        if view.revision != expected {
            return Err(StorageError::Conflict);
        }
        Ok(())
    }
}

pub(super) struct FileStorage {
    root: PathBuf,
    encryption: Encryption,
    _lock: File,
    snapshot: Mutex<Snapshot>,
}

fn io_error(_: std::io::Error) -> StorageError {
    StorageError::Unavailable
}

fn regular_path(path: &Path, directory: bool) -> Result<bool, StorageError> {
    match fs::symlink_metadata(path) {
        Ok(metadata)
            if !metadata.file_type().is_symlink()
                && if directory {
                    metadata.is_dir()
                } else {
                    metadata.is_file()
                } =>
        {
            Ok(true)
        }
        Ok(_) => Err(StorageError::Unavailable),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(false),
        Err(_) => Err(StorageError::Unavailable),
    }
}

#[cfg(unix)]
fn restrict(path: &Path, mode: u32) -> Result<(), StorageError> {
    use std::os::unix::fs::PermissionsExt;
    fs::set_permissions(path, fs::Permissions::from_mode(mode)).map_err(io_error)
}
#[cfg(not(unix))]
fn restrict(_: &Path, _: u32) -> Result<(), StorageError> {
    Ok(())
}

impl FileStorage {
    pub fn open(root: PathBuf, encryption: Encryption) -> Result<Self, StorageError> {
        if root.as_os_str().is_empty() {
            return Err(StorageError::Invalid);
        }
        regular_path(&root, true)?;
        let mut directories = fs::DirBuilder::new();
        directories.recursive(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::DirBuilderExt;
            directories.mode(0o700);
        }
        directories.create(&root).map_err(io_error)?;
        regular_path(&root, true)?;
        restrict(&root, 0o700)?;
        let lock_path = root.join(LOCK_NAME);
        regular_path(&lock_path, false)?;
        let mut options = OpenOptions::new();
        options.read(true).write(true).create(true).truncate(false);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        let lock = options.open(&lock_path).map_err(io_error)?;
        lock.try_lock_exclusive().map_err(io_error)?;
        restrict(&lock_path, 0o600)?;
        let mut storage = Self {
            root,
            encryption,
            _lock: lock,
            snapshot: Mutex::new(Snapshot::empty()),
        };
        if regular_path(&storage.root.join(SNAPSHOT_NAME), false)? {
            let snapshot = storage.load()?;
            *storage
                .snapshot
                .get_mut()
                .map_err(|_| StorageError::Unavailable)? = snapshot;
        } else {
            let snapshot = storage
                .snapshot
                .get_mut()
                .map_err(|_| StorageError::Unavailable)?
                .clone();
            if let Some(error) = storage.commit(&snapshot)? {
                return Err(error);
            }
        }
        Ok(storage)
    }

    fn load(&self) -> Result<Snapshot, StorageError> {
        let path = self.root.join(SNAPSHOT_NAME);
        if !regular_path(&path, false)? {
            return Err(StorageError::Unavailable);
        }
        let file = File::open(&path).map_err(io_error)?;
        if file.metadata().map_err(io_error)?.len() > MAX_BYTES {
            return Err(StorageError::Invalid);
        }
        let mut bytes = Vec::new();
        file.take(MAX_BYTES + 1)
            .read_to_end(&mut bytes)
            .map_err(io_error)?;
        if bytes.len() as u64 > MAX_BYTES {
            return Err(StorageError::Invalid);
        }
        let plaintext = self.encryption.decrypt(SNAPSHOT_ID, &bytes)?;
        let snapshot: Snapshot =
            serde_json::from_slice(&plaintext).map_err(|_| StorageError::Schema)?;
        snapshot.validate()?;
        restrict(&path, 0o600)?;
        Ok(snapshot)
    }

    fn commit(&self, snapshot: &Snapshot) -> Result<Option<StorageError>, StorageError> {
        let plaintext = serde_json::to_vec(snapshot).map_err(|_| StorageError::Invalid)?;
        if plaintext.len() as u64 > MAX_BYTES {
            return Err(StorageError::Invalid);
        }
        let bytes = self.encryption.encrypt(SNAPSHOT_ID, &plaintext)?;
        if bytes.len() as u64 > MAX_BYTES {
            return Err(StorageError::Invalid);
        }
        if !regular_path(&self.root, true)? {
            return Err(StorageError::Unavailable);
        }
        let path = self.root.join(SNAPSHOT_NAME);
        regular_path(&path, false)?;
        let mut temporary = tempfile::NamedTempFile::new_in(&self.root).map_err(io_error)?;
        restrict(temporary.path(), 0o600)?;
        temporary.write_all(&bytes).map_err(io_error)?;
        temporary.flush().map_err(io_error)?;
        temporary.as_file().sync_all().map_err(io_error)?;
        regular_path(&path, false)?;
        temporary
            .persist(&path)
            .map_err(|_| StorageError::Unavailable)?;
        #[cfg(unix)]
        if File::open(&self.root)
            .and_then(|directory| directory.sync_all())
            .is_err()
        {
            return Ok(Some(StorageError::Unavailable));
        }
        Ok(None)
    }

    pub async fn read<T: Send + 'static>(
        self: &Arc<Self>,
        operation: impl FnOnce(&Snapshot) -> Result<T, StorageError> + Send + 'static,
    ) -> Result<T, StorageError> {
        let storage = self.clone();
        tokio::task::spawn_blocking(move || {
            let snapshot = storage
                .snapshot
                .lock()
                .map_err(|_| StorageError::Unavailable)?;
            operation(&snapshot)
        })
        .await
        .map_err(|_| StorageError::Unavailable)?
    }

    async fn mutate<T: Send + 'static>(
        self: &Arc<Self>,
        operation: impl FnOnce(&mut Snapshot) -> Result<T, StorageError> + Send + 'static,
    ) -> Result<T, StorageError> {
        let storage = self.clone();
        tokio::task::spawn_blocking(move || {
            let mut current = storage
                .snapshot
                .lock()
                .map_err(|_| StorageError::Unavailable)?;
            storage.load()?;
            let mut next = current.clone();
            let result = operation(&mut next)?;
            next.validate()?;
            let durability_error = storage.commit(&next)?;
            *current = next;
            if let Some(error) = durability_error {
                return Err(error);
            }
            Ok(result)
        })
        .await
        .map_err(|_| StorageError::Unavailable)?
    }

    pub async fn ready(self: &Arc<Self>) -> Result<(), StorageError> {
        let storage = self.clone();
        self.read(move |_| storage.load().map(|_| ())).await
    }

    pub async fn create_connection(
        self: &Arc<Self>,
        mut record: StoredConnection,
    ) -> Result<String, StorageError> {
        self.mutate(move |snapshot| {
            validate_record(&record)?;
            if snapshot.connections.contains_key(&record.id) {
                return Err(StorageError::Duplicate);
            }
            record.revision = revision();
            let revision = record.revision.clone();
            snapshot.connections.insert(record.id.clone(), record);
            Ok(revision)
        })
        .await
    }

    pub async fn update_connection(
        self: &Arc<Self>,
        id: String,
        expected: String,
        mut record: StoredConnection,
    ) -> Result<String, StorageError> {
        self.mutate(move |snapshot| {
            validate_record(&record)?;
            if snapshot.checked_connection(&id, &expected)?.integration_id != record.integration_id
            {
                return Err(StorageError::Invalid);
            }
            if id != record.id && snapshot.connections.contains_key(&record.id) {
                return Err(StorageError::Duplicate);
            }
            if id != record.id {
                for view in snapshot
                    .views
                    .values_mut()
                    .filter(|view| view.connection_id == id)
                {
                    view.connection_id = record.id.clone();
                    view.revision = revision();
                }
            }
            record.revision = revision();
            let revision = record.revision.clone();
            snapshot.connections.remove(&id);
            snapshot.connections.insert(record.id.clone(), record);
            Ok(revision)
        })
        .await
    }

    pub async fn remove_connection(
        self: &Arc<Self>,
        id: String,
        expected: String,
    ) -> Result<(), StorageError> {
        self.mutate(move |snapshot| {
            snapshot.checked_connection(&id, &expected)?;
            snapshot.connections.remove(&id);
            snapshot.views.retain(|_, view| view.connection_id != id);
            Ok(())
        })
        .await
    }

    pub async fn save_view(
        self: &Arc<Self>,
        mut view: DataView,
        expected_connection: String,
        expected_view: Option<String>,
    ) -> Result<String, StorageError> {
        self.mutate(move |snapshot| {
            validate_view(&view)?;
            snapshot.checked_connection(&view.connection_id, &expected_connection)?;
            if let Some(expected) = expected_view {
                snapshot.checked_view(&view.id, &expected)?;
            } else if snapshot.views.contains_key(&view.id) {
                return Err(StorageError::Duplicate);
            }
            if snapshot.views.values().any(|other| {
                other.id != view.id && other.name.to_lowercase() == view.name.to_lowercase()
            }) {
                return Err(StorageError::Duplicate);
            }
            view.revision = revision();
            let revision = view.revision.clone();
            snapshot.views.insert(view.id.clone(), view);
            Ok(revision)
        })
        .await
    }

    pub async fn remove_view(
        self: &Arc<Self>,
        id: String,
        expected: String,
    ) -> Result<(), StorageError> {
        self.mutate(move |snapshot| {
            snapshot.checked_view(&id, &expected)?;
            snapshot.views.remove(&id);
            Ok(())
        })
        .await
    }

    pub async fn save_settings(
        self: &Arc<Self>,
        settings: Value,
        expected: String,
    ) -> Result<String, StorageError> {
        self.mutate(move |snapshot| {
            validate_settings(&settings)?;
            if snapshot.settings.revision != expected {
                return Err(StorageError::Conflict);
            }
            let revision = revision();
            snapshot.settings = PublicSettings {
                settings,
                revision: revision.clone(),
            };
            Ok(revision)
        })
        .await
    }

    pub async fn create_ai_model(
        self: &Arc<Self>,
        model: StoredAiModel,
    ) -> Result<(), StorageError> {
        self.mutate(move |snapshot| {
            if snapshot.ai_models.contains_key(&model.id) {
                return Err(StorageError::Duplicate);
            }
            if snapshot
                .ai_models
                .values()
                .any(|other| other.name.eq_ignore_ascii_case(&model.name))
            {
                return Err(StorageError::Duplicate);
            }
            snapshot.ai_models.insert(model.id.clone(), model);
            Ok(())
        })
        .await
    }

    pub async fn update_ai_model(
        self: &Arc<Self>,
        id: String,
        expected: String,
        mut model: StoredAiModel,
    ) -> Result<(), StorageError> {
        self.mutate(move |snapshot| {
            let current = snapshot
                .ai_models
                .get(&id)
                .ok_or(StorageError::AiModelNotFound)?;
            if current.revision != expected {
                return Err(StorageError::Conflict);
            }
            if model.id != id
                || snapshot
                    .ai_models
                    .values()
                    .any(|other| other.id != id && other.name.eq_ignore_ascii_case(&model.name))
            {
                return Err(StorageError::Duplicate);
            }
            model.revision = revision();
            snapshot.ai_models.insert(id, model);
            Ok(())
        })
        .await
    }

    pub async fn remove_ai_model(
        self: &Arc<Self>,
        id: String,
        expected: String,
    ) -> Result<(), StorageError> {
        self.mutate(move |snapshot| {
            let current = snapshot
                .ai_models
                .get(&id)
                .ok_or(StorageError::AiModelNotFound)?;
            if current.revision != expected {
                return Err(StorageError::Conflict);
            }
            snapshot.ai_models.remove(&id);
            Ok(())
        })
        .await
    }
}

#[cfg(test)]
mod tests;
