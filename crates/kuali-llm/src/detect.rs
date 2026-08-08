//! Resolves configuration into a ready provider.
//!
//! Kuali should work without setup when an authenticated Claude Code session is
//! already available. Settings are needed only to choose another path.

use std::sync::Arc;

use kuali_core::LlmConfig;

use crate::api::{AnthropicApiProvider, GeminiApiProvider, OpenAiApiProvider};
use crate::catalog;
use crate::cli::{ClaudeCliProvider, CodexCliProvider, GeminiCliProvider};
use crate::provider::{CompletionRequest, LlmError, LlmProvider, ProviderInfo, ProviderStatus};

/// Builds a provider by ID with user settings. `None` means the catalog ID is
/// unknown; missing credentials or binaries produce an unavailable provider.
pub fn build(config: &LlmConfig, id: &str) -> Option<Arc<dyn LlmProvider>> {
    let descriptor = catalog::descriptor(id)?;
    let settings = config.provider(id);
    let model = settings.model().map(str::to_string);
    let api_key = catalog::resolve_api_key(descriptor, settings.api_key());

    let provider: Arc<dyn LlmProvider> = match id {
        "claude-cli" => Arc::new(ClaudeCliProvider::new(model)),
        "codex-cli" => Arc::new(CodexCliProvider::new(model)),
        "gemini-cli" => Arc::new(GeminiCliProvider::new(model)),
        "anthropic-api" => Arc::new(AnthropicApiProvider::new(api_key, model)),
        "gemini-api" => Arc::new(GeminiApiProvider::new(api_key, model)),
        "openai-api" | "openai-compatible" => Arc::new(OpenAiApiProvider::new(
            descriptor,
            api_key,
            model,
            settings.base_url().map(str::to_string),
        )),
        _ => return None,
    };
    Some(provider)
}

/// Every catalog provider in preference order.
fn candidates(config: &LlmConfig) -> Vec<Arc<dyn LlmProvider>> {
    catalog::CATALOG
        .iter()
        .filter_map(|entry| build(config, entry.id))
        .collect()
}

/// Providers currently usable.
pub async fn available_providers(config: &LlmConfig) -> Vec<Arc<dyn LlmProvider>> {
    let mut available = Vec::new();
    for provider in candidates(config) {
        if provider.is_available().await {
            available.push(provider);
        }
    }
    available
}

pub async fn available_provider_infos(config: &LlmConfig) -> Vec<ProviderInfo> {
    available_providers(config)
        .await
        .iter()
        .map(|p| p.info())
        .collect()
}

/// Complete catalog with availability state, allowing Settings to show choices
/// and explain missing requirements without leaving Kuali.
pub async fn provider_statuses(config: &LlmConfig) -> Vec<ProviderStatus> {
    let mut statuses = Vec::with_capacity(catalog::CATALOG.len());
    for descriptor in catalog::CATALOG {
        let settings = config.provider(descriptor.id);
        let Some(provider) = build(config, descriptor.id) else {
            continue;
        };
        let available = provider.is_available().await;
        let has_key = !descriptor.needs_api_key
            || catalog::resolve_api_key(descriptor, settings.api_key()).is_some();

        statuses.push(ProviderStatus {
            id: descriptor.id,
            label: descriptor.label,
            description: descriptor.description,
            kind: descriptor.kind,
            available,
            // If credentials already exist, report the missing model instead of
            // asking for the same key again.
            missing: (!available).then_some({
                if has_key && descriptor.lists_models {
                    "Elige uno de sus modelos."
                } else {
                    descriptor.requirement
                }
            }),
            model: provider.info().model,
            api_key_from_environment: settings.api_key().is_none()
                && catalog::resolve_api_key(descriptor, None).is_some(),
            needs_api_key: descriptor.needs_api_key,
            configurable_base_url: descriptor.configurable_base_url,
            default_base_url: descriptor.default_base_url,
            default_model: descriptor.default_model,
            models: descriptor.models,
            lists_models: descriptor.lists_models,
            structured_output: descriptor.structured_output,
        });
    }
    statuses
}

/// Provider Kuali will use: the available explicit choice, otherwise the first
/// available provider in preference order.
pub async fn select_provider(config: &LlmConfig) -> Result<Arc<dyn LlmProvider>, LlmError> {
    if let Some(preferred) = config
        .preferred_provider
        .as_deref()
        .filter(|p| !p.is_empty())
    {
        let chosen =
            build(config, preferred).ok_or_else(|| LlmError::Unavailable(preferred.to_string()))?;
        if chosen.is_available().await {
            return Ok(chosen);
        }
        // An unavailable explicit choice should fail visibly rather than silently
        // route the transcript to an unexpected provider.
        return Err(LlmError::Unavailable(preferred.to_string()));
    }

    available_providers(config)
        .await
        .into_iter()
        .next()
        .ok_or(LlmError::NoProvider)
}

/// Requests the shortest response to validate credentials, endpoint, and model
/// before a long meeting ends.
///
/// Uses supplied rather than necessarily persisted settings, allowing a newly
/// pasted key to be tested before the user decides to save it.
pub async fn test_provider(config: &LlmConfig, id: &str) -> Result<String, LlmError> {
    let provider = build(config, id).ok_or_else(|| LlmError::Unavailable(id.to_string()))?;
    if !provider.is_available().await {
        let requirement = catalog::descriptor(id)
            .map(|d| d.requirement.to_string())
            .unwrap_or_else(|| id.to_string());
        return Err(LlmError::Provider {
            provider: id.to_string(),
            message: requirement,
        });
    }

    let mut request = CompletionRequest::new(
        "Responde exactamente «LISTO», sin nada más.",
        "Responde exactamente «LISTO», sin nada más.",
    );
    request.max_tokens = 64;

    let answer = provider.complete(&request).await?;
    let info = provider.info();
    Ok(format!("{} respondió con {}", info.label, info.model)
        + if answer.trim().is_empty() {
            ", pero no dijo nada"
        } else {
            ""
        })
}

#[cfg(test)]
mod tests {
    use super::*;
    use kuali_core::ProviderSettings;

    fn config_with(id: &str, settings: ProviderSettings) -> LlmConfig {
        let mut config = LlmConfig {
            preferred_provider: Some(id.to_string()),
            ..Default::default()
        };
        config.providers.insert(id.to_string(), settings);
        config
    }

    #[tokio::test]
    async fn an_explicitly_chosen_provider_that_is_missing_is_an_error() {
        let config = LlmConfig {
            preferred_provider: Some("no-existe".into()),
            ..Default::default()
        };
        // `Arc<dyn LlmProvider>` lacks Debug, so inspect the variant instead of
        // using `unwrap_err`.
        match select_provider(&config).await {
            Err(LlmError::Unavailable(id)) => assert_eq!(id, "no-existe"),
            Err(other) => panic!("expected Unavailable, got {other}"),
            Ok(p) => panic!("provider {} should not have been selected", p.id()),
        }
    }

    #[tokio::test]
    async fn a_key_written_in_the_settings_is_enough_to_make_a_provider_available() {
        let config = config_with(
            "anthropic-api",
            ProviderSettings {
                api_key: "clave-de-prueba".into(),
                ..Default::default()
            },
        );

        let provider = select_provider(&config)
            .await
            .expect("provider should be selected");
        assert_eq!(provider.id(), "anthropic-api");
        assert!(provider.is_available().await);
    }

    #[tokio::test]
    async fn the_model_override_only_reaches_the_chosen_provider() {
        let config = config_with(
            "anthropic-api",
            ProviderSettings {
                api_key: "clave".into(),
                model: Some("claude-sonnet-5".into()),
                ..Default::default()
            },
        );

        assert_eq!(
            build(&config, "anthropic-api").unwrap().info().model,
            "claude-sonnet-5"
        );
        // Other providers retain their own settings now that configuration is scoped.
        assert_eq!(build(&config, "openai-api").unwrap().info().model, "");
    }

    #[tokio::test]
    async fn the_catalog_is_listed_whole_with_what_each_one_is_missing() {
        let statuses = provider_statuses(&LlmConfig::default()).await;
        assert_eq!(statuses.len(), catalog::CATALOG.len());

        let anthropic = statuses
            .iter()
            .find(|status| status.id == "anthropic-api")
            .expect("Anthropic API should be listed even when it is not configured");
        if !anthropic.available {
            assert!(anthropic.missing.is_some());
        }
    }
}
