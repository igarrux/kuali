//! Discord bot that follows a configured user.
//!
//! Kuali must be a real bot invited to the guild. Automating a user account as a
//! selfbot violates Discord's terms and is intentionally unsupported. Kuali can
//! therefore follow users only in guilds where an authorized person invited it.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;

use kuali_core::{
    format_timestamp, ActionItem, DiscordConfig, DiscordSummaryDelivery, Meeting, MeetingSummary,
    CONSENT_MESSAGE,
};
use parking_lot::RwLock;
use serenity::all::{
    ButtonStyle, ChannelId, ComponentInteraction, Context, CreateAllowedMentions, CreateAttachment,
    CreateButton, CreateCommand, CreateEmbed, CreateEmbedAuthor, CreateEmbedFooter,
    CreateInteractionResponse, CreateInteractionResponseFollowup, CreateInteractionResponseMessage,
    CreateMessage, EditInteractionResponse, EditMessage, EventHandler, GatewayIntents, Guild,
    GuildId, Http, Interaction, MessageId, Permissions, Ready, UserId, VoiceState,
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
    ReceiveRecoveryCause, ReceiveRecoveryControl, ReceiveRecoveryRequest, SsrcMap,
    VoiceChannelContext, VoiceReceiver,
};
use crate::speech::load_consent_audio;
use kuali_core::{CallInfo, VoiceEvent, VoiceSessionId};

const SUMMARY_BUTTON_PREFIX: &str = "kuali:summary:";
const TRANSCRIPT_BUTTON_PREFIX: &str = "kuali:transcript:";
const KUALI_WEBSITE: &str = "https://kuali.garrux.dev";
const KUALI_ICON: &str = "https://kuali.garrux.dev/assets/icon.png";
const KUALI_EMBED_COLOR: u32 = 0x7D_DA_B9;
const EMBED_FIELD_LIMIT: usize = 1_024;
const EMBED_DESCRIPTION_LIMIT: usize = 4_096;
const PUBLIC_TASK_LIMIT: usize = 6;
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

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum MeetingAction {
    Summary,
    Transcript,
}

impl MeetingAction {
    fn button_id(self, meeting_id: &str) -> String {
        let prefix = match self {
            Self::Summary => SUMMARY_BUTTON_PREFIX,
            Self::Transcript => TRANSCRIPT_BUTTON_PREFIX,
        };
        format!("{prefix}{meeting_id}")
    }

    fn from_button(custom_id: &str) -> Option<(Self, &str)> {
        [
            (Self::Summary, SUMMARY_BUTTON_PREFIX),
            (Self::Transcript, TRANSCRIPT_BUTTON_PREFIX),
        ]
        .into_iter()
        .find_map(|(action, prefix)| {
            custom_id
                .strip_prefix(prefix)
                .filter(|meeting_id| !meeting_id.is_empty())
                .map(|meeting_id| (action, meeting_id))
        })
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum DiscordLocale {
    Spanish,
    English,
}

impl DiscordLocale {
    fn from_summary_language(language: &str) -> Self {
        let language = language.trim().to_lowercase();
        if language.starts_with("en") || language.contains("ingl") {
            Self::English
        } else {
            Self::Spanish
        }
    }

    fn from_discord_locale(locale: &str) -> Self {
        if locale.trim().to_lowercase().starts_with("en") {
            Self::English
        } else {
            Self::Spanish
        }
    }

    fn text<'a>(self, spanish: &'a str, english: &'a str) -> &'a str {
        match self {
            Self::Spanish => spanish,
            Self::English => english,
        }
    }
}

fn char_count(value: &str) -> usize {
    value.chars().count()
}

fn truncate_text(value: &str, limit: usize) -> String {
    let value = value.trim();
    if char_count(value) <= limit {
        return value.to_string();
    }
    if limit == 0 {
        return String::new();
    }
    let mut truncated = value
        .chars()
        .take(limit.saturating_sub(1))
        .collect::<String>();
    truncated = truncated.trim_end().to_string();
    truncated.push('…');
    truncated
}

fn one_line(value: &str) -> String {
    value.split_whitespace().collect::<Vec<_>>().join(" ")
}

fn participant_count(meeting: &Meeting) -> usize {
    meeting
        .speakers
        .iter()
        .filter(|speaker| !speaker.is_bot)
        .count()
}

fn human_duration(duration_ms: u64, locale: DiscordLocale) -> String {
    let total_minutes = duration_ms.div_ceil(60_000);
    let hours = total_minutes / 60;
    let minutes = total_minutes % 60;
    match (locale, hours, minutes) {
        (DiscordLocale::Spanish, 0, minutes) => format!("{minutes} min"),
        (DiscordLocale::English, 0, minutes) => format!("{minutes} min"),
        (DiscordLocale::Spanish, hours, 0) => format!("{hours} h"),
        (DiscordLocale::English, hours, 0) => format!("{hours} hr"),
        (DiscordLocale::Spanish, hours, minutes) => format!("{hours} h {minutes} min"),
        (DiscordLocale::English, hours, minutes) => format!("{hours} hr {minutes} min"),
    }
}

fn task_line(task: &ActionItem, locale: DiscordLocale) -> String {
    let text = truncate_text(&one_line(&task.text), 260);
    let mut details = Vec::new();
    if let Some(assignee) = task
        .assignee
        .as_deref()
        .map(str::trim)
        .filter(|v| !v.is_empty())
    {
        details.push(format!(
            "{}: {}",
            locale.text("Responsable", "Owner"),
            one_line(assignee)
        ));
    }
    if let Some(due) = task.due.as_deref().map(str::trim).filter(|v| !v.is_empty()) {
        details.push(format!(
            "{}: {}",
            locale.text("Fecha", "Due"),
            one_line(due)
        ));
    }

    let mark = if task.done { "[x]" } else { "[ ]" };
    if details.is_empty() {
        format!("- {mark} {text}")
    } else {
        format!("- {mark} {text}\n  -# {}", details.join(" · "))
    }
}

fn document_task_line(task: &ActionItem, locale: DiscordLocale) -> String {
    let mut details = Vec::new();
    if let Some(assignee) = task
        .assignee
        .as_deref()
        .map(str::trim)
        .filter(|v| !v.is_empty())
    {
        details.push(format!(
            "{}: {}",
            locale.text("Responsable", "Owner"),
            one_line(assignee)
        ));
    }
    if let Some(due) = task.due.as_deref().map(str::trim).filter(|v| !v.is_empty()) {
        details.push(format!(
            "{}: {}",
            locale.text("Fecha", "Due"),
            one_line(due)
        ));
    }
    if let Some(source_ms) = task.source_ms {
        details.push(format_timestamp(source_ms));
    }
    let mark = if task.done { "[x]" } else { "[ ]" };
    let suffix = if details.is_empty() {
        String::new()
    } else {
        format!(" — {}", details.join(" · "))
    };
    format!("- {mark} {}{suffix}", one_line(&task.text))
}

fn task_preview(summary: &MeetingSummary, locale: DiscordLocale) -> String {
    if summary.action_items.is_empty() {
        return locale
            .text(
                "No se detectaron tareas pendientes.",
                "No action items were detected.",
            )
            .to_string();
    }

    let mut output = String::new();
    let mut included = 0;
    for task in summary.action_items.iter().take(PUBLIC_TASK_LIMIT) {
        let line = task_line(task, locale);
        let separator = usize::from(!output.is_empty());
        if char_count(&output) + separator + char_count(&line) > EMBED_FIELD_LIMIT - 50 {
            break;
        }
        if !output.is_empty() {
            output.push('\n');
        }
        output.push_str(&line);
        included += 1;
    }

    let omitted = summary.action_items.len().saturating_sub(included);
    if omitted > 0 {
        let more = match locale {
            DiscordLocale::Spanish => format!("… y {omitted} más en el resumen."),
            DiscordLocale::English => format!("… and {omitted} more in the summary."),
        };
        if !output.is_empty() {
            output.push('\n');
        }
        output.push_str(&more);
    }
    truncate_text(&output, EMBED_FIELD_LIMIT)
}

fn list_preview(items: &[String], limit: usize, locale: DiscordLocale) -> String {
    if items.is_empty() {
        return locale.text("Ninguno.", "None.").to_string();
    }

    let mut output = String::new();
    let mut included = 0;
    for item in items {
        let line = format!("- {}", one_line(item));
        let separator = usize::from(!output.is_empty());
        if char_count(&output) + separator + char_count(&line) > limit - 50 {
            break;
        }
        if !output.is_empty() {
            output.push('\n');
        }
        output.push_str(&line);
        included += 1;
    }
    let omitted = items.len().saturating_sub(included);
    if omitted > 0 {
        let more = match locale {
            DiscordLocale::Spanish => format!("… y {omitted} más en el archivo adjunto."),
            DiscordLocale::English => format!("… and {omitted} more in the attached file."),
        };
        if !output.is_empty() {
            output.push('\n');
        }
        output.push_str(&more);
    }
    truncate_text(&output, limit)
}

fn tasks_preview(items: &[ActionItem], limit: usize, locale: DiscordLocale) -> String {
    if items.is_empty() {
        return locale.text("Ninguna.", "None.").to_string();
    }
    let mut output = String::new();
    let mut included = 0;
    for item in items {
        let line = task_line(item, locale);
        let separator = usize::from(!output.is_empty());
        if char_count(&output) + separator + char_count(&line) > limit - 50 {
            break;
        }
        if !output.is_empty() {
            output.push('\n');
        }
        output.push_str(&line);
        included += 1;
    }
    let omitted = items.len().saturating_sub(included);
    if omitted > 0 {
        let more = match locale {
            DiscordLocale::Spanish => format!("… y {omitted} más en el archivo adjunto."),
            DiscordLocale::English => format!("… and {omitted} more in the attached file."),
        };
        if !output.is_empty() {
            output.push('\n');
        }
        output.push_str(&more);
    }
    truncate_text(&output, limit)
}

fn completion_embed(meeting: &Meeting, locale: DiscordLocale) -> CreateEmbed {
    let summary = meeting.summary.as_ref();
    let tasks = summary
        .map(|summary| task_preview(summary, locale))
        .unwrap_or_else(|| {
            locale
                .text("Resumen no disponible.", "Summary unavailable.")
                .into()
        });
    CreateEmbed::new()
        .author(
            CreateEmbedAuthor::new(locale.text(
                "Kuali · Reunión finalizada",
                "Kuali · Meeting complete",
            ))
            .url(KUALI_WEBSITE),
        )
        .title(truncate_text(&meeting.meta.title(), 256))
        .description(locale.text(
            "La reunión quedó guardada. Revisa las tareas aquí o abre las notas completas en privado.",
            "The meeting has been saved. Review action items here or open the complete notes privately.",
        ))
        .field(
            locale.text("Duración", "Duration"),
            human_duration(meeting.duration_ms(), locale),
            true,
        )
        .field(
            locale.text("Participantes", "Participants"),
            participant_count(meeting).to_string(),
            true,
        )
        .field(
            locale.text("Canal", "Channel"),
            truncate_text(&format!("#{}", one_line(&meeting.meta.channel_name)), 128),
            true,
        )
        .field(locale.text("Tareas pendientes", "Action items"), tasks, false)
        .thumbnail(KUALI_ICON)
        .footer(CreateEmbedFooter::new(
            "Kuali · Discord · kuali.garrux.dev",
        ))
        .color(KUALI_EMBED_COLOR)
}

fn completion_message(meeting: &Meeting, locale: DiscordLocale) -> CreateMessage {
    CreateMessage::new()
        .allowed_mentions(CreateAllowedMentions::new())
        .embed(completion_embed(meeting, locale))
        .button(
            CreateButton::new(MeetingAction::Summary.button_id(&meeting.meta.id))
                .label(locale.text("Ver resumen", "View summary"))
                .style(ButtonStyle::Primary),
        )
        .button(
            CreateButton::new(MeetingAction::Transcript.button_id(&meeting.meta.id))
                .label(locale.text("Ver transcripción", "View transcript"))
                .style(ButtonStyle::Secondary),
        )
}

fn completion_edit_message(meeting: &Meeting, locale: DiscordLocale) -> EditMessage {
    EditMessage::new()
        .content("")
        .allowed_mentions(CreateAllowedMentions::new())
        .embed(completion_embed(meeting, locale))
        .button(
            CreateButton::new(MeetingAction::Summary.button_id(&meeting.meta.id))
                .label(locale.text("Ver resumen", "View summary"))
                .style(ButtonStyle::Primary),
        )
        .button(
            CreateButton::new(MeetingAction::Transcript.button_id(&meeting.meta.id))
                .label(locale.text("Ver transcripción", "View transcript"))
                .style(ButtonStyle::Secondary),
        )
}

fn fallback_completion_message(meeting: &Meeting, locale: DiscordLocale) -> CreateMessage {
    let content = fallback_completion_content(meeting, locale);
    CreateMessage::new()
        .content(content)
        .allowed_mentions(CreateAllowedMentions::new())
        .button(
            CreateButton::new(MeetingAction::Summary.button_id(&meeting.meta.id))
                .label(locale.text("Ver resumen", "View summary"))
                .style(ButtonStyle::Primary),
        )
        .button(
            CreateButton::new(MeetingAction::Transcript.button_id(&meeting.meta.id))
                .label(locale.text("Ver transcripción", "View transcript"))
                .style(ButtonStyle::Secondary),
        )
}

fn fallback_completion_edit_message(meeting: &Meeting, locale: DiscordLocale) -> EditMessage {
    EditMessage::new()
        .content(fallback_completion_content(meeting, locale))
        .embeds(Vec::new())
        .allowed_mentions(CreateAllowedMentions::new())
        .button(
            CreateButton::new(MeetingAction::Summary.button_id(&meeting.meta.id))
                .label(locale.text("Ver resumen", "View summary"))
                .style(ButtonStyle::Primary),
        )
        .button(
            CreateButton::new(MeetingAction::Transcript.button_id(&meeting.meta.id))
                .label(locale.text("Ver transcripción", "View transcript"))
                .style(ButtonStyle::Secondary),
        )
}

fn fallback_completion_content(meeting: &Meeting, locale: DiscordLocale) -> String {
    let tasks = meeting
        .summary
        .as_ref()
        .map(|summary| task_preview(summary, locale))
        .unwrap_or_else(|| {
            locale
                .text("Resumen no disponible.", "Summary unavailable.")
                .into()
        });
    truncate_text(
        &format!(
            "**{}**\n-# {} · {} · #{}\n\n**{}**\n{}",
            meeting.meta.title(),
            human_duration(meeting.duration_ms(), locale),
            match locale {
                DiscordLocale::Spanish => format!("{} participantes", participant_count(meeting)),
                DiscordLocale::English => format!("{} participants", participant_count(meeting)),
            },
            one_line(&meeting.meta.channel_name),
            locale.text("Tareas pendientes", "Action items"),
            tasks
        ),
        1_900,
    )
}

fn private_summary_embed(meeting: &Meeting, locale: DiscordLocale) -> Option<CreateEmbed> {
    let summary = meeting.summary.as_ref()?;
    Some(
        CreateEmbed::new()
            .author(CreateEmbedAuthor::new("Kuali").url(KUALI_WEBSITE))
            .title(truncate_text(
                &format!(
                    "{} · {}",
                    locale.text("Resumen", "Summary"),
                    meeting.meta.title()
                ),
                256,
            ))
            .description(truncate_text(
                if summary.overview.trim().is_empty() {
                    locale.text("Sin descripción general.", "No overview available.")
                } else {
                    summary.overview.trim()
                },
                1_600.min(EMBED_DESCRIPTION_LIMIT),
            ))
            .field(
                locale.text("Puntos clave", "Key points"),
                list_preview(&summary.key_points, 850, locale),
                false,
            )
            .field(
                locale.text("Decisiones", "Decisions"),
                list_preview(&summary.decisions, 850, locale),
                false,
            )
            .field(
                locale.text("Tareas pendientes", "Action items"),
                tasks_preview(&summary.action_items, 850, locale),
                false,
            )
            .field(
                locale.text("Preguntas abiertas", "Open questions"),
                list_preview(&summary.open_questions, 850, locale),
                false,
            )
            .thumbnail(KUALI_ICON)
            .footer(CreateEmbedFooter::new(locale.text(
                "Solo tú puedes ver este mensaje · Kuali",
                "Only you can see this message · Kuali",
            )))
            .color(KUALI_EMBED_COLOR),
    )
}

fn private_transcript_embed(meeting: &Meeting, locale: DiscordLocale) -> CreateEmbed {
    CreateEmbed::new()
        .author(CreateEmbedAuthor::new("Kuali").url(KUALI_WEBSITE))
        .title(truncate_text(
            &format!(
                "{} · {}",
                locale.text("Transcripción completa", "Full transcript"),
                meeting.meta.title()
            ),
            256,
        ))
        .description(format!(
            "{} · {} · {}",
            human_duration(meeting.duration_ms(), locale),
            match locale {
                DiscordLocale::Spanish => format!("{} participantes", participant_count(meeting)),
                DiscordLocale::English => format!("{} participants", participant_count(meeting)),
            },
            match locale {
                DiscordLocale::Spanish => format!("{} intervenciones", meeting.utterances.len()),
                DiscordLocale::English => format!("{} utterances", meeting.utterances.len()),
            }
        ))
        .thumbnail(KUALI_ICON)
        .footer(CreateEmbedFooter::new(locale.text(
            "Solo tú puedes ver este mensaje · Kuali",
            "Only you can see this message · Kuali",
        )))
        .color(KUALI_EMBED_COLOR)
}

fn safe_filename(value: &str) -> String {
    let mut output = String::new();
    let mut separator = false;
    for character in value.trim().to_lowercase().chars() {
        if character.is_alphanumeric() {
            if separator && !output.is_empty() {
                output.push('-');
            }
            output.push(character);
            separator = false;
        } else {
            separator = true;
        }
        if char_count(&output) >= 48 {
            break;
        }
    }
    if output.is_empty() {
        "reunion".to_string()
    } else {
        output
    }
}

fn document_header(meeting: &Meeting, locale: DiscordLocale) -> String {
    let participants = meeting
        .speakers
        .iter()
        .filter(|speaker| !speaker.is_bot)
        .map(|speaker| speaker.display_name.trim())
        .filter(|name| !name.is_empty())
        .collect::<Vec<_>>();
    format!(
        "{}\n{}: {}\n{}: {}\n{}: {}\n{}: {}\n\n",
        meeting.meta.title(),
        locale.text("Fecha", "Date"),
        meeting.meta.started_at.format("%Y-%m-%d %H:%M UTC"),
        locale.text("Duración", "Duration"),
        human_duration(meeting.duration_ms(), locale),
        locale.text("Canal", "Channel"),
        meeting.meta.channel_name,
        locale.text("Participantes", "Participants"),
        participants.join(", ")
    )
}

fn push_document_list(
    output: &mut String,
    locale: DiscordLocale,
    title_es: &str,
    title_en: &str,
    items: &[String],
) {
    if items.is_empty() {
        return;
    }
    output.push_str(locale.text(title_es, title_en));
    output.push('\n');
    for item in items {
        output.push_str(&format!("- {}\n", item.trim()));
    }
    output.push('\n');
}

fn summary_document(meeting: &Meeting, locale: DiscordLocale) -> Option<String> {
    let summary = meeting.summary.as_ref()?;
    let mut output = document_header(meeting, locale);
    output.push_str(locale.text("RESUMEN\n", "SUMMARY\n"));
    output.push_str(summary.overview.trim());
    output.push_str("\n\n");

    push_document_list(
        &mut output,
        locale,
        "PUNTOS CLAVE",
        "KEY POINTS",
        &summary.key_points,
    );
    push_document_list(
        &mut output,
        locale,
        "DECISIONES",
        "DECISIONS",
        &summary.decisions,
    );

    if !summary.action_items.is_empty() {
        output.push_str(locale.text("TAREAS PENDIENTES\n", "ACTION ITEMS\n"));
        for task in &summary.action_items {
            output.push_str(&document_task_line(task, locale));
            output.push('\n');
        }
        output.push('\n');
    }
    push_document_list(
        &mut output,
        locale,
        "PREGUNTAS ABIERTAS",
        "OPEN QUESTIONS",
        &summary.open_questions,
    );

    output.push_str("---\n");
    if !summary.generated_by.trim().is_empty() {
        output.push_str(&format!(
            "{}: {}\n",
            locale.text("Generado por", "Generated by"),
            summary.generated_by.trim()
        ));
    }
    output.push_str(&format!(
        "{}: {}\n",
        locale.text("ID de reunión", "Meeting ID"),
        meeting.meta.id
    ));
    Some(output)
}

fn transcript_document(meeting: &Meeting, locale: DiscordLocale) -> Option<String> {
    let transcript = meeting.transcript_text();
    if transcript.trim().is_empty() {
        return None;
    }
    let mut output = document_header(meeting, locale);
    output.push_str(locale.text("TRANSCRIPCIÓN COMPLETA\n", "FULL TRANSCRIPT\n"));
    output.push_str(transcript.trim_end());
    output.push_str("\n\n---\n");
    output.push_str(&format!(
        "{}: {}\n",
        locale.text("ID de reunión", "Meeting ID"),
        meeting.meta.id
    ));
    Some(output)
}

fn document_attachment(
    meeting: &Meeting,
    action: MeetingAction,
    locale: DiscordLocale,
    content: String,
) -> CreateAttachment {
    let (kind, description) = match (action, locale) {
        (MeetingAction::Summary, DiscordLocale::Spanish) => {
            ("resumen", "Resumen completo generado por Kuali")
        }
        (MeetingAction::Summary, DiscordLocale::English) => {
            ("summary", "Complete summary generated by Kuali")
        }
        (MeetingAction::Transcript, DiscordLocale::Spanish) => {
            ("transcripcion", "Transcripción completa generada por Kuali")
        }
        (MeetingAction::Transcript, DiscordLocale::English) => {
            ("transcript", "Complete transcript generated by Kuali")
        }
    };
    CreateAttachment::bytes(
        content.into_bytes(),
        format!("kuali-{kind}-{}.txt", safe_filename(&meeting.meta.title())),
    )
    .description(description)
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
        register_receiver_events(&call, &receiver).await;

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

    async fn request_meeting(
        &self,
        meeting_id: String,
        guild_id: GuildId,
    ) -> Result<Meeting, String> {
        let (reply, response) = tokio::sync::oneshot::channel();
        if self
            .tx
            .send(VoiceEvent::MeetingRequested {
                meeting_id,
                guild_id: guild_id.get(),
                reply,
            })
            .is_err()
        {
            return Err("Kuali no pudo consultar las reuniones ahora mismo.".to_string());
        }

        match tokio::time::timeout(Duration::from_secs(60), response).await {
            Ok(Ok(result)) => result,
            Ok(Err(_)) | Err(_) => Err("La consulta de la reunión tardó demasiado.".to_string()),
        }
    }

    async fn handle_meeting_button(
        &self,
        ctx: &Context,
        component: &ComponentInteraction,
        action: MeetingAction,
        meeting_id: String,
    ) {
        let Some(guild_id) = component.guild_id else {
            return;
        };
        let locale = DiscordLocale::from_discord_locale(&component.locale);
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

        let meeting = match self.request_meeting(meeting_id, guild_id).await {
            Ok(meeting) => meeting,
            Err(message) => {
                let _ = component
                    .edit_response(&ctx.http, EditInteractionResponse::new().content(message))
                    .await;
                return;
            }
        };

        match action {
            MeetingAction::Summary => {
                let (Some(embed), Some(document)) = (
                    private_summary_embed(&meeting, locale),
                    summary_document(&meeting, locale),
                ) else {
                    let message = locale.text(
                        "Esta reunión todavía no tiene un resumen.",
                        "This meeting does not have a summary yet.",
                    );
                    let _ = component
                        .edit_response(&ctx.http, EditInteractionResponse::new().content(message))
                        .await;
                    return;
                };
                let fits = document.len() <= component.attachment_size_limit as usize;
                if fits {
                    let response = EditInteractionResponse::new()
                        .embed(embed.clone())
                        .new_attachment(document_attachment(
                            &meeting,
                            MeetingAction::Summary,
                            locale,
                            document,
                        ));
                    if component.edit_response(&ctx.http, response).await.is_ok() {
                        return;
                    }
                    tracing::warn!(
                        meeting_id = %meeting.meta.id,
                        "Discord no permitió adjuntar el resumen; mostrando la vista privada"
                    );
                }

                let note = locale.text(
                    "No pude adjuntar el archivo, pero el resumen sigue visible aquí.",
                    "I could not attach the file, but the summary is still visible here.",
                );
                let _ = component
                    .edit_response(
                        &ctx.http,
                        EditInteractionResponse::new().content(note).embed(embed),
                    )
                    .await;
            }
            MeetingAction::Transcript => {
                let Some(document) = transcript_document(&meeting, locale) else {
                    let message = locale.text(
                        "Esta reunión todavía no tiene texto transcrito.",
                        "This meeting does not have transcribed text yet.",
                    );
                    let _ = component
                        .edit_response(&ctx.http, EditInteractionResponse::new().content(message))
                        .await;
                    return;
                };
                let embed = private_transcript_embed(&meeting, locale);
                let fits = document.len() <= component.attachment_size_limit as usize;
                if fits {
                    let response = EditInteractionResponse::new()
                        .embed(embed.clone())
                        .new_attachment(document_attachment(
                            &meeting,
                            MeetingAction::Transcript,
                            locale,
                            document.clone(),
                        ));
                    if component.edit_response(&ctx.http, response).await.is_ok() {
                        return;
                    }
                    tracing::warn!(
                        meeting_id = %meeting.meta.id,
                        "Discord no permitió adjuntar la transcripción; usando mensajes privados"
                    );
                }

                let mut chunks = split_message(&document, 1_900).into_iter();
                let Some(first) = chunks.next() else {
                    return;
                };
                if component
                    .edit_response(
                        &ctx.http,
                        EditInteractionResponse::new().content(first).embed(embed),
                    )
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
                                .allowed_mentions(CreateAllowedMentions::new())
                                .ephemeral(true),
                        )
                        .await
                        .is_err()
                    {
                        break;
                    }
                }
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
                if let Some((action, meeting_id)) =
                    MeetingAction::from_button(&component.data.custom_id)
                        .map(|(action, meeting_id)| (action, meeting_id.to_string()))
                {
                    self.handle_meeting_button(&ctx, &component, action, meeting_id)
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

    /// Publishes or updates the compact meeting card. Complete notes stay behind
    /// private actions so a busy channel receives tasks rather than a wall of
    /// text. A missing or no-longer-editable message is replaced in the same
    /// channel and the new Discord reference is returned for persistence.
    pub async fn sync_summary(
        &self,
        delivery: DiscordSummaryDelivery,
        meeting: &Meeting,
        language: &str,
    ) -> Result<DiscordSummaryDelivery, serenity::Error> {
        let locale = DiscordLocale::from_summary_language(language);
        let channel_id = ChannelId::new(delivery.channel_id);

        if let Some(message_id) = delivery.message_id {
            let message_id = MessageId::new(message_id);
            if channel_id
                .edit_message(
                    &self.http,
                    message_id,
                    completion_edit_message(meeting, locale),
                )
                .await
                .is_ok()
            {
                return Ok(delivery);
            }

            tracing::warn!(
                meeting_id = %meeting.meta.id,
                %message_id,
                "Discord rechazó la edición enriquecida; probando la tarjeta compacta"
            );
            if channel_id
                .edit_message(
                    &self.http,
                    message_id,
                    fallback_completion_edit_message(meeting, locale),
                )
                .await
                .is_ok()
            {
                return Ok(delivery);
            }

            tracing::warn!(
                meeting_id = %meeting.meta.id,
                %message_id,
                "La tarjeta anterior ya no se puede editar; publicando una nueva"
            );
        }

        let message = match channel_id
            .send_message(&self.http, completion_message(meeting, locale))
            .await
        {
            Ok(message) => message,
            Err(error) => {
                tracing::warn!(
                    meeting_id = %meeting.meta.id,
                    %error,
                    "Discord rechazó la tarjeta enriquecida; usando la versión compacta"
                );
                channel_id
                    .send_message(&self.http, fallback_completion_message(meeting, locale))
                    .await?
            }
        };
        Ok(DiscordSummaryDelivery::delivered(
            channel_id.get(),
            message.id.get(),
        ))
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

async fn register_receiver_events(
    call: &Arc<tokio::sync::Mutex<songbird::Call>>,
    receiver: &VoiceReceiver,
) {
    let mut handler = call.lock().await;
    handler.add_global_event(CoreEvent::SpeakingStateUpdate.into(), receiver.clone());
    handler.add_global_event(CoreEvent::VoiceTick.into(), receiver.clone());
    handler.add_global_event(CoreEvent::ClientDisconnect.into(), receiver.clone());
    handler.add_global_event(CoreEvent::DriverDisconnect.into(), receiver.clone());
}

fn recovery_subject(cause: ReceiveRecoveryCause) -> String {
    match cause {
        ReceiveRecoveryCause::Participant(user_id) => format!("usuario {user_id}"),
        ReceiveRecoveryCause::Driver => "conexión completa".to_string(),
    }
}

/// Songbird 0.6 can lose DAVE/MLS state when membership changes. Rebuild only
/// the media transport for a stalled participant so Discord keeps Kuali inside
/// the channel and users hear no departure or arrival sounds. A terminated
/// driver is resumed against the existing gateway voice membership.
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
            cause = %recovery_subject(request.cause),
            attempt = request.attempt,
            "reiniciando silenciosamente el transporte de voz para recuperar DAVE"
        );

        match request.cause {
            ReceiveRecoveryCause::Participant(_) => {
                let Some(call) = songbird.get(guild_id) else {
                    request.control.cancel();
                    let _ = tx.send(VoiceEvent::Warning(
                        "Discord perdió el controlador de la llamada".to_string(),
                    ));
                    continue;
                };
                call.lock().await.reconnect_voice_session();
                request.control.finish();
                tracing::info!(
                    cause = %recovery_subject(request.cause),
                    attempt = request.attempt,
                    "transporte de voz reiniciado sin salir del canal"
                );
            }
            ReceiveRecoveryCause::Driver => match songbird.join(guild_id, channel_id).await {
                Ok(_) => {
                    request.control.finish();
                    tracing::info!(
                        attempt = request.attempt,
                        "controlador de voz restaurado sin salir del canal"
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
            },
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
    use chrono::{TimeZone, Utc};
    use kuali_core::{color_for, MeetingMeta, Speaker, Utterance};

    fn completed_meeting() -> Meeting {
        let mut meeting = Meeting::new(MeetingMeta {
            id: "meeting-123".into(),
            display_title: Some("Plan de lanzamiento".into()),
            guild_id: 10,
            guild_name: "Kuali".into(),
            channel_id: 20,
            channel_name: "general".into(),
            started_at: Utc.with_ymd_and_hms(2026, 8, 11, 14, 0, 0).unwrap(),
            ended_at: Some(Utc.with_ymd_and_hms(2026, 8, 11, 14, 42, 0).unwrap()),
        });
        meeting.upsert_speaker(Speaker {
            user_id: 1,
            source_id: None,
            audio_kind: None,
            display_name: "Ana".into(),
            username: "ana".into(),
            avatar_url: None,
            color: color_for(1).to_string(),
            is_bot: false,
        });
        meeting.upsert_speaker(Speaker {
            user_id: 2,
            source_id: None,
            audio_kind: None,
            display_name: "Luis".into(),
            username: "luis".into(),
            avatar_url: None,
            color: color_for(2).to_string(),
            is_bot: false,
        });
        meeting.push_utterance(Utterance {
            id: "u1".into(),
            speaker_id: 1,
            start_ms: 5_000,
            end_ms: 9_000,
            text: "Publicamos el viernes".into(),
            confidence: Some(0.97),
        });
        meeting.summary = Some(MeetingSummary {
            title: "Plan de lanzamiento".into(),
            overview: "El equipo cerró el plan de lanzamiento.".into(),
            key_points: vec!["La versión candidata está lista".into()],
            decisions: vec!["Publicar el viernes".into()],
            action_items: vec![ActionItem {
                id: "task-1".into(),
                text: "Preparar la publicación".into(),
                assignee: Some("Ana".into()),
                due: Some("viernes".into()),
                source_ms: Some(5_000),
                done: false,
            }],
            open_questions: vec!["¿A qué hora se publica?".into()],
            generated_by: "Claude · Sonnet".into(),
        });
        meeting
    }

    fn embed_text_length(embed: &serde_json::Value) -> usize {
        let scalar = ["title", "description"]
            .into_iter()
            .map(|key| embed[key].as_str().map(char_count).unwrap_or(0))
            .sum::<usize>();
        let author = embed["author"]["name"]
            .as_str()
            .map(char_count)
            .unwrap_or(0);
        let footer = embed["footer"]["text"]
            .as_str()
            .map(char_count)
            .unwrap_or(0);
        let fields = embed["fields"]
            .as_array()
            .into_iter()
            .flatten()
            .map(|field| {
                field["name"].as_str().map(char_count).unwrap_or(0)
                    + field["value"].as_str().map(char_count).unwrap_or(0)
            })
            .sum::<usize>();
        scalar + author + footer + fields
    }

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
    fn meeting_buttons_hide_and_recover_the_action_and_meeting_id() {
        for action in [MeetingAction::Summary, MeetingAction::Transcript] {
            let custom_id = action.button_id("meeting-123");
            assert_eq!(
                MeetingAction::from_button(&custom_id),
                Some((action, "meeting-123"))
            );
            assert!(custom_id.len() <= 100);
        }
        assert_eq!(MeetingAction::from_button("otro:meeting-123"), None);
        assert_eq!(MeetingAction::from_button(TRANSCRIPT_BUTTON_PREFIX), None);
    }

    #[test]
    fn completion_card_keeps_the_channel_compact_and_actionable() {
        let message = serde_json::to_value(completion_message(
            &completed_meeting(),
            DiscordLocale::Spanish,
        ))
        .unwrap();
        assert!(message.get("content").is_none());
        assert_eq!(message["embeds"].as_array().unwrap().len(), 1);
        let embed = &message["embeds"][0];
        assert_eq!(embed["title"], "Plan de lanzamiento");
        assert_eq!(embed["thumbnail"]["url"], KUALI_ICON);
        assert_eq!(
            embed["footer"]["text"],
            "Kuali · Discord · kuali.garrux.dev"
        );
        assert!(embed.to_string().contains("Preparar la publicación"));
        assert!(!embed.to_string().contains("El equipo cerró el plan"));
        assert!(!embed.to_string().contains("La versión candidata"));
        assert!(!embed.to_string().contains("Publicamos el viernes"));

        let buttons = message["components"][0]["components"].as_array().unwrap();
        assert_eq!(buttons.len(), 2);
        assert_eq!(buttons[0]["label"], "Ver resumen");
        assert_eq!(buttons[1]["label"], "Ver transcripción");
        assert_eq!(message["allowed_mentions"]["parse"], serde_json::json!([]));

        let fallback = serde_json::to_value(fallback_completion_message(
            &completed_meeting(),
            DiscordLocale::Spanish,
        ))
        .unwrap();
        assert!(fallback["embeds"].as_array().is_none_or(Vec::is_empty));
        assert!(char_count(fallback["content"].as_str().unwrap()) <= 1_900);
        assert_eq!(
            fallback["components"][0]["components"]
                .as_array()
                .unwrap()
                .len(),
            2
        );

        let rich_edit = serde_json::to_value(completion_edit_message(
            &completed_meeting(),
            DiscordLocale::Spanish,
        ))
        .unwrap();
        assert_eq!(rich_edit["content"], "");
        assert_eq!(rich_edit["embeds"].as_array().unwrap().len(), 1);
        assert_eq!(
            rich_edit["components"][0]["components"]
                .as_array()
                .unwrap()
                .len(),
            2
        );

        let fallback_edit = serde_json::to_value(fallback_completion_edit_message(
            &completed_meeting(),
            DiscordLocale::Spanish,
        ))
        .unwrap();
        assert!(fallback_edit["embeds"].as_array().unwrap().is_empty());
        assert!(fallback_edit["content"]
            .as_str()
            .unwrap()
            .contains("Preparar la publicación"));
    }

    #[test]
    fn public_tasks_and_private_summary_stay_inside_discord_embed_limits() {
        let mut meeting = completed_meeting();
        let summary = meeting.summary.as_mut().unwrap();
        summary.overview = "Descripción larga ".repeat(600);
        summary.key_points = (0..40)
            .map(|index| format!("Punto clave {index} {}", "detalle ".repeat(30)))
            .collect();
        summary.decisions = summary.key_points.clone();
        summary.open_questions = summary.key_points.clone();
        summary.action_items = (0..30)
            .map(|index| ActionItem {
                id: format!("task-{index}"),
                text: format!("Tarea {index} {}", "detalle ".repeat(30)),
                assignee: Some("Responsable".into()),
                due: Some("mañana".into()),
                source_ms: None,
                done: false,
            })
            .collect();

        for embed in [
            completion_embed(&meeting, DiscordLocale::Spanish),
            private_summary_embed(&meeting, DiscordLocale::Spanish).unwrap(),
        ] {
            let embed = serde_json::to_value(embed).unwrap();
            assert!(embed_text_length(&embed) <= 6_000);
            for field in embed["fields"].as_array().unwrap() {
                assert!(char_count(field["name"].as_str().unwrap()) <= 256);
                assert!(char_count(field["value"].as_str().unwrap()) <= EMBED_FIELD_LIMIT);
            }
        }
        let public =
            serde_json::to_value(completion_embed(&meeting, DiscordLocale::Spanish)).unwrap();
        assert!(public.to_string().contains("más en el resumen"));
    }

    #[test]
    fn private_documents_are_complete_without_exposing_ids_in_the_public_card() {
        let meeting = completed_meeting();
        let summary = summary_document(&meeting, DiscordLocale::Spanish).unwrap();
        assert!(summary.contains("RESUMEN"));
        assert!(summary.contains("PUNTOS CLAVE"));
        assert!(summary.contains("DECISIONES"));
        assert!(summary.contains("TAREAS PENDIENTES"));
        assert!(summary.contains("PREGUNTAS ABIERTAS"));
        assert!(!summary.contains("Publicamos el viernes"));
        assert!(summary.trim_end().ends_with("ID de reunión: meeting-123"));

        let transcript = transcript_document(&meeting, DiscordLocale::Spanish).unwrap();
        assert!(transcript.contains("TRANSCRIPCIÓN COMPLETA"));
        assert!(transcript.contains("[00:05] Ana: Publicamos el viernes"));
        assert!(transcript
            .trim_end()
            .ends_with("ID de reunión: meeting-123"));

        let public =
            serde_json::to_string(&completion_embed(&meeting, DiscordLocale::Spanish)).unwrap();
        assert!(!public.contains("ID de reunión"));
        assert!(!public.contains("meeting-123"));
    }

    #[test]
    fn english_summary_settings_localize_the_public_card() {
        assert_eq!(
            DiscordLocale::from_summary_language("English"),
            DiscordLocale::English
        );
        let message = serde_json::to_value(completion_message(
            &completed_meeting(),
            DiscordLocale::English,
        ))
        .unwrap();
        assert_eq!(
            message["components"][0]["components"][0]["label"],
            "View summary"
        );
        assert!(message["embeds"][0].to_string().contains("Action items"));
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
