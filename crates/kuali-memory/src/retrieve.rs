//! Finding the passages that answer a question.
//!
//! Retrieval runs in two moves. A lexical pass ranks passages the question
//! actually shares words with, which is what most questions need. Then the
//! deterministic graph reaches one hop further: meetings that share a channel,
//! a tag, a folder, or a person with the strongest hits contribute their own
//! passages at a reduced score, so a follow-up call that never repeats the
//! original wording is still reachable.
//!
//! Both moves select from the same `visible` CTE. A meeting outside the
//! audience is not ranked lower — it does not exist.

use chrono::{DateTime, Utc};
use rusqlite::types::Value;
use rusqlite::{params_from_iter, Row};

use crate::chunk::ChunkKind;
use crate::embed::Embedder;
use crate::{Audience, Memory, Result};

/// One piece of evidence, carrying enough context to be cited back to the
/// reader: which meeting, which moment, and who was speaking.
#[derive(Debug, Clone, PartialEq)]
pub struct Passage {
    pub meeting_id: String,
    pub meeting_title: String,
    pub channel_name: String,
    pub started_at: DateTime<Utc>,
    pub kind: ChunkKind,
    pub start_ms: Option<u64>,
    pub speakers: String,
    pub text: String,
    pub score: f32,
}

/// How many passages the lexical pass considers before the graph and the final
/// cut have their say.
const LEXICAL_DEPTH: usize = 60;

/// Meetings whose neighbours are worth exploring. Only the strongest hits earn
/// expansion, otherwise one weak match drags in half the library.
const SEED_MEETINGS: usize = 3;

/// Neighbouring meetings admitted per search.
const NEIGHBOUR_MEETINGS: usize = 4;

/// Passages taken from each neighbouring meeting.
const NEIGHBOUR_PASSAGES: usize = 3;

/// A neighbour is a lead, not an answer: its passages have to beat a direct hit
/// by a wide margin to outrank one.
const NEIGHBOUR_DECAY: f32 = 0.55;

/// Score given to a neighbour's summary when it matched no words at all, as a
/// fraction of the best direct hit. It is low enough to sit below real matches
/// and high enough to survive the final cut when direct hits are scarce.
const UNMATCHED_NEIGHBOUR_SHARE: f32 = 0.25;

impl Memory {
    /// Passages this audience may read, ranked by how well they answer the
    /// question.
    ///
    /// With a loaded [`Embedder`], meaning and wording each produce a ranking
    /// and the two are fused. Without one, only wording is used — that path
    /// exists for tests and for indexing decisions, not for answering, which
    /// Kuali gates on the model being installed.
    ///
    /// An empty result is a real answer: it means nothing they were part of
    /// discussed this. It is never a permission error, and the caller should not
    /// present it as one.
    pub fn retrieve(
        &self,
        audience: &Audience,
        question: &str,
        limit: usize,
    ) -> Result<Vec<Passage>> {
        self.retrieve_with(audience, question, limit, None)
    }

    pub fn retrieve_with(
        &self,
        audience: &Audience,
        question: &str,
        limit: usize,
        embedder: Option<&mut Embedder>,
    ) -> Result<Vec<Passage>> {
        let lexical = match fts_query(question) {
            Some(query) => self.lexical(audience, &query, LEXICAL_DEPTH)?,
            None => Vec::new(),
        };
        let semantic = match embedder {
            Some(embedder) => {
                let vector = embedder.embed_query(question)?;
                self.semantic(audience, &vector, LEXICAL_DEPTH)?
            }
            None => Vec::new(),
        };

        let mut found = fuse(lexical, semantic);

        // Neighbour expansion still runs on wording, because a link is about
        // which meetings are related, not about how the question was phrased.
        if let Some(query) = fts_query(question) {
            let seeds = seed_meetings(&found);
            if !seeds.is_empty() {
                let best = found.first().map(|passage| passage.score).unwrap_or(1.0);
                found.extend(self.through_neighbours(audience, &query, &seeds, best)?);
            }
        }

        found.sort_by(|a, b| {
            b.score
                .partial_cmp(&a.score)
                .unwrap_or(std::cmp::Ordering::Equal)
                // Two passages that answer equally well are separated by
                // recency: the later meeting is the one that still holds.
                .then_with(|| b.started_at.cmp(&a.started_at))
        });
        found.dedup_by(|a, b| a.meeting_id == b.meeting_id && a.text == b.text);
        found.truncate(limit);
        Ok(found)
    }

    /// Ranks passages by meaning rather than wording.
    ///
    /// The scan is exhaustive over what the audience may read. That is the right
    /// shape here: a person's meetings are thousands of passages, not millions,
    /// and comparing a few thousand normalized vectors is a handful of
    /// milliseconds. An approximate index would add a structure to keep in sync
    /// for no gain at this size.
    fn semantic(&self, audience: &Audience, query: &[f32], limit: usize) -> Result<Vec<Passage>> {
        let (visible, mut params) = audience.visible_meetings();
        let sql = format!(
            "WITH visible AS ({visible})
             SELECT c.meeting_id, m.title, m.channel_name, m.started_at,
                    c.kind, c.start_ms, c.speakers, c.text, 0.0 AS rank, e.vector
             FROM embedding e
             JOIN chunk c ON c.id = e.chunk_id
             JOIN meeting m ON m.id = c.meeting_id
             WHERE e.model = ?
               AND c.meeting_id IN (SELECT id FROM visible)"
        );
        params.push(Value::Text(crate::embed::MODEL_ID.to_string()));

        let mut statement = self.conn.prepare(&sql)?;
        let mut scored = statement
            .query_map(params_from_iter(params), |row| {
                let mut passage = read_passage(row, 1.0)?;
                let stored: Vec<u8> = row.get(9)?;
                passage.score = crate::embed::from_bytes(&stored)
                    .map(|vector| crate::embed::similarity(query, &vector))
                    // A vector of the wrong width belongs to another model and
                    // is skipped rather than compared.
                    .unwrap_or(f32::NEG_INFINITY);
                Ok(passage)
            })?
            .collect::<std::result::Result<Vec<_>, _>>()?;

        scored.retain(|passage| passage.score.is_finite());
        scored.sort_by(|a, b| {
            b.score
                .partial_cmp(&a.score)
                .unwrap_or(std::cmp::Ordering::Equal)
        });
        scored.truncate(limit);
        Ok(scored)
    }

    /// Ranks passages the question shares words with.
    fn lexical(&self, audience: &Audience, query: &str, limit: usize) -> Result<Vec<Passage>> {
        let (visible, mut params) = audience.visible_meetings();
        let sql = format!(
            "WITH visible AS ({visible})
             SELECT c.meeting_id, m.title, m.channel_name, m.started_at,
                    c.kind, c.start_ms, c.speakers, c.text, bm25(chunk_fts) AS rank
             FROM chunk_fts
             JOIN chunk c ON c.id = chunk_fts.rowid
             JOIN meeting m ON m.id = c.meeting_id
             WHERE chunk_fts MATCH ?
               AND c.meeting_id IN (SELECT id FROM visible)
             ORDER BY rank
             LIMIT ?"
        );
        params.push(Value::Text(query.to_string()));
        params.push(Value::Integer(limit as i64));

        let mut statement = self.conn.prepare(&sql)?;
        let passages = statement
            .query_map(params_from_iter(params), |row| read_passage(row, 1.0))?
            .collect::<std::result::Result<Vec<_>, _>>()?;
        Ok(passages)
    }

    /// Pulls passages from meetings connected to the strongest hits.
    fn through_neighbours(
        &self,
        audience: &Audience,
        query: &str,
        seeds: &[String],
        best_score: f32,
    ) -> Result<Vec<Passage>> {
        let neighbours = self.neighbours(audience, seeds)?;
        let mut found = Vec::new();
        for meeting_id in neighbours {
            let mut passages = self.matching_in(&meeting_id, query, NEIGHBOUR_PASSAGES)?;
            if passages.is_empty() {
                // Connected but sharing no wording: something has to say what
                // this meeting was, or the connection is invisible to the reader.
                passages = self.context_of(&meeting_id, best_score * UNMATCHED_NEIGHBOUR_SHARE)?;
            } else {
                for passage in &mut passages {
                    passage.score *= NEIGHBOUR_DECAY;
                }
            }
            found.extend(passages);
        }
        Ok(found)
    }

    /// Meetings sharing a channel, tag, folder, or person with a seed.
    ///
    /// The audience gate is applied here as well as on the seeds. A link is a
    /// suggestion about relevance and carries no authority: reaching a meeting
    /// through one still requires being allowed to see it.
    fn neighbours(&self, audience: &Audience, seeds: &[String]) -> Result<Vec<String>> {
        let (visible, mut params) = audience.visible_meetings();
        let seed_slots = vec!["?"; seeds.len()].join(", ");
        let sql = format!(
            "WITH visible AS ({visible})
             SELECT other.meeting_id, COUNT(*) AS shared
             FROM link seed
             JOIN link other
               ON other.kind = seed.kind
              AND other.key = seed.key
              AND other.meeting_id <> seed.meeting_id
             WHERE seed.meeting_id IN ({seed_slots})
               AND other.meeting_id NOT IN ({seed_slots})
               AND other.meeting_id IN (SELECT id FROM visible)
             GROUP BY other.meeting_id
             ORDER BY shared DESC
             LIMIT ?"
        );
        // The seed list is bound twice: once to find the edges, once to exclude
        // the seeds themselves from the results.
        for _ in 0..2 {
            params.extend(seeds.iter().map(|id| Value::Text(id.clone())));
        }
        params.push(Value::Integer(NEIGHBOUR_MEETINGS as i64));

        let mut statement = self.conn.prepare(&sql)?;
        let ids = statement
            .query_map(params_from_iter(params), |row| row.get::<_, String>(0))?
            .collect::<std::result::Result<Vec<_>, _>>()?;
        Ok(ids)
    }

    /// Best-matching passages inside one already-authorized meeting.
    fn matching_in(&self, meeting_id: &str, query: &str, limit: usize) -> Result<Vec<Passage>> {
        let mut statement = self.conn.prepare(
            "SELECT c.meeting_id, m.title, m.channel_name, m.started_at,
                    c.kind, c.start_ms, c.speakers, c.text, bm25(chunk_fts) AS rank
             FROM chunk_fts
             JOIN chunk c ON c.id = chunk_fts.rowid
             JOIN meeting m ON m.id = c.meeting_id
             WHERE chunk_fts MATCH ? AND c.meeting_id = ?
             ORDER BY rank
             LIMIT ?",
        )?;
        let passages = statement
            .query_map(
                params_from_iter([
                    Value::Text(query.to_string()),
                    Value::Text(meeting_id.to_string()),
                    Value::Integer(limit as i64),
                ]),
                |row| read_passage(row, 1.0),
            )?
            .collect::<std::result::Result<Vec<_>, _>>()?;
        Ok(passages)
    }

    /// Says what one already-authorized meeting was about, for a neighbour that
    /// shares no wording with the question.
    ///
    /// The distilled lines are the best answer, but a meeting summarized before
    /// an LLM was configured — or one still waiting for its summary — has none.
    /// Falling back to how the meeting opened keeps that meeting reachable
    /// instead of silently dropping it from the graph.
    fn context_of(&self, meeting_id: &str, score: f32) -> Result<Vec<Passage>> {
        let mut passages = self.context_query(
            "AND c.kind IN ('overview', 'decision', 'key_point')
             ORDER BY CASE c.kind
                          WHEN 'overview' THEN 0
                          WHEN 'decision' THEN 1
                          ELSE 2
                      END",
            meeting_id,
            NEIGHBOUR_PASSAGES,
        )?;
        if passages.is_empty() {
            passages = self.context_query(
                "AND c.kind = 'transcript' ORDER BY c.start_ms",
                meeting_id,
                1,
            )?;
        }
        for passage in &mut passages {
            passage.score = score;
        }
        Ok(passages)
    }

    fn context_query(&self, tail: &str, meeting_id: &str, limit: usize) -> Result<Vec<Passage>> {
        let sql = format!(
            "SELECT c.meeting_id, m.title, m.channel_name, m.started_at,
                    c.kind, c.start_ms, c.speakers, c.text, 0.0 AS rank
             FROM chunk c
             JOIN meeting m ON m.id = c.meeting_id
             WHERE c.meeting_id = ? {tail}
             LIMIT ?"
        );
        let mut statement = self.conn.prepare(&sql)?;
        let passages = statement
            .query_map(
                params_from_iter([
                    Value::Text(meeting_id.to_string()),
                    Value::Integer(limit as i64),
                ]),
                |row| read_passage(row, 1.0),
            )?
            .collect::<std::result::Result<Vec<_>, _>>()?;
        Ok(passages)
    }
}

fn read_passage(row: &Row<'_>, multiplier: f32) -> rusqlite::Result<Passage> {
    let kind = ChunkKind::from_label(&row.get::<_, String>(4)?);
    // FTS5 reports bm25 as a negative number where more negative is a better
    // match. Negating turns it into a score that sorts the natural way.
    let rank: f64 = row.get(8)?;
    let started_at: String = row.get(3)?;

    Ok(Passage {
        meeting_id: row.get(0)?,
        meeting_title: row.get(1)?,
        channel_name: row.get(2)?,
        started_at: DateTime::parse_from_rfc3339(&started_at)
            .map(|value| value.with_timezone(&Utc))
            .unwrap_or_else(|_| Utc::now()),
        kind,
        start_ms: row.get::<_, Option<i64>>(5)?.map(|ms| ms as u64),
        speakers: row.get(6)?,
        text: row.get(7)?,
        score: (-rank as f32) * kind.weight() * multiplier,
    })
}

/// Damping constant for rank fusion.
///
/// It flattens the top of each ranking so first place is worth a little more
/// than second rather than several times more, which is what keeps one confident
/// ranker from overruling the other outright. Sixty is the value the original
/// reciprocal-rank-fusion work settled on and it is not sensitive.
const FUSION_DAMPING: f32 = 60.0;

/// Merges the wording ranking and the meaning ranking.
///
/// Their scores cannot be added: BM25 is an unbounded negative number while
/// cosines from this model all crowd between roughly 0.7 and 0.9. Comparing
/// *positions* instead of values sidesteps that entirely, and a passage that
/// both rankers liked ends up above one that only a single ranker loved.
fn fuse(lexical: Vec<Passage>, semantic: Vec<Passage>) -> Vec<Passage> {
    if semantic.is_empty() {
        return lexical;
    }
    if lexical.is_empty() {
        return semantic;
    }

    let mut fused: Vec<Passage> = Vec::new();
    let mut scores: Vec<f32> = Vec::new();

    for ranking in [lexical, semantic] {
        for (position, passage) in ranking.into_iter().enumerate() {
            let contribution = 1.0 / (FUSION_DAMPING + position as f32 + 1.0);
            match fused.iter().position(|kept: &Passage| {
                kept.meeting_id == passage.meeting_id && kept.text == passage.text
            }) {
                Some(index) => scores[index] += contribution,
                None => {
                    fused.push(passage);
                    scores.push(contribution);
                }
            }
        }
    }

    for (passage, score) in fused.iter_mut().zip(&scores) {
        // Kind weighting still applies: a decision line and a transcript window
        // that rank equally are not equally useful as an answer.
        passage.score = score * passage.kind.weight();
    }
    fused
}

/// The meetings whose neighbours are worth exploring, strongest first.
fn seed_meetings(found: &[Passage]) -> Vec<String> {
    let mut seeds: Vec<String> = Vec::new();
    for passage in found {
        if !seeds.contains(&passage.meeting_id) {
            seeds.push(passage.meeting_id.clone());
        }
        if seeds.len() == SEED_MEETINGS {
            break;
        }
    }
    seeds
}

/// Words too common to say anything about relevance. BM25 already discounts
/// them, but dropping them keeps a long question from drowning its own subject
/// in filler.
const STOPWORDS: &[&str] = &[
    // Spanish
    "que", "qué", "por", "para", "con", "los", "las", "del", "una", "uno", "pero", "como", "cómo",
    "cuando", "cuándo", "donde", "dónde", "sobre", "este", "esta", "esto", "eso", "ese", "esa",
    "esos", "esas", "hay", "han", "fue", "ser", "son", "the", "sus", "más", "mas", "muy", "nos",
    "les", "algo", "todo", "toda", "dijo", "dice", "hizo", "hace", "puede", "sido", "está", "esta",
    "estan", "están", // English
    "what", "when", "where", "which", "who", "whom", "was", "were", "did", "does", "have", "has",
    "had", "the", "and", "for", "with", "from", "about", "this", "that", "these", "those", "there",
    "they", "them", "his", "her", "its", "our", "your", "are", "not", "any", "all", "can", "could",
    "would", "should", "said", "say", "says", "into", "over", "than", "then",
];

/// Builds the FTS5 query. Every term is reduced to letters and digits and then
/// quoted, so a question containing `AND`, `*`, or a stray quote is searched for
/// rather than interpreted as query syntax.
fn fts_query(question: &str) -> Option<String> {
    let mut terms: Vec<String> = Vec::new();
    for word in question.split(|character: char| !character.is_alphanumeric()) {
        let term = word.to_lowercase();
        if term.chars().count() < 3 || STOPWORDS.contains(&term.as_str()) {
            continue;
        }
        if !terms.contains(&term) {
            terms.push(term);
        }
        // A question longer than this is a paragraph, and every extra term
        // pulls the ranking further from what was actually asked.
        if terms.len() == 24 {
            break;
        }
    }
    if terms.is_empty() {
        return None;
    }
    Some(
        terms
            .iter()
            .map(|term| format!("\"{term}\""))
            .collect::<Vec<_>>()
            .join(" OR "),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_question_becomes_a_quoted_or_query_without_its_filler() {
        let query = fts_query("¿Qué decidimos sobre el despliegue de Kafka?").unwrap();
        assert_eq!(query, "\"decidimos\" OR \"despliegue\" OR \"kafka\"");
    }

    #[test]
    fn fts_syntax_in_a_question_is_searched_for_instead_of_executed() {
        let query = fts_query("estado del proyecto AND \"algo\" OR * NEAR(x)").unwrap();
        // Bare operators survive only as quoted words, never as syntax.
        assert!(query.contains("\"estado\""));
        assert!(query.contains("\"proyecto\""));
        assert!(!query.contains('*'));
        assert!(!query.contains("NEAR"));
        for fragment in query.split(" OR ") {
            assert!(fragment.starts_with('"') && fragment.ends_with('"'));
        }
    }

    #[test]
    fn a_question_with_nothing_searchable_asks_for_nothing() {
        assert_eq!(fts_query("¿y eso?"), None);
        assert_eq!(fts_query("   "), None);
        assert_eq!(fts_query("what was that"), None);
    }

    #[test]
    fn repeated_words_are_asked_for_once() {
        let query = fts_query("kafka kafka KAFKA despliegue").unwrap();
        assert_eq!(query, "\"kafka\" OR \"despliegue\"");
    }
}
