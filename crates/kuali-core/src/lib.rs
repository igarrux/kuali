//! Types shared across Kuali.
//!
//! Kuali ("to listen" in Kogi) joins Discord calls, attributes each transcribed
//! sentence to its speaker, and asks an LLM for action items and a summary when
//! the call ends.
//!
//! This crate has no behavior of its own. It defines the vocabulary shared by
//! `kuali-discord`, `kuali-stt`, `kuali-llm`, `kuali-store`, and `kuali-engine`.

pub mod config;
pub mod event;
pub mod meeting;
pub mod paths;
pub mod source;

pub use config::{
    ApplicationConfig, DiscordConfig, IntegrationsConfig, KualiConfig, LlmConfig, MeetConfig,
    ProviderSettings, RecordingConfig, WebMeetingsConfig, WebhookScope, WebhookSubscription,
    WhisperConfig, WhisperModel, CONSENT_MESSAGE,
};
pub use event::{EngineStatus, KualiEvent, ModelState};
pub use meeting::{
    color_for, format_timestamp, sanitize_folder, sanitize_tags, ActionItem,
    DiscordSummaryDelivery, DiscordUserId, Meeting, MeetingMeta, MeetingNote, MeetingSummary,
    Speaker, SpeakerId, Utterance, MAX_FOLDER_CHARS, MAX_TAGS_PER_MEETING, MAX_TAG_CHARS,
};
pub use source::{CallInfo, GuildInfo, VoiceEvent, VoiceSessionId};
