//! State machine that turns joining a call into a transcript and action items.
//!
//! This is the only layer connecting Discord, browser meetings, Whisper, LLMs,
//! and storage; every other subsystem remains isolated.

use std::collections::{HashMap, HashSet};
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Weak};
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
const MEMORY_RETRY_DELAYS_SECS: [u64; 4] = [5, 30, 120, 300];

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
    #[error("the meeting index is unavailable, so past meetings cannot be searched")]
    MemoryUnavailable,
    #[error("questions about past meetings are turned off in Settings")]
    QuestionsDisabled,
    #[error("the model that answers questions has not finished downloading")]
    QuestionModelMissing,
    #[error("{0} meeting passages are still waiting to be indexed")]
    QuestionIndexPending(usize),
    #[error("the meeting index is being updated; try again in a moment")]
    QuestionIndexUpdating,
    #[error("one or more finished meetings are missing from the search index")]
    QuestionIndexOutOfSync,
    #[error(transparent)]
    Memory(Box<kuali_memory::MemoryError>),
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
    kuali_memory::MemoryError => Memory,
}

/// User-facing state of one meeting in the derived search index.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub enum MeetingIndexState {
    Indexed,
    Pending,
    NotIndexed,
    Unavailable,
}

/// Counts behind [`MeetingIndexState`], serialized directly for the desktop UI.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct MeetingIndexStatus {
    pub state: MeetingIndexState,
    pub passages: usize,
    pub pending_passages: usize,
}

impl MeetingIndexStatus {
    fn unavailable() -> Self {
        Self {
            state: MeetingIndexState::Unavailable,
            passages: 0,
            pending_passages: 0,
        }
    }

    fn from_stats(questions_enabled: bool, stats: Option<kuali_memory::MeetingIndexStats>) -> Self {
        let Some(stats) = stats else {
            return Self {
                state: MeetingIndexState::NotIndexed,
                passages: 0,
                pending_passages: 0,
            };
        };
        Self {
            state: if questions_enabled && stats.pending_passages > 0 {
                MeetingIndexState::Pending
            } else {
                MeetingIndexState::Indexed
            },
            passages: stats.passages,
            pending_passages: stats.pending_passages,
        }
    }
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
    /// Successful Discord gateway state, kept separately from the aggregate
    /// engine status because web meetings can be active without Discord.
    discord_connected: AtomicBool,
    /// Final transcription, summaries and completion delivery that must finish
    /// before an application update may restart Kuali.
    post_processing: AtomicUsize,
    /// Startup catch-up and bulk embedding can temporarily have no pending rows
    /// even though their store snapshot has not been committed yet.
    memory_maintenance: AtomicUsize,
    /// Set only after a complete store-to-index synchronization succeeds. This
    /// starts false on every launch so an old-but-present row can never be
    /// mistaken for a current one after an interrupted previous process.
    memory_consistent: AtomicBool,
    /// Meetings whose authoritative JSON changed and whose replacement index
    /// has not yet committed successfully.
    memory_dirty: Mutex<HashMap<String, u64>>,
    /// Monotonic ticket prevents an older detached job from clearing a newer
    /// mutation of the same meeting.
    memory_generation: AtomicU64,
    /// At most one detached repair loop may reconcile the store and index.
    /// The loop sleeps without retaining `Inner`, so it cannot keep the app
    /// alive after every Engine handle has gone away.
    memory_retry_scheduled: AtomicBool,
    /// Serializes metadata read-modify-write sequences. Folder automation and
    /// manual library edits can otherwise overwrite one another while an LLM
    /// result is being applied.
    metadata_mutation: Mutex<()>,
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
    /// Searchable index of finished meetings, behind a blocking lock because
    /// SQLite is blocking. `None` when the index could not be opened: asking
    /// then reports a clear failure while recording, transcribing and
    /// summarizing carry on untouched.
    memory: Mutex<Option<kuali_memory::Memory>>,
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

/// Opens the meeting index, degrading to `None` instead of failing startup.
///
/// The index is derived from `meetings/` and can always be rebuilt, so losing it
/// costs a re-sync. Refusing to start Kuali because questions are unavailable
/// would trade recording, transcription and summaries for one feature.
fn open_memory() -> Option<kuali_memory::Memory> {
    match kuali_memory::Memory::open() {
        Ok(memory) => Some(memory),
        Err(error) => {
            tracing::error!(
                %error,
                "no pude abrir el índice de reuniones; las preguntas quedarán desactivadas"
            );
            None
        }
    }
}

fn mark_memory_dirty(inner: &Arc<Inner>, meeting_id: &str) -> u64 {
    let ticket = inner.memory_generation.fetch_add(1, Ordering::AcqRel) + 1;
    inner
        .memory_dirty
        .lock()
        .insert(meeting_id.to_string(), ticket);
    inner.emit(KualiEvent::QuestionsStatusChanged);
    ticket
}

fn clear_memory_dirty(inner: &Arc<Inner>, meeting_id: &str, ticket: u64) {
    let removed = {
        let mut dirty = inner.memory_dirty.lock();
        if dirty.get(meeting_id).copied() == Some(ticket) {
            dirty.remove(meeting_id);
            true
        } else {
            false
        }
    };
    if removed {
        inner.emit(KualiEvent::QuestionsStatusChanged);
    }
}

fn memory_snapshot_is_current(inner: &Inner) -> bool {
    inner.memory_consistent.load(Ordering::Acquire) && inner.memory_dirty.lock().is_empty()
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum RememberOutcome {
    Complete,
    TextCurrent,
    Deleted,
    Failed,
}

fn reconcile_memory_dirty(inner: &Arc<Inner>, memory: &kuali_memory::Memory) {
    let candidates: Vec<(String, u64)> = inner
        .memory_dirty
        .lock()
        .iter()
        .map(|(meeting_id, ticket)| (meeting_id.clone(), *ticket))
        .collect();
    let repaired: Vec<(String, u64)> = candidates
        .into_iter()
        .filter(
            |(meeting_id, _)| match memory.meeting_store_is_current(meeting_id) {
                Ok(true) => true,
                Ok(false) => {
                    // A deletion is also reconciled once both authoritative JSON
                    // and its derived row are absent. Otherwise a failed delete
                    // could leave an unresolvable dirty ticket retrying forever.
                    matches!(
                        kuali_store::load(meeting_id),
                        Err(kuali_store::StoreError::NotFound(_))
                    ) && memory
                        .meeting_index_stats(meeting_id)
                        .is_ok_and(|stats| stats.is_none())
                }
                Err(_) => false,
            },
        )
        .collect();
    if repaired.is_empty() {
        return;
    }
    let mut dirty = inner.memory_dirty.lock();
    for (meeting_id, ticket) in repaired {
        if dirty.get(&meeting_id).copied() == Some(ticket) {
            dirty.remove(&meeting_id);
        }
    }
}

/// Whether a later store/index pass can still make useful progress.
///
/// Pending vectors only keep the repair loop alive when questions are enabled
/// and their weights are actually present. Setup owns downloading the model;
/// once it exists, transient SQLite/model failures are retried automatically.
fn memory_retry_needed(inner: &Arc<Inner>) -> bool {
    if !inner.memory_consistent.load(Ordering::Acquire) || !inner.memory_dirty.lock().is_empty() {
        return true;
    }

    let config = inner.config.read().clone();
    if !config.llm.meeting_questions {
        return false;
    }
    let models_dir = crate::questions::models_dir_for(&config.whisper);
    if !kuali_memory::embed::is_downloaded(&models_dir) {
        return false;
    }

    let guard = inner.memory.lock();
    match guard.as_ref() {
        Some(memory) => memory
            .pending_embeddings()
            .map_or(true, |pending| pending > 0),
        None => true,
    }
}

/// Performs one authoritative store-to-index repair pass.
///
/// Returns `true` when a transient failure or pending installed-model work
/// remains. Every pass owns a short maintenance guard; the backoff between
/// attempts does not block questions, updates, or application shutdown.
fn sync_memory_once(inner: &Arc<Inner>) -> bool {
    // Invalidate any question that already captured passages. Even a complete
    // sync can replace their text/ranking while the provider is still thinking.
    inner.memory_generation.fetch_add(1, Ordering::AcqRel);
    inner.memory_consistent.store(false, Ordering::Release);
    let maintenance = MemoryMaintenanceGuard::new(inner);

    let snapshot = {
        // Store metadata writes are multi-file operations. Serialize the full
        // read/reconcile pass with them so a ticket cannot be cleared against
        // the old JSON while a newer save is still in flight.
        let _metadata = inner.metadata_mutation.lock();
        let mut guard = inner.memory.lock();
        if guard.is_none() {
            *guard = open_memory();
        }
        let Some(memory) = guard.as_mut() else {
            drop(maintenance);
            return true;
        };

        let (synced_ids, sync_complete) = match memory.sync_from_store() {
            Ok(report) => {
                tracing::info!(
                    indexed = report.indexed,
                    unchanged = report.unchanged,
                    removed = report.removed,
                    unreadable = report.unreadable,
                    "índice de reuniones sincronizado"
                );
                reconcile_memory_dirty(inner, memory);
                (report.indexed_meeting_ids, report.unreadable == 0)
            }
            Err(error) => {
                tracing::warn!(%error, "no pude sincronizar el índice de reuniones");
                (Vec::new(), false)
            }
        };
        let pending = match memory.pending_embeddings() {
            Ok(pending) => pending,
            Err(error) => {
                tracing::warn!(%error, "no pude revisar los embeddings pendientes");
                drop(guard);
                drop(maintenance);
                return true;
            }
        };
        let pending_ids = memory
            .pending_embedding_meeting_ids()
            .unwrap_or_else(|error| {
                tracing::warn!(%error, "no pude identificar las reuniones pendientes");
                Vec::new()
            });
        // Publish textual consistency while still owning the same index lock
        // used by deletion and manual repair. A later failed mutation may set
        // this false, and an older sync must never overwrite that verdict.
        inner
            .memory_consistent
            .store(sync_complete, Ordering::Release);
        (pending, synced_ids, pending_ids)
    };

    let (pending, synced_ids, pending_ids) = snapshot;
    let mut changed_ids = synced_ids;
    changed_ids.extend(pending_ids);

    if pending > 0 {
        let config = inner.config.read().clone();
        let models_dir = crate::questions::models_dir_for(&config.whisper);
        if config.llm.meeting_questions && kuali_memory::embed::is_downloaded(&models_dir) {
            match kuali_memory::embed::Embedder::load(&models_dir) {
                Ok(mut embedder) => {
                    let mut guard = inner.memory.lock();
                    if let Some(memory) = guard.as_mut() {
                        match memory.embed_pending(&mut embedder, |_, _| true) {
                            Ok(embedded) => tracing::info!(
                                embedded,
                                "embeddings pendientes de reuniones completados"
                            ),
                            Err(error) => tracing::warn!(
                                %error,
                                "no pude completar los embeddings pendientes"
                            ),
                        }
                    }
                }
                Err(error) => tracing::warn!(
                    %error,
                    "no pude cargar el modelo para completar el índice"
                ),
            }
        }
    }

    drop(maintenance);
    changed_ids.sort();
    changed_ids.dedup();
    for meeting_id in changed_ids {
        inner.emit(KualiEvent::MeetingIndexChanged { meeting_id });
    }

    memory_retry_needed(inner)
}

/// Starts one deduplicated repair worker, optionally without the initial delay.
///
/// The worker keeps retrying with capped backoff for as long as the authoritative
/// store and derived index differ. It retains only a `Weak` reference while
/// sleeping, so this eventual-repair guarantee never prolongs application life.
fn schedule_memory_sync(inner: &Arc<Inner>, immediate: bool) {
    if inner
        .memory_retry_scheduled
        .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
        .is_err()
    {
        return;
    }

    let weak: Weak<Inner> = Arc::downgrade(inner);
    std::thread::spawn(move || {
        let mut retry_index = 0usize;
        let mut wait_before_attempt = !immediate;
        loop {
            if wait_before_attempt {
                let seconds =
                    MEMORY_RETRY_DELAYS_SECS[retry_index.min(MEMORY_RETRY_DELAYS_SECS.len() - 1)];
                std::thread::sleep(Duration::from_secs(seconds));
                retry_index = retry_index.saturating_add(1);
            }

            let Some(inner) = weak.upgrade() else {
                return;
            };
            let retry = sync_memory_once(&inner);
            drop(inner);
            if !retry {
                break;
            }
            wait_before_attempt = true;
        }

        let Some(inner) = weak.upgrade() else {
            return;
        };
        inner.memory_retry_scheduled.store(false, Ordering::Release);
        // Close the only race in the deduplication flag: a mutation may have
        // requested work while the old worker still looked active.
        if memory_retry_needed(&inner) {
            schedule_memory_sync(&inner, false);
        }
    });
}

/// Adds a finished meeting to the index.
///
/// Deliberately not awaited and never fatal: the meeting is already saved, and
/// an index that missed one meeting is repaired by the next
/// [`Engine::sync_memory`]. Delivery of the summary must not wait on search.
fn remember(inner: &Arc<Inner>, meeting: &Meeting) {
    let dirty_ticket = mark_memory_dirty(inner, &meeting.meta.id);
    let processing = PostProcessingGuard::new(inner);
    let maintenance = MemoryMaintenanceGuard::new(inner);
    let inner = Arc::clone(inner);
    let meeting = meeting.clone();
    let meeting_id = meeting.meta.id.clone();
    tokio::task::spawn_blocking(move || {
        let config = inner.config.read().clone();
        let models_dir = crate::questions::models_dir_for(&config.whisper);
        // Loaded only for this meeting and then dropped. A short burst handles
        // momentary failures quickly; the deduplicated repair loop below keeps
        // retrying longer outages with capped backoff.
        let mut embedder = None;
        let mut outcome = RememberOutcome::Failed;
        for attempt in 0..3 {
            if config.llm.meeting_questions && embedder.is_none() {
                embedder = kuali_memory::embed::Embedder::load(&models_dir)
                    .map_err(|error| {
                        tracing::warn!(attempt = attempt + 1, %error, "no pude cargar el modelo para indexar la reunión");
                    })
                    .ok();
            }

            outcome = {
                let _metadata = inner.metadata_mutation.lock();
                let mut guard = inner.memory.lock();
                match guard.as_mut() {
                    Some(memory) => {
                        // Disk is authoritative. In particular, two quick task
                        // toggles can queue two detached remembers; loading only
                        // after taking the single index lock prevents an older
                        // clone from being committed after a newer one.
                        match kuali_store::load(&meeting_id) {
                            Ok(authoritative) => match memory.index(&authoritative) {
                                Err(error) => {
                                    tracing::warn!(
                                        meeting_id = %meeting_id,
                                        attempt = attempt + 1,
                                        %error,
                                        "no pude escribir la reunión en el índice"
                                    );
                                    RememberOutcome::Failed
                                }
                                Ok(_) if !config.llm.meeting_questions => RememberOutcome::Complete,
                                Ok(_) => match embedder.as_mut() {
                                    Some(embedder) => {
                                        match memory.embed_meeting_pending(&meeting_id, embedder) {
                                            Ok(_) => RememberOutcome::Complete,
                                            Err(error) => {
                                                tracing::warn!(
                                                    meeting_id = %meeting_id,
                                                    attempt = attempt + 1,
                                                    %error,
                                                    "el texto quedó indexado, pero faltan sus embeddings"
                                                );
                                                RememberOutcome::TextCurrent
                                            }
                                        }
                                    }
                                    None => RememberOutcome::TextCurrent,
                                },
                            },
                            Err(kuali_store::StoreError::NotFound(_)) => {
                                tracing::info!(
                                    meeting_id = %meeting_id,
                                    "la reunión se borró antes de llegar al índice"
                                );
                                match memory.forget(&meeting_id) {
                                    Ok(()) => RememberOutcome::Deleted,
                                    Err(error) => {
                                        tracing::warn!(
                                            meeting_id = %meeting_id,
                                            %error,
                                            "no pude quitar del índice la reunión borrada"
                                        );
                                        RememberOutcome::Failed
                                    }
                                }
                            }
                            Err(error) => {
                                tracing::warn!(
                                    meeting_id = %meeting_id,
                                    %error,
                                    "no pude recargar la reunión autoritativa antes de indexarla"
                                );
                                RememberOutcome::Failed
                            }
                        }
                    }
                    None => RememberOutcome::Failed,
                }
            };

            if matches!(
                outcome,
                RememberOutcome::Complete | RememberOutcome::Deleted
            ) {
                break;
            }
            if attempt < 2 {
                std::thread::sleep(Duration::from_millis(match attempt {
                    0 => 250,
                    _ => 1_500,
                }));
            }
        }

        if !matches!(outcome, RememberOutcome::Failed) {
            clear_memory_dirty(&inner, &meeting_id, dirty_ticket);
        }
        let target_succeeded = outcome == RememberOutcome::Complete;
        // This marks completion of the target attempt, not success. It is sent
        // before historical catch-up, with the SQLite mutex released, so the UI
        // can query this meeting immediately.
        // Readiness must observe the completed target before the refresh event.
        drop(maintenance);
        drop(processing);
        inner.emit(KualiEvent::MeetingIndexChanged {
            meeting_id: meeting_id.clone(),
        });
        if matches!(
            outcome,
            RememberOutcome::TextCurrent | RememberOutcome::Failed
        ) {
            schedule_memory_sync(&inner, false);
        }

        // A successful target gets priority. Reuse the loaded model afterwards
        // to repair old passages, but never make that backlog delay the target's
        // event or keep application updates waiting.
        if target_succeeded {
            if let Some(embedder) = embedder.as_mut() {
                let catchup_maintenance = MemoryMaintenanceGuard::new(&inner);
                let changed_ids = {
                    let mut guard = inner.memory.lock();
                    match guard.as_mut() {
                        Some(memory) => {
                            let ids = memory.pending_embedding_meeting_ids().unwrap_or_default();
                            if let Err(error) = memory.embed_pending(embedder, |_, _| true) {
                                tracing::warn!(
                                    %error,
                                    "no pude completar los embeddings históricos pendientes"
                                );
                            }
                            ids
                        }
                        None => Vec::new(),
                    }
                };
                drop(catchup_maintenance);
                for meeting_id in changed_ids {
                    inner.emit(KualiEvent::MeetingIndexChanged { meeting_id });
                }
            }
        }
        if memory_retry_needed(&inner) {
            schedule_memory_sync(&inner, false);
        }
    });
}

/// Removes meetings from the index as they leave the library.
///
/// Synchronous and inline, unlike indexing: a deleted meeting must not survive
/// in an answer, so this finishes before the deletion is reported as done.
fn forget_meetings(inner: &Arc<Inner>, ids: impl IntoIterator<Item = String>) {
    let ids: Vec<String> = ids.into_iter().collect();
    {
        let mut dirty = inner.memory_dirty.lock();
        for id in &ids {
            dirty.remove(id);
        }
    }
    let mut guard = inner.memory.lock();
    let Some(memory) = guard.as_mut() else {
        inner.memory_consistent.store(false, Ordering::Release);
        inner.emit(KualiEvent::QuestionsStatusChanged);
        schedule_memory_sync(inner, false);
        return;
    };
    let mut retry = false;
    for id in ids {
        if let Err(error) = memory.forget(&id) {
            tracing::warn!(meeting_id = %id, %error, "no pude quitar la reunión del índice");
            inner.memory_consistent.store(false, Ordering::Release);
            retry = true;
        }
    }
    drop(guard);
    inner.emit(KualiEvent::QuestionsStatusChanged);
    if retry {
        schedule_memory_sync(inner, false);
    }
}

/// Reloads metadata mutation targets from the authoritative store before
/// scheduling their derived-index replacement. Keeping this outside the store
/// mutation lock lets a later edit win: [`remember`] reloads once more while it
/// owns the SQLite lock and therefore cannot commit an older clone last.
fn remember_stored_meetings(inner: &Arc<Inner>, ids: impl IntoIterator<Item = String>) {
    let mut seen = HashSet::new();
    for meeting_id in ids {
        if !seen.insert(meeting_id.clone()) {
            continue;
        }
        let loaded = {
            let _metadata = inner.metadata_mutation.lock();
            let loaded = kuali_store::load(&meeting_id);
            if let Ok(meeting) = &loaded {
                if let Some(active) = inner
                    .active
                    .lock()
                    .values_mut()
                    .find(|active| active.meeting.meta.id == meeting_id)
                {
                    active.meeting.meta.tags = meeting.meta.tags.clone();
                    active.meeting.meta.folder = meeting.meta.folder.clone();
                }
            }
            loaded
        };
        match loaded {
            Ok(meeting) => {
                remember(inner, &meeting);
            }
            Err(kuali_store::StoreError::NotFound(_)) => {
                // A concurrent library deletion is authoritative. Clear both
                // the stale index row and the dirty marker instead of leaving
                // questions disabled forever for an ID that no longer exists.
                forget_meetings(inner, [meeting_id]);
            }
            Err(error) => {
                tracing::warn!(
                    meeting_id = %meeting_id,
                    %error,
                    "no pude recargar la reunión después de cambiar sus metadatos"
                );
                inner.emit(KualiEvent::error("store", error));
                schedule_memory_sync(inner, false);
            }
        }
    }
}

fn mark_meetings_dirty(inner: &Arc<Inner>, ids: &[String]) {
    let mut seen = HashSet::new();
    for meeting_id in ids {
        if seen.insert(meeting_id.as_str()) {
            mark_memory_dirty(inner, meeting_id);
        }
    }
}

fn configured_model_target_changed(previous: &KualiConfig, next: &KualiConfig) -> bool {
    previous.whisper.model != next.whisper.model
        || previous.whisper.resolved_models_directory() != next.whisper.resolved_models_directory()
}

fn replacement_model_after_deletion(
    models_dir: &std::path::Path,
    deleted: WhisperModel,
) -> WhisperModel {
    WhisperModel::SELECTABLE
        .iter()
        .copied()
        .find(|candidate| *candidate != deleted && kuali_stt::is_downloaded(models_dir, *candidate))
        .unwrap_or(WhisperModel::LargeV3TurboQ5)
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
            discord_connected: AtomicBool::new(false),
            post_processing: AtomicUsize::new(0),
            memory_maintenance: AtomicUsize::new(0),
            memory_consistent: AtomicBool::new(false),
            memory_dirty: Mutex::new(HashMap::new()),
            memory_generation: AtomicU64::new(0),
            memory_retry_scheduled: AtomicBool::new(false),
            metadata_mutation: Mutex::new(()),
            stt: SttWorker::spawn(),
            transcriptions: AsyncMutex::new(HashMap::new()),
            previews_in_flight: Mutex::new(HashSet::new()),
            closed_segments: Mutex::new(HashSet::new()),
            model_download: AsyncMutex::new(()),
            model_download_cancellation,
            discord: AsyncMutex::new(None),
            memory: Mutex::new(open_memory()),
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

    pub fn discord_connected(&self) -> bool {
        self.inner.discord_connected.load(Ordering::Acquire)
    }

    /// Updates are allowed only when no capture, transcription, model work or
    /// meeting post-processing can be interrupted by the restart.
    pub fn safe_for_update(&self) -> bool {
        self.inner.active.lock().is_empty()
            && self.inner.post_processing.load(Ordering::Acquire) == 0
            && self.inner.memory_maintenance.load(Ordering::Acquire) == 0
            && matches!(
                self.status(),
                EngineStatus::Offline | EngineStatus::Watching
            )
            && matches!(
                self.model_state(),
                ModelState::Absent | ModelState::Ready | ModelState::Failed { .. }
            )
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
        self.inner.discord_connected.store(true, Ordering::Release);

        if self.inner.active.lock().is_empty() {
            self.inner.set_status(EngineStatus::Watching);
        }
        Ok(())
    }

    /// Disconnects from Discord after closing any active meeting cleanly.
    pub async fn disconnect(&self) {
        self.inner.discord_connected.store(false, Ordering::Release);
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
        let mut config = self.config();
        if config.whisper.model == model {
            config.whisper.model = replacement_model_after_deletion(&models_dir, model);
            // Persist the safe replacement before removing the weight. A disk
            // error can leave an extra model installed, but never a saved
            // selection stranded on a file Kuali already deleted.
            kuali_core::paths::save_config(&config)?;
            *self.inner.config.write() = config;
        }
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

    /// Replaces manual tags and immediately invalidates/rebuilds the meeting's
    /// derived search passages. The dirty marker closes the interval between
    /// the JSON write and the asynchronous index commit.
    pub fn set_meeting_tags(
        &self,
        id: &str,
        tags: Vec<String>,
    ) -> Result<Vec<String>, EngineError> {
        let affected = vec![id.to_string()];
        let result = {
            let _metadata = self.inner.metadata_mutation.lock();
            // Validate before marking an ID that can never be repaired.
            kuali_store::load(id)?;
            mark_meetings_dirty(&self.inner, &affected);
            kuali_store::set_tags(id, tags)
        };
        match result {
            Ok(tags) => {
                remember_stored_meetings(&self.inner, affected);
                Ok(tags)
            }
            Err(error) => {
                schedule_memory_sync(&self.inner, false);
                Err(error.into())
            }
        }
    }

    /// Creates a catalog-only folder under the same mutation lock used by LLM
    /// organization. With no meetings assigned there is no derived index work.
    pub fn create_folder(&self, name: &str) -> Result<Vec<String>, EngineError> {
        let _metadata = self.inner.metadata_mutation.lock();
        Ok(kuali_store::create_folder(name)?)
    }

    /// Renames a folder and rebuilds every meeting whose searchable metadata
    /// changed. Targets are captured before the store mutates them because the
    /// old folder name no longer exists afterwards.
    pub fn rename_folder(&self, from: &str, to: &str) -> Result<Vec<String>, EngineError> {
        let (affected, result) = {
            let _metadata = self.inner.metadata_mutation.lock();
            let affected: Vec<String> = kuali_store::list()?
                .into_iter()
                .filter(|meta| {
                    meta.folder
                        .as_deref()
                        .is_some_and(|folder| folder.eq_ignore_ascii_case(from))
                })
                .map(|meta| meta.id)
                .collect();
            mark_meetings_dirty(&self.inner, &affected);
            let result = kuali_store::rename_folder(from, to);
            (affected, result)
        };
        match result {
            Ok(folders) => {
                remember_stored_meetings(&self.inner, affected);
                Ok(folders)
            }
            Err(error) => {
                schedule_memory_sync(&self.inner, false);
                Err(error.into())
            }
        }
    }

    /// Deletes only the folder and rebuilds the meetings returned to the
    /// unfiled group. Meeting contents remain untouched.
    pub fn delete_folder(&self, name: &str) -> Result<Vec<String>, EngineError> {
        let (affected, result) = {
            let _metadata = self.inner.metadata_mutation.lock();
            let affected: Vec<String> = kuali_store::list()?
                .into_iter()
                .filter(|meta| {
                    meta.folder
                        .as_deref()
                        .is_some_and(|folder| folder.eq_ignore_ascii_case(name))
                })
                .map(|meta| meta.id)
                .collect();
            mark_meetings_dirty(&self.inner, &affected);
            let result = kuali_store::delete_folder(name);
            (affected, result)
        };
        match result {
            Ok(folders) => {
                remember_stored_meetings(&self.inner, affected);
                Ok(folders)
            }
            Err(error) => {
                schedule_memory_sync(&self.inner, false);
                Err(error.into())
            }
        }
    }

    /// Moves meetings into or out of a folder and refreshes their searchable
    /// links. Existing targets are marked dirty before the first store write so
    /// a partially successful batch can never be mistaken for a current index.
    pub fn set_meeting_folder(
        &self,
        ids: &[String],
        folder: Option<&str>,
    ) -> Result<(), EngineError> {
        let (affected, result) = {
            let _metadata = self.inner.metadata_mutation.lock();
            let requested: HashSet<&str> = ids.iter().map(String::as_str).collect();
            let affected: Vec<String> = kuali_store::list()?
                .into_iter()
                .filter(|meta| requested.contains(meta.id.as_str()))
                .map(|meta| meta.id)
                .collect();
            mark_meetings_dirty(&self.inner, &affected);
            let result = kuali_store::set_folder(ids, folder);
            (affected, result)
        };
        match result {
            Ok(()) => {
                remember_stored_meetings(&self.inner, affected);
                Ok(())
            }
            Err(error) => {
                schedule_memory_sync(&self.inner, false);
                Err(error.into())
            }
        }
    }

    /// Returns the current derived index state for one meeting.
    ///
    /// A missing index row is different from an unavailable index: the former
    /// can be repaired by reindexing this meeting, while the latter means the
    /// SQLite index itself could not be opened or queried.
    pub fn meeting_index_status(&self, id: &str) -> MeetingIndexStatus {
        let questions_enabled = self.inner.config.read().llm.meeting_questions;
        if self.inner.memory_dirty.lock().contains_key(id) {
            return MeetingIndexStatus::from_stats(questions_enabled, None);
        }
        let guard = self.inner.memory.lock();
        let Some(memory) = guard.as_ref() else {
            return MeetingIndexStatus::unavailable();
        };
        match memory.meeting_store_is_current(id) {
            Ok(true) => {}
            Ok(false) => return MeetingIndexStatus::from_stats(questions_enabled, None),
            Err(error) => {
                tracing::warn!(meeting_id = %id, %error, "no pude comparar la reunión con su índice");
                return MeetingIndexStatus::unavailable();
            }
        }
        match memory.meeting_index_stats(id) {
            Ok(stats) => MeetingIndexStatus::from_stats(questions_enabled, stats),
            Err(error) => {
                tracing::warn!(meeting_id = %id, %error, "no pude consultar el estado del índice");
                MeetingIndexStatus::unavailable()
            }
        }
    }

    /// Forces a transactional textual rebuild from the authoritative store.
    ///
    /// With an available model, vectors are staged before one atomic replacement
    /// transaction, so failed inference preserves an existing healthy index. An
    /// absent meeting still receives a textual index and reports `pending`; when
    /// weights themselves are absent, the textual rebuild also proceeds alone.
    pub async fn reindex_meeting(&self, id: &str) -> Result<MeetingIndexStatus, EngineError> {
        let processing = PostProcessingGuard::new(&self.inner);
        let maintenance = MemoryMaintenanceGuard::new(&self.inner);
        let inner = Arc::clone(&self.inner);
        let meeting_id = id.to_string();
        tokio::task::spawn_blocking(move || {
            let _processing = processing;
            let maintenance = maintenance;
            let result = (|| {
                let config = inner.config.read().clone();
                let models_dir = crate::questions::models_dir_for(&config.whisper);
                let mut embedder_load_error = None;
                let mut embedder = if config.llm.meeting_questions
                    && kuali_memory::embed::is_downloaded(&models_dir)
                {
                    match kuali_memory::embed::Embedder::load(&models_dir) {
                        Ok(embedder) => Some(embedder),
                        Err(error) => {
                            tracing::warn!(
                                meeting_id = %meeting_id,
                                %error,
                                "no pude cargar el modelo para la reindexación manual"
                            );
                            embedder_load_error = Some(error);
                            None
                        }
                    }
                } else {
                    None
                };

                let _metadata = inner.metadata_mutation.lock();
                let mut guard = inner.memory.lock();
                let Some(memory) = guard.as_mut() else {
                    return Ok(MeetingIndexStatus::unavailable());
                };
                // Load only after taking the same mutex used by deletion's
                // `forget`. Whichever operation wins determines the final
                // index state; an earlier clone can never resurrect a meeting
                // whose delete already completed.
                let existing_ticket = inner.memory_dirty.lock().get(&meeting_id).copied();
                let meeting = match kuali_store::load(&meeting_id) {
                    Ok(meeting) => meeting,
                    Err(error @ kuali_store::StoreError::NotFound(_)) => {
                        if let Err(forget_error) = memory.forget(&meeting_id) {
                            inner.memory_consistent.store(false, Ordering::Release);
                            tracing::warn!(
                                meeting_id = %meeting_id,
                                %forget_error,
                                "no pude quitar una reunión borrada durante la reindexación"
                            );
                        }
                        if let Some(ticket) = existing_ticket {
                            clear_memory_dirty(&inner, &meeting_id, ticket);
                        }
                        return Err(error.into());
                    }
                    Err(error) => return Err(error.into()),
                };
                let source_current = match memory.meeting_store_is_current(&meeting_id) {
                    Ok(current) => current,
                    Err(error) => {
                        mark_memory_dirty(&inner, &meeting_id);
                        return Err(error.into());
                    }
                };
                let repair_ticket = existing_ticket.or_else(|| {
                    (!source_current).then(|| mark_memory_dirty(&inner, &meeting_id))
                });
                if let Some(error) = embedder_load_error {
                    // Refuse to destroy a usable existing row and its vectors
                    // when weights appear installed but cannot be opened. A
                    // missing or already-pending row is not usable, so refresh
                    // its text below and leave it visibly retryable.
                    if memory
                        .meeting_index_stats(&meeting_id)?
                        .is_some_and(|stats| stats.pending_passages == 0)
                    {
                        if source_current {
                            if let Some(ticket) = repair_ticket {
                                clear_memory_dirty(&inner, &meeting_id, ticket);
                            }
                        }
                        return Err(error.into());
                    }
                }
                // This always rewrites, even if the fingerprint happens to be
                // unchanged. With a model, inference is staged before the
                // replacement transaction so a failure cannot erase a healthy
                // row or its vectors.
                if let Some(embedder) = embedder.as_mut() {
                    memory.force_index_with_embeddings(&meeting, embedder)?;
                } else {
                    memory.force_index(&meeting)?;
                }
                let row_current = memory
                    .meeting_store_is_current(&meeting_id)
                    .unwrap_or_else(|error| {
                        tracing::warn!(meeting_id = %meeting_id, %error, "no pude verificar la reindexación manual");
                        false
                    });
                if row_current {
                    if let Some(ticket) = repair_ticket {
                        clear_memory_dirty(&inner, &meeting_id, ticket);
                    }
                } else {
                    mark_memory_dirty(&inner, &meeting_id);
                }
                let globally_current = row_current
                    && inner.memory_dirty.lock().is_empty()
                    && memory.finished_store_is_current().unwrap_or_else(|error| {
                        tracing::warn!(%error, "no pude verificar todas las reparaciones manuales");
                        false
                    });
                inner
                    .memory_consistent
                    .store(globally_current, Ordering::Release);

                let stats = match memory.meeting_index_stats(&meeting_id) {
                    Ok(stats) => stats,
                    Err(error) => {
                        tracing::warn!(
                            meeting_id = %meeting_id,
                            %error,
                            "la reunión se reindexó, pero no pude consultar el estado final"
                        );
                        return Ok(MeetingIndexStatus::unavailable());
                    }
                };
                drop(guard);
                let questions_enabled = inner.config.read().llm.meeting_questions;
                Ok(MeetingIndexStatus::from_stats(questions_enabled, stats))
            })();

            drop(maintenance);
            inner.emit(KualiEvent::MeetingIndexChanged {
                meeting_id: meeting_id.clone(),
            });
            if memory_retry_needed(&inner) {
                schedule_memory_sync(&inner, false);
            }
            result
        })
        .await
        .map_err(|error| EngineError::ModelStorage(error.to_string()))?
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
        {
            let _metadata = self.inner.metadata_mutation.lock();
            mark_memory_dirty(&self.inner, id);
            if let Err(error) = kuali_store::delete(id) {
                schedule_memory_sync(&self.inner, false);
                return Err(error.into());
            }
        }
        forget_meetings(&self.inner, [id.to_string()]);
        Ok(())
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

        let (deleted, deletion_error) = {
            let _metadata = self.inner.metadata_mutation.lock();
            for id in &ids {
                if !kuali_store::meeting_dir(id).is_dir() {
                    return Err(kuali_store::StoreError::NotFound((*id).clone()).into());
                }
            }
            mark_meetings_dirty(
                &self.inner,
                &ids.iter().map(|id| (*id).clone()).collect::<Vec<_>>(),
            );
            let mut deleted = Vec::new();
            let mut deletion_error = None;
            for id in &ids {
                match kuali_store::delete(id) {
                    Ok(()) => deleted.push((*id).clone()),
                    Err(error) => {
                        deletion_error = Some(error);
                        break;
                    }
                }
            }
            (deleted, deletion_error)
        };
        forget_meetings(&self.inner, deleted);
        if let Some(error) = deletion_error {
            schedule_memory_sync(&self.inner, false);
            return Err(error.into());
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
    pub async fn set_task_done(
        &self,
        meeting_id: &str,
        task_id: &str,
        done: bool,
    ) -> Result<(), EngineError> {
        let meeting = {
            let _metadata = self.inner.metadata_mutation.lock();
            let mut meeting = kuali_store::load(meeting_id)?;
            if let Some(summary) = meeting.summary.as_mut() {
                if let Some(task) = summary.action_items.iter_mut().find(|t| t.id == task_id) {
                    task.done = done;
                }
            }
            mark_memory_dirty(&self.inner, meeting_id);
            if let Err(error) = kuali_store::save(&meeting) {
                schedule_memory_sync(&self.inner, false);
                return Err(error.into());
            }
            if let Some(active) = self
                .inner
                .active
                .lock()
                .values_mut()
                .find(|active| active.meeting.meta.id == meeting_id)
            {
                if let Some(summary) = active.meeting.summary.as_mut() {
                    if let Some(task) = summary.action_items.iter_mut().find(|t| t.id == task_id) {
                        task.done = done;
                    }
                }
            }
            meeting
        };
        // Task status is part of the searchable passage. Reuse the normal
        // automatic pipeline so the textual row changes immediately and its
        // vector is refreshed without a special second indexing path.
        remember(&self.inner, &meeting);

        Ok(())
    }

    /// Answers a question from past meetings, restricted to what `audience`
    /// may read.
    ///
    /// The audience is a parameter rather than something derived here, so the
    /// caller that knows who is asking is the one that says so. Discord passes
    /// the account and server it received the command from; the desktop
    /// application passes [`kuali_memory::Audience::Everything`], because it is
    /// running on the machine that recorded the meetings for the person who
    /// owns them.
    pub async fn ask(
        &self,
        audience: kuali_memory::Audience,
        question: &str,
        asker: kuali_memory::Asker,
    ) -> Result<kuali_memory::Answer, EngineError> {
        self.ask_with_history(audience, question, asker, &[]).await
    }

    /// Answers a desktop follow-up with a short, caller-owned conversation.
    ///
    /// The history is never authority. It only expands the retrieval query and
    /// supplies meeting IDs that the memory layer authorizes again before it
    /// returns any passage. Discord deliberately keeps using [`Engine::ask`]
    /// because each slash command is a standalone interaction.
    pub async fn ask_with_history(
        &self,
        audience: kuali_memory::Audience,
        question: &str,
        asker: kuali_memory::Asker,
        history: &[kuali_memory::ConversationTurn],
    ) -> Result<kuali_memory::Answer, EngineError> {
        ask_memory(&self.inner, audience, question, asker, history).await
    }

    /// Who Kuali believes is using the desktop application.
    ///
    /// Browser meetings answer this by themselves: the page marks the tile that
    /// owns the local microphone, so the name comes from the meeting rather
    /// than from a preference. Names typed into Settings come after it, which
    /// is what covers Discord-only libraries and lets someone correct a display
    /// name their platform got wrong.
    ///
    /// Never marked verified: the desktop application authenticates nobody, and
    /// a wrong guess should make the model hedge rather than assert.
    pub fn local_asker(&self) -> kuali_memory::Asker {
        let config = self.inner.config.read().clone();
        // The followed account is who this installation belongs to, so the
        // names its meetings recorded are this person's names. Without it a
        // Discord-only library would know nothing about who is asking, even
        // though Kuali has been told exactly who to follow.
        let follow = config.discord.follow_user_id;

        let mut names = if self.inner.memory_maintenance.load(Ordering::Acquire) > 0 {
            // The command will immediately receive QuestionIndexUpdating. Do
            // not make it wait behind the same long embedding lock merely to
            // enrich an identity that will not be used for an answer yet.
            Vec::new()
        } else {
            let guard = self.inner.memory.lock();
            match guard.as_ref() {
                Some(memory) => {
                    let mut found = follow
                        .and_then(|id| memory.names_for_speaker(id).ok())
                        .unwrap_or_default();
                    found.extend(memory.known_self_names().unwrap_or_default());
                    found
                }
                None => Vec::new(),
            }
        };
        names.extend(config.application.display_names.clone());
        // A name seen in two places is still one name.
        let mut seen = HashSet::new();
        names.retain(|name| seen.insert(name.to_lowercase()));
        kuali_memory::Asker::named(names, false)
    }

    /// What still stands between the user and asking a question.
    ///
    /// `pending_passages` is a real count, which is what lets the interface
    /// promise a time rather than a vague "this may take a while".
    pub fn questions_status(&self) -> crate::questions::QuestionsStatus {
        let config = self.inner.config.read().clone();
        let models_dir = crate::questions::models_dir_for(&config.whisper);
        let model_ready = kuali_memory::embed::is_downloaded(&models_dir);
        let updating = self.inner.memory_maintenance.load(Ordering::Acquire) > 0;

        // Embedding a backlog holds the SQLite handle while the local model
        // works. The atomic guard exists so status remains instant instead of
        // freezing the interface behind that potentially minute-long lock.
        let tracked_current = memory_snapshot_is_current(&self.inner);
        let (pending, embedded, index_available, index_current) = if updating {
            (0, 0, false, false)
        } else {
            let guard = self.inner.memory.lock();
            match guard.as_ref() {
                Some(memory) => match memory.pending_embeddings().and_then(|pending| {
                    memory
                        .embedded_passages()
                        .map(|embedded| (pending, embedded))
                }) {
                    Ok((pending, embedded)) => {
                        let current = tracked_current
                            && memory.finished_store_is_covered().unwrap_or_else(|error| {
                                tracing::warn!(%error, "no pude comprobar que todas las reuniones estén indexadas");
                                false
                            });
                        (pending, embedded, true, current)
                    }
                    Err(error) => {
                        tracing::warn!(%error, "no pude consultar la preparación del índice");
                        (0, 0, false, false)
                    }
                },
                None => (0, 0, false, false),
            }
        };

        crate::questions::QuestionsStatus {
            enabled: config.llm.meeting_questions,
            model_ready,
            index_available,
            index_current,
            pending_passages: pending,
            embedded_passages: embedded,
            updating,
            ready: config.llm.meeting_questions
                && model_ready
                && index_available
                && index_current
                && pending == 0
                && !updating,
        }
    }

    /// Downloads the embedding model if needed, then embeds every stored
    /// passage, reporting progress as it goes.
    ///
    /// Safe to call again after an interruption: the download skips files
    /// already present and the indexing only touches passages that still lack a
    /// vector, so a cancelled run resumes instead of restarting.
    pub async fn prepare_questions(&self) -> Result<(), EngineError> {
        let config = self.inner.config.read().clone();
        let models_dir = crate::questions::models_dir_for(&config.whisper);

        let inner = Arc::clone(&self.inner);
        let result = crate::questions::download_model(&models_dir, |stage, done, total| {
            inner.emit(KualiEvent::QuestionSetupProgress { stage, done, total });
        })
        .await;
        if let Err(message) = result {
            self.inner.emit(KualiEvent::QuestionSetupFinished {
                error: Some(message.clone()),
            });
            return Err(EngineError::ModelStorage(message));
        }

        self.inner.memory_consistent.store(false, Ordering::Release);
        let maintenance = MemoryMaintenanceGuard::new(&self.inner);
        let inner = Arc::clone(&self.inner);
        let indexing = tokio::task::spawn_blocking(move || {
            let _maintenance = maintenance;
            // The index has to be current before its passages are embedded,
            // otherwise a meeting recorded while the feature was off would be
            // missing rather than merely unvectorized.
            {
                let _metadata = inner.metadata_mutation.lock();
                let mut guard = inner.memory.lock();
                let Some(memory) = guard.as_mut() else {
                    return Err(kuali_memory::MemoryError::Embedding {
                        message: "el índice de reuniones no está disponible".into(),
                    });
                };
                let report = memory.sync_from_store()?;
                reconcile_memory_dirty(&inner, memory);
                inner
                    .memory_consistent
                    .store(report.unreadable == 0, Ordering::Release);
            }

            let mut embedder = kuali_memory::embed::Embedder::load(&models_dir)?;
            let mut guard = inner.memory.lock();
            let Some(memory) = guard.as_mut() else {
                return Err(kuali_memory::MemoryError::Embedding {
                    message: "el índice de reuniones no está disponible".into(),
                });
            };
            memory.embed_pending(&mut embedder, |done, total| {
                inner.emit(KualiEvent::QuestionSetupProgress {
                    stage: kuali_core::QuestionSetupStage::Indexing,
                    done: done as u64,
                    total: Some(total as u64),
                });
                true
            })?;
            Ok(())
        })
        .await
        .map_err(|error| EngineError::ModelStorage(error.to_string()))?;

        match indexing {
            Ok(()) => {
                self.inner
                    .emit(KualiEvent::QuestionSetupFinished { error: None });
                Ok(())
            }
            Err(error) => {
                let message = error.to_string();
                self.inner.emit(KualiEvent::QuestionSetupFinished {
                    error: Some(message),
                });
                schedule_memory_sync(&self.inner, false);
                Err(error.into())
            }
        }
    }

    /// Throws away the vectors and the downloaded weights, returning the bytes
    /// freed. Passages and meetings are untouched, so turning the feature back
    /// on re-embeds rather than re-transcribes.
    pub fn discard_question_data(&self) -> Result<u64, EngineError> {
        let config = self.inner.config.read().clone();
        {
            let mut guard = self.inner.memory.lock();
            if let Some(memory) = guard.as_mut() {
                memory.forget_embeddings()?;
            }
        }
        crate::questions::delete_model(&crate::questions::models_dir_for(&config.whisper))
            .map_err(|error| EngineError::ModelStorage(error.to_string()))
    }

    /// Brings the index in line with the stored library, in the background.
    ///
    /// Runs at startup so meetings recorded before this feature existed — or
    /// while the index was missing — become searchable without the user doing
    /// anything. If questions are already enabled, it also finishes any vectors
    /// left pending by that catch-up or by an earlier transient model failure;
    /// otherwise the all-or-nothing question gate would keep the whole RAG
    /// unavailable until the user manually ran setup again.
    ///
    /// Uses a plain thread rather than a runtime task on purpose: startup calls
    /// it from the interface's setup hook, which runs on the main thread before
    /// any reactor exists. A background job that only works from inside an async
    /// context is a trap for whoever calls it next.
    pub fn sync_memory(&self) {
        schedule_memory_sync(&self.inner, true);
    }

    /// Requests another LLM summary after changing providers or receiving a weak result.
    pub async fn resummarize(&self, meeting_id: &str) -> Result<MeetingSummary, EngineError> {
        let config = self.inner.config.read().clone();
        if !config.llm.summarize_on_leave {
            return Err(EngineError::SummariesDisabled);
        }
        let mut meeting = self.load_meeting(meeting_id)?;
        begin_post_processing(&self.inner);
        let maintenance = MemoryMaintenanceGuard::new(&self.inner);
        let mut may_create = false;
        let result = summarize_and_sync(&self.inner, &mut meeting, &config, 0, &mut may_create)
            .await
            .map_err(Into::into);
        // A regenerated summary replaces what questions were answering from.
        remember(&self.inner, &meeting);
        drop(maintenance);
        finish_post_processing(&self.inner);
        result
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
        VoiceEvent::LatestMeetingRequested {
            guild_id,
            channel_id,
            reply,
        } => {
            let _ = reply.send(latest_meeting_for_discord(inner, guild_id, channel_id));
        }
        VoiceEvent::QuestionAsked {
            user_id,
            guild_id,
            question,
            asker_name,
            reply,
        } => {
            let result = answer_for_discord(inner, user_id, guild_id, &question, asker_name).await;
            let _ = reply.send(result);
        }
        VoiceEvent::FollowRequested { user_id, reply } => {
            let result = configure_discord_follow(inner, user_id).await;
            let _ = reply.send(result);
        }
        VoiceEvent::GuildsKnown(guilds) => {
            if let Err(error) = kuali_store::remember_guilds(guilds) {
                tracing::warn!(%error, "no se pudieron guardar los servidores conocidos");
            } else {
                inner.emit(KualiEvent::GuildsUpdated);
            }
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
        // Server identity belongs to the source, not to one call.
        VoiceEvent::GuildsKnown(guilds) => {
            if kuali_store::remember_guilds(guilds).is_ok() {
                inner.emit(KualiEvent::GuildsUpdated);
            }
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
        VoiceEvent::LatestMeetingRequested {
            guild_id,
            channel_id,
            reply,
        } => {
            let _ = reply.send(latest_meeting_for_discord(inner, guild_id, channel_id));
        }
        VoiceEvent::QuestionAsked {
            user_id,
            guild_id,
            question,
            asker_name,
            reply,
        } => {
            let result = answer_for_discord(inner, user_id, guild_id, &question, asker_name).await;
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

/// Newest meeting held in one voice channel, including the one still running.
///
/// A slash command names no meeting, so the channel it was typed in decides
/// which history can be reached at all. Nothing outside that channel is
/// considered, even inside the same server.
async fn ask_memory(
    inner: &Arc<Inner>,
    audience: kuali_memory::Audience,
    question: &str,
    asker: kuali_memory::Asker,
    history: &[kuali_memory::ConversationTurn],
) -> Result<kuali_memory::Answer, EngineError> {
    let config = inner.config.read().clone();
    // Answering means sending transcript excerpts to the configured provider,
    // which is exactly what this setting governs. Someone who turned it off did
    // not carve out an exception for questions.
    if !config.llm.summarize_on_leave {
        return Err(EngineError::SummariesDisabled);
    }
    // Questions are all-or-nothing on purpose. Falling back to word matching
    // when the embedding model is absent would produce a feature that answers
    // well sometimes and misses obvious things other times, and the misses are
    // what people remember.
    if !config.llm.meeting_questions {
        return Err(EngineError::QuestionsDisabled);
    }
    // Enforce completeness in the engine, not only in whichever interface
    // happens to render `questions_status`. Otherwise Discord or a direct
    // command could retrieve from a partially vectorized library.
    if inner.memory_maintenance.load(Ordering::Acquire) > 0 {
        return Err(EngineError::QuestionIndexUpdating);
    }
    {
        let guard = inner.memory.lock();
        ensure_question_index_ready(inner, guard.as_ref())?;
    }
    let models_dir = crate::questions::models_dir_for(&config.whisper);
    if !kuali_memory::embed::is_downloaded(&models_dir) {
        return Err(EngineError::QuestionModelMissing);
    }

    // Own a bounded working copy across the blocking retrieval and provider
    // await. The memory crate separately bounds every text field before it can
    // enter a prompt.
    let history = history[history.len().saturating_sub(MAX_ASK_HISTORY_TURNS)..].to_vec();
    let retrieval_query = conversation_retrieval_query(question, &history, &asker);
    let context_meeting_ids = conversation_meeting_ids(&history);

    let passages = {
        let inner = Arc::clone(inner);
        // SQLite is blocking, and the lock has to be released before the
        // provider is called: one person's question must not hold the index
        // while a model thinks.
        tokio::task::spawn_blocking(move || -> Result<_, EngineError> {
            let mut embedder = kuali_memory::embed::Embedder::load(&models_dir)?;
            if inner.memory_maintenance.load(Ordering::Acquire) > 0 {
                return Err(EngineError::QuestionIndexUpdating);
            }
            let guard = inner.memory.lock();
            match guard.as_ref() {
                Some(memory) => {
                    let generation = inner.memory_generation.load(Ordering::Acquire);
                    // Check again under the same lock used for retrieval: a
                    // meeting may have finished while the model was loading.
                    ensure_question_index_ready(&inner, Some(memory))?;
                    let passages = memory.evidence_with_conversation(
                        &audience,
                        &retrieval_query,
                        &context_meeting_ids,
                        Some(&mut embedder),
                    )?;
                    ensure_memory_read_generation(&inner, generation)?;
                    Ok(Some((passages, generation)))
                }
                None => Ok(None),
            }
        })
        .await
        .map_err(|error| EngineError::ModelStorage(error.to_string()))??
    };

    let Some((passages, generation)) = passages else {
        return Err(EngineError::MemoryUnavailable);
    };
    ensure_memory_read_generation(inner, generation)?;
    if passages.is_empty() {
        // Nothing this person can reach mentions it. That is an answer, and it
        // costs no provider call to give.
        return Ok(kuali_memory::Answer::NothingFound);
    }

    let provider = kuali_llm::select_provider(&config.llm).await?;
    let answer = kuali_memory::answer_with_history(
        provider.as_ref(),
        question,
        &passages,
        &config.llm.output_language,
        &asker,
        &history,
    )
    .await?;
    ensure_memory_read_generation(inner, generation)?;
    Ok(answer)
}

const MAX_ASK_HISTORY_TURNS: usize = 6;
const MAX_ASK_CONTEXT_MEETINGS: usize = 3;

fn conversation_retrieval_query(
    question: &str,
    history: &[kuali_memory::ConversationTurn],
    asker: &kuali_memory::Asker,
) -> String {
    let carries_self_context = question_refers_to_prior_context(question)
        && history
            .last()
            .is_some_and(|turn| question_refers_to_asker(&turn.question));
    if (!question_refers_to_asker(question) && !carries_self_context) || asker.names.is_empty() {
        return kuali_memory::conversation_query(question, history);
    }

    let names = asker
        .names
        .iter()
        .take(4)
        .filter_map(|name| {
            let name: String = name.trim().chars().take(80).collect();
            (!name.is_empty()).then_some(name)
        })
        .collect::<Vec<_>>();
    if names.is_empty() {
        return kuali_memory::conversation_query(question, history);
    }

    // Put identity before the conversation expansion. FTS deliberately keeps a
    // small number of terms and embedding models have a token limit; appending
    // names after several long prior answers could silently truncate the one
    // term that distinguishes the asker's task from everyone else's.
    let expanded = format!(
        "The person asking appears in meeting tasks as: {}\nCurrent question: {question}",
        names.join(", ")
    );
    kuali_memory::conversation_query(&expanded, history)
}

/// Whether a short question points back to the preceding turn instead of
/// restating its subject. Identity from a prior self-task question must survive
/// “¿y en esa reunión?”, but it should not leak into an unrelated new topic.
fn question_refers_to_prior_context(question: &str) -> bool {
    question
        .split(|character: char| !character.is_alphanumeric())
        .map(normalized_question_word)
        .any(|word| {
            matches!(
                word.as_str(),
                "ese"
                    | "esa"
                    | "eso"
                    | "este"
                    | "esta"
                    | "esto"
                    | "aquel"
                    | "aquella"
                    | "ahi"
                    | "alli"
                    | "dicho"
                    | "dicha"
                    | "mencionado"
                    | "mencionada"
                    | "mencionaste"
                    | "mismo"
                    | "misma"
                    | "anterior"
                    | "that"
                    | "there"
                    | "same"
                    | "it"
                    | "mentioned"
                    | "previous"
            )
        })
}

fn normalized_question_word(word: &str) -> String {
    word.to_lowercase()
        .chars()
        .map(|character| match character {
            'á' | 'à' | 'ä' | 'â' => 'a',
            'é' | 'è' | 'ë' | 'ê' => 'e',
            'í' | 'ì' | 'ï' | 'î' => 'i',
            'ó' | 'ò' | 'ö' | 'ô' => 'o',
            'ú' | 'ù' | 'ü' | 'û' => 'u',
            other => other,
        })
        .collect()
}

fn question_refers_to_asker(question: &str) -> bool {
    question
        .split(|character: char| !character.is_alphanumeric())
        .map(normalized_question_word)
        .any(|word| {
            matches!(
                word.as_str(),
                "yo" | "me"
                    | "mi"
                    | "mis"
                    | "mio"
                    | "mia"
                    | "mios"
                    | "mias"
                    | "i"
                    | "my"
                    | "mine"
                    | "tengo"
                    | "debo"
                    | "quede"
                    | "hice"
                    | "dije"
                    | "prometi"
                    | "acorde"
            )
        })
}

/// Most-recent citations win, while duplicates and malformed IDs are ignored.
/// The IDs are only retrieval hints; `evidence_with_conversation` still applies
/// the audience's visibility query before reading their passages.
fn conversation_meeting_ids(history: &[kuali_memory::ConversationTurn]) -> Vec<String> {
    let mut meeting_ids = Vec::new();
    for turn in history.iter().rev() {
        for meeting_id in &turn.meeting_ids {
            let meeting_id = meeting_id.trim();
            if meeting_id.is_empty()
                || meeting_id.chars().count() > 160
                || meeting_ids.iter().any(|known| known == meeting_id)
            {
                continue;
            }
            meeting_ids.push(meeting_id.to_string());
            if meeting_ids.len() == MAX_ASK_CONTEXT_MEETINGS {
                return meeting_ids;
            }
        }
    }
    meeting_ids
}

fn ensure_question_index_ready(
    inner: &Inner,
    memory: Option<&kuali_memory::Memory>,
) -> Result<(), EngineError> {
    if inner.memory_maintenance.load(Ordering::Acquire) > 0 {
        return Err(EngineError::QuestionIndexUpdating);
    }
    let Some(memory) = memory else {
        return Err(EngineError::MemoryUnavailable);
    };
    let pending = memory.pending_embeddings()?;
    if pending > 0 {
        return Err(EngineError::QuestionIndexPending(pending));
    }
    if !memory_snapshot_is_current(inner) {
        return Err(EngineError::QuestionIndexOutOfSync);
    }
    if !memory.finished_store_is_covered()? {
        return Err(EngineError::QuestionIndexOutOfSync);
    }
    Ok(())
}

/// Rejects evidence captured across a concurrent store/index mutation.
///
/// The SQLite lock protects the query itself, but marking a meeting dirty must
/// remain non-blocking. A monotonic generation therefore brackets retrieval and
/// the provider await; any intervening edit makes the old passages unusable.
fn ensure_memory_read_generation(inner: &Inner, expected: u64) -> Result<(), EngineError> {
    if inner.memory_maintenance.load(Ordering::Acquire) > 0 {
        return Err(EngineError::QuestionIndexUpdating);
    }
    if inner.memory_generation.load(Ordering::Acquire) != expected
        || !memory_snapshot_is_current(inner)
    {
        return Err(EngineError::QuestionIndexOutOfSync);
    }
    Ok(())
}

/// Answers a Discord question, scoped to the meetings that account attended in
/// that server.
///
/// The audience is built here, from the identity Discord verified, and never
/// from anything inside the question text.
async fn answer_for_discord(
    inner: &Arc<Inner>,
    user_id: DiscordUserId,
    guild_id: u64,
    question: &str,
    asker_name: Option<String>,
) -> Result<Option<kuali_core::MeetingAnswer>, String> {
    let audience = kuali_memory::Audience::DiscordParticipant { user_id, guild_id };
    // Discord authenticated this account, so the name is verified. The names
    // typed into Settings come along as aliases, because the same person may
    // appear differently in a browser meeting.
    let mut names: Vec<String> = asker_name.into_iter().collect();
    names.extend(inner.config.read().application.display_names.clone());
    let asker = kuali_memory::Asker::named(names, true);

    match ask_memory(inner, audience, question, asker, &[]).await {
        Ok(kuali_memory::Answer::NothingFound) => Ok(None),
        Ok(kuali_memory::Answer::Answered { text, citations }) => {
            Ok(Some(kuali_core::MeetingAnswer {
                text,
                citations: citations
                    .into_iter()
                    .map(|citation| kuali_core::AnswerCitation {
                        meeting_id: citation.meeting_id,
                        title: citation.meeting_title,
                        channel_name: citation.channel_name,
                        started_at: citation.started_at,
                        start_ms: citation.start_ms,
                    })
                    .collect(),
            }))
        }
        Err(error) => {
            // The bot shows this to whoever asked, so it stays about what they
            // can do rather than about engine internals.
            tracing::warn!(%error, "no pude responder una pregunta desde Discord");
            Err(match error {
                EngineError::SummariesDisabled => {
                    "Las funciones de IA están desactivadas en Ajustes de Kuali.".to_string()
                }
                EngineError::MemoryUnavailable => {
                    "El índice de reuniones no está disponible ahora mismo.".to_string()
                }
                EngineError::QuestionsDisabled | EngineError::QuestionModelMissing => {
                    "Las preguntas sobre reuniones pasadas no están activadas en Kuali.".to_string()
                }
                EngineError::QuestionIndexPending(_) => {
                    "El índice de reuniones todavía se está completando. Inténtalo de nuevo en un momento."
                        .to_string()
                }
                EngineError::QuestionIndexUpdating => {
                    "Kuali está terminando de guardar una reunión en la memoria. Inténtalo de nuevo en un momento."
                        .to_string()
                }
                EngineError::QuestionIndexOutOfSync => {
                    "Hay una reunión que todavía no llegó al índice. Ábrela en Kuali y pulsa Reindexar."
                        .to_string()
                }
                other => other.to_string(),
            })
        }
    }
}

fn latest_meeting_for_discord(
    inner: &Arc<Inner>,
    guild_id: u64,
    channel_id: u64,
) -> Result<Option<Meeting>, String> {
    let live = inner
        .active
        .lock()
        .values()
        .map(|active| &active.meeting)
        .filter(|meeting| {
            meeting.meta.guild_id == guild_id && meeting.meta.channel_id == channel_id
        })
        .max_by_key(|meeting| meeting.meta.started_at)
        .cloned();

    let stored = kuali_store::list()
        .map_err(|error| error.to_string())?
        .into_iter()
        .filter(|meta| meta.guild_id == guild_id && meta.channel_id == channel_id)
        .max_by_key(|meta| meta.started_at);

    // A live meeting is also on disk while it runs. Comparing both candidates
    // keeps the in-memory copy, which already holds the newest utterances.
    let newest = match (live, stored) {
        (Some(live), Some(stored)) if stored.started_at > live.meta.started_at => {
            kuali_store::load(&stored.id).map_err(|error| error.to_string())?
        }
        (Some(live), _) => live,
        (None, Some(stored)) => kuali_store::load(&stored.id).map_err(|error| error.to_string())?,
        (None, None) => return Ok(None),
    };
    Ok(Some(newest))
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
        tags: Vec::new(),
        folder: None,
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
    {
        let _metadata = inner.metadata_mutation.lock();
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
    }

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
    // Reserve post-processing before the active entry disappears. This closes
    // the cross-thread instant where an updater could otherwise observe neither
    // a live meeting nor completion work and restart the application mid-save.
    begin_post_processing(inner);

    // Removing `active` and persisting the finished snapshot is one metadata
    // critical section. Delete checks `active` before taking this same lock, so
    // it either refuses while the meeting is live or runs after this save and
    // becomes authoritative; no late post-processing write can resurrect it.
    let (active, mut may_create, mut final_snapshot_needs_retry) = {
        let _metadata = inner.metadata_mutation.lock();
        let Some(mut active) = inner.active.lock().remove(&session) else {
            finish_post_processing(inner);
            return;
        };
        active.meeting.meta.ended_at = Some(Utc::now());
        if active.meeting.meta.display_title.is_none() {
            active.meeting.meta.display_title = Some(active.meeting.fallback_title());
        }
        mark_memory_dirty(inner, &active.meeting.meta.id);
        let (may_create, needs_retry) = match kuali_store::save(&active.meeting) {
            Ok(()) => (false, false),
            Err(error) => {
                inner.emit(KualiEvent::error("store", error));
                (
                    matches!(
                        kuali_store::load(&active.meeting.meta.id),
                        Err(kuali_store::StoreError::NotFound(_))
                    ),
                    true,
                )
            }
        };
        (active, may_create, needs_retry)
    };
    let memory_update = MemoryMaintenanceGuard::new(inner);

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
        inner.set_status(EngineStatus::Summarizing);
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
            if final_snapshot_needs_retry {
                match persist_finished_snapshot_with_retries(&inner, &active.meeting, may_create)
                    .await
                {
                    Ok(Some(saved)) => {
                        active.meeting = saved;
                        may_create = false;
                        final_snapshot_needs_retry = false;
                    }
                    Ok(None) => {
                        may_create = false;
                        final_snapshot_needs_retry = false;
                    }
                    Err(error) => inner.emit(KualiEvent::error("store", error)),
                }
            }
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
                    &mut may_create,
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
            // If the first save failed before creating any readable source,
            // make one final local retry even when summaries were disabled or
            // the provider failed. No user deletion can target an absent row.
            if final_snapshot_needs_retry || may_create {
                match persist_finished_snapshot_with_retries(&inner, &active.meeting, may_create)
                    .await
                {
                    Ok(Some(saved)) => active.meeting = saved,
                    Ok(None) => {}
                    Err(error) => inner.emit(KualiEvent::error("store", error)),
                }
            }
            // Search is local and must not wait behind an integration retry.
            // A failing webhook can back off for hours; the meeting itself is
            // already settled and saved at this point, so index it immediately.
            remember(&inner, &active.meeting);
            drop(memory_update);
            dispatch_completed_webhooks(
                &inner,
                &config.integrations.webhooks,
                &active.meeting,
                summary_status,
            )
            .await;
            finish_post_processing(&inner);
        }
    };

    tokio::spawn(closing);
}

/// Re-persists the complete in-memory capture after a transient final-save
/// failure, without overwriting metadata or task state edited in the meantime.
/// `None` means the user deleted an already-existing meeting and that deletion
/// is authoritative.
async fn persist_finished_snapshot_with_retries(
    inner: &Arc<Inner>,
    meeting: &Meeting,
    may_create: bool,
) -> Result<Option<Meeting>, kuali_store::StoreError> {
    for attempt in 0..3 {
        let result = {
            let _metadata = inner.metadata_mutation.lock();
            match kuali_store::load(&meeting.meta.id) {
                Ok(saved) => {
                    let mut recovered = meeting.clone();
                    merge_authoritative_mutable_fields(&mut recovered, &saved);
                    mark_memory_dirty(inner, &meeting.meta.id);
                    kuali_store::save(&recovered).map(|()| Some(recovered))
                }
                Err(kuali_store::StoreError::NotFound(_)) if may_create => {
                    mark_memory_dirty(inner, &meeting.meta.id);
                    kuali_store::save(meeting).map(|()| Some(meeting.clone()))
                }
                Err(kuali_store::StoreError::NotFound(_)) => Ok(None),
                Err(error) => Err(error),
            }
        };
        match result {
            Ok(meeting) => return Ok(meeting),
            Err(error) if attempt < 2 => {
                tracing::warn!(
                    meeting_id = %meeting.meta.id,
                    attempt = attempt + 1,
                    %error,
                    "no pude persistir el snapshot final; reintentando"
                );
                tokio::time::sleep(Duration::from_millis(match attempt {
                    0 => 250,
                    _ => 1_500,
                }))
                .await;
            }
            Err(error) => return Err(error),
        }
    }
    unreachable!("the bounded retry loop returns on its final attempt")
}

/// Merges the fields users and integrations may edit while an LLM or retry is
/// in flight, without replacing the in-memory capture's speakers, utterances or
/// finished timestamp.
fn merge_authoritative_mutable_fields(target: &mut Meeting, saved: &Meeting) {
    target.meta.tags = saved.meta.tags.clone();
    target.meta.folder = saved.meta.folder.clone();
    target.discord_summary_delivery = saved.discord_summary_delivery;
    match (target.summary.as_mut(), saved.summary.as_ref()) {
        (None, summary) => target.summary = summary.cloned(),
        (Some(target_summary), Some(saved_summary)) => {
            for task in &mut target_summary.action_items {
                if let Some(saved_task) = saved_summary
                    .action_items
                    .iter()
                    .find(|saved_task| saved_task.id == task.id)
                {
                    task.done = saved_task.done;
                }
            }
        }
        (Some(_), None) => {}
    }
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

async fn dispatch_completed_webhooks(
    inner: &Arc<Inner>,
    subscriptions: &[kuali_core::WebhookSubscription],
    meeting: &Meeting,
    summary_status: crate::webhooks::SummaryStatus,
) {
    let mut deliveries = JoinSet::new();
    for subscription in subscriptions
        .iter()
        .filter(|subscription| subscription.enabled && subscription.matches(&meeting.meta))
        .cloned()
    {
        let meeting = meeting.clone();
        let events = inner.events.clone();
        deliveries.spawn(async move {
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
    while let Some(result) = deliveries.join_next().await {
        if let Err(error) = result {
            inner.emit(KualiEvent::error(
                "webhook",
                format!("a completion delivery ended unexpectedly: {error}"),
            ));
        }
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

    let _metadata = inner.metadata_mutation.lock();
    let (meeting_id, snapshot) = {
        let mut active = inner.active.lock();
        let Some(active) = active
            .values_mut()
            .find(|active| active.meeting.meta.id == meeting_id)
        else {
            return;
        };
        active.meeting.upsert_utterance(utterance.clone());
        // Metadata and task mutations are authoritative on disk. Merge those
        // fields while holding the same lock before cloning, so a transcription
        // that began earlier cannot save a stale snapshot over a later edit.
        if let Ok(saved) = kuali_store::load(&active.meeting.meta.id) {
            active.meeting.meta.display_title = saved.meta.display_title;
            active.meeting.meta.tags = saved.meta.tags;
            active.meeting.meta.folder = saved.meta.folder;
            active.meeting.summary = saved.summary;
            active.meeting.discord_summary_delivery = saved.discord_summary_delivery;
        }
        (active.meeting.meta.id.clone(), active.meeting.clone())
    };

    if let Err(e) = kuali_store::save(&snapshot) {
        inner.emit(KualiEvent::error("store", e));
    }
    drop(_metadata);
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
    may_create: &mut bool,
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
            may_create,
        )
        .await;
    }

    let result = summarize_with_retries(
        inner,
        meeting,
        provider.as_ref(),
        &config.llm.output_language,
        may_create,
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
        sync_discord_summary(
            inner,
            meeting,
            &config.llm.output_language,
            state,
            may_create,
        )
        .await;
    }
    result
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct OrganizationPlan {
    replacement_tags: Option<Vec<String>>,
    folder: Option<String>,
    create_folder: bool,
    backfill_ids: Vec<String>,
}

fn labels_equal(left: &str, right: &str) -> bool {
    left.to_lowercase() == right.to_lowercase()
}

fn generic_folder_key(name: &str) -> String {
    let normalized: String = name
        .to_lowercase()
        .chars()
        .map(|character| match character {
            'á' | 'à' | 'ä' | 'â' => 'a',
            'é' | 'è' | 'ë' | 'ê' => 'e',
            'í' | 'ì' | 'ï' | 'î' => 'i',
            'ó' | 'ò' | 'ö' | 'ô' => 'o',
            'ú' | 'ù' | 'ü' | 'û' => 'u',
            'ñ' => 'n',
            character if character.is_alphanumeric() => character,
            _ => ' ',
        })
        .collect();
    normalized.split_whitespace().collect::<Vec<_>>().join(" ")
}

fn is_generic_folder_name(name: &str) -> bool {
    matches!(
        generic_folder_key(name).as_str(),
        "meeting"
            | "meetings"
            | "reunion"
            | "reuniones"
            | "general"
            | "review"
            | "reviews"
            | "revision"
            | "revisiones"
            | "planning"
            | "planificacion"
            | "plan"
            | "plans"
            | "planes"
            | "follow up"
            | "followups"
            | "seguimiento"
            | "seguimientos"
            | "catch up"
            | "check in"
            | "checkin"
            | "standup"
            | "stand up"
            | "sync"
            | "synchronization"
            | "sincronizacion"
            | "status"
            | "status update"
            | "status updates"
            | "estado"
            | "actualizacion"
            | "actualizacion de estado"
            | "weekly"
            | "semanal"
            | "daily"
            | "diaria"
            | "retrospective"
            | "retrospectives"
            | "retrospectiva"
            | "retrospectivas"
            | "call"
            | "calls"
            | "llamada"
            | "llamadas"
            | "discussion"
            | "discussions"
            | "discusion"
            | "discusiones"
            | "notes"
            | "notas"
            | "updates"
            | "actualizaciones"
            | "misc"
            | "miscellaneous"
            | "other"
            | "otros"
            | "otras"
    )
}

fn organization_plan(
    current: &MeetingMeta,
    organization: &kuali_llm::MeetingOrganization,
    context: &kuali_llm::OrganizationContext,
    other_meetings: &[MeetingMeta],
) -> OrganizationPlan {
    let proposed_tags: Vec<String> = kuali_core::sanitize_tags(organization.tags.clone())
        .into_iter()
        .take(3)
        .collect();
    let replacement_tags = current.tags.is_empty().then(|| proposed_tags.clone());

    if current.folder.is_some() {
        return OrganizationPlan {
            replacement_tags,
            folder: None,
            create_folder: false,
            backfill_ids: Vec::new(),
        };
    }

    if let Some(canonical) = organization.folder.as_deref().and_then(|suggested| {
        context
            .folders
            .iter()
            .find(|folder| labels_equal(folder, suggested))
    }) {
        return OrganizationPlan {
            replacement_tags,
            folder: Some(canonical.clone()),
            create_folder: false,
            backfill_ids: Vec::new(),
        };
    }

    let Some(new_folder) = organization
        .new_folder
        .as_deref()
        .and_then(kuali_core::sanitize_folder)
    else {
        return OrganizationPlan {
            replacement_tags,
            folder: None,
            create_folder: false,
            backfill_ids: Vec::new(),
        };
    };
    let matching_tag = proposed_tags
        .iter()
        .find(|tag| labels_equal(tag, &new_folder));
    if new_folder.split_whitespace().count() > 4
        || is_generic_folder_name(&new_folder)
        || matching_tag.is_none()
    {
        return OrganizationPlan {
            replacement_tags,
            folder: None,
            create_folder: false,
            backfill_ids: Vec::new(),
        };
    }

    let backfill_ids: Vec<String> = other_meetings
        .iter()
        .filter(|meta| {
            meta.id != current.id
                && meta.folder.is_none()
                && meta.tags.iter().any(|tag| labels_equal(tag, &new_folder))
        })
        .map(|meta| meta.id.clone())
        .collect();
    // A recurring label only justifies a folder when at least two meetings can
    // actually live in it. A related meeting already filed by hand does not
    // count: creating another folder would leave the current meeting alone.
    if backfill_ids.is_empty() {
        return OrganizationPlan {
            replacement_tags,
            folder: None,
            create_folder: false,
            backfill_ids,
        };
    }

    // The tag and folder deliberately share one canonical spelling. This keeps
    // future prompts stable even when the provider varies capitalization.
    let folder = matching_tag.cloned().unwrap_or(new_folder);
    OrganizationPlan {
        replacement_tags,
        folder: Some(folder),
        create_folder: true,
        backfill_ids,
    }
}

fn organization_context(
    inner: &Arc<Inner>,
    current_id: &str,
) -> (kuali_llm::OrganizationContext, Vec<MeetingMeta>) {
    let folders = match kuali_store::list_folders() {
        Ok(folders) => folders,
        Err(error) => {
            tracing::warn!(%error, "no pude cargar las carpetas para organizar la reunión");
            inner.emit(KualiEvent::error("store", error));
            Vec::new()
        }
    };
    let other_meetings: Vec<MeetingMeta> = match kuali_store::list() {
        Ok(meetings) => meetings
            .into_iter()
            .filter(|meta| meta.id != current_id)
            .collect(),
        Err(error) => {
            tracing::warn!(%error, "no pude cargar las etiquetas para organizar la reunión");
            inner.emit(KualiEvent::error("store", error));
            Vec::new()
        }
    };
    let mut seen = HashSet::new();
    let tags = other_meetings
        .iter()
        .flat_map(|meta| meta.tags.iter())
        .filter(|tag| seen.insert(tag.to_lowercase()))
        .cloned()
        .collect();

    (
        kuali_llm::OrganizationContext { folders, tags },
        other_meetings,
    )
}

fn apply_meeting_organization(
    inner: &Arc<Inner>,
    meeting: &mut Meeting,
    organization: &kuali_llm::MeetingOrganization,
    context: &kuali_llm::OrganizationContext,
    other_meetings: &[MeetingMeta],
    may_create: &mut bool,
) -> bool {
    let metadata = inner.metadata_mutation.lock();
    let mut backfilled = Vec::new();
    // The model may take long enough for the user to edit metadata meanwhile.
    // Re-read while owning the same lock as the commands, so disk is
    // authoritative for the two fields automation is allowed to fill.
    match kuali_store::load(&meeting.meta.id) {
        Ok(saved) => {
            merge_authoritative_mutable_fields(meeting, &saved);
            *may_create = false;
        }
        Err(kuali_store::StoreError::NotFound(_)) if *may_create => {}
        Err(kuali_store::StoreError::NotFound(_)) => return false,
        Err(error) => {
            tracing::warn!(%error, "no pude reconciliar la organización de la reunión");
            inner.emit(KualiEvent::error("store", error));
            schedule_memory_sync(inner, false);
            return false;
        }
    }

    let plan = organization_plan(&meeting.meta, organization, context, other_meetings);
    if let Some(tags) = plan.replacement_tags {
        meeting.meta.tags = tags;
    }

    let mut backfill_ids = plan.backfill_ids;
    if let Some(folder) = plan.folder {
        if plan.create_folder {
            // Revalidate after the LLM call. A user may have filed the only
            // related meeting while the model was thinking; in that case a new
            // folder would again contain just this meeting and must not exist.
            backfill_ids.retain(|meeting_id| match kuali_store::load(meeting_id) {
                Ok(previous) => {
                    previous.meta.folder.is_none()
                        && previous
                            .meta
                            .tags
                            .iter()
                            .any(|tag| labels_equal(tag, &folder))
                }
                Err(error) => {
                    tracing::warn!(meeting_id = %meeting_id, %error, "no pude revalidar una reunión relacionada");
                    inner.emit(KualiEvent::error("store", error));
                    false
                }
            });
            if !backfill_ids.is_empty() {
                match kuali_store::create_folder(&folder) {
                    Ok(folders) => {
                        let canonical = folders
                            .into_iter()
                            .find(|known| labels_equal(known, &folder))
                            .unwrap_or(folder);
                        // Put an older related meeting in the folder first.
                        // The current meeting is assigned only after at least
                        // one such write succeeds, so a concurrent manual edit
                        // cannot leave the model-created folder containing only
                        // the meeting that triggered it.
                        for meeting_id in &backfill_ids {
                            let result = kuali_store::load(meeting_id).and_then(|mut previous| {
                                if previous.meta.folder.is_none()
                                    && previous
                                        .meta
                                        .tags
                                        .iter()
                                        .any(|tag| labels_equal(tag, &canonical))
                                {
                                    mark_memory_dirty(inner, meeting_id);
                                    previous.meta.folder = Some(canonical.clone());
                                    kuali_store::save(&previous)?;
                                    return Ok(Some(previous));
                                }
                                Ok(None)
                            });
                            match result {
                                Ok(Some(previous)) => backfilled.push(previous),
                                Ok(None) => {}
                                Err(error) => {
                                    tracing::warn!(meeting_id = %meeting_id, %error, "no pude mover una reunión relacionada a la carpeta nueva");
                                    inner.emit(KualiEvent::error("store", error));
                                    schedule_memory_sync(inner, false);
                                }
                            }
                        }
                        if !backfilled.is_empty() {
                            meeting.meta.folder = Some(canonical);
                        }
                    }
                    Err(error) => {
                        tracing::warn!(%error, "no pude crear la carpeta sugerida para la reunión");
                        inner.emit(KualiEvent::error("store", error));
                    }
                }
            }
        } else {
            meeting.meta.folder = Some(folder);
        }
    }

    mark_memory_dirty(inner, &meeting.meta.id);
    let saved_current = match kuali_store::save(meeting) {
        Ok(()) => {
            *may_create = false;
            true
        }
        Err(error) => {
            inner.emit(KualiEvent::error("store", error));
            schedule_memory_sync(inner, false);
            false
        }
    };
    drop(metadata);

    // Backfilled meetings changed searchable links and fingerprints too. Queue
    // their replacements after releasing the metadata lock; `remember` reloads
    // once more so any later manual edit still wins.
    for previous in backfilled {
        remember(inner, &previous);
    }
    saved_current
}

async fn summarize_with_retries(
    inner: &Arc<Inner>,
    meeting: &mut Meeting,
    provider: &dyn kuali_llm::LlmProvider,
    language: &str,
    may_create: &mut bool,
) -> Result<MeetingSummary, kuali_llm::LlmError> {
    inner.emit(KualiEvent::SummaryStarted {
        meeting_id: meeting.meta.id.clone(),
    });

    let (organization_context, other_meetings) = organization_context(inner, &meeting.meta.id);

    let mut completed = None;
    for attempt in 1..=MAX_SUMMARY_ATTEMPTS {
        match kuali_llm::analyze(provider, meeting, language, &organization_context).await {
            Ok(analysis) => {
                completed = Some(analysis);
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
                return Err(error);
            }
        }
    }
    let analysis = completed.expect("the retry loop returns on its final failure");
    let summary = analysis.summary;

    meeting.meta.display_title = Some(summary.title.clone());
    meeting.summary = Some(summary.clone());
    let persisted = apply_meeting_organization(
        inner,
        meeting,
        &analysis.organization,
        &organization_context,
        &other_meetings,
        may_create,
    );

    if persisted {
        inner.emit(KualiEvent::SummaryReady {
            meeting_id: meeting.meta.id.clone(),
            summary: summary.clone(),
        });
    }
    Ok(summary)
}

fn begin_post_processing(inner: &Arc<Inner>) {
    inner.post_processing.fetch_add(1, Ordering::AcqRel);
    if inner.active.lock().is_empty() {
        inner.set_status(EngineStatus::Summarizing);
    }
}

/// Keeps update safety false for detached target indexing without pretending
/// that the engine is summarizing. Dropping it exactly once owns the decrement,
/// including panic/cancellation paths where an explicit tail call would be
/// skipped.
struct PostProcessingGuard {
    inner: Arc<Inner>,
}

/// Marks store/index catch-up that is not tied to one meeting's normal
/// post-processing. Unlike [`PostProcessingGuard`], it does not change the app's
/// recording status; it only gates questions and unsafe restarts.
struct MemoryMaintenanceGuard {
    inner: Arc<Inner>,
}

impl MemoryMaintenanceGuard {
    fn new(inner: &Arc<Inner>) -> Self {
        inner.memory_maintenance.fetch_add(1, Ordering::AcqRel);
        inner.emit(KualiEvent::QuestionsStatusChanged);
        Self {
            inner: Arc::clone(inner),
        }
    }
}

impl Drop for MemoryMaintenanceGuard {
    fn drop(&mut self) {
        let previous = self.inner.memory_maintenance.fetch_sub(1, Ordering::AcqRel);
        debug_assert!(previous > 0, "memory-maintenance counter underflow");
        self.inner.emit(KualiEvent::QuestionsStatusChanged);
    }
}

impl PostProcessingGuard {
    fn new(inner: &Arc<Inner>) -> Self {
        inner.post_processing.fetch_add(1, Ordering::AcqRel);
        Self {
            inner: Arc::clone(inner),
        }
    }
}

impl Drop for PostProcessingGuard {
    fn drop(&mut self) {
        finish_post_processing(&self.inner);
    }
}

fn finish_post_processing(inner: &Arc<Inner>) {
    let previous = inner.post_processing.fetch_sub(1, Ordering::AcqRel);
    debug_assert!(previous > 0, "post-processing counter underflow");
    if previous != 1 || !inner.active.lock().is_empty() {
        return;
    }
    inner.set_status(if inner.discord_connected.load(Ordering::Acquire) {
        EngineStatus::Watching
    } else {
        EngineStatus::Offline
    });
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
    may_create: &mut bool,
) {
    let Some(delivery) = meeting.discord_summary_delivery else {
        return;
    };
    // Persist the pending channel before touching Discord. If the bot is
    // offline, a later regeneration can still retry the same destination. Only
    // update that field on an existing authoritative record: saving the whole
    // clone after network I/O could overwrite a user's newer tags or tasks.
    let persisted = {
        let _metadata = inner.metadata_mutation.lock();
        match kuali_store::load(&meeting.meta.id) {
            Ok(mut saved) => {
                mark_memory_dirty(inner, &meeting.meta.id);
                saved.discord_summary_delivery = Some(delivery);
                match kuali_store::save(&saved) {
                    Ok(()) => {
                        *may_create = false;
                        true
                    }
                    Err(error) => {
                        inner.emit(KualiEvent::error("store", error));
                        schedule_memory_sync(inner, false);
                        false
                    }
                }
            }
            Err(kuali_store::StoreError::NotFound(_)) if *may_create => {
                mark_memory_dirty(inner, &meeting.meta.id);
                match kuali_store::save(meeting) {
                    Ok(()) => {
                        *may_create = false;
                        true
                    }
                    Err(error) => {
                        inner.emit(KualiEvent::error("store", error));
                        schedule_memory_sync(inner, false);
                        false
                    }
                }
            }
            Err(kuali_store::StoreError::NotFound(_)) => false,
            Err(error) => {
                inner.emit(KualiEvent::error("store", error));
                schedule_memory_sync(inner, false);
                false
            }
        }
    };
    if !persisted {
        return;
    }

    let discord = inner.discord.lock().await;
    let result = match discord.as_ref() {
        Some(handle) => Some(
            handle
                .sync_summary_state(delivery, meeting, language, state)
                .await,
        ),
        None => None,
    };
    drop(discord);

    if let Some(result) = result {
        match result {
            Ok(delivery) => {
                meeting.discord_summary_delivery = Some(delivery);
                let _metadata = inner.metadata_mutation.lock();
                match kuali_store::load(&meeting.meta.id) {
                    Ok(mut saved) => {
                        mark_memory_dirty(inner, &meeting.meta.id);
                        saved.discord_summary_delivery = Some(delivery);
                        if let Err(error) = kuali_store::save(&saved) {
                            inner.emit(KualiEvent::error("store", error));
                            schedule_memory_sync(inner, false);
                        }
                    }
                    // A deletion while Discord was responding is authoritative.
                    Err(kuali_store::StoreError::NotFound(_)) => {}
                    Err(error) => {
                        inner.emit(KualiEvent::error("store", error));
                        schedule_memory_sync(inner, false);
                    }
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
                "openQuestions":[],
                "tags":[],
                "folder":"",
                "newFolder":""
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
                tags: Vec::new(),
                folder: None,
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
    fn meeting_index_status_is_derived_from_presence_and_pending_vectors() {
        let not_indexed = MeetingIndexStatus::from_stats(true, None);
        assert_eq!(not_indexed.state, MeetingIndexState::NotIndexed);
        assert_eq!(not_indexed.passages, 0);

        let counts = kuali_memory::MeetingIndexStats {
            passages: 7,
            pending_passages: 3,
        };
        let pending = MeetingIndexStatus::from_stats(true, Some(counts));
        assert_eq!(pending.state, MeetingIndexState::Pending);
        assert_eq!(pending.passages, 7);
        assert_eq!(pending.pending_passages, 3);

        let disabled = MeetingIndexStatus::from_stats(false, Some(counts));
        assert_eq!(disabled.state, MeetingIndexState::Indexed);

        let indexed = MeetingIndexStatus::from_stats(
            true,
            Some(kuali_memory::MeetingIndexStats {
                passages: 7,
                pending_passages: 0,
            }),
        );
        assert_eq!(indexed.state, MeetingIndexState::Indexed);
        assert_eq!(
            MeetingIndexStatus::unavailable().state,
            MeetingIndexState::Unavailable
        );

        let json = serde_json::to_value(pending).unwrap();
        assert_eq!(json["state"], "pending");
        assert_eq!(json["pendingPassages"], 3);
    }

    #[test]
    fn questions_status_fails_closed_when_the_memory_index_is_unavailable() {
        let mut config = KualiConfig::default();
        config.llm.meeting_questions = true;
        let (engine, _events) = Engine::new(config);
        *engine.inner.memory.lock() = None;

        let status = engine.questions_status();

        assert!(status.enabled);
        assert!(!status.index_available);
        assert!(!status.index_current);
        assert_eq!(status.pending_passages, 0);
        assert_eq!(status.embedded_passages, 0);
        assert!(!status.updating);
        assert!(!status.ready);
    }

    #[tokio::test]
    async fn memory_maintenance_exposes_both_status_edges_and_emits_refreshes() {
        let (engine, mut events) = Engine::new(KualiConfig::default());
        *engine.inner.memory.lock() = None;
        while events.try_recv().is_ok() {}

        let maintenance = MemoryMaintenanceGuard::new(&engine.inner);
        assert!(engine.questions_status().updating);
        assert!(!engine.questions_status().ready);
        assert!(matches!(
            tokio::time::timeout(Duration::from_secs(1), events.recv())
                .await
                .expect("starting maintenance should emit promptly")
                .expect("the event channel should remain open"),
            KualiEvent::QuestionsStatusChanged
        ));

        drop(maintenance);
        assert!(!engine.questions_status().updating);
        assert!(matches!(
            tokio::time::timeout(Duration::from_secs(1), events.recv())
                .await
                .expect("finishing maintenance should emit promptly")
                .expect("the event channel should remain open"),
            KualiEvent::QuestionsStatusChanged
        ));
    }

    #[test]
    fn question_readiness_reports_pending_before_checking_store_coverage() {
        let (engine, _events) = Engine::new(KualiConfig::default());
        let mut meeting = active_meeting("pending-before-coverage", 0).meeting;
        meeting.push_utterance(Utterance {
            id: "u1".into(),
            speaker_id: 12,
            start_ms: 0,
            end_ms: 1_000,
            text: "este pasaje aún no tiene vector".into(),
            confidence: None,
        });
        let mut memory = kuali_memory::Memory::in_memory().unwrap();
        memory.index(&meeting).unwrap();

        let error = ensure_question_index_ready(&engine.inner, Some(&memory)).unwrap_err();

        assert!(matches!(error, EngineError::QuestionIndexPending(1)));
    }

    #[test]
    fn a_failed_or_queued_replacement_blocks_backend_questions_too() {
        let (engine, _events) = Engine::new(KualiConfig::default());
        engine
            .inner
            .memory_consistent
            .store(true, Ordering::Release);
        engine.inner.memory_dirty.lock().insert("stale".into(), 1);
        let memory = kuali_memory::Memory::in_memory().unwrap();

        let error = ensure_question_index_ready(&engine.inner, Some(&memory)).unwrap_err();

        assert!(matches!(error, EngineError::QuestionIndexOutOfSync));
    }

    #[test]
    fn an_older_index_attempt_cannot_clear_a_newer_dirty_ticket() {
        let (engine, _events) = Engine::new(KualiConfig::default());
        let meeting_id = "generation-guard";

        let old_ticket = mark_memory_dirty(&engine.inner, meeting_id);
        let new_ticket = mark_memory_dirty(&engine.inner, meeting_id);
        assert!(new_ticket > old_ticket);

        clear_memory_dirty(&engine.inner, meeting_id, old_ticket);
        assert_eq!(
            engine.inner.memory_dirty.lock().get(meeting_id).copied(),
            Some(new_ticket),
            "an obsolete indexing job must leave the newer mutation dirty"
        );

        clear_memory_dirty(&engine.inner, meeting_id, new_ticket);
        assert!(!engine.inner.memory_dirty.lock().contains_key(meeting_id));
    }

    #[test]
    fn memory_retry_tracks_consistency_and_dirty_work_when_questions_are_disabled() {
        let mut config = KualiConfig::default();
        config.llm.meeting_questions = false;
        let (engine, _events) = Engine::new(config);

        engine
            .inner
            .memory_consistent
            .store(false, Ordering::Release);
        assert!(memory_retry_needed(&engine.inner));

        engine
            .inner
            .memory_consistent
            .store(true, Ordering::Release);
        assert!(!memory_retry_needed(&engine.inner));

        let ticket = mark_memory_dirty(&engine.inner, "dirty-without-questions");
        assert!(memory_retry_needed(&engine.inner));

        clear_memory_dirty(&engine.inner, "dirty-without-questions", ticket);
        assert!(!memory_retry_needed(&engine.inner));
    }

    #[test]
    fn organization_uses_the_canonical_existing_folder() {
        let current = active_meeting("current", 0).meeting.meta;
        let context = kuali_llm::OrganizationContext {
            folders: vec!["Proyecto Atlas".into()],
            tags: vec!["Backend".into()],
        };
        let organization = kuali_llm::MeetingOrganization {
            tags: vec!["  Backend  ".into(), "backend".into(), "API".into()],
            folder: Some("proyecto atlas".into()),
            new_folder: None,
        };

        let plan = organization_plan(&current, &organization, &context, &[]);

        assert_eq!(
            plan.replacement_tags,
            Some(vec!["Backend".into(), "API".into()])
        );
        assert_eq!(plan.folder.as_deref(), Some("Proyecto Atlas"));
        assert!(!plan.create_folder);
        assert!(plan.backfill_ids.is_empty());
    }

    #[test]
    fn organization_does_not_create_a_folder_for_a_single_meeting() {
        let current = active_meeting("current", 0).meeting.meta;
        let context = kuali_llm::OrganizationContext {
            folders: Vec::new(),
            tags: Vec::new(),
        };
        let organization = kuali_llm::MeetingOrganization {
            tags: vec!["Cliente Acme".into()],
            folder: None,
            new_folder: Some("Cliente Acme".into()),
        };

        let plan = organization_plan(&current, &organization, &context, &[]);

        assert_eq!(plan.folder, None);
        assert!(!plan.create_folder);
        assert!(plan.backfill_ids.is_empty());
    }

    #[test]
    fn recurring_tag_creates_one_folder_and_only_backfills_unfiled_meetings() {
        let current = active_meeting("current", 0).meeting.meta;
        let mut unfiled = active_meeting("previous-unfiled", 0).meeting.meta;
        unfiled.tags = vec!["cliente acme".into()];
        let mut manually_filed = active_meeting("previous-manual", 0).meeting.meta;
        manually_filed.tags = vec!["Cliente Acme".into()];
        manually_filed.folder = Some("Importante".into());
        let context = kuali_llm::OrganizationContext {
            folders: vec!["Importante".into()],
            tags: vec!["cliente acme".into()],
        };
        let organization = kuali_llm::MeetingOrganization {
            tags: vec!["Cliente Acme".into()],
            folder: None,
            new_folder: Some("cliente acme".into()),
        };

        let plan = organization_plan(
            &current,
            &organization,
            &context,
            &[unfiled, manually_filed],
        );

        assert_eq!(plan.folder.as_deref(), Some("Cliente Acme"));
        assert!(plan.create_folder);
        assert_eq!(plan.backfill_ids, vec!["previous-unfiled"]);
    }

    #[test]
    fn a_related_meeting_already_filed_by_hand_does_not_create_a_singleton_folder() {
        let current = active_meeting("current", 0).meeting.meta;
        let mut manually_filed = active_meeting("previous-manual", 0).meeting.meta;
        manually_filed.tags = vec!["Cliente Acme".into()];
        manually_filed.folder = Some("Clientes importantes".into());
        let context = kuali_llm::OrganizationContext {
            folders: vec!["Clientes importantes".into()],
            tags: vec!["Cliente Acme".into()],
        };
        let organization = kuali_llm::MeetingOrganization {
            tags: vec!["Cliente Acme".into()],
            folder: None,
            new_folder: Some("Cliente Acme".into()),
        };

        let plan = organization_plan(&current, &organization, &context, &[manually_filed]);

        assert_eq!(plan.folder, None);
        assert!(!plan.create_folder);
        assert!(plan.backfill_ids.is_empty());
    }

    #[test]
    fn organization_preserves_manual_metadata_and_rejects_generic_folders() {
        let mut current = active_meeting("current", 0).meeting.meta;
        current.tags = vec!["Elegida".into()];
        current.folder = Some("Carpeta manual".into());
        let organization = kuali_llm::MeetingOrganization {
            tags: vec!["Reemplazo".into()],
            folder: Some("Otra".into()),
            new_folder: Some("Reemplazo".into()),
        };
        let context = kuali_llm::OrganizationContext {
            folders: vec!["Otra".into()],
            tags: Vec::new(),
        };

        let preserved = organization_plan(&current, &organization, &context, &[]);
        assert_eq!(preserved.replacement_tags, None);
        assert_eq!(preserved.folder, None);

        current.tags.clear();
        current.folder = None;
        let mut previous = active_meeting("previous", 0).meeting.meta;
        previous.tags = vec!["Reunión".into()];
        let generic = kuali_llm::MeetingOrganization {
            tags: vec!["Reunión".into()],
            folder: None,
            new_folder: Some("REUNIÓN".into()),
        };
        let rejected = organization_plan(&current, &generic, &context, &[previous]);
        assert_eq!(rejected.folder, None);
        assert!(!rejected.create_folder);
    }

    #[test]
    fn organization_does_not_recreate_a_meeting_deleted_while_the_model_was_running() {
        let (engine, _events) = Engine::new(KualiConfig::default());
        let meeting_id = format!("organization-deleted-{}", uuid::Uuid::new_v4());
        let mut stale_meeting = active_meeting(&meeting_id, 0).meeting;
        kuali_store::save(&stale_meeting).unwrap();
        kuali_store::delete(&meeting_id).unwrap();
        assert!(!kuali_store::meeting_dir(&meeting_id).exists());

        let organization = kuali_llm::MeetingOrganization {
            tags: vec!["Proyecto Atlas".into()],
            folder: Some("Proyecto Atlas".into()),
            new_folder: None,
        };
        let context = kuali_llm::OrganizationContext {
            folders: vec!["Proyecto Atlas".into()],
            tags: vec!["Proyecto Atlas".into()],
        };
        let mut may_create = false;

        let persisted = apply_meeting_organization(
            &engine.inner,
            &mut stale_meeting,
            &organization,
            &context,
            &[],
            &mut may_create,
        );

        assert!(!persisted);
        assert!(!may_create);
        assert!(!kuali_store::meeting_dir(&meeting_id).exists());
        assert!(matches!(
            kuali_store::load(&meeting_id),
            Err(kuali_store::StoreError::NotFound(_))
        ));
    }

    #[tokio::test]
    async fn final_transcription_preserves_manual_metadata_from_the_authoritative_store() {
        let (engine, _events) = Engine::new(KualiConfig::default());
        let meeting_id = format!("transcription-metadata-{}", uuid::Uuid::new_v4());
        let session = session(VoiceSource::Web, 81);
        let mut live = active_meeting(&meeting_id, 0);
        live.meeting.summary = Some(MeetingSummary {
            title: "Antes de editar".into(),
            action_items: vec![kuali_core::ActionItem {
                id: "manual-task".into(),
                text: "Confirmar la ruta".into(),
                assignee: Some("Garrux".into()),
                due: None,
                source_ms: None,
                done: false,
            }],
            ..Default::default()
        });
        kuali_store::save(&live.meeting).unwrap();
        engine.inner.active.lock().insert(session, live);

        // Leave the live in-memory snapshot stale on purpose. This mirrors a
        // manual command committing while transcription was awaiting Whisper.
        let folder = format!("Carpeta manual {}", uuid::Uuid::new_v4());
        {
            let _metadata = engine.inner.metadata_mutation.lock();
            let mut edited = kuali_store::load(&meeting_id).unwrap();
            edited.meta.tags = vec!["Etiqueta manual".into()];
            edited.meta.folder = Some(folder.clone());
            edited.summary.as_mut().unwrap().action_items[0].done = true;
            kuali_store::save(&edited).unwrap();
        }

        let (reply, pending) = tokio::sync::oneshot::channel();
        assert!(reply
            .send(Ok(kuali_stt::Transcription {
                text: "Nueva frase transcrita".into(),
                confidence: Some(0.95),
            }))
            .is_ok());
        transcribe_live(
            &engine.inner,
            LiveTranscriptionContext {
                meeting_id: meeting_id.clone(),
                utterance_id: format!("{meeting_id}-segment-1"),
                speaker_id: 7,
                start_ms: 1_000,
                end_ms: 2_000,
                pass: TranscriptionPass::LiveFinal,
            },
            pending,
        )
        .await;

        let saved = kuali_store::load(&meeting_id).unwrap();
        assert_eq!(saved.meta.tags, vec!["Etiqueta manual"]);
        assert_eq!(saved.meta.folder.as_deref(), Some(folder.as_str()));
        assert!(saved.summary.unwrap().action_items[0].done);
        assert!(saved
            .utterances
            .iter()
            .any(|utterance| utterance.text == "Nueva frase transcrita"));

        engine.inner.active.lock().remove(&session);
        kuali_store::delete(&meeting_id).unwrap();
    }

    #[tokio::test]
    async fn final_snapshot_recovery_merges_ram_capture_with_authoritative_manual_edits() {
        let (engine, _events) = Engine::new(KualiConfig::default());
        let meeting_id = format!("final-snapshot-{}", uuid::Uuid::new_v4());
        let manual_folder = format!("Carpeta manual {}", uuid::Uuid::new_v4());

        let mut draft = active_meeting(&meeting_id, 0).meeting;
        draft.meta.tags = vec!["Etiqueta manual".into()];
        draft.meta.folder = Some(manual_folder.clone());
        draft.push_utterance(Utterance {
            id: "draft-utterance".into(),
            speaker_id: 7,
            start_ms: 0,
            end_ms: 1_000,
            text: "Frase guardada en el borrador".into(),
            confidence: Some(0.9),
        });
        draft.summary = Some(MeetingSummary {
            title: "Resumen anterior".into(),
            action_items: vec![kuali_core::ActionItem {
                id: "shared-task".into(),
                text: "Texto anterior de la tarea".into(),
                assignee: Some("Garrux".into()),
                due: None,
                source_ms: None,
                done: true,
            }],
            ..Default::default()
        });
        kuali_store::save(&draft).unwrap();

        let ended_at = Utc::now();
        let mut finished = active_meeting(&meeting_id, 0).meeting;
        finished.meta.ended_at = Some(ended_at);
        finished.meta.tags = vec!["Etiqueta automática obsoleta".into()];
        finished.push_utterance(Utterance {
            id: "draft-utterance".into(),
            speaker_id: 7,
            start_ms: 0,
            end_ms: 1_000,
            text: "Frase guardada en el borrador".into(),
            confidence: Some(0.9),
        });
        finished.push_utterance(Utterance {
            id: "final-utterance".into(),
            speaker_id: 7,
            start_ms: 1_000,
            end_ms: 2_000,
            text: "Frase final que solo estaba en memoria".into(),
            confidence: Some(0.95),
        });
        finished.summary = Some(MeetingSummary {
            title: "Resumen final".into(),
            overview: "Conclusiones generadas al terminar".into(),
            action_items: vec![kuali_core::ActionItem {
                id: "shared-task".into(),
                text: "Texto final de la tarea".into(),
                assignee: Some("Garrux".into()),
                due: None,
                source_ms: Some(1_500),
                done: false,
            }],
            ..Default::default()
        });

        let recovered = persist_finished_snapshot_with_retries(&engine.inner, &finished, false)
            .await
            .unwrap()
            .expect("an existing draft should be recovered");

        assert_eq!(recovered.meta.ended_at, Some(ended_at));
        assert_eq!(recovered.meta.tags, vec!["Etiqueta manual"]);
        assert_eq!(
            recovered.meta.folder.as_deref(),
            Some(manual_folder.as_str())
        );
        assert_eq!(recovered.utterances, finished.utterances);
        let recovered_summary = recovered.summary.as_ref().unwrap();
        assert_eq!(recovered_summary.title, "Resumen final");
        assert_eq!(
            recovered_summary.overview,
            "Conclusiones generadas al terminar"
        );
        assert_eq!(
            recovered_summary.action_items[0].text,
            "Texto final de la tarea"
        );
        assert!(recovered_summary.action_items[0].done);

        let persisted = kuali_store::load(&meeting_id).unwrap();
        assert_eq!(persisted, recovered);
        kuali_store::delete(&meeting_id).unwrap();
    }

    #[tokio::test]
    async fn automatic_indexing_emits_a_refresh_event_after_the_attempt() {
        let (engine, mut events) = Engine::new(KualiConfig::default());
        *engine.inner.memory.lock() = Some(kuali_memory::Memory::in_memory().unwrap());
        let meeting_id = format!("remember-event-{}", uuid::Uuid::new_v4());
        let mut meeting = active_meeting(&meeting_id, 0).meeting;
        meeting.push_utterance(Utterance {
            id: "u1".into(),
            speaker_id: 12,
            start_ms: 0,
            end_ms: 1_000,
            text: "el índice debe avisar cuando termine".into(),
            confidence: None,
        });
        kuali_store::save(&meeting).unwrap();

        let memory_lock = engine.inner.memory.lock();
        remember(&engine.inner, &meeting);
        assert!(
            !engine.safe_for_update(),
            "the target indexing attempt must block an application restart"
        );
        drop(memory_lock);
        let received = tokio::time::timeout(Duration::from_secs(2), async {
            loop {
                if let Some(KualiEvent::MeetingIndexChanged { meeting_id }) = events.recv().await {
                    break meeting_id;
                }
            }
        })
        .await
        .expect("the indexing attempt should emit an event");

        assert_eq!(received, meeting_id);
        assert_eq!(
            engine.meeting_index_status(&meeting_id).state,
            MeetingIndexState::Indexed
        );
        tokio::time::timeout(Duration::from_secs(1), async {
            while !engine.safe_for_update() {
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("update safety should recover after target indexing");
        kuali_store::delete(&meeting_id).unwrap();
    }

    #[tokio::test]
    async fn asking_is_rejected_while_any_passage_is_pending() {
        let mut config = KualiConfig::default();
        config.llm.summarize_on_leave = true;
        config.llm.meeting_questions = true;
        let (engine, _events) = Engine::new(config);
        let mut meeting = active_meeting("pending-question", 0).meeting;
        meeting.push_utterance(Utterance {
            id: "u1".into(),
            speaker_id: 12,
            start_ms: 0,
            end_ms: 1_000,
            text: "este pasaje todavía no tiene vector".into(),
            confidence: None,
        });
        let mut memory = kuali_memory::Memory::in_memory().unwrap();
        memory.index(&meeting).unwrap();
        *engine.inner.memory.lock() = Some(memory);

        let error = ask_memory(
            &engine.inner,
            kuali_memory::Audience::Everything,
            "¿qué pasó?",
            kuali_memory::Asker::unknown(),
            &[],
        )
        .await
        .unwrap_err();

        assert!(matches!(error, EngineError::QuestionIndexPending(1)));
    }

    #[test]
    fn conversation_anchors_prefer_recent_unique_meetings() {
        let history = vec![
            kuali_memory::ConversationTurn {
                question: "primera".into(),
                answer: "respuesta".into(),
                meeting_ids: vec!["old".into(), "shared".into()],
            },
            kuali_memory::ConversationTurn {
                question: "segunda".into(),
                answer: "respuesta".into(),
                meeting_ids: vec!["recent".into(), "shared".into()],
            },
            kuali_memory::ConversationTurn {
                question: "tercera".into(),
                answer: "respuesta".into(),
                meeting_ids: vec!["  newest  ".into(), " ".into(), "x".repeat(161)],
            },
        ];

        assert_eq!(
            conversation_meeting_ids(&history),
            vec!["newest", "recent", "shared"]
        );
    }

    #[test]
    fn self_referential_follow_ups_add_the_askers_meeting_names_to_retrieval() {
        let asker = kuali_memory::Asker::named(vec!["Garrux".into(), "gar".into()], false);
        let history = [kuali_memory::ConversationTurn {
            question: "¿de qué iba la última reunión?".into(),
            answer: format!(
                "Fue Caché, 2FA y rutas de Vivetix. {}",
                "contexto anterior muy largo ".repeat(80)
            ),
            meeting_ids: vec!["meeting-19".into()],
        }];

        let self_query =
            conversation_retrieval_query("¿Cuál fue la última tarea que tengo?", &history, &asker);
        assert!(self_query.contains("Caché, 2FA y rutas de Vivetix"));
        assert!(self_query.contains("Garrux, gar"));
        assert!(
            self_query.find("Garrux").unwrap() < 160,
            "identity must survive lexical and embedding truncation"
        );

        let project_query =
            conversation_retrieval_query("¿Qué decidimos sobre Vivetix?", &history, &asker);
        assert!(!project_query.contains("person asking appears"));
    }

    #[test]
    fn an_anaphoric_follow_up_keeps_the_askers_fourth_task_in_evidence() {
        let asker = kuali_memory::Asker::named(vec!["Garrux".into()], false);
        let history = [kuali_memory::ConversationTurn {
            question: "¿Tengo tareas asignadas a mí en esa reunión?".into(),
            answer: "Sí, revisamos las tareas de Caché, 2FA y rutas de Vivetix.".into(),
            meeting_ids: vec!["meeting-19".into()],
        }];
        let retrieval_query = conversation_retrieval_query("¿y en esa reunión?", &history, &asker);
        assert!(
            retrieval_query.contains("Garrux"),
            "an anaphoric follow-up must inherit the prior self reference"
        );
        let screenshot_query = conversation_retrieval_query(
            "¿y en la reunión que mencionaste recientemente del 19 de agosto?",
            &history,
            &asker,
        );
        assert!(screenshot_query.contains("Garrux"));

        let mut meeting = active_meeting("meeting-19", 0).meeting;
        meeting.summary = Some(MeetingSummary {
            title: "Caché, 2FA y rutas de Vivetix".into(),
            action_items: [
                ("Pedro", "revisar los estilos"),
                ("Ana", "actualizar la documentación"),
                ("Omar", "preparar el despliegue"),
                (
                    "Garrux",
                    "conectar /{slug}/verify con Laravel sin depender del slug",
                ),
            ]
            .into_iter()
            .enumerate()
            .map(|(index, (assignee, text))| kuali_core::ActionItem {
                id: format!("task-{index}"),
                text: text.into(),
                assignee: Some(assignee.into()),
                due: None,
                source_ms: None,
                done: false,
            })
            .collect(),
            ..Default::default()
        });
        let mut memory = kuali_memory::Memory::in_memory().unwrap();
        memory.index(&meeting).unwrap();

        let passages = memory
            .evidence_with_conversation(
                &kuali_memory::Audience::Everything,
                &retrieval_query,
                &["meeting-19".into()],
                None,
            )
            .unwrap();

        assert!(passages.iter().any(|passage| {
            passage.meeting_id == "meeting-19"
                && passage.kind == kuali_memory::ChunkKind::Task
                && passage.text.contains("Garrux")
                && passage.text.contains("Laravel")
        }));
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
        let mut may_create = true;

        let summary = summarize_with_retries(
            &engine.inner,
            &mut meeting,
            &provider,
            "Spanish",
            &mut may_create,
        )
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
        let mut may_create = true;

        let error = summarize_with_retries(
            &engine.inner,
            &mut meeting,
            &provider,
            "Spanish",
            &mut may_create,
        )
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
        let mut may_create = true;

        let error = summarize_and_sync(&engine.inner, &mut meeting, &config, 42, &mut may_create)
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
                tags: Vec::new(),
                folder: None,
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
    fn deleting_the_selected_weight_prefers_another_installed_public_model() {
        let root = std::env::temp_dir().join(format!(
            "kuali-engine-model-replacement-{}",
            uuid::Uuid::new_v4()
        ));
        std::fs::create_dir_all(&root).unwrap();

        assert_eq!(
            replacement_model_after_deletion(&root, WhisperModel::LargeV3TurboQ5),
            WhisperModel::LargeV3TurboQ5
        );

        let installed = WhisperModel::LargeV3Q5;
        let file = std::fs::File::create(kuali_stt::model_path(&root, installed)).unwrap();
        file.set_len(installed.approx_bytes()).unwrap();
        assert_eq!(
            replacement_model_after_deletion(&root, WhisperModel::LargeV3TurboQ5),
            installed
        );

        std::fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn updates_wait_for_post_processing_and_model_activity() {
        let (engine, _events) = Engine::new(KualiConfig::default());
        *engine.inner.status.write() = EngineStatus::Watching;
        *engine.inner.model_state.write() = ModelState::Ready;
        assert!(engine.safe_for_update());

        engine.inner.post_processing.store(1, Ordering::Release);
        assert!(!engine.safe_for_update());
        engine.inner.post_processing.store(0, Ordering::Release);

        *engine.inner.model_state.write() = ModelState::Loading;
        assert!(!engine.safe_for_update());
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
        engine
            .inner
            .discord_connected
            .store(true, Ordering::Release);
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
        assert!(
            wait_for_status(&engine, EngineStatus::Watching, Duration::from_secs(1)).await,
            "the engine should return to watching after completion work"
        );

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
            tags: Vec::new(),
            folder: None,
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
                tags: Vec::new(),
                folder: None,
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
        engine
            .inner
            .discord_connected
            .store(true, Ordering::Release);
        let id = format!("leave-call-{}", uuid::Uuid::new_v4());
        engine
            .inner
            .active
            .lock()
            .insert(session(VoiceSource::Discord, 1), active_meeting(&id, 0));

        engine.leave_call().await.unwrap();

        assert!(engine.current_meeting().is_none());
        assert!(
            wait_for_status(&engine, EngineStatus::Watching, Duration::from_secs(1)).await,
            "leaving voice should wait for completion work before watching"
        );
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
            tags: Vec::new(),
            folder: None,
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
            is_self: false,
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
