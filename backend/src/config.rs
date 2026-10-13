use std::{
    env, fs,
    io::Write,
    net::SocketAddr,
    path::{Path, PathBuf},
};

use crate::storage::{validate_encryption_key, PersistenceOptions, StorageOptions};
use base64::{engine::general_purpose::STANDARD, Engine};
use thiserror::Error;
use url::Url;

const QUICKSTART_DATA_DIRECTORY: &str = "investigenie_data";
const QUICKSTART_KEY_FILE: &str = "encryption.key";

#[derive(Clone)]
pub struct Settings {
    pub bind: SocketAddr,
    pub web_origin: String,
    pub storage: PersistenceOptions,
    pub web_dir: Option<PathBuf>,
    /// No storage settings were supplied, so a local filesystem store with a generated key is used.
    pub quickstart: bool,
}

#[derive(Debug, Error)]
pub enum ConfigError {
    #[error("DATABASE_URL is required and must be a PostgreSQL URL")]
    Database,
    #[error("CONFIG_ENCRYPTION_KEY must be base64 encoding of 32 bytes and CONFIG_ENCRYPTION_KEY_ID must be a valid key identifier")]
    Encryption,
    #[error("PERSISTENCE_MODE must be database or filesystem")]
    PersistenceMode,
    #[error("FILESYSTEM_PATH is required and must be nonempty in filesystem mode")]
    FilesystemPath,
    #[error("DB_MAX_CONNECTIONS must be between 1 and 1000")]
    PoolSize,
    #[error("Invalid RUST_API_BIND address")]
    BindAddress,
    #[error("WEB_ORIGIN must be a valid HTTP or HTTPS origin")]
    WebOrigin,
    #[error("Could not create or read the quickstart encryption key in {0}")]
    QuickstartKey(String),
}

impl Settings {
    pub fn from_env() -> Result<Self, ConfigError> {
        let bind = env::var("RUST_API_BIND")
            .unwrap_or_else(|_| "127.0.0.1:3000".to_owned())
            .parse()
            .map_err(|_| ConfigError::BindAddress)?;

        let web_origin =
            env::var("WEB_ORIGIN").unwrap_or_else(|_| "http://127.0.0.1:5173".to_owned());
        let parsed_origin = Url::parse(&web_origin).map_err(|_| ConfigError::WebOrigin)?;
        if !matches!(parsed_origin.scheme(), "http" | "https")
            || parsed_origin.host_str().is_none()
            || parsed_origin.path() != "/"
            || parsed_origin.query().is_some()
            || parsed_origin.fragment().is_some()
            || !parsed_origin.username().is_empty()
            || parsed_origin.password().is_some()
        {
            return Err(ConfigError::WebOrigin);
        }

        let storage_configured = ["PERSISTENCE_MODE", "CONFIG_ENCRYPTION_KEY", "DATABASE_URL"]
            .iter()
            .any(|name| env::var(name).is_ok_and(|value| !value.trim().is_empty()));
        let storage = if storage_configured {
            persistence_from_values(|name| env::var(name).ok())?
        } else {
            quickstart_storage(&quickstart_directory())?
        };
        let web_dir = match env::var("WEB_DIR") {
            Ok(path) if !path.trim().is_empty() => Some(PathBuf::from(path)),
            _ => Some(application_directory().join("web"))
                .filter(|directory| directory.join("index.html").is_file()),
        };
        Ok(Self {
            bind,
            web_origin,
            storage,
            web_dir,
            quickstart: !storage_configured,
        })
    }
}

/// Directory containing the executable, so double-click and other-cwd launches find bundled files.
fn application_directory() -> PathBuf {
    env::current_exe()
        .ok()
        .and_then(|path| path.parent().map(Path::to_path_buf))
        .unwrap_or_else(|| PathBuf::from("."))
}

fn quickstart_directory() -> PathBuf {
    env::var("FILESYSTEM_PATH")
        .ok()
        .filter(|path| !path.trim().is_empty())
        .map(PathBuf::from)
        .unwrap_or_else(|| application_directory().join(QUICKSTART_DATA_DIRECTORY))
}

fn quickstart_storage(directory: &Path) -> Result<PersistenceOptions, ConfigError> {
    let failure = || ConfigError::QuickstartKey(directory.display().to_string());
    let key_path = directory.join(QUICKSTART_KEY_FILE);
    let key = match fs::read_to_string(&key_path) {
        Ok(existing) => existing.trim().to_owned(),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            let mut directories = fs::DirBuilder::new();
            directories.recursive(true);
            #[cfg(unix)]
            {
                use std::os::unix::fs::DirBuilderExt;
                directories.mode(0o700);
            }
            directories.create(directory).map_err(|_| failure())?;
            let mut bytes = [0u8; 32];
            getrandom::getrandom(&mut bytes).map_err(|_| failure())?;
            let key = STANDARD.encode(bytes);
            let mut options = fs::OpenOptions::new();
            options.write(true).create_new(true);
            #[cfg(unix)]
            {
                use std::os::unix::fs::OpenOptionsExt;
                options.mode(0o600);
            }
            options
                .open(&key_path)
                .and_then(|mut file| file.write_all(key.as_bytes()))
                .map_err(|_| failure())?;
            key
        }
        Err(_) => return Err(failure()),
    };
    validate_encryption_key(&key, "v1").map_err(|_| ConfigError::Encryption)?;
    Ok(PersistenceOptions::Filesystem {
        path: directory.to_path_buf(),
        key,
        key_id: "v1".to_owned(),
    })
}

fn persistence_from_values(
    get: impl Fn(&str) -> Option<String>,
) -> Result<PersistenceOptions, ConfigError> {
    let mode = get("PERSISTENCE_MODE").unwrap_or_else(|| "database".to_owned());
    if !matches!(mode.as_str(), "database" | "filesystem") {
        return Err(ConfigError::PersistenceMode);
    }
    let encryption_key = get("CONFIG_ENCRYPTION_KEY").ok_or(ConfigError::Encryption)?;
    let key_id = get("CONFIG_ENCRYPTION_KEY_ID").unwrap_or_else(|| "v1".to_owned());
    validate_encryption_key(&encryption_key, &key_id).map_err(|_| ConfigError::Encryption)?;
    if mode == "filesystem" {
        let path = get("FILESYSTEM_PATH")
            .filter(|path| !path.trim().is_empty())
            .ok_or(ConfigError::FilesystemPath)?;
        return Ok(PersistenceOptions::Filesystem {
            path: path.into(),
            key: encryption_key,
            key_id,
        });
    }
    let database_url = get("DATABASE_URL").ok_or(ConfigError::Database)?;
    let parsed_database = Url::parse(&database_url).map_err(|_| ConfigError::Database)?;
    if !matches!(parsed_database.scheme(), "postgres" | "postgresql")
        || parsed_database.host_str().is_none()
    {
        return Err(ConfigError::Database);
    }
    let max_connections = get("DB_MAX_CONNECTIONS")
        .unwrap_or_else(|| "10".to_owned())
        .parse::<u32>()
        .map_err(|_| ConfigError::PoolSize)?;
    if !(1..=1000).contains(&max_connections) {
        return Err(ConfigError::PoolSize);
    }
    Ok(PersistenceOptions::Database(StorageOptions {
        database_url,
        encryption_key,
        key_id,
        max_connections,
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn quickstart_generates_a_key_once_and_reuses_it() {
        let directory = tempfile::tempdir().unwrap();
        let store = directory.path().join("data");
        let PersistenceOptions::Filesystem { path, key, key_id } =
            quickstart_storage(&store).unwrap()
        else {
            panic!("quickstart uses filesystem storage");
        };
        assert_eq!(path, store);
        assert_eq!(key_id, "v1");
        assert!(validate_encryption_key(&key, "v1").is_ok());
        let PersistenceOptions::Filesystem { key: reused, .. } =
            quickstart_storage(&store).unwrap()
        else {
            panic!("quickstart uses filesystem storage");
        };
        assert_eq!(reused, key);
        fs::write(store.join(QUICKSTART_KEY_FILE), "corrupt").unwrap();
        assert!(matches!(
            quickstart_storage(&store),
            Err(ConfigError::Encryption)
        ));
    }

    fn parse(values: &[(&str, &str)]) -> Result<PersistenceOptions, ConfigError> {
        persistence_from_values(|name| {
            values
                .iter()
                .find(|(key, _)| *key == name)
                .map(|(_, value)| (*value).to_owned())
        })
    }

    #[test]
    fn modes_validate_only_the_selected_backend() {
        let key = STANDARD.encode([7u8; 32]);
        let base = [("CONFIG_ENCRYPTION_KEY", key.as_str())];
        assert!(matches!(parse(&base), Err(ConfigError::Database)));
        assert!(matches!(
            parse(&[("PERSISTENCE_MODE", "invalid")]),
            Err(ConfigError::PersistenceMode)
        ));
        let mut filesystem = base.to_vec();
        filesystem.extend([
            ("PERSISTENCE_MODE", "filesystem"),
            ("FILESYSTEM_PATH", "custom/store"),
            ("DATABASE_URL", "invalid"),
            ("DB_MAX_CONNECTIONS", "invalid"),
        ]);
        assert!(matches!(
            parse(&filesystem),
            Ok(PersistenceOptions::Filesystem { .. })
        ));
        filesystem[2].1 = " ";
        assert!(matches!(
            parse(&filesystem),
            Err(ConfigError::FilesystemPath)
        ));
        let mut database = base.to_vec();
        database.push((
            "DATABASE_URL",
            "postgres://localhost/investigenie?sslmode=disable",
        ));
        assert!(matches!(
            parse(&database),
            Ok(PersistenceOptions::Database(_))
        ));
        database.push(("DB_MAX_CONNECTIONS", "0"));
        assert!(matches!(parse(&database), Err(ConfigError::PoolSize)));
    }

    #[test]
    fn both_modes_require_a_valid_encryption_key_and_id() {
        let key = STANDARD.encode([7u8; 32]);
        for mode in ["database", "filesystem"] {
            for invalid in ["", "not-base64", "YQ=="] {
                assert!(matches!(
                    parse(&[
                        ("PERSISTENCE_MODE", mode),
                        ("CONFIG_ENCRYPTION_KEY", invalid)
                    ]),
                    Err(ConfigError::Encryption)
                ));
            }
            assert!(matches!(
                parse(&[
                    ("PERSISTENCE_MODE", mode),
                    ("CONFIG_ENCRYPTION_KEY", &key),
                    ("CONFIG_ENCRYPTION_KEY_ID", "bad id")
                ]),
                Err(ConfigError::Encryption)
            ));
        }
    }
}
