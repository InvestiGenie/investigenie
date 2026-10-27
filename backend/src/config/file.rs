use std::{
    env,
    ffi::OsString,
    fs,
    path::{Path, PathBuf},
};

use serde::Deserialize;

use super::ConfigError;

const CONFIGURATION_NAME: &str = "investigenie.toml";
const MAX_CONFIGURATION_BYTES: u64 = 1_048_576;

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Command {
    #[default]
    Serve,
    Migrate,
    Healthcheck,
}

#[derive(Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
struct FileConfiguration {
    server: ServerConfiguration,
    storage: StorageConfiguration,
    logging: LogConfiguration,
    ai: AiConfiguration,
}

#[derive(Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
struct ServerConfiguration {
    bind: Option<String>,
    web_dir: Option<String>,
}

#[derive(Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
struct StorageConfiguration {
    mode: Option<String>,
    path: Option<String>,
    database_url: Option<String>,
    max_connections: Option<u32>,
    encryption_key: Option<String>,
    encryption_key_id: Option<String>,
}

#[derive(Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
struct LogConfiguration {
    level: Option<String>,
    mode: Option<String>,
    path: Option<String>,
    max_files: Option<usize>,
}

#[derive(Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
struct AiConfiguration {
    probe_timeout_seconds: Option<u64>,
}

pub struct Configuration {
    file: FileConfiguration,
    directory: Option<PathBuf>,
    pub command: Command,
}

impl Configuration {
    pub fn load(arguments: impl IntoIterator<Item = OsString>) -> Result<Self, ConfigError> {
        let current = env::current_dir().map_err(|_| ConfigError::ConfigurationFile)?;
        let executable = env::current_exe().map_err(|_| ConfigError::ConfigurationFile)?;
        Self::load_from(arguments, &current, executable.parent().unwrap_or(&current))
    }

    fn load_from(
        arguments: impl IntoIterator<Item = OsString>,
        current: &Path,
        executable: &Path,
    ) -> Result<Self, ConfigError> {
        let (command, explicit) = parse_arguments(arguments)?;
        let path = match explicit {
            Some(path) => Some(if path.is_absolute() {
                path
            } else {
                current.join(path)
            }),
            None => match optional_file(&current.join(CONFIGURATION_NAME))? {
                Some(path) => Some(path),
                None => optional_file(&executable.join(CONFIGURATION_NAME))?,
            },
        };
        let Some(path) = path else {
            return Ok(Self {
                file: FileConfiguration::default(),
                directory: None,
                command,
            });
        };
        let metadata = fs::metadata(&path).map_err(|_| ConfigError::ConfigurationFile)?;
        if !metadata.is_file() || metadata.len() > MAX_CONFIGURATION_BYTES {
            return Err(ConfigError::ConfigurationFile);
        }
        let text = fs::read_to_string(&path).map_err(|_| ConfigError::ConfigurationFile)?;
        if text.len() as u64 > MAX_CONFIGURATION_BYTES {
            return Err(ConfigError::ConfigurationFile);
        }
        let file = toml::from_str(&text).map_err(|_| ConfigError::ConfigurationFormat)?;
        Ok(Self {
            file,
            directory: path.parent().map(Path::to_path_buf),
            command,
        })
    }

    pub fn value(&self, name: &str) -> Option<String> {
        self.value_with(name, |key| env::var(key).ok())
    }

    fn value_with(&self, name: &str, get: impl Fn(&str) -> Option<String>) -> Option<String> {
        if let Some(value) = get(name) {
            return if matches!(name, "LOG_PATH" | "FILESYSTEM_PATH" | "WEB_DIR") {
                self.path_value(Some(&value))
            } else {
                Some(value)
            };
        }
        match name {
            "RUST_API_BIND" => self.file.server.bind.clone(),
            "WEB_DIR" => self.path_value(self.file.server.web_dir.as_deref()),
            "PERSISTENCE_MODE" => self.file.storage.mode.clone(),
            "FILESYSTEM_PATH" => self.path_value(self.file.storage.path.as_deref()),
            "DATABASE_URL" => self.file.storage.database_url.clone(),
            "DB_MAX_CONNECTIONS" => self
                .file
                .storage
                .max_connections
                .map(|value| value.to_string()),
            "CONFIG_ENCRYPTION_KEY" => self.file.storage.encryption_key.clone(),
            "CONFIG_ENCRYPTION_KEY_ID" => self.file.storage.encryption_key_id.clone(),
            "LOG_LEVEL" => self.file.logging.level.clone(),
            "LOG_MODE" => self.file.logging.mode.clone(),
            "LOG_PATH" => {
                self.path_value(Some(self.file.logging.path.as_deref().unwrap_or("logs")))
            }
            "LOG_MAX_FILES" => self.file.logging.max_files.map(|value| value.to_string()),
            "AI_PROBE_TIMEOUT_SECONDS" => self
                .file
                .ai
                .probe_timeout_seconds
                .map(|value| value.to_string()),
            _ => None,
        }
    }

    fn path_value(&self, value: Option<&str>) -> Option<String> {
        value.map(|value| {
            let path = Path::new(value);
            if value.trim().is_empty() || path.is_absolute() {
                value.to_owned()
            } else {
                self.directory
                    .as_ref()
                    .map(|directory| directory.join(path).to_string_lossy().into_owned())
                    .unwrap_or_else(|| value.to_owned())
            }
        })
    }
}

fn optional_file(path: &Path) -> Result<Option<PathBuf>, ConfigError> {
    match fs::metadata(path) {
        Ok(_) => Ok(Some(path.to_owned())),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(_) => Err(ConfigError::ConfigurationFile),
    }
}

fn parse_arguments(
    arguments: impl IntoIterator<Item = OsString>,
) -> Result<(Command, Option<PathBuf>), ConfigError> {
    let mut command = Command::Serve;
    let mut path = None;
    let mut arguments = arguments.into_iter();
    while let Some(argument) = arguments.next() {
        match argument.to_str() {
            Some("--config") if path.is_none() => {
                let value = arguments
                    .next()
                    .filter(|value| !value.is_empty())
                    .ok_or(ConfigError::Arguments)?;
                path = Some(PathBuf::from(value));
            }
            Some(value) if value.starts_with("--config=") && path.is_none() => {
                let value = value
                    .strip_prefix("--config=")
                    .filter(|value| !value.is_empty())
                    .ok_or(ConfigError::Arguments)?;
                path = Some(PathBuf::from(value));
            }
            Some("migrate") if command == Command::Serve => command = Command::Migrate,
            Some("healthcheck") if command == Command::Serve => command = Command::Healthcheck,
            _ => return Err(ConfigError::Arguments),
        }
    }
    Ok((command, path))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn load(directory: &Path, arguments: &[&str]) -> Result<Configuration, ConfigError> {
        Configuration::load_from(arguments.iter().map(OsString::from), directory, directory)
    }

    #[test]
    fn example_configuration_uses_simple_paths_and_quickstart_defaults() {
        let file: FileConfiguration =
            toml::from_str(include_str!("../../../investigenie.example.toml")).unwrap();
        assert_eq!(file.storage.path.as_deref(), Some("data"));
        assert!(file.storage.mode.is_none());
        assert!(file.storage.encryption_key.is_none());
        assert_eq!(file.logging.path.as_deref(), Some("logs"));
        assert_eq!(file.logging.mode.as_deref(), Some("both"));
    }

    #[test]
    fn toml_configuration_is_typed_overridable_and_relative_to_its_file() {
        let directory = tempfile::tempdir().unwrap();
        fs::write(directory.path().join(CONFIGURATION_NAME), "[server]\nbind='127.0.0.1:3100'\n[storage]\npath='data'\n[logging]\nlevel='debug'\nmode='console'\npath='logs'\nmax_files=3\n").unwrap();
        let configuration = load(directory.path(), &[]).unwrap();
        let read = |key| configuration.value_with(key, |_| None);
        assert_eq!(read("RUST_API_BIND").as_deref(), Some("127.0.0.1:3100"));
        assert!(read("WEB_ORIGIN").is_none());
        assert_eq!(
            configuration
                .value_with("WEB_ORIGIN", |_| Some("http://localhost:5173".to_owned()))
                .as_deref(),
            Some("http://localhost:5173")
        );
        assert_eq!(read("LOG_MODE").as_deref(), Some("console"));
        assert_eq!(read("LOG_MAX_FILES").as_deref(), Some("3"));
        assert_eq!(
            PathBuf::from(read("FILESYSTEM_PATH").unwrap()),
            directory.path().join("data")
        );
        assert_eq!(
            PathBuf::from(read("LOG_PATH").unwrap()),
            directory.path().join("logs")
        );
        assert_eq!(
            configuration
                .value_with("LOG_LEVEL", |_| Some("error".to_owned()))
                .as_deref(),
            Some("error")
        );
        assert_eq!(
            PathBuf::from(
                configuration
                    .value_with("FILESYSTEM_PATH", |_| Some("override".to_owned()))
                    .unwrap()
            ),
            directory.path().join("override")
        );
        let absolute = directory.path().join("absolute-logs");
        assert_eq!(
            PathBuf::from(
                configuration
                    .value_with("LOG_PATH", |_| Some(
                        absolute.to_string_lossy().into_owned()
                    ))
                    .unwrap()
            ),
            absolute
        );
    }

    #[test]
    fn toml_configuration_discovers_defaults_and_rejects_bad_explicit_files_safely() {
        let directory = tempfile::tempdir().unwrap();
        let configuration = load(directory.path(), &[]).unwrap();
        assert_eq!(configuration.command, Command::Serve);
        assert!(configuration
            .value_with("PERSISTENCE_MODE", |_| None)
            .is_none());
        for text in [
            "[logging]\nto_file='true'",
            "[server]\nweb_origin='http://127.0.0.1:5173'",
            "[server]\nunknown='private-value'",
            "[unknown]\nsecret='private-value'",
            "private-value=",
        ] {
            fs::write(directory.path().join("custom.toml"), text).unwrap();
            let error = load(directory.path(), &["--config", "custom.toml"])
                .err()
                .unwrap();
            assert!(!error.to_string().contains("private-value"));
        }
        assert!(load(directory.path(), &["--config", "missing.toml"]).is_err());
        assert!(load(directory.path(), &["--config"]).is_err());
        assert!(load(directory.path(), &["--config=a", "--config=b"]).is_err());
        assert!(load(directory.path(), &["unknown-command"]).is_err());
        fs::write(
            directory.path().join("custom.toml"),
            "[logging]\nlevel='warn'",
        )
        .unwrap();
        assert_eq!(
            load(directory.path(), &["migrate", "--config=custom.toml"])
                .unwrap()
                .command,
            Command::Migrate
        );
        assert_eq!(
            load(
                directory.path(),
                &["--config", "custom.toml", "healthcheck"]
            )
            .unwrap()
            .command,
            Command::Healthcheck
        );
        let other = tempfile::tempdir().unwrap();
        fs::write(
            directory.path().join(CONFIGURATION_NAME),
            "[logging]\nlevel='info'",
        )
        .unwrap();
        assert_eq!(
            Configuration::load_from(Vec::<OsString>::new(), other.path(), directory.path())
                .unwrap()
                .value_with("LOG_LEVEL", |_| None)
                .as_deref(),
            Some("info")
        );
    }
}
