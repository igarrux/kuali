//! Events reported by an audio source to the engine.
//!
//! Kuali accepts Discord voice channels and, through the browser extension,
//! Meet, Teams, and Zoom. Every source emits per-channel audio with a bound
//! participant identity.
//!
//! This vocabulary lives outside `kuali-discord` so sources never depend on one
//! another. The engine consumes a single `VoiceEvent` stream, allowing
//! segmentation, Whisper, storage, and summaries to work uniformly.

use serde::{Deserialize, Serialize};
use tokio::sync::oneshot;

use crate::meeting::{DiscordUserId, Meeting, Speaker};

/// Identifies one connection within a source. It is internal so concurrent tabs
/// or Discord sessions never share clocks, segmenters, or meeting shutdown.
pub type VoiceSessionId = u64;

#[derive(Debug, Clone)]
pub struct CallInfo {
    pub guild_id: u64,
    pub guild_name: String,
    pub channel_id: u64,
    pub channel_name: String,
    /// Destination for recording notices and summaries. Modern Discord voice
    /// channels have their own chat, so this is usually the same ID.
    pub text_channel_id: u64,
}

/// Identity of a Discord server, used to show it the way the user sees it in
/// Discord instead of a coloured initial.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct GuildInfo {
    /// Text so a snowflake survives the JavaScript boundary intact.
    pub id: String,
    pub name: String,
    /// Discord CDN address of the server icon, absent when it has none.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub icon: Option<String>,
}

#[derive(Debug)]
pub enum VoiceEvent {
    /// Every server the bot belongs to, reported once the cache is ready. It
    /// lets the library show icons for meetings recorded before this release.
    GuildsKnown(Vec<GuildInfo>),
    /// Event associated with a specific connection. Unwrapped variants remain
    /// for compatibility with older integrations.
    Session {
        session_id: VoiceSessionId,
        event: Box<VoiceEvent>,
    },
    /// A source negotiates admission before sending audio. Every admitted
    /// connection owns an independent meeting while sharing the Whisper model.
    ConnectionRequested {
        info: CallInfo,
        reply: oneshot::Sender<Result<(), String>>,
    },
    /// Kuali has entered a voice channel.
    ///
    /// Preserved for legacy integrations. Bundled connectors use
    /// `ConnectionRequested`, which can report rejection to the client.
    Connected(CallInfo),
    /// Kuali left because the followed user left or the channel became empty.
    Disconnected,
    /// A resolved participant is present in the channel.
    ParticipantPresent(Speaker),
    ParticipantLeft(DiscordUserId),
    /// Twenty milliseconds of participant audio: 16 kHz mono `i16`.
    ///
    /// Songbird asks libopus for Whisper's target format directly rather than
    /// receiving 48 kHz stereo and resampling afterward.
    Audio {
        user_id: DiscordUserId,
        pcm: Vec<i16>,
    },
    /// A participant started or stopped sending audio. The interface uses this
    /// for activity indication before Whisper closes the turn.
    SpeakingChanged {
        user_id: DiscordUserId,
        speaking: bool,
    },
    /// Discord actions resolve through the engine so they can include live
    /// meetings and cannot read data belonging to another server.
    MeetingRequested {
        meeting_id: String,
        guild_id: u64,
        reply: oneshot::Sender<Result<Meeting, String>>,
    },
    /// Newest meeting recorded in one channel. A slash command carries no
    /// meeting ID, and resolving it from the guild and channel keeps the answer
    /// inside the place where the call actually happened.
    ///
    /// An empty channel answers `Ok(None)` rather than an error so the caller
    /// can phrase it in the language of whoever asked.
    LatestMeetingRequested {
        guild_id: u64,
        channel_id: u64,
        reply: oneshot::Sender<Result<Option<Meeting>, String>>,
    },
    /// The bot resolved the configured @username. The engine persists its exact
    /// ID and updates following immediately.
    FollowRequested {
        user_id: DiscordUserId,
        reply: oneshot::Sender<Result<(), String>>,
    },
    /// Ticks every 20 ms with or without audio, providing the clock used to
    /// detect enough silence to close a turn.
    Tick,
    /// A recoverable failure occurred while the call remains active.
    Warning(String),
}
