use super::*;

pub(super) const MAX_KNOWLEDGE_ENTRIES: usize = 128;

#[derive(Clone, Copy, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum KnowledgeKind {
    ServiceContext,
    Runbook,
    ResolvedIncident,
}

#[derive(Clone, Copy, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum KnowledgeStatus {
    Draft,
    Published,
    Archived,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct KnowledgeEntry {
    #[serde(default)]
    pub id: String,
    #[serde(default)]
    pub revision: String,
    pub title: String,
    pub kind: KnowledgeKind,
    pub status: KnowledgeStatus,
    pub content: String,
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
    if !valid_id(&entry.id)
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
    {
        return Err(StorageError::Invalid);
    }
    Ok(())
}

impl Storage {
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
        entry.id = Uuid::new_v4().to_string();
        entry.revision = revision();
        entry.created_at_ms = conversation_timestamp();
        entry.updated_at_ms = entry.created_at_ms;
        validate_entry(&entry)?;
        if let Some(file) = &self.file {
            return file
                .mutate(move |snapshot| {
                    if snapshot.knowledge.len() >= MAX_KNOWLEDGE_ENTRIES {
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
        let count: i64 = sqlx::query_scalar("SELECT count(*) FROM knowledge_entries")
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
