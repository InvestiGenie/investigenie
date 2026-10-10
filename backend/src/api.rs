use std::sync::Arc;

use axum::{
    extract::{DefaultBodyLimit, Path, State},
    http::{header, HeaderMap, HeaderValue, Method, StatusCode},
    response::{IntoResponse, Response},
    routing::{get, post, put},
    Json, Router,
};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use tokio::sync::{OwnedSemaphorePermit, Semaphore};
use tower_http::cors::CorsLayer;

use crate::ai::{self, InvestigationRequest};
use crate::integrations::{
    connect, editable_configuration, public_configuration, registered, validate_registry,
    ConnectionConfig, Connector, ConnectorError, IntegrationDescriptor,
};
use crate::search::SearchRequest;
use crate::storage::{
    AiModelSummary, DataView, ResolvedConnection, Storage, StorageError, StoredAiModel,
    StoredConnection,
};

const MAX_CONCURRENT_UPSTREAM_REQUESTS: usize = 64;

#[derive(Clone)]
struct AppState {
    storage: Storage,
    upstream_slots: Arc<Semaphore>,
    ai_slots: Arc<Semaphore>,
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

pub fn router(web_origin: HeaderValue, storage: Storage) -> Router {
    validate_registry();
    let state = AppState {
        storage,
        upstream_slots: Arc::new(Semaphore::new(MAX_CONCURRENT_UPSTREAM_REQUESTS)),
        ai_slots: Arc::new(Semaphore::new(4)),
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
        .route("/api/ai/models", get(ai_models).post(create_ai_model))
        .route(
            "/api/ai/models/{model_id}",
            put(update_ai_model).delete(remove_ai_model),
        )
        .with_state(state)
        .layer(cors)
}

async fn investigate(
    State(state): State<AppState>,
    Json(request): Json<InvestigationRequest>,
) -> Result<Json<ai::InvestigationResponse>, ApiError> {
    let model = state.storage.ai_model(&request.ai_model_id).await?;
    let settings = ai::AiSettings {
        api_url: model.api_url,
        api_key: model.api_key,
        model: model.model,
    };
    let permit = state
        .ai_slots
        .clone()
        .try_acquire_owned()
        .map_err(|_| ApiError::Unavailable)?;
    let result = ai::investigate(&state.storage, &settings, request, &state.upstream_slots).await;
    drop(permit);
    result.map(Json).map_err(ApiError::Investigation)
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
    ai::test_model(&ai::AiSettings {
        api_url: model.api_url.clone(),
        api_key: model.api_key.clone(),
        model: model.model.clone(),
    })
    .await
    .map_err(ApiError::AiModelTest)?;
    let saved = state
        .storage
        .create_ai_model(model)
        .await
        .map_err(|error| {
            if matches!(error, StorageError::Duplicate) {
                ApiError::DuplicateAiModel
            } else {
                ApiError::Storage(error)
            }
        })?;
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
    ai::test_model(&ai::AiSettings {
        api_url: model.api_url.clone(),
        api_key: model.api_key.clone(),
        model: model.model.clone(),
    })
    .await
    .map_err(ApiError::AiModelTest)?;
    let saved = state
        .storage
        .update_ai_model(&id, &expected, model)
        .await
        .map_err(|error| {
            if matches!(error, StorageError::Duplicate) {
                ApiError::DuplicateAiModel
            } else {
                ApiError::Storage(error)
            }
        })?;
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
    let indices = connector.indices().await?;
    if !indices.iter().any(|index| index.name == index_name) {
        return Err(ApiError::InvalidDataView);
    }
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
    InvalidConnection,
    InvalidDataView,
    Connector(ConnectorError),
    InvalidAiModel,
    DuplicateAiModel,
    AiModelTest(&'static str),
    Investigation(ai::InvestigationError),
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
            Self::Storage(StorageError::Invalid) => (StatusCode::BAD_REQUEST, "invalid_configuration", "Configuration fields are invalid".to_owned()),
            Self::Storage(_) => (StatusCode::SERVICE_UNAVAILABLE, "configuration_storage_unavailable", "Configuration storage is unavailable".to_owned()),
            Self::InvalidTimeRange => (
                StatusCode::BAD_REQUEST,
                "invalid_time_range",
                "A time window requires valid start/end dates, a time field, and start no later than end".to_owned(),
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
        };

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
    let config = editable_configuration(&existing.integration, &existing.record.config, &settings)?;
    let connector = connect(existing.integration.id, &config)?;
    let _permit = acquire(&state).await?;
    connector.test_connection().await?;
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

async fn connection_search(
    State(state): State<AppState>,
    Path(connection_id): Path<String>,
    Json(request): Json<SearchRequest>,
) -> Result<Json<Value>, ApiError> {
    validate_search(&request)?;
    let _permit = acquire(&state).await?;
    let connection = get_connection(&state, &connection_id).await?;
    Ok(Json(connection.connector.search(request).await?))
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
