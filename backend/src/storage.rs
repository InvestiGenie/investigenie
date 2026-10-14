use crate::integrations::{
    connect, registered, ConnectionConfig, Connector, IntegrationDescriptor,
};
use aes_gcm::{
    aead::{Aead, KeyInit, Payload},
    Aes256Gcm, Nonce,
};
use base64::{engine::general_purpose::STANDARD, Engine};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use sqlx::{
    postgres::{PgConnectOptions, PgPoolOptions, PgRow},
    ConnectOptions, PgPool, Postgres, Row, Transaction,
};
use std::{
    str::FromStr,
    sync::Arc,
    time::{Duration, SystemTime, UNIX_EPOCH},
};
use thiserror::Error;
use uuid::Uuid;

mod filesystem;

pub const MAX_AI_CONVERSATIONS: usize = 64;
pub const MAX_AI_CONVERSATION_MESSAGES: usize = 200;
pub const MAX_AI_CONVERSATION_BYTES: usize = 128 * 1024;

#[derive(Clone)]
pub enum PersistenceOptions {
    Database(StorageOptions),
    Filesystem {
        path: std::path::PathBuf,
        key: String,
        key_id: String,
    },
}
impl From<StorageOptions> for PersistenceOptions {
    fn from(options: StorageOptions) -> Self {
        Self::Database(options)
    }
}
impl From<&StorageOptions> for PersistenceOptions {
    fn from(options: &StorageOptions) -> Self {
        Self::Database(options.clone())
    }
}
impl From<&PersistenceOptions> for PersistenceOptions {
    fn from(options: &PersistenceOptions) -> Self {
        options.clone()
    }
}
pub fn validate_encryption_key(key: &str, key_id: &str) -> Result<(), StorageError> {
    Encryption::new(key, key_id).map(|_| ())
}

static MIGRATOR: sqlx::migrate::Migrator = sqlx::migrate!("./migrations");
const VERIFIER_ID: &str = "investigenie-encryption-verifier";
const VERIFIER: &[u8] = b"investigenie configuration encryption v1";
const SNAPSHOT_ID: &str = "investigenie-filesystem-snapshot";
const LEGACY_NAMESPACE_B64: &str = "bm9ydGhzdGFyLWNvbmZpZw==";
const LEGACY_VERIFIER_ID_B64: &str = "bm9ydGhzdGFyLWVuY3J5cHRpb24tdmVyaWZpZXI=";
const LEGACY_VERIFIER_B64: &str = "bm9ydGhzdGFyIGNvbmZpZ3VyYXRpb24gZW5jcnlwdGlvbiB2MQ==";
const LEGACY_SNAPSHOT_ID_B64: &str = "bm9ydGhzdGFyLWZpbGVzeXN0ZW0tc25hcHNob3Q=";

fn legacy_verifier() -> Vec<u8> {
    STANDARD.decode(LEGACY_VERIFIER_B64).expect("valid legacy verifier encoding")
}

#[derive(Debug, Error)]
pub enum StorageError {
    #[error("Configuration storage is unavailable")]
    Unavailable,
    #[error("Configuration schema is not ready; run the explicit migrate command")]
    Schema,
    #[error("Configuration encryption key is invalid or does not match stored data")]
    Encryption,
    #[error("Stored connection configuration or integration is unavailable")]
    Configuration,
    #[error("Record already exists")]
    Duplicate,
    #[error("Connection does not exist")]
    ConnectionNotFound,
    #[error("Data view does not exist")]
    ViewNotFound,
    #[error("AI model does not exist")]
    AiModelNotFound,
    #[error("AI conversation does not exist")]
    AiConversationNotFound,
    #[error("Configuration changed; reload before editing")]
    Conflict,
    #[error("Record is invalid")]
    Invalid,
}
impl From<sqlx::Error> for StorageError {
    fn from(error: sqlx::Error) -> Self {
        if error
            .as_database_error()
            .is_some_and(|error| error.is_unique_violation())
        {
            Self::Duplicate
        } else {
            Self::Unavailable
        }
    }
}

#[derive(Clone)]
pub struct StorageOptions {
    pub database_url: String,
    pub encryption_key: String,
    pub key_id: String,
    pub max_connections: u32,
}
#[derive(Clone)]
struct Encryption {
    cipher: Aes256Gcm,
    key_id: String,
}
#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Envelope {
    version: u8,
    key_id: String,
    nonce: Vec<u8>,
    ciphertext: Vec<u8>,
}
impl Encryption {
    fn new(encoded_key: &str, key_id: &str) -> Result<Self, StorageError> {
        let mut key = STANDARD
            .decode(encoded_key)
            .map_err(|_| StorageError::Encryption)?;
        if key.len() != 32
            || key_id.is_empty()
            || key_id.len() > 64
            || !key_id
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || b"-_".contains(&byte))
        {
            return Err(StorageError::Encryption);
        }
        let cipher = Aes256Gcm::new_from_slice(&key).map_err(|_| StorageError::Encryption)?;
        key.fill(0);
        Ok(Self {
            cipher,
            key_id: key_id.to_owned(),
        })
    }
    fn aad(&self, integration_id: &str) -> Vec<u8> {
        serde_json::to_vec(&json!(["investigenie-config", 1, self.key_id, integration_id]))
            .unwrap()
    }
    fn legacy_aad(&self, integration_id: &str) -> Vec<u8> {
        let namespace = String::from_utf8(
            STANDARD.decode(LEGACY_NAMESPACE_B64).expect("valid legacy namespace encoding"),
        )
        .expect("legacy namespace is UTF-8");
        let legacy_id = match integration_id {
            VERIFIER_ID => String::from_utf8(
                STANDARD.decode(LEGACY_VERIFIER_ID_B64).expect("valid legacy verifier ID encoding"),
            )
            .expect("legacy verifier ID is UTF-8"),
            SNAPSHOT_ID => String::from_utf8(
                STANDARD.decode(LEGACY_SNAPSHOT_ID_B64).expect("valid legacy snapshot ID encoding"),
            )
            .expect("legacy snapshot ID is UTF-8"),
            _ => integration_id.to_owned(),
        };
        serde_json::to_vec(&json!([namespace, 1, self.key_id, legacy_id])).unwrap()
    }
    fn encrypt(&self, integration_id: &str, plaintext: &[u8]) -> Result<Vec<u8>, StorageError> {
        let mut nonce = [0u8; 12];
        getrandom::getrandom(&mut nonce).map_err(|_| StorageError::Encryption)?;
        let ciphertext = self
            .cipher
            .encrypt(
                Nonce::from_slice(&nonce),
                Payload {
                    msg: plaintext,
                    aad: &self.aad(integration_id),
                },
            )
            .map_err(|_| StorageError::Encryption)?;
        serde_json::to_vec(&Envelope {
            version: 1,
            key_id: self.key_id.clone(),
            nonce: nonce.to_vec(),
            ciphertext,
        })
        .map_err(|_| StorageError::Encryption)
    }
    fn decrypt(&self, integration_id: &str, bytes: &[u8]) -> Result<Vec<u8>, StorageError> {
        let envelope: Envelope =
            serde_json::from_slice(bytes).map_err(|_| StorageError::Encryption)?;
        if envelope.version != 1 || envelope.key_id != self.key_id || envelope.nonce.len() != 12 {
            return Err(StorageError::Encryption);
        }
        let nonce = Nonce::from_slice(&envelope.nonce);
        self.cipher
            .decrypt(
                nonce,
                Payload {
                    msg: &envelope.ciphertext,
                    aad: &self.aad(integration_id),
                },
            )
            .or_else(|_| {
                self.cipher.decrypt(
                    nonce,
                    Payload {
                        msg: &envelope.ciphertext,
                        aad: &self.legacy_aad(integration_id),
                    },
                )
            })
            .map_err(|_| StorageError::Encryption)
    }
    #[cfg(test)]
    pub(super) fn encrypt_legacy_fixture(
        &self,
        integration_id: &str,
        plaintext: &[u8],
    ) -> Vec<u8> {
        let nonce = [42u8; 12];
        let ciphertext = self
            .cipher
            .encrypt(
                Nonce::from_slice(&nonce),
                Payload {
                    msg: plaintext,
                    aad: &self.legacy_aad(integration_id),
                },
            )
            .unwrap();
        serde_json::to_vec(&Envelope {
            version: 1,
            key_id: self.key_id.clone(),
            nonce: nonce.to_vec(),
            ciphertext,
        })
        .unwrap()
    }
}

#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct StoredConnection {
    pub id: String,
    pub name: String,
    pub integration_id: String,
    pub config: ConnectionConfig,
    pub revision: String,
}
#[derive(Clone)]
pub struct ResolvedConnection {
    pub record: StoredConnection,
    pub integration: IntegrationDescriptor,
    pub connector: Arc<dyn Connector>,
}
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct DataView {
    pub id: String,
    pub name: String,
    pub connection_id: String,
    pub index: String,
    pub time_field: String,
    pub revision: String,
}
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct PublicSettings {
    pub settings: Value,
    pub revision: String,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct StoredAiModel {
    pub id: String,
    pub revision: String,
    pub name: String,
    pub api_url: String,
    pub model: String,
    pub api_key: String,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct StoredAiConversationMessage {
    pub role: String,
    pub content: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub result: Option<Value>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct StoredAiConversation {
    pub id: String,
    pub revision: String,
    pub title: String,
    pub model_id: String,
    pub data_view_ids: Vec<String>,
    pub created_at_ms: i64,
    pub updated_at_ms: i64,
    pub messages: Vec<StoredAiConversationMessage>,
}

#[derive(Clone, Debug, Serialize)]
pub struct AiConversationSummary {
    pub id: String,
    pub revision: String,
    pub title: String,
    pub updated_at_ms: i64,
}

impl StoredAiConversation {
    pub fn new(id: String, model_id: String, data_view_ids: Vec<String>, question: String) -> Self {
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_millis()
            .min(i64::MAX as u128) as i64;
        Self {
            id,
            revision: revision(),
            title: question.trim().chars().take(160).collect(),
            model_id,
            data_view_ids,
            created_at_ms: now,
            updated_at_ms: now,
            messages: vec![StoredAiConversationMessage {
                role: "user".to_owned(),
                content: question,
                result: None,
            }],
        }
    }

    pub fn summary(&self) -> AiConversationSummary {
        AiConversationSummary {
            id: self.id.clone(),
            revision: self.revision.clone(),
            title: self.title.clone(),
            updated_at_ms: self.updated_at_ms,
        }
    }
}

#[derive(Clone, Debug, Serialize)]
pub struct AiModelSummary {
    pub id: String,
    pub revision: String,
    pub name: String,
    pub api_url: String,
    pub model: String,
    pub has_api_key: bool,
}

impl From<&StoredAiModel> for AiModelSummary {
    fn from(model: &StoredAiModel) -> Self {
        Self {
            id: model.id.clone(),
            revision: model.revision.clone(),
            name: model.name.clone(),
            api_url: model.api_url.clone(),
            model: model.model.clone(),
            has_api_key: true,
        }
    }
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct SettingsFields {
    theme: Option<String>,
    default_timezone: Option<String>,
}
pub fn validate_settings(value: &Value) -> Result<(), StorageError> {
    if !value.is_object()
        || serde_json::to_vec(value)
            .map_err(|_| StorageError::Invalid)?
            .len()
            > 65536
    {
        return Err(StorageError::Invalid);
    }
    let fields: SettingsFields =
        serde_json::from_value(value.clone()).map_err(|_| StorageError::Invalid)?;
    if fields
        .theme
        .as_ref()
        .is_some_and(|theme| !matches!(theme.as_str(), "system" | "light" | "dark"))
        || fields
            .default_timezone
            .as_ref()
            .is_some_and(|zone| zone.parse::<chrono_tz::Tz>().is_err())
        || value.as_object().unwrap().values().any(Value::is_null)
    {
        return Err(StorageError::Invalid);
    }
    Ok(())
}
#[derive(Clone)]
pub struct Storage {
    pool: PgPool,
    encryption: Encryption,
    file: Option<Arc<filesystem::FileStorage>>,
}
pub fn valid_id(id: &str) -> bool {
    !id.is_empty()
        && id.len() <= 64
        && id
            .bytes()
            .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || b"-_".contains(&byte))
}
fn revision() -> String {
    Uuid::new_v4().to_string()
}
fn validate_record(record: &StoredConnection) -> Result<(), StorageError> {
    if !valid_id(&record.id) || record.name.trim().is_empty() || record.name.len() > 256 {
        return Err(StorageError::Invalid);
    }
    descriptor(&record.integration_id)?;
    connect(&record.integration_id, &record.config).map_err(|_| StorageError::Configuration)?;
    if serde_json::to_vec(&record.config)
        .map_err(|_| StorageError::Configuration)?
        .len()
        > 65536
    {
        return Err(StorageError::Invalid);
    }
    Ok(())
}
fn validate_view(view: &DataView) -> Result<(), StorageError> {
    if !valid_id(&view.id)
        || !valid_id(&view.connection_id)
        || view.name.trim().is_empty()
        || view.name.len() > 256
        || view.index.trim().is_empty()
        || view.index.len() > 1024
        || view.time_field.trim().is_empty()
        || view.time_field.len() > 1024
    {
        return Err(StorageError::Invalid);
    }
    Ok(())
}
pub fn validate_ai_model(model: &StoredAiModel) -> Result<(), StorageError> {
    let parsed = url::Url::parse(&model.api_url).map_err(|_| StorageError::Invalid)?;
    if Uuid::parse_str(&model.id).is_err()
        || model.name.trim().is_empty()
        || model.name.len() > 128
        || model.model.trim().is_empty()
        || model.model.len() > 256
        || model.api_key.trim().is_empty()
        || model.api_key.len() > 8192
        || model.api_url.len() > 2048
        || !matches!(parsed.scheme(), "http" | "https")
        || parsed.host_str().is_none()
        || !parsed.username().is_empty()
        || parsed.password().is_some()
        || parsed.query().is_some()
        || parsed.fragment().is_some()
    {
        return Err(StorageError::Invalid);
    }
    Ok(())
}

fn validate_ai_conversation(conversation: &StoredAiConversation) -> Result<(), StorageError> {
    if !valid_id(&conversation.id)
        || Uuid::parse_str(&conversation.revision).is_err()
        || !valid_id(&conversation.model_id)
        || conversation.title.trim().is_empty()
        || conversation.title.len() > 160
        || conversation.created_at_ms < 0
        || conversation.updated_at_ms < conversation.created_at_ms
        || conversation.data_view_ids.is_empty()
        || conversation.data_view_ids.len() > 5
        || conversation.data_view_ids.iter().any(|id| !valid_id(id))
        || conversation.messages.is_empty()
        || conversation.messages.len() > MAX_AI_CONVERSATION_MESSAGES
        || conversation.messages.iter().any(|message| {
            !matches!(message.role.as_str(), "user" | "assistant")
                || message.content.trim().is_empty()
                || message.content.len() > 32_768
        })
        || serde_json::to_vec(conversation)
            .map_err(|_| StorageError::Invalid)?
            .len()
            > MAX_AI_CONVERSATION_BYTES
    {
        return Err(StorageError::Invalid);
    }
    Ok(())
}

fn conversation_timestamp() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis()
        .min(i64::MAX as u128) as i64
}
fn descriptor(id: &str) -> Result<IntegrationDescriptor, StorageError> {
    registered()
        .into_iter()
        .find(|entry| entry.id == id)
        .ok_or(StorageError::Configuration)
}
fn decode_view(row: PgRow) -> Result<DataView, StorageError> {
    Ok(DataView {
        id: row.try_get("id")?,
        name: row.try_get("name")?,
        connection_id: row.try_get("connection_id")?,
        index: row.try_get("index_name")?,
        time_field: row.try_get("time_field")?,
        revision: row.try_get("revision")?,
    })
}
async fn checked_connection(
    transaction: &mut Transaction<'_, Postgres>,
    id: &str,
    expected: &str,
) -> Result<(), StorageError> {
    let actual: Option<String> =
        sqlx::query_scalar("SELECT revision FROM connections WHERE id = $1 FOR UPDATE")
            .bind(id)
            .fetch_optional(&mut **transaction)
            .await?;
    match actual {
        None => Err(StorageError::ConnectionNotFound),
        Some(actual) if actual != expected => Err(StorageError::Conflict),
        _ => Ok(()),
    }
}

impl Storage {
    async fn pool(options: &StorageOptions, schema: Option<&str>) -> Result<PgPool, StorageError> {
        if options.max_connections == 0 || options.max_connections > 1000 {
            return Err(StorageError::Invalid);
        }
        let parsed = url::Url::parse(&options.database_url).map_err(|_| StorageError::Invalid)?;
        let ssl_modes: Vec<_> = parsed
            .query_pairs()
            .filter(|(key, _)| key == "sslmode")
            .map(|(_, value)| value.into_owned())
            .collect();
        let local = matches!(
            parsed.host_str(),
            Some("localhost" | "127.0.0.1" | "[::1]" | "::1")
        );
        if ssl_modes.len() != 1
            || !(ssl_modes[0] == "verify-full" || (local && ssl_modes[0] == "disable"))
        {
            return Err(StorageError::Invalid);
        }
        let connect_options = PgConnectOptions::from_str(&options.database_url)
            .map_err(|_| StorageError::Unavailable)?
            .disable_statement_logging();
        let schema = schema.map(str::to_owned);
        tokio::time::timeout(
            Duration::from_secs(10),
            PgPoolOptions::new()
                .max_connections(options.max_connections)
                .acquire_timeout(Duration::from_secs(5))
                .after_connect(move |connection, _| {
                    let schema = schema.clone();
                    Box::pin(async move {
                        sqlx::query("SET statement_timeout = '5s'")
                            .execute(&mut *connection)
                            .await?;
                        sqlx::query("SET lock_timeout = '5s'")
                            .execute(&mut *connection)
                            .await?;
                        sqlx::query("SET idle_in_transaction_session_timeout = '10s'")
                            .execute(&mut *connection)
                            .await?;
                        if let Some(schema) = schema {
                            sqlx::query("SELECT set_config('search_path', $1, false)")
                                .bind(schema)
                                .execute(connection)
                                .await?;
                        }
                        Ok(())
                    })
                })
                .connect_with(connect_options),
        )
        .await
        .map_err(|_| StorageError::Unavailable)?
        .map_err(StorageError::from)
    }
    pub async fn migrate(options: impl Into<PersistenceOptions>) -> Result<(), StorageError> {
        let options = match options.into() {
            PersistenceOptions::Database(options) => options,
            options @ PersistenceOptions::Filesystem { .. } => {
                drop(Self::open(options).await?);
                return Ok(());
            }
        };
        Encryption::new(&options.encryption_key, &options.key_id)?;
        let pool = Self::pool(&options, None).await?;
        let result = MIGRATOR.run(&pool).await.map_err(|_| StorageError::Schema);
        pool.close().await;
        result
    }
    pub async fn open(options: impl Into<PersistenceOptions>) -> Result<Self, StorageError> {
        let options = match options.into() {
            PersistenceOptions::Database(options) => options,
            PersistenceOptions::Filesystem { path, key, key_id } => {
                let encryption = Encryption::new(&key, &key_id)?;
                let file_encryption = encryption.clone();
                let file = tokio::task::spawn_blocking(move || {
                    filesystem::FileStorage::open(path, file_encryption)
                })
                .await
                .map_err(|_| StorageError::Unavailable)??;
                let pool = PgPoolOptions::new().connect_lazy_with(PgConnectOptions::new());
                return Ok(Self {
                    pool,
                    encryption,
                    file: Some(Arc::new(file)),
                });
            }
        };
        let encryption = Encryption::new(&options.encryption_key, &options.key_id)?;
        let pool = Self::pool(&options, None).await?;
        Self::from_pool(pool, encryption).await
    }
    async fn from_pool(pool: PgPool, encryption: Encryption) -> Result<Self, StorageError> {
        let storage = Self {
            pool,
            encryption,
            file: None,
        };
        let result = async {
            storage.schema_ready().await?;
            let mut transaction = storage.pool.begin().await?;
            let verifier: Option<Vec<u8>> = sqlx::query_scalar(
                "SELECT encryption_verifier FROM app_settings WHERE singleton = true FOR UPDATE",
            )
            .fetch_one(&mut *transaction)
            .await?;
            if let Some(verifier) = verifier {
                let plaintext = storage.encryption.decrypt(VERIFIER_ID, &verifier)?;
                if plaintext == legacy_verifier() {
                    let migrated = storage.encryption.encrypt(VERIFIER_ID, VERIFIER)?;
                    sqlx::query(
                        "UPDATE app_settings SET encryption_verifier = $1 WHERE singleton = true",
                    )
                    .bind(migrated)
                    .execute(&mut *transaction)
                    .await?;
                } else if plaintext.as_slice() != VERIFIER {
                    return Err(StorageError::Encryption);
                }
            } else {
                let verifier = storage.encryption.encrypt(VERIFIER_ID, VERIFIER)?;
                sqlx::query(
                    "UPDATE app_settings SET encryption_verifier = $1 WHERE singleton = true",
                )
                .bind(verifier)
                .execute(&mut *transaction)
                .await?;
            }
            transaction.commit().await?;
            Ok(())
        }
        .await;
        if let Err(error) = result {
            storage.pool.close().await;
            return Err(error);
        }
        Ok(storage)
    }
    async fn schema_ready(&self) -> Result<(), StorageError> {
        let rows =
            sqlx::query("SELECT version, checksum, success FROM _sqlx_migrations ORDER BY version")
                .fetch_all(&self.pool)
                .await
                .map_err(|_| StorageError::Schema)?;
        let expected: Vec<_> = MIGRATOR.iter().collect();
        if rows.len() != expected.len() {
            return Err(StorageError::Schema);
        }
        for (row, migration) in rows.iter().zip(expected) {
            if row.try_get::<i64, _>("version")? != migration.version
                || row.try_get::<Vec<u8>, _>("checksum")? != migration.checksum.as_ref()
                || !row.try_get::<bool, _>("success")?
            {
                return Err(StorageError::Schema);
            }
        }
        sqlx::query("SELECT c.config_encrypted, c.revision, v.revision, s.settings, s.encryption_verifier FROM app_settings s LEFT JOIN connections c ON false LEFT JOIN data_views v ON false WHERE s.singleton = true")
            .fetch_one(&self.pool).await.map_err(|_| StorageError::Schema)?;
        Ok(())
    }
    pub async fn ready(&self) -> Result<(), StorageError> {
        if let Some(file) = &self.file {
            return file.ready().await;
        }
        tokio::time::timeout(Duration::from_secs(6), async {
            sqlx::query("SELECT 1").execute(&self.pool).await?;
            self.schema_ready().await
        })
        .await
        .map_err(|_| StorageError::Unavailable)?
    }
    pub async fn close(&self) {
        if self.file.is_none() {
            self.pool.close().await;
        }
    }
    fn decode_connection(&self, row: PgRow) -> Result<StoredConnection, StorageError> {
        let integration_id: String = row.try_get("integration_id")?;
        let encrypted: Vec<u8> = row.try_get("config_encrypted")?;
        let config = serde_json::from_slice(&self.encryption.decrypt(&integration_id, &encrypted)?)
            .map_err(|_| StorageError::Configuration)?;
        Ok(StoredConnection {
            id: row.try_get("id")?,
            name: row.try_get("name")?,
            integration_id,
            config,
            revision: row.try_get("revision")?,
        })
    }
    pub async fn connections(&self) -> Result<Vec<StoredConnection>, StorageError> {
        if let Some(file) = &self.file {
            return file
                .read(|snapshot| Ok(snapshot.connections.values().cloned().collect()))
                .await;
        }
        sqlx::query("SELECT * FROM connections ORDER BY id")
            .fetch_all(&self.pool)
            .await?
            .into_iter()
            .map(|row| self.decode_connection(row))
            .collect()
    }
    pub async fn connection(&self, id: &str) -> Result<ResolvedConnection, StorageError> {
        if let Some(file) = &self.file {
            let id = id.to_owned();
            return file
                .read(move |snapshot| {
                    let record = snapshot
                        .connections
                        .get(&id)
                        .cloned()
                        .ok_or(StorageError::ConnectionNotFound)?;
                    let integration = descriptor(&record.integration_id)?;
                    let connector = connect(&record.integration_id, &record.config)
                        .map_err(|_| StorageError::Configuration)?;
                    Ok(ResolvedConnection {
                        record,
                        integration,
                        connector,
                    })
                })
                .await;
        }
        let row = sqlx::query("SELECT * FROM connections WHERE id = $1")
            .bind(id)
            .fetch_optional(&self.pool)
            .await?
            .ok_or(StorageError::ConnectionNotFound)?;
        let record = self.decode_connection(row)?;
        let integration = descriptor(&record.integration_id)?;
        let connector = connect(&record.integration_id, &record.config)
            .map_err(|_| StorageError::Configuration)?;
        Ok(ResolvedConnection {
            record,
            integration,
            connector,
        })
    }
    fn encrypted_record(&self, record: &StoredConnection) -> Result<Vec<u8>, StorageError> {
        validate_record(record)?;
        let plaintext =
            serde_json::to_vec(&record.config).map_err(|_| StorageError::Configuration)?;
        if plaintext.len() > 65536 {
            return Err(StorageError::Invalid);
        }
        self.encryption.encrypt(&record.integration_id, &plaintext)
    }
    pub async fn create_connection(
        &self,
        record: StoredConnection,
    ) -> Result<String, StorageError> {
        if let Some(file) = &self.file {
            return file.create_connection(record).await;
        }
        let encrypted = self.encrypted_record(&record)?;
        let revision = revision();
        sqlx::query("INSERT INTO connections (id, name, integration_id, config_encrypted, revision) VALUES ($1,$2,$3,$4,$5)")
            .bind(record.id).bind(record.name).bind(record.integration_id).bind(encrypted).bind(&revision).execute(&self.pool).await?;
        Ok(revision)
    }
    pub async fn update_connection(
        &self,
        existing_id: &str,
        expected: &str,
        record: StoredConnection,
    ) -> Result<String, StorageError> {
        if let Some(file) = &self.file {
            return file
                .update_connection(existing_id.to_owned(), expected.to_owned(), record)
                .await;
        }
        let encrypted = self.encrypted_record(&record)?;
        let mut transaction = self.pool.begin().await?;
        checked_connection(&mut transaction, existing_id, expected).await?;
        let integration_id: String =
            sqlx::query_scalar("SELECT integration_id FROM connections WHERE id = $1")
                .bind(existing_id)
                .fetch_one(&mut *transaction)
                .await?;
        if integration_id != record.integration_id {
            return Err(StorageError::Invalid);
        }
        let revision = revision();
        if existing_id != record.id {
            let ids: Vec<String> = sqlx::query_scalar(
                "SELECT id FROM data_views WHERE connection_id = $1 ORDER BY id FOR UPDATE",
            )
            .bind(existing_id)
            .fetch_all(&mut *transaction)
            .await?;
            for id in ids {
                sqlx::query("UPDATE data_views SET revision = $1 WHERE id = $2")
                    .bind(Uuid::new_v4().to_string())
                    .bind(id)
                    .execute(&mut *transaction)
                    .await?;
            }
        }
        sqlx::query(
            "UPDATE connections SET id=$1, name=$2, config_encrypted=$3, revision=$4 WHERE id=$5",
        )
        .bind(record.id)
        .bind(record.name)
        .bind(encrypted)
        .bind(&revision)
        .bind(existing_id)
        .execute(&mut *transaction)
        .await?;
        transaction.commit().await?;
        Ok(revision)
    }
    pub async fn remove_connection(&self, id: &str, expected: &str) -> Result<(), StorageError> {
        if let Some(file) = &self.file {
            return file
                .remove_connection(id.to_owned(), expected.to_owned())
                .await;
        }
        let mut transaction = self.pool.begin().await?;
        checked_connection(&mut transaction, id, expected).await?;
        sqlx::query("DELETE FROM connections WHERE id=$1")
            .bind(id)
            .execute(&mut *transaction)
            .await?;
        transaction.commit().await?;
        Ok(())
    }
    pub async fn views(&self) -> Result<Vec<DataView>, StorageError> {
        if let Some(file) = &self.file {
            return file
                .read(|snapshot| Ok(snapshot.views.values().cloned().collect()))
                .await;
        }
        sqlx::query("SELECT * FROM data_views ORDER BY id")
            .fetch_all(&self.pool)
            .await?
            .into_iter()
            .map(decode_view)
            .collect()
    }
    pub async fn save_view(
        &self,
        view: DataView,
        expected_connection: &str,
        expected_view: Option<&str>,
    ) -> Result<String, StorageError> {
        if let Some(file) = &self.file {
            return file
                .save_view(
                    view,
                    expected_connection.to_owned(),
                    expected_view.map(str::to_owned),
                )
                .await;
        }
        validate_view(&view)?;
        let mut transaction = self.pool.begin().await?;
        checked_connection(&mut transaction, &view.connection_id, expected_connection).await?;
        let revision = revision();
        if let Some(expected) = expected_view {
            Self::checked_view(&mut transaction, &view.id, expected).await?;
            sqlx::query("UPDATE data_views SET name=$2, connection_id=$3, index_name=$4, time_field=$5, revision=$6 WHERE id=$1")
                .bind(view.id).bind(view.name).bind(view.connection_id).bind(view.index).bind(view.time_field).bind(&revision).execute(&mut *transaction).await?;
        } else {
            sqlx::query("INSERT INTO data_views (id,name,connection_id,index_name,time_field,revision) VALUES ($1,$2,$3,$4,$5,$6)")
                .bind(view.id).bind(view.name).bind(view.connection_id).bind(view.index).bind(view.time_field).bind(&revision).execute(&mut *transaction).await?;
        }
        transaction.commit().await?;
        Ok(revision)
    }
    async fn checked_view(
        transaction: &mut Transaction<'_, Postgres>,
        id: &str,
        expected: &str,
    ) -> Result<(), StorageError> {
        let actual: String =
            sqlx::query_scalar("SELECT revision FROM data_views WHERE id=$1 FOR UPDATE")
                .bind(id)
                .fetch_optional(&mut **transaction)
                .await?
                .ok_or(StorageError::ViewNotFound)?;
        if actual != expected {
            return Err(StorageError::Conflict);
        }
        Ok(())
    }
    pub async fn remove_view(&self, id: &str, expected: &str) -> Result<(), StorageError> {
        if let Some(file) = &self.file {
            return file.remove_view(id.to_owned(), expected.to_owned()).await;
        }
        let mut transaction = self.pool.begin().await?;
        Self::checked_view(&mut transaction, id, expected).await?;
        sqlx::query("DELETE FROM data_views WHERE id=$1")
            .bind(id)
            .execute(&mut *transaction)
            .await?;
        transaction.commit().await?;
        Ok(())
    }
    pub async fn settings(&self) -> Result<PublicSettings, StorageError> {
        if let Some(file) = &self.file {
            return file.read(|snapshot| Ok(snapshot.settings.clone())).await;
        }
        let row = sqlx::query("SELECT settings, revision FROM app_settings WHERE singleton=true")
            .fetch_one(&self.pool)
            .await?;
        Ok(PublicSettings {
            settings: row.try_get("settings")?,
            revision: row.try_get("revision")?,
        })
    }
    pub async fn save_settings(
        &self,
        settings: Value,
        expected: &str,
    ) -> Result<String, StorageError> {
        if let Some(file) = &self.file {
            return file.save_settings(settings, expected.to_owned()).await;
        }
        validate_settings(&settings)?;
        let mut transaction = self.pool.begin().await?;
        let actual: String =
            sqlx::query_scalar("SELECT revision FROM app_settings WHERE singleton=true FOR UPDATE")
                .fetch_one(&mut *transaction)
                .await?;
        if actual != expected {
            return Err(StorageError::Conflict);
        }
        let revision = revision();
        sqlx::query("UPDATE app_settings SET settings=$1, revision=$2 WHERE singleton=true")
            .bind(settings)
            .bind(&revision)
            .execute(&mut *transaction)
            .await?;
        transaction.commit().await?;
        Ok(revision)
    }

    fn decode_ai_model(&self, row: PgRow) -> Result<StoredAiModel, StorageError> {
        let id: String = row.try_get("id")?;
        let encrypted: Vec<u8> = row.try_get("api_key_encrypted")?;
        let api_key = String::from_utf8(
            self.encryption
                .decrypt(&format!("ai-model:{id}"), &encrypted)?,
        )
        .map_err(|_| StorageError::Encryption)?;
        Ok(StoredAiModel {
            id,
            revision: row.try_get("revision")?,
            name: row.try_get("name")?,
            api_url: row.try_get("api_url")?,
            model: row.try_get("model")?,
            api_key,
        })
    }

    fn decode_ai_conversation(&self, row: PgRow) -> Result<StoredAiConversation, StorageError> {
        let id: String = row.try_get("id")?;
        let revision: String = row.try_get("revision")?;
        let updated_at_ms: i64 = row.try_get("updated_at_ms")?;
        let encrypted: Vec<u8> = row.try_get("content_encrypted")?;
        let plaintext = self
            .encryption
            .decrypt(&format!("ai-conversation:{id}"), &encrypted)?;
        let conversation: StoredAiConversation =
            serde_json::from_slice(&plaintext).map_err(|_| StorageError::Invalid)?;
        if conversation.id != id
            || conversation.revision != revision
            || conversation.updated_at_ms != updated_at_ms
        {
            return Err(StorageError::Invalid);
        }
        validate_ai_conversation(&conversation)?;
        Ok(conversation)
    }

    fn encrypted_ai_conversation(
        &self,
        conversation: &StoredAiConversation,
    ) -> Result<Vec<u8>, StorageError> {
        validate_ai_conversation(conversation)?;
        let plaintext = serde_json::to_vec(conversation).map_err(|_| StorageError::Invalid)?;
        self.encryption
            .encrypt(&format!("ai-conversation:{}", conversation.id), &plaintext)
    }

    pub async fn ai_models(&self) -> Result<Vec<StoredAiModel>, StorageError> {
        if let Some(file) = &self.file {
            return file
                .read(|snapshot| Ok(snapshot.ai_models.values().cloned().collect()))
                .await;
        }
        sqlx::query("SELECT * FROM ai_models ORDER BY id")
            .fetch_all(&self.pool)
            .await?
            .into_iter()
            .map(|row| self.decode_ai_model(row))
            .collect()
    }

    pub async fn ai_model(&self, id: &str) -> Result<StoredAiModel, StorageError> {
        if let Some(file) = &self.file {
            let id = id.to_owned();
            return file
                .read(move |snapshot| {
                    snapshot
                        .ai_models
                        .get(&id)
                        .cloned()
                        .ok_or(StorageError::AiModelNotFound)
                })
                .await;
        }
        let row = sqlx::query("SELECT * FROM ai_models WHERE id = $1")
            .bind(id)
            .fetch_optional(&self.pool)
            .await?
            .ok_or(StorageError::AiModelNotFound)?;
        self.decode_ai_model(row)
    }

    pub async fn create_ai_model(
        &self,
        mut model: StoredAiModel,
    ) -> Result<StoredAiModel, StorageError> {
        model.id = Uuid::new_v4().to_string();
        model.revision = revision();
        validate_ai_model(&model)?;
        if let Some(file) = &self.file {
            file.create_ai_model(model.clone()).await?;
            return Ok(model);
        }
        let encrypted = self
            .encryption
            .encrypt(&format!("ai-model:{}", model.id), model.api_key.as_bytes())?;
        sqlx::query("INSERT INTO ai_models (id,name,api_url,model,api_key_encrypted,revision) VALUES ($1,$2,$3,$4,$5,$6)")
            .bind(&model.id).bind(&model.name).bind(&model.api_url).bind(&model.model).bind(encrypted).bind(&model.revision)
            .execute(&self.pool).await?;
        Ok(model)
    }

    pub async fn update_ai_model(
        &self,
        id: &str,
        expected: &str,
        mut model: StoredAiModel,
    ) -> Result<StoredAiModel, StorageError> {
        validate_ai_model(&model)?;
        if model.id != id {
            return Err(StorageError::Invalid);
        }
        if let Some(file) = &self.file {
            file.update_ai_model(id.to_owned(), expected.to_owned(), model.clone())
                .await?;
            return self.ai_model(id).await;
        }
        let encrypted = self
            .encryption
            .encrypt(&format!("ai-model:{id}"), model.api_key.as_bytes())?;
        let mut transaction = self.pool.begin().await?;
        let actual: Option<String> =
            sqlx::query_scalar("SELECT revision FROM ai_models WHERE id=$1 FOR UPDATE")
                .bind(id)
                .fetch_optional(&mut *transaction)
                .await?;
        match actual {
            None => return Err(StorageError::AiModelNotFound),
            Some(actual) if actual != expected => return Err(StorageError::Conflict),
            _ => {}
        }
        model.revision = revision();
        sqlx::query("UPDATE ai_models SET name=$2,api_url=$3,model=$4,api_key_encrypted=$5,revision=$6 WHERE id=$1")
            .bind(id).bind(&model.name).bind(&model.api_url).bind(&model.model).bind(encrypted).bind(&model.revision)
            .execute(&mut *transaction).await?;
        transaction.commit().await?;
        Ok(model)
    }

    pub async fn remove_ai_model(&self, id: &str, expected: &str) -> Result<(), StorageError> {
        if let Some(file) = &self.file {
            return file
                .remove_ai_model(id.to_owned(), expected.to_owned())
                .await;
        }
        let mut transaction = self.pool.begin().await?;
        let actual: Option<String> =
            sqlx::query_scalar("SELECT revision FROM ai_models WHERE id=$1 FOR UPDATE")
                .bind(id)
                .fetch_optional(&mut *transaction)
                .await?;
        match actual {
            None => return Err(StorageError::AiModelNotFound),
            Some(actual) if actual != expected => return Err(StorageError::Conflict),
            _ => {}
        }
        sqlx::query("DELETE FROM ai_models WHERE id=$1")
            .bind(id)
            .execute(&mut *transaction)
            .await?;
        transaction.commit().await?;
        Ok(())
    }

    pub async fn ai_conversations(&self) -> Result<Vec<AiConversationSummary>, StorageError> {
        if let Some(file) = &self.file {
            return file
                .read(|snapshot| {
                    let mut summaries = snapshot
                        .ai_conversations
                        .values()
                        .map(StoredAiConversation::summary)
                        .collect::<Vec<_>>();
                    summaries.sort_by(|left, right| {
                        right
                            .updated_at_ms
                            .cmp(&left.updated_at_ms)
                            .then_with(|| left.id.cmp(&right.id))
                    });
                    Ok(summaries)
                })
                .await;
        }
        sqlx::query("SELECT * FROM ai_conversations ORDER BY updated_at_ms DESC, id")
            .fetch_all(&self.pool)
            .await?
            .into_iter()
            .map(|row| self.decode_ai_conversation(row).map(|item| item.summary()))
            .collect()
    }

    pub async fn ai_conversation(&self, id: &str) -> Result<StoredAiConversation, StorageError> {
        if let Some(file) = &self.file {
            let id = id.to_owned();
            return file
                .read(move |snapshot| {
                    snapshot
                        .ai_conversations
                        .get(&id)
                        .cloned()
                        .ok_or(StorageError::AiConversationNotFound)
                })
                .await;
        }
        let row = sqlx::query("SELECT * FROM ai_conversations WHERE id=$1")
            .bind(id)
            .fetch_optional(&self.pool)
            .await?
            .ok_or(StorageError::AiConversationNotFound)?;
        self.decode_ai_conversation(row)
    }

    pub async fn create_ai_conversation(
        &self,
        conversation: StoredAiConversation,
    ) -> Result<StoredAiConversation, StorageError> {
        validate_ai_conversation(&conversation)?;
        if let Some(file) = &self.file {
            file.create_ai_conversation(conversation.clone()).await?;
            return Ok(conversation);
        }
        let encrypted = self.encrypted_ai_conversation(&conversation)?;
        let mut transaction = self.pool.begin().await?;
        let count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM ai_conversations")
            .fetch_one(&mut *transaction)
            .await?;
        if count >= MAX_AI_CONVERSATIONS as i64 {
            return Err(StorageError::Invalid);
        }
        sqlx::query("INSERT INTO ai_conversations (id, revision, updated_at_ms, content_encrypted) VALUES ($1,$2,$3,$4)")
            .bind(&conversation.id)
            .bind(&conversation.revision)
            .bind(conversation.updated_at_ms)
            .bind(encrypted)
            .execute(&mut *transaction)
            .await?;
        transaction.commit().await?;
        Ok(conversation)
    }

    pub async fn append_ai_conversation_messages(
        &self,
        id: &str,
        expected: &str,
        messages: Vec<StoredAiConversationMessage>,
    ) -> Result<StoredAiConversation, StorageError> {
        if let Some(file) = &self.file {
            return file
                .append_ai_conversation_messages(id.to_owned(), expected.to_owned(), messages)
                .await;
        }
        let mut transaction = self.pool.begin().await?;
        let row = sqlx::query("SELECT * FROM ai_conversations WHERE id=$1 FOR UPDATE")
            .bind(id)
            .fetch_optional(&mut *transaction)
            .await?
            .ok_or(StorageError::AiConversationNotFound)?;
        let mut conversation = self.decode_ai_conversation(row)?;
        if conversation.revision != expected {
            return Err(StorageError::Conflict);
        }
        conversation.messages.extend(messages);
        conversation.revision = revision();
        conversation.updated_at_ms =
            conversation_timestamp().max(conversation.updated_at_ms.saturating_add(1));
        let encrypted = self.encrypted_ai_conversation(&conversation)?;
        sqlx::query("UPDATE ai_conversations SET revision=$2, updated_at_ms=$3, content_encrypted=$4 WHERE id=$1")
            .bind(id)
            .bind(&conversation.revision)
            .bind(conversation.updated_at_ms)
            .bind(encrypted)
            .execute(&mut *transaction)
            .await?;
        transaction.commit().await?;
        Ok(conversation)
    }

    pub async fn remove_ai_conversation(
        &self,
        id: &str,
        expected: &str,
    ) -> Result<(), StorageError> {
        if let Some(file) = &self.file {
            return file
                .remove_ai_conversation(id.to_owned(), expected.to_owned())
                .await;
        }
        let mut transaction = self.pool.begin().await?;
        let actual: Option<String> =
            sqlx::query_scalar("SELECT revision FROM ai_conversations WHERE id=$1 FOR UPDATE")
                .bind(id)
                .fetch_optional(&mut *transaction)
                .await?;
        match actual {
            None => return Err(StorageError::AiConversationNotFound),
            Some(actual) if actual != expected => return Err(StorageError::Conflict),
            _ => {}
        }
        sqlx::query("DELETE FROM ai_conversations WHERE id=$1")
            .bind(id)
            .execute(&mut *transaction)
            .await?;
        transaction.commit().await?;
        Ok(())
    }
}

#[cfg(test)]
pub mod tests;
