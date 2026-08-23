//! Converts a transcript into a concise summary and actionable follow-up for
//! someone who missed the meeting.

use kuali_core::{
    sanitize_folder, sanitize_tags, ActionItem, Meeting, MeetingNote, MeetingSummary,
};
use serde::{Deserialize, Serialize};

use crate::json::{extract_json_object, non_empty};
use crate::provider::{CompletionRequest, LlmError, LlmProvider};

const MAX_TITLE_CHARS: usize = 60;
const MAX_ANALYSIS_TAGS: usize = 3;

/// Labels already used by the library. Supplying them lets the model organize
/// new meetings consistently instead of inventing near-duplicates each time.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct OrganizationContext {
    pub folders: Vec<String>,
    pub tags: Vec<String>,
}

/// Organization proposed as part of the same model pass that writes a summary.
/// folder always refers to an existing folder; new_folder is a deliberately
/// conservative proposal that the caller may create.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct MeetingOrganization {
    pub tags: Vec<String>,
    pub folder: Option<String>,
    pub new_folder: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct MeetingAnalysis {
    pub summary: MeetingSummary,
    pub organization: MeetingOrganization,
}

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
            "notes": {
                "type": "array",
                "items": {
                    "type": "object",
                    "properties": {
                        "text": { "type": "string" },
                        "author": { "type": "string" },
                        "timestamp": { "type": "string" }
                    },
                    "required": ["text", "author", "timestamp"],
                    "additionalProperties": false
                }
            },
            "openQuestions": { "type": "array", "items": { "type": "string" } },
            "tags": {
                "type": "array",
                "minItems": 0,
                "maxItems": MAX_ANALYSIS_TAGS,
                "items": { "type": "string" }
            },
            "folder": { "type": "string" },
            "newFolder": { "type": "string" }
        },
        "required": [
            "title", "overview", "keyPoints", "decisions", "actionItems", "notes", "openQuestions",
            "tags", "folder", "newFolder"
        ],
        "additionalProperties": false
    })
}

/// The prompt is written in English because that is where every provider is
/// strongest; the language of the *answer* is decided separately by
/// [`language_rule`], so the instructions never fight the output language.
pub fn system_prompt(language: &str) -> String {
    let language_rule = language_rule(language);
    format!(
        r#"You are Kuali's meeting analyst. You receive the automatic transcript of a voice meeting and return what is useful to someone who was not there: what was decided and what has to be done.

About the material you work with:

- It comes from automatic speech recognition, so it has misheard words, mangled proper nouns and sentences cut in half. Interpret with common sense instead of repeating word for word something that is clearly mistranscribed.
- Every line carries its timestamp and the name of whoever is speaking, taken from the meeting platform. Use them: a task with no owner is worth far less than one with an owner.
- Go through the commitments of every participant. Do not centre the tasks on whoever spoke the most, or on a single person.
- When a task has an owner, copy one of the names in the Participants list exactly. If the owner is unclear, use an empty string; do not invent or merge names.
- Someone mentioning something does not turn it into a task. A task is a commitment: someone is going to do something concrete. If none came up in the meeting, return an empty list; inventing tasks is worse than finding none.
- When someone says they are writing something down —"me lo apunto", "tomo nota", "anoto eso", "I'll write that down", in any language—, record that note under that person in "notes". The note is what that person would want to read later (the fact, the figure, the reference, the instruction), not the sentence they used to say they would note it down. A note is not a task: writing down a version number is a note; committing to update it is a task. If nobody asked to note anything, return an empty list.
- Meetings wander. Tell apart what was decided from what was only mentioned in passing.
- The title has to make the conversation recognisable tomorrow: between 4 and 9 words and 60 characters at most, centred on the main subject, with no date, no platform and not the word "meeting".
- Add between 1 and 3 short topical labels in "tags". Prefer a label from Existing tags whenever it applies, copying it exactly; create a new label only when the existing catalogue has no good match.
- "folder" files the meeting into one of Existing folders. Copy that folder exactly or use an empty string when none fits.
- "newFolder" is only for a stable, reusable category such as a project, product, client, or workstream. It must exactly match one of the labels proposed in "tags". Never create a folder from the meeting title, date, platform, a person's name, or a generic activity such as meeting, call, follow-up, planning, review, or stand-up. Do not make one folder per meeting. If there is any doubt, use an empty string.
- Reuse an existing folder instead of proposing a synonymous new one. Never fill both "folder" and "newFolder".

{language_rule}

Return a JSON object only, with no text around it and without wrapping it in a code block, in this shape:

{{
  "title": "short, specific name of the main subject",
  "overview": "two or three sentences on what the meeting was about and where it landed",
  "keyPoints": ["the points that actually matter"],
  "decisions": ["what was decided, not what was proposed"],
  "actionItems": [
    {{
      "text": "what has to be done",
      "assignee": "exact name from Participants of whoever committed to it; empty string if unclear",
      "due": "the deadline as it was said (\"on Friday\", \"before the demo\"); empty string if it was not mentioned",
      "timestamp": "the timestamp of the line the task comes from, in the same format as the transcript"
    }}
  ],
  "notes": [
    {{
      "text": "what that person wanted written down, phrased so it stands on its own",
      "author": "exact name from Participants of whoever said they were noting it; empty string if unclear",
      "timestamp": "the timestamp of the line where it was asked to be noted, in the same format as the transcript"
    }}
  ],
  "openQuestions": ["what was left unresolved"],
  "tags": ["one to three short topical labels; copy matching Existing tags exactly"],
  "folder": "exact value from Existing folders, or an empty string",
  "newFolder": "stable reusable category matching one proposed tag exactly, or an empty string"
}}"#
    )
}

/// Which language the summary comes back in. An empty or automatic setting
/// follows the meeting itself: the notes and tasks quote what people actually
/// said, so translating them costs more fidelity than it buys. Any other value
/// is an explicit request from the user and overrides the meeting.
pub fn language_rule(language: &str) -> String {
    let choice = language.trim();
    let automatic = choice.is_empty()
        || ["auto", "automatic", "automático", "automatico"]
            .iter()
            .any(|value| choice.eq_ignore_ascii_case(value));

    if automatic {
        "Write every field in the language the meeting was held in. If the transcript mixes languages, use the one most of the conversation is in.".to_string()
    } else {
        format!(
            "Write every field in {choice}, whatever language the meeting was held in. Keep proper nouns, product names and quoted figures as they were said."
        )
    }
}

pub fn user_prompt(meeting: &Meeting) -> String {
    user_prompt_with_context(meeting, &OrganizationContext::default())
}

fn user_prompt_with_context(meeting: &Meeting, context: &OrganizationContext) -> String {
    let participants = meeting
        .speakers
        .iter()
        .filter(|s| !s.is_bot)
        .map(|s| s.display_name.as_str())
        .collect::<Vec<_>>()
        .join(", ");

    format!(
        "Meeting: {}\nDate: {}\nDuration: {}\nParticipants: {}\nExisting folders: {}\nExisting tags: {}\n\n--- TRANSCRIPT ---\n{}",
        meeting.meta.source_title(),
        meeting.meta.started_at.format("%Y-%m-%d %H:%M UTC"),
        kuali_core::format_timestamp(meeting.duration_ms()),
        if participants.is_empty() {
            "(unknown)"
        } else {
            &participants
        },
        format_catalog(clean_folder_catalog(context.folders.iter().cloned())),
        format_catalog(clean_tag_catalog(context.tags.iter().cloned())),
        meeting.transcript_text()
    )
}

fn format_catalog(values: impl IntoIterator<Item = String>) -> String {
    let values = values.into_iter().collect::<Vec<_>>();
    if values.is_empty() {
        "(none)".to_string()
    } else {
        serde_json::to_string(&values).expect("a string catalogue always serializes")
    }
}

/// Requests the summary and organization together in one provider completion.
pub async fn analyze(
    provider: &dyn LlmProvider,
    meeting: &Meeting,
    language: &str,
    context: &OrganizationContext,
) -> Result<MeetingAnalysis, LlmError> {
    let request = CompletionRequest::new(
        system_prompt(language),
        user_prompt_with_context(meeting, context),
    )
    .with_schema(output_schema());

    let raw = provider.complete(&request).await?;
    let info = provider.info();
    let mut analysis = parse_analysis(&raw, &format!("{} · {}", info.label, info.model))?;
    if analysis.summary.title.trim().is_empty() {
        analysis.summary.title = meeting.fallback_title();
    }
    canonicalize_assignees(&mut analysis.summary, meeting);
    canonicalize_organization(&mut analysis.organization, context);
    Ok(analysis)
}

/// Requests a summary and returns validated output.
pub async fn summarize(
    provider: &dyn LlmProvider,
    meeting: &Meeting,
    language: &str,
) -> Result<MeetingSummary, LlmError> {
    Ok(
        analyze(provider, meeting, language, &OrganizationContext::default())
            .await?
            .summary,
    )
}

/// Models may return a handle or drop a diacritic despite exact-name prompting.
/// Normalization preserves one UI identity without discarding useful assignees.
fn canonicalize_assignees(summary: &mut MeetingSummary, meeting: &Meeting) {
    let people = summary
        .action_items
        .iter_mut()
        .map(|task| &mut task.assignee)
        .chain(summary.notes.iter_mut().map(|note| &mut note.author));

    for person in people {
        let Some(name) = person.as_deref() else {
            continue;
        };
        let normalized = normalize_person(name);
        let canonical = meeting.speakers.iter().find(|speaker| {
            !speaker.is_bot
                && [speaker.display_name.as_str(), speaker.username.as_str()]
                    .iter()
                    .any(|candidate| normalize_person(candidate) == normalized)
        });
        if let Some(speaker) = canonical {
            *person = Some(speaker.display_name.clone());
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
    #[serde(default, alias = "meeting_notes", alias = "meetingNotes")]
    notes: Vec<RawNote>,
    #[serde(default, alias = "open_questions")]
    open_questions: Vec<String>,
    #[serde(default)]
    tags: Vec<String>,
    #[serde(default)]
    folder: String,
    #[serde(default, alias = "new_folder")]
    new_folder: String,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct RawNote {
    #[serde(default, alias = "note", alias = "content")]
    text: String,
    #[serde(default, alias = "owner", alias = "speaker")]
    author: String,
    #[serde(default, alias = "time")]
    timestamp: String,
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
    Ok(parse_analysis(raw, generated_by)?.summary)
}

pub fn parse_analysis(raw: &str, generated_by: &str) -> Result<MeetingAnalysis, LlmError> {
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

    let organization = parse_organization(parsed.tags, &parsed.folder, &parsed.new_folder);

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

    let notes = parsed
        .notes
        .into_iter()
        .filter(|note| !note.text.trim().is_empty())
        .map(|note| MeetingNote {
            id: uuid::Uuid::new_v4().to_string(),
            text: note.text.trim().to_string(),
            author: non_empty(&note.author),
            source_ms: parse_timestamp(&note.timestamp),
        })
        .collect();

    let summary = MeetingSummary {
        title: clean_title(&parsed.title),
        overview: parsed.overview.trim().to_string(),
        key_points: clean(parsed.key_points),
        decisions: clean(parsed.decisions),
        action_items,
        notes,
        open_questions: clean(parsed.open_questions),
        generated_by: generated_by.to_string(),
    };
    if summary.overview.is_empty()
        && summary.key_points.is_empty()
        && summary.decisions.is_empty()
        && summary.action_items.is_empty()
        && summary.notes.is_empty()
        && summary.open_questions.is_empty()
    {
        return Err(LlmError::BadJson {
            provider: generated_by.to_string(),
            message: "the response contained no usable summary sections".into(),
        });
    }
    Ok(MeetingAnalysis {
        summary,
        organization,
    })
}

fn parse_organization(tags: Vec<String>, folder: &str, new_folder: &str) -> MeetingOrganization {
    let tags = clean_tag_catalog(tags)
        .into_iter()
        .take(MAX_ANALYSIS_TAGS)
        .collect::<Vec<_>>();
    let folder = sanitize_folder(folder);
    let new_folder = sanitize_folder(new_folder).and_then(|candidate| {
        tags.iter()
            .find(|tag| equivalent_label(tag, &candidate))
            .cloned()
    });

    MeetingOrganization {
        tags,
        folder,
        new_folder,
    }
}

fn canonicalize_organization(
    organization: &mut MeetingOrganization,
    context: &OrganizationContext,
) {
    let existing_tags = clean_tag_catalog(context.tags.iter().cloned());
    let existing_folders = clean_folder_catalog(context.folders.iter().cloned());

    organization.tags = clean_tag_catalog(std::mem::take(&mut organization.tags))
        .into_iter()
        .map(|tag| canonical_label(&tag, &existing_tags).unwrap_or(tag))
        .take(MAX_ANALYSIS_TAGS)
        .collect();

    organization.folder = organization
        .folder
        .take()
        .and_then(|folder| canonical_label(&folder, &existing_folders));

    if organization.folder.is_some() {
        organization.new_folder = None;
        return;
    }

    let Some(candidate) = organization.new_folder.take() else {
        return;
    };
    if let Some(existing) = canonical_label(&candidate, &existing_folders) {
        organization.folder = Some(existing);
        return;
    }
    organization.new_folder = organization
        .tags
        .iter()
        .find(|tag| equivalent_label(tag, &candidate))
        .cloned();
}

fn clean_tag_catalog(values: impl IntoIterator<Item = String>) -> Vec<String> {
    let mut clean: Vec<String> = Vec::new();
    for value in values {
        let Some(value) = sanitize_tags([value]).into_iter().next() else {
            continue;
        };
        if clean
            .iter()
            .any(|existing| equivalent_label(existing, &value))
        {
            continue;
        }
        clean.push(value);
    }
    clean
}

fn clean_folder_catalog(values: impl IntoIterator<Item = String>) -> Vec<String> {
    let mut clean: Vec<String> = Vec::new();
    for value in values {
        let Some(value) = sanitize_folder(&value) else {
            continue;
        };
        if clean
            .iter()
            .any(|existing| equivalent_label(existing, &value))
        {
            continue;
        }
        clean.push(value);
    }
    clean
}

fn canonical_label(value: &str, catalog: &[String]) -> Option<String> {
    catalog
        .iter()
        .find(|candidate| equivalent_label(candidate, value))
        .cloned()
}

fn equivalent_label(left: &str, right: &str) -> bool {
    left.trim().to_lowercase() == right.trim().to_lowercase()
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
    use std::sync::{Arc, Mutex};

    use async_trait::async_trait;

    use crate::provider::{ProviderInfo, ProviderKind};

    fn test_meeting() -> Meeting {
        serde_json::from_value(serde_json::json!({
            "meta": {
                "id": "meeting-1",
                "guildId": 1,
                "guildName": "Servidor",
                "channelId": 2,
                "channelName": "producto",
                "startedAt": "2026-08-06T12:00:00Z",
                "endedAt": "2026-08-06T12:30:00Z"
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
        .unwrap()
    }

    struct RecordingProvider {
        response: String,
        requests: Arc<Mutex<Vec<CompletionRequest>>>,
    }

    #[async_trait]
    impl LlmProvider for RecordingProvider {
        fn id(&self) -> &'static str {
            "recording"
        }

        fn info(&self) -> ProviderInfo {
            ProviderInfo {
                id: self.id().to_string(),
                label: "Recording".to_string(),
                model: "test".to_string(),
                kind: ProviderKind::LocalCli,
                structured_output: true,
            }
        }

        async fn is_available(&self) -> bool {
            true
        }

        async fn complete(&self, request: &CompletionRequest) -> Result<String, LlmError> {
            self.requests.lock().unwrap().push(request.clone());
            Ok(self.response.clone())
        }
    }

    #[test]
    fn timestamps_parse_in_every_shape_the_model_might_use() {
        assert_eq!(parse_timestamp("00:45"), Some(45_000));
        assert_eq!(parse_timestamp("[01:02]"), Some(62_000));
        assert_eq!(parse_timestamp("01:02:03"), Some(3_723_000));
        assert_eq!(parse_timestamp(""), None);
        assert_eq!(parse_timestamp("pronto"), None);
    }

    #[test]
    fn output_schema_requires_organization_without_weakening_summary_fields() {
        let schema = output_schema();
        let required = schema["required"].as_array().unwrap();
        for field in [
            "title",
            "overview",
            "keyPoints",
            "decisions",
            "actionItems",
            "notes",
            "openQuestions",
            "tags",
            "folder",
            "newFolder",
        ] {
            assert!(
                required.iter().any(|value| value == field),
                "{field} must remain required"
            );
        }
        assert_eq!(schema["properties"]["tags"]["minItems"], 0);
        assert_eq!(schema["properties"]["tags"]["maxItems"], MAX_ANALYSIS_TAGS);
        assert_eq!(schema["additionalProperties"], false);
    }

    #[test]
    fn organization_prompt_forbids_one_folder_per_meeting() {
        let prompt = system_prompt("auto");
        assert!(prompt.contains("Do not make one folder per meeting"));
        assert!(prompt.contains("project, product, client, or workstream"));
        assert!(prompt.contains("must exactly match one of the labels"));
        assert!(prompt.contains("If there is any doubt, use an empty string"));
    }

    #[test]
    fn user_prompt_includes_existing_catalogues_and_marks_empty_ones() {
        let meeting = test_meeting();
        let empty = user_prompt(&meeting);
        assert!(empty.contains("Existing folders: (none)"));
        assert!(empty.contains("Existing tags: (none)"));

        let prompt = user_prompt_with_context(
            &meeting,
            &OrganizationContext {
                folders: vec![" Producto ".into(), "Clientes".into()],
                tags: vec![" Backend ".into(), "UX".into()],
            },
        );
        assert!(prompt.contains(r#"Existing folders: ["Producto","Clientes"]"#));
        assert!(prompt.contains(r#"Existing tags: ["Backend","UX"]"#));
    }

    #[test]
    fn parse_analysis_sanitizes_deduplicates_and_bounds_organization() {
        let raw = r#"{
            "overview": "Resumen útil",
            "tags": ["  Proyecto   Aurora ", "producto", "PRODUCTO", "diseño", "DISEÑO", "extra"],
            "folder": "",
            "newFolder": "proyecto aurora"
        }"#;

        let analysis = parse_analysis(raw, "test").unwrap();
        assert_eq!(
            analysis.organization.tags,
            vec!["Proyecto Aurora", "producto", "diseño"]
        );
        assert_eq!(analysis.organization.folder, None);
        assert_eq!(
            analysis.organization.new_folder.as_deref(),
            Some("Proyecto Aurora")
        );
        assert_eq!(analysis.summary.overview, "Resumen útil");
    }

    #[test]
    fn a_new_folder_is_dropped_unless_it_matches_a_proposed_tag() {
        let raw =
            r#"{"overview":"x","tags":["Backend"],"folder":"","newFolder":"Proyecto secreto"}"#;
        let analysis = parse_analysis(raw, "test").unwrap();
        assert_eq!(analysis.organization.tags, vec!["Backend"]);
        assert_eq!(analysis.organization.new_folder, None);
    }

    #[test]
    fn an_unclassifiable_meeting_keeps_its_summary_with_empty_organization() {
        let raw = r#"{
            "overview": "La transcripción no alcanza para inferir un tema.",
            "tags": [],
            "folder": "",
            "newFolder": ""
        }"#;
        let analysis = parse_analysis(raw, "test").unwrap();
        assert!(analysis.organization.tags.is_empty());
        assert_eq!(analysis.organization.folder, None);
        assert_eq!(analysis.organization.new_folder, None);
        assert!(!analysis.summary.overview.is_empty());
    }

    #[tokio::test]
    async fn analyze_uses_one_call_and_canonicalizes_existing_labels() {
        let requests = Arc::new(Mutex::new(Vec::new()));
        let provider = RecordingProvider {
            response: r#"{
                "title": "Estado del producto",
                "overview": "Se revisó el avance.",
                "keyPoints": [],
                "decisions": [],
                "actionItems": [],
                "notes": [],
                "openQuestions": [],
                "tags": ["backend", "Investigación", "ux", "ignorada"],
                "folder": "producto",
                "newFolder": ""
            }"#
            .into(),
            requests: Arc::clone(&requests),
        };
        let context = OrganizationContext {
            folders: vec!["Producto".into(), "Clientes".into()],
            tags: vec!["Backend".into(), "UX".into()],
        };

        let analysis = analyze(&provider, &test_meeting(), "auto", &context)
            .await
            .unwrap();

        assert_eq!(analysis.organization.folder.as_deref(), Some("Producto"));
        assert_eq!(
            analysis.organization.tags,
            vec!["Backend", "Investigación", "UX"]
        );
        assert_eq!(analysis.organization.new_folder, None);

        let requests = requests.lock().unwrap();
        assert_eq!(requests.len(), 1, "summary and organization share one call");
        assert!(requests[0]
            .prompt
            .contains(r#"Existing folders: ["Producto","Clientes"]"#));
        assert!(requests[0].json_schema.is_some());
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
    fn an_empty_object_is_invalid_instead_of_becoming_a_blank_summary() {
        let error = parse_summary("{}", "test").unwrap_err();
        assert!(matches!(error, LlmError::BadJson { .. }));
    }

    #[test]
    fn notes_are_kept_with_their_author_and_position() {
        let raw = r#"{
            "overview": "x",
            "notes": [
                {"text": "La clave de la API caduca el 30 de septiembre", "author": "Ana", "timestamp": "04:10"},
                {"note": "Servidor de pruebas: 10.0.0.4", "owner": "", "time": ""},
                {"text": "   ", "author": "Ana", "timestamp": "01:00"}
            ]
        }"#;

        let summary = parse_summary(raw, "test").unwrap();

        assert_eq!(summary.notes.len(), 2);
        assert_eq!(summary.notes[0].author.as_deref(), Some("Ana"));
        assert_eq!(summary.notes[0].source_ms, Some(250_000));
        assert_eq!(summary.notes[1].text, "Servidor de pruebas: 10.0.0.4");
        assert_eq!(summary.notes[1].author, None);
    }

    #[test]
    fn a_summary_with_only_notes_is_still_usable() {
        let summary = parse_summary(
            r#"{"notes":[{"text":"Apuntar el número de versión: 1.2.4","author":"","timestamp":""}]}"#,
            "test",
        )
        .unwrap();
        assert_eq!(summary.notes.len(), 1);
    }

    #[test]
    fn the_prompt_separates_a_note_from_a_commitment() {
        let prompt = system_prompt("español");
        assert!(prompt.contains("me lo apunto"));
        assert!(prompt.contains("in any language"));
        assert!(prompt.contains("A note is not a task"));
    }

    #[test]
    fn prompt_requires_tasks_for_every_participant_with_exact_names() {
        let prompt = system_prompt("español");
        assert!(prompt.contains("commitments of every participant"));
        assert!(prompt.contains("copy one of the names in the Participants list exactly"));
    }

    #[test]
    fn a_configured_language_overrides_the_language_of_the_meeting() {
        let prompt = system_prompt("  español  ");
        assert!(prompt.contains("Write every field in español, whatever language"));
    }

    #[test]
    fn no_configured_language_follows_the_meeting() {
        for setting in ["", "  ", "auto", "Automático"] {
            let prompt = system_prompt(setting);
            assert!(
                prompt.contains("in the language the meeting was held in"),
                "{setting:?} should follow the meeting"
            );
        }
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
        ],"notes":[{"text":"Nota","author":"angela.dev","timestamp":""}]}"#;
        let mut summary = parse_summary(raw, "test").unwrap();

        canonicalize_assignees(&mut summary, &meeting);

        assert_eq!(summary.action_items[0].assignee.as_deref(), Some("Ángela"));
        assert_eq!(summary.action_items[1].assignee.as_deref(), Some("Ángela"));
        assert_eq!(
            summary.action_items[2].assignee.as_deref(),
            Some("Equipo de backend")
        );
        assert_eq!(summary.notes[0].author.as_deref(), Some("Ángela"));
    }
}
