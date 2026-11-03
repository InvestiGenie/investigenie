use super::*;

pub(super) const MAX_KNOWLEDGE_ENTRIES: usize = 128;
pub const MAX_INVESTIGATION_PLAYBOOK_STEPS: usize = 8;
pub const MAX_INVESTIGATION_PLAYBOOK_QUERY_BYTES: usize = 8_192;
pub const DEFAULT_INVESTIGATION_PLAYBOOK_ID: &str = "00000000-0000-4000-8000-000000000001";

#[derive(Clone, Copy, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum KnowledgeKind {
    ServiceContext,
    Runbook,
    ResolvedIncident,
    InvestigationPlaybook,
}

#[derive(Clone, Copy, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum KnowledgeStatus {
    Draft,
    Published,
    Archived,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct InvestigationPlaybookStep {
    pub name: String,
    #[serde(default)]
    pub data_view_ids: Vec<String>,
    pub query: String,
    #[serde(default)]
    pub breakdown_field: Option<String>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct KnowledgeEntry {
    #[serde(default)]
    pub id: String,
    #[serde(default)]
    pub revision: String,
    #[serde(default)]
    pub is_default: bool,
    pub title: String,
    pub kind: KnowledgeKind,
    pub status: KnowledgeStatus,
    pub content: String,
    #[serde(default)]
    pub steps: Vec<InvestigationPlaybookStep>,
    #[serde(default)]
    pub services: Vec<String>,
    #[serde(default)]
    pub environments: Vec<String>,
    #[serde(default)]
    pub connection_ids: Vec<String>,
    #[serde(default)]
    pub data_view_ids: Vec<String>,
    #[serde(default)]
    pub tags: Vec<String>,
    #[serde(default)]
    pub source_url: Option<String>,
    #[serde(default)]
    pub created_at_ms: i64,
    #[serde(default)]
    pub updated_at_ms: i64,
}

pub(super) fn validate_entry(entry: &KnowledgeEntry) -> Result<(), StorageError> {
    let labels_valid = [&entry.services, &entry.environments, &entry.tags]
        .into_iter()
        .all(|labels| {
            labels.len() <= 20
                && labels
                    .iter()
                    .all(|label| !label.trim().is_empty() && label.len() <= 128)
        });
    let scopes_valid = [&entry.connection_ids, &entry.data_view_ids]
        .into_iter()
        .all(|ids| ids.len() <= 20 && ids.iter().all(|id| valid_id(id)));
    let source_valid = entry.source_url.as_ref().is_none_or(|source| {
        source.len() <= 2048
            && reqwest::Url::parse(source).is_ok_and(|url| {
                matches!(url.scheme(), "http" | "https")
                    && url.has_host()
                    && url.username().is_empty()
                    && url.password().is_none()
            })
    });
    let steps_valid = match entry.kind {
        KnowledgeKind::InvestigationPlaybook => {
            !entry.steps.is_empty()
                && entry.steps.len() <= MAX_INVESTIGATION_PLAYBOOK_STEPS
                && entry.steps.iter().all(|step| {
                    !step.name.trim().is_empty()
                        && step.name.len() <= 128
                        && step.query.len() <= MAX_INVESTIGATION_PLAYBOOK_QUERY_BYTES
                        && step.data_view_ids.len() <= 5
                        && step.data_view_ids.iter().all(|id| valid_id(id))
                        && (step.query.trim().is_empty() || !step.data_view_ids.is_empty())
                        && step
                            .breakdown_field
                            .as_ref()
                            .is_none_or(|field| !field.trim().is_empty() && field.len() <= 1024)
                        && (step.breakdown_field.is_none() || !step.data_view_ids.is_empty())
                })
        }
        _ => entry.steps.is_empty(),
    };
    let default_valid = !entry.is_default
        || (entry.id == DEFAULT_INVESTIGATION_PLAYBOOK_ID
            && entry.kind == KnowledgeKind::InvestigationPlaybook);
    if !valid_id(&entry.id)
        || (entry.id == DEFAULT_INVESTIGATION_PLAYBOOK_ID && !entry.is_default)
        || !default_valid
        || Uuid::parse_str(&entry.revision).is_err()
        || entry.title.trim().is_empty()
        || entry.title.len() > 256
        || entry.content.trim().is_empty()
        || entry.content.len() > 32_768
        || entry.created_at_ms <= 0
        || entry.updated_at_ms < entry.created_at_ms
        || !labels_valid
        || !scopes_valid
        || !source_valid
        || !steps_valid
    {
        return Err(StorageError::Invalid);
    }
    Ok(())
}

fn default_investigation_playbook() -> KnowledgeEntry {
    let now = conversation_timestamp();
    KnowledgeEntry {
        id: DEFAULT_INVESTIGATION_PLAYBOOK_ID.to_owned(),
        revision: revision(),
        is_default: true,
        title: "Default investigation playbook".to_owned(),
        kind: KnowledgeKind::InvestigationPlaybook,
        status: KnowledgeStatus::Published,
        content: "Collect bounded activity aggregates for each selected Data View. Compare a bounded investigation window with the immediately preceding window of the same duration. For All time, report descriptive aggregates without a baseline.".to_owned(),
        steps: vec![InvestigationPlaybookStep {
            name: "Overall activity".to_owned(),
            data_view_ids: Vec::new(),
            query: String::new(),
            breakdown_field: None,
        }],
        services: Vec::new(),
        environments: Vec::new(),
        connection_ids: Vec::new(),
        data_view_ids: Vec::new(),
        tags: vec!["default".to_owned()],
        source_url: None,
        created_at_ms: now,
        updated_at_ms: now,
    }
}

impl Storage {
    pub async fn ensure_default_investigation_playbook(&self) -> Result<(), StorageError> {
        let entry = default_investigation_playbook();
        if let Some(file) = &self.file {
            let id = entry.id.clone();
            let exists = file
                .read(move |snapshot| Ok(snapshot.knowledge.contains_key(&id)))
                .await?;
            if exists {
                return Ok(());
            }
            return file
                .mutate(move |snapshot| {
                    snapshot.knowledge.entry(entry.id.clone()).or_insert(entry);
                    Ok(())
                })
                .await;
        }
        let encrypted = self.encode_knowledge(&entry)?;
        sqlx::query(
            "INSERT INTO knowledge_entries (id,revision,entry_encrypted) VALUES ($1,$2,$3) ON CONFLICT (id) DO NOTHING",
        )
        .bind(&entry.id)
        .bind(&entry.revision)
        .bind(encrypted)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    pub async fn knowledge_entries(&self) -> Result<Vec<KnowledgeEntry>, StorageError> {
        let mut entries: Vec<KnowledgeEntry> = if let Some(file) = &self.file {
            file.read(|snapshot| Ok(snapshot.knowledge.values().cloned().collect()))
                .await?
        } else {
            sqlx::query("SELECT * FROM knowledge_entries")
                .fetch_all(&self.pool)
                .await?
                .into_iter()
                .map(|row| self.decode_knowledge(row))
                .collect::<Result<_, _>>()?
        };
        entries.sort_by(|left, right| {
            right
                .updated_at_ms
                .cmp(&left.updated_at_ms)
                .then(left.id.cmp(&right.id))
        });
        Ok(entries)
    }

    pub async fn knowledge_entry(&self, id: &str) -> Result<KnowledgeEntry, StorageError> {
        if let Some(file) = &self.file {
            let id = id.to_owned();
            return file
                .read(move |snapshot| {
                    snapshot
                        .knowledge
                        .get(&id)
                        .cloned()
                        .ok_or(StorageError::KnowledgeNotFound)
                })
                .await;
        }
        let row = sqlx::query("SELECT * FROM knowledge_entries WHERE id=$1")
            .bind(id)
            .fetch_optional(&self.pool)
            .await?
            .ok_or(StorageError::KnowledgeNotFound)?;
        self.decode_knowledge(row)
    }

    pub async fn create_knowledge(
        &self,
        mut entry: KnowledgeEntry,
    ) -> Result<KnowledgeEntry, StorageError> {
        if !entry.is_default {
            entry.id = Uuid::new_v4().to_string();
        }
        entry.revision = revision();
        entry.created_at_ms = conversation_timestamp();
        entry.updated_at_ms = entry.created_at_ms;
        validate_entry(&entry)?;
        if let Some(file) = &self.file {
            return file
                .mutate(move |snapshot| {
                    if snapshot
                        .knowledge
                        .values()
                        .filter(|entry| !entry.is_default)
                        .count()
                        >= MAX_KNOWLEDGE_ENTRIES
                    {
                        return Err(StorageError::Invalid);
                    }
                    snapshot.knowledge.insert(entry.id.clone(), entry.clone());
                    Ok(entry)
                })
                .await;
        }
        let encrypted = self.encode_knowledge(&entry)?;
        let mut transaction = self.pool.begin().await?;
        sqlx::query("SELECT pg_advisory_xact_lock(1229407566, 2)")
            .execute(&mut *transaction)
            .await?;
        let count: i64 =
            sqlx::query_scalar("SELECT count(*) FROM knowledge_entries WHERE id <> $1")
                .bind(DEFAULT_INVESTIGATION_PLAYBOOK_ID)
                .fetch_one(&mut *transaction)
                .await?;
        if count >= MAX_KNOWLEDGE_ENTRIES as i64 {
            return Err(StorageError::Invalid);
        }
        sqlx::query(
            "INSERT INTO knowledge_entries (id,revision,entry_encrypted) VALUES ($1,$2,$3)",
        )
        .bind(&entry.id)
        .bind(&entry.revision)
        .bind(encrypted)
        .execute(&mut *transaction)
        .await?;
        transaction.commit().await?;
        Ok(entry)
    }

    pub async fn update_knowledge(
        &self,
        id: &str,
        expected: &str,
        mut entry: KnowledgeEntry,
    ) -> Result<KnowledgeEntry, StorageError> {
        entry.id = id.to_owned();
        entry.revision = revision();
        entry.updated_at_ms = conversation_timestamp();
        if let Some(file) = &self.file {
            let expected = expected.to_owned();
            return file
                .mutate(move |snapshot| {
                    let current = snapshot
                        .knowledge
                        .get(&entry.id)
                        .ok_or(StorageError::KnowledgeNotFound)?;
                    if current.revision != expected {
                        return Err(StorageError::Conflict);
                    }
                    entry.created_at_ms = current.created_at_ms;
                    entry.is_default = current.is_default;
                    entry.updated_at_ms = entry.updated_at_ms.max(current.updated_at_ms);
                    validate_entry(&entry)?;
                    snapshot.knowledge.insert(entry.id.clone(), entry.clone());
                    Ok(entry)
                })
                .await;
        }
        let mut transaction = self.pool.begin().await?;
        let row = sqlx::query("SELECT * FROM knowledge_entries WHERE id=$1 FOR UPDATE")
            .bind(id)
            .fetch_optional(&mut *transaction)
            .await?
            .ok_or(StorageError::KnowledgeNotFound)?;
        let current = self.decode_knowledge(row)?;
        if current.revision != expected {
            return Err(StorageError::Conflict);
        }
        entry.created_at_ms = current.created_at_ms;
        entry.is_default = current.is_default;
        entry.updated_at_ms = entry.updated_at_ms.max(current.updated_at_ms);
        validate_entry(&entry)?;
        sqlx::query("UPDATE knowledge_entries SET revision=$2,entry_encrypted=$3 WHERE id=$1")
            .bind(id)
            .bind(&entry.revision)
            .bind(self.encode_knowledge(&entry)?)
            .execute(&mut *transaction)
            .await?;
        transaction.commit().await?;
        Ok(entry)
    }

    pub async fn remove_knowledge(&self, id: &str, expected: &str) -> Result<(), StorageError> {
        if let Some(file) = &self.file {
            let id = id.to_owned();
            let expected = expected.to_owned();
            return file
                .mutate(move |snapshot| {
                    let current = snapshot
                        .knowledge
                        .get(&id)
                        .ok_or(StorageError::KnowledgeNotFound)?;
                    if current.revision != expected {
                        return Err(StorageError::Conflict);
                    }
                    if current.is_default {
                        return Err(StorageError::Invalid);
                    }
                    snapshot.knowledge.remove(&id);
                    Ok(())
                })
                .await;
        }
        let mut transaction = self.pool.begin().await?;
        let actual: Option<String> =
            sqlx::query_scalar("SELECT revision FROM knowledge_entries WHERE id=$1 FOR UPDATE")
                .bind(id)
                .fetch_optional(&mut *transaction)
                .await?;
        match actual {
            None => return Err(StorageError::KnowledgeNotFound),
            Some(actual) if actual != expected => return Err(StorageError::Conflict),
            _ => {}
        }
        if id == DEFAULT_INVESTIGATION_PLAYBOOK_ID {
            return Err(StorageError::Invalid);
        }
        sqlx::query("DELETE FROM knowledge_entries WHERE id=$1")
            .bind(id)
            .execute(&mut *transaction)
            .await?;
        transaction.commit().await?;
        Ok(())
    }

    fn encode_knowledge(&self, entry: &KnowledgeEntry) -> Result<Vec<u8>, StorageError> {
        let plaintext = serde_json::to_vec(entry).map_err(|_| StorageError::Invalid)?;
        self.encryption
            .encrypt(&format!("knowledge:{}", entry.id), &plaintext)
    }

    fn decode_knowledge(&self, row: PgRow) -> Result<KnowledgeEntry, StorageError> {
        let id: String = row.try_get("id")?;
        let actual: String = row.try_get("revision")?;
        let bytes: Vec<u8> = row.try_get("entry_encrypted")?;
        let plaintext = self
            .encryption
            .decrypt(&format!("knowledge:{id}"), &bytes)?;
        let entry: KnowledgeEntry =
            serde_json::from_slice(&plaintext).map_err(|_| StorageError::Invalid)?;
        validate_entry(&entry)?;
        if entry.id != id || entry.revision != actual {
            return Err(StorageError::Invalid);
        }
        Ok(entry)
    }
}
