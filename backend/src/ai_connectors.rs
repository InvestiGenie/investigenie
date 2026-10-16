use std::{future::Future, pin::Pin, time::Duration};

use axum::http::StatusCode;
use serde_json::Value;

#[path = "../../integrations/ai_connectors/openai/mod.rs"]
pub mod openai;

#[derive(Clone)]
pub struct AiSettings {
    pub api_url: String,
    pub api_key: String,
    pub model: String,
    pub timeout: Duration,
}

#[derive(Clone, Debug, Default)]
pub struct ModelUsage {
    pub prompt_tokens: Option<u64>,
    pub completion_tokens: Option<u64>,
    pub total_tokens: Option<u64>,
}

pub struct ProviderReply {
    pub content: String,
    pub usage: ModelUsage,
    pub truncated: bool,
}

#[derive(Debug)]
pub struct AiConnectorError {
    pub status: StatusCode,
    pub code: &'static str,
    pub message: String,
}

impl AiConnectorError {
    pub fn upstream(code: &'static str, message: &'static str) -> Self {
        Self {
            status: StatusCode::BAD_GATEWAY,
            code,
            message: message.to_owned(),
        }
    }
}

pub type AiConnectorFuture<'a> =
    Pin<Box<dyn Future<Output = Result<ProviderReply, AiConnectorError>> + Send + 'a>>;

pub trait AiConnector: Send + Sync {
    fn complete<'a>(
        &'a self,
        settings: &'a AiSettings,
        messages: &'a [Value],
        json_mode: bool,
        max_tokens: Option<u64>,
    ) -> AiConnectorFuture<'a>;
}

pub fn default_connector() -> &'static dyn AiConnector {
    static OPENAI: openai::OpenAiConnector = openai::OpenAiConnector;
    &OPENAI
}
