//! Discord voice receiver.
//!
//! Discord provides separate audio per participant rather than a mixed stream.
//! `SpeakingStateUpdate` associates each RTP SSRC with a Discord user, giving
//! every subsequent packet an owner without diarization.

use std::collections::{HashMap, HashSet};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;

use kuali_core::DiscordUserId;
use parking_lot::RwLock;
use songbird::events::context_data::{DisconnectData, DisconnectReason, VoiceTick};
use songbird::model::payload::{ClientDisconnect, Speaking};
use songbird::{Event, EventContext, EventHandler as VoiceEventHandler};
use tokio::sync::mpsc::UnboundedSender;

use crate::identity::MemberResolver;
use kuali_core::VoiceEvent;

/// Initial capacity rather than a limit. The map can accept more SSRCs, while 64
/// comfortably covers the target of 30 or more simultaneous voices.
const EXPECTED_CONCURRENT_SPEAKERS: usize = 64;

/// DAVE may announce a sender without delivering PCM after a desynchronized MLS
/// join transition. Six seconds comfortably exceeds jitter-buffer delays while
/// allowing prompt automatic recovery.
const AUDIO_DECODE_TIMEOUT: Duration = Duration::from_secs(6);
const MAX_RECOVERY_ATTEMPTS: u8 = 3;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum RecoveryRequestOutcome {
    Queued,
    AlreadyRecovering,
    Exhausted,
    Unavailable,
}

#[derive(Debug)]
pub(crate) struct ReceiveRecoveryRequest {
    pub guild_id: u64,
    pub channel_id: u64,
    pub user_id: DiscordUserId,
    pub attempt: u8,
    pub control: ReceiveRecoveryControl,
}

#[derive(Debug, Clone, Copy)]
pub(crate) struct VoiceChannelContext {
    pub guild_id: u64,
    pub channel_id: u64,
}

/// State shared by the receiver and connection-renewal loop. Every voice
/// connection receives a new instance so attempts and guards cannot leak into
/// the next meeting.
#[derive(Clone, Debug, Default)]
pub(crate) struct ReceiveRecoveryControl {
    inner: Arc<ReceiveRecoveryState>,
}

#[derive(Debug, Default)]
struct ReceiveRecoveryState {
    in_progress: AtomicBool,
    suppress_requested_disconnect: AtomicBool,
    attempts: RwLock<HashMap<DiscordUserId, u8>>,
}

impl ReceiveRecoveryControl {
    fn request(
        &self,
        tx: &UnboundedSender<ReceiveRecoveryRequest>,
        guild_id: u64,
        channel_id: u64,
        user_id: DiscordUserId,
    ) -> RecoveryRequestOutcome {
        if self
            .inner
            .in_progress
            .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
            .is_err()
        {
            return RecoveryRequestOutcome::AlreadyRecovering;
        }

        let attempt = {
            let mut attempts = self.inner.attempts.write();
            let attempt = attempts.entry(user_id).or_default();
            *attempt = attempt.saturating_add(1);
            *attempt
        };
        if attempt > MAX_RECOVERY_ATTEMPTS {
            self.inner.in_progress.store(false, Ordering::Release);
            return RecoveryRequestOutcome::Exhausted;
        }

        let request = ReceiveRecoveryRequest {
            guild_id,
            channel_id,
            user_id,
            attempt,
            control: self.clone(),
        };
        if tx.send(request).is_err() {
            self.inner.in_progress.store(false, Ordering::Release);
            return RecoveryRequestOutcome::Unavailable;
        }
        RecoveryRequestOutcome::Queued
    }

    pub(crate) fn prepare_disconnect(&self) {
        self.inner
            .suppress_requested_disconnect
            .store(true, Ordering::Release);
    }

    pub(crate) fn finish(&self) {
        self.inner.in_progress.store(false, Ordering::Release);
    }

    pub(crate) fn cancel(&self) {
        self.inner
            .suppress_requested_disconnect
            .store(false, Ordering::Release);
        self.finish();
    }

    pub(crate) fn expire_disconnect_suppression(&self) {
        self.inner
            .suppress_requested_disconnect
            .store(false, Ordering::Release);
    }

    fn mark_healthy(&self, user_id: DiscordUserId) {
        self.inner.attempts.write().remove(&user_id);
    }

    fn forget(&self, user_id: DiscordUserId) {
        self.inner.attempts.write().remove(&user_id);
    }

    fn should_suppress(&self, disconnect: &DisconnectData<'_>) -> bool {
        if disconnect.reason != Some(DisconnectReason::Requested) {
            return false;
        }
        self.inner
            .suppress_requested_disconnect
            .swap(false, Ordering::AcqRel)
    }
}

#[derive(Debug, Default)]
struct DecodeWatchdog {
    pending: RwLock<HashMap<DiscordUserId, u64>>,
    next_generation: AtomicU64,
}

impl DecodeWatchdog {
    fn arm(&self, user_id: DiscordUserId) -> u64 {
        let generation = self.next_generation.fetch_add(1, Ordering::Relaxed) + 1;
        self.pending.write().insert(user_id, generation);
        generation
    }

    fn confirm_audio(&self, user_id: DiscordUserId) {
        self.pending.write().remove(&user_id);
    }

    fn expire(&self, user_id: DiscordUserId, generation: u64) -> bool {
        let mut pending = self.pending.write();
        if pending.get(&user_id) != Some(&generation) {
            return false;
        }
        pending.remove(&user_id);
        true
    }

    fn forget(&self, user_id: DiscordUserId) {
        self.pending.write().remove(&user_id);
    }

    fn clear(&self) {
        self.pending.write().clear();
    }
}

fn speaking_changes(
    previous: &HashSet<DiscordUserId>,
    active: &HashSet<DiscordUserId>,
) -> (Vec<DiscordUserId>, Vec<DiscordUserId>) {
    (
        active.difference(previous).copied().collect(),
        previous.difference(active).copied().collect(),
    )
}

fn should_capture_user(user_id: DiscordUserId, kuali_user_id: DiscordUserId) -> bool {
    user_id != kuali_user_id
}

/// SSRC-to-user table populated by `SpeakingStateUpdate` and read on each `VoiceTick`.
pub struct SsrcMap {
    inner: RwLock<HashMap<u32, DiscordUserId>>,
}

impl Default for SsrcMap {
    fn default() -> Self {
        Self {
            inner: RwLock::new(HashMap::with_capacity(EXPECTED_CONCURRENT_SPEAKERS)),
        }
    }
}

impl SsrcMap {
    pub fn insert(&self, ssrc: u32, user_id: DiscordUserId) {
        self.inner.write().insert(ssrc, user_id);
    }

    pub fn get(&self, ssrc: u32) -> Option<DiscordUserId> {
        self.inner.read().get(&ssrc).copied()
    }

    pub fn remove_user(&self, user_id: DiscordUserId) {
        self.inner.write().retain(|_, id| *id != user_id);
    }

    pub fn clear(&self) {
        self.inner.write().clear();
    }

    pub fn len(&self) -> usize {
        self.inner.read().len()
    }

    pub fn is_empty(&self) -> bool {
        self.inner.read().is_empty()
    }
}

#[derive(Clone)]
pub struct VoiceReceiver {
    ssrc_map: Arc<SsrcMap>,
    resolver: Arc<MemberResolver>,
    tx: UnboundedSender<VoiceEvent>,
    speaking_users: Arc<RwLock<HashSet<DiscordUserId>>>,
    kuali_user_id: DiscordUserId,
    guild_id: u64,
    channel_id: u64,
    recovery_tx: UnboundedSender<ReceiveRecoveryRequest>,
    recovery: ReceiveRecoveryControl,
    decode_watchdog: Arc<DecodeWatchdog>,
}

impl VoiceReceiver {
    /// Builds a standalone receiver. Full Kuali integration uses
    /// `new_with_recovery` to renew calls after DAVE failures; this constructor
    /// remains available to crate consumers.
    pub fn new(
        ssrc_map: Arc<SsrcMap>,
        resolver: Arc<MemberResolver>,
        tx: UnboundedSender<VoiceEvent>,
        kuali_user_id: DiscordUserId,
    ) -> Self {
        let (recovery_tx, recovery_rx) = tokio::sync::mpsc::unbounded_channel();
        drop(recovery_rx);
        Self::new_with_recovery(
            ssrc_map,
            resolver,
            tx,
            kuali_user_id,
            VoiceChannelContext {
                guild_id: 0,
                channel_id: 0,
            },
            recovery_tx,
            ReceiveRecoveryControl::default(),
        )
    }

    pub(crate) fn new_with_recovery(
        ssrc_map: Arc<SsrcMap>,
        resolver: Arc<MemberResolver>,
        tx: UnboundedSender<VoiceEvent>,
        kuali_user_id: DiscordUserId,
        channel: VoiceChannelContext,
        recovery_tx: UnboundedSender<ReceiveRecoveryRequest>,
        recovery: ReceiveRecoveryControl,
    ) -> Self {
        Self {
            ssrc_map,
            resolver,
            tx,
            speaking_users: Arc::new(RwLock::new(HashSet::with_capacity(
                EXPECTED_CONCURRENT_SPEAKERS,
            ))),
            kuali_user_id,
            guild_id: channel.guild_id,
            channel_id: channel.channel_id,
            recovery_tx,
            recovery,
            decode_watchdog: Arc::new(DecodeWatchdog::default()),
        }
    }

    fn send(&self, event: VoiceEvent) {
        // A dropped receiver means the meeting ended; no work or panic is needed.
        let _ = self.tx.send(event);
    }

    fn on_speaking(&self, speaking: &Speaking) {
        if let Some(user_id) = speaking.user_id {
            // Discord normally omits the sender's own track. This explicit guard
            // still prevents Kuali's spoken notice from entering the transcript
            // if behavior changes or a reconnection behaves unexpectedly.
            if !should_capture_user(user_id.0, self.kuali_user_id) {
                return;
            }
            self.ssrc_map.insert(speaking.ssrc, user_id.0);
            // This is the only event that identifies a stream owner, so resolve
            // participant metadata here as well.
            self.resolver.resolve(user_id.0);

            // `SpeakingStateUpdate` confirms Discord saw this sender. If their
            // DAVE join fails, the SSRC appears here but never in VoiceTick PCM;
            // that contrast detects failure without mistaking a quiet call.
            if speaking.speaking.microphone() {
                let user_id = user_id.0;
                let generation = self.decode_watchdog.arm(user_id);
                let watchdog = Arc::clone(&self.decode_watchdog);
                let recovery = self.recovery.clone();
                let recovery_tx = self.recovery_tx.clone();
                let voice_tx = self.tx.clone();
                let guild_id = self.guild_id;
                let channel_id = self.channel_id;
                tokio::spawn(async move {
                    tokio::time::sleep(AUDIO_DECODE_TIMEOUT).await;
                    if watchdog.expire(user_id, generation) {
                        tracing::warn!(
                            user_id,
                            "Discord anunció voz pero DAVE no entregó PCM; renovando la conexión"
                        );
                        match recovery.request(&recovery_tx, guild_id, channel_id, user_id) {
                            RecoveryRequestOutcome::Exhausted => {
                                let _ = voice_tx.send(VoiceEvent::Warning(format!(
                                    "Discord sigue sin entregar el audio del usuario {user_id} después de {MAX_RECOVERY_ATTEMPTS} intentos"
                                )));
                            }
                            RecoveryRequestOutcome::Unavailable => {
                                let _ = voice_tx.send(VoiceEvent::Warning(
                                    "el recuperador de audio de Discord dejó de estar disponible"
                                        .to_string(),
                                ));
                            }
                            RecoveryRequestOutcome::Queued
                            | RecoveryRequestOutcome::AlreadyRecovering => {}
                        }
                    }
                });
            }
        }
    }

    fn on_tick(&self, tick: &VoiceTick) {
        let mut active_users = HashSet::with_capacity(EXPECTED_CONCURRENT_SPEAKERS);
        for (ssrc, data) in &tick.speaking {
            let Some(user_id) = self.ssrc_map.get(*ssrc) else {
                // Audio can precede its ownership event by a few milliseconds.
                // Dropping 20 ms is safer than assigning it to the wrong person.
                continue;
            };
            if !should_capture_user(user_id, self.kuali_user_id) {
                continue;
            }
            let Some(pcm) = &data.decoded_voice else {
                continue;
            };
            if pcm.is_empty() {
                continue;
            }
            self.decode_watchdog.confirm_audio(user_id);
            self.recovery.mark_healthy(user_id);
            active_users.insert(user_id);
            self.send(VoiceEvent::Audio {
                user_id,
                pcm: pcm.clone(),
            });
        }

        let (started, stopped) = {
            let mut previous = self.speaking_users.write();
            let (started, stopped) = speaking_changes(&previous, &active_users);
            *previous = active_users;
            (started, stopped)
        };
        for user_id in started {
            self.send(VoiceEvent::SpeakingChanged {
                user_id,
                speaking: true,
            });
        }
        for user_id in stopped {
            self.send(VoiceEvent::SpeakingChanged {
                user_id,
                speaking: false,
            });
        }

        // Emit the tick after audio so the engine processes arrivals before
        // deciding whether a turn should close.
        self.send(VoiceEvent::Tick);
    }

    fn on_disconnect(&self, disconnect: &ClientDisconnect) {
        let user_id = disconnect.user_id.0;
        self.ssrc_map.remove_user(user_id);
        if self.speaking_users.write().remove(&user_id) {
            self.send(VoiceEvent::SpeakingChanged {
                user_id,
                speaking: false,
            });
        }
        // Forget departed users so a changed nickname resolves after rejoining.
        self.resolver.forget(user_id);
        self.decode_watchdog.forget(user_id);
        self.recovery.forget(user_id);
        self.send(VoiceEvent::ParticipantLeft(user_id));
    }

    fn on_driver_disconnect(&self, disconnect: &DisconnectData<'_>) {
        let speaking = self.speaking_users.write().drain().collect::<Vec<_>>();
        for user_id in speaking {
            self.send(VoiceEvent::SpeakingChanged {
                user_id,
                speaking: false,
            });
        }
        self.ssrc_map.clear();
        self.decode_watchdog.clear();

        // Deliberate renewal belongs to the same meeting and must not close it,
        // save a partial summary, or unload Whisper.
        if !self.recovery.should_suppress(disconnect) {
            self.send(VoiceEvent::Disconnected);
        }
    }
}

#[async_trait::async_trait]
impl VoiceEventHandler for VoiceReceiver {
    async fn act(&self, ctx: &EventContext<'_>) -> Option<Event> {
        match ctx {
            EventContext::SpeakingStateUpdate(speaking) => self.on_speaking(speaking),
            EventContext::VoiceTick(tick) => self.on_tick(tick),
            EventContext::ClientDisconnect(disconnect) => self.on_disconnect(disconnect),
            EventContext::DriverDisconnect(disconnect) => self.on_driver_disconnect(disconnect),
            _ => {}
        }
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_ssrc_resolves_to_its_user_once_discord_tells_us() {
        let map = SsrcMap::default();
        assert_eq!(map.get(1234), None);

        map.insert(1234, 42);
        assert_eq!(map.get(1234), Some(42));
    }

    #[test]
    fn a_reconnecting_user_gets_their_new_ssrc() {
        // Reconnection gives the same participant a new SSRC. Both must resolve
        // to the same user until the old mapping is cleaned up.
        let map = SsrcMap::default();
        map.insert(1, 42);
        map.insert(2, 42);
        assert_eq!(map.get(1), Some(42));
        assert_eq!(map.get(2), Some(42));
    }

    #[test]
    fn leaving_removes_every_ssrc_the_user_had() {
        let map = SsrcMap::default();
        map.insert(1, 42);
        map.insert(2, 42);
        map.insert(3, 99);

        map.remove_user(42);
        assert_eq!(map.get(1), None);
        assert_eq!(map.get(2), None);
        assert_eq!(map.get(3), Some(99), "no debería tocar a los demás");
    }

    #[test]
    fn kualis_own_voice_is_never_captured() {
        assert!(!should_capture_user(42, 42));
        assert!(should_capture_user(99, 42));
    }

    #[test]
    fn clearing_between_meetings_leaves_nothing_behind() {
        let map = SsrcMap::default();
        map.insert(1, 42);
        map.clear();
        assert!(map.is_empty());
        assert_eq!(map.len(), 0);
    }

    #[test]
    fn speaking_changes_are_only_emitted_on_transitions() {
        let previous = HashSet::from([10, 20]);
        let active = HashSet::from([20, 30]);
        let (mut started, mut stopped) = speaking_changes(&previous, &active);
        started.sort_unstable();
        stopped.sort_unstable();

        assert_eq!(started, vec![30]);
        assert_eq!(stopped, vec![10]);
        assert_eq!(speaking_changes(&active, &active), (Vec::new(), Vec::new()));
    }

    #[test]
    fn sixty_four_simultaneous_speakers_are_all_tracked() {
        let previous = HashSet::new();
        let active = (1..=64).collect::<HashSet<_>>();
        let (started, stopped) = speaking_changes(&previous, &active);

        assert_eq!(started.len(), 64);
        assert!(stopped.is_empty());
    }

    #[test]
    fn decoded_audio_disarms_the_dave_watchdog() {
        let watchdog = DecodeWatchdog::default();
        let generation = watchdog.arm(42);
        watchdog.confirm_audio(42);

        assert!(!watchdog.expire(42, generation));
    }

    #[test]
    fn only_the_latest_watchdog_generation_can_expire() {
        let watchdog = DecodeWatchdog::default();
        let stale = watchdog.arm(42);
        let current = watchdog.arm(42);

        assert!(!watchdog.expire(42, stale));
        assert!(watchdog.expire(42, current));
    }

    #[test]
    fn recovery_attempts_are_capped_per_participant() {
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        let recovery = ReceiveRecoveryControl::default();

        for expected in 1..=MAX_RECOVERY_ATTEMPTS {
            assert_eq!(
                recovery.request(&tx, 1, 2, 42),
                RecoveryRequestOutcome::Queued
            );
            let request = rx.try_recv().expect("recovery should be requested");
            assert_eq!(request.attempt, expected);
            request.control.finish();
        }
        assert_eq!(
            recovery.request(&tx, 1, 2, 42),
            RecoveryRequestOutcome::Exhausted
        );
        assert!(rx.try_recv().is_err());

        recovery.mark_healthy(42);
        assert_eq!(
            recovery.request(&tx, 1, 2, 42),
            RecoveryRequestOutcome::Queued
        );
        assert_eq!(rx.try_recv().unwrap().attempt, 1);
    }
}
