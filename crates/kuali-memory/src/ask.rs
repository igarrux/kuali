//! Turning retrieved passages into an answer.
//!
//! The prompt lives next to the retrieval that feeds it because the two are one
//! contract: the model is told that the excerpts are the entire world, and
//! retrieval is what makes that true. Passages outside the asker's audience were
//! never fetched, so "answer only from the excerpts" and "answer only from what
//! this person may read" are the same instruction.
//!
//! Citations are validated against the passages that were actually sent. A model
//! cannot cite a meeting into existence, and it cannot cite one it was not
//! given — the number simply will not resolve.

use kuali_llm::{CompletionRequest, LlmError, LlmProvider};
use serde::Deserialize;

use crate::retrieve::Passage;
use crate::{Audience, Memory, Result};

/// Passages sent to the model. Beyond this, extra evidence mostly repeats what
/// the top hits already said while crowding the context window.
const MAX_PASSAGES: usize = 14;

/// Total characters of evidence in one request, chosen to stay comfortable on
/// the smaller context windows Kuali supports through local and CLI providers.
const MAX_EVIDENCE_CHARS: usize = 12_000;

/// Longest single passage. A transcript window is already bounded, but a
/// pathological turn should not consume the whole budget by itself.
const MAX_PASSAGE_CHARS: usize = 1_400;

/// What Kuali found. The empty case is a variant rather than an empty string so
/// the caller has to phrase it, in the language of whoever asked, instead of
/// showing a blank answer.
#[derive(Debug, Clone, PartialEq)]
pub enum Answer {
    /// Nothing this person can reach discusses the question.
    NothingFound,
    Answered {
        text: String,
        /// Meetings the answer rests on, newest first, deduplicated.
        citations: Vec<Citation>,
    },
}

/// Who is asking, as far as the answer is concerned.
///
/// Separate from [`Audience`], which decides what may be *read*. This decides
/// only whether "what did I commit to?" can be resolved into a name. Getting it
/// wrong misattributes a task; it can never widen what the search reaches.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Asker {
    /// Names this person appears under in transcripts, best first.
    pub names: Vec<String>,
    /// Whether the platform vouched for the first name.
    ///
    /// Discord authenticates the account behind a slash command, so the name is
    /// certain. In the desktop application it is whatever the person typed into
    /// Settings, which is a claim, not proof — and the prompt is told which of
    /// the two it has.
    pub verified: bool,
}

impl Asker {
    pub fn unknown() -> Self {
        Self::default()
    }

    pub fn named(names: Vec<String>, verified: bool) -> Self {
        let names: Vec<String> = names
            .into_iter()
            .map(|name| name.trim().to_string())
            .filter(|name| !name.is_empty())
            .collect();
        Self { names, verified }
    }

    fn is_known(&self) -> bool {
        !self.names.is_empty()
    }

    /// The line handed to the model, or nothing when Kuali does not know who is
    /// asking. Saying nothing is deliberate: a guessed identity produces
    /// confident answers about the wrong person.
    fn describe(&self) -> Option<String> {
        if !self.is_known() {
            return None;
        }
        let (first, rest) = self.names.split_first()?;
        let also = match rest.is_empty() {
            true => String::new(),
            false => format!(" They also appear as {}.", rest.join(", ")),
        };
        Some(match self.verified {
            true => format!(
                "The person asking is {first}, confirmed by the platform they are asking from.{also}"
            ),
            false => format!(
                "The person asking says they are {first}.{also} That is their own claim, not something verified, so if no participant plausibly matches it, answer without attributing anything to them and say you could not tell which participant they are."
            ),
        })
    }
}

/// Where a claim came from, in the form the reader needs to go check it.
#[derive(Debug, Clone, PartialEq)]
pub struct Citation {
    pub meeting_id: String,
    pub meeting_title: String,
    pub channel_name: String,
    pub started_at: chrono::DateTime<chrono::Utc>,
    /// Position in the transcript, when the cited passage had one.
    pub start_ms: Option<u64>,
}

impl Memory {
    /// The evidence for a question, already trimmed to what one request can
    /// carry.
    ///
    /// Separate from [`answer`] because the index is a blocking SQLite handle
    /// while the model call is a long await. Fetching first and generating
    /// afterwards means one person's question never holds the index while a
    /// provider thinks.
    pub fn evidence_for(&self, audience: &Audience, question: &str) -> Result<Vec<Passage>> {
        self.evidence_with(audience, question, None)
    }

    /// The same, using meaning as well as wording when the model is loaded.
    pub fn evidence_with(
        &self,
        audience: &Audience,
        question: &str,
        embedder: Option<&mut crate::embed::Embedder>,
    ) -> Result<Vec<Passage>> {
        Ok(within_budget(self.retrieve_with(
            audience,
            question,
            MAX_PASSAGES,
            embedder,
        )?))
    }
}

/// Answers a question from passages already retrieved for a specific audience.
///
/// Every passage reaching this function came out of [`Memory::retrieve`], which
/// cannot run without an [`Audience`]. That is what lets the prompt tell the
/// model the excerpts are the entire world and be telling the truth.
pub async fn answer(
    provider: &dyn LlmProvider,
    question: &str,
    passages: &[Passage],
    language: &str,
    asker: &Asker,
) -> std::result::Result<Answer, LlmError> {
    if passages.is_empty() {
        return Ok(Answer::NothingFound);
    }
    answer_from(provider, question, passages, language, asker).await
}

/// Trims the evidence to what one request can carry, strongest first.
fn within_budget(passages: Vec<Passage>) -> Vec<Passage> {
    let mut kept = Vec::new();
    let mut budget = MAX_EVIDENCE_CHARS;
    for mut passage in passages {
        if passage.text.chars().count() > MAX_PASSAGE_CHARS {
            passage.text = passage
                .text
                .chars()
                .take(MAX_PASSAGE_CHARS)
                .collect::<String>()
                + "…";
        }
        let cost = passage.text.chars().count();
        // Always keep the best passage: a single oversized one is still the
        // answer, and returning nothing because it did not fit is worse.
        if cost > budget && !kept.is_empty() {
            break;
        }
        budget = budget.saturating_sub(cost);
        kept.push(passage);
    }
    kept
}

async fn answer_from(
    provider: &dyn LlmProvider,
    question: &str,
    passages: &[Passage],
    language: &str,
    asker: &Asker,
) -> std::result::Result<Answer, LlmError> {
    let request = CompletionRequest::new(
        system_prompt(language),
        user_prompt(question, passages, asker),
    )
    .with_schema(output_schema());
    let raw = provider.complete(&request).await?;

    let info = provider.info();
    let parsed = kuali_llm::json::extract_json_object(&raw).ok_or_else(|| LlmError::BadJson {
        provider: info.label.clone(),
        message: "no object in the response".into(),
    })?;
    let parsed: RawAnswer = serde_json::from_str(parsed).map_err(|error| LlmError::BadJson {
        provider: info.label.clone(),
        message: error.to_string(),
    })?;

    let text = parsed.answer.trim().to_string();
    if text.is_empty() {
        return Err(LlmError::BadJson {
            provider: info.label,
            message: "the answer was empty".into(),
        });
    }
    Ok(Answer::Answered {
        text,
        citations: resolve_citations(&parsed.citations, passages),
    })
}

/// Maps the numbers the model returned back onto real meetings.
///
/// Anything out of range is dropped rather than repaired. A number that does not
/// resolve is the model referring to evidence it was never given, and inventing
/// a meeting for it would defeat the point of citing at all.
fn resolve_citations(numbers: &[usize], passages: &[Passage]) -> Vec<Citation> {
    let mut citations: Vec<Citation> = Vec::new();
    for number in numbers {
        let Some(passage) = number.checked_sub(1).and_then(|index| passages.get(index)) else {
            continue;
        };
        // One line per meeting: three excerpts from the same call are one place
        // to go and read, not three.
        if citations
            .iter()
            .any(|citation| citation.meeting_id == passage.meeting_id)
        {
            continue;
        }
        citations.push(Citation {
            meeting_id: passage.meeting_id.clone(),
            meeting_title: passage.meeting_title.clone(),
            channel_name: passage.channel_name.clone(),
            started_at: passage.started_at,
            start_ms: passage.start_ms,
        });
    }
    citations.sort_by(|a, b| b.started_at.cmp(&a.started_at));
    citations
}

fn output_schema() -> serde_json::Value {
    serde_json::json!({
        "type": "object",
        "properties": {
            "answer": { "type": "string" },
            "citations": { "type": "array", "items": { "type": "integer" } }
        },
        "required": ["answer", "citations"],
        "additionalProperties": false
    })
}

/// Written in English because that is where every provider is strongest, while
/// the language of the answer is decided separately, exactly as the summarizer
/// does it.
pub fn system_prompt(language: &str) -> String {
    let language_rule = kuali_llm::language_rule(language);
    format!(
        r#"You are Kuali's meeting memory. Someone is asking about meetings they took part in, and you answer from numbered excerpts of those meetings.

The excerpts are everything you have. They were selected for this person and this question, and there is nothing else to consult. Do not reason about what other meetings might have said.

- Answer only from the excerpts. If they do not contain the answer, say so plainly and stop. Being wrong about what a team decided is far worse than admitting the meetings do not cover it.
- Every claim has to trace back to an excerpt you cite. Put those numbers in "citations" and never in "answer".
- Write as someone who remembers the meetings. Do not mention excerpts, numbers, retrieval, or that you were given material.
- The excerpts carry the meeting, the date and who was speaking. Use them: "on 3 August, in the planning call, Ana said the rollout was postponed" is worth far more than "it was postponed".
- The text comes from automatic speech recognition, so proper nouns and technical terms arrive mangled. Read with common sense instead of repeating an obvious mistranscription.
- When excerpts disagree, the more recent meeting is what stands. Say that the position changed rather than picking one silently.
- A distilled line — a decision, a task, a note — states a conclusion. A transcript excerpt is what people actually said. When they conflict, the transcript is the evidence.
- When you are told who is asking, resolve their "I", "me" and "my" to that person, and address them directly. Without that line you do not know which participant they are, so answer about the meeting rather than guessing, and never assume the asker is whoever speaks most.
- Be brief. Two or three sentences of prose when that answers it.

Format the answer as Markdown, using only what genuinely helps it be read:

- **Bold** for the thing the reader came for: a decision, an owner, a deadline.
- A bullet list when the answer really is several items — pending tasks, separate decisions. Never a list of one, and never a list where a sentence would do.
- `Backticks` for literal names of files, commands, versions, ports and identifiers, which arrive mangled from speech recognition and are easier to read set apart.
- Blank lines between paragraphs.

Do not use headings, tables, images, links or horizontal rules. This is a short answer inside a chat, not a document.

{language_rule}

Return a JSON object only, with no text around it and without wrapping it in a code block:

{{
  "answer": "what the meetings say, as Markdown, answering the question that was asked",
  "citations": [1, 3]
}}"#
    )
}

pub fn user_prompt(question: &str, passages: &[Passage], asker: &Asker) -> String {
    let mut prompt = String::new();
    if let Some(identity) = asker.describe() {
        prompt.push_str(&identity);
        prompt.push_str("\n\n");
    }
    prompt.push_str(&format!(
        "Question: {}\n\n--- EXCERPTS ---\n",
        question.trim()
    ));
    for (index, passage) in passages.iter().enumerate() {
        prompt.push_str(&format!(
            "\n[{}] {} · #{} · {}{}{}\n{}\n",
            index + 1,
            passage.meeting_title,
            passage.channel_name,
            passage.started_at.format("%Y-%m-%d"),
            passage
                .start_ms
                .map(|ms| format!(" · {}", kuali_core::format_timestamp(ms)))
                .unwrap_or_default(),
            match passage.speakers.is_empty() {
                true => String::new(),
                false => format!(" · {}", passage.speakers),
            },
            passage.text.trim(),
        ));
    }
    prompt
}

#[derive(Debug, Deserialize)]
struct RawAnswer {
    #[serde(default, alias = "response", alias = "text")]
    answer: String,
    #[serde(default, alias = "sources", alias = "excerpts")]
    citations: Vec<usize>,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::chunk::ChunkKind;
    use chrono::{TimeZone, Utc};

    fn passage(meeting_id: &str, text: &str) -> Passage {
        Passage {
            meeting_id: meeting_id.into(),
            meeting_title: format!("Reunión {meeting_id}"),
            channel_name: "General".into(),
            started_at: Utc.with_ymd_and_hms(2026, 8, 3, 10, 0, 0).unwrap(),
            kind: ChunkKind::Transcript,
            start_ms: Some(750_000),
            speakers: "Ana".into(),
            text: text.into(),
            score: 1.0,
        }
    }

    #[test]
    fn a_citation_the_model_invented_is_dropped_instead_of_resolved() {
        let passages = vec![passage("m1", "uno"), passage("m2", "dos")];

        let citations = resolve_citations(&[1, 99, 0], &passages);

        assert_eq!(citations.len(), 1);
        assert_eq!(citations[0].meeting_id, "m1");
    }

    #[test]
    fn several_excerpts_from_one_meeting_become_one_citation() {
        let passages = vec![
            passage("m1", "uno"),
            passage("m1", "otro"),
            passage("m2", "dos"),
        ];

        let citations = resolve_citations(&[1, 2, 3], &passages);

        assert_eq!(citations.len(), 2);
        assert_eq!(citations[0].meeting_id, "m1");
        assert_eq!(citations[1].meeting_id, "m2");
    }

    #[test]
    fn the_prompt_gives_every_excerpt_a_number_a_meeting_and_a_moment() {
        let prompt = user_prompt(
            "¿qué decidimos?",
            &[passage("m1", "hablamos de kafka")],
            &Asker::unknown(),
        );

        assert!(prompt.contains("Question: ¿qué decidimos?"));
        assert!(prompt.contains("[1] Reunión m1 · #General · 2026-08-03 · 12:30 · Ana"));
        assert!(prompt.contains("hablamos de kafka"));
    }

    #[test]
    fn an_oversized_passage_is_cut_down_rather_than_dropped() {
        let huge = "x".repeat(MAX_EVIDENCE_CHARS * 2);
        let kept = within_budget(vec![passage("m1", &huge), passage("m2", "corto")]);

        assert_eq!(kept.len(), 2, "trimming the first left room for the second");
        assert!(kept[0].text.chars().count() <= MAX_PASSAGE_CHARS + 1);
        assert!(kept[0].text.ends_with('…'));
        assert_eq!(kept[1].text, "corto");
    }

    #[test]
    fn evidence_stops_at_the_budget_keeping_the_strongest_passages() {
        let long = "palabra ".repeat(MAX_PASSAGE_CHARS / 8);
        let many: Vec<Passage> = (0..40)
            .map(|index| passage(&format!("m{index}"), &long))
            .collect();

        let kept = within_budget(many);

        assert!(!kept.is_empty());
        assert!(kept.len() < 40, "the budget has to cut something");
        let total: usize = kept.iter().map(|p| p.text.chars().count()).sum();
        assert!(total <= MAX_EVIDENCE_CHARS + MAX_PASSAGE_CHARS);
        // Retrieval ranked these, so the cut keeps the head of the list.
        assert_eq!(kept[0].meeting_id, "m0");
    }

    #[test]
    fn the_answer_language_follows_the_configured_setting() {
        assert!(system_prompt("es").contains("in es"));
        assert!(system_prompt("auto").contains("the language the meeting was held in"));
    }
}

#[cfg(test)]
mod identity_tests {
    use super::*;

    #[test]
    fn a_verified_name_is_stated_plainly() {
        let asker = Asker::named(vec!["Juan Sebastián".into()], true);
        let described = asker.describe().unwrap();

        assert!(described.contains("Juan Sebastián"));
        assert!(described.contains("confirmed by the platform"));
    }

    #[test]
    fn an_unverified_name_is_marked_as_a_claim_the_model_should_check() {
        let asker = Asker::named(vec!["Ana".into()], false);
        let described = asker.describe().unwrap();

        assert!(described.contains("says they are Ana"));
        // A guessed identity that goes unchallenged produces confident answers
        // about the wrong person, so the model is told to hedge instead.
        assert!(described.contains("not something verified"));
    }

    #[test]
    fn alternative_names_are_offered_because_platforms_disagree() {
        let asker = Asker::named(vec!["Juan Sebastián".into(), "juansebas".into()], true);
        assert!(asker
            .describe()
            .unwrap()
            .contains("also appear as juansebas"));
    }

    #[test]
    fn an_unknown_asker_says_nothing_rather_than_guessing() {
        assert_eq!(Asker::unknown().describe(), None);
        assert_eq!(Asker::named(vec!["  ".into()], true).describe(), None);
    }

    #[test]
    fn the_prompt_carries_the_identity_before_the_question() {
        use crate::chunk::ChunkKind;
        use chrono::{TimeZone, Utc};

        let passage = Passage {
            meeting_id: "m1".into(),
            meeting_title: "Reunión".into(),
            channel_name: "General".into(),
            started_at: Utc.with_ymd_and_hms(2026, 8, 3, 10, 0, 0).unwrap(),
            kind: ChunkKind::Transcript,
            start_ms: Some(0),
            speakers: "Ana".into(),
            text: "hola".into(),
            score: 1.0,
        };

        let named = user_prompt(
            "¿qué me toca?",
            std::slice::from_ref(&passage),
            &Asker::named(vec!["Ana".into()], true),
        );
        assert!(named.find("Ana").unwrap() < named.find("Question:").unwrap());

        // Without an identity the prompt starts at the question, so the model is
        // never handed a blank or invented "you are …" line.
        let anonymous = user_prompt("¿qué me toca?", &[passage], &Asker::unknown());
        assert!(anonymous.starts_with("Question:"));
    }

    #[test]
    fn the_answer_is_requested_as_markdown_without_document_furniture() {
        let prompt = system_prompt("es");
        assert!(prompt.contains("Markdown"));
        assert!(prompt.contains("**Bold**"));
        // It is a chat reply, not a document.
        assert!(prompt.contains("Do not use headings, tables, images, links"));
    }
}
