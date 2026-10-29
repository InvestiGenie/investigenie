use std::{
    collections::HashMap,
    path::Path as FilePath,
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};

use axum::{
    extract::{DefaultBodyLimit, Path, Request, State},
    http::{header, HeaderMap, HeaderValue, Method, StatusCode},
    response::{IntoResponse, Response},
    routing::{get, post, put},
    Json, Router,
};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use tokio::sync::{OwnedSemaphorePermit, Semaphore};
use tower_http::cors::CorsLayer;
use tower_http::services::{ServeDir, ServeFile};
use tracing::Instrument;

use crate::ai::{self, InvestigationRequest};
use crate::ai_connectors::AiSettings;
use crate::data_connectors::{
    connect, editable_configuration, investigation_dialect, public_configuration, registered,
    validate_registry, ConnectionConfig, Connector, ConnectorError, IntegrationDescriptor,
};
use crate::investigation_dialect::{dialect_for_language, translate_query};
use crate::search::{SearchRequest, SearchWindow};
use crate::storage::{
    AiConversationSummary, AiModelSummary, DataView, ResolvedConnection, Storage, StorageError,
    StoredAiConversation, StoredAiConversationMessage, StoredAiModel, StoredConnection,
    MAX_AI_CONVERSATION_MESSAGES,
};

const MAX_CONCURRENT_UPSTREAM_REQUESTS: usize = 64;
const MAX_TRACKED_INVESTIGATIONS: usize = 64;
const FINISHED_INVESTIGATION_RETENTION: Duration = Duration::from_secs(30 * 60);

mod knowledge;

type InvestigationJobs = Arc<Mutex<HashMap<String, Arc<ai::InvestigationJob>>>>;

#[derive(Clone)]
struct AppState {
    storage: Storage,
    upstream_slots: Arc<Semaphore>,
    ai_slots: Arc<Semaphore>,
    investigations: InvestigationJobs,
    ai_probe_timeout: Duration,
}

#[derive(Debug, Deserialize)]
struct CreateDataViewRequest {
    name: String,
    connection_id: String,
    index: String,
    time_field: String,
}

#[derive(Debug, Deserialize)]
struct AddConnectionRequest {
    name: String,
    connection_type: String,
    url: String,
    #[serde(flatten)]
    settings: ConnectionConfig,
}

#[derive(Debug, Deserialize)]
struct UpdateConnectionRequest {
    name: String,
    url: String,
    #[serde(flatten)]
    settings: ConnectionConfig,
}

#[derive(Debug, Serialize)]
struct ConnectionSummary {
    id: String,
    revision: String,
    name: String,
    connection_type: &'static str,
    url: String,
    database: Option<String>,
    status: &'static str,
    integration: IntegrationDescriptor,
    configuration: ConnectionConfig,
}

pub fn router(web_origin: HeaderValue, storage: Storage, ai_probe_timeout: Duration) -> Router {
    validate_registry();
    let state = AppState {
        storage,
        upstream_slots: Arc::new(Semaphore::new(MAX_CONCURRENT_UPSTREAM_REQUESTS)),
        ai_slots: Arc::new(Semaphore::new(4)),
        investigations: Arc::default(),
        ai_probe_timeout,
    };
    let cors = CorsLayer::new()
        .allow_origin(web_origin)
        .allow_methods([Method::GET, Method::POST, Method::PUT, Method::DELETE])
        .allow_headers([header::CONTENT_TYPE, header::IF_MATCH]);

    Router::new()
        .route("/api/health", get(health))
        .route("/api/health/live", get(health))
        .route("/api/health/ready", get(readiness))
        .route("/api/settings", get(settings).put(update_settings))
        .route("/api/integrations", get(integrations))
        .route(
            "/api/knowledge",
            get(knowledge::list)
                .post(knowledge::create)
                .layer(DefaultBodyLimit::max(65_536)),
        )
        .route(
            "/api/knowledge/{id}",
            get(knowledge::read)
                .put(knowledge::update)
                .delete(knowledge::remove)
                .layer(DefaultBodyLimit::max(65_536)),
        )
        .route("/api/connections", get(connections).post(add_connection))
        .route("/api/data-views", get(data_views).post(create_data_view))
        .route(
            "/api/data-views/{data_view_id}",
            put(update_data_view).delete(remove_data_view),
        )
        .route(
            "/api/connections/{connection_id}",
            put(update_connection).delete(remove_connection),
        )
        .route(
            "/api/connections/{connection_id}/status",
            get(connection_status),
        )
        .route(
            "/api/connections/{connection_id}/indices",
            get(connection_indices),
        )
        .route(
            "/api/connections/{connection_id}/indices/{index}/fields",
            get(connection_fields),
        )
        .route(
            "/api/connections/{connection_id}/indices/{index}/documents/{id}",
            get(connection_document),
        )
        .route(
            "/api/connections/{connection_id}/search",
            post(connection_search),
        )
        .route(
            "/api/ai/investigate",
            post(investigate).layer(DefaultBodyLimit::max(65_536)),
        )
        .route(
            "/api/ai/investigations",
            post(start_investigation).layer(DefaultBodyLimit::max(65_536)),
        )
        .route("/api/ai/investigations/{id}", get(investigation_status))
        .route(
            "/api/ai/investigations/{id}/decision",
            post(decide_investigation),
        )
        .route("/api/ai/conversations", get(ai_conversations))
        .route(
            "/api/ai/conversations/{id}",
            get(ai_conversation).delete(remove_ai_conversation),
        )
        .route(
            "/api/ai/conversations/{id}/messages",
            post(continue_ai_conversation).layer(DefaultBodyLimit::max(65_536)),
        )
        .route("/api/ai/models", get(ai_models).post(create_ai_model))
        .route(
            "/api/ai/models/{model_id}",
            put(update_ai_model).delete(remove_ai_model),
        )
        .with_state(state)
        .layer(cors)
}

/// Serves the built UI from the same origin as the API; unknown non-API paths load the SPA.
pub fn with_web(router: Router, directory: &FilePath) -> Router {
    let files = ServeDir::new(directory).fallback(ServeFile::new(directory.join("index.html")));
    router.fallback(move |request: Request| {
        let mut files = files.clone();
        async move {
            if request.uri().path().starts_with("/api/") {
                return (
                    StatusCode::NOT_FOUND,
                    Json(
                        json!({ "error": { "code": "not_found", "message": "Unknown API route" } }),
                    ),
                )
                    .into_response();
            }
            match files.try_call(request).await {
                Ok(response) => response.map(axum::body::Body::new),
                Err(_) => StatusCode::INTERNAL_SERVER_ERROR.into_response(),
            }
        }
    })
}

fn model_settings(model: &StoredAiModel, timeout: Duration) -> AiSettings {
    AiSettings {
        api_url: model.api_url.clone(),
        api_key: model.api_key.clone(),
        model: model.model.clone(),
        timeout,
    }
}

async fn prepare_ai_call(
    state: &AppState,
    model_id: &str,
    timeout: Duration,
) -> Result<(AiSettings, OwnedSemaphorePermit), ApiError> {
    let model = state.storage.ai_model(model_id).await?;
    let settings = model_settings(&model, timeout);
    let permit = state
        .ai_slots
        .clone()
        .try_acquire_owned()
        .map_err(|_| ApiError::Unavailable)?;
    Ok((settings, permit))
}

async fn probe_ai_model(model: &StoredAiModel, timeout: Duration) -> Result<(), ApiError> {
    ai::test_model(&model_settings(model, timeout))
        .await
        .map_err(ApiError::AiModelTest)
}

fn ai_model_save_error(error: StorageError) -> ApiError {
    match error {
        StorageError::Duplicate => ApiError::DuplicateAiModel,
        error => ApiError::Storage(error),
    }
}

async fn investigate(
    State(state): State<AppState>,
    Json(request): Json<InvestigationRequest>,
) -> Result<Json<ai::InvestigationResponse>, ApiError> {
    let timeout = ai::provider_timeout(&request).map_err(ApiError::Investigation)?;
    let (settings, permit) = prepare_ai_call(&state, &request.ai_model_id, timeout).await?;
    let result = ai::investigate(
        &state.storage,
        &settings,
        request,
        &state.upstream_slots,
        &ai::Control::unattended(),
    )
    .await;
    drop(permit);
    result.map(Json).map_err(ApiError::Investigation)
}

async fn start_investigation(
    State(state): State<AppState>,
    Json(request): Json<InvestigationRequest>,
) -> Result<(StatusCode, Json<Value>), ApiError> {
    let timeout = ai::provider_timeout(&request).map_err(ApiError::Investigation)?;
    let budget = ai::time_budget(&request).map_err(ApiError::Investigation)?;
    let (settings, permit) = prepare_ai_call(&state, &request.ai_model_id, timeout).await?;
    let job = ai::InvestigationJob::new(uuid::Uuid::new_v4().to_string(), budget);
    {
        let mut jobs = state
            .investigations
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let cutoff = Instant::now()
            .checked_sub(FINISHED_INVESTIGATION_RETENTION)
            .unwrap_or_else(Instant::now);
        jobs.retain(|_, job| !job.finished_before(cutoff));
        if jobs.len() >= MAX_TRACKED_INVESTIGATIONS {
            jobs.retain(|_, job| !job.finished_before(Instant::now()));
        }
        if jobs.len() >= MAX_TRACKED_INVESTIGATIONS {
            return Err(ApiError::Unavailable);
        }
        jobs.insert(job.id.clone(), job.clone());
    }
    let conversation = StoredAiConversation::new(
        job.id.clone(),
        request.ai_model_id.clone(),
        request.data_view_ids.clone(),
        request.question.clone(),
    );
    let conversation = match state.storage.create_ai_conversation(conversation).await {
        Ok(conversation) => conversation,
        Err(error) => {
            state
                .investigations
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .remove(&job.id);
            return Err(error.into());
        }
    };
    let task_job = job.clone();
    let conversation_storage = state.storage.clone();
    let conversation_id = conversation.id.clone();
    let conversation_revision = conversation.revision.clone();
    let job_span = tracing::error_span!("investigation", job_id = %job.id);
    tokio::spawn(
        async move {
            let _permit = permit;
            let control = ai::Control::for_job(task_job.clone());
            let result = ai::investigate(
                &state.storage,
                &settings,
                request,
                &state.upstream_slots,
                &control,
            )
            .await;
            let message = match &result {
                Ok(result) => {
                    let result_value = serde_json::to_value(result).unwrap_or(Value::Null);
                    let summary = result_value
                        .get("summary")
                        .and_then(Value::as_str)
                        .unwrap_or("Investigation complete")
                        .to_owned();
                    StoredAiConversationMessage {
                        role: "assistant".to_owned(),
                        content: summary,
                        result: Some(result_value),
                    }
                }
                Err(error) => StoredAiConversationMessage {
                    role: "assistant".to_owned(),
                    content: error.message.clone(),
                    result: None,
                },
            };
            if conversation_storage
                .append_ai_conversation_messages(
                    &conversation_id,
                    &conversation_revision,
                    vec![message],
                )
                .await
                .is_err()
            {
                tracing::warn!(
                    conversation_id,
                    error_code = "conversation_persistence_failed",
                    "Could not persist AI investigation result"
                );
            }
            task_job.finish(result);
        }
        .instrument(job_span),
    );
    Ok((
        StatusCode::ACCEPTED,
        Json(json!({ "investigation": job.snapshot() })),
    ))
}

fn find_investigation(state: &AppState, id: &str) -> Result<Arc<ai::InvestigationJob>, ApiError> {
    state
        .investigations
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .get(id)
        .cloned()
        .ok_or(ApiError::InvestigationNotFound)
}

async fn investigation_status(
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> Result<Json<Value>, ApiError> {
    let job = find_investigation(&state, &id)?;
    Ok(Json(json!({ "investigation": job.snapshot() })))
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct InvestigationDecisionRequest {
    action: ai::Decision,
}

async fn decide_investigation(
    State(state): State<AppState>,
    Path(id): Path<String>,
    Json(request): Json<InvestigationDecisionRequest>,
) -> Result<Json<Value>, ApiError> {
    let job = find_investigation(&state, &id)?;
    job.decide(request.action)
        .map_err(ApiError::InvestigationDecision)?;
    Ok(Json(json!({ "investigation": job.snapshot() })))
}

async fn ai_conversations(State(state): State<AppState>) -> Result<Json<Value>, ApiError> {
    let conversations: Vec<AiConversationSummary> = state.storage.ai_conversations().await?;
    Ok(Json(json!({ "conversations": conversations })))
}

async fn ai_conversation(
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> Result<Json<Value>, ApiError> {
    Ok(Json(json!({
        "conversation": state.storage.ai_conversation(&id).await?
    })))
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct ContinueConversationRequest {
    message: String,
}

async fn continue_ai_conversation(
    State(state): State<AppState>,
    Path(id): Path<String>,
    headers: HeaderMap,
    Json(request): Json<ContinueConversationRequest>,
) -> Result<Json<Value>, ApiError> {
    let expected = if_match(&headers)?;
    let message = request.message.trim();
    if message.is_empty() || message.len() > 8_000 {
        return Err(ApiError::InvalidConversationMessage);
    }
    let conversation = state.storage.ai_conversation(&id).await?;
    if conversation.revision != expected {
        return Err(ApiError::Storage(StorageError::Conflict));
    }
    if conversation.messages.len() + 2 > MAX_AI_CONVERSATION_MESSAGES {
        return Err(ApiError::InvalidConversationMessage);
    }
    let (settings, _permit) =
        prepare_ai_call(&state, &conversation.model_id, Duration::from_secs(60)).await?;
    let answer = ai::continue_conversation(&settings, &conversation.messages, message)
        .await
        .map_err(ApiError::Investigation)?;
    let updated = state
        .storage
        .append_ai_conversation_messages(
            &id,
            &expected,
            vec![
                StoredAiConversationMessage {
                    role: "user".to_owned(),
                    content: message.to_owned(),
                    result: None,
                },
                StoredAiConversationMessage {
                    role: "assistant".to_owned(),
                    content: answer,
                    result: None,
                },
            ],
        )
        .await?;
    Ok(Json(json!({ "conversation": updated })))
}

async fn remove_ai_conversation(
    State(state): State<AppState>,
    Path(id): Path<String>,
    headers: HeaderMap,
) -> Result<StatusCode, ApiError> {
    let expected = if_match(&headers)?;
    state.storage.remove_ai_conversation(&id, &expected).await?;
    Ok(StatusCode::NO_CONTENT)
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct CreateAiModelRequest {
    name: String,
    api_url: String,
    model: String,
    api_key: String,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct UpdateAiModelRequest {
    name: String,
    api_url: String,
    model: String,
    api_key: Option<String>,
}

fn provider_settings(
    name: String,
    api_url: String,
    model: String,
    api_key: String,
) -> Result<StoredAiModel, ApiError> {
    let record = StoredAiModel {
        id: uuid::Uuid::new_v4().to_string(),
        revision: String::new(),
        name: name.trim().to_owned(),
        api_url: api_url.trim().trim_end_matches('/').to_owned(),
        model: model.trim().to_owned(),
        api_key,
    };
    crate::storage::validate_ai_model(&record).map_err(|_| ApiError::InvalidAiModel)?;
    Ok(record)
}

async fn ai_models(State(state): State<AppState>) -> Result<Json<Value>, ApiError> {
    let models = state
        .storage
        .ai_models()
        .await?
        .iter()
        .map(AiModelSummary::from)
        .collect::<Vec<_>>();
    Ok(Json(json!({"models":models})))
}

async fn create_ai_model(
    State(state): State<AppState>,
    Json(request): Json<CreateAiModelRequest>,
) -> Result<Json<Value>, ApiError> {
    let model = provider_settings(
        request.name,
        request.api_url,
        request.model,
        request.api_key,
    )?;
    probe_ai_model(&model, state.ai_probe_timeout).await?;
    let saved = state
        .storage
        .create_ai_model(model)
        .await
        .map_err(ai_model_save_error)?;
    Ok(Json(json!({"model":AiModelSummary::from(&saved)})))
}

async fn update_ai_model(
    State(state): State<AppState>,
    Path(id): Path<String>,
    headers: HeaderMap,
    Json(request): Json<UpdateAiModelRequest>,
) -> Result<Json<Value>, ApiError> {
    let expected = if_match(&headers)?;
    let current = state.storage.ai_model(&id).await?;
    if current.revision != expected {
        return Err(StorageError::Conflict.into());
    }
    let api_key = request.api_key.unwrap_or(current.api_key);
    let mut model = provider_settings(request.name, request.api_url, request.model, api_key)?;
    model.id = id.clone();
    probe_ai_model(&model, state.ai_probe_timeout).await?;
    let saved = state
        .storage
        .update_ai_model(&id, &expected, model)
        .await
        .map_err(ai_model_save_error)?;
    Ok(Json(json!({"model":AiModelSummary::from(&saved)})))
}

async fn remove_ai_model(
    State(state): State<AppState>,
    Path(id): Path<String>,
    headers: HeaderMap,
) -> Result<StatusCode, ApiError> {
    let expected = if_match(&headers)?;
    state.storage.remove_ai_model(&id, &expected).await?;
    Ok(StatusCode::NO_CONTENT)
}

async fn health() -> Json<Value> {
    Json(json!({ "status": "ok" }))
}

async fn readiness(State(state): State<AppState>) -> Result<Json<Value>, ApiError> {
    state.storage.ready().await?;
    Ok(Json(json!({ "status": "ok" })))
}

async fn settings(State(state): State<AppState>) -> Result<Json<Value>, ApiError> {
    Ok(Json(
        serde_json::to_value(state.storage.settings().await?)
            .map_err(|_| ApiError::Storage(StorageError::Unavailable))?,
    ))
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct SettingsRequest {
    settings: Value,
}

async fn update_settings(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(request): Json<SettingsRequest>,
) -> Result<Json<Value>, ApiError> {
    let expected = if_match(&headers)?;
    let revision = state
        .storage
        .save_settings(request.settings.clone(), &expected)
        .await?;
    Ok(Json(
        json!({ "settings": request.settings, "revision": revision }),
    ))
}

fn if_match(headers: &HeaderMap) -> Result<String, ApiError> {
    let mut values = headers.get_all(header::IF_MATCH).iter();
    let value = values.next().ok_or(ApiError::PreconditionRequired)?;
    if values.next().is_some() {
        return Err(ApiError::InvalidRevision);
    }
    let value = value.to_str().map_err(|_| ApiError::InvalidRevision)?;
    let token = value
        .strip_prefix('"')
        .and_then(|value| value.strip_suffix('"'))
        .ok_or(ApiError::InvalidRevision)?;
    if token.is_empty()
        || token.len() > 128
        || !token
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || b"-_".contains(&byte))
    {
        return Err(ApiError::InvalidRevision);
    }
    Ok(token.to_owned())
}

async fn integrations() -> Json<Value> {
    Json(json!({ "integrations": registered() }))
}

async fn data_views(State(state): State<AppState>) -> Result<Json<Value>, ApiError> {
    Ok(Json(json!({ "data_views": state.storage.views().await? })))
}

async fn create_data_view(
    State(state): State<AppState>,
    Json(request): Json<CreateDataViewRequest>,
) -> Result<Json<Value>, ApiError> {
    let id = connection_id(&request.name).map_err(|_| ApiError::InvalidDataView)?;
    let (data_view, connection_revision) = validate_data_view(&state, id.clone(), request).await?;
    let revision = state
        .storage
        .save_view(data_view, &connection_revision, None)
        .await?;
    Ok(Json(
        json!({ "data_view": id, "status": "configured", "revision": revision }),
    ))
}

async fn update_data_view(
    State(state): State<AppState>,
    Path(id): Path<String>,
    headers: HeaderMap,
    Json(request): Json<CreateDataViewRequest>,
) -> Result<Json<Value>, ApiError> {
    let expected = if_match(&headers)?;
    connection_id(&request.name).map_err(|_| ApiError::InvalidDataView)?;
    let (data_view, connection_revision) = validate_data_view(&state, id.clone(), request).await?;
    let revision = state
        .storage
        .save_view(data_view, &connection_revision, Some(&expected))
        .await?;
    Ok(Json(
        json!({ "data_view": id, "status": "configured", "revision": revision }),
    ))
}

async fn validate_data_view(
    state: &AppState,
    id: String,
    request: CreateDataViewRequest,
) -> Result<(DataView, String), ApiError> {
    if request.index.trim().is_empty() || request.time_field.trim().is_empty() {
        return Err(ApiError::InvalidDataView);
    }

    let _permit = acquire(state).await?;
    let connection = get_connection(state, &request.connection_id).await?;
    let time_field = validate_source(
        connection.connector.as_ref(),
        &request.index,
        &request.time_field,
    )
    .await?;
    Ok((
        DataView {
            id,
            name: request.name.trim().to_owned(),
            connection_id: request.connection_id,
            index: request.index,
            time_field,
            revision: String::new(),
        },
        connection.record.revision,
    ))
}

async fn validate_source(
    connector: &dyn Connector,
    index_name: &str,
    time_field_name: &str,
) -> Result<String, ApiError> {
    let fields = connector.fields(index_name).await?;
    let time_field = fields
        .iter()
        .find(|field| field.name == time_field_name)
        .filter(|field| field.is_time_field)
        .ok_or(ApiError::InvalidDataView)?;

    Ok(time_field.name.clone())
}

async fn remove_data_view(
    State(state): State<AppState>,
    Path(data_view_id): Path<String>,
    headers: HeaderMap,
) -> Result<StatusCode, ApiError> {
    let expected = if_match(&headers)?;
    state.storage.remove_view(&data_view_id, &expected).await?;
    Ok(StatusCode::NO_CONTENT)
}

async fn acquire(state: &AppState) -> Result<OwnedSemaphorePermit, ApiError> {
    state
        .upstream_slots
        .clone()
        .try_acquire_owned()
        .map_err(|_| ApiError::Unavailable)
}

#[derive(Debug)]
enum ApiError {
    PreconditionRequired,
    InvalidRevision,
    Storage(StorageError),
    Unavailable,
    InvalidTimeRange,
    InvalidSearchWindow(&'static str),
    QueryTranslation(String),
    InvalidConnection,
    InvalidDataView,
    Connector(ConnectorError),
    InvalidAiModel,
    DuplicateAiModel,
    AiModelTest(&'static str),
    Investigation(ai::InvestigationError),
    InvestigationNotFound,
    InvalidConversationMessage,
    InvalidKnowledge,
    InvestigationDecision(&'static str),
}

impl From<StorageError> for ApiError {
    fn from(error: StorageError) -> Self {
        Self::Storage(error)
    }
}

impl From<ConnectorError> for ApiError {
    fn from(error: ConnectorError) -> Self {
        Self::Connector(error)
    }
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        let (status, code, message) = match self {
            Self::PreconditionRequired => (StatusCode::PRECONDITION_REQUIRED, "precondition_required", "A quoted If-Match revision is required".to_owned()),
            Self::InvalidRevision => (StatusCode::BAD_REQUEST, "invalid_revision", "If-Match must contain one quoted revision".to_owned()),
            Self::Storage(StorageError::Conflict) => (StatusCode::CONFLICT, "configuration_conflict", "Configuration changed; reload before editing".to_owned()),
            Self::Storage(StorageError::Duplicate) => (StatusCode::CONFLICT, "duplicate_connection", "A configuration with this name already exists".to_owned()),
            Self::Storage(StorageError::ConnectionNotFound) => (StatusCode::NOT_FOUND, "connection_not_found", "The requested connection does not exist".to_owned()),
            Self::Storage(StorageError::ViewNotFound) => (StatusCode::NOT_FOUND, "data_view_not_found", "The requested data view does not exist".to_owned()),
            Self::Storage(StorageError::AiModelNotFound) => (StatusCode::NOT_FOUND, "ai_model_not_found", "The requested AI model does not exist".to_owned()),
            Self::Storage(StorageError::AiConversationNotFound) => (StatusCode::NOT_FOUND, "ai_conversation_not_found", "The requested AI conversation does not exist".to_owned()),
            Self::Storage(StorageError::KnowledgeNotFound) => (StatusCode::NOT_FOUND, "knowledge_not_found", "The requested knowledge entry does not exist".to_owned()),
            Self::Storage(StorageError::Invalid) => (StatusCode::BAD_REQUEST, "invalid_configuration", "Configuration fields are invalid".to_owned()),
            Self::Storage(_) => (StatusCode::SERVICE_UNAVAILABLE, "configuration_storage_unavailable", "Configuration storage is unavailable".to_owned()),
            Self::InvalidTimeRange => (
                StatusCode::BAD_REQUEST,
                "invalid_time_range",
                "A time window requires valid start/end dates, a time field, and start no later than end".to_owned(),
            ),
            Self::InvalidSearchWindow(message) => (
                StatusCode::BAD_REQUEST,
                "invalid_search_window",
                message.to_owned(),
            ),
            Self::QueryTranslation(message) => (
                StatusCode::BAD_REQUEST,
                "query_translation_failed",
                message,
            ),
            Self::Unavailable => (
                StatusCode::SERVICE_UNAVAILABLE,
                "upstream_capacity_unavailable",
                "Search service is temporarily at capacity".to_owned(),
            ),
            Self::InvalidConnection => (
                StatusCode::BAD_REQUEST,
                "invalid_connection",
                "Connection details are invalid or incomplete".to_owned(),
            ),
            Self::InvalidDataView => (
                StatusCode::BAD_REQUEST,
                "invalid_data_view",
                "Data view must reference an available connection, index/table, and date/time field".to_owned(),
            ),
            Self::Connector(error) => (error.status, error.code, error.message),
            Self::InvalidAiModel => (StatusCode::BAD_REQUEST, "invalid_ai_model", "AI model fields are invalid".to_owned()),
            Self::DuplicateAiModel => (StatusCode::CONFLICT, "duplicate", "An AI model with this name already exists".to_owned()),
            Self::AiModelTest(message) => (StatusCode::BAD_GATEWAY, "ai_model_test_failed", message.to_owned()),
            Self::Investigation(error) => (error.status, error.code, error.message),
            Self::InvestigationNotFound => (StatusCode::NOT_FOUND, "investigation_not_found", "The investigation does not exist or has expired".to_owned()),
            Self::InvalidConversationMessage => (StatusCode::BAD_REQUEST, "invalid_ai_message", "A follow-up message must contain 1 to 8000 bytes and the conversation must stay within its message limit".to_owned()),
            Self::InvalidKnowledge => (StatusCode::BAD_REQUEST, "invalid_knowledge", "Knowledge fields are invalid or the 128-entry limit has been reached. Content must contain 1 to 32768 bytes; source links must use HTTP or HTTPS without credentials.".to_owned()),
            Self::InvestigationDecision(message) => (StatusCode::CONFLICT, "investigation_not_paused", message.to_owned()),
        };

        if status.is_server_error() {
            tracing::error!(
                status = status.as_u16(),
                error_code = code,
                "API request rejected"
            );
        } else {
            tracing::warn!(
                status = status.as_u16(),
                error_code = code,
                "API request rejected"
            );
        }
        (
            status,
            Json(json!({ "error": { "code": code, "message": message } })),
        )
            .into_response()
    }
}

async fn connections(State(state): State<AppState>) -> Result<Json<Value>, ApiError> {
    let summaries: Vec<ConnectionSummary> = state
        .storage
        .connections()
        .await?
        .into_iter()
        .map(|connection| {
            let integration = registered()
                .into_iter()
                .find(|entry| entry.id == connection.integration_id)
                .ok_or(StorageError::Configuration)?;
            let connector = connect(integration.id, &connection.config)
                .map_err(|_| StorageError::Configuration)?;
            let mut configuration = connection.config.clone();
            configuration.extend(connector.public_configuration());
            Ok(ConnectionSummary {
                id: connection.id.clone(),
                revision: connection.revision.clone(),
                name: connection.name.clone(),
                connection_type: integration.id,
                url: safe_endpoint(
                    connection
                        .config
                        .get("url")
                        .and_then(Value::as_str)
                        .unwrap_or_default(),
                ),
                database: connection
                    .config
                    .get("database")
                    .and_then(Value::as_str)
                    .map(str::to_owned),
                status: "connected",
                configuration: public_configuration(&integration, &configuration),
                integration,
            })
        })
        .collect::<Result<_, StorageError>>()?;
    Ok(Json(json!({ "connections": summaries })))
}

async fn add_connection(
    State(state): State<AppState>,
    Json(request): Json<AddConnectionRequest>,
) -> Result<Json<Value>, ApiError> {
    let id = connection_id(&request.name)?;

    let integration = registered()
        .into_iter()
        .find(|descriptor| descriptor.id == request.connection_type)
        .ok_or(ApiError::InvalidConnection)?;
    let mut config = request.settings;
    config.insert("url".to_owned(), Value::String(request.url));
    let connector = connect(integration.id, &config)?;
    let _permit = acquire(&state).await?;
    connector.test_connection().await?;
    config.extend(connector.learned_configuration());
    let connection = StoredConnection {
        id: id.clone(),
        name: request.name,
        integration_id: integration.id.to_owned(),
        config,
        revision: String::new(),
    };
    let revision = state.storage.create_connection(connection).await?;
    Ok(Json(
        json!({ "connection": id, "status": "connected", "revision": revision }),
    ))
}

async fn update_connection(
    State(state): State<AppState>,
    Path(existing_id): Path<String>,
    headers: HeaderMap,
    Json(request): Json<UpdateConnectionRequest>,
) -> Result<Json<Value>, ApiError> {
    let expected = if_match(&headers)?;
    let new_id = connection_id(&request.name)?;
    let existing = get_connection(&state, &existing_id).await?;
    if existing.record.revision != expected {
        return Err(StorageError::Conflict.into());
    }
    let mut settings = request.settings;
    settings.insert("url".to_owned(), Value::String(request.url));
    let mut config =
        editable_configuration(&existing.integration, &existing.record.config, &settings)?;
    let connector = connect(existing.integration.id, &config)?;
    let _permit = acquire(&state).await?;
    connector.test_connection().await?;
    config.extend(connector.learned_configuration());
    let updated = StoredConnection {
        id: new_id.clone(),
        name: request.name,
        integration_id: existing.integration.id.to_owned(),
        config,
        revision: String::new(),
    };
    let revision = state
        .storage
        .update_connection(&existing_id, &expected, updated)
        .await?;
    Ok(Json(
        json!({ "connection": new_id, "status": "connected", "revision": revision }),
    ))
}

async fn remove_connection(
    State(state): State<AppState>,
    Path(connection_id): Path<String>,
    headers: HeaderMap,
) -> Result<StatusCode, ApiError> {
    let expected = if_match(&headers)?;
    state
        .storage
        .remove_connection(&connection_id, &expected)
        .await?;
    Ok(StatusCode::NO_CONTENT)
}

async fn connection_status(
    State(state): State<AppState>,
    Path(connection_id): Path<String>,
) -> Result<Json<Value>, ApiError> {
    let _permit = acquire(&state).await?;
    let connection = state.storage.connection(&connection_id).await?;
    let connected = matches!(
        tokio::time::timeout(
            std::time::Duration::from_secs(5),
            connection.connector.test_connection(),
        )
        .await,
        Ok(Ok(()))
    );
    Ok(Json(
        json!({ "status": if connected { "connected" } else { "not_connected" } }),
    ))
}

async fn connection_indices(
    State(state): State<AppState>,
    Path(connection_id): Path<String>,
) -> Result<Json<Value>, ApiError> {
    let _permit = acquire(&state).await?;
    let connection = get_connection(&state, &connection_id).await?;
    let indices = connection.connector.indices().await?;
    Ok(Json(json!({ "indices": indices })))
}

async fn connection_fields(
    State(state): State<AppState>,
    Path((connection_id, index)): Path<(String, String)>,
) -> Result<Json<Value>, ApiError> {
    let _permit = acquire(&state).await?;
    let connection = get_connection(&state, &connection_id).await?;
    let fields = connection.connector.fields(&index).await?;
    Ok(Json(json!({ "fields": fields })))
}

async fn connection_document(
    State(state): State<AppState>,
    Path((connection_id, index, id)): Path<(String, String, String)>,
) -> Result<Json<Value>, ApiError> {
    let _permit = acquire(&state).await?;
    let connection = get_connection(&state, &connection_id).await?;
    Ok(Json(connection.connector.document(&index, &id).await?))
}

#[derive(Deserialize)]
struct ExploreSearch {
    #[serde(flatten)]
    request: SearchRequest,
    /// The language `query` is written in when it is not the Data View's own.
    #[serde(default)]
    query_language: Option<String>,
}

async fn connection_search(
    State(state): State<AppState>,
    Path(connection_id): Path<String>,
    Json(search): Json<ExploreSearch>,
) -> Result<Json<Value>, ApiError> {
    let ExploreSearch {
        mut request,
        query_language,
    } = search;
    validate_search(&request)?;
    let window = SearchWindow::prepare(&mut request).map_err(ApiError::InvalidSearchWindow)?;
    let _permit = acquire(&state).await?;
    let connection = get_connection(&state, &connection_id).await?;
    let translated = match query_language.as_deref().map(str::trim) {
        Some(language)
            if !language.is_empty()
                && !language.eq_ignore_ascii_case(connection.integration.query_language)
                && !request.query.trim().is_empty() =>
        {
            Some(translate_search_query(&connection, &mut request, language).await?)
        }
        _ => None,
    };
    let mut result = connection.connector.search(request).await?;
    window.finish(&mut result);
    if let Some((language, query)) = translated {
        result["query_language"] = json!(language);
        result["translated_query"] = json!(query);
    }
    Ok(Json(result))
}

/// Rewrites `request.query` from `language` into the Data View's language, typing each field
/// from the Data View's own schema; returns the canonical language name and the sent query.
async fn translate_search_query(
    connection: &ResolvedConnection,
    request: &mut SearchRequest,
    language: &str,
) -> Result<(&'static str, String), ApiError> {
    let source = dialect_for_language(language).ok_or_else(|| {
        ApiError::QueryTranslation(format!(
            "Queries can be translated from {}",
            translatable_languages()
        ))
    })?;
    let target = investigation_dialect(connection.integration.id).ok_or_else(|| {
        ApiError::QueryTranslation(format!(
            "{} Data Views accept {} queries only",
            connection.integration.display_name, connection.integration.query_language
        ))
    })?;
    let field_types = connection
        .connector
        .fields(&request.index)
        .await?
        .into_iter()
        .map(|field| (field.name, field.field_type))
        .collect();
    let query = translate_query(
        source.language(),
        &request.query,
        target,
        &request.index,
        &field_types,
    )
    .map_err(|error| ApiError::QueryTranslation(error.to_string()))?;
    request.query.clone_from(&query);
    Ok((source.language(), query))
}

fn translatable_languages() -> String {
    let mut languages = crate::data_connectors::investigation_dialects()
        .into_iter()
        .map(|dialect| dialect.language())
        .collect::<Vec<_>>();
    languages.sort_unstable();
    languages.dedup();
    languages.join(", ")
}

fn validate_search(request: &SearchRequest) -> Result<(), ApiError> {
    if request.time_range.as_ref().is_some_and(|range| {
        !range.is_valid()
            || request
                .histogram_field
                .as_deref()
                .is_none_or(|field| field.trim().is_empty())
    }) {
        return Err(ApiError::InvalidTimeRange);
    }
    Ok(())
}

async fn get_connection(state: &AppState, id: &str) -> Result<ResolvedConnection, ApiError> {
    Ok(state.storage.connection(id).await?)
}

fn connection_id(name: &str) -> Result<String, ApiError> {
    let id = name.trim().to_ascii_lowercase().replace(' ', "-");
    if id.is_empty()
        || id.len() > 64
        || !id.bytes().all(|byte| {
            byte.is_ascii_lowercase() || byte.is_ascii_digit() || "-_".contains(byte as char)
        })
    {
        return Err(ApiError::InvalidConnection);
    }
    Ok(id)
}

fn safe_endpoint(raw_url: &str) -> String {
    url::Url::parse(raw_url)
        .map(|url| {
            let mut endpoint = url.origin().ascii_serialization();
            let path = url.path().trim_end_matches('/');
            if !path.is_empty() {
                endpoint.push_str(path);
            }
            endpoint
        })
        .unwrap_or_default()
}

#[cfg(test)]
mod tests;
