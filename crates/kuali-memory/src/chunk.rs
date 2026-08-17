//! Cutting a meeting into passages worth retrieving.
//!
//! The transcript is the substance: "what exactly did Ana say about the
//! rollback?" is only answerable from the words themselves. So the transcript is
//! indexed in full, as overlapping windows that keep speaker and timestamp
//! attached.
//!
//! Summary items are indexed alongside it rather than instead of it. They are
//! short, already distilled, and answer a different kind of question — "what did
//! we decide" lands better on the decision line than on the two minutes of
//! discussion that produced it. Both layers compete in the same ranking, and the
//! question decides which wins.

use kuali_core::{format_timestamp, Meeting};

/// What a passage was made from, used to weight it during ranking and to label
/// the citation shown to the reader.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ChunkKind {
    Transcript,
    Overview,
    KeyPoint,
    Decision,
    Task,
    Note,
    Question,
}

impl ChunkKind {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Transcript => "transcript",
            Self::Overview => "overview",
            Self::KeyPoint => "key_point",
            Self::Decision => "decision",
            Self::Task => "task",
            Self::Note => "note",
            Self::Question => "question",
        }
    }

    pub fn from_label(value: &str) -> Self {
        match value {
            "overview" => Self::Overview,
            "key_point" => Self::KeyPoint,
            "decision" => Self::Decision,
            "task" => Self::Task,
            "note" => Self::Note,
            "question" => Self::Question,
            // An unknown kind comes from an index written by a newer Kuali.
            // Treating it as transcript keeps the passage usable instead of
            // dropping evidence over a label.
            _ => Self::Transcript,
        }
    }

    /// Nudge applied to a passage's score. Distilled lines are worth slightly
    /// more than raw speech at equal lexical relevance, because they already say
    /// what the meeting concluded. The margin stays small on purpose: a
    /// transcript window that matches the question well should still win.
    pub fn weight(self) -> f32 {
        match self {
            Self::Decision | Self::Task => 1.15,
            Self::Overview | Self::KeyPoint | Self::Note | Self::Question => 1.05,
            Self::Transcript => 1.0,
        }
    }
}

/// A passage ready to be written to the index.
#[derive(Debug, Clone, PartialEq)]
pub struct DraftChunk {
    pub kind: ChunkKind,
    pub start_ms: Option<u64>,
    pub end_ms: Option<u64>,
    /// Display names appearing in the passage, for the citation line.
    pub speakers: String,
    pub text: String,
}

/// Roughly how much transcript goes into one window.
///
/// Small enough that a retrieved passage is mostly about one thing, large enough
/// that a question and its answer usually stay together in the same window.
const TARGET_CHARS: usize = 900;

/// Turns repeated at the start of the next window, so an exchange split across a
/// boundary is still retrievable whole from one side of it.
const OVERLAP_TURNS: usize = 1;

/// One speaker's uninterrupted stretch, after merging their consecutive
/// utterances the way the summarizer already does.
struct Turn {
    name: String,
    start_ms: u64,
    end_ms: u64,
    text: String,
}

pub fn chunks(meeting: &Meeting) -> Vec<DraftChunk> {
    let mut drafts = transcript_chunks(meeting);
    drafts.extend(summary_chunks(meeting));
    drafts
}

fn turns(meeting: &Meeting) -> Vec<Turn> {
    let mut turns: Vec<Turn> = Vec::new();
    for utterance in &meeting.utterances {
        let text = utterance.text.trim();
        if text.is_empty() {
            continue;
        }
        let name = meeting.speaker_name(utterance.speaker_id);
        match turns.last_mut() {
            Some(last) if last.name == name => {
                last.text.push(' ');
                last.text.push_str(text);
                last.end_ms = utterance.end_ms;
            }
            _ => turns.push(Turn {
                name,
                start_ms: utterance.start_ms,
                end_ms: utterance.end_ms,
                text: text.to_string(),
            }),
        }
    }
    turns
}

fn transcript_chunks(meeting: &Meeting) -> Vec<DraftChunk> {
    let turns = turns(meeting);
    let mut drafts = Vec::new();
    let mut window: Vec<&Turn> = Vec::new();
    let mut length = 0usize;

    for turn in &turns {
        // A window always takes at least one turn, so a single turn longer than
        // the target still becomes its own passage instead of being dropped.
        if !window.is_empty() && length + turn.text.len() > TARGET_CHARS {
            drafts.push(render_window(&window));
            // Carrying turns forward only helps while something is left behind.
            // A window no longer than the overlap would otherwise reappear whole
            // inside the next one, indexing the same words twice and never
            // advancing.
            let carried = match window.len() > OVERLAP_TURNS {
                true => window.len() - OVERLAP_TURNS,
                false => window.len(),
            };
            window.drain(..carried);
            length = window.iter().map(|turn| turn.text.len()).sum();
        }
        length += turn.text.len();
        window.push(turn);
    }
    if !window.is_empty() {
        drafts.push(render_window(&window));
    }
    drafts
}

/// Renders a window in the same shape the summarizer sends to the LLM, so a
/// model reading a retrieved passage sees the format it was trained on here and
/// can quote a real timestamp back.
fn render_window(window: &[&Turn]) -> DraftChunk {
    let mut text = String::new();
    let mut speakers: Vec<&str> = Vec::new();
    for turn in window {
        text.push_str(&format!(
            "[{}] {}: {}\n",
            format_timestamp(turn.start_ms),
            turn.name,
            turn.text
        ));
        if !speakers.contains(&turn.name.as_str()) {
            speakers.push(&turn.name);
        }
    }

    DraftChunk {
        kind: ChunkKind::Transcript,
        start_ms: window.first().map(|turn| turn.start_ms),
        end_ms: window.last().map(|turn| turn.end_ms),
        speakers: speakers.join(", "),
        text,
    }
}

fn summary_chunks(meeting: &Meeting) -> Vec<DraftChunk> {
    let Some(summary) = &meeting.summary else {
        return Vec::new();
    };
    let mut drafts = Vec::new();

    let mut push = |kind: ChunkKind, speakers: String, text: String, start_ms: Option<u64>| {
        if !text.trim().is_empty() {
            drafts.push(DraftChunk {
                kind,
                start_ms,
                end_ms: None,
                speakers,
                text,
            });
        }
    };

    push(
        ChunkKind::Overview,
        String::new(),
        summary.overview.clone(),
        None,
    );
    for point in &summary.key_points {
        push(ChunkKind::KeyPoint, String::new(), point.clone(), None);
    }
    for decision in &summary.decisions {
        push(ChunkKind::Decision, String::new(), decision.clone(), None);
    }
    for question in &summary.open_questions {
        push(ChunkKind::Question, String::new(), question.clone(), None);
    }
    for task in &summary.action_items {
        // Joining owner, task, and deadline keeps them in one retrievable unit:
        // "what is Ana supposed to deliver on Friday" needs all three to match.
        let text = [
            task.assignee.as_deref(),
            Some(task.text.as_str()),
            task.due.as_deref(),
        ]
        .into_iter()
        .flatten()
        .filter(|part| !part.trim().is_empty())
        .collect::<Vec<_>>()
        .join(" · ");
        push(
            ChunkKind::Task,
            task.assignee.clone().unwrap_or_default(),
            text,
            task.source_ms,
        );
    }
    for note in &summary.notes {
        let text = [note.author.as_deref(), Some(note.text.as_str())]
            .into_iter()
            .flatten()
            .filter(|part| !part.trim().is_empty())
            .collect::<Vec<_>>()
            .join(" · ");
        push(
            ChunkKind::Note,
            note.author.clone().unwrap_or_default(),
            text,
            note.source_ms,
        );
    }

    drafts
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::Utc;
    use kuali_core::{
        color_for, ActionItem, MeetingMeta, MeetingSummary, Speaker, SpeakerId, Utterance,
    };

    fn meeting_with(utterances: Vec<(SpeakerId, u64, u64, &str)>) -> Meeting {
        let mut meeting = Meeting::new(MeetingMeta {
            id: "m1".into(),
            display_title: None,
            guild_id: 1,
            guild_name: "Servidor".into(),
            channel_id: 2,
            channel_name: "General".into(),
            started_at: Utc::now(),
            ended_at: None,
            tags: Vec::new(),
            folder: None,
        });
        for (user_id, name) in [(10u64, "Ana"), (20, "Luis")] {
            meeting.upsert_speaker(Speaker {
                user_id,
                source_id: None,
                audio_kind: None,
                display_name: name.into(),
                username: name.to_lowercase(),
                avatar_url: None,
                color: color_for(user_id).to_string(),
                is_bot: false,
                is_self: false,
            });
        }
        for (speaker_id, start_ms, end_ms, text) in utterances {
            meeting.push_utterance(Utterance {
                id: format!("{speaker_id}-{start_ms}"),
                speaker_id,
                start_ms,
                end_ms,
                text: text.into(),
                confidence: None,
            });
        }
        meeting
    }

    #[test]
    fn a_transcript_passage_keeps_who_spoke_and_when() {
        let meeting = meeting_with(vec![
            (10, 0, 1_000, "hola"),
            (10, 1_000, 2_000, "vamos con el despliegue"),
            (20, 62_000, 63_000, "de acuerdo"),
        ]);

        let drafts = chunks(&meeting);
        let transcript = &drafts[0];

        assert_eq!(transcript.kind, ChunkKind::Transcript);
        assert_eq!(
            transcript.text,
            "[00:00] Ana: hola vamos con el despliegue\n[01:02] Luis: de acuerdo\n"
        );
        assert_eq!(transcript.speakers, "Ana, Luis");
        assert_eq!(transcript.start_ms, Some(0));
        assert_eq!(transcript.end_ms, Some(63_000));
    }

    #[test]
    fn a_long_transcript_is_split_into_overlapping_windows() {
        let long = "palabra ".repeat(80);
        let meeting = meeting_with(vec![
            (10, 0, 1_000, long.as_str()),
            (20, 1_000, 2_000, long.as_str()),
            (10, 2_000, 3_000, "cierre"),
        ]);

        let windows: Vec<_> = chunks(&meeting)
            .into_iter()
            .filter(|draft| draft.kind == ChunkKind::Transcript)
            .collect();

        assert!(windows.len() > 1, "a long meeting needs several windows");
        // The turn ending one window opens the next, so an exchange split at the
        // boundary is still retrievable in one piece.
        assert!(windows[1].text.contains("[00:01] Luis"));
        assert!(windows[1].text.contains("cierre"));
        // No window may contain another, which is what happens when the overlap
        // is carried out of a window that had nothing else in it.
        for pair in windows.windows(2) {
            assert!(!pair[1].text.contains(&pair[0].text));
        }
    }

    #[test]
    fn a_window_overlaps_by_one_turn_when_there_is_something_to_leave_behind() {
        let turn = "palabra ".repeat(50);
        let meeting = meeting_with(vec![
            (10, 0, 1_000, turn.as_str()),
            (20, 1_000, 2_000, turn.as_str()),
            (10, 2_000, 3_000, turn.as_str()),
        ]);

        let windows: Vec<_> = chunks(&meeting)
            .into_iter()
            .filter(|draft| draft.kind == ChunkKind::Transcript)
            .collect();

        assert_eq!(windows.len(), 2);
        // The second turn closes the first window and opens the second.
        assert!(windows[0].text.contains("[00:01] Luis"));
        assert!(windows[1].text.contains("[00:01] Luis"));
        assert!(windows[1].text.contains("[00:02] Ana"));
    }

    #[test]
    fn a_turn_longer_than_a_window_still_becomes_a_passage() {
        let huge = "x".repeat(TARGET_CHARS * 3);
        let meeting = meeting_with(vec![(10, 0, 1_000, huge.as_str())]);

        let windows: Vec<_> = chunks(&meeting)
            .into_iter()
            .filter(|draft| draft.kind == ChunkKind::Transcript)
            .collect();

        assert_eq!(windows.len(), 1);
        assert!(windows[0].text.contains(&huge));
    }

    #[test]
    fn a_task_stays_in_one_passage_with_its_owner_and_deadline() {
        let mut meeting = meeting_with(vec![(10, 0, 1_000, "hola")]);
        meeting.summary = Some(MeetingSummary {
            overview: "Hablamos del despliegue.".into(),
            decisions: vec!["Posponer el rollout".into()],
            action_items: vec![ActionItem {
                id: "t1".into(),
                text: "Preparar la demostración".into(),
                assignee: Some("Ana".into()),
                due: Some("el viernes".into()),
                source_ms: Some(1_000),
                done: false,
            }],
            ..Default::default()
        });

        let drafts = chunks(&meeting);
        let task = drafts
            .iter()
            .find(|draft| draft.kind == ChunkKind::Task)
            .expect("the task should be indexed");

        assert_eq!(task.text, "Ana · Preparar la demostración · el viernes");
        assert_eq!(task.speakers, "Ana");
        assert_eq!(task.start_ms, Some(1_000));

        assert!(drafts.iter().any(|draft| draft.kind == ChunkKind::Decision));
        assert!(drafts.iter().any(|draft| draft.kind == ChunkKind::Overview));
    }

    #[test]
    fn a_meeting_without_a_summary_still_indexes_its_transcript() {
        let meeting = meeting_with(vec![(10, 0, 1_000, "algo se dijo")]);
        let drafts = chunks(&meeting);

        assert_eq!(drafts.len(), 1);
        assert_eq!(drafts[0].kind, ChunkKind::Transcript);
    }

    #[test]
    fn empty_utterances_never_become_passages() {
        let meeting = meeting_with(vec![(10, 0, 1_000, "   "), (20, 1_000, 2_000, "")]);
        assert!(chunks(&meeting).is_empty());
    }
}
