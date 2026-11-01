//! Interactive, real-browser web-meeting E2E (Google Meet by default; use
//! `--platform yandex_telemost` for Yandex Telemost).
//!
//! This deliberately uses the production Engine and puts a transparent probe
//! between the browser extension and Kuali's web-meeting receiver. The probe
//! observes the capture.v1 wire without changing it; the Engine still performs
//! the real segmentation, Silero VAD, Whisper transcription and model unload.

use std::collections::{BTreeMap, BTreeSet};
use std::io::Write;
use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use anyhow::{anyhow, Context, Result};
use chrono::Utc;
use futures_util::{SinkExt, StreamExt};
use kuali_core::{EngineStatus, KualiEvent, ModelState, Speaker, Utterance};
use kuali_engine::Engine;
use kuali_meet::ingest::platform_label;
use kuali_meet::wire::{decode_binary, decode_text, Frame, CAPTURE_SAMPLE_RATE};
use parking_lot::Mutex;
use serde::Serialize;
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::mpsc;
use tokio_tungstenite::tungstenite::handshake::server::{Request, Response};
use tokio_tungstenite::tungstenite::Message;

const DEFAULT_TIMEOUT_SECONDS: u64 = 15 * 60;
const DEFAULT_PLATFORM: &str = "google_meet";
const DEFAULT_TERMS: [&str; 6] = [
    "kuali",
    "waitingroom",
    "kafka",
    "redis",
    "visual studio code",
    "postgresql",
];
const MAX_TIMELINE_ENTRIES: usize = 5000;
const MAX_ACTIVE_SECONDS: usize = 20_000;
/// Reserved extension channel carrying the local microphone, named «Tú».
const MIC_CHANNEL: u32 = 1000;
const SELF_SPEAKER_NAME: &str = "Tú";
const SELF_MARKER: &str = "@self";

#[derive(Debug)]
struct Options {
    port: u16,
    timeout_seconds: u64,
    full: bool,
    report_dir: PathBuf,
    platform: String,
    terms: Option<Vec<String>>,
    min_terms: Option<usize>,
    expect_speakers: Vec<ExpectSpeaker>,
    language: Option<String>,
}

/// `--expect-speaker "<substring>=<term>"`: who must have said what.
#[derive(Debug, Clone, PartialEq, Eq)]
struct ExpectSpeaker {
    substring: String,
    term: String,
}

impl ExpectSpeaker {
    fn parse(value: &str) -> Result<Self> {
        let (substring, term) = value
            .split_once('=')
            .context("--expect-speaker necesita «nombre=término»")?;
        let (substring, term) = (substring.trim(), term.trim());
        if substring.is_empty() || term.is_empty() {
            return Err(anyhow!(
                "--expect-speaker necesita nombre y término no vacíos"
            ));
        }
        Ok(Self {
            substring: substring.to_string(),
            term: term.to_string(),
        })
    }
}

fn parse_terms(value: &str) -> Vec<String> {
    value
        .split(',')
        .map(|term| term.trim().to_string())
        .filter(|term| !term.is_empty())
        .collect()
}

fn report_prefix(platform: &str) -> &str {
    if platform == DEFAULT_PLATFORM {
        "meet"
    } else {
        platform
    }
}

impl Options {
    fn parse() -> Result<Self> {
        let mut port = 0;
        let mut timeout_seconds = DEFAULT_TIMEOUT_SECONDS;
        let mut full = true;
        let mut report_dir = workspace_root().join("target/e2e/reports");
        let mut platform = DEFAULT_PLATFORM.to_string();
        let mut terms = None;
        let mut min_terms = None;
        let mut expect_speakers = Vec::new();
        let mut language = None;
        let mut args = std::env::args().skip(1);
        while let Some(arg) = args.next() {
            match arg.as_str() {
                "--port" => {
                    port = args
                        .next()
                        .context("--port necesita un número")?
                        .parse()
                        .context("puerto E2E inválido")?;
                }
                "--timeout-seconds" => {
                    timeout_seconds = args
                        .next()
                        .context("--timeout-seconds necesita un número")?
                        .parse()
                        .context("timeout E2E inválido")?;
                }
                "--solo" => full = false,
                "--full" => full = true,
                "--report-dir" => {
                    report_dir =
                        PathBuf::from(args.next().context("--report-dir necesita una ruta")?);
                }
                "--platform" => {
                    platform = args.next().context("--platform necesita un id")?;
                }
                "--terms" => {
                    let list = parse_terms(&args.next().context("--terms necesita una lista")?);
                    if list.is_empty() {
                        return Err(anyhow!("--terms no puede estar vacío"));
                    }
                    terms = Some(list);
                }
                "--min-terms" => {
                    min_terms = Some(
                        args.next()
                            .context("--min-terms necesita un número")?
                            .parse()
                            .context("--min-terms inválido")?,
                    );
                }
                "--expect-speaker" => {
                    expect_speakers.push(ExpectSpeaker::parse(
                        &args
                            .next()
                            .context("--expect-speaker necesita «nombre=término»")?,
                    )?);
                }
                "--language" => {
                    let code = args.next().context("--language necesita un código")?;
                    if code.trim().is_empty() {
                        return Err(anyhow!("--language no puede estar vacío"));
                    }
                    language = Some(code.trim().to_string());
                }
                "--help" | "-h" => {
                    println!(
                        "Uso: cargo run -p kuali-engine --example meet_live_e2e -- \
                         [--port 19099] [--timeout-seconds 900] [--full|--solo] \
                         [--platform google_meet|yandex_telemost] [--terms a,b,c] \
                         [--min-terms N] [--expect-speaker \"nombre|@self=término\"]... \
                         [--language ru]"
                    );
                    std::process::exit(0);
                }
                other => return Err(anyhow!("argumento desconocido: {other}")),
            }
        }
        Ok(Self {
            port,
            timeout_seconds,
            full,
            report_dir,
            platform,
            terms,
            min_terms,
            expect_speakers,
            language,
        })
    }
}

#[derive(Debug, Clone, Default, Serialize)]
#[serde(rename_all = "camelCase")]
struct ChannelProbe {
    channel: u32,
    connected: bool,
    participant_id: Option<String>,
    display_name: Option<String>,
    avatar_url: Option<String>,
    is_self: bool,
    audio_kind: Option<String>,
    platform: Option<String>,
    frames: u64,
    samples: u64,
    peak: f32,
    first_audio_ms: Option<u64>,
    last_audio_ms: Option<u64>,
    /// Wall-clock epoch seconds with at least one frame of peak >= 0.01.
    active_seconds: BTreeSet<u64>,
}

/// One non-audio meeting event as the probe saw it on the wire.
#[derive(Debug, Clone, Default, Serialize, PartialEq)]
#[serde(rename_all = "camelCase")]
struct TimelineEntry {
    ts_ms: u64,
    kind: String,
    channel: Option<u32>,
    participant_id: Option<String>,
    display_name: Option<String>,
    is_self: Option<bool>,
    audio_kind: Option<String>,
    in_call: Option<bool>,
    participant_count: Option<u64>,
    participants: Option<Vec<TimelineParticipant>>,
}

#[derive(Debug, Clone, Default, Serialize, PartialEq)]
#[serde(rename_all = "camelCase")]
struct TimelineParticipant {
    participant_id: Option<String>,
    display_name: Option<String>,
    is_self: Option<bool>,
}

fn timeline_entry(kind: &str, detail: Option<&serde_json::Value>, ts_ms: u64) -> TimelineEntry {
    let mut entry = TimelineEntry {
        ts_ms,
        kind: kind.to_string(),
        ..TimelineEntry::default()
    };
    let Some(detail) = detail else { return entry };
    entry.channel = detail
        .get("channel")
        .or_else(|| detail.get("index"))
        .and_then(|value| value.as_u64())
        .and_then(|value| u32::try_from(value).ok());
    entry.participant_id = string_field(detail, &["participantId", "id"]);
    entry.display_name = string_field(detail, &["displayName", "name"]);
    entry.is_self = detail.get("isSelf").and_then(|value| value.as_bool());
    entry.audio_kind = string_field(detail, &["audioKind"]);
    entry.in_call = detail.get("inCall").and_then(|value| value.as_bool());
    entry.participant_count = detail
        .get("participantCount")
        .and_then(|value| value.as_u64());
    entry.participants = detail
        .get("participants")
        .and_then(|value| value.as_array())
        .map(|list| {
            list.iter()
                .map(|participant| TimelineParticipant {
                    participant_id: string_field(participant, &["participantId", "id"]),
                    display_name: string_field(participant, &["displayName", "name"]),
                    is_self: participant.get("isSelf").and_then(|value| value.as_bool()),
                })
                .collect()
        });
    entry
}

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|elapsed| elapsed.as_millis() as u64)
        .unwrap_or(0)
}

#[derive(Debug, Clone, Default, Serialize)]
#[serde(rename_all = "camelCase")]
struct WireProbe {
    request_path: Option<String>,
    meeting_events: u64,
    audio_frames: u64,
    audio_samples: u64,
    peak: f32,
    clean_close: bool,
    participant_count: Option<u64>,
    roster: Vec<serde_json::Value>,
    meet_probe: Option<serde_json::Value>,
    channels: BTreeMap<u32, ChannelProbe>,
    timeline: Vec<TimelineEntry>,
    connections: Vec<IngestConnection>,
}

/// One `/ingest` session, timed so runners can measure automatic stops.
#[derive(Debug, Clone, Default, Serialize)]
#[serde(rename_all = "camelCase")]
struct IngestConnection {
    index: usize,
    path: String,
    opened_ms: u64,
    close_code: Option<u16>,
    close_frame_ms: Option<u64>,
    closed_ms: Option<u64>,
}

fn is_ingest_path(path: &str) -> bool {
    path.split('?').next() == Some("/ingest")
}

/// Prints the session as a `KUALI_E2E_INGEST` line for runners that follow
/// stdout while the browser is still in the call.
fn announce_ingest(event: &str, connection: &IngestConnection) {
    let line = serde_json::json!({ "event": event, "connection": connection });
    println!("KUALI_E2E_INGEST {line}");
    let _ = std::io::stdout().flush();
}

impl WireProbe {
    fn open_ingest(&mut self, path: &str, now_ms: u64) -> IngestConnection {
        self.request_path.get_or_insert_with(|| path.to_string());
        let connection = IngestConnection {
            index: self.connections.len(),
            path: path.to_string(),
            opened_ms: now_ms,
            ..IngestConnection::default()
        };
        self.connections.push(connection.clone());
        connection
    }

    fn observe(&mut self, message: &Message) {
        self.observe_at(message, now_ms());
    }

    fn observe_at(&mut self, message: &Message, now_ms: u64) {
        match message {
            Message::Binary(bytes) => {
                let Ok(Frame::Audio(frame)) = decode_binary(bytes) else {
                    return;
                };
                let peak = frame
                    .samples
                    .iter()
                    .fold(0.0_f32, |current, sample| current.max(sample.abs()));
                self.audio_frames += 1;
                self.audio_samples += frame.samples.len() as u64;
                self.peak = self.peak.max(peak);
                let channel =
                    self.channels
                        .entry(frame.speaker_index)
                        .or_insert_with(|| ChannelProbe {
                            channel: frame.speaker_index,
                            ..ChannelProbe::default()
                        });
                channel.frames += 1;
                channel.samples += frame.samples.len() as u64;
                channel.peak = channel.peak.max(peak);
                channel.first_audio_ms.get_or_insert(now_ms);
                channel.last_audio_ms = Some(now_ms);
                if peak >= 0.01 && channel.active_seconds.len() < MAX_ACTIVE_SECONDS {
                    channel.active_seconds.insert(now_ms / 1000);
                }
                if channel.display_name.is_none() {
                    channel.display_name = frame.speaker_name.clone();
                }
            }
            Message::Text(text) => {
                let Ok(Frame::Event(event)) = decode_text(text) else {
                    return;
                };
                self.meeting_events += 1;
                if event.kind != "keepalive" && self.timeline.len() < MAX_TIMELINE_ENTRIES {
                    self.timeline
                        .push(timeline_entry(&event.kind, event.detail.as_ref(), now_ms));
                }
                if event.kind == "roster-state" {
                    let Some(detail) = event.detail else { return };
                    self.participant_count = detail
                        .get("participantCount")
                        .and_then(|value| value.as_u64());
                    self.roster = detail
                        .get("participants")
                        .and_then(|value| value.as_array())
                        .cloned()
                        .unwrap_or_default();
                    return;
                }
                if event.kind == "meet-probe" {
                    self.meet_probe = event.detail;
                    return;
                }
                if event.kind != "participant-upsert" && event.kind != "track-connected" {
                    return;
                }
                let Some(detail) = event.detail else { return };
                let Some(channel) = detail
                    .get("channel")
                    .or_else(|| detail.get("index"))
                    .and_then(|value| value.as_u64())
                    .and_then(|value| u32::try_from(value).ok())
                else {
                    return;
                };
                let probe = self
                    .channels
                    .entry(channel)
                    .or_insert_with(|| ChannelProbe {
                        channel,
                        ..ChannelProbe::default()
                    });
                if event.kind == "track-connected" {
                    probe.connected = true;
                    probe.audio_kind =
                        string_field(&detail, &["audioKind"]).or_else(|| probe.audio_kind.clone());
                    probe.platform =
                        string_field(&detail, &["platform"]).or_else(|| probe.platform.clone());
                    return;
                }
                probe.participant_id = string_field(&detail, &["participantId", "id"])
                    .or_else(|| probe.participant_id.clone());
                probe.display_name = string_field(&detail, &["displayName", "name"])
                    .or_else(|| probe.display_name.clone());
                probe.avatar_url = string_field(&detail, &["avatarUrl", "avatar"])
                    .or_else(|| probe.avatar_url.clone());
                probe.is_self = detail
                    .get("isSelf")
                    .and_then(|value| value.as_bool())
                    .unwrap_or(probe.is_self);
                probe.audio_kind =
                    string_field(&detail, &["audioKind"]).or_else(|| probe.audio_kind.clone());
                probe.platform =
                    string_field(&detail, &["platform"]).or_else(|| probe.platform.clone());
            }
            Message::Close(frame) => {
                self.clean_close = frame
                    .as_ref()
                    .is_none_or(|frame| u16::from(frame.code) == 1000);
            }
            _ => {}
        }
    }
}

fn string_field(value: &serde_json::Value, keys: &[&str]) -> Option<String> {
    keys.iter()
        .find_map(|key| value.get(*key)?.as_str())
        .map(str::to_owned)
        .filter(|value| !value.trim().is_empty())
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct Assertion {
    name: String,
    passed: bool,
    detail: String,
}

/// Utterance plus its resolved speaker (start/end: ms since the meeting began).
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct ReportUtterance {
    #[serde(flatten)]
    utterance: Utterance,
    speaker_name: Option<String>,
    speaker_is_self: bool,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct E2eReport {
    schema_version: u8,
    outcome: String,
    mode: String,
    started_at: String,
    finished_at: String,
    model: String,
    meeting_id: Option<String>,
    meeting_title: Option<String>,
    statuses: Vec<EngineStatus>,
    model_states: Vec<ModelState>,
    speakers: Vec<Speaker>,
    preview_count: usize,
    utterances: Vec<ReportUtterance>,
    warnings: Vec<String>,
    wire: WireProbe,
    assertions: Vec<Assertion>,
    test_meeting_removed: bool,
}

#[tokio::main]
async fn main() -> Result<()> {
    let options = Options::parse()?;
    let started_at = Utc::now();
    let external_listener = TcpListener::bind(SocketAddr::new(
        IpAddr::V4(Ipv4Addr::LOCALHOST),
        options.port,
    ))
    .await
    .with_context(|| format!("no se pudo reservar el puerto E2E {}", options.port))?;
    let external_port = external_listener.local_addr()?.port();
    let internal_port = unused_loopback_port()?;

    let mut config = kuali_core::paths::load_config().context("no pude leer config.toml")?;
    let model = config.whisper.model;
    let models_dir = config.whisper.resolved_models_directory();
    let model_path = kuali_stt::model_path(&models_dir, model);
    if !kuali_stt::is_downloaded(&models_dir, model) {
        return Err(anyhow!(
            "el modelo seleccionado no está descargado: {}",
            model_path.display()
        ));
    }
    if !kuali_stt::is_vad_downloaded(&models_dir) {
        return Err(anyhow!(
            "Silero VAD no está descargado en {}",
            models_dir.display()
        ));
    }

    // The E2E must not call a summary provider, post to Discord or notify real
    // integrations. It only reads the user's selected Whisper configuration.
    config.meet.enabled = true;
    config.meet.port = internal_port;
    config.llm.summarize_on_leave = false;
    config.discord.post_summary_to_channel = false;
    config.integrations.webhooks.clear();
    if let Some(language) = &options.language {
        config.whisper.language = language.clone();
    }

    let (engine, mut events) = Engine::new(config);
    engine.start_web_ingest().await?;

    let wire = Arc::new(Mutex::new(WireProbe::default()));
    let (proxy_errors_tx, mut proxy_errors_rx) = mpsc::unbounded_channel();
    let proxy_wire = Arc::clone(&wire);
    let proxy = tokio::spawn(serve_extension(
        external_listener,
        internal_port,
        proxy_wire,
        proxy_errors_tx,
    ));

    let ready = serde_json::json!({
        "port": external_port,
        "mode": if options.full { "full" } else { "solo" },
        "platform": options.platform,
        "model": model.file_name(),
    });
    println!("KUALI_E2E_READY {ready}");
    if options.terms.is_none() {
        println!("Frase local:  Prueba local de Kuali, código mango cuatro ocho dos siete. WaitingRoom, Kafka y Redis.");
        if options.full {
            println!("Frase remota: Prueba remota de Kuali, código nube nueve tres seis uno. Visual Studio Code y PostgreSQL.");
        }
    }
    println!("Cuando termines, pulsa “Detener captura” en la extensión.");
    std::io::stdout().flush()?;

    let mut meeting_id = None;
    let mut meeting_title = None;
    let mut statuses = Vec::new();
    let mut model_states = Vec::new();
    let mut speakers = BTreeMap::<u64, Speaker>::new();
    let mut previews = 0usize;
    let mut utterances = Vec::new();
    let mut warnings = Vec::new();
    let mut ended = false;
    let mut interrupted = false;
    let deadline = tokio::time::sleep(Duration::from_secs(options.timeout_seconds));
    tokio::pin!(deadline);

    while !ended {
        tokio::select! {
            _ = &mut deadline => {
                warnings.push(format!("La prueba excedió {} segundos.", options.timeout_seconds));
                break;
            }
            result = tokio::signal::ctrl_c() => {
                result?;
                interrupted = true;
                warnings.push("La prueba fue interrumpida por el usuario.".to_string());
                break;
            }
            Some(error) = proxy_errors_rx.recv() => {
                eprintln!("ERROR proxy: {error}");
                warnings.push(format!("Proxy E2E: {error}"));
                break;
            }
            event = events.recv() => {
                let Some(event) = event else { break };
                match event {
                    KualiEvent::StatusChanged { status } => {
                        println!("[motor] {status:?}");
                        statuses.push(status);
                    }
                    KualiEvent::ModelStateChanged { state } => {
                        println!("[modelo] {state:?}");
                        model_states.push(state);
                    }
                    KualiEvent::MeetingStarted { meeting } => {
                        println!("[reunión] {} · {}", meeting.guild_name, meeting.channel_name);
                        meeting_title = Some(meeting.title());
                        meeting_id = Some(meeting.id);
                    }
                    KualiEvent::SpeakerJoined { speaker, .. } => {
                        println!(
                            "[participante] {} · id={} · {:?} · foto={}",
                            speaker.display_name,
                            speaker.source_id.as_deref().unwrap_or("sin id"),
                            speaker.audio_kind,
                            if speaker.avatar_url.is_some() { "sí" } else { "no" },
                        );
                        speakers.insert(speaker.user_id, speaker);
                    }
                    KualiEvent::UtterancePreview { utterance, .. } => {
                        previews += 1;
                        let name = speakers
                            .get(&utterance.speaker_id)
                            .map(|speaker| speaker.display_name.as_str())
                            .unwrap_or("Participante");
                        println!("[en vivo · {name}] {}", utterance.text);
                    }
                    KualiEvent::UtteranceAdded { utterance, .. } => {
                        let name = speakers
                            .get(&utterance.speaker_id)
                            .map(|speaker| speaker.display_name.as_str())
                            .unwrap_or("Participante");
                        println!("[final · {name}] {}", utterance.text);
                        utterances.push(utterance);
                    }
                    KualiEvent::MeetingEnded { .. } => {
                        println!("[reunión] captura finalizada y Whisper descargado");
                        ended = true;
                    }
                    KualiEvent::Error { source, message } => {
                        eprintln!("[aviso · {source}] {message}");
                        warnings.push(format!("{source}: {message}"));
                    }
                    _ => {}
                }
            }
        }
    }

    if engine.current_meeting().is_some() {
        let _ = engine.leave_call().await;
    }
    engine.stop_web_ingest().await;
    engine.disconnect().await;

    proxy.abort();

    let test_meeting_removed = match meeting_id.as_deref() {
        Some(id) => engine.delete_meeting(id).is_ok(),
        None => true,
    };
    let wire = wire.lock().clone();
    let speaker_values = speakers.into_values().collect::<Vec<_>>();
    let assertions = assess(AssessmentInput {
        full: options.full,
        platform: &options.platform,
        terms: options.terms.as_deref(),
        min_terms: options.min_terms,
        expect_speakers: &options.expect_speakers,
        ended,
        previews,
        utterances: &utterances,
        speakers: &speaker_values,
        statuses: &statuses,
        model_states: &model_states,
        wire: &wire,
        removed: test_meeting_removed,
    });
    let report_utterances = utterances
        .into_iter()
        .map(|utterance| {
            let speaker = speaker_values
                .iter()
                .find(|speaker| speaker.user_id == utterance.speaker_id);
            ReportUtterance {
                speaker_name: speaker.map(|speaker| speaker.display_name.clone()),
                speaker_is_self: speaker.is_some_and(|speaker| speaker_is_self(speaker, &wire)),
                utterance,
            }
        })
        .collect::<Vec<_>>();
    let passed = assertions.iter().all(|assertion| assertion.passed) && !interrupted;
    let exercised = wire.request_path.is_some();
    let outcome = if passed {
        "passed"
    } else if !exercised {
        "not-run"
    } else {
        "failed"
    };
    let report = E2eReport {
        schema_version: 1,
        outcome: outcome.to_string(),
        mode: if options.full { "full" } else { "solo" }.to_string(),
        started_at: started_at.to_rfc3339(),
        finished_at: Utc::now().to_rfc3339(),
        model: model.file_name().to_string(),
        meeting_id,
        meeting_title,
        statuses,
        model_states,
        speakers: speaker_values,
        preview_count: previews,
        utterances: report_utterances,
        warnings,
        wire,
        assertions,
        test_meeting_removed,
    };

    std::fs::create_dir_all(&options.report_dir)?;
    let stamp = started_at.format("%Y%m%dT%H%M%SZ");
    let report_path = options
        .report_dir
        .join(format!("{}-{stamp}.json", report_prefix(&options.platform)));
    std::fs::write(&report_path, serde_json::to_vec_pretty(&report)?)?;
    let result_title = match outcome {
        "passed" => "E2E APROBADO",
        "not-run" => "E2E NO EJECUTADO — la extensión nunca inició la captura",
        _ => "E2E FALLÓ",
    };
    println!("\n{result_title}");
    for assertion in &report.assertions {
        println!(
            "{} {} — {}",
            if assertion.passed { "✓" } else { "✗" },
            assertion.name,
            assertion.detail
        );
    }
    println!("KUALI_E2E_REPORT {}", report_path.display());
    std::io::stdout().flush()?;

    if !passed {
        std::process::exit(if exercised { 1 } else { 2 });
    }
    Ok(())
}

/// Accepts every extension connection. `/health` checks pass straight through
/// to the engine; `/ingest` sessions are also observed and timed.
async fn serve_extension(
    listener: TcpListener,
    upstream_port: u16,
    wire: Arc<Mutex<WireProbe>>,
    errors: mpsc::UnboundedSender<String>,
) {
    loop {
        let stream = match listener.accept().await {
            Ok((stream, _)) => stream,
            Err(error) => {
                let _ = errors.send(error.to_string());
                return;
            }
        };
        tokio::spawn(proxy_connection(
            stream,
            upstream_port,
            Arc::clone(&wire),
            errors.clone(),
        ));
    }
}

// Tungstenite's handshake callback owns a deliberately large HTTP error type.
#[allow(clippy::result_large_err)]
async fn proxy_connection(
    stream: TcpStream,
    upstream_port: u16,
    wire: Arc<Mutex<WireProbe>>,
    errors: mpsc::UnboundedSender<String>,
) {
    let request_path = Arc::new(Mutex::new(String::new()));
    let captured_path = Arc::clone(&request_path);
    let client = match tokio_tungstenite::accept_hdr_async(
        stream,
        move |request: &Request, response: Response| {
            *captured_path.lock() = request.uri().to_string();
            Ok(response)
        },
    )
    .await
    {
        Ok(client) => client,
        Err(error) => {
            eprintln!("[sonda] conexión rechazada: {error}");
            return;
        }
    };
    let request_path = request_path.lock().clone();
    let session = is_ingest_path(&request_path).then(|| {
        let connection = wire.lock().open_ingest(&request_path, now_ms());
        announce_ingest("open", &connection);
        connection.index
    });

    let piped: Result<()> = async {
        let upstream_stream = connect_with_retry(upstream_port).await?;
        let upstream_url = format!("ws://127.0.0.1:{upstream_port}{request_path}");
        let (upstream, _) = tokio_tungstenite::client_async(upstream_url, upstream_stream).await?;
        let (mut client_sink, mut client_source) = client.split();
        let (mut upstream_sink, mut upstream_source) = upstream.split();

        loop {
            tokio::select! {
                message = client_source.next() => {
                    let Some(message) = message else { break };
                    let message = message?;
                    let closing = matches!(message, Message::Close(_));
                    if let Some(index) = session {
                        let mut wire = wire.lock();
                        wire.observe(&message);
                        if let Message::Close(frame) = &message {
                            let connection = &mut wire.connections[index];
                            connection.close_code =
                                frame.as_ref().map(|frame| u16::from(frame.code));
                            connection.close_frame_ms = Some(now_ms());
                            announce_ingest("close-frame", connection);
                        }
                    }
                    upstream_sink.send(message).await?;
                    if closing { break; }
                }
                message = upstream_source.next() => {
                    let Some(message) = message else { break };
                    let message = message?;
                    let closing = matches!(message, Message::Close(_));
                    client_sink.send(message).await?;
                    if closing { break; }
                }
            }
        }
        Ok(())
    }
    .await;

    if let Some(index) = session {
        let mut wire = wire.lock();
        let connection = &mut wire.connections[index];
        connection.closed_ms = Some(now_ms());
        announce_ingest("closed", connection);
    }
    match piped {
        Ok(()) => {}
        // Only a broken capture session invalidates the run.
        Err(error) if session.is_some() => {
            let _ = errors.send(error.to_string());
        }
        Err(error) => eprintln!("[sonda] {request_path}: {error}"),
    }
}

async fn connect_with_retry(port: u16) -> Result<TcpStream> {
    let addr = SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), port);
    let deadline = tokio::time::Instant::now() + Duration::from_secs(8);
    loop {
        match TcpStream::connect(addr).await {
            Ok(stream) => return Ok(stream),
            Err(error) if tokio::time::Instant::now() < deadline => {
                let _ = error;
                tokio::time::sleep(Duration::from_millis(50)).await;
            }
            Err(error) => return Err(error.into()),
        }
    }
}

struct AssessmentInput<'a> {
    full: bool,
    platform: &'a str,
    terms: Option<&'a [String]>,
    min_terms: Option<usize>,
    expect_speakers: &'a [ExpectSpeaker],
    ended: bool,
    previews: usize,
    utterances: &'a [Utterance],
    speakers: &'a [Speaker],
    statuses: &'a [EngineStatus],
    model_states: &'a [ModelState],
    wire: &'a WireProbe,
    removed: bool,
}

fn assess(input: AssessmentInput<'_>) -> Vec<Assertion> {
    let AssessmentInput {
        full,
        platform,
        terms: terms_override,
        min_terms,
        expect_speakers,
        ended,
        previews,
        utterances,
        speakers,
        statuses,
        model_states,
        wire,
        removed,
    } = input;
    let audio_channels = wire
        .channels
        .values()
        .filter(|channel| channel.frames > 0)
        .collect::<Vec<_>>();
    let self_audio = audio_channels
        .iter()
        .filter(|channel| channel.is_self)
        .count();
    let remote_audio = audio_channels
        .iter()
        .filter(|channel| !channel.is_self)
        .count();
    let separate_audio = audio_channels
        .iter()
        .filter(|channel| channel.audio_kind.as_deref() == Some("separate"))
        .count();
    let attributed_audio = audio_channels
        .iter()
        .filter(|channel| {
            channel.participant_id.is_some()
                && channel
                    .display_name
                    .as_deref()
                    .is_some_and(|name| !name.contains("sin identificar"))
        })
        .count();
    let avatars = wire
        .channels
        .values()
        .filter(|channel| channel.avatar_url.is_some())
        .count();
    let transcript_speakers = utterances
        .iter()
        .map(|utterance| utterance.speaker_id)
        .collect::<BTreeSet<_>>()
        .len();
    let transcript = normalize(
        &utterances
            .iter()
            .map(|utterance| utterance.text.as_str())
            .collect::<Vec<_>>()
            .join(" "),
    );
    let (matched_terms, min_required) = control_terms(&transcript, terms_override, min_terms, full);
    let total_seconds = wire.audio_samples as f64 / CAPTURE_SAMPLE_RATE as f64;

    let mut assertions = vec![
        assertion(
            &format!("Handshake de {}", platform_label(platform)),
            wire.request_path.as_deref().is_some_and(|path| {
                path.contains(&format!("platform={platform}"))
                    && path.contains("protocol=capture.v1%2Bparticipants")
            }),
            wire.request_path
                .as_deref()
                .unwrap_or("no hubo conexión")
                .to_string(),
        ),
        assertion(
            "PCM 16 kHz con señal",
            wire.audio_frames > 0 && total_seconds >= 3.0 && wire.peak >= 0.001,
            format!(
                "{} tramas, {:.1} s acumulados, pico {:.4}",
                wire.audio_frames, total_seconds, wire.peak
            ),
        ),
        assertion(
            "Canal local identificado",
            self_audio >= 1,
            format!("{self_audio} canal(es) locales con audio"),
        ),
        assertion(
            "Canales remotos separados",
            !full || (remote_audio >= 1 && separate_audio >= 2),
            if full {
                format!("{remote_audio} remoto(s), {separate_audio} separado(s)")
            } else {
                "omitido en modo solo".to_string()
            },
        ),
        assertion(
            "ID y nombre por canal",
            attributed_audio >= if full { 2 } else { 1 },
            format!(
                "{attributed_audio}/{} canales de audio atribuidos",
                audio_channels.len()
            ),
        ),
        assertion(
            "Foto de participante",
            !full || avatars >= 1,
            if full {
                format!("{avatars} avatar(es) recibido(s)")
            } else {
                "omitido en modo solo".to_string()
            },
        ),
        assertion(
            "Whisper activo y liberado",
            model_states.contains(&ModelState::Active) && model_states.contains(&ModelState::Ready),
            format!("estados: {model_states:?}"),
        ),
        assertion(
            "Transcripción mostrada en vivo",
            previews > 0,
            format!("{previews} borrador(es) emitido(s)"),
        ),
        assertion(
            "Transcripción final atribuida",
            !utterances.is_empty() && (!full || transcript_speakers >= 2),
            format!(
                "{} turno(s), {transcript_speakers} hablante(s)",
                utterances.len()
            ),
        ),
        assertion(
            "Frase de control reconocida",
            matched_terms.len() >= min_required,
            format!(
                "términos reconocidos ({}/{min_required} mín.): {}",
                matched_terms.len(),
                matched_terms.join(", ")
            ),
        ),
        assertion(
            "Cierre limpio",
            ended && wire.clean_close && statuses.contains(&EngineStatus::Finalizing),
            format!("meeting-ended={ended}, websocket-1000={}", wire.clean_close),
        ),
        assertion(
            "Sin residuos en la biblioteca",
            removed,
            if removed {
                "la reunión E2E fue eliminada".to_string()
            } else {
                "no se pudo eliminar la reunión E2E".to_string()
            },
        ),
        assertion(
            "Participantes visibles para el motor",
            speakers.len() >= if full { 2 } else { 1 },
            format!("{} participante(s)", speakers.len()),
        ),
    ];
    for expectation in expect_speakers {
        assertions.push(check_expect_speaker(
            expectation,
            utterances,
            speakers,
            wire,
        ));
    }
    assertions
}

/// Terms found in the (already normalized) transcript and the minimum needed.
fn control_terms(
    transcript: &str,
    terms_override: Option<&[String]>,
    min_terms: Option<usize>,
    full: bool,
) -> (Vec<String>, usize) {
    let terms: Vec<String> = match terms_override {
        Some(list) => list.iter().map(|term| normalize(term)).collect(),
        None => DEFAULT_TERMS.iter().map(|term| term.to_string()).collect(),
    };
    let matched = terms
        .into_iter()
        .filter(|term| transcript.contains(term.as_str()))
        .collect::<Vec<_>>();
    (matched, min_terms.unwrap_or(if full { 4 } else { 2 }))
}

/// Whether the speaker is the person running Kuali. The engine's `is_self`
/// is not enough: the reserved microphone channel (1000, «Tú») gets a
/// fallback speaker with `is_self=false`, so also accept the wire's
/// `isSelf` participant ids and the reserved name.
fn speaker_is_self(speaker: &Speaker, wire: &WireProbe) -> bool {
    speaker.is_self
        || speaker.display_name == SELF_SPEAKER_NAME
        || speaker.source_id.as_deref().is_some_and(|source_id| {
            wire.channels.values().any(|channel| {
                (channel.is_self || channel.channel == MIC_CHANNEL)
                    && channel.participant_id.as_deref() == Some(source_id)
            })
        })
}

fn check_expect_speaker(
    expectation: &ExpectSpeaker,
    utterances: &[Utterance],
    speakers: &[Speaker],
    wire: &WireProbe,
) -> Assertion {
    let term = normalize(&expectation.term);
    let wants_self = expectation.substring.eq_ignore_ascii_case(SELF_MARKER);
    let needle = expectation.substring.to_lowercase();
    let mut said_by = Vec::new();
    let mut passed = false;
    for utterance in utterances
        .iter()
        .filter(|utterance| normalize(&utterance.text).contains(&term))
    {
        let speaker = speakers
            .iter()
            .find(|speaker| speaker.user_id == utterance.speaker_id);
        let name = speaker.map_or("Participante", |speaker| speaker.display_name.as_str());
        let matches = speaker.is_some_and(|speaker| {
            if wants_self {
                speaker_is_self(speaker, wire)
            } else {
                speaker.display_name.to_lowercase().contains(&needle)
            }
        });
        passed |= matches;
        said_by.push(name.to_string());
    }
    let detail = if said_by.is_empty() {
        "nadie dijo ese término".to_string()
    } else {
        format!("lo dijo(eron): {}", said_by.join(", "))
    };
    Assertion {
        name: format!(
            "Atribución: {} dijo «{}»",
            expectation.substring, expectation.term
        ),
        passed,
        detail,
    }
}

fn assertion(name: &str, passed: bool, detail: String) -> Assertion {
    Assertion {
        name: name.to_string(),
        passed,
        detail,
    }
}

fn normalize(text: &str) -> String {
    text.to_lowercase()
        .replace(['á', 'à', 'ä'], "a")
        .replace(['é', 'è', 'ë'], "e")
        .replace(['í', 'ì', 'ï'], "i")
        .replace(['ó', 'ò', 'ö'], "o")
        .replace(['ú', 'ù', 'ü'], "u")
}

fn unused_loopback_port() -> Result<u16> {
    let listener = std::net::TcpListener::bind((Ipv4Addr::LOCALHOST, 0))?;
    Ok(listener.local_addr()?.port())
}

fn workspace_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .and_then(Path::parent)
        .expect("kuali-engine debe vivir dentro del workspace")
        .to_path_buf()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn speaker(id: u64, name: &str, is_self: bool) -> Speaker {
        let mut speaker = Speaker::unknown(id);
        speaker.display_name = name.to_string();
        speaker.is_self = is_self;
        speaker
    }

    fn utterance(speaker_id: u64, text: &str) -> Utterance {
        Utterance {
            id: format!("u{speaker_id}"),
            speaker_id,
            start_ms: 0,
            end_ms: 1000,
            text: text.to_string(),
            confidence: None,
        }
    }

    fn expect(value: &str) -> ExpectSpeaker {
        ExpectSpeaker::parse(value).unwrap()
    }

    #[test]
    fn an_expected_speaker_matches_names_and_terms_case_insensitively() {
        let speakers = [speaker(1, "Ana García", false), speaker(2, "Tú", false)];
        let utterances = [utterance(1, "Código MANGO cuatro")];
        let wire = WireProbe::default();
        let ok = check_expect_speaker(&expect("garcía=mango"), &utterances, &speakers, &wire);
        assert!(ok.passed, "{}", ok.detail);
        let ok = check_expect_speaker(&expect("ANA=Mango"), &utterances, &speakers, &wire);
        assert!(ok.passed, "{}", ok.detail);
        let wrong = check_expect_speaker(&expect("Tú=mango"), &utterances, &speakers, &wire);
        assert!(!wrong.passed);
        assert!(wrong.detail.contains("Ana García"));
        let none = check_expect_speaker(&expect("ana=nube"), &utterances, &speakers, &wire);
        assert!(!none.passed);
        assert_eq!(none.detail, "nadie dijo ese término");
    }

    #[test]
    fn the_local_user_is_known_by_flag_reserved_name_or_wire_channel() {
        let wire = WireProbe::default();
        let utterances = [utterance(1, "hola kuali")];
        let by_flag = [speaker(1, "Yo", true)];
        assert!(check_expect_speaker(&expect("@self=kuali"), &utterances, &by_flag, &wire).passed);
        let by_name = [speaker(1, "Tú", false)];
        assert!(check_expect_speaker(&expect("@self=kuali"), &utterances, &by_name, &wire).passed);
        let other = [speaker(1, "Ana", false)];
        assert!(!check_expect_speaker(&expect("@self=kuali"), &utterances, &other, &wire).passed);

        let mut wired = WireProbe::default();
        wired.channels.insert(
            3,
            ChannelProbe {
                channel: 3,
                participant_id: Some("dev-1".into()),
                is_self: true,
                ..ChannelProbe::default()
            },
        );
        let mut by_wire = speaker(1, "Ana", false);
        by_wire.source_id = Some("dev-1".into());
        assert!(
            check_expect_speaker(&expect("@self=kuali"), &utterances, &[by_wire], &wired).passed
        );
    }

    #[test]
    fn an_expected_speaker_needs_both_a_name_and_a_term() {
        assert!(ExpectSpeaker::parse("sin igual").is_err());
        assert!(ExpectSpeaker::parse("=x").is_err());
        assert_eq!(
            expect(" Ana = mango cuatro "),
            ExpectSpeaker {
                substring: "Ana".into(),
                term: "mango cuatro".into()
            }
        );
    }

    #[test]
    fn custom_terms_replace_the_default_phrases_and_their_minimum() {
        let transcript = normalize("Kafka y Redis, código MANGO");
        let (matched, min) = control_terms(&transcript, None, None, true);
        assert_eq!(matched, ["kafka", "redis"]);
        assert_eq!(min, 4);
        assert_eq!(control_terms(&transcript, None, None, false).1, 2);

        let custom = parse_terms("Mango, ñandú ,, ");
        assert_eq!(custom, ["Mango", "ñandú"]);
        let (matched, min) = control_terms(&transcript, Some(&custom), Some(1), true);
        assert_eq!(matched, ["mango"]);
        assert_eq!(min, 1);
    }

    #[test]
    fn reports_keep_the_meet_name_and_use_the_platform_id_otherwise() {
        assert_eq!(report_prefix("google_meet"), "meet");
        assert_eq!(report_prefix("yandex_telemost"), "yandex_telemost");
    }

    #[test]
    fn only_ingest_sessions_are_observed_and_the_first_one_names_the_run() {
        assert!(is_ingest_path("/ingest?platform=yandex_telemost"));
        assert!(is_ingest_path("/ingest"));
        assert!(!is_ingest_path("/health?client=kuali-extension"));
        assert!(!is_ingest_path("/ingestion"));

        let mut wire = WireProbe::default();
        let first = wire.open_ingest("/ingest?platform=a", 10);
        let second = wire.open_ingest("/ingest?platform=b", 20);
        assert_eq!((first.index, second.index), (0, 1));
        assert_eq!(wire.request_path.as_deref(), Some("/ingest?platform=a"));
        assert_eq!(wire.connections[1].opened_ms, 20);
    }

    #[test]
    fn the_timeline_keeps_meeting_events_but_not_keepalives() {
        let mut wire = WireProbe::default();
        for json in [
            r#"{"kind":"keepalive","ts":1}"#,
            r#"{"kind":"roster-state","ts":2,"detail":{"participantCount":2,"participants":[{"participantId":"a","displayName":"Ana","isSelf":true},{"participantId":"b"}]}}"#,
            r#"{"kind":"participant-upsert","ts":3,"detail":{"channel":4,"participantId":"b","displayName":"Bob","inCall":true}}"#,
        ] {
            wire.observe_at(&Message::Text(json.into()), 5000);
        }
        assert_eq!(wire.timeline.len(), 2);
        assert_eq!(wire.timeline[0].participant_count, Some(2));
        assert_eq!(wire.timeline[0].participants.as_ref().unwrap().len(), 2);
        assert_eq!(wire.timeline[1].channel, Some(4));
        assert_eq!(wire.timeline[1].in_call, Some(true));
        assert_eq!(wire.timeline[1].ts_ms, 5000);
    }
}
