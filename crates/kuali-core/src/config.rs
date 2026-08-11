//! Persistent configuration stored as hand-editable TOML and updated by the UI.

use std::collections::BTreeMap;
use std::path::PathBuf;

use serde::{Deserialize, Serialize};

/// Mandatory notice Kuali posts when joining and when a participant arrives.
/// Keeping it outside configuration prevents stale preferences from disabling
/// informed consent accidentally.
pub const CONSENT_MESSAGE: &str = "Esta llamada se graba y transcribe para uso de los participantes. Al permanecer, consientes. Si no aceptas, sal de la llamada.";

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, rename_all = "kebab-case")]
pub struct KualiConfig {
    pub application: ApplicationConfig,
    pub discord: DiscordConfig,
    pub whisper: WhisperConfig,
    pub llm: LlmConfig,
    pub recording: RecordingConfig,
    pub integrations: IntegrationsConfig,
    /// Meetings captured through the Kuali extension. The key name remains for
    /// compatibility with earlier configurations.
    pub meet: WebMeetingsConfig,
}

/// Preferences that affect Kuali's desktop interface rather than a meeting.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, rename_all = "kebab-case")]
pub struct ApplicationConfig {
    /// `auto` follows the operating system/browser locale. `es` and `en` pin
    /// the interface to that language without changing transcription or
    /// summary languages.
    pub language: String,
    /// Install signed updates whenever no recording or summary is in progress.
    /// Kuali checks the release feed at startup regardless of this preference.
    pub automatic_updates: bool,
}

impl Default for ApplicationConfig {
    fn default() -> Self {
        Self {
            language: "auto".into(),
            automatic_updates: true,
        }
    }
}

/// Browser meetings as a second audio source for Meet, Teams, and Zoom.
///
/// Kuali does not join the meeting. A browser extension captures participant
/// audio in the existing tab and sends it over a local WebSocket.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, rename_all = "kebab-case")]
pub struct WebMeetingsConfig {
    /// Whether Kuali listens for incoming browser-meeting audio.
    pub enabled: bool,
    /// **Loopback-only** listening port, matching the extension default.
    pub port: u16,
}

impl Default for WebMeetingsConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            port: 9099,
        }
    }
}

/// Legacy name retained for integrations that already import this type.
pub type MeetConfig = WebMeetingsConfig;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, rename_all = "kebab-case")]
pub struct DiscordConfig {
    /// Bot token. This is a secret, so the file is written with mode `0600`.
    pub bot_token: String,
    /// Discord username entered in Kuali. The bot compares it against the real
    /// `username` from voice state and never the non-unique display name.
    pub follow_username: Option<String>,
    /// ID resolved by searching shared guilds for `follow_username` or receiving
    /// its next voice state. Kuali follows this user into voice channels.
    ///
    /// Serialized as text because Discord snowflakes exceed JavaScript's safe
    /// integer range. Deserialization also accepts numbers written by earlier
    /// Kuali versions.
    #[serde(with = "option_u64_string")]
    pub follow_user_id: Option<u64>,
    /// Pauses following without forgetting the ID. Manual commands such as
    /// `/record` remain available while disabled.
    pub follow_automatically: bool,
    /// Leave automatically only when no human remains in the channel.
    pub leave_when_empty: bool,
    /// Post the summary and tasks to the text channel on completion.
    pub post_summary_to_channel: bool,
}

impl Default for DiscordConfig {
    fn default() -> Self {
        Self {
            bot_token: String::new(),
            follow_username: None,
            follow_user_id: None,
            follow_automatically: true,
            leave_when_empty: true,
            post_summary_to_channel: false,
        }
    }
}

impl DiscordConfig {
    /// Username awaiting resolution, without the optional leading `@`. It stays
    /// alongside the ID so account changes or cleared advanced links can be
    /// resolved again.
    pub fn automatic_follow_username(&self) -> Option<&str> {
        if !self.follow_automatically {
            return None;
        }
        self.follow_username
            .as_deref()
            .map(str::trim)
            .map(|username| username.trim_start_matches('@'))
            .filter(|username| !username.is_empty())
    }

    /// User the bot should currently follow. Centralizing this decision keeps
    /// pause semantics consistent across consumers.
    pub fn automatic_follow_user_id(&self) -> Option<u64> {
        self.follow_automatically
            .then_some(self.follow_user_id)
            .flatten()
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, rename_all = "kebab-case")]
pub struct IntegrationsConfig {
    /// External applications that receive complete meetings after completion.
    pub webhooks: Vec<WebhookSubscription>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, rename_all = "kebab-case")]
pub struct WebhookSubscription {
    /// Stable subscription identifier; not a secret.
    pub id: String,
    /// Human-readable name of the receiving application.
    pub name: String,
    /// HTTP(S) endpoint to which Kuali sends POST requests.
    pub url: String,
    /// Standard Webhooks HMAC-SHA256 secret serialized with the `whsec_` prefix.
    pub secret: String,
    pub enabled: bool,
    pub scope: WebhookScope,
}

impl Default for WebhookSubscription {
    fn default() -> Self {
        Self {
            id: String::new(),
            name: String::new(),
            url: String::new(),
            secret: String::new(),
            enabled: true,
            scope: WebhookScope::All,
        }
    }
}

impl WebhookSubscription {
    pub fn matches(&self, meeting: &crate::MeetingMeta) -> bool {
        match &self.scope {
            WebhookScope::All => true,
            WebhookScope::Channel {
                guild_id,
                channel_id,
            } => {
                guild_id == &meeting.guild_id.to_string()
                    && channel_id == &meeting.channel_id.to_string()
            }
        }
    }

    pub fn validate_fields(&self) -> Result<(), String> {
        if self.id.trim().is_empty() {
            return Err("la suscripción no tiene identificador".into());
        }
        if self.name.trim().is_empty() {
            return Err("la suscripción no tiene nombre".into());
        }
        if self.url.trim().is_empty() {
            return Err(format!("{} no tiene URL", self.name));
        }
        if !self.secret.trim().starts_with("whsec_") {
            return Err(format!(
                "el secreto de {} debe usar el formato whsec_ de Standard Webhooks",
                self.name
            ));
        }
        if let WebhookScope::Channel {
            guild_id,
            channel_id,
        } = &self.scope
        {
            guild_id
                .parse::<u64>()
                .map_err(|_| format!("{} tiene un servidor inválido", self.name))?;
            channel_id
                .parse::<u64>()
                .map_err(|_| format!("{} tiene un canal inválido", self.name))?;
        }
        Ok(())
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "kebab-case")]
pub enum WebhookScope {
    #[default]
    All,
    Channel {
        #[serde(rename = "guild-id")]
        guild_id: String,
        #[serde(rename = "channel-id")]
        channel_id: String,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum WhisperModel {
    Tiny,
    Base,
    Small,
    Medium,
    LargeV3Turbo,
    /// Five-bit quantization with near-`large-v3-turbo` quality at roughly one
    /// third of the size; the best balance on Apple Silicon.
    LargeV3TurboQ5,
    /// Large v3 Turbo fine-tuned on Latin American Spanish from Common Voice,
    /// retained in F16 for unquantized quality comparisons.
    LargeV3TurboLatam,
    /// The same Latin American fine-tune quantized to Q5_0.
    LargeV3TurboLatamQ5,
}

impl WhisperModel {
    pub const ALL: [WhisperModel; 8] = [
        Self::Tiny,
        Self::Base,
        Self::Small,
        Self::Medium,
        Self::LargeV3Turbo,
        Self::LargeV3TurboQ5,
        Self::LargeV3TurboLatam,
        Self::LargeV3TurboLatamQ5,
    ];

    pub fn file_name(self) -> &'static str {
        match self {
            Self::Tiny => "ggml-tiny.bin",
            Self::Base => "ggml-base.bin",
            Self::Small => "ggml-small.bin",
            Self::Medium => "ggml-medium.bin",
            Self::LargeV3Turbo => "ggml-large-v3-turbo.bin",
            Self::LargeV3TurboQ5 => "ggml-large-v3-turbo-q5_0.bin",
            Self::LargeV3TurboLatam => "ggml-large-v3-turbo-latam.bin",
            Self::LargeV3TurboLatamQ5 => "ggml-large-v3-turbo-latam-q5_0.bin",
        }
    }

    pub fn download_url(self) -> String {
        let base = match self {
            Self::LargeV3TurboLatam | Self::LargeV3TurboLatamQ5 => {
                "https://github.com/igarrux/kuali-models/releases/download/v1"
            }
            _ => "https://huggingface.co/ggerganov/whisper.cpp/resolve/main",
        };
        format!("{base}/{}", self.file_name())
    }

    /// Approximate size used to warn before large downloads.
    pub fn approx_bytes(self) -> u64 {
        match self {
            Self::Tiny => 77_691_713,
            Self::Base => 147_951_465,
            Self::Small => 487_601_967,
            Self::Medium => 1_533_763_059,
            Self::LargeV3Turbo => 1_624_555_275,
            Self::LargeV3TurboQ5 => 574_041_195,
            Self::LargeV3TurboLatam => 1_624_555_275,
            Self::LargeV3TurboLatamQ5 => 574_041_195,
        }
    }

    /// SHA-256 published alongside each official `whisper.cpp` weight.
    pub fn sha256(self) -> &'static str {
        match self {
            Self::Tiny => "be07e048e1e599ad46341c8d2a135645097a538221678b7acdd1b1919c6e1b21",
            Self::Base => "60ed5bc3dd14eea856493d334349b405782ddcaf0028d4b5df4088345fba2efe",
            Self::Small => "1be3a9b2063867b937e64e2ec7483364a79917e157fa98c5d94b5c1fffea987b",
            Self::Medium => "6c14d5adee5f86394037b4e4e8b59f1673b6cee10e3cf0b11bbdbee79c156208",
            Self::LargeV3Turbo => {
                "1fc70f774d38eb169993ac391eea357ef47c88757ef72ee5943879b7e8e2bc69"
            }
            Self::LargeV3TurboQ5 => {
                "394221709cd5ad1f40c46e6031ca61bce88931e6e088c188294c6d5a55ffa7e2"
            }
            Self::LargeV3TurboLatam => {
                "b2e3f5e5b159a6978164d237f981fe95335693abb716fb1c229507b235ace540"
            }
            Self::LargeV3TurboLatamQ5 => {
                "1f6261540cb4bdb81cb5821ea40e1dc8ae3041fa4a73f00249abe3e65738dcc4"
            }
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Self::Tiny => "Tiny — el más rápido, calidad justa",
            Self::Base => "Base — rápido, útil para pruebas",
            Self::Small => "Small — equilibrio decente",
            Self::Medium => "Medium — buena calidad, más lento",
            Self::LargeV3Turbo => "Large v3 Turbo — máxima calidad",
            Self::LargeV3TurboQ5 => "Large v3 Turbo Q5 — recomendado",
            Self::LargeV3TurboLatam => "Large v3 Turbo LatAm — español latino, F16",
            Self::LargeV3TurboLatamQ5 => "Large v3 Turbo LatAm Q5 — español latino, ligero",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, rename_all = "kebab-case")]
pub struct WhisperConfig {
    pub model: WhisperModel,
    /// Directory containing weights. `None` uses `~/.kuali`; weights are never
    /// embedded in the executable.
    pub models_directory: Option<PathBuf>,
    /// ISO language code such as `es` or `en`, or `auto` for detection.
    pub language: String,
    /// Proper names and technical terms whose spelling should bias Whisper.
    pub custom_vocabulary: Vec<String>,
    /// Translate into English instead of transcribing in the source language.
    pub translate: bool,
    /// Metal acceleration on macOS. Disable only for troubleshooting.
    pub gpu: bool,
    /// CPU threads for whisper.cpp. Zero selects half the available cores.
    pub threads: u16,
}

impl Default for WhisperConfig {
    fn default() -> Self {
        Self {
            model: WhisperModel::LargeV3TurboQ5,
            models_directory: None,
            language: "es".to_string(),
            custom_vocabulary: Vec::new(),
            translate: false,
            gpu: true,
            threads: 0,
        }
    }
}

impl WhisperConfig {
    pub fn resolved_models_directory(&self) -> PathBuf {
        crate::paths::resolve_models_dir(self.models_directory.as_deref())
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, rename_all = "kebab-case")]
pub struct LlmConfig {
    /// Provider ID. Empty enables automatic detection in preference order.
    pub preferred_provider: Option<String>,
    /// Forced model for the selected provider. Retained for configuration files
    /// predating per-provider settings, then migrated into `providers` on load.
    pub model_override: Option<String>,
    /// Settings keyed by provider ID. Each provider retains its key and model
    /// when the user switches away and back.
    pub providers: BTreeMap<String, ProviderSettings>,
    /// Language used for summaries and action items.
    pub output_language: String,
    /// Allow meeting transcripts to be processed by the configured LLM.
    /// When disabled, automatic and manual summaries are both blocked.
    pub summarize_on_leave: bool,
}

impl Default for LlmConfig {
    fn default() -> Self {
        Self {
            preferred_provider: None,
            model_override: None,
            providers: BTreeMap::new(),
            output_language: "español".to_string(),
            summarize_on_leave: true,
        }
    }
}

/// User settings for one provider. Every field is optional; an unconfigured
/// provider uses environment credentials and its default model.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, rename_all = "kebab-case")]
pub struct ProviderSettings {
    /// API key entered in Kuali. Empty uses the environment variable. This secret
    /// shares the mode-`0600` configuration file with the Discord token.
    pub api_key: String,
    /// Specific model instead of the provider default.
    pub model: Option<String>,
    /// Alternate endpoint for OpenAI-compatible providers such as Ollama,
    /// LM Studio, OpenRouter, and Groq.
    pub base_url: Option<String>,
}

impl ProviderSettings {
    pub fn api_key(&self) -> Option<&str> {
        Some(self.api_key.trim()).filter(|key| !key.is_empty())
    }

    pub fn model(&self) -> Option<&str> {
        self.model
            .as_deref()
            .map(str::trim)
            .filter(|m| !m.is_empty())
    }

    pub fn base_url(&self) -> Option<&str> {
        self.base_url
            .as_deref()
            .map(str::trim)
            .filter(|url| !url.is_empty())
    }
}

impl LlmConfig {
    /// Provider settings, or empty defaults when never configured.
    pub fn provider(&self, id: &str) -> ProviderSettings {
        self.providers.get(id).cloned().unwrap_or_default()
    }

    /// Moves the legacy global `model-override` into the selected provider,
    /// preserving the choice while migrating to the per-provider format.
    fn migrate_model_override(&mut self) {
        let (Some(provider), Some(model)) = (
            self.preferred_provider
                .clone()
                .filter(|p| !p.trim().is_empty()),
            self.model_override.take().filter(|m| !m.trim().is_empty()),
        ) else {
            return;
        };
        let settings = self.providers.entry(provider).or_default();
        if settings.model().is_none() {
            settings.model = Some(model);
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, rename_all = "kebab-case")]
pub struct RecordingConfig {
    /// Silence that closes a speech turn once it is **long enough**.
    pub silence_ms: u64,
    /// Target audio duration before sending a turn to Whisper. Brief silence
    /// below this target keeps collecting context because complete phrases are
    /// more reliable than fragments. This is not a hard minimum; see
    /// `end_of_turn_ms`.
    pub target_segment_ms: u64,
    /// Silence that closes a turn even below the target, preventing short but
    /// complete replies from merging into much later speech.
    pub end_of_turn_ms: u64,
    /// Forced cut so a continuously speaking participant is transcribed before
    /// eventually becoming silent.
    pub max_segment_ms: u64,
    /// Audio below this duration is usually coughing, throat clearing, or
    /// microphone impact and is not worth decoding.
    pub min_segment_ms: u64,
}

impl Default for RecordingConfig {
    fn default() -> Self {
        Self {
            silence_ms: 700,
            target_segment_ms: 3_000,
            end_of_turn_ms: 2_500,
            max_segment_ms: 20_000,
            // Below roughly half a second there is usually only a syllable, and
            // Whisper tends to invent the rest of the word.
            min_segment_ms: 700,
        }
    }
}

impl KualiConfig {
    /// Idempotently migrates a configuration immediately after loading it.
    pub fn migrated(mut self) -> Self {
        self.llm.migrate_model_override();
        self
    }

    /// Requirements still missing before Kuali can operate.
    pub fn missing_requirements(&self) -> Vec<&'static str> {
        let mut missing = Vec::new();
        if self.discord.bot_token.trim().is_empty() {
            missing.push("El token del bot de Discord");
        }
        missing
    }

    pub fn is_ready(&self) -> bool {
        self.missing_requirements().is_empty()
    }
}

/// Keeps a Rust `u64` while crossing JSON as text. Accepting integers as input
/// also preserves compatibility with configuration files written before this
/// change.
mod option_u64_string {
    use serde::{de::Error, Deserialize, Deserializer, Serializer};

    #[derive(Deserialize)]
    #[serde(untagged)]
    enum StringOrInteger {
        String(String),
        Unsigned(u64),
        Signed(i64),
    }

    pub fn serialize<S>(value: &Option<u64>, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        match value {
            Some(value) => serializer.serialize_some(&value.to_string()),
            None => serializer.serialize_none(),
        }
    }

    pub fn deserialize<'de, D>(deserializer: D) -> Result<Option<u64>, D::Error>
    where
        D: Deserializer<'de>,
    {
        let value = Option::<StringOrInteger>::deserialize(deserializer)?;
        value
            .map(|value| match value {
                StringOrInteger::String(value) => value.parse().map_err(D::Error::custom),
                StringOrInteger::Unsigned(value) => Ok(value),
                StringOrInteger::Signed(value) => u64::try_from(value).map_err(D::Error::custom),
            })
            .transpose()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_config_reports_what_is_missing() {
        let cfg = KualiConfig::default();
        assert_eq!(cfg.application.language, "auto");
        assert!(!cfg.is_ready());
        assert_eq!(
            cfg.missing_requirements(),
            vec!["El token del bot de Discord"]
        );
    }

    #[test]
    fn application_preferences_round_trip_and_old_configs_use_safe_defaults() {
        let mut cfg = KualiConfig::default();
        cfg.application.language = "en".into();
        cfg.application.automatic_updates = false;
        let saved = toml_round_trip(&cfg);
        assert_eq!(saved.application.language, "en");
        assert!(!saved.application.automatic_updates);

        let old: KualiConfig = serde_json::from_value(serde_json::json!({
            "discord": { "bot-token": "token" }
        }))
        .expect("deserialize config written before UI languages existed");
        assert_eq!(old.application.language, "auto");
        assert!(old.application.automatic_updates);
    }

    #[test]
    fn config_is_ready_with_a_token_even_without_a_followed_user() {
        let cfg = KualiConfig {
            discord: DiscordConfig {
                bot_token: "token".into(),
                ..Default::default()
            },
            ..Default::default()
        };
        assert!(cfg.is_ready());
    }

    #[test]
    fn consent_message_is_the_required_copy() {
        assert_eq!(
            CONSENT_MESSAGE,
            "Esta llamada se graba y transcribe para uso de los participantes. Al permanecer, consientes. Si no aceptas, sal de la llamada."
        );
    }

    #[test]
    fn config_round_trips_through_toml() {
        let cfg = KualiConfig::default();
        let text = toml_round_trip(&cfg);
        assert_eq!(cfg, text);
    }

    #[test]
    fn webhook_subscriptions_round_trip_with_string_discord_ids() {
        let mut cfg = KualiConfig::default();
        cfg.integrations.webhooks.push(WebhookSubscription {
            id: "hook-1".into(),
            name: "Mi app".into(),
            url: "http://localhost:3000/kuali".into(),
            secret: "whsec_MDEyMzQ1Njc4OWFiY2RlZjAxMjM0NTY3ODlhYmNkZWY=".into(),
            enabled: true,
            scope: WebhookScope::Channel {
                guild_id: "123456789012345678".into(),
                channel_id: "987654321098765432".into(),
            },
        });

        let json = serde_json::to_value(&cfg).unwrap();
        assert_eq!(
            json["integrations"]["webhooks"][0]["scope"]["channel-id"],
            "987654321098765432"
        );
        assert_eq!(toml_round_trip(&cfg), cfg);
    }

    #[test]
    fn webhook_scope_matches_all_or_one_exact_channel() {
        let meta = crate::MeetingMeta {
            id: "meeting".into(),
            display_title: None,
            guild_id: 12,
            guild_name: "Servidor".into(),
            channel_id: 34,
            channel_name: "General".into(),
            started_at: chrono::Utc::now(),
            ended_at: None,
        };
        let mut webhook = WebhookSubscription {
            scope: WebhookScope::All,
            ..Default::default()
        };
        assert!(webhook.matches(&meta));
        webhook.scope = WebhookScope::Channel {
            guild_id: "12".into(),
            channel_id: "34".into(),
        };
        assert!(webhook.matches(&meta));
        webhook.scope = WebhookScope::Channel {
            guild_id: "12".into(),
            channel_id: "35".into(),
        };
        assert!(!webhook.matches(&meta));
    }

    #[test]
    fn custom_vocabulary_keeps_each_term_and_uses_the_frontend_key() {
        let mut cfg = KualiConfig::default();
        cfg.whisper.custom_vocabulary = vec!["WaitingRoom".into(), "Kuali".into()];

        let json = serde_json::to_value(&cfg).expect("serialize config");
        assert_eq!(
            json["whisper"]["custom-vocabulary"],
            serde_json::json!(["WaitingRoom", "Kuali"])
        );
        assert_eq!(toml_round_trip(&cfg), cfg);
    }

    #[test]
    fn changing_the_models_directory_preserves_language_and_vocabulary() {
        let mut cfg = KualiConfig::default();
        cfg.whisper.language = "es".into();
        cfg.whisper.custom_vocabulary = vec!["ONWEV".into(), "WaitingRoom".into()];

        let mut moved = cfg.clone();
        moved.whisper.models_directory = Some(PathBuf::from("/Volumes/modelos-kuali"));

        assert_eq!(moved.whisper.language, cfg.whisper.language);
        assert_eq!(
            moved.whisper.custom_vocabulary,
            cfg.whisper.custom_vocabulary
        );
    }

    #[test]
    fn discord_user_id_is_serialized_as_a_string_for_javascript() {
        let user_id = 1_234_567_890_123_456_789;
        let cfg = KualiConfig {
            discord: DiscordConfig {
                follow_user_id: Some(user_id),
                ..Default::default()
            },
            ..Default::default()
        };

        let json = serde_json::to_value(cfg).expect("serialize config");
        assert_eq!(
            json["discord"]["follow-user-id"],
            serde_json::Value::String(user_id.to_string())
        );
    }

    #[test]
    fn automatic_follow_can_be_paused_without_forgetting_the_user() {
        let mut discord = DiscordConfig {
            follow_username: Some("garrux".into()),
            follow_user_id: Some(42),
            ..Default::default()
        };
        assert_eq!(discord.automatic_follow_user_id(), Some(42));
        assert_eq!(discord.automatic_follow_username(), Some("garrux"));

        discord.follow_automatically = false;
        assert_eq!(discord.follow_username.as_deref(), Some("garrux"));
        assert_eq!(discord.follow_user_id, Some(42));
        assert_eq!(discord.automatic_follow_user_id(), None);
        assert_eq!(discord.automatic_follow_username(), None);
    }

    #[test]
    fn discord_username_accepts_the_at_sign_but_never_compares_it() {
        let discord = DiscordConfig {
            follow_username: Some("  @Garrux  ".into()),
            ..Default::default()
        };

        assert_eq!(discord.automatic_follow_username(), Some("Garrux"));
    }

    #[test]
    fn old_configs_keep_automatic_follow_enabled() {
        let cfg: KualiConfig =
            toml::from_str("[discord]\nbot-token = \"token\"\nfollow-user-id = \"42\"\n")
                .expect("deserialize config without the new switch");

        assert!(cfg.discord.follow_automatically);
        assert_eq!(cfg.discord.automatic_follow_user_id(), Some(42));
    }

    #[test]
    fn legacy_numeric_discord_user_id_still_deserializes() {
        let user_id = 1_234_567_890_123_456_789_u64;
        let cfg: KualiConfig = serde_json::from_value(serde_json::json!({
            "discord": { "follow-user-id": user_id }
        }))
        .expect("deserialize legacy config");

        assert_eq!(cfg.discord.follow_user_id, Some(user_id));
    }

    #[test]
    fn legacy_numeric_toml_discord_user_id_still_deserializes() {
        let user_id = 1_234_567_890_123_456_789_u64;
        let cfg: KualiConfig = toml::from_str(&format!("[discord]\nfollow-user-id = {user_id}\n"))
            .expect("deserialize legacy TOML config");

        assert_eq!(cfg.discord.follow_user_id, Some(user_id));
    }

    #[test]
    fn a_legacy_model_override_moves_to_the_chosen_provider() {
        let cfg = toml::from_str::<KualiConfig>(
            "[llm]\npreferred-provider = \"anthropic-api\"\nmodel-override = \"claude-sonnet-5\"\n",
        )
        .expect("deserialize legacy llm preferences")
        .migrated();

        assert_eq!(cfg.llm.model_override, None);
        assert_eq!(
            cfg.llm.provider("anthropic-api").model(),
            Some("claude-sonnet-5")
        );
        // A second migration must neither revert nor duplicate anything.
        assert_eq!(cfg.clone().migrated(), cfg);
    }

    #[test]
    fn a_legacy_model_override_without_a_provider_is_dropped_instead_of_guessed() {
        let cfg = toml::from_str::<KualiConfig>("[llm]\nmodel-override = \"un-modelo\"\n")
            .expect("deserialize legacy llm preferences")
            .migrated();

        // Without a selected provider, assigning this alias to the first
        // available one could route summaries to the wrong service.
        assert!(cfg.llm.providers.is_empty());
    }

    #[test]
    fn provider_settings_treat_blanks_as_absent() {
        let settings = ProviderSettings {
            api_key: "   ".into(),
            model: Some(String::new()),
            base_url: Some("  ".into()),
        };
        assert_eq!(settings.api_key(), None);
        assert_eq!(settings.model(), None);
        assert_eq!(settings.base_url(), None);
    }

    #[test]
    fn legacy_announcement_preferences_are_safely_ignored() {
        let cfg: KualiConfig = toml::from_str(
            "[discord]\nbot-token = \"token\"\nannounce-on-join = false\nannounce-message = \"Mensaje viejo\"\n",
        )
        .expect("deserialize legacy announcement preferences");

        assert!(cfg.is_ready());
    }

    fn toml_round_trip(cfg: &KualiConfig) -> KualiConfig {
        // `toml` is not a kuali-core dependency, so JSON exercises the same
        // `#[serde(default)]` contract here.
        let json = serde_json::to_string(cfg).expect("serialize");
        serde_json::from_str(&json).expect("deserialize")
    }

    #[test]
    fn every_model_has_a_distinct_file_name() {
        let mut names: Vec<_> = WhisperModel::ALL.iter().map(|m| m.file_name()).collect();
        names.sort_unstable();
        let count = names.len();
        names.dedup();
        assert_eq!(names.len(), count);
    }

    #[test]
    fn every_model_has_an_official_sha256() {
        for model in WhisperModel::ALL {
            assert_eq!(model.sha256().len(), 64);
            assert!(model
                .sha256()
                .chars()
                .all(|character| character.is_ascii_hexdigit()));
        }
    }

    #[test]
    fn latam_models_keep_stable_ids_and_download_urls() {
        let cases = [
            (WhisperModel::LargeV3TurboLatam, "large-v3-turbo-latam"),
            (WhisperModel::LargeV3TurboLatamQ5, "large-v3-turbo-latam-q5"),
        ];

        for (model, expected_id) in cases {
            assert_eq!(
                serde_json::to_value(model).unwrap().as_str(),
                Some(expected_id)
            );
            assert_eq!(
                model.download_url(),
                format!(
                    "https://github.com/igarrux/kuali-models/releases/download/v1/{}",
                    model.file_name()
                )
            );
        }
    }
}
