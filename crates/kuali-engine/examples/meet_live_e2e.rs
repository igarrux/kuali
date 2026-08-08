//! Interactive, real-browser Google Meet E2E.
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
use std::time::Duration;

use anyhow::{anyhow, Context, Result};
use chrono::Utc;
use futures_util::{SinkExt, StreamExt};
use kuali_core::{EngineStatus, KualiEvent, ModelState, Speaker, Utterance};
use kuali_engine::Engine;
use kuali_meet::wire::{decode_binary, decode_text, Frame, CAPTURE_SAMPLE_RATE};
use parking_lot::Mutex;
use serde::Serialize;
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::mpsc;
use tokio_tungstenite::tungstenite::handshake::server::{Request, Response};
use tokio_tungstenite::tungstenite::Message;

const DEFAULT_TIMEOUT_SECONDS: u64 = 15 * 60;

#[derive(Debug)]
struct Options {
    port: u16,
    timeout_seconds: u64,
    full: bool,
    report_dir: PathBuf,
}

impl Options {
    fn parse() -> Result<Self> {
        let mut port = 0;
        let mut timeout_seconds = DEFAULT_TIMEOUT_SECONDS;
        let mut full = true;
        let mut report_dir = workspace_root().join("target/e2e/reports");
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
                "--help" | "-h" => {
                    println!(
                        "Uso: cargo run -p kuali-engine --example meet_live_e2e -- \
                         [--port 19099] [--timeout-seconds 900] [--full|--solo]"
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
}

impl WireProbe {
    fn observe(&mut self, message: &Message) {
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
                if channel.display_name.is_none() {
                    channel.display_name = frame.speaker_name.clone();
                }
            }
            Message::Text(text) => {
                let Ok(Frame::Event(event)) = decode_text(text) else {
                    return;
                };
                self.meeting_events += 1;
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
    utterances: Vec<Utterance>,
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

    let (engine, mut events) = Engine::new(config);
    engine.start_web_ingest().await?;

    let wire = Arc::new(Mutex::new(WireProbe::default()));
    let (proxy_errors_tx, mut proxy_errors_rx) = mpsc::unbounded_channel();
    let proxy_wire = Arc::clone(&wire);
    let proxy = tokio::spawn(async move {
        if let Err(error) = proxy_once(external_listener, internal_port, proxy_wire).await {
            let _ = proxy_errors_tx.send(error.to_string());
        }
    });

    let ready = serde_json::json!({
        "port": external_port,
        "mode": if options.full { "full" } else { "solo" },
        "model": model.file_name(),
    });
    println!("KUALI_E2E_READY {ready}");
    println!("Frase local:  Prueba local de Kuali, código mango cuatro ocho dos siete. WaitingRoom, Kafka y Redis.");
    if options.full {
        println!("Frase remota: Prueba remota de Kuali, código nube nueve tres seis uno. Visual Studio Code y PostgreSQL.");
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

    if !proxy.is_finished() {
        tokio::time::sleep(Duration::from_millis(250)).await;
    }
    if !proxy.is_finished() {
        proxy.abort();
    }

    let test_meeting_removed = match meeting_id.as_deref() {
        Some(id) => engine.delete_meeting(id).is_ok(),
        None => true,
    };
    let wire = wire.lock().clone();
    let speaker_values = speakers.into_values().collect::<Vec<_>>();
    let assertions = assess(AssessmentInput {
        full: options.full,
        ended,
        previews,
        utterances: &utterances,
        speakers: &speaker_values,
        statuses: &statuses,
        model_states: &model_states,
        wire: &wire,
        removed: test_meeting_removed,
    });
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
        utterances,
        warnings,
        wire,
        assertions,
        test_meeting_removed,
    };

    std::fs::create_dir_all(&options.report_dir)?;
    let stamp = started_at.format("%Y%m%dT%H%M%SZ");
    let report_path = options.report_dir.join(format!("meet-{stamp}.json"));
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

// Tungstenite's handshake callback owns a deliberately large HTTP error type.
#[allow(clippy::result_large_err)]
async fn proxy_once(
    listener: TcpListener,
    upstream_port: u16,
    wire: Arc<Mutex<WireProbe>>,
) -> Result<()> {
    let (stream, _) = listener.accept().await?;
    let request_path = Arc::new(Mutex::new(String::new()));
    let captured_path = Arc::clone(&request_path);
    let client = tokio_tungstenite::accept_hdr_async(
        stream,
        move |request: &Request, response: Response| {
            *captured_path.lock() = request.uri().to_string();
            Ok(response)
        },
    )
    .await?;
    let request_path = request_path.lock().clone();
    wire.lock().request_path = Some(request_path.clone());

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
                wire.lock().observe(&message);
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
    let terms = [
        "kuali",
        "waitingroom",
        "kafka",
        "redis",
        "visual studio code",
        "postgresql",
    ];
    let matched_terms = terms
        .iter()
        .filter(|term| transcript.contains(**term))
        .copied()
        .collect::<Vec<_>>();
    let total_seconds = wire.audio_samples as f64 / CAPTURE_SAMPLE_RATE as f64;

    vec![
        assertion(
            "Handshake de Google Meet",
            wire.request_path.as_deref().is_some_and(|path| {
                path.contains("platform=google_meet")
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
            matched_terms.len() >= if full { 4 } else { 2 },
            format!("términos reconocidos: {}", matched_terms.join(", ")),
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
    ]
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
