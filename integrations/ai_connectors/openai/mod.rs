use axum::http::StatusCode;
use serde_json::{json, Value};

use super::{
    AiConnector, AiConnectorError, AiConnectorFuture, AiSettings, ModelUsage, ProviderReply,
};

const MAX_PROMPT_BYTES: usize = 65_536;
const MAX_COMPLETION_BYTES: usize = 32_768;

pub struct OpenAiConnector;

impl AiConnector for OpenAiConnector {
    fn complete<'a>(
        &'a self,
        settings: &'a AiSettings,
        messages: &'a [Value],
        json_mode: bool,
        max_tokens: Option<u64>,
    ) -> AiConnectorFuture<'a> {
        Box::pin(request_completion(
            settings, messages, json_mode, max_tokens,
        ))
    }
}

async fn request_completion(
    settings: &AiSettings,
    messages: &[Value],
    json_mode: bool,
    max_tokens: Option<u64>,
) -> Result<ProviderReply, AiConnectorError> {
    let client = reqwest::Client::builder()
        .timeout(settings.timeout)
        .build()
        .map_err(|_| {
            AiConnectorError::upstream(
                "ai_unavailable",
                "Could not initialize the AI provider request",
            )
        })?;
    let mut body = json!({"model": settings.model, "messages": messages});
    if json_mode {
        body["response_format"] = json!({"type":"json_object"});
    }
    if let Some(max_tokens) = max_tokens {
        body["max_tokens"] = json!(max_tokens);
    }
    let serialized = serde_json::to_vec(&body).map_err(|_| {
        AiConnectorError::upstream(
            "ai_request_too_large",
            "Investigation input exceeds the supported size limit",
        )
    })?;
    if serialized.len() > MAX_PROMPT_BYTES {
        return Err(AiConnectorError::upstream(
            "ai_request_too_large",
            "Investigation input exceeds the supported size limit",
        ));
    }
    let endpoint = format!("{}/chat/completions", settings.api_url);
    let mut response = send_completion(&client, &endpoint, &settings.api_key, &body).await?;
    if !response.status().is_success() {
        let status = response.status();
        tracing::debug!(
            upstream_status = status.as_u16(),
            "AI provider rejected completion request"
        );
        let error_preview = response_error_preview(response).await;
        if !json_mode || !response_format_unsupported(status, &error_preview) {
            return Err(AiConnectorError::upstream(
                "ai_provider_error",
                "The AI provider returned an unusable response",
            ));
        }
        body.as_object_mut()
            .expect("completion request is an object")
            .remove("response_format");
        tracing::debug!("Retrying AI completion without JSON response format");
        response = send_completion(&client, &endpoint, &settings.api_key, &body).await?;
        if !response.status().is_success() {
            return Err(AiConnectorError::upstream(
                "ai_provider_error",
                "The AI provider returned an unusable response",
            ));
        }
    }
    if response
        .content_length()
        .is_some_and(|length| length > MAX_COMPLETION_BYTES as u64)
    {
        return Err(AiConnectorError::upstream(
            "ai_provider_error",
            "The AI provider returned an unusable response",
        ));
    }
    let mut response = response;
    let mut bytes = Vec::new();
    while let Some(chunk) = response.chunk().await.map_err(|error| {
        if error.is_timeout() {
            AiConnectorError::upstream(
                "ai_timeout",
                "The AI provider did not respond before the configured timeout",
            )
        } else {
            AiConnectorError::upstream(
                "ai_provider_error",
                "The AI provider returned an unusable response",
            )
        }
    })? {
        if bytes.len() + chunk.len() > MAX_COMPLETION_BYTES {
            return Err(AiConnectorError::upstream(
                "ai_provider_error",
                "The AI provider response exceeds the supported size limit",
            ));
        }
        bytes.extend_from_slice(&chunk);
    }
    let envelope: Value = serde_json::from_slice(&bytes).map_err(|_| {
        AiConnectorError::upstream(
            "ai_provider_error",
            "The AI provider returned an unusable response",
        )
    })?;
    let message = envelope.pointer("/choices/0/message");
    let content = match message.and_then(|message| message.get("content")) {
        Some(Value::String(text)) => text.clone(),
        Some(Value::Array(parts)) => parts
            .iter()
            .filter_map(|part| part.get("text").and_then(Value::as_str))
            .collect(),
        _ => String::new(),
    };
    let truncated = envelope
        .pointer("/choices/0/finish_reason")
        .and_then(Value::as_str)
        .is_some_and(|reason| reason == "length" || reason == "max_tokens");
    if content.trim().is_empty() && !truncated {
        let refused = message
            .and_then(|message| message.get("refusal"))
            .is_some_and(|refusal| !refusal.is_null());
        return Err(if refused {
            AiConnectorError::upstream(
                "ai_refused",
                "The AI provider declined to answer this request",
            )
        } else {
            AiConnectorError::upstream(
                "ai_provider_error",
                "The AI provider returned an empty response",
            )
        });
    }
    let usage = envelope
        .get("usage")
        .map(|usage| {
            let prompt_tokens = usage
                .get("prompt_tokens")
                .and_then(Value::as_u64)
                .or_else(|| usage.get("input_tokens").and_then(Value::as_u64));
            let completion_tokens = usage
                .get("completion_tokens")
                .and_then(Value::as_u64)
                .or_else(|| usage.get("output_tokens").and_then(Value::as_u64));
            let total_tokens = usage
                .get("total_tokens")
                .and_then(Value::as_u64)
                .or_else(|| {
                    prompt_tokens
                        .zip(completion_tokens)
                        .map(|(input, output)| input.saturating_add(output))
                });
            ModelUsage {
                prompt_tokens,
                completion_tokens,
                total_tokens,
            }
        })
        .unwrap_or_default();
    Ok(ProviderReply {
        content,
        usage,
        truncated,
    })
}

async fn send_completion(
    client: &reqwest::Client,
    endpoint: &str,
    api_key: &str,
    body: &Value,
) -> Result<reqwest::Response, AiConnectorError> {
    client
        .post(endpoint)
        .bearer_auth(api_key)
        .json(body)
        .send()
        .await
        .map_err(|error| {
            if error.is_timeout() {
                AiConnectorError::upstream(
                    "ai_timeout",
                    "The AI provider did not respond before the configured timeout",
                )
            } else {
                AiConnectorError::upstream(
                    "ai_unavailable",
                    "The AI provider request failed or timed out",
                )
            }
        })
}

async fn response_error_preview(mut response: reqwest::Response) -> String {
    const MAX_ERROR_PREVIEW_BYTES: usize = 4_096;
    let mut bytes = Vec::with_capacity(MAX_ERROR_PREVIEW_BYTES);
    while bytes.len() < MAX_ERROR_PREVIEW_BYTES {
        match response.chunk().await {
            Ok(Some(chunk)) => {
                let remaining = MAX_ERROR_PREVIEW_BYTES - bytes.len();
                bytes.extend_from_slice(&chunk[..chunk.len().min(remaining)]);
            }
            _ => break,
        }
    }
    String::from_utf8_lossy(&bytes).into_owned()
}

fn response_format_unsupported(status: StatusCode, error_preview: &str) -> bool {
    if !matches!(
        status,
        StatusCode::BAD_REQUEST | StatusCode::UNPROCESSABLE_ENTITY
    ) {
        return false;
    }
    let message = error_preview.to_ascii_lowercase();
    let mentions_format = ["response_format", "json_object", "structured output"]
        .iter()
        .any(|term| message.contains(term));
    let rejects_format = [
        "not supported",
        "unsupported",
        "unknown parameter",
        "unrecognized",
        "not implemented",
        "invalid parameter",
    ]
    .iter()
    .any(|term| message.contains(term));
    mentions_format && rejects_format
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;
    use wiremock::{matchers, Mock, MockServer, ResponseTemplate};

    fn settings(server: &MockServer) -> AiSettings {
        AiSettings {
            api_url: server.uri(),
            api_key: "private-test-key".to_owned(),
            model: "test-model".to_owned(),
            timeout: Duration::from_secs(5),
        }
    }

    #[tokio::test]
    async fn oversized_prompts_are_rejected_before_network_io() {
        let server = MockServer::start().await;
        let messages = [json!({"role":"user","content":"x".repeat(MAX_PROMPT_BYTES)})];
        let error = OpenAiConnector
            .complete(&settings(&server), &messages, false, None)
            .await
            .err()
            .unwrap();
        assert_eq!(error.code, "ai_request_too_large");
        assert!(server.received_requests().await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn provider_errors_are_sanitized_without_retrying_auth_failures() {
        let server = MockServer::start().await;
        Mock::given(matchers::method("POST"))
            .and(matchers::path("/chat/completions"))
            .and(matchers::header("authorization", "Bearer private-test-key"))
            .respond_with(
                ResponseTemplate::new(401)
                    .set_body_string("private-test-key response_format unsupported"),
            )
            .expect(1)
            .mount(&server)
            .await;
        let error = OpenAiConnector
            .complete(&settings(&server), &[], true, None)
            .await
            .err()
            .unwrap();
        assert_eq!(error.code, "ai_provider_error");
        assert!(!error.message.contains("private-test-key"));
        assert!(!error.message.contains(&server.uri()));
    }

    #[tokio::test]
    async fn oversized_responses_are_rejected() {
        let server = MockServer::start().await;
        Mock::given(matchers::method("POST"))
            .respond_with(
                ResponseTemplate::new(200).set_body_string("x".repeat(MAX_COMPLETION_BYTES + 1)),
            )
            .expect(1)
            .mount(&server)
            .await;
        let error = OpenAiConnector
            .complete(&settings(&server), &[], false, None)
            .await
            .err()
            .unwrap();
        assert_eq!(error.code, "ai_provider_error");
    }
}
