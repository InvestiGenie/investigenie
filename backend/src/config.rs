use std::{env, net::SocketAddr};

use crate::storage::{validate_encryption_key, PersistenceOptions, StorageOptions};
use thiserror::Error;
use url::Url;

#[derive(Clone)]
pub struct Settings {
    pub bind: SocketAddr,
    pub web_origin: String,
    pub storage: PersistenceOptions,
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

        let storage = persistence_from_values(|name| env::var(name).ok())?;
        Ok(Self {
            bind,
            web_origin,
            storage,
        })
    }
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
    use base64::{engine::general_purpose::STANDARD, Engine};

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
            "postgres://localhost/northstar?sslmode=disable",
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
