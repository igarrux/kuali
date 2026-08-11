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
    ButtonStyle, ChannelId, ComponentInteraction, Context, CreateAllowedMentions, CreateButton,
    CreateCommand, CreateEmbed, CreateEmbedAuthor, CreateEmbedFooter, CreateInteractionResponse,
    CreateInteractionResponseMessage, CreateMessage, EditInteractionResponse, EditMessage,
    EventHandler, GatewayIntents, Guild, GuildId, Http, Interaction, MessageId, Permissions, Ready,
    UserId, VoiceState,
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
const KEY_POINTS_BUTTON_PREFIX: &str = "kuali:key-points:";
const DECISIONS_BUTTON_PREFIX: &str = "kuali:decisions:";
const OPEN_QUESTIONS_BUTTON_PREFIX: &str = "kuali:open-questions:";
const TRANSCRIPT_BUTTON_PREFIX: &str = "kuali:transcript:";
const PRIVATE_PAGE_BUTTON_PREFIX: &str = "kuali:page:";
const KUALI_WEBSITE: &str = "https://kuali.garrux.dev";
const KUALI_ICON: &str = "https://kuali.garrux.dev/assets/icon.png";
const KUALI_EMBED_COLOR: u32 = 0x7D_DA_B9;
const KUALI_PENDING_COLOR: u32 = 0x7C_A6_DA;
const KUALI_ATTENTION_COLOR: u32 = 0xE8_A2_3B;
const KUALI_ERROR_COLOR: u32 = 0xDF_6D_73;
const EMBED_FIELD_LIMIT: usize = 1_024;
// Leave room for the card title, metadata, and footer within Discord's display
// text budget. The downloadable file remains complete regardless of paging.
const PRIVATE_PAGE_CHAR_LIMIT: usize = 3_200;
const PRIVATE_TEXT_CHUNK_LIMIT: usize = 3_800;
const PRIVATE_TEXT_COMPONENT_LIMIT: usize = 24;
const DISCORD_COMPONENTS_V2_FLAG: u32 = 1 << 15;
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
    KeyPoints,
    Decisions,
    OpenQuestions,
    Transcript,
}

/// State rendered into the one persistent Discord card for a meeting.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DiscordSummaryState {
    Preparing,
    Ready,
    Failed,
    AttentionRequired,
}

impl MeetingAction {
    const READY_ACTIONS: [Self; 5] = [
        Self::Summary,
        Self::KeyPoints,
        Self::Decisions,
        Self::OpenQuestions,
        Self::Transcript,
    ];

    fn button_id(self, meeting_id: &str) -> String {
        let prefix = match self {
            Self::Summary => SUMMARY_BUTTON_PREFIX,
            Self::KeyPoints => KEY_POINTS_BUTTON_PREFIX,
            Self::Decisions => DECISIONS_BUTTON_PREFIX,
            Self::OpenQuestions => OPEN_QUESTIONS_BUTTON_PREFIX,
            Self::Transcript => TRANSCRIPT_BUTTON_PREFIX,
        };
        format!("{prefix}{meeting_id}")
    }

    fn page_code(self) -> &'static str {
        match self {
            Self::Summary => "s",
            Self::KeyPoints => "k",
            Self::Decisions => "d",
            Self::OpenQuestions => "q",
            Self::Transcript => "t",
        }
    }

    fn from_page_code(code: &str) -> Option<Self> {
        match code {
            "s" => Some(Self::Summary),
            "k" => Some(Self::KeyPoints),
            "d" => Some(Self::Decisions),
            "q" => Some(Self::OpenQuestions),
            "t" => Some(Self::Transcript),
            _ => None,
        }
    }

    fn page_button_id(self, meeting_id: &str, page: usize) -> String {
        format!(
            "{PRIVATE_PAGE_BUTTON_PREFIX}{}:{page}:{meeting_id}",
            self.page_code()
        )
    }

    fn from_page_button(custom_id: &str) -> Option<(Self, usize, &str)> {
        let rest = custom_id.strip_prefix(PRIVATE_PAGE_BUTTON_PREFIX)?;
        let (action, rest) = rest.split_once(':')?;
        let (page, meeting_id) = rest.split_once(':')?;
        let action = Self::from_page_code(action)?;
        let page = page.parse().ok()?;
        (!meeting_id.is_empty()).then_some((action, page, meeting_id))
    }

    fn label(self, locale: DiscordLocale) -> &'static str {
        match self {
            Self::Summary => locale.text("Resumen", "Summary"),
            Self::KeyPoints => locale.text("Puntos clave", "Key points"),
            Self::Decisions => locale.text("Decisiones", "Decisions"),
            Self::OpenQuestions => locale.text("Preguntas", "Questions"),
            Self::Transcript => locale.text("Transcripción", "Transcript"),
        }
    }

    fn from_button(custom_id: &str) -> Option<(Self, &str)> {
        [
            (Self::Summary, SUMMARY_BUTTON_PREFIX),
            (Self::KeyPoints, KEY_POINTS_BUTTON_PREFIX),
            (Self::Decisions, DECISIONS_BUTTON_PREFIX),
            (Self::OpenQuestions, OPEN_QUESTIONS_BUTTON_PREFIX),
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

fn summary_state_embed(
    meeting: &Meeting,
    locale: DiscordLocale,
    state: DiscordSummaryState,
) -> CreateEmbed {
    if state == DiscordSummaryState::Ready {
        return completion_embed(meeting, locale);
    }

    let (author, description, next_step, color) = match state {
        DiscordSummaryState::Preparing => (
            locale.text("Kuali · Preparando notas", "Kuali · Preparing notes"),
            locale.text(
                "La transcripción ya está guardada. Kuali está preparando el resumen, las decisiones y las tareas.",
                "The transcript is already saved. Kuali is preparing the summary, decisions, and action items.",
            ),
            locale.text(
                "Este mismo mensaje se actualizará al terminar.",
                "This message will update when the summary is ready.",
            ),
            KUALI_PENDING_COLOR,
        ),
        DiscordSummaryState::Failed => (
            locale.text("Kuali · Resumen no disponible", "Kuali · Summary unavailable"),
            locale.text(
                "El modelo no logró preparar un resumen válido después de tres intentos. La transcripción completa sigue disponible.",
                "The model could not prepare a valid summary after three attempts. The full transcript is still available.",
            ),
            locale.text(
                "Revisa el proveedor en Kuali y usa «Rehacer resumen» para intentarlo otra vez.",
                "Check the provider in Kuali and use “Regenerate summary” to try again.",
            ),
            KUALI_ERROR_COLOR,
        ),
        DiscordSummaryState::AttentionRequired => (
            locale.text("Kuali · El modelo requiere atención", "Kuali · Model needs attention"),
            locale.text(
                "El proveedor rechazó el resumen por credenciales, cuota, límites de tokens o configuración del modelo. La transcripción completa sigue disponible.",
                "The provider rejected the summary because of credentials, quota, token limits, or model configuration. The full transcript is still available.",
            ),
            locale.text(
                "Abre Ajustes → Resúmenes y tareas en Kuali antes de reintentarlo.",
                "Open Settings → Summaries and tasks in Kuali before trying again.",
            ),
            KUALI_ATTENTION_COLOR,
        ),
        DiscordSummaryState::Ready => unreachable!(),
    };

    CreateEmbed::new()
        .author(CreateEmbedAuthor::new(author).url(KUALI_WEBSITE))
        .title(truncate_text(&meeting.meta.title(), 256))
        .description(description)
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
        .field(locale.text("Estado", "Status"), next_step, false)
        .thumbnail(KUALI_ICON)
        .footer(CreateEmbedFooter::new("Kuali · Discord · kuali.garrux.dev"))
        .color(color)
}

fn add_summary_state_buttons(
    mut message: CreateMessage,
    meeting: &Meeting,
    locale: DiscordLocale,
    state: DiscordSummaryState,
) -> CreateMessage {
    let actions: &[MeetingAction] = if state == DiscordSummaryState::Ready {
        &MeetingAction::READY_ACTIONS
    } else {
        &[MeetingAction::Transcript]
    };
    for action in actions {
        let style = if *action == MeetingAction::Summary {
            ButtonStyle::Primary
        } else {
            ButtonStyle::Secondary
        };
        message = message.button(
            CreateButton::new(action.button_id(&meeting.meta.id))
                .label(action.label(locale))
                .style(style),
        );
    }
    message
}

fn add_summary_state_edit_buttons(
    mut message: EditMessage,
    meeting: &Meeting,
    locale: DiscordLocale,
    state: DiscordSummaryState,
) -> EditMessage {
    let actions: &[MeetingAction] = if state == DiscordSummaryState::Ready {
        &MeetingAction::READY_ACTIONS
    } else {
        &[MeetingAction::Transcript]
    };
    for action in actions {
        let style = if *action == MeetingAction::Summary {
            ButtonStyle::Primary
        } else {
            ButtonStyle::Secondary
        };
        message = message.button(
            CreateButton::new(action.button_id(&meeting.meta.id))
                .label(action.label(locale))
                .style(style),
        );
    }
    message
}

fn summary_state_message(
    meeting: &Meeting,
    locale: DiscordLocale,
    state: DiscordSummaryState,
) -> CreateMessage {
    let message = CreateMessage::new()
        .allowed_mentions(CreateAllowedMentions::new())
        .embed(summary_state_embed(meeting, locale, state));
    add_summary_state_buttons(message, meeting, locale, state)
}

fn summary_state_edit_message(
    meeting: &Meeting,
    locale: DiscordLocale,
    state: DiscordSummaryState,
) -> EditMessage {
    let message = EditMessage::new()
        .content("")
        .allowed_mentions(CreateAllowedMentions::new())
        .embed(summary_state_embed(meeting, locale, state));
    add_summary_state_edit_buttons(message, meeting, locale, state)
}

fn fallback_summary_state_message(
    meeting: &Meeting,
    locale: DiscordLocale,
    state: DiscordSummaryState,
) -> CreateMessage {
    let message = CreateMessage::new()
        .content(fallback_summary_state_content(meeting, locale, state))
        .allowed_mentions(CreateAllowedMentions::new());
    add_summary_state_buttons(message, meeting, locale, state)
}

fn fallback_summary_state_edit_message(
    meeting: &Meeting,
    locale: DiscordLocale,
    state: DiscordSummaryState,
) -> EditMessage {
    let message = EditMessage::new()
        .content(fallback_summary_state_content(meeting, locale, state))
        .embeds(Vec::new())
        .allowed_mentions(CreateAllowedMentions::new());
    add_summary_state_edit_buttons(message, meeting, locale, state)
}

#[cfg(test)]
fn completion_message(meeting: &Meeting, locale: DiscordLocale) -> CreateMessage {
    summary_state_message(meeting, locale, DiscordSummaryState::Ready)
}

#[cfg(test)]
fn fallback_completion_message(meeting: &Meeting, locale: DiscordLocale) -> CreateMessage {
    fallback_summary_state_message(meeting, locale, DiscordSummaryState::Ready)
}

fn fallback_summary_state_content(
    meeting: &Meeting,
    locale: DiscordLocale,
    state: DiscordSummaryState,
) -> String {
    if state != DiscordSummaryState::Ready {
        let description = match state {
            DiscordSummaryState::Preparing => locale.text(
                "Kuali está preparando el resumen, las decisiones y las tareas. Este mensaje se actualizará al terminar.",
                "Kuali is preparing the summary, decisions, and action items. This message will update when ready.",
            ),
            DiscordSummaryState::Failed => locale.text(
                "El modelo no logró preparar un resumen válido después de tres intentos. Revisa Kuali antes de reintentarlo.",
                "The model could not prepare a valid summary after three attempts. Check Kuali before trying again.",
            ),
            DiscordSummaryState::AttentionRequired => locale.text(
                "El proveedor requiere atención: revisa sus credenciales, cuota, límites de tokens y modelo en Kuali.",
                "The provider needs attention: check its credentials, quota, token limits, and model in Kuali.",
            ),
            DiscordSummaryState::Ready => unreachable!(),
        };
        return truncate_text(
            &format!("**{}**\n-# Kuali\n\n{description}", meeting.meta.title()),
            1_900,
        );
    }

    fallback_completion_content(meeting, locale)
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

fn private_document_fallback_embed(
    meeting: &Meeting,
    locale: DiscordLocale,
    action: MeetingAction,
    visible: &str,
) -> CreateEmbed {
    let kind = match action {
        MeetingAction::Summary => locale.text("Resumen completo", "Full summary"),
        MeetingAction::KeyPoints => locale.text("Puntos clave", "Key points"),
        MeetingAction::Decisions => locale.text("Decisiones", "Decisions"),
        MeetingAction::OpenQuestions => locale.text("Preguntas abiertas", "Open questions"),
        MeetingAction::Transcript => locale.text("Transcripción completa", "Full transcript"),
    };
    CreateEmbed::new()
        .author(CreateEmbedAuthor::new("Kuali").url(KUALI_WEBSITE))
        .title(truncate_text(
            &format!("{kind} · {}", meeting.meta.title()),
            256,
        ))
        .description(truncate_text(visible, 4_000))
        .thumbnail(KUALI_ICON)
        .footer(CreateEmbedFooter::new(locale.text(
            "Solo tú puedes ver este mensaje · Kuali",
            "Only you can see this message · Kuali",
        )))
        .color(KUALI_EMBED_COLOR)
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

fn push_visible_list(output: &mut String, title: &str, items: &[String], locale: DiscordLocale) {
    output.push_str(&format!("### {title}\n"));
    if items.is_empty() {
        output.push_str(locale.text("_Ninguno._\n\n", "_None._\n\n"));
        return;
    }
    for item in items {
        output.push_str(&format!("- {}\n", item.trim()));
    }
    output.push('\n');
}

fn visible_task_line(task: &ActionItem, locale: DiscordLocale) -> String {
    let mut details = Vec::new();
    if let Some(assignee) = task
        .assignee
        .as_deref()
        .map(str::trim)
        .filter(|value| !value.is_empty())
    {
        details.push(format!(
            "{}: {}",
            locale.text("Responsable", "Owner"),
            assignee
        ));
    }
    if let Some(due) = task
        .due
        .as_deref()
        .map(str::trim)
        .filter(|value| !value.is_empty())
    {
        details.push(format!("{}: {}", locale.text("Fecha", "Due"), due));
    }
    if let Some(source_ms) = task.source_ms {
        details.push(format_timestamp(source_ms));
    }
    let mark = if task.done { "x" } else { " " };
    if details.is_empty() {
        format!("- [{mark}] {}", task.text.trim())
    } else {
        format!(
            "- [{mark}] {}\n  -# {}",
            task.text.trim(),
            details.join(" · ")
        )
    }
}

fn visible_summary_document(meeting: &Meeting, locale: DiscordLocale) -> Option<String> {
    let summary = meeting.summary.as_ref()?;
    let mut output = String::new();
    output.push_str(&format!("### {}\n", locale.text("Resumen", "Summary")));
    if summary.overview.trim().is_empty() {
        output.push_str(locale.text("_Sin descripción general._\n\n", "_No overview._\n\n"));
    } else {
        output.push_str(summary.overview.trim());
        output.push_str("\n\n");
    }

    push_visible_list(
        &mut output,
        locale.text("Puntos clave", "Key points"),
        &summary.key_points,
        locale,
    );
    push_visible_list(
        &mut output,
        locale.text("Decisiones", "Decisions"),
        &summary.decisions,
        locale,
    );

    output.push_str(&format!(
        "### {}\n",
        locale.text("Tareas pendientes", "Action items")
    ));
    if summary.action_items.is_empty() {
        output.push_str(locale.text("_Ninguna._\n\n", "_None._\n\n"));
    } else {
        for task in &summary.action_items {
            output.push_str(&visible_task_line(task, locale));
            output.push('\n');
        }
        output.push('\n');
    }

    push_visible_list(
        &mut output,
        locale.text("Preguntas abiertas", "Open questions"),
        &summary.open_questions,
        locale,
    );
    if !summary.generated_by.trim().is_empty() {
        output.push_str(&format!(
            "-# {} · {}",
            locale.text("Generado por", "Generated by"),
            summary.generated_by.trim()
        ));
    }
    Some(output)
}

fn visible_summary_section(
    meeting: &Meeting,
    locale: DiscordLocale,
    action: MeetingAction,
) -> Option<String> {
    let summary = meeting.summary.as_ref()?;
    let mut output = String::new();
    match action {
        MeetingAction::Summary => return visible_summary_document(meeting, locale),
        MeetingAction::KeyPoints => push_visible_list(
            &mut output,
            locale.text("Puntos clave", "Key points"),
            &summary.key_points,
            locale,
        ),
        MeetingAction::Decisions => push_visible_list(
            &mut output,
            locale.text("Decisiones", "Decisions"),
            &summary.decisions,
            locale,
        ),
        MeetingAction::OpenQuestions => push_visible_list(
            &mut output,
            locale.text("Preguntas abiertas", "Open questions"),
            &summary.open_questions,
            locale,
        ),
        MeetingAction::Transcript => return None,
    }
    Some(output)
}

fn paginate_private_document(visible: &str) -> Vec<String> {
    let mut pages = Vec::new();
    let mut current = String::new();

    for block in visible
        .split("\n\n")
        .filter(|block| !block.trim().is_empty())
    {
        if char_count(block) > PRIVATE_PAGE_CHAR_LIMIT {
            if !current.is_empty() {
                pages.push(std::mem::take(&mut current));
            }
            pages.extend(split_message(block, PRIVATE_PAGE_CHAR_LIMIT));
            continue;
        }

        let separator = if current.is_empty() { 0 } else { 2 };
        if char_count(&current) + separator + char_count(block) > PRIVATE_PAGE_CHAR_LIMIT {
            pages.push(std::mem::take(&mut current));
        }
        if !current.is_empty() {
            current.push_str("\n\n");
        }
        current.push_str(block);
    }

    if !current.is_empty() {
        pages.push(current);
    }
    if pages.is_empty() {
        pages.push(visible.to_string());
    }
    pages
}

fn transcript_page_range(visible: &str) -> Option<String> {
    let timestamps = visible.lines().filter_map(|line| {
        line.strip_prefix("**")?
            .split_once(" · ")
            .map(|(timestamp, _)| timestamp)
            .filter(|timestamp| {
                !timestamp.is_empty()
                    && timestamp
                        .chars()
                        .all(|character| character.is_ascii_digit() || character == ':')
            })
    });
    let timestamps = timestamps.collect::<Vec<_>>();
    let first = timestamps.first()?;
    let last = timestamps.last()?;
    if first == last {
        Some((*first).to_string())
    } else {
        Some(format!("{first}–{last}"))
    }
}

fn escape_inline_markdown(value: &str) -> String {
    let mut output = String::with_capacity(value.len());
    for character in value.chars() {
        if matches!(character, '\\' | '*' | '_' | '~' | '`' | '|') {
            output.push('\\');
        }
        output.push(character);
    }
    output
}

fn visible_transcript_document(meeting: &Meeting, locale: DiscordLocale) -> Option<String> {
    if meeting.utterances.is_empty() {
        return None;
    }
    let mut output = format!(
        "### {}\n",
        locale.text("Transcripción completa", "Full transcript")
    );
    let mut wrote_utterance = false;
    for utterance in &meeting.utterances {
        let text = utterance.text.trim();
        if text.is_empty() {
            continue;
        }
        let speaker = escape_inline_markdown(&meeting.speaker_name(utterance.speaker_id));
        output.push_str(&format!(
            "**{} · {}**\n> {}\n\n",
            format_timestamp(utterance.start_ms),
            speaker,
            text.replace('\n', "\n> ")
        ));
        wrote_utterance = true;
    }
    wrote_utterance.then_some(output)
}

fn private_document_filename(action: MeetingAction, locale: DiscordLocale) -> Option<&'static str> {
    match (action, locale) {
        (MeetingAction::Summary, DiscordLocale::Spanish) => Some("kuali-resumen.txt"),
        (MeetingAction::Summary, DiscordLocale::English) => Some("kuali-summary.txt"),
        (MeetingAction::Transcript, DiscordLocale::Spanish) => Some("kuali-transcripcion.txt"),
        (MeetingAction::Transcript, DiscordLocale::English) => Some("kuali-transcript.txt"),
        _ => None,
    }
}

fn private_document_description(
    action: MeetingAction,
    locale: DiscordLocale,
) -> Option<&'static str> {
    match (action, locale) {
        (MeetingAction::Summary, DiscordLocale::Spanish) => {
            Some("Descargar el resumen completo en texto")
        }
        (MeetingAction::Summary, DiscordLocale::English) => {
            Some("Download the complete summary as text")
        }
        (MeetingAction::Transcript, DiscordLocale::Spanish) => {
            Some("Descargar la transcripción completa en texto")
        }
        (MeetingAction::Transcript, DiscordLocale::English) => {
            Some("Download the complete transcript as text")
        }
        _ => None,
    }
}

fn private_pagination_row(
    action: MeetingAction,
    meeting_id: &str,
    locale: DiscordLocale,
    page: usize,
    page_count: usize,
) -> serde_json::Value {
    let previous = page.saturating_sub(1);
    let next = (page + 1).min(page_count - 1);
    serde_json::json!({
        "type": 1,
        "components": [
            {
                "type": 2,
                "style": 2,
                "label": locale.text("Inicio", "First"),
                "custom_id": action.page_button_id(meeting_id, 0),
                "disabled": page == 0
            },
            {
                "type": 2,
                "style": 2,
                "label": locale.text("Anterior", "Previous"),
                "custom_id": action.page_button_id(meeting_id, previous),
                "disabled": page == 0
            },
            {
                "type": 2,
                "style": 2,
                "label": format!("{}/{}", page + 1, page_count),
                "custom_id": "kuali:page-indicator",
                "disabled": true
            },
            {
                "type": 2,
                "style": 2,
                "label": locale.text("Siguiente", "Next"),
                "custom_id": action.page_button_id(meeting_id, next),
                "disabled": page + 1 == page_count
            },
            {
                "type": 2,
                "style": 2,
                "label": locale.text("Final", "Last"),
                "custom_id": action.page_button_id(meeting_id, page_count - 1),
                "disabled": page + 1 == page_count
            }
        ]
    })
}

fn private_document_payload(
    meeting: &Meeting,
    action: MeetingAction,
    locale: DiscordLocale,
    visible: &str,
    page: usize,
    page_count: usize,
    attachment_id: Option<serde_json::Value>,
) -> serde_json::Value {
    let kind = match action {
        MeetingAction::Summary => locale.text("Resumen completo", "Full summary"),
        MeetingAction::KeyPoints => locale.text("Puntos clave", "Key points"),
        MeetingAction::Decisions => locale.text("Decisiones", "Decisions"),
        MeetingAction::OpenQuestions => locale.text("Preguntas abiertas", "Open questions"),
        MeetingAction::Transcript => locale.text("Transcripción completa", "Full transcript"),
    };
    let participants = match locale {
        DiscordLocale::Spanish => format!("{} participantes", participant_count(meeting)),
        DiscordLocale::English => format!("{} participants", participant_count(meeting)),
    };
    let detail = match (action, locale) {
        (MeetingAction::Summary, DiscordLocale::Spanish) => {
            "Puntos clave, decisiones y tareas".to_string()
        }
        (MeetingAction::Summary, DiscordLocale::English) => {
            "Key points, decisions, and action items".to_string()
        }
        (MeetingAction::KeyPoints, DiscordLocale::Spanish) => {
            "Hallazgos y temas principales".to_string()
        }
        (MeetingAction::KeyPoints, DiscordLocale::English) => {
            "Main findings and topics".to_string()
        }
        (MeetingAction::Decisions, DiscordLocale::Spanish) => {
            "Acuerdos tomados durante la reunión".to_string()
        }
        (MeetingAction::Decisions, DiscordLocale::English) => {
            "Agreements made during the meeting".to_string()
        }
        (MeetingAction::OpenQuestions, DiscordLocale::Spanish) => {
            "Temas que todavía requieren respuesta".to_string()
        }
        (MeetingAction::OpenQuestions, DiscordLocale::English) => {
            "Topics that still need an answer".to_string()
        }
        (MeetingAction::Transcript, DiscordLocale::Spanish) => {
            format!("{} intervenciones", meeting.utterances.len())
        }
        (MeetingAction::Transcript, DiscordLocale::English) => {
            format!("{} utterances", meeting.utterances.len())
        }
    };
    let mut metadata = vec![
        human_duration(meeting.duration_ms(), locale),
        participants,
        detail,
    ];
    if action == MeetingAction::Transcript {
        if let Some(range) = transcript_page_range(visible) {
            metadata.push(range);
        }
    }
    if page_count > 1 {
        metadata.push(match locale {
            DiscordLocale::Spanish => format!("Página {} de {page_count}", page + 1),
            DiscordLocale::English => format!("Page {} of {page_count}", page + 1),
        });
    }
    let header = format!(
        "## {kind}\n**{}**\n-# {}",
        meeting.meta.title(),
        metadata.join(" · "),
    );
    let mut chunks = split_message(visible, PRIVATE_TEXT_CHUNK_LIMIT);
    if chunks.len() > PRIVATE_TEXT_COMPONENT_LIMIT {
        let overflow = chunks.split_off(PRIVATE_TEXT_COMPONENT_LIMIT - 1);
        chunks.push(overflow.join("\n"));
    }

    let content = chunks
        .into_iter()
        .map(|content| serde_json::json!({ "type": 10, "content": content }))
        .collect::<Vec<_>>();
    let footer = serde_json::json!({
        "type": 10,
        "content": locale.text(
            "-# Solo tú puedes ver este mensaje · Kuali",
            "-# Only you can see this message · Kuali"
        )
    });
    let separator = || serde_json::json!({ "type": 14, "divider": true, "spacing": 1 });

    let filename = private_document_filename(action, locale);
    let file = filename
        .filter(|_| attachment_id.is_some())
        .map(|filename| {
            serde_json::json!({
                "type": 13,
                "file": { "url": format!("attachment://{filename}") }
            })
        });

    // Serenity 0.12 can receive unknown top-level V2 components, but it cannot
    // deserialize a Container with nested Text Displays. Paginated cards stay
    // flat so their navigation button interactions reach Kuali reliably.
    let components = if page_count > 1 {
        let mut components = vec![
            serde_json::json!({
                "type": 10,
                "content": format!("## Kuali · {}", header.trim_start_matches("## "))
            }),
            separator(),
        ];
        components.extend(content);
        components.push(separator());
        components.push(private_pagination_row(
            action,
            &meeting.meta.id,
            locale,
            page,
            page_count,
        ));
        components.push(footer);
        if let Some(file) = file {
            components.push(file);
        }
        components
    } else {
        let mut children = vec![
            serde_json::json!({
                "type": 9,
                "components": [{ "type": 10, "content": header }],
                "accessory": {
                    "type": 11,
                    "media": { "url": KUALI_ICON },
                    "description": "Kuali"
                }
            }),
            separator(),
        ];
        children.extend(content);
        children.push(separator());
        children.push(footer);
        if let Some(file) = file {
            children.push(file);
        }
        vec![serde_json::json!({
            "type": 17,
            "accent_color": KUALI_EMBED_COLOR,
            "components": children
        })]
    };

    let attachments = if let (Some(id), Some(filename), Some(description)) = (
        attachment_id,
        filename,
        private_document_description(action, locale),
    ) {
        serde_json::json!([{
            "id": id,
            "filename": filename,
            "description": description
        }])
    } else {
        serde_json::json!([])
    };

    serde_json::json!({
        "flags": DISCORD_COMPONENTS_V2_FLAG,
        "content": null,
        "embeds": [],
        "allowed_mentions": { "parse": [] },
        "attachments": attachments,
        "components": components
    })
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

struct PrivateMeetingDocument {
    action: MeetingAction,
    locale: DiscordLocale,
    visible: String,
    download: Option<String>,
    page: usize,
    page_count: usize,
}

fn private_meeting_document(
    meeting: &Meeting,
    action: MeetingAction,
    locale: DiscordLocale,
    requested_page: usize,
) -> Option<PrivateMeetingDocument> {
    let visible = match action {
        MeetingAction::Summary
        | MeetingAction::KeyPoints
        | MeetingAction::Decisions
        | MeetingAction::OpenQuestions => visible_summary_section(meeting, locale, action)?,
        MeetingAction::Transcript => visible_transcript_document(meeting, locale)?,
    };
    let download = match action {
        MeetingAction::Summary => summary_document(meeting, locale),
        MeetingAction::Transcript => transcript_document(meeting, locale),
        MeetingAction::KeyPoints | MeetingAction::Decisions | MeetingAction::OpenQuestions => None,
    };
    let pages = paginate_private_document(&visible);
    let page_count = pages.len();
    let page = requested_page.min(page_count - 1);
    Some(PrivateMeetingDocument {
        action,
        locale,
        visible: pages.into_iter().nth(page)?,
        download,
        page,
        page_count,
    })
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

    async fn respond_with_private_document(
        &self,
        ctx: &Context,
        component: &ComponentInteraction,
        meeting: &Meeting,
        document: PrivateMeetingDocument,
    ) {
        let filename = private_document_filename(document.action, document.locale);
        let has_existing_attachment = filename.is_some_and(|filename| {
            component
                .message
                .attachments
                .iter()
                .any(|attachment| attachment.filename == filename)
        });
        let can_upload = document
            .download
            .as_ref()
            .is_some_and(|download| download.len() <= component.attachment_size_limit as usize);
        let include_download = has_existing_attachment || can_upload;
        let result = self
            .edit_private_document_components(component, meeting, &document, include_download)
            .await;
        let mut error = match result {
            Ok(()) => return,
            Err(error) => error,
        };

        if can_upload && !has_existing_attachment {
            tracing::warn!(
                meeting_id = %meeting.meta.id,
                %error,
                "Discord no permitió adjuntar el documento; reintentando sin descarga"
            );
            match self
                .edit_private_document_components(component, meeting, &document, false)
                .await
            {
                Ok(()) => return,
                Err(retry_error) => error = retry_error,
            }
        }

        tracing::warn!(
            meeting_id = %meeting.meta.id,
            %error,
            "Discord rechazó la tarjeta moderna; usando una respuesta compatible"
        );
        let _ = component
            .edit_response(
                &ctx.http,
                EditInteractionResponse::new()
                    .embed(private_document_fallback_embed(
                        meeting,
                        document.locale,
                        document.action,
                        &document.visible,
                    ))
                    .allowed_mentions(CreateAllowedMentions::new()),
            )
            .await;
    }

    async fn edit_private_document_components(
        &self,
        component: &ComponentInteraction,
        meeting: &Meeting,
        document: &PrivateMeetingDocument,
        include_download: bool,
    ) -> Result<(), String> {
        let filename = private_document_filename(document.action, document.locale);
        let existing_attachment = filename.and_then(|filename| {
            component
                .message
                .attachments
                .iter()
                .find(|attachment| attachment.filename == filename)
        });
        let attachment_id = if include_download {
            Some(match existing_attachment {
                Some(attachment) => serde_json::json!(attachment.id.get().to_string()),
                None => serde_json::json!(0),
            })
        } else {
            None
        };
        let payload = private_document_payload(
            meeting,
            document.action,
            document.locale,
            &document.visible,
            document.page,
            document.page_count,
            attachment_id,
        );
        let url = format!(
            "https://discord.com/api/v10/webhooks/{}/{}/messages/@original",
            component.application_id.get(),
            component.token
        );
        let request = reqwest::Client::new().patch(url).header(
            reqwest::header::USER_AGENT,
            format!("Kuali/{} ({KUALI_WEBSITE})", env!("CARGO_PKG_VERSION")),
        );
        let response = if include_download && existing_attachment.is_none() {
            let filename = filename.ok_or_else(|| "la vista no tiene una descarga".to_string())?;
            let download = document
                .download
                .as_ref()
                .ok_or_else(|| "la vista no tiene una descarga".to_string())?;
            let file = reqwest::multipart::Part::bytes(download.as_bytes().to_vec())
                .file_name(filename)
                .mime_str("text/plain; charset=utf-8")
                .map_err(|_| "no pude preparar el archivo de descarga".to_string())?;
            request
                .multipart(
                    reqwest::multipart::Form::new()
                        .text("payload_json", payload.to_string())
                        .part("files[0]", file),
                )
                .send()
                .await
        } else {
            request.json(&payload).send().await
        }
        .map_err(|error| format!("falló la conexión con Discord: {}", error.without_url()))?;

        let status = response.status();
        if status.is_success() {
            return Ok(());
        }
        let body = response.text().await.unwrap_or_default();
        Err(format!(
            "Discord respondió {status}: {}",
            truncate_text(&body, 300)
        ))
    }

    async fn handle_meeting_button(
        &self,
        ctx: &Context,
        component: &ComponentInteraction,
        action: MeetingAction,
        meeting_id: String,
        requested_page: usize,
        update_existing: bool,
    ) {
        let Some(guild_id) = component.guild_id else {
            return;
        };
        let locale = DiscordLocale::from_discord_locale(&component.locale);
        let deferred = if update_existing {
            CreateInteractionResponse::Acknowledge
        } else {
            CreateInteractionResponse::Defer(
                CreateInteractionResponseMessage::new().ephemeral(true),
            )
        };
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

        let Some(document) = private_meeting_document(&meeting, action, locale, requested_page)
        else {
            let message = if action == MeetingAction::Transcript {
                locale.text(
                    "Esta reunión todavía no tiene texto transcrito.",
                    "This meeting does not have transcribed text yet.",
                )
            } else {
                locale.text(
                    "Esta reunión todavía no tiene notas generadas.",
                    "This meeting does not have generated notes yet.",
                )
            };
            let _ = component
                .edit_response(&ctx.http, EditInteractionResponse::new().content(message))
                .await;
            return;
        };
        self.respond_with_private_document(ctx, component, &meeting, document)
            .await;
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
                if let Some((action, page, meeting_id)) =
                    MeetingAction::from_page_button(&component.data.custom_id)
                        .map(|(action, page, meeting_id)| (action, page, meeting_id.to_string()))
                {
                    self.handle_meeting_button(&ctx, &component, action, meeting_id, page, true)
                        .await;
                } else if let Some((action, meeting_id)) =
                    MeetingAction::from_button(&component.data.custom_id)
                        .map(|(action, meeting_id)| (action, meeting_id.to_string()))
                {
                    self.handle_meeting_button(&ctx, &component, action, meeting_id, 0, false)
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
        self.sync_summary_state(delivery, meeting, language, DiscordSummaryState::Ready)
            .await
    }

    /// Uses the same Discord message while a summary moves from preparation to
    /// its final or actionable error state.
    pub async fn sync_summary_state(
        &self,
        delivery: DiscordSummaryDelivery,
        meeting: &Meeting,
        language: &str,
        state: DiscordSummaryState,
    ) -> Result<DiscordSummaryDelivery, serenity::Error> {
        let locale = DiscordLocale::from_summary_language(language);
        let channel_id = ChannelId::new(delivery.channel_id);

        if let Some(message_id) = delivery.message_id {
            let message_id = MessageId::new(message_id);
            if channel_id
                .edit_message(
                    &self.http,
                    message_id,
                    summary_state_edit_message(meeting, locale, state),
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
                    fallback_summary_state_edit_message(meeting, locale, state),
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
            .send_message(&self.http, summary_state_message(meeting, locale, state))
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
                    .send_message(
                        &self.http,
                        fallback_summary_state_message(meeting, locale, state),
                    )
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

    fn component_count(component: &serde_json::Value) -> usize {
        1 + component["components"]
            .as_array()
            .into_iter()
            .flatten()
            .map(component_count)
            .sum::<usize>()
    }

    fn component_display_text_length(component: &serde_json::Value) -> usize {
        component["content"].as_str().map(char_count).unwrap_or(0)
            + component["components"]
                .as_array()
                .into_iter()
                .flatten()
                .map(component_display_text_length)
                .sum::<usize>()
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
        for action in MeetingAction::READY_ACTIONS {
            let custom_id = action.button_id("meeting-123");
            assert_eq!(
                MeetingAction::from_button(&custom_id),
                Some((action, "meeting-123"))
            );
            assert!(custom_id.len() <= 100);

            let page_id = action.page_button_id("meeting-123", 7);
            assert_eq!(
                MeetingAction::from_page_button(&page_id),
                Some((action, 7, "meeting-123"))
            );
            assert!(page_id.len() <= 100);
        }
        assert_eq!(MeetingAction::from_button("otro:meeting-123"), None);
        assert_eq!(MeetingAction::from_button(TRANSCRIPT_BUTTON_PREFIX), None);
        assert_eq!(MeetingAction::from_page_button("kuali:page:x:1:id"), None);
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
        assert_eq!(buttons.len(), 5);
        assert_eq!(buttons[0]["label"], "Resumen");
        assert_eq!(buttons[1]["label"], "Puntos clave");
        assert_eq!(buttons[2]["label"], "Decisiones");
        assert_eq!(buttons[3]["label"], "Preguntas");
        assert_eq!(buttons[4]["label"], "Transcripción");
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
            5
        );

        let rich_edit = serde_json::to_value(summary_state_edit_message(
            &completed_meeting(),
            DiscordLocale::Spanish,
            DiscordSummaryState::Ready,
        ))
        .unwrap();
        assert_eq!(rich_edit["content"], "");
        assert_eq!(rich_edit["embeds"].as_array().unwrap().len(), 1);
        assert_eq!(
            rich_edit["components"][0]["components"]
                .as_array()
                .unwrap()
                .len(),
            5
        );

        let fallback_edit = serde_json::to_value(fallback_summary_state_edit_message(
            &completed_meeting(),
            DiscordLocale::Spanish,
            DiscordSummaryState::Ready,
        ))
        .unwrap();
        assert!(fallback_edit["embeds"].as_array().unwrap().is_empty());
        assert!(fallback_edit["content"]
            .as_str()
            .unwrap()
            .contains("Preparar la publicación"));
    }

    #[test]
    fn public_tasks_fit_embeds_and_private_documents_page_without_losing_content() {
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

        let public =
            serde_json::to_value(completion_embed(&meeting, DiscordLocale::Spanish)).unwrap();
        assert!(embed_text_length(&public) <= 6_000);
        for field in public["fields"].as_array().unwrap() {
            assert!(char_count(field["name"].as_str().unwrap()) <= 256);
            assert!(char_count(field["value"].as_str().unwrap()) <= EMBED_FIELD_LIMIT);
        }
        assert!(public.to_string().contains("más en el resumen"));

        let visible = visible_summary_document(&meeting, DiscordLocale::Spanish).unwrap();
        assert!(visible.contains("Punto clave 39"));
        assert!(visible.contains("Tarea 29"));
        assert!(visible.contains("Pregunta"));

        let pages = paginate_private_document(&visible);
        assert!(pages.len() > 1);
        assert!(pages
            .iter()
            .all(|page| char_count(page) <= PRIVATE_PAGE_CHAR_LIMIT));

        let mut rendered = String::new();
        for (page, visible_page) in pages.iter().enumerate() {
            let private = private_document_payload(
                &meeting,
                MeetingAction::Summary,
                DiscordLocale::Spanish,
                visible_page,
                page,
                pages.len(),
                Some(serde_json::json!(0)),
            );
            assert_eq!(private["flags"], DISCORD_COMPONENTS_V2_FLAG);
            assert_eq!(private["content"], serde_json::Value::Null);
            assert!(private["embeds"].as_array().unwrap().is_empty());
            let components = private["components"].as_array().unwrap();
            assert!(components.len() <= 10);
            assert!(components.iter().map(component_count).sum::<usize>() <= 40);
            assert!(
                components
                    .iter()
                    .map(component_display_text_length)
                    .sum::<usize>()
                    < 4_000
            );
            let _: Vec<serenity::all::ActionRow> =
                serde_json::from_value(private["components"].clone()).unwrap();
            rendered.push_str(&private.to_string());
        }
        assert!(rendered.contains("Punto clave 39"));
        assert!(rendered.contains("Tarea 29"));
        assert!(rendered.contains("attachment://kuali-resumen.txt"));
        assert!(rendered.contains("Solo tú puedes ver este mensaje"));
    }

    #[test]
    fn focused_note_buttons_show_only_the_requested_section() {
        let meeting = completed_meeting();
        let cases = [
            (
                MeetingAction::KeyPoints,
                "La versión candidata está lista",
                "Publicar el viernes",
            ),
            (
                MeetingAction::Decisions,
                "Publicar el viernes",
                "La versión candidata está lista",
            ),
            (
                MeetingAction::OpenQuestions,
                "¿A qué hora se publica?",
                "Publicar el viernes",
            ),
        ];

        for (action, expected, unrelated) in cases {
            let document =
                private_meeting_document(&meeting, action, DiscordLocale::Spanish, 0).unwrap();
            assert!(document.visible.contains(expected));
            assert!(!document.visible.contains(unrelated));
            assert!(document.download.is_none());

            let payload = private_document_payload(
                &meeting,
                action,
                DiscordLocale::Spanish,
                &document.visible,
                document.page,
                document.page_count,
                None,
            );
            assert!(payload["attachments"].as_array().unwrap().is_empty());
            assert!(!payload.to_string().contains("attachment://"));
        }
    }

    #[test]
    fn long_transcripts_page_inside_one_card_without_losing_turns() {
        let mut meeting = completed_meeting();
        meeting.utterances.clear();
        for index in 0..260 {
            meeting.push_utterance(Utterance {
                id: format!("utterance-{index}"),
                speaker_id: 1,
                start_ms: index * 5_000,
                end_ms: index * 5_000 + 4_000,
                text: format!(
                    "Intervención {index}: {}",
                    "contenido importante ".repeat(12)
                ),
                confidence: Some(0.95),
            });
        }

        let visible = visible_transcript_document(&meeting, DiscordLocale::Spanish).unwrap();
        let pages = paginate_private_document(&visible);
        assert!(pages.len() > 1);
        assert!(pages
            .iter()
            .all(|page| char_count(page) <= PRIVATE_PAGE_CHAR_LIMIT));
        assert_eq!(pages.join("\n\n"), visible.trim_end());

        let document = private_meeting_document(
            &meeting,
            MeetingAction::Transcript,
            DiscordLocale::Spanish,
            0,
        )
        .unwrap();
        assert_eq!(document.page, 0);
        assert_eq!(document.page_count, pages.len());
        assert!(document.visible.contains("Intervención 0"));
        assert!(!document.visible.contains("Intervención 259"));

        let payload = private_document_payload(
            &meeting,
            document.action,
            document.locale,
            &document.visible,
            document.page,
            document.page_count,
            Some(serde_json::json!("987654321")),
        );
        let components = payload["components"].as_array().unwrap();
        assert!(components.len() <= 10);
        assert!(components.iter().map(component_count).sum::<usize>() <= 40);
        assert!(
            components
                .iter()
                .map(component_display_text_length)
                .sum::<usize>()
                < 4_000
        );
        assert_eq!(payload["attachments"][0]["id"], "987654321");
        assert!(payload.to_string().contains("Página 1 de"));
        let _: Vec<serenity::all::ActionRow> =
            serde_json::from_value(payload["components"].clone()).unwrap();

        let controls = components
            .iter()
            .find(|component| component["type"] == 1)
            .unwrap();
        let buttons = controls["components"].as_array().unwrap();
        assert_eq!(buttons.len(), 5);
        assert_eq!(buttons[0]["disabled"], true);
        assert_eq!(buttons[1]["disabled"], true);
        assert_eq!(buttons[2]["label"], format!("1/{}", pages.len()));
        assert_eq!(
            MeetingAction::from_page_button(buttons[3]["custom_id"].as_str().unwrap()),
            Some((MeetingAction::Transcript, 1, "meeting-123"))
        );

        let final_document = private_meeting_document(
            &meeting,
            MeetingAction::Transcript,
            DiscordLocale::Spanish,
            usize::MAX,
        )
        .unwrap();
        assert_eq!(final_document.page + 1, final_document.page_count);
        assert!(final_document.visible.contains("Intervención 259"));
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

        let visible_summary = visible_summary_document(&meeting, DiscordLocale::Spanish).unwrap();
        assert!(visible_summary.contains("### Resumen"));
        assert!(visible_summary.contains("### Puntos clave"));
        assert!(visible_summary.contains("### Decisiones"));
        assert!(visible_summary.contains("### Tareas pendientes"));
        assert!(visible_summary.contains("### Preguntas abiertas"));
        assert!(visible_summary.contains("Preparar la publicación"));
        assert!(!visible_summary.contains("meeting-123"));

        let visible_transcript =
            visible_transcript_document(&meeting, DiscordLocale::Spanish).unwrap();
        assert!(visible_transcript.contains("**00:05 · Ana**"));
        assert!(visible_transcript.contains("> Publicamos el viernes"));
        assert!(!visible_transcript.contains("meeting-123"));

        let private_transcript = private_document_payload(
            &meeting,
            MeetingAction::Transcript,
            DiscordLocale::Spanish,
            &visible_transcript,
            0,
            1,
            Some(serde_json::json!(0)),
        );
        assert_eq!(
            private_transcript["components"].as_array().unwrap().len(),
            1
        );
        assert!(private_transcript
            .to_string()
            .contains("attachment://kuali-transcripcion.txt"));
        assert!(private_transcript
            .to_string()
            .contains("Publicamos el viernes"));

        let public =
            serde_json::to_string(&completion_embed(&meeting, DiscordLocale::Spanish)).unwrap();
        assert!(!public.contains("ID de reunión"));
        assert!(!public.contains("meeting-123"));
    }

    #[test]
    fn one_public_card_moves_from_progress_to_success_or_actionable_failure() {
        let meeting = completed_meeting();
        let preparing = serde_json::to_value(summary_state_message(
            &meeting,
            DiscordLocale::Spanish,
            DiscordSummaryState::Preparing,
        ))
        .unwrap();
        assert!(preparing["embeds"][0]
            .to_string()
            .contains("Este mismo mensaje se actualizará"));
        let preparing_buttons = preparing["components"][0]["components"].as_array().unwrap();
        assert_eq!(preparing_buttons.len(), 1);
        assert_eq!(preparing_buttons[0]["label"], "Transcripción");

        let failed = serde_json::to_value(summary_state_edit_message(
            &meeting,
            DiscordLocale::Spanish,
            DiscordSummaryState::Failed,
        ))
        .unwrap();
        assert!(failed["embeds"][0]
            .to_string()
            .contains("después de tres intentos"));
        assert_eq!(
            failed["components"][0]["components"]
                .as_array()
                .unwrap()
                .len(),
            1
        );

        let attention = serde_json::to_value(summary_state_edit_message(
            &meeting,
            DiscordLocale::Spanish,
            DiscordSummaryState::AttentionRequired,
        ))
        .unwrap();
        assert!(attention["embeds"][0].to_string().contains("credenciales"));
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
            "Summary"
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
