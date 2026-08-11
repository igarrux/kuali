//! Meeting types: who spoke, when they spoke, and the resulting insights.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

/// Stable internal participant identifier within Kuali.
///
/// The legacy alias remains because Discord is still one source, while browser
/// platforms also produce speaker identifiers.
pub type SpeakerId = u64;
pub type DiscordUserId = SpeakerId;

/// Stable speaker palette readable on light and dark backgrounds. Deriving the
/// index from the participant ID keeps colors consistent across meetings.
const SPEAKER_PALETTE: [&str; 8] = [
    "#4C8DFF", "#E8833A", "#3FBF8F", "#C563D6", "#E5555F", "#3FB6C9", "#B58A2E", "#8B7CF0",
];

pub fn color_for(user_id: DiscordUserId) -> &'static str {
    // Low bits in Discord snowflakes are nearly constant within one process, so
    // mix the value before distributing it across the palette.
    let mixed = user_id.wrapping_mul(0x9E37_79B9_7F4A_7C15) >> 32;
    SPEAKER_PALETTE[(mixed % SPEAKER_PALETTE.len() as u64) as usize]
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Speaker {
    pub user_id: DiscordUserId,
    /// Original platform identifier (`participantId`, `userId`, and similar).
    /// Discord snowflakes fit in `user_id`; browser IDs are usually textual.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source_id: Option<String>,
    /// `separate` for an individual channel; `mixed` when the platform only
    /// exposes room audio.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub audio_kind: Option<String>,
    /// Server nickname when present; otherwise the global display name.
    pub display_name: String,
    /// Account handle, useful for disambiguating repeated display names.
    pub username: String,
    pub avatar_url: Option<String>,
    pub color: String,
    pub is_bot: bool,
}

impl Speaker {
    pub fn unknown(user_id: DiscordUserId) -> Self {
        Self {
            user_id,
            source_id: None,
            audio_kind: None,
            display_name: format!("Desconocido ({user_id})"),
            username: String::new(),
            avatar_url: None,
            color: color_for(user_id).to_string(),
            is_bot: false,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Utterance {
    pub id: String,
    pub speaker_id: DiscordUserId,
    /// Milliseconds since the meeting began.
    pub start_ms: u64,
    pub end_ms: u64,
    pub text: String,
    /// Mean probability reported by Whisper for this segment, from 0.0 to 1.0.
    pub confidence: Option<f32>,
}

impl Utterance {
    pub fn duration_ms(&self) -> u64 {
        self.end_ms.saturating_sub(self.start_ms)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ActionItem {
    pub id: String,
    pub text: String,
    /// Assignee exactly as stated during the meeting.
    pub assignee: Option<String>,
    /// Mentioned date or deadline in its original wording.
    pub due: Option<String>,
    /// Transcript position that produced the task, used for navigation.
    pub source_ms: Option<u64>,
    #[serde(default)]
    pub done: bool,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct MeetingSummary {
    /// Short recognizable name used to find the meeting again.
    #[serde(default)]
    pub title: String,
    pub overview: String,
    #[serde(default)]
    pub key_points: Vec<String>,
    #[serde(default)]
    pub decisions: Vec<String>,
    #[serde(default)]
    pub action_items: Vec<ActionItem>,
    #[serde(default)]
    pub open_questions: Vec<String>,
    /// Provider that generated the result, preserving summary provenance.
    #[serde(default)]
    pub generated_by: String,
}

/// Location of Kuali's compact meeting card in Discord.
///
/// Keeping the channel before the first successful delivery lets a regenerated
/// summary retry publication. Once Discord accepts a message, its ID turns
/// later regenerations into edits instead of duplicate channel posts.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DiscordSummaryDelivery {
    /// Discord snowflakes cross JSON as strings so JavaScript cannot truncate
    /// them before a meeting is saved again by the desktop interface.
    #[serde(with = "snowflake_string")]
    pub channel_id: u64,
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        with = "snowflake_string::option"
    )]
    pub message_id: Option<u64>,
}

impl DiscordSummaryDelivery {
    pub fn pending(channel_id: u64) -> Self {
        Self {
            channel_id,
            message_id: None,
        }
    }

    pub fn delivered(channel_id: u64, message_id: u64) -> Self {
        Self {
            channel_id,
            message_id: Some(message_id),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct MeetingMeta {
    pub id: String,
    /// Starts with a participant-based name that the LLM may improve during
    /// summarization. `default` keeps legacy meetings readable.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub display_title: Option<String>,
    pub guild_id: u64,
    pub guild_name: String,
    pub channel_id: u64,
    pub channel_name: String,
    pub started_at: DateTime<Utc>,
    pub ended_at: Option<DateTime<Utc>>,
}

impl MeetingMeta {
    pub fn title(&self) -> String {
        self.display_title
            .as_deref()
            .filter(|title| !title.trim().is_empty())
            .map(str::to_string)
            .unwrap_or_else(|| self.source_title())
    }

    pub fn source_title(&self) -> String {
        format!("{} · {}", self.guild_name, self.channel_name)
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Meeting {
    // Avoid `flatten`: a nested header gives the interface a clearer contract
    // than mixing metadata with content.
    pub meta: MeetingMeta,
    #[serde(default)]
    pub speakers: Vec<Speaker>,
    #[serde(default)]
    pub utterances: Vec<Utterance>,
    #[serde(default)]
    pub summary: Option<MeetingSummary>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub discord_summary_delivery: Option<DiscordSummaryDelivery>,
}

impl Meeting {
    pub fn new(meta: MeetingMeta) -> Self {
        Self {
            meta,
            speakers: Vec::new(),
            utterances: Vec::new(),
            summary: None,
            discord_summary_delivery: None,
        }
    }

    pub fn speaker(&self, id: DiscordUserId) -> Option<&Speaker> {
        self.speakers.iter().find(|s| s.user_id == id)
    }

    pub fn speaker_name(&self, id: DiscordUserId) -> String {
        self.speaker(id)
            .map(|s| s.display_name.clone())
            .unwrap_or_else(|| format!("Desconocido ({id})"))
    }

    pub fn upsert_speaker(&mut self, speaker: Speaker) {
        match self
            .speakers
            .iter_mut()
            .find(|s| s.user_id == speaker.user_id)
        {
            Some(existing) => *existing = speaker,
            None => self.speakers.push(speaker),
        }
    }

    /// Inserts chronologically. Parallel speakers can complete out of order when
    /// a long segment finishes after a later short one.
    pub fn push_utterance(&mut self, utterance: Utterance) {
        let pos = self
            .utterances
            .iter()
            .rposition(|u| u.start_ms <= utterance.start_ms)
            .map(|i| i + 1)
            .unwrap_or(0);
        self.utterances.insert(pos, utterance);
    }

    /// Inserts an utterance or replaces one with the same ID while preserving
    /// chronological order. Returns `true` when an item was replaced.
    pub fn upsert_utterance(&mut self, utterance: Utterance) -> bool {
        if let Some(existing) = self
            .utterances
            .iter_mut()
            .find(|existing| existing.id == utterance.id)
        {
            *existing = utterance;
            true
        } else {
            self.push_utterance(utterance);
            false
        }
    }

    pub fn duration_ms(&self) -> u64 {
        self.utterances.last().map(|u| u.end_ms).unwrap_or(0)
    }

    /// Useful name even without an LLM, avoiding date-only or ephemeral browser
    /// meeting identifiers in the library.
    pub fn fallback_title(&self) -> String {
        let names = self
            .speakers
            .iter()
            .filter(|speaker| !speaker.is_bot && !speaker.display_name.trim().is_empty())
            .map(|speaker| speaker.display_name.trim())
            .collect::<Vec<_>>();
        match names.as_slice() {
            [] if self.meta.guild_name == "Google Meet" => "Reunión de Google Meet".into(),
            [] if self.meta.guild_name == "Microsoft Teams" => "Reunión de Microsoft Teams".into(),
            [] if self.meta.guild_name == "Zoom" => "Reunión de Zoom".into(),
            [] => format!("Reunión en #{}", self.meta.channel_name),
            [one] => format!("Sesión de {one}"),
            [one, two] => format!("{one} y {two}"),
            [one, two, three] => format!("{one}, {two} y {three}"),
            [one, two, rest @ ..] => format!("{one}, {two} y {} más", rest.len()),
        }
    }

    /// Transcript representation sent to the LLM: timestamps, real speaker
    /// names, and adjacent turns merged to avoid wasting context on repetition.
    pub fn transcript_text(&self) -> String {
        let mut out = String::new();
        let mut current: Option<(DiscordUserId, u64, String)> = None;

        for u in &self.utterances {
            match &mut current {
                Some((speaker, _, buf)) if *speaker == u.speaker_id => {
                    buf.push(' ');
                    buf.push_str(u.text.trim());
                }
                _ => {
                    if let Some((speaker, start, buf)) = current.take() {
                        push_turn(&mut out, &self.speaker_name(speaker), start, &buf);
                    }
                    current = Some((u.speaker_id, u.start_ms, u.text.trim().to_string()));
                }
            }
        }
        if let Some((speaker, start, buf)) = current {
            push_turn(&mut out, &self.speaker_name(speaker), start, &buf);
        }
        out
    }
}

mod snowflake_string {
    use serde::{de::Error, Deserialize, Deserializer, Serializer};

    #[derive(Deserialize)]
    #[serde(untagged)]
    enum StringOrInteger {
        String(String),
        Unsigned(u64),
        Signed(i64),
    }

    pub fn serialize<S>(value: &u64, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        serializer.serialize_str(&value.to_string())
    }

    pub fn deserialize<'de, D>(deserializer: D) -> Result<u64, D::Error>
    where
        D: Deserializer<'de>,
    {
        match StringOrInteger::deserialize(deserializer)? {
            StringOrInteger::String(value) => value.parse().map_err(D::Error::custom),
            StringOrInteger::Unsigned(value) => Ok(value),
            StringOrInteger::Signed(value) => u64::try_from(value).map_err(D::Error::custom),
        }
    }

    pub mod option {
        use super::StringOrInteger;
        use serde::{de::Error, Deserialize, Deserializer, Serializer};

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
            Option::<StringOrInteger>::deserialize(deserializer)?
                .map(|value| match value {
                    StringOrInteger::String(value) => value.parse().map_err(D::Error::custom),
                    StringOrInteger::Unsigned(value) => Ok(value),
                    StringOrInteger::Signed(value) => {
                        u64::try_from(value).map_err(D::Error::custom)
                    }
                })
                .transpose()
        }
    }
}

fn push_turn(out: &mut String, name: &str, start_ms: u64, text: &str) {
    if text.is_empty() {
        return;
    }
    out.push_str(&format!(
        "[{}] {}: {}\n",
        format_timestamp(start_ms),
        name,
        text
    ));
}

pub fn format_timestamp(ms: u64) -> String {
    let total_secs = ms / 1000;
    let (h, m, s) = (total_secs / 3600, (total_secs % 3600) / 60, total_secs % 60);
    if h > 0 {
        format!("{h:02}:{m:02}:{s:02}")
    } else {
        format!("{m:02}:{s:02}")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn meeting_with(utterances: Vec<(DiscordUserId, u64, u64, &str)>) -> Meeting {
        let mut m = Meeting::new(MeetingMeta {
            id: "m1".into(),
            display_title: None,
            guild_id: 1,
            guild_name: "Servidor".into(),
            channel_id: 2,
            channel_name: "General".into(),
            started_at: Utc::now(),
            ended_at: None,
        });
        m.upsert_speaker(Speaker {
            user_id: 10,
            source_id: None,
            audio_kind: None,
            display_name: "Ana".into(),
            username: "ana".into(),
            avatar_url: None,
            color: color_for(10).to_string(),
            is_bot: false,
        });
        m.upsert_speaker(Speaker {
            user_id: 20,
            source_id: None,
            audio_kind: None,
            display_name: "Luis".into(),
            username: "luis".into(),
            avatar_url: None,
            color: color_for(20).to_string(),
            is_bot: false,
        });
        for (speaker_id, start_ms, end_ms, text) in utterances {
            m.push_utterance(Utterance {
                id: format!("{speaker_id}-{start_ms}"),
                speaker_id,
                start_ms,
                end_ms,
                text: text.into(),
                confidence: None,
            });
        }
        m
    }

    #[test]
    fn discord_delivery_snowflakes_cross_json_without_javascript_truncation() {
        let mut meeting = meeting_with(Vec::new());
        meeting.discord_summary_delivery = Some(DiscordSummaryDelivery::delivered(
            543_321_203_243_483_137,
            643_321_203_243_483_149,
        ));

        let json = serde_json::to_value(&meeting).unwrap();
        assert_eq!(
            json["discordSummaryDelivery"]["channelId"],
            "543321203243483137"
        );
        assert_eq!(
            json["discordSummaryDelivery"]["messageId"],
            "643321203243483149"
        );

        let restored: Meeting = serde_json::from_value(json).unwrap();
        assert_eq!(
            restored.discord_summary_delivery,
            meeting.discord_summary_delivery
        );
    }

    #[test]
    fn utterances_stay_chronological_when_inserted_out_of_order() {
        let m = meeting_with(vec![
            (10, 5_000, 6_000, "tercero"),
            (20, 1_000, 2_000, "primero"),
            (10, 3_000, 4_000, "segundo"),
        ]);
        let order: Vec<_> = m.utterances.iter().map(|u| u.text.as_str()).collect();
        assert_eq!(order, vec!["primero", "segundo", "tercero"]);
    }

    #[test]
    fn upserting_an_utterance_replaces_it_without_moving_it() {
        let mut meeting = meeting_with(vec![
            (10, 1_000, 2_000, "borrador"),
            (20, 3_000, 4_000, "después"),
        ]);
        let id = meeting.utterances[0].id.clone();
        let replaced = meeting.upsert_utterance(Utterance {
            id,
            speaker_id: 10,
            start_ms: 1_000,
            end_ms: 2_000,
            text: "versión precisa".into(),
            confidence: Some(0.95),
        });

        assert!(replaced);
        assert_eq!(meeting.utterances.len(), 2);
        assert_eq!(meeting.utterances[0].text, "versión precisa");
        assert_eq!(meeting.utterances[1].text, "después");
    }

    #[test]
    fn transcript_merges_consecutive_turns_by_same_speaker() {
        let m = meeting_with(vec![
            (10, 0, 1_000, "hola"),
            (10, 1_000, 2_000, "qué tal"),
            (20, 3_000, 4_000, "bien"),
        ]);
        assert_eq!(
            m.transcript_text(),
            "[00:00] Ana: hola qué tal\n[00:03] Luis: bien\n"
        );
    }

    #[test]
    fn timestamps_grow_an_hour_field_only_when_needed() {
        assert_eq!(format_timestamp(0), "00:00");
        assert_eq!(format_timestamp(62_000), "01:02");
        assert_eq!(format_timestamp(3_723_000), "01:02:03");
    }

    #[test]
    fn a_speaker_keeps_the_same_colour_across_meetings() {
        assert_eq!(color_for(1234567890), color_for(1234567890));
    }

    #[test]
    fn a_meeting_without_an_llm_gets_a_participant_title() {
        let meeting = meeting_with(vec![(10, 0, 1_000, "hola"), (20, 2_000, 3_000, "qué tal")]);
        assert_eq!(meeting.fallback_title(), "Ana y Luis");
    }

    #[test]
    fn old_metadata_without_a_display_title_still_deserializes() {
        let meta: MeetingMeta = serde_json::from_value(serde_json::json!({
            "id": "old",
            "guildId": 1,
            "guildName": "Servidor",
            "channelId": 2,
            "channelName": "General",
            "startedAt": "2026-08-06T12:00:00Z",
            "endedAt": null
        }))
        .unwrap();
        assert_eq!(meta.display_title, None);
        assert_eq!(meta.title(), "Servidor · General");
    }
}
