//! Everything the engine reports to the interface, serialized as one enum and
//! emitted over Tauri's event channel.

use serde::{Deserialize, Serialize};

use crate::config::WhisperModel;
use crate::meeting::{DiscordUserId, MeetingMeta, MeetingSummary, Speaker, Utterance};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum EngineStatus {
    /// Missing token or configuration; Kuali is not connected to Discord.
    Offline,
    /// Connected and waiting for the followed user to enter a voice channel.
    Watching,
    /// Joining the channel and loading Whisper.
    Joining,
    /// In a meeting and transcribing.
    Recording,
    /// The call ended; pending audio is being drained before summarization.
    Finalizing,
    /// Outside the call and requesting the LLM summary.
    Summarizing,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(
    tag = "state",
    rename_all = "camelCase",
    rename_all_fields = "camelCase"
)]
pub enum ModelState {
    /// Weights are not yet present on disk.
    Absent,
    Downloading {
        /// Exact weight being transferred, so every interface can identify it.
        model: WhisperModel,
        downloaded_bytes: u64,
        total_bytes: Option<u64>,
    },
    /// Downloaded but not in memory: the idle state between meetings.
    Ready,
    /// Verifying the official SHA-256 after relocating weights.
    Verifying,
    Loading,
    /// Loaded in RAM and transcribing.
    Active,
    Failed {
        message: String,
    },
}

/// Event sent from the engine to the interface. The JSON `type` field identifies
/// the variant for direct frontend dispatch.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(
    tag = "type",
    rename_all = "camelCase",
    rename_all_fields = "camelCase"
)]
pub enum KualiEvent {
    StatusChanged {
        status: EngineStatus,
    },
    ModelStateChanged {
        state: ModelState,
    },
    /// A same-size weight failed its delayed integrity check and will be fetched
    /// again without requiring the user to leave or rejoin the meeting.
    ModelRecoveryStarted {
        model: WhisperModel,
    },
    /// State of the local port used by the browser extension.
    WebMeetingsStatusChanged {
        enabled: bool,
        port: u16,
        listening: bool,
    },
    DiscordFollowChanged {
        /// Text prevents snowflake truncation at the JavaScript boundary.
        user_id: String,
        enabled: bool,
    },
    MeetingStarted {
        meeting: MeetingMeta,
    },
    MeetingEnded {
        meeting_id: String,
    },
    SpeakerJoined {
        meeting_id: String,
        speaker: Speaker,
    },
    SpeakerLeft {
        meeting_id: String,
        user_id: DiscordUserId,
    },
    /// A participant is currently speaking, allowing live indication before
    /// Whisper returns text.
    SpeakingChanged {
        meeting_id: String,
        user_id: DiscordUserId,
        speaking: bool,
    },
    UtteranceAdded {
        meeting_id: String,
        utterance: Utterance,
    },
    /// Ephemeral text for an open turn. It is neither stored nor summarized.
    UtterancePreview {
        meeting_id: String,
        utterance: Utterance,
    },
    UtterancePreviewCleared {
        meeting_id: String,
        utterance_id: String,
    },
    SummaryStarted {
        meeting_id: String,
    },
    SummaryReady {
        meeting_id: String,
        summary: MeetingSummary,
    },
    /// Recoverable errors shown in the interface without stopping the app.
    Error {
        message: String,
        /// User-facing context such as `discord`, `whisper`, `llm`, or `store`.
        source: String,
    },
}

impl KualiEvent {
    pub fn error(source: impl Into<String>, message: impl std::fmt::Display) -> Self {
        Self::Error {
            message: message.to_string(),
            source: source.into(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn followed_user_ids_cross_the_frontend_boundary_as_text() {
        let event = KualiEvent::DiscordFollowChanged {
            user_id: "543321203243483137".into(),
            enabled: true,
        };
        let json = serde_json::to_value(event).unwrap();
        assert_eq!(json["type"], "discordFollowChanged");
        assert_eq!(json["userId"], "543321203243483137");
    }

    #[test]
    fn model_download_progress_identifies_the_exact_weight() {
        let state = ModelState::Downloading {
            model: WhisperModel::LargeV3,
            downloaded_bytes: 49_000_000,
            total_bytes: Some(3_095_033_483),
        };
        let json = serde_json::to_value(state).unwrap();

        assert_eq!(json["state"], "downloading");
        assert_eq!(json["model"], "large-v3");
        assert_eq!(json["downloadedBytes"], 49_000_000u64);
        assert_eq!(json["totalBytes"], 3_095_033_483u64);
    }

    #[test]
    fn model_recovery_identifies_the_weight_that_will_be_replaced() {
        let json = serde_json::to_value(KualiEvent::ModelRecoveryStarted {
            model: WhisperModel::LargeV3,
        })
        .unwrap();

        assert_eq!(json["type"], "modelRecoveryStarted");
        assert_eq!(json["model"], "large-v3");
    }
}
