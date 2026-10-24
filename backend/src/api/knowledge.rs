use super::*;
use crate::storage::{KnowledgeEntry, KnowledgeKind, KnowledgeStatus};

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct KnowledgeInput {
    title: String,
    kind: KnowledgeKind,
    status: KnowledgeStatus,
    content: String,
    #[serde(default)]
    services: Vec<String>,
    #[serde(default)]
    environments: Vec<String>,
    #[serde(default)]
    connection_ids: Vec<String>,
    #[serde(default)]
    data_view_ids: Vec<String>,
    #[serde(default)]
    tags: Vec<String>,
    #[serde(default)]
    source_url: Option<String>,
}

impl From<KnowledgeInput> for KnowledgeEntry {
    fn from(input: KnowledgeInput) -> Self {
        Self {
            id: String::new(),
            revision: String::new(),
            created_at_ms: 0,
            updated_at_ms: 0,
            title: input.title.trim().to_owned(),
            kind: input.kind,
            status: input.status,
            content: input.content,
            services: input.services,
            environments: input.environments,
            connection_ids: input.connection_ids,
            data_view_ids: input.data_view_ids,
            tags: input.tags,
            source_url: input.source_url,
        }
    }
}

fn storage_error(error: StorageError) -> ApiError {
    match error {
        StorageError::Invalid => ApiError::InvalidKnowledge,
        other => other.into(),
    }
}

pub(super) async fn list(State(state): State<AppState>) -> Result<Json<Value>, ApiError> {
    Ok(Json(
        json!({"entries": state.storage.knowledge_entries().await?}),
    ))
}

pub(super) async fn read(
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> Result<Json<Value>, ApiError> {
    Ok(Json(
        json!({"entry": state.storage.knowledge_entry(&id).await?}),
    ))
}

pub(super) async fn create(
    State(state): State<AppState>,
    Json(input): Json<KnowledgeInput>,
) -> Result<(StatusCode, Json<Value>), ApiError> {
    let entry = state
        .storage
        .create_knowledge(input.into())
        .await
        .map_err(storage_error)?;
    Ok((StatusCode::CREATED, Json(json!({"entry": entry}))))
}

pub(super) async fn update(
    State(state): State<AppState>,
    Path(id): Path<String>,
    headers: HeaderMap,
    Json(input): Json<KnowledgeInput>,
) -> Result<Json<Value>, ApiError> {
    let expected = if_match(&headers)?;
    let entry = state
        .storage
        .update_knowledge(&id, &expected, input.into())
        .await
        .map_err(storage_error)?;
    Ok(Json(json!({"entry": entry})))
}

pub(super) async fn remove(
    State(state): State<AppState>,
    Path(id): Path<String>,
    headers: HeaderMap,
) -> Result<StatusCode, ApiError> {
    let expected = if_match(&headers)?;
    state.storage.remove_knowledge(&id, &expected).await?;
    Ok(StatusCode::NO_CONTENT)
}
