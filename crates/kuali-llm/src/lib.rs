//! Meeting summaries and action-item extraction.
//!
//! Kuali is provider-neutral. It can use an authenticated local CLI such as
//! Claude Code, Codex, or Gemini, or a remote API, selecting the best available
//! provider unless explicitly configured otherwise.

pub mod api;
pub mod catalog;
pub mod cli;
pub mod detect;
pub mod json;
pub mod models;
pub mod provider;
pub mod summarize;

pub use api::{AnthropicApiProvider, GeminiApiProvider, OpenAiApiProvider};
pub use catalog::{ModelOption, ProviderDescriptor, CATALOG};
pub use cli::{ClaudeCliProvider, CodexCliProvider, GeminiCliProvider};
pub use detect::{
    available_provider_infos, available_providers, provider_statuses, select_provider,
    test_provider,
};
pub use models::{list_models, ModelChoice};
pub use provider::{
    CompletionRequest, LlmError, LlmFailureKind, LlmProvider, ProviderInfo, ProviderKind,
    ProviderStatus,
};
pub use summarize::{parse_summary, summarize, system_prompt, user_prompt};
