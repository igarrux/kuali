//! Static provider knowledge: display name, requirements, and useful models.
//!
//! Centralization lets Settings show unavailable providers and explain missing
//! requirements instead of silently omitting them.

use crate::provider::ProviderKind;

/// Fallback model shown **until the live catalog is available**. Provider data
/// from `models.rs` is authoritative because hard-coded lists age quickly.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ModelOption {
    pub id: &'static str,
    pub label: &'static str,
}

const fn model(id: &'static str, label: &'static str) -> ModelOption {
    ModelOption { id, label }
}

/// Provider descriptor independent of current availability.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ProviderDescriptor {
    pub id: &'static str,
    pub label: &'static str,
    pub kind: ProviderKind,
    /// Short description for the Settings card.
    pub description: &'static str,
    /// Plain-language setup guidance when unavailable.
    pub requirement: &'static str,
    /// Accepted key environment variables, avoiding duplicate configuration.
    pub env_vars: &'static [&'static str],
    /// Whether an API key is required.
    pub needs_api_key: bool,
    /// Whether the endpoint is configurable.
    pub configurable_base_url: bool,
    /// Default endpoint when the user provides none.
    pub default_base_url: Option<&'static str>,
    /// Executable searched on `PATH` for command-line providers.
    pub command: Option<&'static str>,
    /// Model used when none is selected. Empty requires a choice and is preferred
    /// for live catalogs, where fixed IDs eventually become stale.
    pub default_model: &'static str,
    /// Emergency suggestions normally replaced by the provider's live catalog.
    pub models: &'static [ModelOption],
    /// Whether Kuali can request the provider's catalog.
    pub lists_models: bool,
    /// Whether structured JSON is guaranteed or needs manual extraction.
    pub structured_output: bool,
}

/// All supported providers in preference order.
///
/// CLIs precede APIs because an existing authenticated session requires no new
/// key or separate billing setup.
pub const CATALOG: &[ProviderDescriptor] = &[
    ProviderDescriptor {
        id: "claude-cli",
        label: "Claude Code",
        kind: ProviderKind::LocalCli,
        description: "Usa la sesión que ya tienes iniciada en Claude Code. Sin claves ni facturas aparte.",
        requirement: "Instala Claude Code e inicia sesión con «claude».",
        env_vars: &[],
        needs_api_key: false,
        configurable_base_url: false,
        default_base_url: None,
        command: Some("claude"),
        // `sonnet` suits extraction-focused summaries and returns them quickly.
        default_model: "sonnet",
        models: &[
            model("sonnet", "Sonnet — recomendado para resumir"),
            model("opus", "Opus — más caro, para reuniones enrevesadas"),
            model("haiku", "Haiku — el más rápido"),
        ],
        structured_output: false,
        lists_models: true,
    },
    ProviderDescriptor {
        id: "codex-cli",
        label: "Codex CLI",
        kind: ProviderKind::LocalCli,
        description: "Usa la sesión de Codex instalada en tu equipo.",
        requirement: "Instala Codex CLI e inicia sesión con «codex».",
        env_vars: &[],
        needs_api_key: false,
        configurable_base_url: false,
        default_base_url: None,
        command: Some("codex"),
        default_model: "",
        models: &[],
        structured_output: true,
        lists_models: true,
    },
    ProviderDescriptor {
        id: "gemini-cli",
        label: "Gemini CLI",
        kind: ProviderKind::LocalCli,
        description: "Usa la sesión de Gemini instalada en tu equipo.",
        requirement: "Instala Gemini CLI e inicia sesión con «gemini».",
        env_vars: &[],
        needs_api_key: false,
        configurable_base_url: false,
        default_base_url: None,
        command: Some("gemini"),
        default_model: "",
        models: &[],
        structured_output: false,
        lists_models: true,
    },
    ProviderDescriptor {
        id: "anthropic-api",
        label: "Anthropic API",
        kind: ProviderKind::RemoteApi,
        description: "Claude con tu propia clave. Garantiza la forma del resumen.",
        requirement: "Pega una clave de console.anthropic.com.",
        env_vars: &["ANTHROPIC_API_KEY"],
        needs_api_key: true,
        configurable_base_url: false,
        default_base_url: None,
        command: None,
        default_model: "claude-opus-5",
        models: &[
            model("claude-opus-5", "Claude Opus 5 — la mejor calidad"),
            model("claude-sonnet-5", "Claude Sonnet 5 — equilibrado"),
            model("claude-haiku-4-5", "Claude Haiku 4.5 — el más barato"),
            model("claude-opus-4-8", "Claude Opus 4.8"),
        ],
        structured_output: true,
        lists_models: true,
    },
    ProviderDescriptor {
        id: "openai-api",
        label: "OpenAI API",
        kind: ProviderKind::RemoteApi,
        description: "GPT con tu propia clave. Garantiza la forma del resumen.",
        requirement: "Pega una clave de platform.openai.com.",
        env_vars: &["OPENAI_API_KEY"],
        needs_api_key: true,
        configurable_base_url: false,
        default_base_url: None,
        command: None,
        // OpenAI intentionally has no fixed default because its catalog changes
        // frequently. Once credentials exist, the live list supplies valid IDs.
        default_model: "",
        models: &[],
        structured_output: true,
        lists_models: true,
    },
    ProviderDescriptor {
        id: "gemini-api",
        label: "Gemini API",
        kind: ProviderKind::RemoteApi,
        description: "Gemini con tu propia clave.",
        requirement: "Pega una clave de aistudio.google.com.",
        env_vars: &["GEMINI_API_KEY", "GOOGLE_API_KEY"],
        needs_api_key: true,
        configurable_base_url: false,
        default_base_url: None,
        command: None,
        // Like OpenAI, Gemini models come from Google's live catalog.
        default_model: "",
        models: &[],
        structured_output: true,
        lists_models: true,
    },
    ProviderDescriptor {
        id: "openai-compatible",
        label: "Endpoint compatible con OpenAI",
        kind: ProviderKind::RemoteApi,
        description: "Cualquier servidor que hable como OpenAI: Ollama y LM Studio en tu equipo, OpenRouter, Groq, Together...",
        requirement: "Escribe la dirección del servidor y el nombre del modelo.",
        env_vars: &["OPENAI_COMPATIBLE_API_KEY"],
        // Local servers may need no key while remote ones do. An endpoint alone
        // therefore counts as configured.
        needs_api_key: false,
        configurable_base_url: true,
        default_base_url: Some("http://localhost:11434/v1"),
        command: None,
        default_model: "",
        models: &[],
        structured_output: true,
        lists_models: true,
    },
];

pub fn descriptor(id: &str) -> Option<&'static ProviderDescriptor> {
    CATALOG.iter().find(|entry| entry.id == id)
}

/// Resolves the configured key before environment fallbacks. A key explicitly
/// entered in Kuali must override one inherited from the launching terminal.
pub fn resolve_api_key(
    descriptor: &ProviderDescriptor,
    configured: Option<&str>,
) -> Option<String> {
    if let Some(key) = configured {
        return Some(key.to_string());
    }
    descriptor
        .env_vars
        .iter()
        .filter_map(|name| std::env::var(name).ok())
        .find(|key| !key.trim().is_empty())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_provider_has_a_distinct_id() {
        let mut ids: Vec<_> = CATALOG.iter().map(|entry| entry.id).collect();
        let count = ids.len();
        ids.sort_unstable();
        ids.dedup();
        assert_eq!(ids.len(), count);
    }

    #[test]
    fn claude_code_is_the_first_thing_we_reach_for() {
        assert_eq!(CATALOG[0].id, "claude-cli");
    }

    #[test]
    fn local_clis_have_a_binary_and_apis_have_an_environment_variable() {
        for entry in CATALOG {
            match entry.kind {
                ProviderKind::LocalCli => assert!(
                    entry.command.is_some(),
                    "{} no dice qué binario buscar",
                    entry.id
                ),
                ProviderKind::RemoteApi => assert!(
                    !entry.env_vars.is_empty(),
                    "{} no dice de qué variable sacar la clave",
                    entry.id
                ),
            }
        }
    }

    #[test]
    fn suggested_models_include_the_default_one() {
        for entry in CATALOG.iter().filter(|e| !e.models.is_empty()) {
            assert!(
                entry.models.iter().any(|m| m.id == entry.default_model),
                "el modelo por defecto de {} no está entre los sugeridos",
                entry.id
            );
        }
    }

    #[test]
    fn a_key_written_in_kuali_beats_the_environment() {
        let entry = descriptor("anthropic-api").unwrap();
        std::env::set_var("ANTHROPIC_API_KEY", "del-entorno");
        assert_eq!(
            resolve_api_key(entry, Some("de-la-interfaz")).as_deref(),
            Some("de-la-interfaz")
        );
        assert_eq!(resolve_api_key(entry, None).as_deref(), Some("del-entorno"));
        std::env::remove_var("ANTHROPIC_API_KEY");
    }
}
