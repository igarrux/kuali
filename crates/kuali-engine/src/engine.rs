//! State machine that turns joining a call into a transcript and action items.
//!
//! This is the only layer connecting Discord, browser meetings, Whisper, LLMs,
//! and storage; every other subsystem remains isolated.

use std::collections::{HashMap, HashSet};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

use chrono::Utc;
use kuali_core::{
    DiscordSummaryDelivery, DiscordUserId, EngineStatus, KualiConfig, KualiEvent, Meeting,
    MeetingMeta, MeetingSummary, ModelState, Utterance, VoiceSessionId, WhisperModel,
};
use kuali_discord::{CallInfo, DiscordHandle, DiscordSummaryState, VoiceEvent};
use kuali_stt::{i16_to_f32, Segment, Segmenter};
use parking_lot::{Mutex, RwLock};
use tokio::sync::mpsc::{self, UnboundedReceiver, UnboundedSender};
use tokio::sync::watch;
use tokio::sync::Mutex as AsyncMutex;
use tokio::task::JoinSet;

use crate::stt_worker::{PendingTranscription, SttWorker, TranscriptionPass};

/// Discord emits a tick every 20 ms. Counting ticks instead of wall time keeps
/// timestamps aligned with captured audio even when transcription falls behind.
const TICK_MS: u64 = 20;

/// Drafts improve latency but must never block final utterances under concurrent
/// speech. Whisper has one mutable state, so pending drafts are bounded.
const MAX_QUEUED_PREVIEWS: usize = 4;
const MAX_SUMMARY_ATTEMPTS: usize = 3;

/// Wrapped dependency errors include large enums such as `serenity::Error`.
/// Boxing prevents every engine `Result` from carrying that stack cost on the
/// overwhelmingly common `Ok` path.
#[derive(Debug, thiserror::Error)]
pub enum EngineError {
    #[error("configuration is incomplete: missing {0}")]
    Incomplete(String),
    #[error(transparent)]
    Discord(Box<kuali_discord::DiscordError>),
    #[error(transparent)]
    Store(Box<kuali_store::StoreError>),
    #[error(transparent)]
    Llm(Box<kuali_llm::LlmError>),
    #[error("failed to save configuration: {0}")]
    Config(Box<kuali_core::paths::ConfigError>),
    #[error("failed to relocate Whisper weights: {0}")]
    ModelStorage(String),
    #[error("that model cannot be deleted while Kuali is using it in a call")]
    ActiveModelDeletion,
    #[error("model download cancelled")]
    ModelDownloadCancelled,
    #[error(transparent)]
    Model(Box<kuali_stt::ModelError>),
    #[error("there is no meeting in progress")]
    NoActiveMeeting,
    #[error("a meeting cannot be deleted while it is in progress")]
    ActiveMeetingDeletion,
    #[error("invalid webhook: {0}")]
    Webhook(String),
    #[error("failed to start web meeting ingest: {0}")]
    WebMeetings(String),
    #[error("summaries and tasks are disabled in Settings")]
    SummariesDisabled,
}

// `#[from]` would generate `From<Box<_>>`; these implementations let `?` accept
// the unboxed dependency errors directly.
macro_rules! boxed_from {
    ($($source:ty => $variant:ident),* $(,)?) => {
        $(impl From<$source> for EngineError {
            fn from(error: $source) -> Self {
                Self::$variant(Box::new(error))
            }
        })*
    };
}

boxed_from! {
    kuali_discord::DiscordError => Discord,
    kuali_store::StoreError => Store,
    kuali_llm::LlmError => Llm,
    kuali_stt::ModelError => Model,
    kuali_core::paths::ConfigError => Config,
}

struct ActiveMeeting {
    meeting: Meeting,
    segmenter: Segmenter,
    /// Ticks since admission; multiplying by 20 yields elapsed milliseconds.
    ticks: u64,
    text_channel_id: u64,
    /// Closing rejects new packets, flushes existing segments, and waits for this
    /// meeting's queued Whisper work.
    ending: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
enum VoiceSource {
    Discord,
    Web,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
struct VoiceSessionKey {
    source: VoiceSource,
    id: VoiceSessionId,
}

impl ActiveMeeting {
    fn now_ms(&self) -> u64 {
        self.ticks * TICK_MS
    }
}

struct Inner {
    config: RwLock<KualiConfig>,
    events: UnboundedSender<KualiEvent>,
    status: RwLock<EngineStatus>,
    model_state: RwLock<ModelState>,
    /// Model loaded or reserved for loading. This is separate from configuration
    /// because users may change their selection while an earlier meeting closes.
    loaded_model: RwLock<Option<WhisperModel>>,
    active: Mutex<HashMap<VoiceSessionKey, ActiveMeeting>>,
    /// Every source owns a queue. One loop handles them while session identity
    /// prevents foreign disconnects, ticks, or audio from affecting a meeting.
    discord_voice_tx: UnboundedSender<VoiceEvent>,
    web_voice_tx: UnboundedSender<VoiceEvent>,
    /// Receivers are handed to the loop together exactly once.
    voice_rx: Mutex<Option<(UnboundedReceiver<VoiceEvent>, UnboundedReceiver<VoiceEvent>)>>,
    /// Browser-meeting receiver task while listening.
    web_ingest: AsyncMutex<Option<tokio::task::JoinHandle<()>>>,
    /// UI-readable state that avoids blocking on the async mutex.
    web_ingest_ready: AtomicBool,
    stt: SttWorker,
    /// Each meeting owns its task group. Whisper serializes inference globally,
    /// but closing one meeting waits only for its work and never blocks audio
    /// reception for others.
    transcriptions: AsyncMutex<HashMap<String, JoinSet<()>>>,
    /// Prevents obsolete drafts for one turn from accumulating behind final work
    /// on the single Whisper worker.
    previews_in_flight: Mutex<HashSet<String>>,
    /// Prevents a slow draft from reappearing after its turn closes.
    closed_segments: Mutex<HashSet<String>>,
    /// Prevents concurrent writes to one `.part` and coordinates relocation with
    /// any in-progress download.
    model_download: AsyncMutex<()>,
    /// A generation change cancels the active download and every duplicate
    /// request that was queued before the user pressed Cancel.
    model_download_cancellation: watch::Sender<u64>,
    discord: AsyncMutex<Option<DiscordHandle>>,
}

impl Inner {
    fn emit(&self, event: KualiEvent) {
        // A closed interface is not a reason to stop the engine.
        let _ = self.events.send(event);
    }

    fn set_status(&self, status: EngineStatus) {
        *self.status.write() = status.clone();
        self.emit(KualiEvent::StatusChanged { status });
    }

    fn set_model_state(&self, state: ModelState) {
        *self.model_state.write() = state.clone();
        self.emit(KualiEvent::ModelStateChanged { state });
    }
}

#[derive(Clone)]
pub struct Engine {
    inner: Arc<Inner>,
}

fn configured_model_target_changed(previous: &KualiConfig, next: &KualiConfig) -> bool {
    previous.whisper.model != next.whisper.model
        || previous.whisper.resolved_models_directory() != next.whisper.resolved_models_directory()
}

impl Engine {
    /// Creates the engine and returns the event receiver consumed by the interface.
    pub fn new(config: KualiConfig) -> (Self, UnboundedReceiver<KualiEvent>) {
        let (events, rx) = mpsc::unbounded_channel();
        let (discord_voice_tx, discord_voice_rx) = mpsc::unbounded_channel();
        let (web_voice_tx, web_voice_rx) = mpsc::unbounded_channel();
        let (model_download_cancellation, _) = watch::channel(0);

        let inner = Arc::new(Inner {
            config: RwLock::new(config),
            events,
            status: RwLock::new(EngineStatus::Offline),
            model_state: RwLock::new(ModelState::Absent),
            loaded_model: RwLock::new(None),
            active: Mutex::new(HashMap::new()),
            discord_voice_tx,
            web_voice_tx,
            voice_rx: Mutex::new(Some((discord_voice_rx, web_voice_rx))),
            web_ingest: AsyncMutex::new(None),
            web_ingest_ready: AtomicBool::new(false),
            stt: SttWorker::spawn(),
            transcriptions: AsyncMutex::new(HashMap::new()),
            previews_in_flight: Mutex::new(HashSet::new()),
            closed_segments: Mutex::new(HashSet::new()),
            model_download: AsyncMutex::new(()),
            model_download_cancellation,
            discord: AsyncMutex::new(None),
        });

        let engine = Self { inner };
        engine.refresh_model_state();
        (engine, rx)
    }

    pub fn config(&self) -> KualiConfig {
        self.inner.config.read().clone()
    }

    pub fn status(&self) -> EngineStatus {
        self.inner.status.read().clone()
    }

    pub fn model_state(&self) -> ModelState {
        self.inner.model_state.read().clone()
    }

    /// Requests cancellation without waiting for the network stream or the
    /// download mutex. The active task performs safe partial-file cleanup.
    pub fn cancel_model_download(&self) -> bool {
        if !matches!(
            *self.inner.model_state.read(),
            ModelState::Downloading { .. }
        ) {
            return false;
        }

        let generation = *self.inner.model_download_cancellation.borrow();
        self.inner
            .model_download_cancellation
            .send_replace(generation.wrapping_add(1));
        true
    }

    /// `true` only after successfully binding the local port.
    pub fn web_ingest_ready(&self) -> bool {
        self.inner.web_ingest_ready.load(Ordering::Acquire)
    }

    /// Current meeting for rendering when the interface opens mid-call.
    pub fn current_meeting(&self) -> Option<Meeting> {
        self.current_meetings()
            .into_iter()
            .max_by_key(|meeting| meeting.meta.started_at)
    }

    /// Every live meeting. They share Whisper while retaining independent state
    /// and UI presence.
    pub fn current_meetings(&self) -> Vec<Meeting> {
        self.inner
            .active
            .lock()
            .values()
            .map(|active| active.meeting.clone())
            .collect()
    }

    fn refresh_model_state(&self) {
        *self.inner.model_state.write() = resting_model_state(&self.inner);
    }

    /// Starts the voice loop on first use.
    ///
    /// This cannot happen in `new`, before a Tokio runtime exists.
    fn ensure_voice_loop(&self) {
        let Some((discord_rx, web_rx)) = self.inner.voice_rx.lock().take() else {
            return;
        };
        let inner = Arc::clone(&self.inner);
        tokio::spawn(async move { run_voice_loop(inner, discord_rx, web_rx).await });
    }

    /// Listens for Meet, Teams, or Zoom audio from the extension. Idempotence
    /// prevents duplicate servers.
    pub async fn start_web_ingest(&self) -> Result<(), EngineError> {
        let config = self.config().meet;
        if !config.enabled {
            self.inner.web_ingest_ready.store(false, Ordering::Release);
            self.inner.emit(KualiEvent::WebMeetingsStatusChanged {
                enabled: false,
                port: config.port,
                listening: false,
            });
            return Ok(());
        }

        let mut running = self.inner.web_ingest.lock().await;
        if running.as_ref().is_some_and(|task| !task.is_finished()) {
            return Ok(());
        }

        self.ensure_voice_loop();
        let addr = std::net::SocketAddr::from(([127, 0, 0, 1], config.port));
        let listener = match tokio::net::TcpListener::bind(addr).await {
            Ok(listener) => listener,
            Err(error) => {
                self.inner.web_ingest_ready.store(false, Ordering::Release);
                self.inner.emit(KualiEvent::WebMeetingsStatusChanged {
                    enabled: true,
                    port: config.port,
                    listening: false,
                });
                return Err(EngineError::WebMeetings(format!("{addr}: {error}")));
            }
        };
        self.inner.web_ingest_ready.store(true, Ordering::Release);
        self.inner.emit(KualiEvent::WebMeetingsStatusChanged {
            enabled: true,
            port: config.port,
            listening: true,
        });
        let events = self.inner.web_voice_tx.clone();
        let inner = Arc::clone(&self.inner);
        *running = Some(tokio::spawn(async move {
            tracing::info!("listening for web meetings on ws://{addr}/ingest");
            kuali_meet::ingest::serve_on(listener, events).await;
            inner.web_ingest_ready.store(false, Ordering::Release);
            inner.emit(KualiEvent::WebMeetingsStatusChanged {
                enabled: true,
                port: addr.port(),
                listening: false,
            });
        }));
        Ok(())
    }

    /// Stops browser-meeting ingest and closes its active meetings cleanly.
    pub async fn stop_web_ingest(&self) {
        self.inner.web_ingest_ready.store(false, Ordering::Release);
        if let Some(task) = self.inner.web_ingest.lock().await.take() {
            task.abort();
        }
        for session in session_keys_for_source(&self.inner, VoiceSource::Web) {
            finish_meeting(&self.inner, session).await;
        }
        let config = self.config().meet;
        self.inner.emit(KualiEvent::WebMeetingsStatusChanged {
            enabled: config.enabled,
            port: config.port,
            listening: false,
        });
    }

    /// API compatibility with early builds that named only Meet support.
    pub async fn start_meet_ingest(&self) -> Result<(), EngineError> {
        self.start_web_ingest().await
    }

    pub async fn stop_meet_ingest(&self) {
        self.stop_web_ingest().await;
    }

    /// Connects to Discord and waits for a voice call.
    pub async fn connect(&self) -> Result<(), EngineError> {
        let config = self.config();
        if let Some(missing) = config.missing_requirements().first() {
            return Err(EngineError::Incomplete((*missing).to_string()));
        }

        self.disconnect().await;

        self.ensure_voice_loop();
        let handle =
            kuali_discord::start(config.discord.clone(), self.inner.discord_voice_tx.clone())
                .await?;
        *self.inner.discord.lock().await = Some(handle);

        if self.inner.active.lock().is_empty() {
            self.inner.set_status(EngineStatus::Watching);
        }
        Ok(())
    }

    /// Disconnects from Discord after closing any active meeting cleanly.
    pub async fn disconnect(&self) {
        for session in session_keys_for_source(&self.inner, VoiceSource::Discord) {
            finish_meeting(&self.inner, session).await;
        }
        if let Some(handle) = self.inner.discord.lock().await.take() {
            handle.shutdown().await;
        }
        if self.inner.active.lock().is_empty() {
            if let Err(error) = self.inner.stt.unload().await {
                self.inner.emit(KualiEvent::error("whisper", error));
            }
            *self.inner.loaded_model.write() = None;
            self.inner.set_status(EngineStatus::Offline);
        }
    }

    /// Leaves only the voice channel. The meeting closes and Whisper may unload,
    /// while the Discord gateway remains connected and watching.
    pub async fn leave_call(&self) -> Result<(), EngineError> {
        let sessions = session_keys_for_source(&self.inner, VoiceSource::Discord);
        if sessions.is_empty() {
            return Err(EngineError::NoActiveMeeting);
        }

        {
            let discord = self.inner.discord.lock().await;
            if let Some(handle) = discord.as_ref() {
                handle.leave_call().await;
            }
        }
        for session in sessions {
            finish_meeting(&self.inner, session).await;
        }
        Ok(())
    }

    /// Saves configuration and reconnects when required by changed settings.
    pub async fn update_config(&self, config: KualiConfig) -> Result<(), EngineError> {
        validate_webhook_config(&config)?;
        let previous = self.config();
        let previous_models = previous.whisper.resolved_models_directory();
        let next_models = config.whisper.resolved_models_directory();
        let model_storage_changed = previous_models != next_models;
        let download_configured_model = configured_model_target_changed(&previous, &config);

        if model_storage_changed {
            let _download_guard = self.inner.model_download.lock().await;
            self.inner.set_model_state(ModelState::Verifying);
            let relocation = async {
                relocate_model_sources(
                    next_models.clone(),
                    vec![
                        previous_models,
                        kuali_core::paths::models_dir(),
                        kuali_core::paths::legacy_models_dir(),
                    ],
                )
                .await?;
                verify_models_after_relocation(next_models.clone()).await
            }
            .await;
            let corrupted = match relocation {
                Ok(corrupted) => corrupted,
                Err(error) => {
                    self.inner.set_model_state(ModelState::Failed {
                        message: error.to_string(),
                    });
                    return Err(error);
                }
            };
            if !corrupted.is_empty() {
                tracing::warn!(
                    ?corrupted,
                    "removed corrupt weights after changing their location"
                );
                self.inner.emit(KualiEvent::error(
                    "whisper",
                    "Uno de los modelos trasladados no superó la verificación de integridad. Kuali descargará una copia limpia si hace falta.",
                ));
            }
        }
        kuali_core::paths::save_config(&config)?;

        let reconnect = previous.discord.bot_token != config.discord.bot_token;
        let refresh_discord_config = previous.discord != config.discord;
        let restart_web_ingest = previous.meet != config.meet;

        *self.inner.config.write() = config.clone();
        self.refresh_model_state();
        self.inner.emit(KualiEvent::ModelStateChanged {
            state: self.model_state(),
        });

        if restart_web_ingest {
            self.stop_web_ingest().await;
            self.start_web_ingest().await?;
        }

        if refresh_discord_config && !reconnect {
            if let Some(handle) = self.inner.discord.lock().await.as_ref() {
                handle.update_config(config.discord.clone());
            }
        }

        if reconnect && config.is_ready() {
            self.connect().await?;
        }
        // Configuration unrelated to transcription must not choose and start a
        // large model download during first-run onboarding. Changing the model
        // or its storage location still guarantees the selected weight exists.
        if download_configured_model {
            self.download_configured_model_if_missing();
        }
        Ok(())
    }

    /// Downloads configured model weights while reporting progress.
    pub async fn download_model(&self, model: WhisperModel) -> Result<(), EngineError> {
        download_model(&self.inner, model).await
    }

    /// Deletes downloaded weights. Mutual exclusion prevents removing a `.part`
    /// during writes, and `loaded_model` protects meetings. Silero is base
    /// infrastructure and remains even after the final Whisper weight is removed.
    pub async fn delete_model(&self, model: WhisperModel) -> Result<u64, EngineError> {
        let _download_guard = self.inner.model_download.lock().await;
        if *self.inner.loaded_model.read() == Some(model) {
            return Err(EngineError::ActiveModelDeletion);
        }

        let models_dir = self.config().whisper.resolved_models_directory();
        let removed_bytes = tokio::task::spawn_blocking(move || {
            let paths = [
                kuali_stt::model_path(&models_dir, model),
                kuali_stt::model::partial_path(&models_dir, model),
            ];
            let bytes: u64 = paths
                .iter()
                .filter_map(|path| std::fs::metadata(path).ok())
                .map(|metadata| metadata.len())
                .sum();
            kuali_stt::model::remove(&models_dir, model)?;
            Ok::<_, std::io::Error>(bytes)
        })
        .await
        .map_err(|error| EngineError::ModelStorage(error.to_string()))?
        .map_err(|error| EngineError::ModelStorage(error.to_string()))?;

        self.refresh_model_state();
        self.inner.emit(KualiEvent::ModelStateChanged {
            state: self.model_state(),
        });
        Ok(removed_bytes)
    }

    /// Consolidates legacy and default-location weights into configured storage.
    pub async fn prepare_model_storage(&self) -> Result<(), EngineError> {
        let destination = self.config().whisper.resolved_models_directory();
        let _download_guard = self.inner.model_download.lock().await;
        let relocated = relocate_model_sources(
            destination.clone(),
            vec![
                kuali_core::paths::models_dir(),
                kuali_core::paths::legacy_models_dir(),
            ],
        )
        .await?;
        if relocated > 0 {
            self.inner.set_model_state(ModelState::Verifying);
            let corrupted = match verify_models_after_relocation(destination).await {
                Ok(corrupted) => corrupted,
                Err(error) => {
                    self.inner.set_model_state(ModelState::Failed {
                        message: error.to_string(),
                    });
                    return Err(error);
                }
            };
            if !corrupted.is_empty() {
                tracing::warn!(
                    ?corrupted,
                    "removed corrupt weights after consolidating their location"
                );
                self.inner.emit(KualiEvent::error(
                    "whisper",
                    "Uno de los modelos trasladados no superó la verificación de integridad. Kuali descargará una copia limpia si hace falta.",
                ));
            }
        }
        self.refresh_model_state();
        self.inner.emit(KualiEvent::ModelStateChanged {
            state: self.model_state(),
        });
        Ok(())
    }

    /// Starts the selected model download without blocking settings or Discord startup.
    pub fn download_configured_model_if_missing(&self) {
        let config = self.config();
        let model = config.whisper.model;
        let models_dir = config.whisper.resolved_models_directory();
        if kuali_stt::is_downloaded(&models_dir, model) && kuali_stt::is_vad_downloaded(&models_dir)
        {
            return;
        }

        let inner = Arc::clone(&self.inner);
        tokio::spawn(async move {
            if let Err(error) = download_model(&inner, model).await {
                if !matches!(error, EngineError::ModelDownloadCancelled) {
                    tracing::warn!(%error, "failed to download the model automatically");
                }
            }
        });
    }

    pub fn list_meetings(&self) -> Result<Vec<MeetingMeta>, EngineError> {
        Ok(kuali_store::list()?)
    }

    pub fn load_meeting(&self, id: &str) -> Result<Meeting, EngineError> {
        // In-memory state is newer than disk for an active meeting.
        if let Some(active) = self
            .inner
            .active
            .lock()
            .values()
            .find(|active| active.meeting.meta.id == id)
        {
            return Ok(active.meeting.clone());
        }
        Ok(kuali_store::load(id)?)
    }

    pub fn delete_meeting(&self, id: &str) -> Result<(), EngineError> {
        if self
            .inner
            .active
            .lock()
            .values()
            .any(|active| active.meeting.meta.id == id)
        {
            return Err(EngineError::ActiveMeetingDeletion);
        }
        Ok(kuali_store::delete(id)?)
    }

    /// Deletes multiple meetings as one operation after validating that every
    /// target exists and none is active.
    pub fn delete_meetings(&self, ids: &[String]) -> Result<usize, EngineError> {
        let mut seen = HashSet::new();
        let ids: Vec<&String> = ids
            .iter()
            .filter(|id| !id.trim().is_empty() && seen.insert(id.as_str()))
            .collect();

        let active_ids = self
            .inner
            .active
            .lock()
            .values()
            .map(|active| active.meeting.meta.id.clone())
            .collect::<HashSet<_>>();
        if ids.iter().any(|id| active_ids.contains(id.as_str())) {
            return Err(EngineError::ActiveMeetingDeletion);
        }

        for id in &ids {
            if !kuali_store::meeting_dir(id).is_dir() {
                return Err(kuali_store::StoreError::NotFound((*id).clone()).into());
            }
        }
        for id in &ids {
            kuali_store::delete(id)?;
        }
        Ok(ids.len())
    }

    /// Library groups are virtual Discord server/channel or browser-platform
    /// folders. One anchor ID avoids sending precision-sensitive snowflakes to JavaScript.
    pub fn delete_channel_meetings(&self, meeting_id: &str) -> Result<usize, EngineError> {
        let meetings = kuali_store::list()?;
        let anchor = meetings
            .iter()
            .find(|meeting| meeting.id == meeting_id)
            .ok_or_else(|| kuali_store::StoreError::NotFound(meeting_id.to_string()))?;
        let web_platform = matches!(
            anchor.guild_name.as_str(),
            "Google Meet" | "Microsoft Teams" | "Zoom" | "Reunión web"
        );
        let ids: Vec<String> = meetings
            .iter()
            .filter(|meeting| {
                if web_platform {
                    meeting.guild_name == anchor.guild_name
                } else {
                    meeting.guild_id == anchor.guild_id && meeting.channel_id == anchor.channel_id
                }
            })
            .map(|meeting| meeting.id.clone())
            .collect();
        self.delete_meetings(&ids)
    }

    /// Marks an action item complete or pending.
    pub fn set_task_done(
        &self,
        meeting_id: &str,
        task_id: &str,
        done: bool,
    ) -> Result<(), EngineError> {
        let mut meeting = self.load_meeting(meeting_id)?;
        if let Some(summary) = meeting.summary.as_mut() {
            if let Some(task) = summary.action_items.iter_mut().find(|t| t.id == task_id) {
                task.done = done;
            }
        }
        kuali_store::save(&meeting)?;

        if let Some(active) = self
            .inner
            .active
            .lock()
            .values_mut()
            .find(|active| active.meeting.meta.id == meeting_id)
        {
            active.meeting = meeting;
        }
        Ok(())
    }

    /// Requests another LLM summary after changing providers or receiving a weak result.
    pub async fn resummarize(&self, meeting_id: &str) -> Result<MeetingSummary, EngineError> {
        let config = self.inner.config.read().clone();
        if !config.llm.summarize_on_leave {
            return Err(EngineError::SummariesDisabled);
        }
        let mut meeting = self.load_meeting(meeting_id)?;
        summarize_and_sync(&self.inner, &mut meeting, &config, 0)
            .await
            .map_err(Into::into)
    }

    pub fn export(
        &self,
        meeting_id: &str,
        path: &std::path::Path,
        as_markdown: bool,
    ) -> Result<(), EngineError> {
        let meeting = self.load_meeting(meeting_id)?;
        if as_markdown {
            kuali_store::export_markdown(&meeting, path)?;
        } else {
            kuali_store::export_json(&meeting, path)?;
        }
        Ok(())
    }

    pub fn suggested_filename(&self, meeting_id: &str, extension: &str) -> String {
        self.load_meeting(meeting_id)
            .map(|m| kuali_store::suggested_filename(&m, extension))
            .unwrap_or_else(|_| format!("reunion.{extension}"))
    }

    pub async fn available_providers(&self) -> Vec<kuali_llm::ProviderInfo> {
        kuali_llm::available_provider_infos(&self.config().llm).await
    }

    /// Full provider catalog with availability state for Settings.
    pub async fn provider_statuses(&self) -> Vec<kuali_llm::ProviderStatus> {
        kuali_llm::provider_statuses(&self.config().llm).await
    }

    /// Checks a provider with a minimal call and returns its response for Settings.
    ///
    /// `settings` are unsaved on-screen values. Testing never writes to disk, so
    /// canceling after an invalid key leaves configuration unchanged.
    pub async fn test_provider(
        &self,
        id: &str,
        settings: Option<kuali_core::ProviderSettings>,
    ) -> Result<String, EngineError> {
        let mut config = self.config().llm;
        if let Some(settings) = settings {
            config.providers.insert(id.to_string(), settings);
        }
        Ok(kuali_llm::test_provider(&config, id).await?)
    }

    /// Models currently published by the provider, avoiding stale bundled lists.
    pub async fn list_models(
        &self,
        id: &str,
        settings: Option<kuali_core::ProviderSettings>,
    ) -> Result<Vec<kuali_llm::ModelChoice>, EngineError> {
        let mut config = self.config().llm;
        if let Some(settings) = settings {
            config.providers.insert(id.to_string(), settings);
        }
        Ok(kuali_llm::list_models(&config, id).await?)
    }

    /// Sends a test event with unsaved Settings values.
    pub async fn test_webhook(
        &self,
        subscription: &kuali_core::WebhookSubscription,
    ) -> Result<String, EngineError> {
        crate::webhooks::test(subscription)
            .await
            .map_err(|error| EngineError::Webhook(error.to_string()))
    }
}

// ---------------------------------------------------------------------------
// Voice loop
// ---------------------------------------------------------------------------

async fn run_voice_loop(
    inner: Arc<Inner>,
    mut discord_rx: UnboundedReceiver<VoiceEvent>,
    mut web_rx: UnboundedReceiver<VoiceEvent>,
) {
    loop {
        let next = tokio::select! {
            event = discord_rx.recv() => event.map(|event| (VoiceSource::Discord, event)),
            event = web_rx.recv() => event.map(|event| (VoiceSource::Web, event)),
        };
        let Some((source, event)) = next else {
            break;
        };
        handle_voice_event(&inner, source, event).await;
    }
}

async fn handle_voice_event(inner: &Arc<Inner>, source: VoiceSource, event: VoiceEvent) {
    match event {
        VoiceEvent::Session { session_id, event } => {
            handle_session_event(
                inner,
                VoiceSessionKey {
                    source,
                    id: session_id,
                },
                *event,
            )
            .await;
        }
        VoiceEvent::MeetingRequested {
            meeting_id,
            guild_id,
            reply,
        } => {
            let result = meeting_for_discord(inner, &meeting_id, guild_id);
            let _ = reply.send(result);
        }
        VoiceEvent::FollowRequested { user_id, reply } => {
            let result = configure_discord_follow(inner, user_id).await;
            let _ = reply.send(result);
        }
        VoiceEvent::Warning(message) => emit_voice_warning(inner, source, message),
        // Legacy integrations emitted unwrapped events. Assign a stable session
        // per source to preserve compatibility.
        event => {
            handle_session_event(inner, VoiceSessionKey { source, id: 0 }, event).await;
        }
    }
}

async fn configure_discord_follow(
    inner: &Arc<Inner>,
    user_id: DiscordUserId,
) -> Result<(), String> {
    let mut config = inner.config.read().clone();
    if config
        .discord
        .follow_user_id
        .is_some_and(|configured| configured != user_id)
    {
        return Err(
            "Kuali ya sigue a otra persona. Borra ese usuario en Ajustes → Discord antes de vincular uno nuevo."
                .into(),
        );
    }
    config.discord.follow_user_id = Some(user_id);
    config.discord.follow_automatically = true;
    kuali_core::paths::save_config(&config).map_err(|error| error.to_string())?;
    *inner.config.write() = config.clone();
    if let Some(handle) = inner.discord.lock().await.as_ref() {
        handle.update_config(config.discord);
    }
    inner.emit(KualiEvent::DiscordFollowChanged {
        user_id: user_id.to_string(),
        enabled: true,
    });
    Ok(())
}

async fn handle_session_event(inner: &Arc<Inner>, session: VoiceSessionKey, event: VoiceEvent) {
    match event {
        VoiceEvent::ConnectionRequested { info: _, reply } => {
            let _ = reply.send(Ok(()));
        }
        VoiceEvent::Connected(info) => {
            if let Err(message) = start_meeting(inner, session, info).await {
                inner.emit(KualiEvent::error("capture", message));
            }
        }
        VoiceEvent::Disconnected => {
            // Closing may wait for Whisper, so run it outside the sole receive
            // loop while other meetings continue consuming real-time events.
            let inner = Arc::clone(inner);
            tokio::spawn(async move { finish_meeting(&inner, session).await });
        }
        VoiceEvent::ParticipantPresent(speaker) => {
            let meeting_id = {
                let mut active = inner.active.lock();
                let Some(active) = active.get_mut(&session) else {
                    return;
                };
                active.meeting.upsert_speaker(speaker.clone());
                active.meeting.meta.id.clone()
            };
            inner.emit(KualiEvent::SpeakerJoined {
                meeting_id,
                speaker,
            });
        }
        VoiceEvent::ParticipantLeft(user_id) => {
            // Speech in progress at disconnect still belongs to the meeting.
            let (meeting_id, segment) = {
                let mut active = inner.active.lock();
                let Some(active) = active.get_mut(&session) else {
                    return;
                };
                (
                    active.meeting.meta.id.clone(),
                    active.segmenter.close(user_id),
                )
            };
            if let Some(segment) = segment {
                queue_final_transcription(inner, &meeting_id, segment).await;
            }
            inner.emit(KualiEvent::SpeakerLeft {
                meeting_id,
                user_id,
            });
        }
        VoiceEvent::Audio { user_id, pcm } => {
            let samples = i16_to_f32(&pcm);
            let (meeting_id, pushed) = {
                let mut active = inner.active.lock();
                let Some(active) = active.get_mut(&session) else {
                    return;
                };
                if active.ending {
                    return;
                }
                let now = active.now_ms();
                (
                    active.meeting.meta.id.clone(),
                    active.segmenter.push_continuous(user_id, now, &samples),
                )
            };
            if let Some(preview) = pushed.preview {
                queue_preview_transcription(inner, &meeting_id, preview).await;
            }
            if let Some(segment) = pushed.final_segment {
                queue_final_transcription(inner, &meeting_id, segment).await;
            }
        }
        VoiceEvent::SpeakingChanged { user_id, speaking } => {
            let meeting_id = {
                let active = inner.active.lock();
                let Some(active) = active.get(&session) else {
                    return;
                };
                active.meeting.meta.id.clone()
            };
            inner.emit(KualiEvent::SpeakingChanged {
                meeting_id,
                user_id,
                speaking,
            });
        }
        VoiceEvent::Tick => {
            let (meeting_id, segments) = {
                let mut active = inner.active.lock();
                let Some(active) = active.get_mut(&session) else {
                    return;
                };
                if active.ending {
                    return;
                }
                active.ticks += 1;
                let now = active.now_ms();
                (active.meeting.meta.id.clone(), active.segmenter.tick(now))
            };
            for segment in segments {
                queue_final_transcription(inner, &meeting_id, segment).await;
            }
        }
        VoiceEvent::Warning(message) => emit_voice_warning(inner, session.source, message),
        VoiceEvent::MeetingRequested {
            meeting_id,
            guild_id,
            reply,
        } => {
            let result = meeting_for_discord(inner, &meeting_id, guild_id);
            let _ = reply.send(result);
        }
        VoiceEvent::FollowRequested { user_id, reply } => {
            let result = configure_discord_follow(inner, user_id).await;
            let _ = reply.send(result);
        }
        VoiceEvent::Session { .. } => {}
    }
}

fn emit_voice_warning(inner: &Arc<Inner>, source: VoiceSource, message: String) {
    inner.emit(KualiEvent::error(
        match source {
            VoiceSource::Discord => "discord",
            VoiceSource::Web => "reunión web",
        },
        message,
    ));
}

fn session_keys_for_source(inner: &Arc<Inner>, source: VoiceSource) -> Vec<VoiceSessionKey> {
    inner
        .active
        .lock()
        .keys()
        .filter(|session| session.source == source)
        .copied()
        .collect()
}

fn meeting_for_discord(
    inner: &Arc<Inner>,
    meeting_id: &str,
    guild_id: u64,
) -> Result<Meeting, String> {
    let active = inner
        .active
        .lock()
        .values()
        .find(|active| active.meeting.meta.id == meeting_id)
        .map(|active| active.meeting.clone());
    let meeting = match active {
        Some(meeting) => meeting,
        None => kuali_store::load(meeting_id).map_err(|error| error.to_string())?,
    };

    if meeting.meta.guild_id != guild_id {
        return Err("Esa reunión no pertenece a este servidor.".to_string());
    }
    Ok(meeting)
}

async fn start_meeting(
    inner: &Arc<Inner>,
    session: VoiceSessionKey,
    info: CallInfo,
) -> Result<(), String> {
    if inner.active.lock().contains_key(&session) {
        return Ok(());
    }

    let needs_model = inner.loaded_model.read().is_none();
    if needs_model {
        inner.set_status(EngineStatus::Joining);
    }

    let config = inner.config.read().clone();
    let meta = MeetingMeta {
        id: uuid::Uuid::new_v4().to_string(),
        display_title: None,
        guild_id: info.guild_id,
        guild_name: info.guild_name,
        channel_id: info.channel_id,
        channel_name: info.channel_name,
        started_at: Utc::now(),
        ended_at: None,
    };

    // Load the model into RAM only after a meeting exists.
    let model = inner.loaded_model.read().unwrap_or(config.whisper.model);
    let models_dir = config.whisper.resolved_models_directory();
    if needs_model {
        // Reserve before touching disk to close the race with deletion during join.
        *inner.loaded_model.write() = Some(model);
        if (!kuali_stt::is_downloaded(&models_dir, model)
            || !kuali_stt::is_vad_downloaded(&models_dir))
            && download_model(inner, model).await.is_err()
        {
            *inner.loaded_model.write() = None;
            inner.set_status(EngineStatus::Watching);
            return Err("No se pudieron preparar los pesos de Whisper.".to_string());
        }
        inner.set_model_state(ModelState::Loading);
        if let Err(message) = load_model_for_meeting(inner, model, &models_dir, &config).await {
            *inner.loaded_model.write() = None;
            inner.set_model_state(ModelState::Failed {
                message: message.clone(),
            });
            inner.set_status(EngineStatus::Watching);
            return Err(message);
        }
    }
    inner.set_model_state(ModelState::Active);

    let mut meeting = Meeting::new(meta.clone());
    prepare_discord_summary_delivery(&mut meeting, info.text_channel_id);
    if let Err(e) = kuali_store::save(&meeting) {
        inner.emit(KualiEvent::error("store", e));
    }

    inner.active.lock().insert(
        session,
        ActiveMeeting {
            meeting,
            segmenter: Segmenter::new(config.recording),
            ticks: 0,
            text_channel_id: info.text_channel_id,
            ending: false,
        },
    );

    inner.emit(KualiEvent::MeetingStarted { meeting: meta });
    inner.set_status(EngineStatus::Recording);
    Ok(())
}

/// Loads an already downloaded weight without hashing it on every meeting. If
/// whisper.cpp rejects the file, Kuali then pays the one-time verification cost
/// to distinguish damaged contents from a Metal or memory failure.
async fn load_model_for_meeting(
    inner: &Arc<Inner>,
    model: WhisperModel,
    models_dir: &std::path::Path,
    config: &KualiConfig,
) -> Result<(), String> {
    let path = kuali_stt::model_path(models_dir, model);
    let first_error = match inner.stt.load(path.clone(), &config.whisper).await {
        Ok(()) => return Ok(()),
        Err(error) => error,
    };

    inner.set_model_state(ModelState::Verifying);
    let verification_dir = models_dir.to_path_buf();
    let corrupt = tokio::task::spawn_blocking(move || {
        kuali_stt::model::remove_if_corrupt(&verification_dir, model)
    })
    .await
    .map_err(|error| format!("No se pudo comprobar la integridad de Whisper: {error}"))?
    .map_err(|error| format!("No se pudo comprobar la integridad de Whisper: {error}"))?;

    if corrupt {
        tracing::warn!(
            ?model,
            "replacing a corrupt Whisper weight after a load failure"
        );
        inner.emit(KualiEvent::ModelRecoveryStarted { model });
        download_model(inner, model)
            .await
            .map_err(|error| format!("No se pudo reemplazar el modelo dañado: {error}"))?;
        inner.set_model_state(ModelState::Loading);
        let clean_error = match inner.stt.load(path.clone(), &config.whisper).await {
            Ok(()) => return Ok(()),
            Err(error) => error,
        };
        return load_model_without_gpu(inner, path, &config.whisper, clean_error).await;
    }

    inner.set_model_state(ModelState::Loading);
    load_model_without_gpu(inner, path, &config.whisper, first_error).await
}

async fn load_model_without_gpu(
    inner: &Arc<Inner>,
    path: std::path::PathBuf,
    config: &kuali_core::WhisperConfig,
    gpu_error: crate::stt_worker::WorkerError,
) -> Result<(), String> {
    if !config.gpu {
        return Err(format!(
            "Whisper no pudo cargar el modelo aunque sus pesos están íntegros. Libera memoria o elige Large v3 Q5. Detalle: {gpu_error}"
        ));
    }

    let mut cpu_config = config.clone();
    cpu_config.gpu = false;
    tracing::warn!(%gpu_error, "Whisper failed with Metal; retrying on CPU");
    match inner.stt.load(path, &cpu_config).await {
        Ok(()) => {
            tracing::warn!("Whisper is using the CPU fallback for this meeting");
            Ok(())
        }
        Err(cpu_error) => Err(format!(
            "Whisper no pudo cargar el modelo aunque sus pesos están íntegros. Fallaron Metal ({gpu_error}) y CPU ({cpu_error}). Libera memoria o elige Large v3 Q5."
        )),
    }
}

async fn finish_meeting(inner: &Arc<Inner>, session: VoiceSessionKey) {
    // Partially buffered speech also belongs to the meeting.
    let claimed = {
        let mut active = inner.active.lock();
        let last_active = active.len() == 1;
        match active.get_mut(&session) {
            Some(active) => {
                if active.ending {
                    None
                } else {
                    active.ending = true;
                    Some((
                        active.meeting.meta.id.clone(),
                        active.segmenter.flush(),
                        last_active,
                    ))
                }
            }
            None => return,
        }
    };
    let Some((meeting_id, leftovers, last_active)) = claimed else {
        // Another path may already be closing this session, such as Songbird
        // disconnect racing the Leave call button. Explicit commands wait for
        // actual completion.
        while inner.active.lock().contains_key(&session) {
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
        return;
    };
    // Do not reprocess a recording. This state only drains live fragments already queued.
    if last_active {
        inner.set_status(EngineStatus::Finalizing);
    }
    for segment in leftovers {
        queue_final_transcription(inner, &meeting_id, segment).await;
    }
    drain_transcriptions(inner, &meeting_id).await;

    let Some(mut active) = inner.active.lock().remove(&session) else {
        return;
    };

    active.meeting.meta.ended_at = Some(Utc::now());
    if active.meeting.meta.display_title.is_none() {
        active.meeting.meta.display_title = Some(active.meeting.fallback_title());
    }
    if let Err(e) = kuali_store::save(&active.meeting) {
        inner.emit(KualiEvent::error("store", e));
    }

    // The model is shared and unloads only after the final session ends.
    if inner.active.lock().is_empty() {
        match inner.stt.unload().await {
            Ok(()) => {
                *inner.loaded_model.write() = None;
                inner.set_model_state(ModelState::Ready);
            }
            Err(error) => {
                *inner.loaded_model.write() = None;
                let message = error.to_string();
                inner.set_model_state(ModelState::Failed {
                    message: message.clone(),
                });
                inner.emit(KualiEvent::error("whisper", message));
            }
        }
        inner.set_status(EngineStatus::Watching);
    } else {
        inner.set_model_state(ModelState::Active);
        inner.set_status(EngineStatus::Recording);
    }
    inner.emit(KualiEvent::MeetingEnded {
        meeting_id: active.meeting.meta.id.clone(),
    });

    let config = inner.config.read().clone();

    // Summarization may take tens of seconds and must never block other live audio.
    let closing = {
        let inner = Arc::clone(inner);
        let mut active = active;
        async move {
            let summary_status = if active.meeting.utterances.is_empty() {
                crate::webhooks::SummaryStatus::Empty
            } else if !config.llm.summarize_on_leave {
                crate::webhooks::SummaryStatus::Disabled
            } else {
                match summarize_and_sync(
                    &inner,
                    &mut active.meeting,
                    &config,
                    active.text_channel_id,
                )
                .await
                {
                    Ok(summary) => {
                        drop(summary);
                        crate::webhooks::SummaryStatus::Ready
                    }
                    Err(e) => {
                        inner.emit(KualiEvent::error("llm", e));
                        crate::webhooks::SummaryStatus::Failed
                    }
                }
            };
            dispatch_completed_webhooks(
                &inner,
                &config.integrations.webhooks,
                &active.meeting,
                summary_status,
            );
        }
    };

    tokio::spawn(closing);
}

fn validate_webhook_config(config: &KualiConfig) -> Result<(), EngineError> {
    let mut ids = HashSet::new();
    for subscription in &config.integrations.webhooks {
        if !ids.insert(subscription.id.trim()) {
            return Err(EngineError::Webhook(format!(
                "hay dos suscripciones con id «{}»",
                subscription.id
            )));
        }
        if subscription.enabled {
            crate::webhooks::validate(subscription)
                .map_err(|error| EngineError::Webhook(error.to_string()))?;
        }
    }
    Ok(())
}

fn dispatch_completed_webhooks(
    inner: &Arc<Inner>,
    subscriptions: &[kuali_core::WebhookSubscription],
    meeting: &Meeting,
    summary_status: crate::webhooks::SummaryStatus,
) {
    for subscription in subscriptions
        .iter()
        .filter(|subscription| subscription.enabled && subscription.matches(&meeting.meta))
        .cloned()
    {
        let meeting = meeting.clone();
        let events = inner.events.clone();
        tokio::spawn(async move {
            match crate::webhooks::deliver_completed(&subscription, &meeting, summary_status).await
            {
                Ok(()) => tracing::info!(
                    webhook = %subscription.name,
                    meeting = %meeting.meta.id,
                    "meeting delivered to webhook"
                ),
                Err(error) => {
                    tracing::warn!(webhook = %subscription.name, %error, "webhook delivery failed");
                    let _ = events.send(KualiEvent::error(
                        format!("webhook · {}", subscription.name),
                        error,
                    ));
                }
            }
        });
    }
}

fn utterance_id(meeting_id: &str, segment_id: u64) -> String {
    format!("{meeting_id}-segment-{segment_id}")
}

async fn transcribe_live(
    inner: &Arc<Inner>,
    context: LiveTranscriptionContext,
    pending: PendingTranscription,
) {
    let LiveTranscriptionContext {
        meeting_id,
        utterance_id,
        speaker_id,
        start_ms,
        end_ms,
        pass,
    } = context;
    let transcription = match SttWorker::resolve_transcription(pending).await {
        Ok(t) => t,
        Err(e) => {
            inner.emit(KualiEvent::error("whisper", e));
            return;
        }
    };

    if pass == TranscriptionPass::Preview {
        // The turn may have ended while this draft waited in the queue.
        if inner.closed_segments.lock().contains(&utterance_id) {
            return;
        }
        if transcription.is_empty() {
            inner.emit(KualiEvent::UtterancePreviewCleared {
                meeting_id,
                utterance_id,
            });
            return;
        }
        inner.emit(KualiEvent::UtterancePreview {
            meeting_id,
            utterance: Utterance {
                id: utterance_id,
                speaker_id,
                start_ms,
                end_ms,
                text: transcription.text,
                confidence: transcription.confidence,
            },
        });
        return;
    }

    // Empty output means rejected silence or hallucination, which is successful filtering.
    if transcription.is_empty() {
        return;
    }
    let utterance = Utterance {
        id: utterance_id,
        speaker_id,
        start_ms,
        end_ms,
        text: transcription.text,
        confidence: transcription.confidence,
    };

    let (meeting_id, snapshot) = {
        let mut active = inner.active.lock();
        let Some(active) = active
            .values_mut()
            .find(|active| active.meeting.meta.id == meeting_id)
        else {
            return;
        };
        active.meeting.upsert_utterance(utterance.clone());
        (active.meeting.meta.id.clone(), active.meeting.clone())
    };

    if let Err(e) = kuali_store::save(&snapshot) {
        inner.emit(KualiEvent::error("store", e));
    }
    inner.emit(KualiEvent::UtteranceAdded {
        meeting_id,
        utterance,
    });
}

struct LiveTranscriptionContext {
    meeting_id: String,
    utterance_id: String,
    speaker_id: u64,
    start_ms: u64,
    end_ms: u64,
    pass: TranscriptionPass,
}

async fn queue_preview_transcription(inner: &Arc<Inner>, meeting_id: &str, segment: Segment) {
    let meeting_id = meeting_id.to_string();
    let id = utterance_id(&meeting_id, segment.id);
    if inner.closed_segments.lock().contains(&id) {
        return;
    }
    let queued = {
        let mut previews = inner.previews_in_flight.lock();
        previews.len() < MAX_QUEUED_PREVIEWS && previews.insert(id.clone())
    };
    if !queued {
        return;
    }

    let speaker_id = segment.speaker_id;
    let start_ms = segment.start_ms;
    let end_ms = segment.end_ms;
    let pending = match inner.stt.enqueue_transcription(
        speaker_id,
        start_ms,
        end_ms,
        segment.samples,
        TranscriptionPass::Preview,
        segment.overlap_with_previous,
    ) {
        Ok(pending) => pending,
        Err(error) => {
            inner.previews_in_flight.lock().remove(&id);
            inner.emit(KualiEvent::error("whisper", error));
            return;
        }
    };

    let task_inner = Arc::clone(inner);
    let group_id = meeting_id.clone();
    let mut groups = inner.transcriptions.lock().await;
    let tasks = groups.entry(group_id).or_default();
    reap_finished_transcriptions(inner, tasks);
    tasks.spawn(async move {
        transcribe_live(
            &task_inner,
            LiveTranscriptionContext {
                meeting_id,
                utterance_id: id.clone(),
                speaker_id,
                start_ms,
                end_ms,
                pass: TranscriptionPass::Preview,
            },
            pending,
        )
        .await;
        task_inner.previews_in_flight.lock().remove(&id);
    });
}

/// Enqueues a final turn. Audio remains only in memory until Whisper responds
/// and is never stored for post-call replay.
async fn queue_final_transcription(inner: &Arc<Inner>, meeting_id: &str, segment: Segment) {
    let meeting_id = meeting_id.to_string();
    let id = utterance_id(&meeting_id, segment.id);

    inner.closed_segments.lock().insert(id.clone());
    inner.emit(KualiEvent::UtterancePreviewCleared {
        meeting_id: meeting_id.clone(),
        utterance_id: id.clone(),
    });

    let speaker_id = segment.speaker_id;
    let start_ms = segment.start_ms;
    let end_ms = segment.end_ms;
    let pending = match inner.stt.enqueue_transcription(
        speaker_id,
        start_ms,
        end_ms,
        segment.samples,
        TranscriptionPass::LiveFinal,
        segment.overlap_with_previous,
    ) {
        Ok(pending) => pending,
        Err(error) => {
            inner.emit(KualiEvent::error("whisper", error));
            return;
        }
    };

    let task_inner = Arc::clone(inner);
    let group_id = meeting_id.clone();
    let mut groups = inner.transcriptions.lock().await;
    let tasks = groups.entry(group_id).or_default();
    reap_finished_transcriptions(inner, tasks);
    tasks.spawn(async move {
        transcribe_live(
            &task_inner,
            LiveTranscriptionContext {
                meeting_id,
                utterance_id: id,
                speaker_id,
                start_ms,
                end_ms,
                pass: TranscriptionPass::LiveFinal,
            },
            pending,
        )
        .await;
    });
}

fn reap_finished_transcriptions(inner: &Arc<Inner>, tasks: &mut JoinSet<()>) {
    while let Some(result) = tasks.try_join_next() {
        if let Err(error) = result {
            inner.emit(KualiEvent::error(
                "whisper",
                format!("a transcription task ended unexpectedly: {error}"),
            ));
        }
    }
}

/// Waits only for this meeting's capture before persistence and summarization;
/// other groups continue enqueueing speech.
async fn drain_transcriptions(inner: &Arc<Inner>, meeting_id: &str) {
    let Some(mut tasks) = inner.transcriptions.lock().await.remove(meeting_id) else {
        return;
    };
    while let Some(result) = tasks.join_next().await {
        if let Err(error) = result {
            inner.emit(KualiEvent::error(
                "whisper",
                format!("a transcription task ended unexpectedly: {error}"),
            ));
        }
    }
}

async fn summarize_and_sync(
    inner: &Arc<Inner>,
    meeting: &mut Meeting,
    config: &KualiConfig,
    text_channel_id: u64,
) -> Result<MeetingSummary, kuali_llm::LlmError> {
    // Provider selection comes first: without a configured and available model,
    // Kuali neither posts a progress card nor leaves a misleading failure in Discord.
    let provider = kuali_llm::select_provider(&config.llm).await?;
    if config.discord.post_summary_to_channel {
        prepare_discord_summary_delivery(meeting, text_channel_id);
        sync_discord_summary(
            inner,
            meeting,
            &config.llm.output_language,
            DiscordSummaryState::Preparing,
        )
        .await;
    }

    let result = summarize_with_retries(
        inner,
        meeting,
        provider.as_ref(),
        &config.llm.output_language,
    )
    .await;
    if config.discord.post_summary_to_channel {
        let state = match &result {
            Ok(_) => DiscordSummaryState::Ready,
            Err(error) if error.failure_kind() == kuali_llm::LlmFailureKind::AttentionRequired => {
                DiscordSummaryState::AttentionRequired
            }
            Err(_) => DiscordSummaryState::Failed,
        };
        sync_discord_summary(inner, meeting, &config.llm.output_language, state).await;
    }
    result
}

async fn summarize_with_retries(
    inner: &Arc<Inner>,
    meeting: &mut Meeting,
    provider: &dyn kuali_llm::LlmProvider,
    language: &str,
) -> Result<MeetingSummary, kuali_llm::LlmError> {
    if inner.active.lock().is_empty() {
        inner.set_status(EngineStatus::Summarizing);
    }
    inner.emit(KualiEvent::SummaryStarted {
        meeting_id: meeting.meta.id.clone(),
    });

    let mut completed = None;
    for attempt in 1..=MAX_SUMMARY_ATTEMPTS {
        match kuali_llm::summarize(provider, meeting, language).await {
            Ok(summary) => {
                completed = Some(summary);
                break;
            }
            Err(error) if attempt < MAX_SUMMARY_ATTEMPTS => {
                tracing::warn!(
                    meeting_id = %meeting.meta.id,
                    attempt,
                    max_attempts = MAX_SUMMARY_ATTEMPTS,
                    error = %error,
                    "el proveedor no produjo un resumen válido; reintentando"
                );
                tokio::time::sleep(Duration::from_millis(400 * attempt as u64)).await;
            }
            Err(error) => {
                inner.set_status(if inner.active.lock().is_empty() {
                    EngineStatus::Watching
                } else {
                    EngineStatus::Recording
                });
                return Err(error);
            }
        }
    }
    let summary = completed.expect("the retry loop returns on its final failure");

    meeting.meta.display_title = Some(summary.title.clone());
    meeting.summary = Some(summary.clone());
    if let Err(e) = kuali_store::save(meeting) {
        inner.emit(KualiEvent::error("store", e));
    }

    inner.emit(KualiEvent::SummaryReady {
        meeting_id: meeting.meta.id.clone(),
        summary: summary.clone(),
    });
    inner.set_status(if inner.active.lock().is_empty() {
        EngineStatus::Watching
    } else {
        EngineStatus::Recording
    });
    Ok(summary)
}

fn prepare_discord_summary_delivery(meeting: &mut Meeting, text_channel_id: u64) {
    if meeting.discord_summary_delivery.is_some() {
        return;
    }

    let channel_id = if text_channel_id != 0 {
        Some(text_channel_id)
    } else if !matches!(
        meeting.meta.guild_name.as_str(),
        "Google Meet" | "Microsoft Teams" | "Zoom" | "Reunión web"
    ) && meeting.meta.guild_id != 0
        && meeting.meta.channel_id != 0
    {
        // Meetings saved before delivery references existed still use their
        // Discord voice channel as the integrated text destination.
        Some(meeting.meta.channel_id)
    } else {
        None
    };

    if let Some(channel_id) = channel_id {
        meeting.discord_summary_delivery = Some(DiscordSummaryDelivery::pending(channel_id));
    }
}

async fn sync_discord_summary(
    inner: &Arc<Inner>,
    meeting: &mut Meeting,
    language: &str,
    state: DiscordSummaryState,
) {
    let Some(delivery) = meeting.discord_summary_delivery else {
        return;
    };
    // Persist the pending channel before touching Discord. If the bot is
    // offline, a later regeneration can still retry the same destination.
    if let Err(error) = kuali_store::save(meeting) {
        inner.emit(KualiEvent::error("store", error));
    }
    let discord = inner.discord.lock().await;
    if let Some(handle) = discord.as_ref() {
        match handle
            .sync_summary_state(delivery, meeting, language, state)
            .await
        {
            Ok(delivery) => {
                meeting.discord_summary_delivery = Some(delivery);
                if let Err(error) = kuali_store::save(meeting) {
                    inner.emit(KualiEvent::error("store", error));
                }
            }
            Err(error) => inner.emit(KualiEvent::error("discord", error)),
        }
    }
}

async fn relocate_model_sources(
    destination: std::path::PathBuf,
    sources: Vec<std::path::PathBuf>,
) -> Result<usize, EngineError> {
    tokio::task::spawn_blocking(move || {
        let mut relocated = 0;
        let mut visited = Vec::new();
        for source in sources {
            if source == destination || visited.contains(&source) {
                continue;
            }
            visited.push(source.clone());
            relocated += kuali_stt::model::relocate_models(&source, &destination)?;
        }
        Ok::<_, std::io::Error>(relocated)
    })
    .await
    .map_err(|error| EngineError::ModelStorage(error.to_string()))?
    .map_err(|error| EngineError::ModelStorage(error.to_string()))
}

/// Verifies complete weights only after their location changes. Hash mismatches
/// are removed so automatic download can replace them cleanly.
async fn verify_models_after_relocation(
    destination: std::path::PathBuf,
) -> Result<Vec<String>, EngineError> {
    tokio::task::spawn_blocking(move || {
        let mut corrupted = Vec::new();
        for model in WhisperModel::ALL {
            if !kuali_stt::is_downloaded(&destination, model) {
                continue;
            }

            let path = kuali_stt::model_path(&destination, model);
            match kuali_stt::verify_integrity(&path, model) {
                Ok(()) => {}
                Err(kuali_stt::ModelError::HashMismatch { .. }) => {
                    kuali_stt::model::remove(&destination, model)
                        .map_err(|error| error.to_string())?;
                    corrupted.push(format!("{model:?}"));
                }
                Err(error) => return Err(error.to_string()),
            }
        }
        if kuali_stt::is_vad_downloaded(&destination) {
            let path = kuali_stt::vad_model_path(&destination);
            match kuali_stt::verify_vad_integrity(&path) {
                Ok(()) => {}
                Err(kuali_stt::ModelError::HashMismatch { .. }) => {
                    std::fs::remove_file(&path).map_err(|error| error.to_string())?;
                    corrupted.push("Silero VAD".to_string());
                }
                Err(error) => return Err(error.to_string()),
            }
        }
        Ok::<_, String>(corrupted)
    })
    .await
    .map_err(|error| EngineError::ModelStorage(error.to_string()))?
    .map_err(EngineError::ModelStorage)
}

async fn download_model(inner: &Arc<Inner>, model: WhisperModel) -> Result<(), EngineError> {
    let mut cancellation = inner.model_download_cancellation.subscribe();
    let generation = *cancellation.borrow();
    let _download_guard = inner.model_download.lock().await;
    if *cancellation.borrow() != generation {
        return Err(EngineError::ModelDownloadCancelled);
    }
    let models_dir = inner.config.read().whisper.resolved_models_directory();
    let model_ready = kuali_stt::is_downloaded(&models_dir, model);
    let vad_ready = kuali_stt::is_vad_downloaded(&models_dir);
    if model_ready && vad_ready {
        let state = if !inner.active.lock().is_empty() {
            ModelState::Active
        } else {
            ModelState::Ready
        };
        inner.set_model_state(state);
        return Ok(());
    }

    if !model_ready {
        inner.set_model_state(ModelState::Downloading {
            model,
            downloaded_bytes: 0,
            total_bytes: Some(model.approx_bytes()),
        });

        // Per-chunk progress would flood the UI, so emit every half percentage point.
        let mut last_emitted = 0u64;
        let step = model.approx_bytes() / 200;
        let result = tokio::select! {
            result = kuali_stt::ensure_downloaded(&models_dir, model, |downloaded, total| {
                if downloaded.saturating_sub(last_emitted) >= step {
                    last_emitted = downloaded;
                    let _ = inner.events.send(KualiEvent::ModelStateChanged {
                        state: ModelState::Downloading {
                            model,
                            downloaded_bytes: downloaded,
                            total_bytes: total,
                        },
                    });
                }
            }) => Some(result),
            _ = cancellation.changed() => None,
        };
        let Some(result) = result else {
            cleanup_cancelled_download(&models_dir, model).await;
            inner.set_model_state(resting_model_state(inner));
            return Err(EngineError::ModelDownloadCancelled);
        };
        if let Err(e) = result {
            let message = e.to_string();
            inner.set_model_state(ModelState::Failed {
                message: message.clone(),
            });
            inner.emit(KualiEvent::error("whisper", message));
            return Err(e.into());
        }
    }

    if !vad_ready {
        // Silero prevents noise from reaching Whisper. Never silently fall back
        // to RMS; report and retry a failed small download instead of decoding noise.
        let result = tokio::select! {
            result = kuali_stt::ensure_vad_downloaded(&models_dir, |_, _| {}) => Some(result),
            _ = cancellation.changed() => None,
        };
        let Some(result) = result else {
            cleanup_cancelled_download(&models_dir, model).await;
            inner.set_model_state(resting_model_state(inner));
            return Err(EngineError::ModelDownloadCancelled);
        };
        if let Err(error) = result {
            let message = error.to_string();
            inner.set_model_state(ModelState::Failed {
                message: message.clone(),
            });
            inner.emit(KualiEvent::error("whisper", message));
            return Err(error.into());
        }
    }

    let state = if !inner.active.lock().is_empty() {
        ModelState::Active
    } else {
        ModelState::Ready
    };
    inner.set_model_state(state);
    Ok(())
}

fn resting_model_state(inner: &Inner) -> ModelState {
    if !inner.active.lock().is_empty() {
        return ModelState::Active;
    }
    let whisper = inner.config.read().whisper.clone();
    let model = whisper.model;
    let models_dir = whisper.resolved_models_directory();
    if kuali_stt::is_downloaded(&models_dir, model) && kuali_stt::is_vad_downloaded(&models_dir) {
        ModelState::Ready
    } else {
        ModelState::Absent
    }
}

async fn cleanup_cancelled_download(models_dir: &std::path::Path, model: WhisperModel) {
    for path in [
        kuali_stt::model::partial_path(models_dir, model),
        kuali_stt::vad_model_path(models_dir).with_extension("part"),
    ] {
        match tokio::fs::remove_file(&path).await {
            Ok(()) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => {
                tracing::warn!(%error, path = %path.display(), "failed to clean a cancelled model download")
            }
        }
    }
}

/// Waits for a specific engine state. Test-only helper.
#[doc(hidden)]
pub async fn wait_for_status(engine: &Engine, status: EngineStatus, timeout: Duration) -> bool {
    let deadline = tokio::time::Instant::now() + timeout;
    while tokio::time::Instant::now() < deadline {
        if engine.status() == status {
            return true;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    false
}

#[cfg(test)]
mod tests {
    use super::*;
    use async_trait::async_trait;
    use kuali_core::{color_for, Speaker};
    use kuali_llm::{CompletionRequest, LlmError, LlmProvider, ProviderInfo, ProviderKind};
    use std::sync::atomic::{AtomicUsize, Ordering as AtomicOrdering};

    struct SummaryTestProvider {
        failures: usize,
        attempts: AtomicUsize,
    }

    impl SummaryTestProvider {
        fn new(failures: usize) -> Self {
            Self {
                failures,
                attempts: AtomicUsize::new(0),
            }
        }

        fn attempts(&self) -> usize {
            self.attempts.load(AtomicOrdering::Relaxed)
        }
    }

    #[async_trait]
    impl LlmProvider for SummaryTestProvider {
        fn id(&self) -> &'static str {
            "summary-test"
        }

        fn info(&self) -> ProviderInfo {
            ProviderInfo {
                id: self.id().into(),
                label: "Summary test".into(),
                model: "deterministic".into(),
                kind: ProviderKind::LocalCli,
                structured_output: false,
            }
        }

        async fn is_available(&self) -> bool {
            true
        }

        async fn complete(&self, _request: &CompletionRequest) -> Result<String, LlmError> {
            let attempt = self.attempts.fetch_add(1, AtomicOrdering::Relaxed) + 1;
            if attempt <= self.failures {
                return Err(LlmError::BadJson {
                    provider: self.id().into(),
                    message: format!("invalid response {attempt}"),
                });
            }
            Ok(r#"{
                "title":"Plan listo",
                "overview":"Resumen válido",
                "keyPoints":[],
                "decisions":[],
                "actionItems":[],
                "openQuestions":[]
            }"#
            .into())
        }
    }

    fn session(source: VoiceSource, id: u64) -> VoiceSessionKey {
        VoiceSessionKey { source, id }
    }

    fn active_meeting(id: &str, ticks: u64) -> ActiveMeeting {
        ActiveMeeting {
            meeting: Meeting::new(MeetingMeta {
                id: id.into(),
                display_title: None,
                guild_id: 1,
                guild_name: "Servidor".into(),
                channel_id: 2,
                channel_name: "General".into(),
                started_at: Utc::now(),
                ended_at: None,
            }),
            segmenter: Segmenter::new(Default::default()),
            ticks,
            text_channel_id: 2,
            ending: false,
        }
    }

    #[test]
    fn a_fresh_engine_is_offline_with_no_meeting() {
        let (engine, _rx) = Engine::new(KualiConfig::default());
        assert_eq!(engine.status(), EngineStatus::Offline);
        assert!(engine.current_meeting().is_none());
    }

    #[test]
    fn only_transcription_target_changes_request_an_automatic_download() {
        let original = KualiConfig::default();

        let mut discord_only = original.clone();
        discord_only.discord.bot_token = "configured".into();
        assert!(!configured_model_target_changed(&original, &discord_only));

        let mut another_model = original.clone();
        another_model.whisper.model = WhisperModel::Tiny;
        assert!(configured_model_target_changed(&original, &another_model));

        let mut another_directory = original.clone();
        another_directory.whisper.models_directory = Some("/tmp/kuali-models".into());
        assert!(configured_model_target_changed(
            &original,
            &another_directory
        ));
    }

    #[tokio::test]
    async fn summary_generation_succeeds_on_the_third_and_final_attempt() {
        let (engine, _events) = Engine::new(KualiConfig::default());
        let provider = SummaryTestProvider::new(2);
        let id = format!("summary-retry-success-{}", uuid::Uuid::new_v4());
        let mut meeting = active_meeting(&id, 0).meeting;

        let summary = summarize_with_retries(&engine.inner, &mut meeting, &provider, "Spanish")
            .await
            .unwrap();

        assert_eq!(provider.attempts(), MAX_SUMMARY_ATTEMPTS);
        assert_eq!(summary.title, "Plan listo");
        assert_eq!(meeting.summary, Some(summary));
        kuali_store::delete(&id).unwrap();
    }

    #[tokio::test]
    async fn summary_generation_stops_after_three_invalid_responses() {
        let (engine, _events) = Engine::new(KualiConfig::default());
        let provider = SummaryTestProvider::new(usize::MAX);
        let mut meeting = active_meeting("summary-retry-failure", 0).meeting;

        let error = summarize_with_retries(&engine.inner, &mut meeting, &provider, "Spanish")
            .await
            .unwrap_err();

        assert_eq!(provider.attempts(), MAX_SUMMARY_ATTEMPTS);
        assert!(matches!(error, LlmError::BadJson { .. }));
        assert!(meeting.summary.is_none());
    }

    #[tokio::test]
    async fn an_unconfigured_model_never_creates_a_discord_delivery() {
        let (engine, _events) = Engine::new(KualiConfig::default());
        let mut config = KualiConfig::default();
        config.llm.preferred_provider = Some("provider-that-does-not-exist".into());
        config.discord.post_summary_to_channel = true;
        let mut meeting = active_meeting("summary-without-provider", 0).meeting;

        let error = summarize_and_sync(&engine.inner, &mut meeting, &config, 42)
            .await
            .unwrap_err();

        assert_eq!(
            error.failure_kind(),
            kuali_llm::LlmFailureKind::MissingConfiguration
        );
        assert!(meeting.discord_summary_delivery.is_none());
    }

    #[test]
    fn discord_summary_delivery_reuses_saved_messages_and_recovers_legacy_channels() {
        let mut discord = active_meeting("discord-delivery", 0).meeting;
        prepare_discord_summary_delivery(&mut discord, 42);
        assert_eq!(
            discord.discord_summary_delivery,
            Some(DiscordSummaryDelivery::pending(42))
        );

        discord.discord_summary_delivery = Some(DiscordSummaryDelivery::delivered(42, 99));
        prepare_discord_summary_delivery(&mut discord, 77);
        assert_eq!(
            discord.discord_summary_delivery,
            Some(DiscordSummaryDelivery::delivered(42, 99)),
            "a regenerated summary must keep editing the original card"
        );

        let mut legacy = active_meeting("legacy-discord", 0).meeting;
        prepare_discord_summary_delivery(&mut legacy, 0);
        assert_eq!(
            legacy.discord_summary_delivery,
            Some(DiscordSummaryDelivery::pending(legacy.meta.channel_id))
        );

        let mut meet = active_meeting("browser-meeting", 0).meeting;
        meet.meta.guild_name = "Google Meet".into();
        prepare_discord_summary_delivery(&mut meet, 0);
        assert_eq!(meet.discord_summary_delivery, None);
    }

    #[tokio::test]
    async fn disabled_summaries_never_reach_an_llm() {
        let mut config = KualiConfig::default();
        config.llm.summarize_on_leave = false;
        let (engine, _rx) = Engine::new(config);

        assert!(matches!(
            engine.resummarize("any-meeting").await,
            Err(EngineError::SummariesDisabled)
        ));
    }

    #[tokio::test]
    async fn connecting_without_a_token_says_what_is_missing() {
        let (engine, _rx) = Engine::new(KualiConfig::default());
        match engine.connect().await {
            Err(EngineError::Incomplete(what)) => assert!(what.contains("token")),
            other => panic!("expected Incomplete, got {other:?}"),
        }
    }

    #[test]
    fn the_tick_clock_tracks_the_audio_and_not_the_wall_clock() {
        let mut active = ActiveMeeting {
            meeting: Meeting::new(MeetingMeta {
                id: "m".into(),
                display_title: None,
                guild_id: 0,
                guild_name: String::new(),
                channel_id: 0,
                channel_name: String::new(),
                started_at: Utc::now(),
                ended_at: None,
            }),
            segmenter: Segmenter::new(Default::default()),
            ticks: 0,
            text_channel_id: 0,
            ending: false,
        };

        assert_eq!(active.now_ms(), 0);
        // Fifty 20 ms ticks equal one second of audio regardless of inference lag.
        active.ticks = 50;
        assert_eq!(active.now_ms(), 1_000);
        active.ticks = 3_000;
        assert_eq!(active.now_ms(), 60_000);
    }

    #[test]
    fn the_model_state_reflects_whether_the_weights_are_on_disk() {
        let (engine, _rx) = Engine::new(KualiConfig::default());
        // Startup may be `Absent` or `Ready` depending on disk state, but never
        // loaded in memory.
        assert!(matches!(
            engine.model_state(),
            ModelState::Absent | ModelState::Ready
        ));
    }

    #[tokio::test]
    async fn cancellation_is_available_only_while_a_model_is_downloading() {
        let root = std::env::temp_dir().join(format!(
            "kuali-engine-model-cancel-{}",
            uuid::Uuid::new_v4()
        ));
        let mut config = KualiConfig::default();
        config.whisper.models_directory = Some(root.clone());
        let (engine, _events) = Engine::new(config);
        let mut cancellation = engine.inner.model_download_cancellation.subscribe();

        assert!(!engine.cancel_model_download());
        *engine.inner.model_state.write() = ModelState::Downloading {
            model: WhisperModel::LargeV3,
            downloaded_bytes: 49_000_000,
            total_bytes: Some(3_095_033_483),
        };

        assert!(engine.cancel_model_download());
        cancellation.changed().await.unwrap();
        assert_eq!(*cancellation.borrow(), 1);

        std::fs::create_dir_all(&root).unwrap();
        std::fs::write(
            kuali_stt::model::partial_path(&root, WhisperModel::LargeV3),
            b"partial model",
        )
        .unwrap();
        std::fs::write(
            kuali_stt::vad_model_path(&root).with_extension("part"),
            b"partial vad",
        )
        .unwrap();
        cleanup_cancelled_download(&root, WhisperModel::LargeV3).await;
        assert!(!kuali_stt::model::partial_path(&root, WhisperModel::LargeV3).exists());
        assert!(!kuali_stt::vad_model_path(&root)
            .with_extension("part")
            .exists());
        std::fs::remove_dir_all(&root).unwrap();
    }

    #[tokio::test]
    async fn a_loaded_model_cannot_be_deleted_but_an_idle_one_can() {
        let root = std::env::temp_dir().join(format!(
            "kuali-engine-model-delete-{}",
            uuid::Uuid::new_v4()
        ));
        std::fs::create_dir_all(&root).unwrap();
        let model = WhisperModel::Tiny;
        let path = kuali_stt::model_path(&root, model);
        std::fs::write(&path, b"pesos de prueba").unwrap();
        let vad_path = kuali_stt::vad_model_path(&root);
        std::fs::write(&vad_path, b"silero").unwrap();

        let mut config = KualiConfig::default();
        config.whisper.models_directory = Some(root.clone());
        let (engine, _rx) = Engine::new(config);
        *engine.inner.loaded_model.write() = Some(model);

        assert!(matches!(
            engine.delete_model(model).await,
            Err(EngineError::ActiveModelDeletion)
        ));
        assert!(path.exists());

        *engine.inner.loaded_model.write() = None;
        assert_eq!(engine.delete_model(model).await.unwrap(), 15);
        assert!(!path.exists());
        assert!(vad_path.exists(), "borrar el último peso conserva Silero");
        std::fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn an_active_meeting_cannot_be_deleted() {
        let (engine, _rx) = Engine::new(KualiConfig::default());
        engine
            .inner
            .active
            .lock()
            .insert(session(VoiceSource::Discord, 1), active_meeting("live", 0));

        assert!(matches!(
            engine.delete_meeting("live"),
            Err(EngineError::ActiveMeetingDeletion)
        ));
    }

    #[tokio::test]
    async fn simultaneous_sessions_keep_independent_clocks() {
        let (engine, _rx) = Engine::new(KualiConfig::default());
        let discord = session(VoiceSource::Discord, 1);
        let web = session(VoiceSource::Web, 2);
        engine
            .inner
            .active
            .lock()
            .insert(discord, active_meeting("discord-live", 7));
        engine
            .inner
            .active
            .lock()
            .insert(web, active_meeting("web-live", 11));

        handle_voice_event(
            &engine.inner,
            VoiceSource::Discord,
            VoiceEvent::Session {
                session_id: discord.id,
                event: Box::new(VoiceEvent::Tick),
            },
        )
        .await;
        handle_voice_event(
            &engine.inner,
            VoiceSource::Web,
            VoiceEvent::Session {
                session_id: web.id,
                event: Box::new(VoiceEvent::Tick),
            },
        )
        .await;

        let active = engine.inner.active.lock();
        assert_eq!(active.get(&discord).unwrap().ticks, 8);
        assert_eq!(active.get(&web).unwrap().ticks, 12);
    }

    #[tokio::test]
    async fn a_slow_meeting_shutdown_does_not_pause_another_sessions_audio_loop() {
        let (engine, _events) = Engine::new(KualiConfig::default());
        let discord = session(VoiceSource::Discord, 31);
        let web = session(VoiceSource::Web, 32);
        let discord_id = format!("nonblocking-discord-{}", uuid::Uuid::new_v4());
        let web_id = format!("nonblocking-web-{}", uuid::Uuid::new_v4());
        {
            let mut active = engine.inner.active.lock();
            active.insert(discord, active_meeting(&discord_id, 20));
            active.insert(web, active_meeting(&web_id, 40));
        }

        // Simulate unfinished browser inference at hangup. The close event must
        // return without waiting for this channel.
        let (release, held) = tokio::sync::oneshot::channel::<()>();
        let mut web_jobs = JoinSet::new();
        web_jobs.spawn(async move {
            let _ = held.await;
        });
        engine
            .inner
            .transcriptions
            .lock()
            .await
            .insert(web_id.clone(), web_jobs);

        tokio::time::timeout(
            Duration::from_millis(100),
            handle_voice_event(
                &engine.inner,
                VoiceSource::Web,
                VoiceEvent::Session {
                    session_id: web.id,
                    event: Box::new(VoiceEvent::Disconnected),
                },
            ),
        )
        .await
        .expect("shutdown must not block the shared receiver");

        handle_voice_event(
            &engine.inner,
            VoiceSource::Discord,
            VoiceEvent::Session {
                session_id: discord.id,
                event: Box::new(VoiceEvent::Tick),
            },
        )
        .await;
        assert_eq!(engine.inner.active.lock().get(&discord).unwrap().ticks, 21);

        release.send(()).unwrap();
        tokio::time::timeout(Duration::from_secs(1), async {
            while engine.inner.active.lock().contains_key(&web) {
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
        })
        .await
        .expect("web meeting should finish after releasing its inference slot");

        finish_meeting(&engine.inner, discord).await;
        kuali_store::delete(&web_id).unwrap();
        kuali_store::delete(&discord_id).unwrap();
    }

    #[tokio::test]
    async fn a_second_platform_is_admitted_while_the_first_is_active() {
        let (engine, _events) = Engine::new(KualiConfig::default());
        engine.inner.active.lock().insert(
            session(VoiceSource::Discord, 1),
            active_meeting("discord-live", 0),
        );
        let (reply, decision) = tokio::sync::oneshot::channel();

        handle_voice_event(
            &engine.inner,
            VoiceSource::Web,
            VoiceEvent::ConnectionRequested {
                info: CallInfo {
                    guild_id: 9,
                    guild_name: "Google Meet".into(),
                    channel_id: 8,
                    channel_name: "abc-defg-hij".into(),
                    text_channel_id: 0,
                },
                reply,
            },
        )
        .await;

        decision.await.unwrap().unwrap();
        assert_eq!(
            engine.current_meeting().unwrap().meta.id,
            "discord-live",
            "pedir admisión no debe desalojar la primera"
        );
    }

    #[tokio::test]
    async fn ending_one_session_keeps_the_other_recording_and_the_model_active() {
        let (engine, _events) = Engine::new(KualiConfig::default());
        let discord = session(VoiceSource::Discord, 11);
        let web = session(VoiceSource::Web, 22);
        let discord_id = format!("parallel-discord-{}", uuid::Uuid::new_v4());
        let web_id = format!("parallel-web-{}", uuid::Uuid::new_v4());
        {
            let mut active = engine.inner.active.lock();
            active.insert(discord, active_meeting(&discord_id, 3));
            active.insert(web, active_meeting(&web_id, 5));
        }

        finish_meeting(&engine.inner, web).await;

        let remaining = engine.current_meetings();
        assert_eq!(remaining.len(), 1);
        assert_eq!(remaining[0].meta.id, discord_id);
        assert_eq!(engine.status(), EngineStatus::Recording);
        assert_eq!(engine.model_state(), ModelState::Active);
        assert!(kuali_store::load(&web_id).unwrap().meta.ended_at.is_some());

        finish_meeting(&engine.inner, discord).await;
        assert!(engine.current_meetings().is_empty());
        assert_eq!(engine.status(), EngineStatus::Watching);

        kuali_store::delete(&web_id).unwrap();
        kuali_store::delete(&discord_id).unwrap();
    }

    #[test]
    fn batch_deletion_validates_every_meeting_before_removing_any() {
        let (engine, _rx) = Engine::new(KualiConfig::default());
        let existing_id = format!("batch-existing-{}", uuid::Uuid::new_v4());
        let missing_id = format!("batch-missing-{}", uuid::Uuid::new_v4());
        let meeting = Meeting::new(MeetingMeta {
            id: existing_id.clone(),
            display_title: None,
            guild_id: 91,
            guild_name: "Servidor".into(),
            channel_id: 92,
            channel_name: "General".into(),
            started_at: Utc::now(),
            ended_at: Some(Utc::now()),
        });
        kuali_store::save(&meeting).unwrap();

        assert!(engine
            .delete_meetings(&[existing_id.clone(), missing_id])
            .is_err());
        assert!(kuali_store::meeting_dir(&existing_id).is_dir());

        kuali_store::delete(&existing_id).unwrap();
    }

    #[test]
    fn deleting_a_channel_folder_removes_only_that_channels_meetings() {
        let (engine, _rx) = Engine::new(KualiConfig::default());
        let suffix = uuid::Uuid::new_v4();
        let guild_id = suffix.as_u128() as u64;
        let channel_id = (suffix.as_u128() >> 64) as u64;
        let first_id = format!("folder-first-{suffix}");
        let second_id = format!("folder-second-{suffix}");
        let other_id = format!("folder-other-{suffix}");

        for (id, channel) in [
            (&first_id, channel_id),
            (&second_id, channel_id),
            (&other_id, channel_id.wrapping_add(1)),
        ] {
            kuali_store::save(&Meeting::new(MeetingMeta {
                id: id.clone(),
                display_title: None,
                guild_id,
                guild_name: "Servidor".into(),
                channel_id: channel,
                channel_name: "Canal".into(),
                started_at: Utc::now(),
                ended_at: Some(Utc::now()),
            }))
            .unwrap();
        }

        assert_eq!(engine.delete_channel_meetings(&first_id).unwrap(), 2);
        assert!(!kuali_store::meeting_dir(&first_id).exists());
        assert!(!kuali_store::meeting_dir(&second_id).exists());
        assert!(kuali_store::meeting_dir(&other_id).is_dir());

        kuali_store::delete(&other_id).unwrap();
    }

    #[tokio::test]
    async fn leaving_a_call_finishes_it_without_disconnect_from_discord() {
        let (engine, _rx) = Engine::new(KualiConfig::default());
        let id = format!("leave-call-{}", uuid::Uuid::new_v4());
        engine
            .inner
            .active
            .lock()
            .insert(session(VoiceSource::Discord, 1), active_meeting(&id, 0));

        engine.leave_call().await.unwrap();

        assert!(engine.current_meeting().is_none());
        assert_eq!(engine.status(), EngineStatus::Watching);
        assert!(kuali_store::load(&id).unwrap().meta.ended_at.is_some());
        kuali_store::delete(&id).unwrap();
    }

    #[test]
    fn a_meeting_request_returns_live_data_only_to_its_guild() {
        let (engine, _rx) = Engine::new(KualiConfig::default());
        let mut meeting = Meeting::new(MeetingMeta {
            id: "live-transcript".into(),
            display_title: None,
            guild_id: 42,
            guild_name: "Servidor".into(),
            channel_id: 2,
            channel_name: "General".into(),
            started_at: Utc::now(),
            ended_at: None,
        });
        meeting.upsert_speaker(Speaker {
            user_id: 7,
            source_id: None,
            audio_kind: None,
            display_name: "Ana".into(),
            username: "ana".into(),
            avatar_url: None,
            color: color_for(7).into(),
            is_bot: false,
        });
        meeting.push_utterance(Utterance {
            id: "u1".into(),
            speaker_id: 7,
            start_ms: 1_000,
            end_ms: 2_000,
            text: "Texto completo".into(),
            confidence: Some(0.9),
        });
        engine.inner.active.lock().insert(
            session(VoiceSource::Discord, 1),
            ActiveMeeting {
                meeting,
                segmenter: Segmenter::new(Default::default()),
                ticks: 100,
                text_channel_id: 2,
                ending: false,
            },
        );

        let meeting = meeting_for_discord(&engine.inner, "live-transcript", 42).unwrap();
        assert!(meeting
            .transcript_text()
            .contains("[00:01] Ana: Texto completo"));
        assert_eq!(meeting.meta.id, "live-transcript");
        assert!(meeting_for_discord(&engine.inner, "live-transcript", 99).is_err());
    }
}
