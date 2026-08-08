//! Converts a transcript into a concise summary and actionable follow-up for
//! someone who missed the meeting.

use kuali_core::{ActionItem, Meeting, MeetingSummary};
use serde::Deserialize;

use crate::json::{extract_json_object, non_empty};
use crate::provider::{CompletionRequest, LlmError, LlmProvider};

const MAX_TITLE_CHARS: usize = 60;

/// Output contract. Optional fields intentionally use an empty string instead of
/// `null` because several structured-output modes reject nullable types, while
/// an empty string maps unambiguously to `None`.
pub fn output_schema() -> serde_json::Value {
    serde_json::json!({
        "type": "object",
        "properties": {
            "title": { "type": "string", "maxLength": MAX_TITLE_CHARS },
            "overview": { "type": "string" },
            "keyPoints": { "type": "array", "items": { "type": "string" } },
            "decisions": { "type": "array", "items": { "type": "string" } },
            "actionItems": {
                "type": "array",
                "items": {
                    "type": "object",
                    "properties": {
                        "text": { "type": "string" },
                        "assignee": { "type": "string" },
                        "due": { "type": "string" },
                        "timestamp": { "type": "string" }
                    },
                    "required": ["text", "assignee", "due", "timestamp"],
                    "additionalProperties": false
                }
            },
            "openQuestions": { "type": "array", "items": { "type": "string" } }
        },
        "required": ["title", "overview", "keyPoints", "decisions", "actionItems", "openQuestions"],
        "additionalProperties": false
    })
}

pub fn system_prompt(language: &str) -> String {
    format!(
        r#"Eres el analista de reuniones de Kuali. Recibes la transcripción automática de una reunión de voz y devuelves lo que le sirve a alguien que no estuvo: qué se decidió y qué hay que hacer.

Sobre el material con el que trabajas:

- Viene de reconocimiento automático de voz, así que tiene palabras mal reconocidas, nombres propios destrozados y frases cortadas por la mitad. Interpreta con sentido común en lugar de repetir literalmente algo que claramente está mal transcrito.
- Cada línea lleva su marca de tiempo y el nombre de quien habla, tomado de Discord. Úsalos: una tarea sin dueño vale bastante menos que una con dueño.
- Revisa los compromisos de todos los participantes. No centres las tareas en quien más habló ni en una sola persona.
- Cuando una tarea tenga dueño, copia exactamente uno de los nombres de la lista de Participantes. Si el dueño no queda claro, usa una cadena vacía; no inventes ni combines nombres.
- Que alguien mencione algo no lo convierte en tarea. Una tarea es un compromiso: alguien va a hacer algo concreto. Si en la reunión no salió ninguna, devuelve la lista vacía; inventar tareas es peor que no encontrarlas.
- Las reuniones se van por las ramas. Distingue lo que se decidió de lo que solo se comentó de paso.
- El título debe permitir reconocer la conversación mañana: entre 4 y 9 palabras y 60 caracteres como máximo, centrado en el asunto principal, sin fecha, plataforma ni la palabra "reunión".

Escribe en {language}.

Devuelve únicamente un objeto JSON, sin texto alrededor y sin envolverlo en un bloque de código, con esta forma:

{{
  "title": "nombre breve y específico del tema principal",
  "overview": "dos o tres frases sobre de qué fue la reunión y en qué quedó",
  "keyPoints": ["los puntos que de verdad importan"],
  "decisions": ["lo que se decidió, no lo que se propuso"],
  "actionItems": [
    {{
      "text": "qué hay que hacer",
      "assignee": "nombre exacto de Participantes de quien se comprometió; cadena vacía si no queda claro",
      "due": "el plazo tal y como se dijo (\"el viernes\", \"antes de la demo\"); cadena vacía si no se mencionó",
      "timestamp": "la marca de tiempo de la línea de la que sale la tarea, en el mismo formato que la transcripción"
    }}
  ],
  "openQuestions": ["lo que quedó sin resolver"]
}}"#
    )
}

pub fn user_prompt(meeting: &Meeting) -> String {
    let participants = meeting
        .speakers
        .iter()
        .filter(|s| !s.is_bot)
        .map(|s| s.display_name.as_str())
        .collect::<Vec<_>>()
        .join(", ");

    format!(
        "Reunión: {}\nFecha: {}\nDuración: {}\nParticipantes: {}\n\n--- TRANSCRIPCIÓN ---\n{}",
        meeting.meta.source_title(),
        meeting.meta.started_at.format("%Y-%m-%d %H:%M UTC"),
        kuali_core::format_timestamp(meeting.duration_ms()),
        if participants.is_empty() {
            "(desconocidos)"
        } else {
            &participants
        },
        meeting.transcript_text()
    )
}

/// Requests a summary and returns validated output.
pub async fn summarize(
    provider: &dyn LlmProvider,
    meeting: &Meeting,
    language: &str,
) -> Result<MeetingSummary, LlmError> {
    let request = CompletionRequest::new(system_prompt(language), user_prompt(meeting))
        .with_schema(output_schema());

    let raw = provider.complete(&request).await?;
    let info = provider.info();
    let mut summary = parse_summary(&raw, &format!("{} · {}", info.label, info.model))?;
    if summary.title.trim().is_empty() {
        summary.title = meeting.fallback_title();
    }
    canonicalize_assignees(&mut summary, meeting);
    Ok(summary)
}

/// Models may return a handle or drop a diacritic despite exact-name prompting.
/// Normalization preserves one UI identity without discarding useful assignees.
fn canonicalize_assignees(summary: &mut MeetingSummary, meeting: &Meeting) {
    for task in &mut summary.action_items {
        let Some(assignee) = task.assignee.as_deref() else {
            continue;
        };
        let normalized = normalize_person(assignee);
        let canonical = meeting.speakers.iter().find(|speaker| {
            !speaker.is_bot
                && [speaker.display_name.as_str(), speaker.username.as_str()]
                    .iter()
                    .any(|name| normalize_person(name) == normalized)
        });
        if let Some(speaker) = canonical {
            task.assignee = Some(speaker.display_name.clone());
        }
    }
}

fn normalize_person(value: &str) -> String {
    value
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
        .collect::<String>()
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct RawSummary {
    #[serde(default, alias = "meeting_title", alias = "meetingTitle")]
    title: String,
    #[serde(default)]
    overview: String,
    #[serde(default, alias = "key_points")]
    key_points: Vec<String>,
    #[serde(default)]
    decisions: Vec<String>,
    #[serde(default, alias = "action_items", alias = "tasks")]
    action_items: Vec<RawActionItem>,
    #[serde(default, alias = "open_questions")]
    open_questions: Vec<String>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct RawActionItem {
    #[serde(default, alias = "task", alias = "description")]
    text: String,
    #[serde(default, alias = "owner")]
    assignee: String,
    #[serde(default, alias = "deadline")]
    due: String,
    #[serde(default, alias = "time")]
    timestamp: String,
}

pub fn parse_summary(raw: &str, generated_by: &str) -> Result<MeetingSummary, LlmError> {
    let json = extract_json_object(raw).ok_or_else(|| LlmError::BadJson {
        provider: generated_by.to_string(),
        message: format!(
            "response did not contain a JSON object: {}",
            truncate(raw, 200)
        ),
    })?;

    let parsed: RawSummary = serde_json::from_str(json).map_err(|e| LlmError::BadJson {
        provider: generated_by.to_string(),
        message: e.to_string(),
    })?;

    let action_items = parsed
        .action_items
        .into_iter()
        .filter(|item| !item.text.trim().is_empty())
        .map(|item| ActionItem {
            id: uuid::Uuid::new_v4().to_string(),
            text: item.text.trim().to_string(),
            assignee: non_empty(&item.assignee),
            due: non_empty(&item.due),
            source_ms: parse_timestamp(&item.timestamp),
            done: false,
        })
        .collect();

    Ok(MeetingSummary {
        title: clean_title(&parsed.title),
        overview: parsed.overview.trim().to_string(),
        key_points: clean(parsed.key_points),
        decisions: clean(parsed.decisions),
        action_items,
        open_questions: clean(parsed.open_questions),
        generated_by: generated_by.to_string(),
    })
}

fn clean_title(value: &str) -> String {
    let trimmed = value.trim();
    if trimmed.chars().count() <= MAX_TITLE_CHARS {
        return trimmed.to_string();
    }
    let candidate = trimmed
        .chars()
        .take(MAX_TITLE_CHARS - 1)
        .collect::<String>();
    let boundary = candidate
        .char_indices()
        .rev()
        .find(|(_, character)| character.is_whitespace())
        .map(|(index, _)| index)
        .filter(|index| *index >= MAX_TITLE_CHARS / 2)
        .unwrap_or(candidate.len());
    format!(
        "{}…",
        candidate[..boundary]
            .trim_end_matches(|character: char| { matches!(character, ' ' | '-' | ':') })
    )
}

fn clean(values: Vec<String>) -> Vec<String> {
    values
        .into_iter()
        .map(|v| v.trim().to_string())
        .filter(|v| !v.is_empty())
        .collect()
}

fn truncate(text: &str, max: usize) -> String {
    let trimmed = text.trim();
    if trimmed.chars().count() <= max {
        return trimmed.to_string();
    }
    let cut: String = trimmed.chars().take(max).collect();
    format!("{cut}…")
}

/// Converts `MM:SS`, `HH:MM:SS`, or `[HH:MM:SS]` to milliseconds. Invalid or
/// invented timestamps return `None` rather than failing the whole summary.
pub fn parse_timestamp(value: &str) -> Option<u64> {
    let cleaned: String = value
        .chars()
        .filter(|c| c.is_ascii_digit() || *c == ':')
        .collect();
    if cleaned.is_empty() {
        return None;
    }

    let parts: Vec<u64> = cleaned
        .split(':')
        .map(|p| p.parse::<u64>().ok())
        .collect::<Option<Vec<_>>>()?;

    let seconds = match parts.as_slice() {
        [s] => *s,
        [m, s] => m * 60 + s,
        [h, m, s] => h * 3600 + m * 60 + s,
        _ => return None,
    };
    Some(seconds * 1000)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn timestamps_parse_in_every_shape_the_model_might_use() {
        assert_eq!(parse_timestamp("00:45"), Some(45_000));
        assert_eq!(parse_timestamp("[01:02]"), Some(62_000));
        assert_eq!(parse_timestamp("01:02:03"), Some(3_723_000));
        assert_eq!(parse_timestamp(""), None);
        assert_eq!(parse_timestamp("pronto"), None);
    }

    #[test]
    fn parses_a_well_formed_summary() {
        let raw = r#"{
            "title": "Plan de la demo del viernes",
            "overview": "Repaso del sprint.",
            "keyPoints": ["La demo se retrasa", "  "],
            "decisions": ["Cortamos el alcance"],
            "actionItems": [
                {"text": "Preparar la demo", "assignee": "Ana", "due": "el viernes", "timestamp": "12:30"},
                {"text": "  ", "assignee": "", "due": "", "timestamp": ""}
            ],
            "openQuestions": []
        }"#;

        let summary = parse_summary(raw, "test").expect("summary should parse");
        assert_eq!(summary.title, "Plan de la demo del viernes");
        assert_eq!(summary.overview, "Repaso del sprint.");
        // Blank input is discarded.
        assert_eq!(summary.key_points, vec!["La demo se retrasa"]);
        assert_eq!(summary.action_items.len(), 1);

        let task = &summary.action_items[0];
        assert_eq!(task.assignee.as_deref(), Some("Ana"));
        assert_eq!(task.due.as_deref(), Some("el viernes"));
        assert_eq!(task.source_ms, Some(750_000));
        assert!(!task.done);
        assert_eq!(summary.generated_by, "test");
    }

    #[test]
    fn a_provider_cannot_overflow_the_meeting_title() {
        let raw = r#"{
            "title": "Planificación extraordinariamente detallada del lanzamiento internacional de la nueva plataforma Kuali",
            "overview": "x", "keyPoints": [], "decisions": [],
            "actionItems": [], "openQuestions": []
        }"#;
        let summary = parse_summary(raw, "test").unwrap();
        assert!(summary.title.chars().count() <= MAX_TITLE_CHARS);
        assert!(summary.title.ends_with('…'));
    }

    #[test]
    fn empty_optional_fields_become_none() {
        let raw = r#"{"overview":"x","keyPoints":[],"decisions":[],
            "actionItems":[{"text":"Hacer algo","assignee":"","due":"","timestamp":""}],
            "openQuestions":[]}"#;
        let summary = parse_summary(raw, "test").unwrap();
        let task = &summary.action_items[0];
        assert_eq!(task.assignee, None);
        assert_eq!(task.due, None);
        assert_eq!(task.source_ms, None);
    }

    #[test]
    fn survives_snake_case_from_a_less_obedient_model() {
        let raw = r#"{"overview":"x","key_points":["a"],"decisions":[],
            "action_items":[{"task":"Hacer algo","owner":"Luis","deadline":"mañana","time":"00:10"}],
            "open_questions":["¿y esto?"]}"#;
        let summary = parse_summary(raw, "test").unwrap();
        assert_eq!(summary.key_points, vec!["a"]);
        assert_eq!(summary.action_items[0].text, "Hacer algo");
        assert_eq!(summary.action_items[0].assignee.as_deref(), Some("Luis"));
        assert_eq!(summary.open_questions, vec!["¿y esto?"]);
    }

    #[test]
    fn survives_a_chatty_model_that_wraps_the_json_in_prose() {
        let raw = "Claro, aquí tienes el resumen:\n\n```json\n{\"overview\":\"ok\",\"keyPoints\":[],\"decisions\":[],\"actionItems\":[],\"openQuestions\":[]}\n```\n\n¿Necesitas algo más?";
        assert_eq!(parse_summary(raw, "test").unwrap().overview, "ok");
    }

    #[test]
    fn a_response_with_no_json_is_a_clear_error_not_a_panic() {
        let err = parse_summary("Lo siento, no puedo ayudarte con eso.", "test").unwrap_err();
        assert!(matches!(err, LlmError::BadJson { .. }));
    }

    #[test]
    fn missing_arrays_default_to_empty_instead_of_failing() {
        let summary = parse_summary(r#"{"overview":"solo esto"}"#, "test").unwrap();
        assert_eq!(summary.overview, "solo esto");
        assert!(summary.action_items.is_empty());
    }

    #[test]
    fn prompt_requires_tasks_for_every_participant_with_exact_names() {
        let prompt = system_prompt("español");
        assert!(prompt.contains("compromisos de todos los participantes"));
        assert!(prompt.contains("copia exactamente uno de los nombres"));
    }

    #[test]
    fn assignees_using_a_handle_or_missing_an_accent_become_display_names() {
        let meeting: Meeting = serde_json::from_value(serde_json::json!({
            "meta": {
                "id": "meeting-1",
                "guildId": 1,
                "guildName": "Servidor",
                "channelId": 2,
                "channelName": "producto",
                "startedAt": "2026-08-06T12:00:00Z",
                "endedAt": null
            },
            "speakers": [{
                "userId": 3,
                "displayName": "Ángela",
                "username": "angela.dev",
                "avatarUrl": null,
                "color": "#fff",
                "isBot": false
            }],
            "utterances": [],
            "summary": null
        }))
        .unwrap();
        let raw = r#"{"overview":"x","actionItems":[
            {"text":"Uno","assignee":"angela.dev","due":"","timestamp":""},
            {"text":"Dos","assignee":"Angela","due":"","timestamp":""},
            {"text":"Tres","assignee":"Equipo de backend","due":"","timestamp":""}
        ]}"#;
        let mut summary = parse_summary(raw, "test").unwrap();

        canonicalize_assignees(&mut summary, &meeting);

        assert_eq!(summary.action_items[0].assignee.as_deref(), Some("Ángela"));
        assert_eq!(summary.action_items[1].assignee.as_deref(), Some("Ángela"));
        assert_eq!(
            summary.action_items[2].assignee.as_deref(),
            Some("Equipo de backend")
        );
    }
}
