//! Contract for anything able to summarize a meeting, from an authenticated
//! local CLI to a keyed remote API.

use async_trait::async_trait;

#[derive(Debug, thiserror::Error)]
pub enum LlmError {
    #[error("provider `{0}` is unavailable")]
    Unavailable(String),
    #[error("no LLM provider is configured")]
    NoProvider,
    #[error("{provider} failed: {message}")]
    Provider { provider: String, message: String },
    #[error("{provider} returned invalid JSON: {message}")]
    BadJson { provider: String, message: String },
    #[error("{provider} rejected the request because of its safety filters{}", detail.as_ref().map(|d| format!(" ({d})")).unwrap_or_default())]
    Refused {
        provider: String,
        detail: Option<String>,
    },
    #[error("network error while contacting {provider}: {source}")]
    Http {
        provider: String,
        #[source]
        source: reqwest::Error,
    },
    #[error("failed to run `{command}`: {source}")]
    Spawn {
        command: String,
        #[source]
        source: std::io::Error,
    },
}

/// High-level recovery path for a failed summary request.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LlmFailureKind {
    /// No provider can be selected, so Kuali should remain silent externally.
    MissingConfiguration,
    /// Credentials, quota, model selection, or context limits need a person.
    AttentionRequired,
    /// A malformed response or transient provider failure may succeed on retry.
    Retryable,
}

impl LlmError {
    pub fn failure_kind(&self) -> LlmFailureKind {
        match self {
            Self::NoProvider | Self::Unavailable(_) => LlmFailureKind::MissingConfiguration,
            Self::Spawn { .. } => LlmFailureKind::AttentionRequired,
            Self::Provider { message, .. } if provider_message_needs_attention(message) => {
                LlmFailureKind::AttentionRequired
            }
            Self::Provider { .. }
            | Self::BadJson { .. }
            | Self::Refused { .. }
            | Self::Http { .. } => LlmFailureKind::Retryable,
        }
    }
}

fn provider_message_needs_attention(message: &str) -> bool {
    let message = message.to_lowercase();
    [
        "http 401",
        "http 402",
        "http 403",
        "http 404",
        "http 429",
        "unauthorized",
        "forbidden",
        "permission_denied",
        "authentication",
        "api key",
        "api_key",
        "invalid_api_key",
        "not logged",
        "log in",
        "please login",
        "please run /login",
        "login required",
        "quota",
        "resource_exhausted",
        "rate limit",
        "rate_limit",
        "usage limit",
        "billing",
        "credit balance",
        "not enough credits",
        "insufficient credits",
        "insufficient_quota",
        "context length",
        "context window",
        "maximum context",
        "prompt is too long",
        "input is too long",
        "too many tokens",
        "token limit",
        "max_tokens",
        "model not found",
        "does not exist",
    ]
    .iter()
    .any(|needle| message.contains(needle))
}

/// Model request. Providers that enforce structured output use `json_schema`;
/// others receive prompt instructions and are validated during parsing.
#[derive(Debug, Clone)]
pub struct CompletionRequest {
    pub system: String,
    pub prompt: String,
    pub json_schema: Option<serde_json::Value>,
    pub max_tokens: u32,
}

impl CompletionRequest {
    pub fn new(system: impl Into<String>, prompt: impl Into<String>) -> Self {
        Self {
            system: system.into(),
            prompt: prompt.into(),
            json_schema: None,
            max_tokens: 16_000,
        }
    }

    pub fn with_schema(mut self, schema: serde_json::Value) -> Self {
        self.json_schema = Some(schema);
        self
    }
}

/// Credential source displayed in Settings so users do not have to infer it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum ProviderKind {
    /// Installed command-line tool with an authenticated session.
    LocalCli,
    /// Remote API using a key from the environment.
    RemoteApi,
}

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ProviderInfo {
    pub id: String,
    pub label: String,
    pub model: String,
    pub kind: ProviderKind,
    /// Whether structured JSON is guaranteed or requires manual extraction.
    pub structured_output: bool,
}

/// Provider state shown in Settings, including availability and any reason it
/// cannot be used. Unavailable providers remain visible because a missing key is
/// actionable while a missing card is not.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ProviderStatus {
    pub id: &'static str,
    pub label: &'static str,
    pub description: &'static str,
    pub kind: ProviderKind,
    pub available: bool,
    /// Missing requirement, or `None` when ready.
    pub missing: Option<&'static str>,
    /// Model that would currently be requested.
    pub model: String,
    /// Whether credentials come from the environment rather than configuration,
    /// allowing the UI to explain an intentionally empty field.
    pub api_key_from_environment: bool,
    pub needs_api_key: bool,
    pub configurable_base_url: bool,
    pub default_base_url: Option<&'static str>,
    pub default_model: &'static str,
    /// Fallback suggestions used until the UI obtains the live provider catalog.
    pub models: &'static [crate::catalog::ModelOption],
    /// Whether the provider exposes a model catalog.
    pub lists_models: bool,
    pub structured_output: bool,
}

#[async_trait]
pub trait LlmProvider: Send + Sync {
    fn id(&self) -> &'static str;
    fn info(&self) -> ProviderInfo;

    /// Cheap availability check used at startup and when opening Settings: the
    /// CLI exists and is authenticated, or the required key is configured.
    async fn is_available(&self) -> bool;

    async fn complete(&self, request: &CompletionRequest) -> Result<String, LlmError>;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn missing_providers_do_not_create_external_failure_messages() {
        assert_eq!(
            LlmError::NoProvider.failure_kind(),
            LlmFailureKind::MissingConfiguration
        );
        assert_eq!(
            LlmError::Unavailable("claude-cli".into()).failure_kind(),
            LlmFailureKind::MissingConfiguration
        );
    }

    #[test]
    fn quota_authentication_and_context_errors_require_attention() {
        for message in [
            "HTTP 429: rate_limit_exceeded",
            "HTTP 401: invalid API key",
            "credit balance is too low",
            "maximum context length exceeded",
            "RESOURCE_EXHAUSTED: quota exceeded",
            "model not found",
        ] {
            assert_eq!(
                LlmError::Provider {
                    provider: "test".into(),
                    message: message.into(),
                }
                .failure_kind(),
                LlmFailureKind::AttentionRequired,
                "{message}"
            );
        }
    }

    #[test]
    fn invalid_output_and_transient_provider_errors_can_be_retried() {
        assert_eq!(
            LlmError::BadJson {
                provider: "test".into(),
                message: "missing object".into(),
            }
            .failure_kind(),
            LlmFailureKind::Retryable
        );
        assert_eq!(
            LlmError::Provider {
                provider: "test".into(),
                message: "temporary backend failure".into(),
            }
            .failure_kind(),
            LlmFailureKind::Retryable
        );
    }
}
