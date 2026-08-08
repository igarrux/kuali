//! Gemini through `generateContent`.
//!
//! `responseSchema` is omitted because Gemini validates an OpenAPI subset that
//! rejects `additionalProperties`, making Kuali's schema incompatible. The JSON
//! MIME type guarantees syntax while the prompt and parser enforce shape.

use std::time::Duration;

use async_trait::async_trait;

use crate::provider::{CompletionRequest, LlmError, LlmProvider, ProviderInfo, ProviderKind};

pub struct GeminiApiProvider {
    api_key: String,
    model: String,
    client: reqwest::Client,
}

impl GeminiApiProvider {
    /// Construction does not require a key or model so the unavailable provider
    /// still appears in Settings. There is intentionally no default model; the
    /// current choice comes from Google's live catalog.
    pub fn new(api_key: Option<String>, model: Option<String>) -> Self {
        Self {
            api_key: api_key.unwrap_or_default(),
            model: model.unwrap_or_default(),
            client: reqwest::Client::builder()
                .timeout(Duration::from_secs(900))
                .build()
                .unwrap_or_default(),
        }
    }
}

#[async_trait]
impl LlmProvider for GeminiApiProvider {
    fn id(&self) -> &'static str {
        "gemini-api"
    }

    fn info(&self) -> ProviderInfo {
        ProviderInfo {
            id: self.id().to_string(),
            label: "Gemini API".to_string(),
            model: self.model.clone(),
            kind: ProviderKind::RemoteApi,
            structured_output: true,
        }
    }

    async fn is_available(&self) -> bool {
        !self.api_key.trim().is_empty() && !self.model.trim().is_empty()
    }

    async fn complete(&self, request: &CompletionRequest) -> Result<String, LlmError> {
        let url = format!(
            "https://generativelanguage.googleapis.com/v1beta/models/{}:generateContent",
            self.model
        );

        let mut generation_config = serde_json::json!({
            "maxOutputTokens": request.max_tokens,
        });
        if request.json_schema.is_some() {
            generation_config["responseMimeType"] = serde_json::json!("application/json");
        }

        let body = serde_json::json!({
            "systemInstruction": { "parts": [{ "text": request.system }] },
            "contents": [{ "role": "user", "parts": [{ "text": request.prompt }] }],
            "generationConfig": generation_config,
        });

        let response = self
            .client
            .post(&url)
            .header("x-goog-api-key", &self.api_key)
            .json(&body)
            .send()
            .await
            .map_err(|source| LlmError::Http {
                provider: "gemini-api".into(),
                source,
            })?;

        if !response.status().is_success() {
            let status = response.status();
            let detail = response.text().await.unwrap_or_default();
            return Err(LlmError::Provider {
                provider: "gemini-api".into(),
                message: format!("HTTP {status}: {}", detail.trim()),
            });
        }

        let json: serde_json::Value = response.json().await.map_err(|source| LlmError::Http {
            provider: "gemini-api".into(),
            source,
        })?;

        // Gemini splits long responses across multiple `parts`.
        let parts = json["candidates"][0]["content"]["parts"]
            .as_array()
            .ok_or_else(|| LlmError::BadJson {
                provider: "gemini-api".into(),
                message: json["promptFeedback"]["blockReason"]
                    .as_str()
                    .map(|r| format!("la petición fue bloqueada: {r}"))
                    .unwrap_or_else(|| "la respuesta no traía contenido".into()),
            })?;

        Ok(parts
            .iter()
            .filter_map(|p| p.get("text").and_then(|v| v.as_str()))
            .collect::<String>())
    }
}
