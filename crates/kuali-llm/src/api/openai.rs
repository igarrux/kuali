//! OpenAI through `chat/completions` with strict structured output.
//!
//! The same code supports compatible endpoints such as local Ollama or LM Studio
//! and remote OpenRouter, Groq, or Together. Supporting this common dialect
//! unlocks many models without new provider implementations.

use std::time::Duration;

use async_trait::async_trait;

use crate::catalog::ProviderDescriptor;
use crate::provider::{CompletionRequest, LlmError, LlmProvider, ProviderInfo};

const OPENAI_BASE_URL: &str = "https://api.openai.com/v1";

pub struct OpenAiApiProvider {
    descriptor: &'static ProviderDescriptor,
    api_key: String,
    model: String,
    base_url: String,
    client: reqwest::Client,
}

impl OpenAiApiProvider {
    /// The model comes from the provider catalog rather than a fixed value
    /// because endpoint names change frequently and stale IDs fail at runtime.
    pub fn new(
        descriptor: &'static ProviderDescriptor,
        api_key: Option<String>,
        model: Option<String>,
        base_url: Option<String>,
    ) -> Self {
        let fallback_base = descriptor.default_base_url.unwrap_or(OPENAI_BASE_URL);
        Self {
            descriptor,
            api_key: api_key.unwrap_or_default(),
            model: model.unwrap_or_else(|| descriptor.default_model.to_string()),
            // Accept endpoints with or without a trailing slash, a common and
            // harmless copy-paste variation.
            base_url: base_url
                .unwrap_or_else(|| fallback_base.to_string())
                .trim_end_matches('/')
                .to_string(),
            client: reqwest::Client::builder()
                .timeout(Duration::from_secs(900))
                .build()
                .unwrap_or_default(),
        }
    }

    fn endpoint(&self) -> String {
        format!("{}/chat/completions", self.base_url)
    }

    fn body(&self, request: &CompletionRequest) -> serde_json::Value {
        let mut body = serde_json::json!({
            "model": self.model,
            "store": true,
            "max_completion_tokens": request.max_tokens,
            "messages": [
                { "role": "system", "content": request.system },
                { "role": "user", "content": request.prompt },
            ],
        });

        if let Some(schema) = &request.json_schema {
            body["response_format"] = serde_json::json!({
                "type": "json_schema",
                "json_schema": {
                    "name": "meeting_summary",
                    "strict": true,
                    "schema": schema,
                }
            });
        }
        body
    }
}

#[async_trait]
impl LlmProvider for OpenAiApiProvider {
    fn id(&self) -> &'static str {
        self.descriptor.id
    }

    fn info(&self) -> ProviderInfo {
        ProviderInfo {
            id: self.descriptor.id.to_string(),
            label: self.descriptor.label.to_string(),
            model: self.model.clone(),
            kind: self.descriptor.kind,
            structured_output: self.descriptor.structured_output,
        }
    }

    async fn is_available(&self) -> bool {
        // Local compatible servers may need no key; an endpoint and model are sufficient.
        if self.descriptor.needs_api_key && self.api_key.trim().is_empty() {
            return false;
        }
        !self.base_url.is_empty() && !self.model.trim().is_empty()
    }

    async fn complete(&self, request: &CompletionRequest) -> Result<String, LlmError> {
        let mut call = self.client.post(self.endpoint());
        if !self.api_key.trim().is_empty() {
            call = call.bearer_auth(&self.api_key);
        }

        let response = call
            .json(&self.body(request))
            .send()
            .await
            .map_err(|source| LlmError::Http {
                provider: self.descriptor.id.into(),
                source,
            })?;

        if !response.status().is_success() {
            let status = response.status();
            let detail = response.text().await.unwrap_or_default();
            return Err(LlmError::Provider {
                provider: self.descriptor.id.into(),
                message: format!("HTTP {status}: {}", detail.trim()),
            });
        }

        let json: serde_json::Value = response.json().await.map_err(|source| LlmError::Http {
            provider: self.descriptor.id.into(),
            source,
        })?;

        json["choices"][0]["message"]["content"]
            .as_str()
            .map(str::to_string)
            .ok_or_else(|| LlmError::BadJson {
                provider: self.descriptor.id.into(),
                message: "la respuesta no traía contenido".into(),
            })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::catalog::descriptor;

    fn openai(model: Option<String>, base_url: Option<String>) -> OpenAiApiProvider {
        OpenAiApiProvider::new(
            descriptor("openai-api").unwrap(),
            Some("k".into()),
            model,
            base_url,
        )
    }

    #[tokio::test]
    async fn talks_to_openai_by_default_but_needs_a_model_chosen() {
        let provider = openai(None, None);
        assert_eq!(
            provider.endpoint(),
            "https://api.openai.com/v1/chat/completions"
        );
        // Without a model, report unavailable rather than risk a fixed ID that
        // the API may have retired.
        assert_eq!(provider.model, "");
        assert!(!provider.is_available().await);
    }

    #[test]
    fn a_trailing_slash_in_the_endpoint_does_not_produce_a_double_slash() {
        let provider = openai(None, Some("http://localhost:11434/v1/".into()));
        assert_eq!(
            provider.endpoint(),
            "http://localhost:11434/v1/chat/completions"
        );
    }

    #[tokio::test]
    async fn a_compatible_endpoint_works_without_a_key_but_needs_a_model() {
        let compatible = descriptor("openai-compatible").unwrap();

        let with_model = OpenAiApiProvider::new(
            compatible,
            None,
            Some("llama3.1".into()),
            Some("http://localhost:11434/v1".into()),
        );
        assert!(with_model.is_available().await);

        let without_model = OpenAiApiProvider::new(compatible, None, None, None);
        assert!(!without_model.is_available().await);
    }

    #[tokio::test]
    async fn openai_itself_still_requires_a_key() {
        let provider = OpenAiApiProvider::new(descriptor("openai-api").unwrap(), None, None, None);
        assert!(!provider.is_available().await);
    }

    #[test]
    fn schema_is_only_attached_when_the_caller_asks_for_one() {
        let provider = openai(None, None);
        assert!(provider
            .body(&CompletionRequest::new("s", "p"))
            .get("response_format")
            .is_none());

        let structured = provider.body(
            &CompletionRequest::new("s", "p").with_schema(serde_json::json!({"type": "object"})),
        );
        assert_eq!(structured["response_format"]["type"], "json_schema");
    }
}
