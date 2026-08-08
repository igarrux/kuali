//! Persistent consent audit log. JSON Lines ensures every event reaches disk
//! immediately even if the application exits unexpectedly.

use std::fs::{File, OpenOptions};
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use chrono::{DateTime, Utc};
use parking_lot::Mutex;
use serde::Serialize;
use songbird::events::TrackEvent;
use songbird::tracks::{PlayMode, TrackHandle};
use songbird::{Event, EventContext, EventHandler as VoiceEventHandler};
use tokio::sync::mpsc::UnboundedSender;
use uuid::Uuid;

use kuali_core::VoiceEvent;

pub fn consent_audit_path() -> PathBuf {
    // The UI already opens this directory through View files, while
    // `kuali-store::list` ignores loose files when enumerating meetings.
    kuali_core::paths::meetings_dir().join("consent-audit.jsonl")
}

#[derive(Debug, Clone)]
pub struct AuditSubject {
    pub user_id: u64,
    pub display_name: String,
}

#[derive(Debug, Clone)]
pub struct AnnouncementContext {
    pub guild_id: u64,
    pub channel_id: u64,
    pub subject: AuditSubject,
    pub announcement_id: Option<String>,
}

impl AnnouncementContext {
    pub fn for_join(guild_id: u64, channel_id: u64, subject: AuditSubject) -> Self {
        Self {
            guild_id,
            channel_id,
            subject,
            announcement_id: None,
        }
    }

    pub fn with_announcement(mut self) -> Self {
        self.announcement_id = Some(Uuid::new_v4().to_string());
        self
    }
}

#[derive(Debug, Clone, Copy, Serialize)]
#[serde(rename_all = "camelCase")]
pub enum AuditKind {
    KualiJoined,
    ParticipantJoined,
    AnnouncementQueued,
    AnnouncementStarted,
    AnnouncementCompleted,
    AnnouncementFailed,
    TextNoticePosted,
    TextNoticeFailed,
}

impl AuditKind {
    fn carries_notice(self) -> bool {
        !matches!(self, Self::KualiJoined | Self::ParticipantJoined)
    }
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct AuditRecord<'a> {
    schema_version: u8,
    id: String,
    occurred_at: DateTime<Utc>,
    event: AuditKind,
    // Store snowflakes as text so JavaScript viewers cannot truncate them.
    guild_id: String,
    channel_id: String,
    participant_user_id: String,
    participant_display_name: &'a str,
    announcement_id: Option<&'a str>,
    notice_text: Option<&'static str>,
    detail: Option<&'a str>,
}

pub struct AuditLog {
    path: PathBuf,
    file: Mutex<File>,
}

impl AuditLog {
    pub fn open(path: impl AsRef<Path>) -> io::Result<Self> {
        let path = path.as_ref().to_path_buf();
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }

        let mut options = OpenOptions::new();
        options.create(true).append(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        let file = options.open(&path)?;
        Ok(Self {
            path,
            file: Mutex::new(file),
        })
    }

    pub fn record(
        &self,
        event: AuditKind,
        context: &AnnouncementContext,
        detail: Option<&str>,
    ) -> io::Result<()> {
        let record = AuditRecord {
            schema_version: 1,
            id: Uuid::new_v4().to_string(),
            occurred_at: Utc::now(),
            event,
            guild_id: context.guild_id.to_string(),
            channel_id: context.channel_id.to_string(),
            participant_user_id: context.subject.user_id.to_string(),
            participant_display_name: &context.subject.display_name,
            announcement_id: context.announcement_id.as_deref(),
            notice_text: event
                .carries_notice()
                .then_some(kuali_core::CONSENT_MESSAGE),
            detail,
        };

        let mut line = serde_json::to_vec(&record)
            .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))?;
        line.push(b'\n');

        let mut file = self.file.lock();
        file.write_all(&line)?;
        file.flush()?;
        // This is an audit trail, so do not leave the line only in OS buffers.
        file.sync_data()
    }

    pub fn path(&self) -> &Path {
        &self.path
    }
}

fn record_or_warn(
    audit: &AuditLog,
    tx: &UnboundedSender<VoiceEvent>,
    event: AuditKind,
    context: &AnnouncementContext,
    detail: Option<&str>,
) {
    if let Err(error) = audit.record(event, context, detail) {
        let _ = tx.send(VoiceEvent::Warning(format!(
            "no pude escribir el registro de consentimiento en {}: {error}",
            audit.path().display()
        )));
    }
}

/// Attaches logging to actual playback. `Delayed(20 ms)` starts only after the
/// track begins, even when it previously waited in the queue.
pub fn attach_track_audit(
    track: &TrackHandle,
    audit: Arc<AuditLog>,
    tx: UnboundedSender<VoiceEvent>,
    context: AnnouncementContext,
) -> Result<(), String> {
    track
        .add_event(
            Event::Delayed(Duration::from_millis(20)),
            TrackStarted {
                audit: Arc::clone(&audit),
                tx: tx.clone(),
                context: context.clone(),
            },
        )
        .map_err(|error| format!("no pude observar el comienzo del aviso: {error}"))?;
    track
        .add_event(
            Event::Track(TrackEvent::End),
            TrackFinished { audit, tx, context },
        )
        .map_err(|error| format!("no pude observar el final del aviso: {error}"))?;
    Ok(())
}

struct TrackStarted {
    audit: Arc<AuditLog>,
    tx: UnboundedSender<VoiceEvent>,
    context: AnnouncementContext,
}

#[async_trait::async_trait]
impl VoiceEventHandler for TrackStarted {
    async fn act(&self, _ctx: &EventContext<'_>) -> Option<Event> {
        record_or_warn(
            &self.audit,
            &self.tx,
            AuditKind::AnnouncementStarted,
            &self.context,
            Some("la pista de voz comenzó a reproducirse"),
        );
        None
    }
}

struct TrackFinished {
    audit: Arc<AuditLog>,
    tx: UnboundedSender<VoiceEvent>,
    context: AnnouncementContext,
}

#[async_trait::async_trait]
impl VoiceEventHandler for TrackFinished {
    async fn act(&self, ctx: &EventContext<'_>) -> Option<Event> {
        let mode = match ctx {
            EventContext::Track(tracks) => tracks.first().map(|(state, _)| &state.playing),
            _ => None,
        };
        let (event, detail) = match mode {
            Some(PlayMode::End) => (
                AuditKind::AnnouncementCompleted,
                "la pista de voz terminó de reproducirse".to_string(),
            ),
            Some(PlayMode::Errored(error)) => (
                AuditKind::AnnouncementFailed,
                format!("la reproducción falló: {error}"),
            ),
            Some(PlayMode::Stop) => (
                AuditKind::AnnouncementFailed,
                "la reproducción fue detenida antes de terminar".to_string(),
            ),
            _ => (
                AuditKind::AnnouncementFailed,
                "la pista terminó en un estado inesperado".to_string(),
            ),
        };
        record_or_warn(&self.audit, &self.tx, event, &self.context, Some(&detail));
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn audit_records_are_append_only_json_lines_with_string_ids() {
        let path = std::env::temp_dir().join(format!("kuali-audit-test-{}.jsonl", Uuid::new_v4()));
        let log = AuditLog::open(&path).unwrap();
        let context = AnnouncementContext::for_join(
            1_234_567_890_123_456_789,
            987_654_321,
            AuditSubject {
                user_id: 543_321_203_243_483_137,
                display_name: "Garrux".into(),
            },
        );

        log.record(AuditKind::ParticipantJoined, &context, None)
            .unwrap();
        log.record(
            AuditKind::AnnouncementQueued,
            &context.clone().with_announcement(),
            None,
        )
        .unwrap();
        drop(log);

        let lines = std::fs::read_to_string(&path).unwrap();
        let records = lines
            .lines()
            .map(|line| serde_json::from_str::<serde_json::Value>(line).unwrap())
            .collect::<Vec<_>>();
        assert_eq!(records.len(), 2);
        assert_eq!(records[0]["event"], "participantJoined");
        assert_eq!(records[0]["participantUserId"], "543321203243483137");
        assert_eq!(records[1]["event"], "announcementQueued");
        assert_eq!(records[1]["noticeText"], kuali_core::CONSENT_MESSAGE);

        std::fs::remove_file(path).unwrap();
    }
}
