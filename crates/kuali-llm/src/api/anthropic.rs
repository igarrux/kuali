//! Claude through the Anthropic API.
//!
//! Anthropic has no official Rust SDK, so this calls `/v1/messages` directly.
//! Streaming protects long transcript requests from connection timeouts.

use std::time::Duration;

use async_trait::async_trait;
use futures::StreamExt;

use crate::provider::{CompletionRequest, LlmError, LlmProvider, ProviderInfo, ProviderKind};

const API_URL: &str = "https://api.anthropic.com/v1/messages";
const API_VERSION: &str = "2023-06-01";
/// Enables `fallbacks` so safety-filter rejection can retry another model within
/// the same API call instead of leaving the user without a summary.
const FALLBACK_BETA: &str = "server-side-fallback-2026-07-01";

pub struct AnthropicApiProvider {
    api_key: String,
    model: String,
    client: reqwest::Client,
}

impl AnthropicApiProvider {
    pub const DEFAULT_MODEL: &'static str = "claude-opus-5";

    /// The key may be absent. The provider still exists but reports unavailable
    /// so Settings can request the missing credential.
    pub fn new(api_key: Option<String>, model: Option<String>) -> Self {
        Self {
            api_key: api_key.unwrap_or_default(),
            model: model.unwrap_or_else(|| Self::DEFAULT_MODEL.to_string()),
            client: reqwest::Client::builder()
                .timeout(Duration::from_secs(900))
                .build()
                .unwrap_or_default(),
        }
    }

    fn body(&self, request: &CompletionRequest) -> serde_json::Value {
        let mut body = serde_json::json!({
            "model": self.model,
            "max_tokens": request.max_tokens,
            "stream": true,
            // Let Claude choose its reasoning depth; extracting tasks from a
            // disorganized conversation benefits from that flexibility.
            "thinking": { "type": "adaptive" },
            "system": request.system,
            "messages": [{ "role": "user", "content": request.prompt }],
            "fallbacks": "default",
        });

        if let Some(schema) = &request.json_schema {
            body["output_config"] = serde_json::json!({
                "format": { "type": "json_schema", "schema": schema }
            });
        }
        body
    }
}

#[async_trait]
impl LlmProvider for AnthropicApiProvider {
    fn id(&self) -> &'static str {
        "anthropic-api"
    }

    fn info(&self) -> ProviderInfo {
        ProviderInfo {
            id: self.id().to_string(),
            label: "Anthropic API".to_string(),
            model: self.model.clone(),
            kind: ProviderKind::RemoteApi,
            structured_output: true,
        }
    }

    async fn is_available(&self) -> bool {
        !self.api_key.trim().is_empty()
    }

    async fn complete(&self, request: &CompletionRequest) -> Result<String, LlmError> {
        let response = self
            .client
            .post(API_URL)
            .header("x-api-key", &self.api_key)
            .header("anthropic-version", API_VERSION)
            .header("anthropic-beta", FALLBACK_BETA)
            .json(&self.body(request))
            .send()
            .await
            .map_err(|source| LlmError::Http {
                provider: "anthropic-api".into(),
                source,
            })?;

        if !response.status().is_success() {
            let status = response.status();
            let detail = response.text().await.unwrap_or_default();
            return Err(LlmError::Provider {
                provider: "anthropic-api".into(),
                message: format!("HTTP {status}: {}", detail.trim()),
            });
        }

        collect_sse_text(response).await
    }
}

/// Collects `text_delta` events from SSE. Thinking blocks arrive empty by
/// default and are ignored; `stop_reason` must still be checked because a
/// refusal can arrive with HTTP 200.
async fn collect_sse_text(response: reqwest::Response) -> Result<String, LlmError> {
    let mut text = String::new();
    let mut stop_reason: Option<String> = None;
    let mut refusal_detail: Option<String> = None;
    let mut buffer = String::new();
    let mut stream = response.bytes_stream();

    while let Some(chunk) = stream.next().await {
        let chunk = chunk.map_err(|source| LlmError::Http {
            provider: "anthropic-api".into(),
            source,
        })?;
        buffer.push_str(&String::from_utf8_lossy(&chunk));

        // Events are line-delimited. Process complete lines and retain the tail
        // for the next chunk.
        while let Some(newline) = buffer.find('\n') {
            let line: String = buffer.drain(..=newline).collect();
            let line = line.trim_end();
            let Some(payload) = line.strip_prefix("data:") else {
                continue;
            };
            let payload = payload.trim();
            if payload.is_empty() || payload == "[DONE]" {
                continue;
            }
            let Ok(event) = serde_json::from_str::<serde_json::Value>(payload) else {
                continue;
            };

            match event.get("type").and_then(|v| v.as_str()) {
                Some("content_block_delta") => {
                    let delta = &event["delta"];
                    if delta.get("type").and_then(|v| v.as_str()) == Some("text_delta") {
                        if let Some(t) = delta.get("text").and_then(|v| v.as_str()) {
                            text.push_str(t);
                        }
                    }
                }
                Some("message_delta") => {
                    if let Some(reason) = event["delta"].get("stop_reason").and_then(|v| v.as_str())
                    {
                        stop_reason = Some(reason.to_string());
                    }
                    if let Some(details) = event["delta"].get("stop_details") {
                        refusal_detail = details
                            .get("category")
                            .and_then(|v| v.as_str())
                            .map(str::to_string);
                    }
                }
                Some("error") => {
                    return Err(LlmError::Provider {
                        provider: "anthropic-api".into(),
                        message: event["error"]["message"]
                            .as_str()
                            .unwrap_or("error sin detalle")
                            .to_string(),
                    });
                }
                _ => {}
            }
        }
    }

    if stop_reason.as_deref() == Some("refusal") {
        return Err(LlmError::Refused {
            provider: "anthropic-api".into(),
            detail: refusal_detail,
        });
    }

    Ok(text)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn schema_is_only_attached_when_the_caller_asks_for_one() {
        let provider = AnthropicApiProvider::new(Some("k".into()), None);

        let plain = provider.body(&CompletionRequest::new("s", "p"));
        assert!(plain.get("output_config").is_none());

        let structured = provider.body(
            &CompletionRequest::new("s", "p").with_schema(serde_json::json!({"type": "object"})),
        );
        assert_eq!(structured["output_config"]["format"]["type"], "json_schema");
    }

    #[test]
    fn requests_stream_and_opt_into_fallbacks() {
        let provider = AnthropicApiProvider::new(Some("k".into()), None);
        let body = provider.body(&CompletionRequest::new("s", "p"));
        assert_eq!(body["stream"], true);
        assert_eq!(body["fallbacks"], "default");
        assert_eq!(body["thinking"]["type"], "adaptive");
        assert_eq!(body["model"], AnthropicApiProvider::DEFAULT_MODEL);
    }

    #[test]
    fn an_explicit_model_overrides_the_default() {
        let provider = AnthropicApiProvider::new(Some("k".into()), Some("claude-sonnet-5".into()));
        assert_eq!(
            provider.body(&CompletionRequest::new("s", "p"))["model"],
            "claude-sonnet-5"
        );
    }
}
