//! WebSocket ingest for browser-meeting audio.
//!
//! This is the Meet, Teams, and Zoom equivalent of Songbird for Discord. The
//! Kuali extension sends audio plus events binding every channel to a speaker
//! ID, name, and avatar.
//!
//! It listens on **loopback only** and rejects ordinary web origins. Browser
//! connections must come from a Chrome extension, while native clients without
//! an `Origin` header remain available for local tooling.

use std::collections::{HashMap, HashSet};
use std::net::SocketAddr;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use futures_util::{SinkExt, StreamExt};
use kuali_core::{color_for, CallInfo, DiscordUserId, Speaker, VoiceEvent};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::mpsc::UnboundedSender;
use tokio_tungstenite::tungstenite::{http::StatusCode, Message};

use crate::wire::{
    decode_binary, decode_text, AudioFrame, Frame, MeetingEvent, CAPTURE_SAMPLE_RATE,
};

/// Port configured by default in the extension.
pub const DEFAULT_PORT: u16 = 9099;
static NEXT_WEB_SESSION_ID: AtomicU64 = AtomicU64::new(1);

/// Clock used by the engine to close speech turns. Discord supplies packet ticks;
/// browser ingest must generate them or silence would never close a turn.
const TICK_MS: u64 = 20;

/// Default listening address: loopback, never `0.0.0.0`.
pub fn default_addr() -> SocketAddr {
    SocketAddr::from(([127, 0, 0, 1], DEFAULT_PORT))
}

#[derive(Debug, thiserror::Error)]
pub enum IngestError {
    #[error("failed to listen on {addr}: {source}")]
    Bind {
        addr: SocketAddr,
        source: std::io::Error,
    },
}

/// Accepts connections until the event sender is dropped.
///
/// Every connection receives its own session identity, allowing concurrent tabs
/// without mixing meetings.
pub async fn serve(
    addr: SocketAddr,
    events: UnboundedSender<VoiceEvent>,
) -> Result<(), IngestError> {
    let listener = TcpListener::bind(addr)
        .await
        .map_err(|source| IngestError::Bind { addr, source })?;
    tracing::info!("listening for web meetings on ws://{addr}/ingest");
    serve_on(listener, events).await;
    Ok(())
}

/// Serves an already-bound listener. Separation from `serve` lets tests reserve
/// a port instead of guessing which one is free.
pub async fn serve_on(listener: TcpListener, events: UnboundedSender<VoiceEvent>) {
    // Each connection runs in its own task and wraps events with a session ID,
    // preserving independent clocks and segmenters across tabs.
    while let Ok((stream, _)) = listener.accept().await {
        if events.is_closed() {
            break;
        }
        let events = events.clone();
        tokio::spawn(async move {
            match handle_connection(stream, &events).await {
                Ok(()) => tracing::info!("web meeting ended"),
                // Ordinary HTTP traffic on this port is not a Kuali failure;
                // record it at debug level and continue.
                Err(e) => tracing::debug!("Meet connection dropped: {e}"),
            }
        });
    }
}

// The large `Err` reported by Clippy is required by tungstenite's callback type.
#[allow(clippy::result_large_err)]
async fn handle_connection(
    stream: TcpStream,
    events: &UnboundedSender<VoiceEvent>,
) -> Result<(), tokio_tungstenite::tungstenite::Error> {
    // The path carries meeting data: `?platform=…&native_meeting_id=…`.
    let mut request_path = String::new();
    let ws = tokio_tungstenite::accept_hdr_async(stream, |req: &Request, res: Response| {
        if !origin_is_allowed(req) {
            let mut response = ErrorResponse::new(Some(
                "Kuali only accepts its browser extension on this port".into(),
            ));
            *response.status_mut() = StatusCode::FORBIDDEN;
            return Err(response);
        }
        request_path = req.uri().to_string();
        Ok(res)
    })
    .await?;

    let meeting = MeetingParams::from_uri(&request_path);
    let (mut sink, mut source) = ws.split();

    // The extension probes this endpoint before suggesting capture. It creates
    // no session, loads no model, and adds nothing to the library; it only proves
    // that Kuali is listening on this port.
    if request_path.split('?').next() == Some("/health") {
        let health = serde_json::json!({
            "type": "health",
            "service": "kuali",
            "status": "ready",
            "protocol": "capture.v1",
        });
        sink.send(Message::Text(health.to_string().into())).await?;
        sink.send(Message::Close(None)).await?;
        return Ok(());
    }

    let session_id = NEXT_WEB_SESSION_ID.fetch_add(1, Ordering::Relaxed);
    let (session_events, mut session_rx) = tokio::sync::mpsc::unbounded_channel();
    let parent = events.clone();
    tokio::spawn(async move {
        while let Some(event) = session_rx.recv().await {
            if parent
                .send(VoiceEvent::Session {
                    session_id,
                    event: Box::new(event),
                })
                .is_err()
            {
                break;
            }
        }
    });
    let events = &session_events;

    let (reply, decision) = tokio::sync::oneshot::channel();
    if events
        .send(VoiceEvent::ConnectionRequested {
            info: meeting.call_info(),
            reply,
        })
        .is_err()
    {
        return Ok(());
    }
    match decision.await {
        Ok(Ok(())) => {}
        Ok(Err(message)) => {
            let rejected = serde_json::json!({
                "type": "error",
                "code": "connection-rejected",
                "message": message,
            });
            sink.send(Message::Text(rejected.to_string().into()))
                .await?;
            sink.send(Message::Close(None)).await?;
            return Ok(());
        }
        Err(_) => return Ok(()),
    }
    let _ = events.send(VoiceEvent::Connected(meeting.call_info()));

    // The extension waits for a server `ready` confirmation before triggering
    // `BEGIN_CAPTURE` in the tab. Without it the popup remains connecting and no
    // audio frames arrive.
    let ready = serde_json::json!({
        "type": "ready",
        "meeting_id": meeting.native_meeting_id,
    });
    sink.send(Message::Text(ready.to_string().into())).await?;

    let mut ticker = tokio::time::interval(Duration::from_millis(TICK_MS));
    let mut session = Session::new(meeting);
    // This distinguishes an extension that cannot connect from one that connects
    // but captures no audio, which require different diagnostics.
    let mut frames = 0u64;
    let mut samples = 0u64;
    // Track signal peak because very quiet input produces more hallucinations.
    let mut peak = 0.0f32;
    let mut reported = std::time::Instant::now();

    loop {
        tokio::select! {
            _ = ticker.tick() => {
                if events.send(VoiceEvent::Tick).is_err() {
                    break;
                }
                if reported.elapsed() >= Duration::from_secs(5) {
                    reported = std::time::Instant::now();
                    if frames == 0 {
                        tracing::warn!(
                            "the extension is connected but is not sending audio; \
                             verify microphone permission and active speech"
                        );
                    } else {
                        tracing::info!(
                            frames,
                            seconds = samples as f64 / CAPTURE_SAMPLE_RATE as f64,
                            speakers = session.channels.len(),
                            peak,
                            "received web meeting audio"
                        );
                    }
                }
            }
            message = source.next() => {
                let Some(message) = message else { break };
                match message? {
                    Message::Binary(bytes) => {
                        if let Ok(Frame::Audio(frame)) = decode_binary(&bytes) {
                            if frames == 0 {
                                tracing::info!(
                                    speaker = frame.speaker_name.as_deref().unwrap_or("unnamed"),
                                    channel = frame.speaker_index,
                                    "received first web meeting audio frame"
                                );
                            }
                            frames += 1;
                            samples += frame.samples.len() as u64;
                            peak = peak.max(
                                frame.samples.iter().fold(0.0f32, |m, s| m.max(s.abs())),
                            );
                        }
                        on_binary(&bytes, &mut session, events)
                    }
                    Message::Text(json) => on_text(&json, &mut session, events),
                    Message::Ping(payload) => sink.send(Message::Pong(payload)).await?,
                    Message::Close(_) => break,
                    _ => {}
                }
            }
        }
    }

    let _ = events.send(VoiceEvent::Disconnected);
    Ok(())
}

type Request = tokio_tungstenite::tungstenite::handshake::server::Request;
type Response = tokio_tungstenite::tungstenite::handshake::server::Response;
type ErrorResponse = tokio_tungstenite::tungstenite::handshake::server::ErrorResponse;

/// Page-created WebSockets include an origin. Ordinary websites do not need
/// local ingest access, while the extension does. Native clients omit `Origin`
/// and remain available for E2E tests and local diagnostics.
fn origin_is_allowed(request: &Request) -> bool {
    let Some(origin) = request
        .headers()
        .get("origin")
        .and_then(|value| value.to_str().ok())
    else {
        return true;
    };
    let Some(extension_id) = origin.strip_prefix("chrome-extension://") else {
        return false;
    };
    extension_id.len() == 32
        && extension_id
            .bytes()
            .all(|byte| (b'a'..=b'p').contains(&byte))
}

fn on_binary(bytes: &[u8], session: &mut Session, events: &UnboundedSender<VoiceEvent>) {
    let frame = match decode_binary(bytes) {
        Ok(Frame::Audio(frame)) => frame,
        // Combined recordings share the socket but are not transcription audio.
        Ok(_) => return,
        Err(e) => {
            tracing::debug!("unreadable web meeting frame: {e}");
            return;
        }
    };

    announce_speaker(&frame, session, events);
    if frame.samples.is_empty() {
        return;
    }
    let _ = events.send(VoiceEvent::Audio {
        user_id: session.speaker_id(frame.speaker_index),
        pcm: to_i16(&frame.samples),
    });
}

/// Notifies the engine when a participant is first heard or later identified.
/// `upsert_speaker` replaces in place so late names correct earlier placeholders
/// without duplicating people.
fn announce_speaker(
    frame: &AudioFrame,
    session: &mut Session,
    events: &UnboundedSender<VoiceEvent>,
) {
    let name = frame.speaker_name.clone().unwrap_or_default();
    if let Some(binding) = session.channels.get_mut(&frame.speaker_index) {
        if name.is_empty() || binding.speaker.display_name == name {
            return;
        }
        binding.speaker.display_name = name;
        binding.speaker.username = String::new();
        let _ = events.send(VoiceEvent::ParticipantPresent(binding.speaker.clone()));
        return;
    }
    let speaker = session.fallback_speaker(frame.speaker_index, &name);
    session.channels.insert(
        frame.speaker_index,
        ChannelBinding {
            speaker: speaker.clone(),
            audio_kind: AudioKind::Separate,
        },
    );
    let _ = events.send(VoiceEvent::ParticipantPresent(speaker));
}

/// Names a channel from either an audio frame or meeting event.
fn name_speaker(
    index: u32,
    name: &str,
    session: &mut Session,
    events: &UnboundedSender<VoiceEvent>,
) {
    if let Some(binding) = session.channels.get_mut(&index) {
        if binding.speaker.display_name == name {
            return;
        }
        binding.speaker.display_name = name.to_string();
        let _ = events.send(VoiceEvent::ParticipantPresent(binding.speaker.clone()));
    } else {
        let speaker = session.fallback_speaker(index, name);
        session.channels.insert(
            index,
            ChannelBinding {
                speaker: speaker.clone(),
                audio_kind: AudioKind::Separate,
            },
        );
        let _ = events.send(VoiceEvent::ParticipantPresent(speaker));
    }
}

fn on_text(json: &str, session: &mut Session, events: &UnboundedSender<VoiceEvent>) {
    let Ok(Frame::Event(event)) = decode_text(json) else {
        tracing::debug!("unreadable web meeting event");
        return;
    };
    match event.kind.as_str() {
        "participant-upsert" => upsert_participant(&event, session, events),
        // Roster presence does not require speech, allowing the UI to show all
        // participants, including muted ones, from the beginning.
        "roster-state" => upsert_roster(&event, session, events),
        "participant-left" | "speaker-left" => {
            if let Some(id) = speaker_index_of(&event) {
                if let Some(binding) = session.channels.remove(&id) {
                    let _ = events.send(VoiceEvent::ParticipantLeft(binding.speaker.user_id));
                }
            }
        }
        "active-speaker" | "speaker-joined" => {
            let Some(index) = speaker_index_of(&event) else {
                return;
            };
            // This event carries identity before the page can bind it to audio.
            // Without it, unresolved channels would remain Unknown.
            if let Some(name) = event.speaker.as_deref().filter(|n| !n.trim().is_empty()) {
                name_speaker(index, name, session, events);
            }
            if event.kind == "active-speaker" {
                let _ = events.send(VoiceEvent::SpeakingChanged {
                    user_id: session.speaker_id(index),
                    speaking: !ended(&event),
                });
            }
        }
        "active-speakers" => update_active_speakers(&event, session, events),
        "meet-probe" => update_microphone_diagnostic(&event, session),
        "warning" => {
            let message = event
                .detail
                .as_ref()
                .and_then(|detail| detail.get("message"))
                .and_then(|value| value.as_str())
                .or(event.text.as_deref())
                .unwrap_or("La extensión reportó un problema de captura.");
            let _ = events.send(VoiceEvent::Warning(message.to_string()));
        }
        _ => {}
    }
}

fn update_microphone_diagnostic(event: &MeetingEvent, session: &mut Session) {
    let Some(detail) = event.detail.as_ref() else {
        return;
    };
    let Some(gate) = detail.get("microphoneGate") else {
        return;
    };
    let state = (
        gate.get("controlMuted").and_then(|value| value.as_bool()),
        gate.get("protocolMuted").and_then(|value| value.as_bool()),
        gate.get("selfSpeaking").and_then(|value| value.as_bool()),
        gate.get("allowed").and_then(|value| value.as_bool()),
    );
    if session.last_microphone_gate == Some(state) {
        return;
    }
    session.last_microphone_gate = Some(state);

    let local = detail
        .get("captureLanes")
        .and_then(|lanes| lanes.as_array())
        .and_then(|lanes| {
            lanes.iter().find(|lane| {
                lane.get("channel").and_then(|value| value.as_u64()) == Some(MIC_CHANNEL as u64)
            })
        });
    let pcm_frames = local
        .and_then(|lane| lane.get("pcmFrames"))
        .and_then(|value| value.as_u64())
        .unwrap_or(0);
    let blocked_frames = local
        .and_then(|lane| lane.get("blockedFrames"))
        .and_then(|value| value.as_u64())
        .unwrap_or(0);
    tracing::info!(
        control_muted = ?state.0,
        protocol_muted = ?state.1,
        self_speaking = ?state.2,
        allowed = ?state.3,
        pcm_frames,
        blocked_frames,
        "Meet local microphone capture state"
    );
}

fn update_active_speakers(
    event: &MeetingEvent,
    session: &mut Session,
    events: &UnboundedSender<VoiceEvent>,
) {
    let participants = event
        .detail
        .as_ref()
        .and_then(|detail| detail.get("participants"))
        .and_then(|participants| participants.as_array());
    let mut active = HashSet::new();
    for participant in participants.into_iter().flatten() {
        let source_id = detail_string(participant, &["participantId", "participant_id", "id"])
            .filter(|id| !id.trim().is_empty());
        let display_name = detail_string(participant, &["displayName", "display_name", "name"])
            .filter(|name| !name.trim().is_empty());
        let user_id = source_id
            .as_deref()
            .map(|id| session.stable_participant_id(id))
            .or_else(|| {
                display_name
                    .as_deref()
                    .map(|name| session.stable_participant_id(&format!("active:{name}")))
            });
        let Some(user_id) = user_id else {
            continue;
        };
        active.insert(user_id);
    }

    for user_id in active.difference(&session.active_speakers) {
        let _ = events.send(VoiceEvent::SpeakingChanged {
            user_id: *user_id,
            speaking: true,
        });
    }
    for user_id in session.active_speakers.difference(&active) {
        let _ = events.send(VoiceEvent::SpeakingChanged {
            user_id: *user_id,
            speaking: false,
        });
    }
    session.active_speakers = active;
}

fn upsert_roster(
    event: &MeetingEvent,
    session: &mut Session,
    events: &UnboundedSender<VoiceEvent>,
) {
    let Some(participants) = event
        .detail
        .as_ref()
        .and_then(|detail| detail.get("participants"))
        .and_then(|participants| participants.as_array())
    else {
        return;
    };

    for participant in participants {
        let Some(source_id) =
            detail_string(participant, &["participantId", "participant_id", "id"])
                .filter(|id| !id.trim().is_empty())
        else {
            continue;
        };
        let user_id = session.stable_participant_id(&source_id);
        let mut speaker = Speaker::unknown(user_id);
        speaker.source_id = Some(source_id.clone());
        speaker.audio_kind = Some("separate".to_string());
        speaker.display_name = detail_string(participant, &["displayName", "display_name", "name"])
            .filter(|name| !name.trim().is_empty())
            .unwrap_or_else(|| "Participante".to_string());
        speaker.avatar_url = detail_string(participant, &["avatarUrl", "avatar_url", "avatar"])
            .filter(|url| !url.trim().is_empty());
        speaker.color = color_for(user_id).to_string();

        if session.roster.get(&source_id) == Some(&speaker) {
            continue;
        }
        session.roster.insert(source_id, speaker.clone());
        let _ = events.send(VoiceEvent::ParticipantPresent(speaker));
    }
}

/// Registers identity before the first PCM block. The extension repeats this
/// event whenever data is learned or corrected, including names and avatars that
/// appear late in the DOM.
fn upsert_participant(
    event: &MeetingEvent,
    session: &mut Session,
    events: &UnboundedSender<VoiceEvent>,
) {
    let Some(detail) = event.detail.as_ref() else {
        return;
    };
    let Some(index) = detail_u32(detail, &["channel", "index"]) else {
        return;
    };
    let source_id = detail_string(detail, &["participantId", "participant_id", "id"]);
    let display_name = detail_string(detail, &["displayName", "display_name", "name"])
        .or_else(|| event.speaker.clone())
        .filter(|name| !name.trim().is_empty());
    let username = detail_string(detail, &["username", "handle"]).unwrap_or_default();
    let avatar_url = detail_string(detail, &["avatarUrl", "avatar_url", "avatar"])
        .filter(|url| !url.trim().is_empty());
    let audio_kind = match detail_string(detail, &["audioKind", "audio_kind"]).as_deref() {
        Some("mixed") => AudioKind::Mixed,
        _ => AudioKind::Separate,
    };
    // Platforms recycle channels; Meet often keeps a small track pool. Derive
    // internal IDs from participants so reassignment cannot change earlier
    // attribution. Legacy clients without IDs fall back to channel identity.
    let user_id = source_id
        .as_deref()
        .map(|id| session.stable_participant_id(id))
        .unwrap_or_else(|| session.fallback_id(index));
    let mut speaker = Speaker::unknown(user_id);
    speaker.source_id = source_id;
    speaker.audio_kind = Some(
        match audio_kind {
            AudioKind::Separate => "separate",
            AudioKind::Mixed => "mixed",
        }
        .to_string(),
    );
    if let Some(name) = display_name {
        speaker.display_name = name;
    } else if let Some(reserved) = reserved_name(index) {
        speaker.display_name = reserved.to_string();
    }
    speaker.username = username;
    speaker.avatar_url = avatar_url;
    speaker.color = color_for(user_id).to_string();
    speaker.is_bot = detail
        .get("isBot")
        .or_else(|| detail.get("is_bot"))
        .and_then(|value| value.as_bool())
        .unwrap_or(false);

    if let Some(previous) = session.channels.get(&index) {
        if previous.speaker.user_id != speaker.user_id {
            let _ = events.send(VoiceEvent::ParticipantLeft(previous.speaker.user_id));
        }
    }
    session.channels.insert(
        index,
        ChannelBinding {
            speaker: speaker.clone(),
            audio_kind,
        },
    );
    let _ = events.send(VoiceEvent::ParticipantPresent(speaker));
}

fn detail_u32(detail: &serde_json::Value, keys: &[&str]) -> Option<u32> {
    keys.iter()
        .find_map(|key| detail.get(*key)?.as_u64())
        .and_then(|value| u32::try_from(value).ok())
}

fn detail_string(detail: &serde_json::Value, keys: &[&str]) -> Option<String> {
    keys.iter()
        .find_map(|key| detail.get(*key)?.as_str())
        .map(ToOwned::to_owned)
}

fn speaker_index_of(event: &MeetingEvent) -> Option<u32> {
    detail_u32(event.detail.as_ref()?, &["channel", "index"])
}

fn ended(event: &MeetingEvent) -> bool {
    event
        .detail
        .as_ref()
        .and_then(|d| d.get("isEnd"))
        .and_then(|v| v.as_bool())
        .unwrap_or(false)
}

/// Reserved extension channels: 999 is the full-tab mix and 1000 is the local
/// microphone. Meeting participants occupy lower indices.
const MIXED_CHANNEL: u32 = 999;
const MIC_CHANNEL: u32 = 1000;

/// The two special channels have known identities without waiting for the page
/// to bind a name and should never be labeled Unknown.
fn reserved_name(index: u32) -> Option<&'static str> {
    match index {
        MIC_CHANNEL => Some("Tú"),
        MIXED_CHANNEL => Some("Sala"),
        _ => None,
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum AudioKind {
    Separate,
    Mixed,
}

#[derive(Debug, Clone)]
struct ChannelBinding {
    speaker: Speaker,
    /// Retained for diagnostics and to prevent channel 999 from becoming a real
    /// participant. Identity is never inferred from a mixed track.
    #[allow(dead_code)]
    audio_kind: AudioKind,
}

type MicrophoneGateState = (Option<bool>, Option<bool>, Option<bool>, Option<bool>);

struct Session {
    meeting: MeetingParams,
    channels: HashMap<u32, ChannelBinding>,
    /// Participants announced by the page before they speak.
    roster: HashMap<String, Speaker>,
    /// Latest platform activity snapshot, used to avoid UI flicker from repeated
    /// events every 250 ms.
    active_speakers: HashSet<DiscordUserId>,
    /// Latest local-gate state already written to the log.
    last_microphone_gate: Option<MicrophoneGateState>,
}

impl Session {
    fn new(meeting: MeetingParams) -> Self {
        Self {
            meeting,
            channels: HashMap::new(),
            roster: HashMap::new(),
            active_speakers: HashSet::new(),
            last_microphone_gate: None,
        }
    }

    fn speaker_id(&self, channel: u32) -> DiscordUserId {
        self.channels
            .get(&channel)
            .map(|binding| binding.speaker.user_id)
            .unwrap_or_else(|| self.fallback_id(channel))
    }

    fn stable_participant_id(&self, source_id: &str) -> DiscordUserId {
        web_id(&format!(
            "{}\0{}\0participant\0{}",
            self.meeting.platform, self.meeting.native_meeting_id, source_id
        ))
    }

    fn fallback_id(&self, channel: u32) -> DiscordUserId {
        web_id(&format!(
            "{}\0{}\0channel\0{}",
            self.meeting.platform, self.meeting.native_meeting_id, channel
        ))
    }

    fn fallback_speaker(&self, channel: u32, name: &str) -> Speaker {
        let mut speaker = Speaker::unknown(self.fallback_id(channel));
        match (name.trim().is_empty(), reserved_name(channel)) {
            (false, _) => speaker.display_name = name.to_string(),
            (true, Some(reserved)) => speaker.display_name = reserved.to_string(),
            (true, None) => {}
        }
        speaker
    }
}

/// FNV-1a reproduces the same ID across runs without persistent tables. A `WEB`
/// prefix separates it from Discord snowflakes, while `Speaker::source_id`
/// retains the original ID.
fn web_id(value: &str) -> u64 {
    let mut hash = 0xcbf2_9ce4_8422_2325u64;
    for byte in value.as_bytes() {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
    }
    0x5745_0000_0000_0000 | (hash & 0x0000_ffff_ffff_ffff)
}

/// `VoiceEvent::Audio` uses `i16` to match Discord. The wire carries `f32` over
/// the same range, so conversion is exact apart from rounding to the source
/// microphone's 16-bit precision.
fn to_i16(samples: &[f32]) -> Vec<i16> {
    samples
        .iter()
        .map(|s| (s.clamp(-1.0, 1.0) * i16::MAX as f32) as i16)
        .collect()
}

#[derive(Debug, PartialEq)]
struct MeetingParams {
    platform: String,
    native_meeting_id: String,
}

impl MeetingParams {
    fn from_uri(uri: &str) -> Self {
        let query = uri.split_once('?').map(|(_, q)| q).unwrap_or("");
        let mut params: HashMap<&str, String> = HashMap::new();
        for pair in query.split('&').filter(|p| !p.is_empty()) {
            let (key, value) = pair.split_once('=').unwrap_or((pair, ""));
            params.insert(key, percent_decode(value));
        }
        Self {
            platform: params
                .remove("platform")
                .unwrap_or_else(|| "google_meet".to_string()),
            native_meeting_id: params.remove("native_meeting_id").unwrap_or_default(),
        }
    }

    /// Presents a browser meeting in Discord's shape so library, search, and
    /// export logic work unchanged.
    fn call_info(&self) -> CallInfo {
        let name = if self.native_meeting_id.is_empty() {
            "Reunión".to_string()
        } else {
            self.native_meeting_id.clone()
        };
        CallInfo {
            guild_id: web_id(&format!("platform\0{}", self.platform)),
            guild_name: platform_label(&self.platform).to_string(),
            channel_id: web_id(&format!(
                "meeting\0{}\0{}",
                self.platform, self.native_meeting_id
            )),
            channel_name: name,
            // Browser meetings have no Discord chat destination.
            text_channel_id: 0,
        }
    }
}

fn platform_label(platform: &str) -> &str {
    match platform {
        "google_meet" | "meet" => "Google Meet",
        "microsoft_teams" | "ms_teams" | "teams" => "Microsoft Teams",
        "zoom" => "Zoom",
        _ => "Reunión web",
    }
}

fn percent_decode(value: &str) -> String {
    let bytes = value.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        match bytes[i] {
            b'%' if i + 2 < bytes.len() => match u8::from_str_radix(&value[i + 1..i + 3], 16) {
                Ok(byte) => {
                    out.push(byte);
                    i += 3;
                }
                Err(_) => {
                    out.push(bytes[i]);
                    i += 1;
                }
            },
            b'+' => {
                out.push(b' ');
                i += 1;
            }
            byte => {
                out.push(byte);
                i += 1;
            }
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::sync::mpsc;
    use tokio_tungstenite::tungstenite::client::IntoClientRequest;

    fn test_session() -> Session {
        Session::new(MeetingParams {
            platform: "google_meet".into(),
            native_meeting_id: "abc-defg-hij".into(),
        })
    }

    fn test_speaker_id(channel: u32) -> DiscordUserId {
        test_session().fallback_id(channel)
    }

    fn test_participant_id(source_id: &str) -> DiscordUserId {
        test_session().stable_participant_id(source_id)
    }

    async fn recv_scoped(
        rx: &mut mpsc::UnboundedReceiver<VoiceEvent>,
    ) -> (kuali_core::VoiceSessionId, VoiceEvent) {
        match rx.recv().await.expect("ingest task should still be alive") {
            VoiceEvent::Session { session_id, event } => (session_id, *event),
            other => panic!("expected a session event, got {other:?}"),
        }
    }

    #[test]
    fn the_meeting_code_comes_from_the_query_string() {
        let params = MeetingParams::from_uri(
            "/ingest?platform=google_meet&native_meeting_id=abc-defg-hij&api_key=x&language=es",
        );
        assert_eq!(params.platform, "google_meet");
        assert_eq!(params.native_meeting_id, "abc-defg-hij");

        let info = params.call_info();
        assert_eq!(info.guild_name, "Google Meet");
        assert_eq!(info.channel_name, "abc-defg-hij");
    }

    #[test]
    fn a_connection_without_parameters_still_names_the_meeting() {
        let info = MeetingParams::from_uri("/ingest").call_info();
        assert_eq!(info.channel_name, "Reunión");
    }

    #[test]
    fn encoded_characters_in_the_query_are_decoded() {
        let params = MeetingParams::from_uri("/ingest?native_meeting_id=sala%20uno+dos");
        assert_eq!(params.native_meeting_id, "sala uno dos");
    }

    #[test]
    fn only_extension_and_native_origins_can_reach_the_local_ingest() {
        let native = Request::builder().uri("/health").body(()).unwrap();
        assert!(origin_is_allowed(&native));

        let extension = Request::builder()
            .uri("/health")
            .header(
                "Origin",
                "chrome-extension://abcdefghijklmnopabcdefghijklmnop",
            )
            .body(())
            .unwrap();
        assert!(origin_is_allowed(&extension));

        for origin in [
            "https://example.com",
            "http://127.0.0.1:3000",
            "chrome-extension://not-a-valid-extension-id",
        ] {
            let request = Request::builder()
                .uri("/health")
                .header("Origin", origin)
                .body(())
                .unwrap();
            assert!(!origin_is_allowed(&request), "se aceptó {origin}");
        }
    }

    #[test]
    fn meet_speaker_ids_never_collide_with_discord_ones() {
        // Discord snowflakes are shifted timestamps far outside this ID range.
        assert!(test_speaker_id(0) > 543_321_203_243_483_137);
        assert_ne!(test_speaker_id(0), test_speaker_id(1));
    }

    #[test]
    fn float_samples_convert_to_the_integer_range_discord_uses() {
        assert_eq!(to_i16(&[0.0, 1.0, -1.0]), vec![0, i16::MAX, -i16::MAX]);
        // Clamp out-of-range samples rather than wrapping into explosive audio.
        assert_eq!(to_i16(&[9.0, -9.0]), vec![i16::MAX, -i16::MAX]);
    }

    #[test]
    fn a_speaker_is_announced_once_and_again_when_the_name_arrives() {
        let (tx, mut rx) = mpsc::unbounded_channel();
        let mut session = test_session();

        let anonymous = AudioFrame {
            speaker_index: 2,
            timestamp_ms: 0.0,
            samples: vec![],
            speaker_name: None,
        };
        announce_speaker(&anonymous, &mut session, &tx);
        announce_speaker(&anonymous, &mut session, &tx);

        let named = AudioFrame {
            speaker_name: Some("Ana".into()),
            ..anonymous.clone()
        };
        announce_speaker(&named, &mut session, &tx);

        let mut announced = Vec::new();
        while let Ok(VoiceEvent::ParticipantPresent(speaker)) = rx.try_recv() {
            announced.push(speaker.display_name);
        }
        assert_eq!(announced.len(), 2, "solo al aparecer y al ponerle nombre");
        assert_eq!(announced[1], "Ana");
    }

    #[test]
    fn the_reserved_channels_are_named_without_waiting_for_the_page() {
        let (tx, mut rx) = mpsc::unbounded_channel();
        let mut session = test_session();
        name_speaker(MIC_CHANNEL, "", &mut session, &tx);
        match rx.try_recv() {
            Ok(VoiceEvent::ParticipantPresent(speaker)) => assert_eq!(speaker.display_name, "Tú"),
            other => panic!("expected the local microphone, got {other:?}"),
        }
        // An unnamed participant channel remains pending identity resolution.
        name_speaker(3, "", &mut session, &tx);
        match rx.try_recv() {
            Ok(VoiceEvent::ParticipantPresent(speaker)) => {
                assert!(speaker.display_name.starts_with("Desconocido"));
            }
            other => panic!("expected an unnamed participant, got {other:?}"),
        }
    }

    #[test]
    fn a_recording_chunk_produces_no_audio() {
        let (tx, mut rx) = mpsc::unbounded_channel();
        let mut bytes = 0x5245_4331u32.to_le_bytes().to_vec();
        bytes.extend_from_slice(&[0; 12]);
        on_binary(&bytes, &mut test_session(), &tx);
        assert!(
            rx.try_recv().is_err(),
            "la grabación no es audio a transcribir"
        );
    }

    /// Named frame matching Vexa's published golden vector.
    const AUDIO_NAMED: &[u8] = &[
        7, 0, 0, 128, 0, 128, 220, 121, 12, 0, 121, 66, 5, 0, 0, 0, 65, 108, 105, 99, 101, 0, 0, 0,
        0, 0, 236, 190, 0, 0, 230, 190, 0, 0, 224, 190, 0, 0, 218, 190, 0, 0, 212, 190, 0, 0, 206,
        190,
    ];

    /// End-to-end test over a real socket: handshake, parameterized path, binary
    /// frame, and close. This is the extension's first integration layer, where
    /// handshake failures appear as connection failures.
    #[tokio::test]
    async fn a_browser_connection_becomes_a_meeting_with_named_audio() {
        let (tx, mut rx) = mpsc::unbounded_channel();
        let listener = TcpListener::bind(("127.0.0.1", 0)).await.unwrap();
        let port = listener.local_addr().unwrap().port();
        tokio::spawn(serve_on(listener, tx));

        let url = format!(
            "ws://127.0.0.1:{port}/ingest?platform=google_meet&native_meeting_id=abc-defg-hij&api_key=x&language=es"
        );
        let stream = TcpStream::connect(("127.0.0.1", port)).await.unwrap();
        let (mut ws, _) = tokio_tungstenite::client_async(&url, stream).await.unwrap();

        let (session_id, event) = recv_scoped(&mut rx).await;
        match event {
            VoiceEvent::ConnectionRequested { info, reply } => {
                assert_eq!(info.guild_name, "Google Meet");
                assert_eq!(info.channel_name, "abc-defg-hij");
                reply.send(Ok(())).unwrap();
            }
            other => panic!("expected ConnectionRequested, got {other:?}"),
        }
        let (connected_session, event) = recv_scoped(&mut rx).await;
        assert_eq!(connected_session, session_id);
        match event {
            VoiceEvent::Connected(info) => {
                assert_eq!(info.guild_name, "Google Meet");
                assert_eq!(info.channel_name, "abc-defg-hij");
            }
            other => panic!("expected Connected, got {other:?}"),
        }

        // Without `ready`, the extension never begins capture and remains in the
        // connecting state indefinitely.
        match ws
            .next()
            .await
            .expect("server should send a greeting")
            .unwrap()
        {
            Message::Text(json) => {
                let ready: serde_json::Value = serde_json::from_str(&json).unwrap();
                assert_eq!(ready["type"], "ready");
                assert_eq!(ready["meeting_id"], "abc-defg-hij");
            }
            other => panic!("expected the `ready` greeting, got {other:?}"),
        }

        ws.send(Message::Binary(AUDIO_NAMED.into())).await.unwrap();

        // The loop emits a tick every 20 ms; this test only needs other events.
        let mut seen = Vec::new();
        while seen.len() < 2 {
            let (event_session, event) = recv_scoped(&mut rx).await;
            assert_eq!(event_session, session_id);
            match event {
                VoiceEvent::Tick => continue,
                event => seen.push(event),
            }
        }

        match &seen[0] {
            VoiceEvent::ParticipantPresent(speaker) => {
                assert_eq!(speaker.display_name, "Alice");
                assert_eq!(speaker.user_id, test_speaker_id(7));
            }
            other => panic!("expected ParticipantPresent, got {other:?}"),
        }
        match &seen[1] {
            VoiceEvent::Audio { user_id, pcm } => {
                assert_eq!(*user_id, test_speaker_id(7));
                assert_eq!(pcm.len(), 6, "las seis muestras del vector golden");
            }
            other => panic!("expected Audio, got {other:?}"),
        }

        ws.close(None).await.unwrap();
        loop {
            let (event_session, event) = recv_scoped(&mut rx).await;
            assert_eq!(event_session, session_id);
            match event {
                VoiceEvent::Disconnected => break,
                _ => continue,
            }
        }
    }

    #[tokio::test]
    async fn a_health_check_reports_kuali_without_starting_a_meeting() {
        let (tx, mut rx) = mpsc::unbounded_channel();
        let listener = TcpListener::bind(("127.0.0.1", 0)).await.unwrap();
        let port = listener.local_addr().unwrap().port();
        tokio::spawn(serve_on(listener, tx));

        let url = format!("ws://127.0.0.1:{port}/health?client=kuali-extension");
        let stream = TcpStream::connect(("127.0.0.1", port)).await.unwrap();
        let mut request = url.into_client_request().unwrap();
        request.headers_mut().insert(
            "Origin",
            "chrome-extension://abcdefghijklmnopabcdefghijklmnop"
                .parse()
                .unwrap(),
        );
        let (mut ws, _) = tokio_tungstenite::client_async(request, stream)
            .await
            .unwrap();
        let message = ws.next().await.unwrap().unwrap();
        let Message::Text(json) = message else {
            panic!("expected Kuali health response");
        };
        let health: serde_json::Value = serde_json::from_str(&json).unwrap();
        assert_eq!(health["type"], "health");
        assert_eq!(health["service"], "kuali");
        assert_eq!(health["status"], "ready");

        assert!(
            tokio::time::timeout(Duration::from_millis(50), rx.recv())
                .await
                .is_err(),
            "consultar salud no debe emitir ConnectionRequested ni Connected"
        );
        ws.close(None).await.ok();
    }

    #[tokio::test]
    async fn two_browser_meetings_are_captured_as_independent_sessions() {
        let (tx, mut rx) = mpsc::unbounded_channel();
        let listener = TcpListener::bind(("127.0.0.1", 0)).await.unwrap();
        let port = listener.local_addr().unwrap().port();
        tokio::spawn(serve_on(listener, tx));

        let first_url = format!(
            "ws://127.0.0.1:{port}/ingest?platform=google_meet&native_meeting_id=primera-sala"
        );
        let first_stream = TcpStream::connect(("127.0.0.1", port)).await.unwrap();
        let (mut first, _) = tokio_tungstenite::client_async(&first_url, first_stream)
            .await
            .unwrap();
        let (first_session, first_event) = recv_scoped(&mut rx).await;
        match first_event {
            VoiceEvent::ConnectionRequested { info, reply } => {
                assert_eq!(info.channel_name, "primera-sala");
                reply.send(Ok(())).unwrap();
            }
            other => panic!("expected the first admission, got {other:?}"),
        }
        assert!(
            matches!(recv_scoped(&mut rx).await, (id, VoiceEvent::Connected(_)) if id == first_session)
        );
        assert!(matches!(
            first.next().await.unwrap().unwrap(),
            Message::Text(_)
        ));

        let second_url =
            format!("ws://127.0.0.1:{port}/ingest?platform=zoom&native_meeting_id=segunda-sala");
        let second_stream = TcpStream::connect(("127.0.0.1", port)).await.unwrap();
        let (mut second, _) = tokio_tungstenite::client_async(&second_url, second_stream)
            .await
            .unwrap();
        let (second_session, second_event) = loop {
            let (id, event) = recv_scoped(&mut rx).await;
            if matches!(event, VoiceEvent::Tick) {
                continue;
            }
            break (id, event);
        };
        assert_ne!(second_session, first_session);
        match second_event {
            VoiceEvent::ConnectionRequested { info, reply } => {
                assert_eq!(info.guild_name, "Zoom");
                assert_eq!(info.channel_name, "segunda-sala");
                reply.send(Ok(())).unwrap();
            }
            other => panic!("expected the second admission, got {other:?}"),
        }

        loop {
            let (id, event) = recv_scoped(&mut rx).await;
            if id == second_session && matches!(event, VoiceEvent::Connected(_)) {
                break;
            }
        }
        assert!(matches!(
            second.next().await.unwrap().unwrap(),
            Message::Text(_)
        ));

        // Closing one tab must neither disconnect nor silence another.
        first.close(None).await.unwrap();
        loop {
            let (id, event) = recv_scoped(&mut rx).await;
            if id == first_session && matches!(event, VoiceEvent::Disconnected) {
                break;
            }
        }

        second
            .send(Message::Binary(AUDIO_NAMED.into()))
            .await
            .unwrap();
        loop {
            let (id, event) = recv_scoped(&mut rx).await;
            if id == second_session && matches!(&event, VoiceEvent::Audio { .. }) {
                break;
            }
            assert!(
                !(id == second_session && matches!(&event, VoiceEvent::Disconnected)),
                "la segunda reunión se desconectó al cerrar la primera"
            );
        }
        second.close(None).await.unwrap();
    }

    #[test]
    fn the_active_speaker_event_drives_the_live_indicator() {
        let (tx, mut rx) = mpsc::unbounded_channel();
        let mut session = test_session();
        on_text(
            r#"{"kind":"active-speaker","ts":1,"speaker":"Ana","detail":{"index":3,"isEnd":false}}"#,
            &mut session,
            &tx,
        );
        // Identity arriving in the event replaces the Unknown placeholder.
        match rx.try_recv() {
            Ok(VoiceEvent::ParticipantPresent(speaker)) => {
                assert_eq!(speaker.display_name, "Ana");
                assert_eq!(speaker.user_id, test_speaker_id(3));
            }
            other => panic!("expected the event to identify the speaker, got {other:?}"),
        }
        match rx.try_recv() {
            Ok(VoiceEvent::SpeakingChanged { user_id, speaking }) => {
                assert_eq!(user_id, test_speaker_id(3));
                assert!(speaking);
            }
            other => panic!("expected SpeakingChanged, got {other:?}"),
        }
    }

    #[test]
    fn the_meet_active_speaker_snapshot_only_emits_real_transitions() {
        let (tx, mut rx) = mpsc::unbounded_channel();
        let mut session = test_session();
        let active = r#"{"kind":"active-speakers","ts":1,"detail":{"participants":[{"participantId":"ana-device","displayName":"Ana","isSelf":false}]}}"#;
        on_text(active, &mut session, &tx);
        match rx.try_recv() {
            Ok(VoiceEvent::SpeakingChanged { user_id, speaking }) => {
                assert_eq!(user_id, test_participant_id("ana-device"));
                assert!(speaking);
            }
            other => panic!("expected speech to start, got {other:?}"),
        }

        on_text(active, &mut session, &tx);
        assert!(
            rx.try_recv().is_err(),
            "una foto idéntica no debe parpadear"
        );

        on_text(
            r#"{"kind":"active-speakers","ts":2,"detail":{"participants":[]}}"#,
            &mut session,
            &tx,
        );
        match rx.try_recv() {
            Ok(VoiceEvent::SpeakingChanged { user_id, speaking }) => {
                assert_eq!(user_id, test_participant_id("ana-device"));
                assert!(!speaking);
            }
            other => panic!("expected speech to end, got {other:?}"),
        }
    }

    #[test]
    fn participant_metadata_binds_id_name_and_avatar_to_its_audio_channel() {
        let (tx, mut rx) = mpsc::unbounded_channel();
        let mut session = test_session();
        on_text(
            r#"{"kind":"participant-upsert","ts":1,"speaker":"Ana","detail":{"channel":3,"participantId":"teams-user-42","displayName":"Ana María","username":"ana","avatarUrl":"https://example.test/ana.jpg","audioKind":"separate"}}"#,
            &mut session,
            &tx,
        );

        let speaker = match rx.try_recv() {
            Ok(VoiceEvent::ParticipantPresent(speaker)) => speaker,
            other => panic!("expected participant metadata, got {other:?}"),
        };
        assert_eq!(speaker.user_id, test_participant_id("teams-user-42"));
        assert_eq!(speaker.source_id.as_deref(), Some("teams-user-42"));
        assert_eq!(speaker.display_name, "Ana María");
        assert_eq!(speaker.username, "ana");
        assert_eq!(speaker.audio_kind.as_deref(), Some("separate"));
        assert_eq!(
            speaker.avatar_url.as_deref(),
            Some("https://example.test/ana.jpg")
        );
        assert_eq!(session.speaker_id(3), speaker.user_id);
    }

    #[test]
    fn roster_announces_silent_participants_once_with_their_real_identity() {
        let (tx, mut rx) = mpsc::unbounded_channel();
        let mut session = test_session();
        let roster = r#"{"kind":"roster-state","ts":1,"detail":{"participantCount":2,"participants":[{"participantId":"meet-device-1","displayName":"Garrux","avatarUrl":"https://example.test/garrux.jpg","isSelf":true},{"participantId":"meet-device-2","displayName":"Delphys","avatarUrl":"https://example.test/delphys.jpg","isSelf":false}]}}"#;

        on_text(roster, &mut session, &tx);
        on_text(roster, &mut session, &tx);

        let mut speakers = Vec::new();
        while let Ok(VoiceEvent::ParticipantPresent(speaker)) = rx.try_recv() {
            speakers.push(speaker);
        }
        assert_eq!(
            speakers.len(),
            2,
            "el segundo snapshot idéntico no se repite"
        );
        assert_eq!(speakers[0].display_name, "Garrux");
        assert_eq!(speakers[1].display_name, "Delphys");
        assert_eq!(speakers[1].source_id.as_deref(), Some("meet-device-2"));
        assert_eq!(speakers[1].audio_kind.as_deref(), Some("separate"));
        assert_eq!(
            speakers[1].avatar_url.as_deref(),
            Some("https://example.test/delphys.jpg")
        );
        assert_eq!(speakers[1].user_id, test_participant_id("meet-device-2"));
    }

    #[test]
    fn teams_and_zoom_get_their_own_library_source_and_stable_meeting_id() {
        let teams = MeetingParams::from_uri(
            "/ingest?platform=microsoft_teams&native_meeting_id=weekly-room",
        )
        .call_info();
        let zoom = MeetingParams::from_uri("/ingest?platform=zoom&native_meeting_id=123456789")
            .call_info();
        assert_eq!(teams.guild_name, "Microsoft Teams");
        assert_eq!(zoom.guild_name, "Zoom");
        assert_ne!(teams.guild_id, zoom.guild_id);
        assert_ne!(teams.channel_id, zoom.channel_id);
        assert_ne!(teams.channel_id, 0);
    }
}
