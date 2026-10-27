use std::{future::Future, pin::Pin, time::Duration};

use axum::http::StatusCode;
use serde_json::Value;
use tracing::Instrument;

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

struct LoggedAiConnector;

fn log_request(
    settings: &AiSettings,
    messages: &[Value],
    json_mode: bool,
    max_tokens: Option<u64>,
) {
    tracing::debug!(
        messages = messages.len(),
        json_mode,
        max_tokens,
        timeout_seconds = settings.timeout.as_secs(),
        "AI completion requested"
    );
}

fn log_completion(
    operation: &mut crate::logging::Operation,
    result: &Result<ProviderReply, AiConnectorError>,
) {
    match result {
        Ok(reply) => {
            tracing::debug!(
                prompt_tokens = reply.usage.prompt_tokens,
                completion_tokens = reply.usage.completion_tokens,
                total_tokens = reply.usage.total_tokens,
                truncated = reply.truncated,
                "AI completion usage"
            );
            operation.complete(200, None, None);
        }
        Err(error) => operation.complete(error.status.as_u16(), Some(error.code), None),
    }
}

impl AiConnector for LoggedAiConnector {
    fn complete<'a>(
        &'a self,
        settings: &'a AiSettings,
        messages: &'a [Value],
        json_mode: bool,
        max_tokens: Option<u64>,
    ) -> AiConnectorFuture<'a> {
        Box::pin(
            async move {
                let mut operation = crate::logging::Operation::new("ai", "complete");
                log_request(settings, messages, json_mode, max_tokens);
                let result = openai::OpenAiConnector
                    .complete(settings, messages, json_mode, max_tokens)
                    .await;
                log_completion(&mut operation, &result);
                result
            }
            .instrument(tracing::error_span!("ai_provider", provider = "openai")),
        )
    }
}

pub fn default_connector() -> &'static dyn AiConnector {
    static OPENAI: LoggedAiConnector = LoggedAiConnector;
    &OPENAI
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::process::Command;
    use wiremock::{matchers, Mock, MockServer, ResponseTemplate};

    #[test]
    fn debug_ai_logs_metadata_without_credentials_prompts_or_replies() {
        let captured = crate::logging::tests::capture();
        tracing::dispatcher::with_default(&captured.subscriber, || {
            let settings = AiSettings {
                api_url: "https://private-endpoint.example".to_owned(),
                api_key: "private-api-key".to_owned(),
                model: "private-model".to_owned(),
                timeout: Duration::from_secs(2),
            };
            let messages = [serde_json::json!({"role":"user","content":"private-prompt"})];
            log_request(&settings, &messages, false, None);
            let reply = ProviderReply {
                content: "private-reply".to_owned(),
                usage: ModelUsage {
                    prompt_tokens: Some(12),
                    completion_tokens: Some(4),
                    total_tokens: Some(16),
                },
                truncated: false,
            };
            log_completion(
                &mut crate::logging::Operation::new("ai", "complete"),
                &Ok(reply),
            );
            log_completion(
                &mut crate::logging::Operation::new("ai", "complete"),
                &Err(AiConnectorError::upstream(
                    "ai_unavailable",
                    "private-upstream-error",
                )),
            );
        });
        let text = String::from_utf8(captured.bytes.lock().unwrap().clone()).unwrap();
        assert!(text.contains("AI completion usage"));
        assert!(text.contains("total_tokens=16"));
        for secret in [
            "private-api-key",
            "private-model",
            "private-prompt",
            "private-reply",
            "private-endpoint.example",
            "private-upstream-error",
        ] {
            assert!(
                !text.contains(secret),
                "Unexpected private content in AI logs"
            );
        }
    }

    #[tokio::test]
    async fn async_completion_writes_safe_ai_logs_to_file() {
        const CHILD_FLAG: &str = "INVESTIGENIE_AI_LOG_SMOKE_CHILD";
        if std::env::var_os(CHILD_FLAG).is_none() {
            let output = Command::new(std::env::current_exe().unwrap())
                .args([
                    "--exact",
                    "ai_connectors::tests::async_completion_writes_safe_ai_logs_to_file",
                    "--nocapture",
                ])
                .env(CHILD_FLAG, "1")
                .output()
                .unwrap();
            assert!(
                output.status.success(),
                "Isolated AI logging test failed:\n{}{}",
                String::from_utf8_lossy(&output.stdout),
                String::from_utf8_lossy(&output.stderr)
            );
            return;
        }

        let directory = tempfile::tempdir().unwrap();
        let server = MockServer::start().await;
        Mock::given(matchers::method("POST"))
            .and(matchers::path("/chat/completions"))
            .and(matchers::header(
                "authorization",
                "Bearer synthetic-api-key",
            ))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "choices": [{
                    "message": {"content": "synthetic-reply"},
                    "finish_reason": "stop"
                }],
                "usage": {
                    "prompt_tokens": 12,
                    "completion_tokens": 4,
                    "total_tokens": 16
                }
            })))
            .expect(1)
            .mount(&server)
            .await;

        let guard = crate::logging::initialize(&crate::logging::LoggingSettings {
            level: tracing_subscriber::filter::LevelFilter::DEBUG,
            mode: crate::logging::LogMode::File,
            path: directory.path().to_path_buf(),
            max_files: 1,
        })
        .unwrap()
        .unwrap();
        let settings = AiSettings {
            api_url: server.uri(),
            api_key: "synthetic-api-key".to_owned(),
            model: "synthetic-model".to_owned(),
            timeout: Duration::from_secs(2),
        };
        let messages = [serde_json::json!({
            "role": "user",
            "content": "synthetic-prompt"
        })];
        let reply = default_connector()
            .complete(&settings, &messages, false, None)
            .await
            .unwrap();
        assert_eq!(reply.content, "synthetic-reply");
        drop(guard);

        let log_path = std::fs::read_dir(directory.path())
            .unwrap()
            .map(|entry| entry.unwrap().path())
            .find(|path| path.extension().is_some_and(|extension| extension == "log"))
            .expect("file logging should create a daily log");
        let log = std::fs::read_to_string(log_path).unwrap();
        assert!(log.contains("AI completion requested"));
        assert!(log.contains("AI completion usage"));
        assert!(log.contains("total_tokens=16"));
        assert!(log.contains("Operation completed"));
        for secret in [
            "synthetic-api-key",
            "synthetic-model",
            "synthetic-prompt",
            "synthetic-reply",
            server.uri().as_str(),
        ] {
            assert!(
                !log.contains(secret),
                "Unexpected private content in AI logs"
            );
        }
    }
}
