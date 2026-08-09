//! Discord bot that follows a configured user.
//!
//! Kuali must be a real bot invited to the guild. Automating a user account as a
//! selfbot violates Discord's terms and is intentionally unsupported. Kuali can
//! therefore follow users only in guilds where an authorized person invited it.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;

use kuali_core::{DiscordConfig, CONSENT_MESSAGE};
use parking_lot::RwLock;
use serenity::all::{
    ButtonStyle, ChannelId, ComponentInteraction, Context, CreateButton, CreateCommand,
    CreateInteractionResponse, CreateInteractionResponseFollowup, CreateInteractionResponseMessage,
    CreateMessage, EditInteractionResponse, EventHandler, GatewayIntents, Guild, GuildId, Http,
    Interaction, Permissions, Ready, UserId, VoiceState,
};
use serenity::Client;
use songbird::driver::{Channels, DecodeConfig, DecodeMode, SampleRate};
use songbird::{CoreEvent, SerenityInit, Songbird};
use tokio::sync::mpsc::{unbounded_channel, UnboundedReceiver, UnboundedSender};
use tokio::task::JoinHandle;

use crate::audit::{
    attach_track_audit, consent_audit_path, AnnouncementContext, AuditKind, AuditLog, AuditSubject,
};
use crate::identity::MemberResolver;
use crate::receiver::{
    ReceiveRecoveryControl, ReceiveRecoveryRequest, SsrcMap, VoiceChannelContext, VoiceReceiver,
};
use crate::speech::load_consent_audio;
use kuali_core::{CallInfo, VoiceEvent, VoiceSessionId};

const TRANSCRIPT_BUTTON_PREFIX: &str = "kuali:transcript:";
const RECORD_COMMAND_ES: &str = "grabar";
const RECORD_COMMAND_EN: &str = "record";
static NEXT_VOICE_SESSION_ID: AtomicU64 = AtomicU64::new(1);

fn send_session(tx: &UnboundedSender<VoiceEvent>, session_id: VoiceSessionId, event: VoiceEvent) {
    let _ = tx.send(VoiceEvent::Session {
        session_id,
        event: Box::new(event),
    });
}

fn discord_username_matches(configured: &str, actual: &str) -> bool {
    configured
        .trim()
        .trim_start_matches('@')
        .trim()
        .to_lowercase()
        == actual.trim().to_lowercase()
}

fn is_record_command(name: &str) -> bool {
    matches!(name, RECORD_COMMAND_ES | RECORD_COMMAND_EN)
}

fn session_sender(
    parent: UnboundedSender<VoiceEvent>,
    session_id: VoiceSessionId,
) -> UnboundedSender<VoiceEvent> {
    let (tx, mut rx) = unbounded_channel();
    tokio::spawn(async move {
        while let Some(event) = rx.recv().await {
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
    tx
}

fn transcript_button_id(meeting_id: &str) -> String {
    format!("{TRANSCRIPT_BUTTON_PREFIX}{meeting_id}")
}

fn transcript_id_from_button(custom_id: &str) -> Option<&str> {
    custom_id
        .strip_prefix(TRANSCRIPT_BUTTON_PREFIX)
        .filter(|meeting_id| !meeting_id.is_empty())
}

#[derive(Debug, thiserror::Error)]
pub enum DiscordError {
    #[error("falta el token del bot de Discord")]
    MissingToken,
    #[error("Discord rejected the connection: {0}")]
    Client(#[from] serenity::Error),
    #[error("no pude preparar el aviso hablado obligatorio: {0}")]
    Speech(String),
    #[error("no pude abrir el registro de consentimiento: {0}")]
    Audit(String),
}

/// Kuali's current voice location.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct CurrentCall {
    session_id: VoiceSessionId,
    guild_id: GuildId,
    channel_id: ChannelId,
    origin: CallOrigin,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum CallOrigin {
    FollowedUser,
    SlashCommand,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum JoinOutcome {
    Joined,
    AlreadyHere,
    Busy,
    Failed,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum FollowUsernameResolution {
    Unique(UserId),
    Ambiguous,
    NotFound,
    Unavailable,
}

/// Keeps an explicit UI departure from being undone by ordinary voice-state
/// updates such as mute, camera, or screen-share changes. The pause belongs to
/// one channel and ends when the followed user leaves it, moves elsewhere, or
/// explicitly invites Kuali again.
#[derive(Debug, Default)]
struct ManualFollowPause {
    channel: Option<(GuildId, ChannelId)>,
}

impl ManualFollowPause {
    fn block(&mut self, guild_id: GuildId, channel_id: ChannelId) {
        self.channel = Some((guild_id, channel_id));
    }

    fn clear(&mut self) {
        self.channel = None;
    }

    fn should_follow(&mut self, guild_id: GuildId, channel_id: ChannelId) -> bool {
        match self.channel {
            Some(blocked) if blocked == (guild_id, channel_id) => false,
            Some(_) => {
                // Moving to another channel is a real transition, so automatic
                // following resumes immediately at the new location.
                self.clear();
                true
            }
            None => true,
        }
    }
}

struct Handler {
    config: Arc<RwLock<DiscordConfig>>,
    tx: UnboundedSender<VoiceEvent>,
    current: Arc<RwLock<Option<CurrentCall>>>,
    manual_follow_pause: Arc<RwLock<ManualFollowPause>>,
    consent_audio: Arc<[u8]>,
    audit: Arc<AuditLog>,
    recovery_tx: UnboundedSender<ReceiveRecoveryRequest>,
}

impl Handler {
    fn follow_user(&self) -> Option<UserId> {
        self.config
            .read()
            .automatic_follow_user_id()
            .map(UserId::new)
    }

    fn follow_username(&self) -> Option<String> {
        self.config
            .read()
            .automatic_follow_username()
            .map(str::to_owned)
    }

    async fn persist_follow_user(&self, user_id: UserId) -> bool {
        let (reply, response) = tokio::sync::oneshot::channel();
        if self
            .tx
            .send(VoiceEvent::FollowRequested {
                user_id: user_id.get(),
                reply,
            })
            .is_err()
        {
            return false;
        }
        matches!(
            tokio::time::timeout(Duration::from_secs(10), response).await,
            Ok(Ok(Ok(())))
        )
    }

    async fn resolve_follow_username(
        &self,
        ctx: &Context,
        guilds: &[GuildId],
    ) -> FollowUsernameResolution {
        let Some(expected) = self.follow_username() else {
            return FollowUsernameResolution::NotFound;
        };
        let mut matched_ids = Vec::new();
        let mut had_search_error = false;
        for guild_id in guilds {
            match guild_id
                .search_members(&ctx.http, &expected, Some(1_000))
                .await
            {
                Ok(members) => {
                    matched_ids.extend(
                        members
                            .into_iter()
                            .filter(|member| !member.user.bot)
                            .filter(|member| discord_username_matches(&expected, &member.user.name))
                            .map(|member| member.user.id),
                    );
                }
                Err(error) => {
                    had_search_error = true;
                    tracing::debug!(
                        %guild_id,
                        %error,
                        "Discord no permitió buscar el @usuario en este servidor"
                    );
                }
            }
        }
        matched_ids.sort_unstable();
        matched_ids.dedup();
        // A failed response could hide another account with the same username.
        // Do not persist an ID until ambiguity can be ruled out.
        if had_search_error {
            return FollowUsernameResolution::Unavailable;
        }
        match matched_ids.as_slice() {
            [user_id] => FollowUsernameResolution::Unique(*user_id),
            [] => FollowUsernameResolution::NotFound,
            _ => FollowUsernameResolution::Ambiguous,
        }
    }

    async fn resolve_follow_from_voice_state(&self, ctx: &Context, state: &VoiceState) -> bool {
        if let Some(user_id) = self.follow_user() {
            return user_id == state.user_id;
        }
        let Some(expected) = self.follow_username() else {
            return false;
        };
        let Some(member) = state.member.as_ref() else {
            return false;
        };
        if member.user.bot || !discord_username_matches(&expected, &member.user.name) {
            return false;
        }

        let guilds = ctx.cache.guilds();
        match self.resolve_follow_username(ctx, &guilds).await {
            FollowUsernameResolution::Unique(user_id) if user_id == state.user_id => {
                self.persist_follow_user(user_id).await
            }
            FollowUsernameResolution::Ambiguous => {
                let _ = self.tx.send(VoiceEvent::Warning(
                    "Hay varias cuentas con ese nombre de usuario en los servidores compartidos; revisa el @usuario en Ajustes → Discord."
                        .into(),
                ));
                false
            }
            FollowUsernameResolution::Unique(_) => false,
            FollowUsernameResolution::NotFound | FollowUsernameResolution::Unavailable => false,
        }
    }

    async fn join(
        &self,
        ctx: &Context,
        guild_id: GuildId,
        channel_id: ChannelId,
        origin: CallOrigin,
    ) -> JoinOutcome {
        if origin == CallOrigin::SlashCommand {
            // An explicit /record or /grabar request overrides a previous
            // departure selected in Kuali's desktop UI.
            self.manual_follow_pause.write().clear();
        }
        if let Some(current) = *self.current.read() {
            if current.guild_id == guild_id && current.channel_id == channel_id {
                return JoinOutcome::AlreadyHere;
            }
            // A manual invitation must never interrupt another meeting.
            // Configured following may still move behind its user across channels.
            if origin == CallOrigin::SlashCommand {
                return JoinOutcome::Busy;
            }
        }

        let Some(manager) = songbird::get(ctx).await else {
            let _ = self.tx.send(VoiceEvent::Warning(
                "songbird no está inicializado".to_string(),
            ));
            return JoinOutcome::Failed;
        };

        let info = CallInfo {
            guild_id: guild_id.get(),
            guild_name: guild_name(ctx, guild_id).await,
            channel_id: channel_id.get(),
            channel_name: channel_name(ctx, channel_id).await,
            // Discord voice channels have integrated chat, so the same ID is a
            // valid message destination.
            text_channel_id: channel_id.get(),
        };
        let session_id = NEXT_VOICE_SESSION_ID.fetch_add(1, Ordering::Relaxed);
        let session_tx = session_sender(self.tx.clone(), session_id);
        let (reply, decision) = tokio::sync::oneshot::channel();
        if session_tx
            .send(VoiceEvent::ConnectionRequested {
                info: info.clone(),
                reply,
            })
            .is_err()
        {
            return JoinOutcome::Failed;
        }
        match decision.await {
            Ok(Ok(())) => {}
            Ok(Err(_)) => return JoinOutcome::Busy,
            Err(_) => return JoinOutcome::Failed,
        }
        // Admission above is immediate. The engine begins loading Whisper while
        // Songbird joins; audio arriving in that interval queues behind it.
        let _ = session_tx.send(VoiceEvent::Connected(info));

        let ssrc_map = Arc::new(SsrcMap::default());
        let resolver = Arc::new(MemberResolver::new(
            Arc::clone(&ctx.http),
            guild_id,
            session_tx.clone(),
        ));
        let receiver = VoiceReceiver::new_with_recovery(
            ssrc_map,
            Arc::clone(&resolver),
            session_tx.clone(),
            ctx.cache.current_user().id.get(),
            VoiceChannelContext {
                guild_id: guild_id.get(),
                channel_id: channel_id.get(),
            },
            self.recovery_tx.clone(),
            ReceiveRecoveryControl::default(),
        );

        // Register handlers before opening RTP. When Kuali joins after others,
        // Songbird may deliver initial SSRC state during `join`; registering later
        // used to leave the first speaker unknown and discard their PCM.
        let call = manager.get_or_insert(guild_id);
        {
            let mut handler = call.lock().await;
            handler.add_global_event(CoreEvent::SpeakingStateUpdate.into(), receiver.clone());
            handler.add_global_event(CoreEvent::VoiceTick.into(), receiver.clone());
            handler.add_global_event(CoreEvent::ClientDisconnect.into(), receiver.clone());
            handler.add_global_event(CoreEvent::DriverDisconnect.into(), receiver);
        }

        let call = match manager.join(guild_id, channel_id).await {
            Ok(call) => call,
            Err(e) => {
                let _ = manager.remove(guild_id).await;
                let _ = session_tx.send(VoiceEvent::Disconnected);
                let _ = self
                    .tx
                    .send(VoiceEvent::Warning(format!("no pude entrar al canal: {e}")));
                return JoinOutcome::Failed;
            }
        };

        {
            let mut handler = call.lock().await;
            // Kuali listens during meetings but must be unmuted to play the
            // mandatory consent notice.
            if let Err(error) = handler.deafen(false).await {
                let _ = self.tx.send(VoiceEvent::Warning(format!(
                    "no pude habilitar la escucha de Kuali: {error}"
                )));
            }
            if let Err(error) = handler.mute(false).await {
                let _ = self.tx.send(VoiceEvent::Warning(format!(
                    "no pude habilitar la voz de Kuali: {error}"
                )));
            }
        }

        // Discord's cache knows members already in the channel before they speak.
        // Announce them to the UI immediately; the registered SpeakingStateUpdate
        // later binds each SSRC to audio.
        let bot_id = ctx.cache.current_user().id;
        let initial_users = ctx
            .cache
            .guild(guild_id)
            .map(|guild| {
                guild
                    .voice_states
                    .values()
                    .filter(|state| state.channel_id == Some(channel_id))
                    .filter(|state| state.user_id != bot_id)
                    .filter(|state| {
                        state
                            .member
                            .as_ref()
                            .map(|member| !member.user.bot)
                            .unwrap_or(true)
                    })
                    .map(|state| state.user_id.get())
                    .collect::<Vec<_>>()
            })
            .unwrap_or_default();
        for user_id in initial_users {
            resolver.resolve(user_id);
        }

        *self.current.write() = Some(CurrentCall {
            session_id,
            guild_id,
            channel_id,
            origin,
        });

        // Existing participants receive one notice when Kuali joins, not one
        // notice per person already present.
        let subject = {
            let bot = ctx.cache.current_user();
            AuditSubject {
                user_id: bot.id.get(),
                display_name: bot.name.clone(),
            }
        };
        let audit_context =
            AnnouncementContext::for_join(guild_id.get(), channel_id.get(), subject);
        self.record_audit(AuditKind::KualiJoined, &audit_context, None);
        self.announce_consent(ctx, guild_id, channel_id, audit_context)
            .await;
        JoinOutcome::Joined
    }

    async fn announce_consent(
        &self,
        ctx: &Context,
        guild_id: GuildId,
        channel_id: ChannelId,
        audit_context: AnnouncementContext,
    ) {
        let audit_context = audit_context.with_announcement();
        self.record_audit(
            AuditKind::AnnouncementQueued,
            &audit_context,
            Some("aviso de voz añadido a la cola"),
        );

        let message = CreateMessage::new().content(CONSENT_MESSAGE);
        match channel_id.send_message(&ctx.http, message).await {
            Ok(_) => self.record_audit(
                AuditKind::TextNoticePosted,
                &audit_context,
                Some("Discord confirmó la publicación en el chat del canal"),
            ),
            Err(e) => {
                self.record_audit(
                    AuditKind::TextNoticeFailed,
                    &audit_context,
                    Some(&e.to_string()),
                );
                let _ = self.tx.send(VoiceEvent::Warning(format!(
                    "no pude publicar el aviso de grabación; revisa View Channel y Send Messages: {e}"
                )));
            }
        }

        let Some(manager) = songbird::get(ctx).await else {
            self.record_audit(
                AuditKind::AnnouncementFailed,
                &audit_context,
                Some("Songbird no estaba disponible"),
            );
            return;
        };
        let Some(call) = manager.get(guild_id) else {
            self.record_audit(
                AuditKind::AnnouncementFailed,
                &audit_context,
                Some("la llamada ya no estaba disponible"),
            );
            return;
        };
        // Queue notices so arrivals a few seconds apart cannot overlap playback.
        let track = call
            .lock()
            .await
            .enqueue_input(Arc::clone(&self.consent_audio).into())
            .await;
        if let Err(error) = attach_track_audit(
            &track,
            Arc::clone(&self.audit),
            self.tx.clone(),
            audit_context.clone(),
        ) {
            self.record_audit(AuditKind::AnnouncementFailed, &audit_context, Some(&error));
            let _ = self.tx.send(VoiceEvent::Warning(error));
        }
    }

    fn record_audit(&self, kind: AuditKind, context: &AnnouncementContext, detail: Option<&str>) {
        if let Err(error) = self.audit.record(kind, context, detail) {
            let _ = self.tx.send(VoiceEvent::Warning(format!(
                "no pude escribir el registro de consentimiento en {}: {error}",
                self.audit.path().display()
            )));
        }
    }

    async fn human_subject(&self, ctx: &Context, state: &VoiceState) -> Option<AuditSubject> {
        let bot_id = ctx.cache.current_user().id;
        if state.user_id == bot_id {
            return None;
        }
        if let Some(member) = state.member.as_ref() {
            return (!member.user.bot).then(|| AuditSubject {
                user_id: state.user_id.get(),
                display_name: member.display_name().to_string(),
            });
        }
        match state.guild_id {
            Some(guild_id) => match guild_id.member(&ctx.http, state.user_id).await {
                Ok(member) if !member.user.bot => Some(AuditSubject {
                    user_id: state.user_id.get(),
                    display_name: member.display_name().to_string(),
                }),
                Ok(_) => None,
                Err(_) => Some(AuditSubject {
                    user_id: state.user_id.get(),
                    display_name: format!("Usuario {}", state.user_id),
                }),
            },
            None => Some(AuditSubject {
                user_id: state.user_id.get(),
                display_name: format!("Usuario {}", state.user_id),
            }),
        }
    }

    async fn handle_record_command(
        &self,
        ctx: &Context,
        command: &serenity::all::CommandInteraction,
    ) {
        let english = command.data.name == RECORD_COMMAND_EN;
        let command_name = if english { "/record" } else { "/grabar" };
        let Some(guild_id) = command.guild_id else {
            let response = CreateInteractionResponse::Message(
                CreateInteractionResponseMessage::new()
                    .content(if english {
                        "Use /record inside a server."
                    } else {
                        "Usa /grabar dentro de un servidor."
                    })
                    .ephemeral(true),
            );
            let _ = command.create_response(&ctx.http, response).await;
            return;
        };

        let user_id = command
            .member
            .as_ref()
            .map(|member| member.user.id)
            .unwrap_or(command.user.id);
        let channel_id = ctx.cache.guild(guild_id).and_then(|guild| {
            guild
                .voice_states
                .get(&user_id)
                .and_then(|state| state.channel_id)
        });

        let Some(channel_id) = channel_id else {
            let response = CreateInteractionResponse::Message(
                CreateInteractionResponseMessage::new()
                    .content(if english {
                        "Join a voice channel first, then use /record again.".to_string()
                    } else {
                        format!("Entra primero a un canal de voz y vuelve a usar {command_name}.")
                    })
                    .ephemeral(true),
            );
            let _ = command.create_response(&ctx.http, response).await;
            return;
        };

        let deferred = CreateInteractionResponse::Defer(
            CreateInteractionResponseMessage::new().ephemeral(true),
        );
        if command.create_response(&ctx.http, deferred).await.is_err() {
            return;
        }

        let outcome = self
            .join(ctx, guild_id, channel_id, CallOrigin::SlashCommand)
            .await;
        let content = match (english, outcome) {
            (true, JoinOutcome::Joined) => {
                "Done: I joined the channel and I am recording and transcribing."
            }
            (true, JoinOutcome::AlreadyHere) => {
                "I am already recording and transcribing this channel."
            }
            (true, JoinOutcome::Busy) => "I am currently recording another call.",
            (true, JoinOutcome::Failed) => {
                "I could not join. Check my View Channel and Connect permissions."
            }
            (false, JoinOutcome::Joined) => {
                "Listo: entré al canal y ya estoy grabando y transcribiendo."
            }
            (false, JoinOutcome::AlreadyHere) => "Ya estoy grabando y transcribiendo este canal.",
            (false, JoinOutcome::Busy) => "Ahora mismo estoy grabando otra llamada.",
            (false, JoinOutcome::Failed) => {
                "No pude entrar. Revisa mis permisos de View Channel y Connect."
            }
        };
        let _ = command
            .edit_response(&ctx.http, EditInteractionResponse::new().content(content))
            .await;
    }

    async fn request_transcript(&self, meeting_id: String, guild_id: GuildId) -> String {
        let (reply, response) = tokio::sync::oneshot::channel();
        if self
            .tx
            .send(VoiceEvent::TranscriptRequested {
                meeting_id,
                guild_id: guild_id.get(),
                reply,
            })
            .is_err()
        {
            return "Kuali no pudo consultar las reuniones ahora mismo.".to_string();
        }

        match tokio::time::timeout(Duration::from_secs(60), response).await {
            Ok(Ok(Ok(text))) => text,
            Ok(Ok(Err(message))) => message,
            Ok(Err(_)) | Err(_) => "La consulta de la transcripción tardó demasiado.".to_string(),
        }
    }

    async fn handle_transcription_button(
        &self,
        ctx: &Context,
        component: &ComponentInteraction,
        meeting_id: String,
    ) {
        let Some(guild_id) = component.guild_id else {
            return;
        };
        let deferred = CreateInteractionResponse::Defer(
            CreateInteractionResponseMessage::new().ephemeral(true),
        );
        if component
            .create_response(&ctx.http, deferred)
            .await
            .is_err()
        {
            return;
        }

        let text = self.request_transcript(meeting_id, guild_id).await;
        let mut chunks = split_message(&text, 1_900).into_iter();
        let Some(first) = chunks.next() else {
            return;
        };
        if component
            .edit_response(&ctx.http, EditInteractionResponse::new().content(first))
            .await
            .is_err()
        {
            return;
        }
        for chunk in chunks {
            if component
                .create_followup(
                    &ctx.http,
                    CreateInteractionResponseFollowup::new()
                        .content(chunk)
                        .ephemeral(true),
                )
                .await
                .is_err()
            {
                break;
            }
        }
    }

    async fn leave(&self, ctx: &Context, guild_id: GuildId) {
        let Some(current) = self.current.write().take() else {
            return;
        };

        if let Some(manager) = songbird::get(ctx).await {
            let _ = manager.remove(guild_id).await;
        }
        send_session(&self.tx, current.session_id, VoiceEvent::Disconnected);
    }

    /// Whether any human remains in Kuali's current channel.
    fn humans_left(&self, guild: &Guild, channel_id: ChannelId) -> usize {
        guild
            .voice_states
            .values()
            .filter(|state| state.channel_id == Some(channel_id))
            .filter(|state| {
                // When Discord omits member metadata, assume human presence;
                // staying too long is safer than ending a live meeting.
                state.member.as_ref().map(|m| !m.user.bot).unwrap_or(true)
            })
            .count()
    }
}

#[serenity::async_trait]
impl EventHandler for Handler {
    async fn ready(&self, _ctx: Context, ready: Ready) {
        tracing::info!(bot = %ready.user.name, "Kuali conectado a Discord");
    }

    /// Checks for an existing call after cache readiness so starting Kuali mid-
    /// meeting works without requiring the followed user to leave and rejoin.
    async fn cache_ready(&self, ctx: Context, guilds: Vec<GuildId>) {
        // Guild commands appear immediately unlike global commands. Replacing
        // the list also removes obsolete Kuali definitions.
        for guild_id in &guilds {
            let record_es = CreateCommand::new(RECORD_COMMAND_ES)
                .description("Invita a Kuali a grabar y transcribir tu canal de voz")
                .default_member_permissions(Permissions::CONNECT)
                .dm_permission(false);
            let record_en = CreateCommand::new(RECORD_COMMAND_EN)
                .description("Invite Kuali to record and transcribe your voice channel")
                .default_member_permissions(Permissions::CONNECT)
                .dm_permission(false);
            if let Err(e) = guild_id
                .set_commands(&ctx.http, vec![record_es, record_en])
                .await
            {
                let _ = self.tx.send(VoiceEvent::Warning(format!(
                    "no pude registrar los comandos /grabar y /record: {e}"
                )));
            }
        }

        // Search returns username and nickname matches, so filter by exact real
        // username. Unlike listing all members, this endpoint needs no privileged intent.
        if self.follow_user().is_none() {
            if self.follow_username().is_none() {
                return;
            }
            match self.resolve_follow_username(&ctx, &guilds).await {
                FollowUsernameResolution::Unique(user_id) => {
                    let _ = self.persist_follow_user(user_id).await;
                }
                FollowUsernameResolution::Ambiguous => {
                    let _ = self.tx.send(VoiceEvent::Warning(
                    "Hay varias cuentas con ese nombre de usuario en los servidores compartidos; revisa el @usuario en Ajustes → Discord."
                        .into(),
                    ));
                }
                FollowUsernameResolution::NotFound => {
                    let username = self.follow_username().unwrap_or_default();
                    let _ = self.tx.send(VoiceEvent::Warning(format!(
                        "No encontré a @{username} en los servidores compartidos. Comprueba el @usuario y que el bot ya esté invitado."
                    )));
                }
                FollowUsernameResolution::Unavailable => {
                    let _ = self.tx.send(VoiceEvent::Warning(
                        "Discord no permitió comprobar el @usuario en todos los servidores compartidos; Kuali no guardará un ID sin verificarlo."
                            .into(),
                    ));
                }
            }
        }

        let Some(follow) = self.follow_user() else {
            return;
        };

        for guild_id in guilds {
            // The cache guard is not `Send`; copy the value and drop it before
            // any `await`.
            let channel_id = {
                let Some(guild) = ctx.cache.guild(guild_id) else {
                    continue;
                };
                guild
                    .voice_states
                    .get(&follow)
                    .and_then(|state| state.channel_id)
            };

            if let Some(channel_id) = channel_id {
                tracing::info!(%guild_id, %channel_id, "ya estabas en una llamada, entrando");
                self.join(&ctx, guild_id, channel_id, CallOrigin::FollowedUser)
                    .await;
                return;
            }
        }
    }

    async fn interaction_create(&self, ctx: Context, interaction: Interaction) {
        match interaction {
            Interaction::Command(command) => match command.data.name.as_str() {
                name if is_record_command(name) => self.handle_record_command(&ctx, &command).await,
                _ => {}
            },
            Interaction::Component(component) => {
                if let Some(meeting_id) =
                    transcript_id_from_button(&component.data.custom_id).map(str::to_string)
                {
                    self.handle_transcription_button(&ctx, &component, meeting_id)
                        .await;
                }
            }
            _ => {}
        }
    }

    async fn voice_state_update(&self, ctx: Context, old: Option<VoiceState>, new: VoiceState) {
        if self.resolve_follow_from_voice_state(&ctx, &new).await {
            match (new.guild_id, new.channel_id) {
                // The followed user joined or moved to a channel.
                (Some(guild_id), Some(channel_id)) => {
                    if !self
                        .manual_follow_pause
                        .write()
                        .should_follow(guild_id, channel_id)
                    {
                        // Discord also emits voice-state updates when screen
                        // sharing, video, mute, or other flags change. Remaining
                        // in the blocked channel is not a new join.
                        return;
                    }
                    let entered_channel =
                        old.as_ref().and_then(|state| state.channel_id) != Some(channel_id);
                    let join_context = if entered_channel {
                        self.human_subject(&ctx, &new).await.map(|subject| {
                            AnnouncementContext::for_join(guild_id.get(), channel_id.get(), subject)
                        })
                    } else {
                        None
                    };
                    if let Some(audit_context) = join_context.as_ref() {
                        self.record_audit(AuditKind::ParticipantJoined, audit_context, None);
                    }

                    let entered_existing_call = self.current.read().is_some_and(|call| {
                        call.guild_id == guild_id
                            && call.channel_id == channel_id
                            && entered_channel
                    });
                    let outcome = self
                        .join(&ctx, guild_id, channel_id, CallOrigin::FollowedUser)
                        .await;
                    if entered_existing_call && outcome == JoinOutcome::AlreadyHere {
                        if let Some(audit_context) = join_context {
                            self.announce_consent(&ctx, guild_id, channel_id, audit_context)
                                .await;
                        }
                    }
                }
                // The followed user disconnected.
                (guild_id, None) => {
                    // The next real channel entry may be followed again.
                    self.manual_follow_pause.write().clear();
                    let current = *self.current.read();
                    if current.map(|call| call.origin) == Some(CallOrigin::FollowedUser) {
                        let guild_id = guild_id
                            .or_else(|| old.as_ref().and_then(|state| state.guild_id))
                            .or_else(|| current.map(|call| call.guild_id));
                        if let Some(guild_id) = guild_id {
                            self.leave(&ctx, guild_id).await;
                        }
                        return;
                    }
                }
                _ => {}
            }
            if new.channel_id.is_some() {
                return;
            }
        }

        let Some(current) = *self.current.read() else {
            return;
        };

        // Anyone joining after Kuali receives an individual notice. Existing
        // participants received only the initial announcement.
        let entered = new.channel_id == Some(current.channel_id)
            && old.as_ref().and_then(|state| state.channel_id) != Some(current.channel_id);
        if entered {
            if let Some(subject) = self.human_subject(&ctx, &new).await {
                let audit_context = AnnouncementContext::for_join(
                    current.guild_id.get(),
                    current.channel_id.get(),
                    subject,
                );
                self.record_audit(AuditKind::ParticipantJoined, &audit_context, None);
                self.announce_consent(&ctx, current.guild_id, current.channel_id, audit_context)
                    .await;
            }
        }

        if !self.config.read().leave_when_empty {
            return;
        }

        // After another member moves, leave an empty-human channel instead of
        // wasting model memory and battery.
        let empty = {
            match ctx.cache.guild(current.guild_id) {
                Some(guild) => self.humans_left(&guild, current.channel_id) == 0,
                None => false,
            }
        };
        if empty {
            tracing::info!("voice channel is empty; leaving");
            self.leave(&ctx, current.guild_id).await;
        }
    }
}

async fn guild_name(ctx: &Context, guild_id: GuildId) -> String {
    if let Some(guild) = ctx.cache.guild(guild_id) {
        return guild.name.clone();
    }
    guild_id
        .to_partial_guild(&ctx.http)
        .await
        .map(|g| g.name)
        .unwrap_or_else(|_| format!("Servidor {guild_id}"))
}

async fn channel_name(ctx: &Context, channel_id: ChannelId) -> String {
    channel_id
        .to_channel(&ctx.http)
        .await
        .ok()
        .and_then(|c| c.guild())
        .map(|c| c.name)
        .unwrap_or_else(|| format!("Canal {channel_id}"))
}

/// Bot control handle used to publish summaries and shut down.
pub struct DiscordHandle {
    http: Arc<Http>,
    songbird: Arc<Songbird>,
    shard_manager: Arc<serenity::gateway::ShardManager>,
    current: Arc<RwLock<Option<CurrentCall>>>,
    manual_follow_pause: Arc<RwLock<ManualFollowPause>>,
    config: Arc<RwLock<DiscordConfig>>,
    task: JoinHandle<()>,
    recovery_task: JoinHandle<()>,
}

impl DiscordHandle {
    /// Applies settings that require no new gateway session. Pausing automatic
    /// following must never end an active call.
    pub fn update_config(&self, config: DiscordConfig) {
        let previous = self.config.read().clone();
        let follow_was_explicitly_resumed =
            !previous.follow_automatically && config.follow_automatically;
        let followed_user_changed = previous.follow_user_id != config.follow_user_id
            || previous.follow_username != config.follow_username;
        if follow_was_explicitly_resumed || followed_user_changed {
            self.manual_follow_pause.write().clear();
        }
        *self.config.write() = config;
    }

    /// Publishes the summary and attaches transcript access to the final chunk,
    /// even when Discord requires splitting the message.
    pub async fn post_summary(
        &self,
        channel_id: u64,
        text: &str,
        meeting_id: &str,
    ) -> Result<(), serenity::Error> {
        // Discord limits messages to 2,000 characters, so split long summaries
        // into line-aware chunks.
        let chunks = split_message(text, 1_900);
        let last = chunks.len().saturating_sub(1);
        for (index, chunk) in chunks.into_iter().enumerate() {
            let mut message = CreateMessage::new().content(chunk);
            if index == last {
                message = message.button(
                    CreateButton::new(transcript_button_id(meeting_id))
                        .label("Ver transcripción completa")
                        .style(ButtonStyle::Primary),
                );
            }
            ChannelId::new(channel_id)
                .send_message(&self.http, message)
                .await?;
        }
        Ok(())
    }

    /// Leaves the current voice channel, if any.
    pub async fn leave_call(&self) {
        let current = self.current.write().take();
        if let Some(current) = current {
            self.manual_follow_pause
                .write()
                .block(current.guild_id, current.channel_id);
            let _ = self.songbird.remove(current.guild_id).await;
        }
    }

    pub async fn shutdown(self) {
        self.leave_call().await;
        self.shard_manager.shutdown_all().await;
        self.task.abort();
        self.recovery_task.abort();
    }
}

/// Songbird 0.6 can miss the key for a participant joining after the bot during
/// a DAVE/MLS transition. The receiver detects announced speech without PCM and
/// this loop renews the voice session. `leave` preserves Songbird handlers, so
/// the meeting, Whisper instance, and transcript remain unchanged.
async fn run_receive_recovery(
    songbird: Arc<Songbird>,
    current: Arc<RwLock<Option<CurrentCall>>>,
    tx: UnboundedSender<VoiceEvent>,
    mut rx: UnboundedReceiver<ReceiveRecoveryRequest>,
) {
    while let Some(request) = rx.recv().await {
        let guild_id = GuildId::new(request.guild_id);
        let channel_id = ChannelId::new(request.channel_id);
        let is_current = current
            .read()
            .is_some_and(|call| call.guild_id == guild_id && call.channel_id == channel_id);
        if !is_current {
            request.control.cancel();
            continue;
        }

        tracing::warn!(
            user_id = request.user_id,
            attempt = request.attempt,
            "renovando la conexión de voz para recuperar audio DAVE"
        );
        request.control.prepare_disconnect();
        if let Err(error) = songbird.leave(guild_id).await {
            request.control.cancel();
            let _ = tx.send(VoiceEvent::Warning(format!(
                "no pude renovar la escucha de Discord: {error}"
            )));
            continue;
        }

        // Give the gateway time to confirm departure before reconnecting, avoiding
        // reuse of the DAVE session just identified as inconsistent.
        tokio::time::sleep(Duration::from_millis(500)).await;
        match songbird.join(guild_id, channel_id).await {
            Ok(_) => {
                request.control.finish();
                let control = request.control.clone();
                tokio::spawn(async move {
                    // DriverDisconnect normally consumes this guard. If it did
                    // not, the guard must not hide a later manual departure.
                    tokio::time::sleep(Duration::from_secs(2)).await;
                    control.expire_disconnect_suppression();
                });
                tracing::info!(
                    user_id = request.user_id,
                    attempt = request.attempt,
                    "conexión de voz renovada; esperando PCM"
                );
            }
            Err(error) => {
                request.control.cancel();
                let removed = {
                    let mut active = current.write();
                    if active.is_some_and(|call| {
                        call.guild_id == guild_id && call.channel_id == channel_id
                    }) {
                        active.take()
                    } else {
                        None
                    }
                };
                let _ = tx.send(VoiceEvent::Warning(format!(
                    "Discord no permitió restaurar la escucha de la llamada: {error}"
                )));
                if let Some(call) = removed {
                    send_session(&tx, call.session_id, VoiceEvent::Disconnected);
                }
            }
        }
    }
}

/// Splits text along line boundaries to avoid cutting sentences in Discord.
pub fn split_message(text: &str, limit: usize) -> Vec<String> {
    let mut chunks = Vec::new();
    let mut current = String::new();

    for line in text.lines() {
        // A line exceeding the limit must be split by characters. The result is
        // imperfect but preferable to Discord rejecting the entire message.
        if line.len() > limit {
            if !current.is_empty() {
                chunks.push(std::mem::take(&mut current));
            }
            let mut rest = line;
            while !rest.is_empty() {
                let cut = rest
                    .char_indices()
                    .take_while(|(i, _)| *i < limit)
                    .last()
                    .map(|(i, c)| i + c.len_utf8())
                    .unwrap_or(rest.len());
                chunks.push(rest[..cut].to_string());
                rest = &rest[cut..];
            }
            continue;
        }

        if current.len() + line.len() + 1 > limit {
            chunks.push(std::mem::take(&mut current));
        }
        if !current.is_empty() {
            current.push('\n');
        }
        current.push_str(line);
    }

    if !current.is_empty() {
        chunks.push(current);
    }
    chunks
}

/// Starts the bot and returns once connected while the gateway continues in its task.
pub async fn start(
    config: DiscordConfig,
    tx: UnboundedSender<VoiceEvent>,
) -> Result<DiscordHandle, DiscordError> {
    if config.bot_token.trim().is_empty() {
        return Err(DiscordError::MissingToken);
    }
    // Do not record without voice and audit setup. Consent is a meeting
    // precondition rather than an optional enhancement.
    let consent_audio = load_consent_audio().await.map_err(DiscordError::Speech)?;
    let audit_path = consent_audit_path();
    let audit = Arc::new(
        AuditLog::open(&audit_path)
            .map_err(|error| DiscordError::Audit(format!("{}: {error}", audit_path.display())))?,
    );
    let current = Arc::new(RwLock::new(None));
    let manual_follow_pause = Arc::new(RwLock::new(ManualFollowPause::default()));
    let (recovery_tx, recovery_rx) = unbounded_channel();
    let recovery_voice_tx = tx.clone();
    let live_config = Arc::new(RwLock::new(config.clone()));
    let handler = Handler {
        config: Arc::clone(&live_config),
        tx,
        current: Arc::clone(&current),
        manual_follow_pause: Arc::clone(&manual_follow_pause),
        consent_audio,
        audit,
        recovery_tx,
    };

    // GUILD_VOICE_STATES exposes channel joins, while GUILDS maintains guild and
    // channel caches. Neither intent is privileged.
    let intents = GatewayIntents::GUILDS | GatewayIntents::GUILD_VOICE_STATES;

    // Skip an entire processing stage by asking libopus for 16 kHz mono directly
    // instead of receiving 48 kHz stereo PCM and resampling it. This improves
    // quality and reduces CPU cost.
    let songbird_config = songbird::Config::default().decode_mode(DecodeMode::Decode(
        DecodeConfig::new(Channels::Mono, SampleRate::Hz16000),
    ));

    // Build Songbird directly to retain its `Arc`, allowing the interface to
    // leave a channel without an event `Context`.
    let songbird = Songbird::serenity_from_config(songbird_config);
    let recovery_task = tokio::spawn(run_receive_recovery(
        Arc::clone(&songbird),
        Arc::clone(&current),
        recovery_voice_tx,
        recovery_rx,
    ));

    let mut client = Client::builder(&config.bot_token, intents)
        .event_handler(handler)
        .register_songbird_with(Arc::clone(&songbird))
        .await?;

    let http = Arc::clone(&client.http);
    let shard_manager = Arc::clone(&client.shard_manager);

    let task = tokio::spawn(async move {
        if let Err(e) = client.start().await {
            tracing::error!(error = %e, "Discord client stopped unexpectedly");
        }
    });

    Ok(DiscordHandle {
        http,
        songbird,
        shard_manager,
        current,
        manual_follow_pause,
        config: live_config,
        task,
        recovery_task,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_discord_username_accepts_at_and_case_without_accepting_display_names() {
        assert!(discord_username_matches(" @Garrux ", "garrux"));
        assert!(discord_username_matches("@ÁRBOL", "árbol"));
        assert!(!discord_username_matches("Garrux IA", "garrux"));
        assert!(!discord_username_matches("@otro", "garrux"));
    }

    #[test]
    fn record_command_is_available_in_spanish_and_english() {
        assert!(is_record_command("grabar"));
        assert!(is_record_command("record"));
        assert!(!is_record_command("transcription"));
    }

    #[test]
    fn a_short_message_stays_in_one_piece() {
        let chunks = split_message("hola\nqué tal", 100);
        assert_eq!(chunks, vec!["hola\nqué tal"]);
    }

    #[test]
    fn a_long_message_is_split_on_line_boundaries() {
        let text = (0..10)
            .map(|i| format!("línea {i}"))
            .collect::<Vec<_>>()
            .join("\n");
        let chunks = split_message(&text, 30);
        assert!(chunks.len() > 1);
        assert!(chunks.iter().all(|c| c.len() <= 30));
        // Splitting and rejoining must preserve every character.
        assert_eq!(chunks.join("\n"), text);
    }

    #[test]
    fn a_single_overlong_line_is_split_without_panicking_on_utf8() {
        let text = "á".repeat(100); // two bytes per character
        let chunks = split_message(&text, 30);
        assert!(chunks.iter().all(|c| c.len() <= 30));
        assert_eq!(chunks.concat(), text);
    }

    #[test]
    fn an_empty_message_produces_nothing_to_send() {
        assert!(split_message("", 100).is_empty());
    }

    #[test]
    fn transcript_button_hides_and_recovers_the_meeting_id() {
        let custom_id = transcript_button_id("meeting-123");
        assert_eq!(transcript_id_from_button(&custom_id), Some("meeting-123"));
        assert_eq!(transcript_id_from_button("otro:meeting-123"), None);
        assert_eq!(transcript_id_from_button(TRANSCRIPT_BUTTON_PREFIX), None);
    }

    #[test]
    fn manual_departure_ignores_updates_until_the_followed_user_really_leaves() {
        let guild = GuildId::new(10);
        let channel = ChannelId::new(20);
        let mut pause = ManualFollowPause::default();

        pause.block(guild, channel);
        assert!(!pause.should_follow(guild, channel));
        assert!(!pause.should_follow(guild, channel));

        pause.clear();
        assert!(pause.should_follow(guild, channel));
    }

    #[test]
    fn moving_channels_resumes_following_after_a_manual_departure() {
        let guild = GuildId::new(10);
        let blocked = ChannelId::new(20);
        let destination = ChannelId::new(21);
        let mut pause = ManualFollowPause::default();

        pause.block(guild, blocked);
        assert!(pause.should_follow(guild, destination));
        assert!(pause.should_follow(guild, blocked));
    }
}
