//! Writing meetings into the index.
//!
//! The index is derived, never authoritative: `meetings/` on disk remains the
//! source of truth, and everything here can be rebuilt from it. That is what
//! makes [`Memory::sync_from_store`] safe to run at startup and what lets a
//! corrupt index be deleted rather than repaired.

use std::collections::HashSet;

use kuali_core::{is_browser_identifier, Meeting, MeetingMeta};
use rusqlite::{params, OptionalExtension, Transaction};

use crate::chunk;
use crate::{normalize_key, Memory, Result};

/// Version of the derived textual representation stored in `content_hash`.
///
/// Bump this whenever chunk construction or indexed meeting metadata changes in
/// a way that serialization alone cannot detect. The version prefix makes the
/// next store synchronization rebuild every legacy row exactly once; rows
/// written by this version remain idempotent.
const INDEX_FORMAT_VERSION: u32 = 2;

/// What a full synchronization did, so the caller can log something meaningful
/// instead of "done".
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SyncReport {
    pub indexed: usize,
    /// IDs whose textual rows were rewritten during this synchronization.
    pub indexed_meeting_ids: Vec<String>,
    pub unchanged: usize,
    pub removed: usize,
    /// Meetings the library could not open. They are skipped rather than
    /// failing the sync, matching how the library itself survives one corrupt
    /// meeting.
    pub unreadable: usize,
}

/// Derived storage counts for one meeting.
///
/// `None` from [`Memory::meeting_index_stats`] means the meeting has no row in
/// the index at all. A present value with zero passages is still indexed: an
/// empty meeting legitimately has nothing to chunk.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct MeetingIndexStats {
    pub passages: usize,
    pub pending_passages: usize,
}

impl Memory {
    /// Indexes a meeting, replacing whatever was stored for it.
    ///
    /// Returns `false` when the stored fingerprint already matches, which is the
    /// common case while a meeting is being saved on every utterance.
    pub fn index(&mut self, meeting: &Meeting) -> Result<bool> {
        let hash = fingerprint(meeting);
        let known: Option<String> = self
            .conn
            .query_row(
                "SELECT content_hash FROM meeting WHERE id = ?",
                params![meeting.meta.id],
                |row| row.get(0),
            )
            .ok();
        if known.as_deref() == Some(hash.as_str()) {
            return Ok(false);
        }

        let drafts = chunk::chunks(meeting);
        let tx = self.conn.transaction()?;
        write_meeting(&tx, meeting, &hash, &drafts, None)?;
        tx.commit()?;
        Ok(true)
    }

    /// Rebuilds one meeting even when its stored fingerprint still matches.
    ///
    /// The rewrite is one transaction, including deletion of the previous
    /// passages. If any write fails, SQLite restores the previously healthy
    /// index instead of leaving a half-rebuilt meeting behind.
    pub fn force_index(&mut self, meeting: &Meeting) -> Result<()> {
        let hash = fingerprint(meeting);
        let drafts = chunk::chunks(meeting);
        let tx = self.conn.transaction()?;
        write_meeting(&tx, meeting, &hash, &drafts, None)?;
        tx.commit()?;
        Ok(())
    }

    /// Rebuilds a meeting and its vectors without exposing a half-finished
    /// replacement.
    ///
    /// Inference happens before the write transaction. If it fails for a row
    /// whose vectors are complete, the old healthy text and vectors remain
    /// byte-for-byte untouched. A missing or already-pending row receives the
    /// current textual passages so it stays visible as `pending` and can recover
    /// later.
    pub fn force_index_with_embeddings(
        &mut self,
        meeting: &Meeting,
        embedder: &mut crate::embed::Embedder,
    ) -> Result<()> {
        self.force_index_using(meeting, |texts| embedder.embed_passages(texts))
    }

    fn force_index_using(
        &mut self,
        meeting: &Meeting,
        embed: impl FnOnce(&[String]) -> Result<Vec<Vec<f32>>>,
    ) -> Result<()> {
        let drafts = chunk::chunks(meeting);
        let texts = drafts
            .iter()
            .map(|draft| draft.text.clone())
            .collect::<Vec<_>>();
        let existing_is_healthy = self
            .meeting_index_stats(&meeting.meta.id)?
            .is_some_and(|stats| stats.pending_passages == 0);
        let embedding = if texts.is_empty() {
            Ok(Vec::new())
        } else {
            embed(&texts).and_then(|vectors| {
                if vectors.len() == drafts.len() {
                    Ok(vectors)
                } else {
                    Err(crate::MemoryError::Embedding {
                        message: format!(
                            "el modelo devolvió {} vectores para {} pasajes",
                            vectors.len(),
                            drafts.len()
                        ),
                    })
                }
            })
        };
        let vectors = match embedding {
            Ok(vectors) => vectors,
            Err(error) if existing_is_healthy => return Err(error),
            Err(error) => {
                tracing::warn!(
                    meeting_id = %meeting.meta.id,
                    %error,
                    "guardé el índice textual; los embeddings siguen pendientes"
                );
                let hash = fingerprint(meeting);
                let tx = self.conn.transaction()?;
                write_meeting(&tx, meeting, &hash, &drafts, None)?;
                tx.commit()?;
                return Ok(());
            }
        };

        let hash = fingerprint(meeting);
        let tx = self.conn.transaction()?;
        write_meeting(&tx, meeting, &hash, &drafts, Some(&vectors))?;
        tx.commit()?;
        Ok(())
    }

    /// Indexes a meeting and embeds every passage still waiting for a vector.
    ///
    /// Without an embedder the passages are still written, just without vectors.
    /// They are then picked up by [`Memory::embed_pending`] whenever the model
    /// becomes available, which is what makes turning the feature on later a
    /// catch-up rather than a full rebuild. Backfilling all pending passages here
    /// also means the next successful meeting repairs one whose earlier model
    /// load or inference failed.
    pub fn index_with(
        &mut self,
        meeting: &Meeting,
        embedder: Option<&mut crate::embed::Embedder>,
    ) -> Result<bool> {
        let reindexed = self.index(meeting)?;
        if let Some(embedder) = embedder {
            // The meeting that just ended should become queryable before a
            // potentially large historical backlog. Once it is complete, the
            // same successful model session repairs older pending passages.
            self.embed_meeting_pending(&meeting.meta.id, embedder)?;
            self.embed_pending(embedder, |_, _| true)?;
        }
        Ok(reindexed)
    }

    /// How many passages still have no vector.
    ///
    /// This is the honest input to a time estimate: it is a count of real work,
    /// not a guess from the number of meetings.
    pub fn pending_embeddings(&self) -> Result<usize> {
        let count: i64 = self.conn.query_row(
            "SELECT COUNT(*) FROM chunk c
             LEFT JOIN embedding e ON e.chunk_id = c.id AND e.model = ?
             WHERE e.chunk_id IS NULL",
            params![crate::embed::MODEL_ID],
            |row| row.get(0),
        )?;
        Ok(count as usize)
    }

    /// Meetings containing at least one passage without a current vector.
    pub fn pending_embedding_meeting_ids(&self) -> Result<Vec<String>> {
        let mut statement = self.conn.prepare(
            "SELECT DISTINCT c.meeting_id FROM chunk c
             LEFT JOIN embedding e ON e.chunk_id = c.id AND e.model = ?
             WHERE e.chunk_id IS NULL
             ORDER BY c.meeting_id",
        )?;
        let ids = statement
            .query_map(params![crate::embed::MODEL_ID], |row| row.get(0))?
            .collect::<std::result::Result<Vec<_>, _>>()?;
        Ok(ids)
    }

    /// Passage and vector counts for one indexed meeting.
    ///
    /// Absence is distinct from an indexed meeting with no content, allowing
    /// the interface to say "not indexed" without persisting UI state in the
    /// authoritative `meeting.json`.
    pub fn meeting_index_stats(&self, meeting_id: &str) -> Result<Option<MeetingIndexStats>> {
        self.conn
            .query_row(
                "SELECT
                    (SELECT COUNT(*) FROM chunk c WHERE c.meeting_id = m.id),
                    (SELECT COUNT(*)
                       FROM chunk c
                       LEFT JOIN embedding e
                         ON e.chunk_id = c.id AND e.model = ?2
                      WHERE c.meeting_id = m.id AND e.chunk_id IS NULL)
                 FROM meeting m
                 WHERE m.id = ?1",
                params![meeting_id, crate::embed::MODEL_ID],
                |row| {
                    Ok(MeetingIndexStats {
                        passages: row.get::<_, i64>(0)? as usize,
                        pending_passages: row.get::<_, i64>(1)? as usize,
                    })
                },
            )
            .optional()
            .map_err(Into::into)
    }

    /// Whether one indexed row represents the current authoritative meeting.
    ///
    /// This is intentionally scoped to one ID so the meeting badge can detect a
    /// stale (including legacy-format) fingerprint without scanning the entire
    /// library. A meeting removed from the store is not current, even if a
    /// derived row still exists for it.
    pub fn meeting_store_is_current(&self, meeting_id: &str) -> Result<bool> {
        let meeting = match kuali_store::load(meeting_id) {
            Ok(meeting) => meeting,
            Err(kuali_store::StoreError::NotFound(_)) => return Ok(false),
            Err(error) => return Err(error.into()),
        };
        self.indexed_meeting_is_current(&meeting)
    }

    fn indexed_meeting_is_current(&self, meeting: &Meeting) -> Result<bool> {
        let known: Option<String> = self
            .conn
            .query_row(
                "SELECT content_hash FROM meeting WHERE id = ?",
                params![meeting.meta.id],
                |row| row.get(0),
            )
            .optional()?;
        Ok(known.as_deref() == Some(fingerprint(meeting).as_str()))
    }

    /// Whether every finished meeting in the authoritative library has at
    /// least one row in this derived index.
    ///
    /// Pending vectors are checked separately. This closes the more dangerous
    /// gap where a failed write leaves no chunks at all: counting pending chunks
    /// would report zero and incorrectly allow a partial-library answer.
    pub fn finished_store_is_covered(&self) -> Result<bool> {
        let indexed: HashSet<String> = {
            let mut statement = self.conn.prepare("SELECT id FROM meeting")?;
            let ids = statement
                .query_map([], |row| row.get(0))?
                .collect::<std::result::Result<_, _>>()?;
            ids
        };
        Ok(finished_ids_are_covered(&kuali_store::list()?, &indexed))
    }

    /// Whether every finished meeting's complete authoritative JSON matches
    /// the fingerprint stored in the derived index.
    ///
    /// This is intentionally heavier than [`Memory::finished_store_is_covered`]
    /// and is used only to recover a globally failed sync after explicit manual
    /// repairs, not on every question.
    pub fn finished_store_is_current(&self) -> Result<bool> {
        let metas = kuali_store::list()?;
        let store_ids: HashSet<&str> = metas.iter().map(|meta| meta.id.as_str()).collect();
        let indexed_ids: Vec<String> = {
            let mut statement = self.conn.prepare("SELECT id FROM meeting")?;
            let ids = statement
                .query_map([], |row| row.get(0))?
                .collect::<std::result::Result<_, _>>()?;
            ids
        };
        if indexed_ids
            .iter()
            .any(|meeting_id| !store_ids.contains(meeting_id.as_str()))
        {
            return Ok(false);
        }

        for meta in metas.into_iter().filter(|meta| meta.ended_at.is_some()) {
            let meeting = kuali_store::load(&meta.id)?;
            if !self.indexed_meeting_is_current(&meeting)? {
                return Ok(false);
            }
        }
        Ok(true)
    }

    /// Names the meeting platforms reported as belonging to this computer,
    /// most recently seen first.
    ///
    /// This is how "what did I commit to?" works in a Meet or Teams call: the
    /// page marks its own tile, because that tile owns the microphone being
    /// spoken into. Nothing has to be typed into Settings for it to work.
    pub fn known_self_names(&self) -> Result<Vec<String>> {
        let mut statement = self.conn.prepare(
            "SELECT self_name, MAX(started_at) AS seen
             FROM meeting
             WHERE self_name IS NOT NULL AND TRIM(self_name) <> ''
             GROUP BY self_name
             ORDER BY seen DESC",
        )?;
        let names: Vec<String> = statement
            .query_map([], |row| row.get::<_, String>(0))?
            .collect::<std::result::Result<Vec<_>, _>>()?;
        Ok(names)
    }

    /// Names one Discord account has appeared under across meetings, most
    /// recently first.
    ///
    /// The attendance table already records the display name each meeting saw,
    /// so the account Kuali is configured to follow resolves into the names the
    /// transcripts actually use — including a nickname that changed over time.
    pub fn names_for_speaker(&self, speaker_id: u64) -> Result<Vec<String>> {
        let mut statement = self.conn.prepare(
            "SELECT a.display_name, MAX(m.started_at) AS seen
             FROM attendance a
             JOIN meeting m ON m.id = a.meeting_id
             WHERE a.speaker_id = ? AND TRIM(a.display_name) <> ''
             GROUP BY a.display_name
             ORDER BY seen DESC",
        )?;
        let names: Vec<String> = statement
            .query_map(params![speaker_id.to_string()], |row| {
                row.get::<_, String>(0)
            })?
            .collect::<std::result::Result<Vec<_>, _>>()?;
        Ok(names)
    }

    /// How many passages already carry a vector.
    pub fn embedded_passages(&self) -> Result<usize> {
        let count: i64 = self.conn.query_row(
            "SELECT COUNT(*) FROM embedding WHERE model = ?",
            params![crate::embed::MODEL_ID],
            |row| row.get(0),
        )?;
        Ok(count as usize)
    }

    /// Embeds passages that have no vector yet, reporting progress as it goes.
    ///
    /// `progress` receives how many are done out of the original total. It
    /// returns `false` to stop, which is what makes the backfill cancellable
    /// without leaving the index inconsistent: whatever was embedded stays, and
    /// the rest is still pending.
    pub fn embed_pending(
        &mut self,
        embedder: &mut crate::embed::Embedder,
        mut progress: impl FnMut(usize, usize) -> bool,
    ) -> Result<usize> {
        let total = self.pending_embeddings()?;
        let mut done = 0usize;

        loop {
            let batch = self.pending_batch(EMBED_BATCH)?;
            if batch.is_empty() {
                break;
            }
            let texts: Vec<String> = batch.iter().map(|(_, text)| text.clone()).collect();
            let vectors = embedder.embed_passages(&texts)?;

            let tx = self.conn.transaction()?;
            for ((chunk_id, _), vector) in batch.iter().zip(&vectors) {
                store_vector(&tx, *chunk_id, vector)?;
            }
            tx.commit()?;

            done += batch.len();
            if !progress(done, total) {
                break;
            }
        }
        Ok(done)
    }

    /// Embeds only the passages of one meeting that still need a vector.
    ///
    /// Automatic completion uses this to prioritize the meeting that just
    /// ended before it starts repairing any historical backlog.
    pub fn embed_meeting_pending(
        &mut self,
        meeting_id: &str,
        embedder: &mut crate::embed::Embedder,
    ) -> Result<usize> {
        let mut done = 0usize;
        loop {
            let batch = self.pending_meeting_batch(meeting_id, EMBED_BATCH)?;
            if batch.is_empty() {
                break;
            }
            let texts: Vec<String> = batch.iter().map(|(_, text)| text.clone()).collect();
            let vectors = embedder.embed_passages(&texts)?;

            let tx = self.conn.transaction()?;
            for ((chunk_id, _), vector) in batch.iter().zip(&vectors) {
                store_vector(&tx, *chunk_id, vector)?;
            }
            tx.commit()?;
            done += batch.len();
        }
        Ok(done)
    }

    fn pending_batch(&self, limit: usize) -> Result<Vec<(i64, String)>> {
        let mut statement = self.conn.prepare(
            "SELECT c.id, c.text FROM chunk c
             LEFT JOIN embedding e ON e.chunk_id = c.id AND e.model = ?
             WHERE e.chunk_id IS NULL
             LIMIT ?",
        )?;
        let rows: Vec<(i64, String)> = statement
            .query_map(params![crate::embed::MODEL_ID, limit as i64], |row| {
                Ok((row.get(0)?, row.get(1)?))
            })?
            .collect::<std::result::Result<Vec<_>, _>>()?;
        Ok(rows)
    }

    fn pending_meeting_batch(&self, meeting_id: &str, limit: usize) -> Result<Vec<(i64, String)>> {
        let mut statement = self.conn.prepare(
            "SELECT c.id, c.text FROM chunk c
             LEFT JOIN embedding e ON e.chunk_id = c.id AND e.model = ?
             WHERE c.meeting_id = ? AND e.chunk_id IS NULL
             LIMIT ?",
        )?;
        let rows: Vec<(i64, String)> = statement
            .query_map(
                params![crate::embed::MODEL_ID, meeting_id, limit as i64],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )?
            .collect::<std::result::Result<Vec<_>, _>>()?;
        Ok(rows)
    }

    /// Drops every stored vector, used when the feature is turned off or the
    /// model changes. The passages themselves are untouched.
    pub fn forget_embeddings(&mut self) -> Result<()> {
        self.conn.execute("DELETE FROM embedding", [])?;
        Ok(())
    }

    /// Removes a meeting from the index. Used when it is deleted from the
    /// library, so an answer can never cite something the user threw away.
    pub fn forget(&mut self, meeting_id: &str) -> Result<()> {
        let tx = self.conn.transaction()?;
        clear_meeting(&tx, meeting_id)?;
        tx.execute("DELETE FROM meeting WHERE id = ?", params![meeting_id])?;
        tx.commit()?;
        Ok(())
    }

    /// Brings the index in line with the stored library: indexes what changed
    /// and drops what no longer exists.
    pub fn sync_from_store(&mut self) -> Result<SyncReport> {
        let mut report = SyncReport::default();
        let metas = kuali_store::list()?;

        let mut live: std::collections::HashSet<String> = std::collections::HashSet::new();
        for meta in &metas {
            live.insert(meta.id.clone());
            let meeting = match kuali_store::load(&meta.id) {
                Ok(meeting) => meeting,
                Err(error) => {
                    tracing::warn!(
                        meeting_id = %meta.id,
                        %error,
                        "no pude leer la reunión para indexarla; la omito"
                    );
                    report.unreadable += 1;
                    continue;
                }
            };
            match self.index(&meeting)? {
                true => {
                    report.indexed += 1;
                    report.indexed_meeting_ids.push(meta.id.clone());
                }
                false => report.unchanged += 1,
            }
        }

        let indexed: Vec<String> = {
            let mut statement = self.conn.prepare("SELECT id FROM meeting")?;
            let ids = statement
                .query_map([], |row| row.get::<_, String>(0))?
                .collect::<std::result::Result<Vec<_>, _>>()?;
            ids
        };
        for id in indexed {
            if !live.contains(&id) {
                self.forget(&id)?;
                report.removed += 1;
            }
        }

        Ok(report)
    }
}

fn finished_ids_are_covered(metas: &[MeetingMeta], indexed: &HashSet<String>) -> bool {
    metas
        .iter()
        .filter(|meta| meta.ended_at.is_some())
        .all(|meta| indexed.contains(&meta.id))
}

/// Passages embedded before the progress callback is consulted again.
///
/// Small enough that cancelling feels immediate and a crash loses almost
/// nothing, large enough that the transaction overhead stays negligible.
const EMBED_BATCH: usize = 32;

fn store_vector(tx: &Transaction<'_>, chunk_id: i64, vector: &[f32]) -> Result<()> {
    tx.execute(
        "INSERT OR REPLACE INTO embedding (chunk_id, model, vector) VALUES (?, ?, ?)",
        params![
            chunk_id,
            crate::embed::MODEL_ID,
            crate::embed::to_bytes(vector)
        ],
    )?;
    Ok(())
}

/// Deletes everything derived from a meeting.
///
/// Chunks go first and explicitly, rather than through `ON DELETE CASCADE`,
/// because SQLite only fires delete triggers for cascaded rows when recursive
/// triggers are enabled — and the FTS5 mirror depends on those triggers.
fn clear_meeting(tx: &Transaction<'_>, meeting_id: &str) -> Result<()> {
    tx.execute(
        "DELETE FROM chunk WHERE meeting_id = ?",
        params![meeting_id],
    )?;
    tx.execute(
        "DELETE FROM attendance WHERE meeting_id = ?",
        params![meeting_id],
    )?;
    tx.execute("DELETE FROM link WHERE meeting_id = ?", params![meeting_id])?;
    Ok(())
}

fn write_meeting(
    tx: &Transaction<'_>,
    meeting: &Meeting,
    hash: &str,
    drafts: &[crate::DraftChunk],
    vectors: Option<&[Vec<f32>]>,
) -> Result<()> {
    let meta = &meeting.meta;
    clear_meeting(tx, &meta.id)?;

    // A browser platform gets a synthesized guild identifier, so a tagged guild
    // marks the whole meeting as something no Discord account can claim.
    let discord_attributable = !is_browser_identifier(meta.guild_id);

    tx.execute(
        "INSERT INTO meeting (
             id, guild_id, guild_name, channel_id, channel_name,
             title, started_at, folder, discord_attributable, self_name, content_hash
         ) VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)
         ON CONFLICT(id) DO UPDATE SET
             guild_id = excluded.guild_id,
             guild_name = excluded.guild_name,
             channel_id = excluded.channel_id,
             channel_name = excluded.channel_name,
             title = excluded.title,
             started_at = excluded.started_at,
             folder = excluded.folder,
             discord_attributable = excluded.discord_attributable,
             self_name = excluded.self_name,
             content_hash = excluded.content_hash",
        params![
            meta.id,
            meta.guild_id.to_string(),
            meta.guild_name,
            meta.channel_id.to_string(),
            meta.channel_name,
            meta.title(),
            meta.started_at.to_rfc3339(),
            meta.folder,
            discord_attributable as i64,
            // Whoever the platform said was at this keyboard.
            meeting
                .speakers
                .iter()
                .find(|speaker| speaker.is_self && !speaker.is_bot)
                .map(|speaker| speaker.display_name.clone()),
            hash,
        ],
    )?;

    write_attendance(tx, meeting, discord_attributable)?;
    write_links(tx, meeting)?;
    write_chunks(tx, &meeting.meta.id, drafts, vectors)?;
    Ok(())
}

/// Records who may reach this meeting.
///
/// Three separate reasons keep a speaker out, and all of them fail closed:
/// a bot was never a person, a `source_id` means the identity came from a
/// browser platform, and a tagged identifier is one Kuali synthesized rather
/// than received from Discord.
fn write_attendance(
    tx: &Transaction<'_>,
    meeting: &Meeting,
    discord_attributable: bool,
) -> Result<()> {
    if !discord_attributable {
        return Ok(());
    }
    for speaker in &meeting.speakers {
        if speaker.is_bot || speaker.source_id.is_some() || is_browser_identifier(speaker.user_id) {
            continue;
        }
        tx.execute(
            "INSERT OR REPLACE INTO attendance (meeting_id, speaker_id, display_name)
             VALUES (?, ?, ?)",
            params![
                meeting.meta.id,
                speaker.user_id.to_string(),
                speaker.display_name
            ],
        )?;
    }
    Ok(())
}

/// Records the deterministic edges used to reach neighbouring meetings.
///
/// Nothing here is a permission: `person` deliberately includes people who were
/// only named as a task owner and may never have attended anything. Links widen
/// a search inside what the audience can already see; they never widen the
/// audience.
fn write_links(tx: &Transaction<'_>, meeting: &Meeting) -> Result<()> {
    let mut edges: Vec<(&str, String)> = Vec::new();
    edges.push(("channel", meeting.meta.channel_id.to_string()));
    for tag in &meeting.meta.tags {
        edges.push(("tag", normalize_key(tag)));
    }
    if let Some(folder) = &meeting.meta.folder {
        edges.push(("folder", normalize_key(folder)));
    }
    for speaker in meeting.speakers.iter().filter(|speaker| !speaker.is_bot) {
        edges.push(("person", normalize_key(&speaker.display_name)));
    }
    if let Some(summary) = &meeting.summary {
        for assignee in summary
            .action_items
            .iter()
            .filter_map(|task| task.assignee.as_deref())
        {
            edges.push(("person", normalize_key(assignee)));
        }
        for author in summary
            .notes
            .iter()
            .filter_map(|note| note.author.as_deref())
        {
            edges.push(("person", normalize_key(author)));
        }
    }

    for (kind, key) in edges {
        if key.is_empty() {
            continue;
        }
        tx.execute(
            "INSERT OR IGNORE INTO link (meeting_id, kind, key) VALUES (?, ?, ?)",
            params![meeting.meta.id, kind, key],
        )?;
    }
    Ok(())
}

fn write_chunks(
    tx: &Transaction<'_>,
    meeting_id: &str,
    drafts: &[crate::DraftChunk],
    vectors: Option<&[Vec<f32>]>,
) -> Result<()> {
    for (index, draft) in drafts.iter().enumerate() {
        tx.execute(
            "INSERT INTO chunk (meeting_id, kind, start_ms, end_ms, speakers, text)
             VALUES (?, ?, ?, ?, ?, ?)",
            params![
                meeting_id,
                draft.kind.as_str(),
                draft.start_ms.map(|ms| ms as i64),
                draft.end_ms.map(|ms| ms as i64),
                draft.speakers,
                draft.text,
            ],
        )?;
        if let Some(vectors) = vectors {
            store_vector(tx, tx.last_insert_rowid(), &vectors[index])?;
        }
    }
    Ok(())
}

/// Fingerprint that decides whether a meeting needs re-indexing.
///
/// It covers the serialized meeting because a corrected utterance changes the
/// text without changing any count, and the store rewrites the file on every
/// utterance while a call is live.
fn fingerprint(meeting: &Meeting) -> String {
    let bytes = serde_json::to_vec(meeting).unwrap_or_default();
    // FNV-1a over the serialized record. This guards against re-indexing work,
    // not against tampering, so a fast non-cryptographic hash is the right tool.
    let mut hash = 0xcbf2_9ce4_8422_2325u64;
    for byte in bytes {
        hash ^= u64::from(byte);
        hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
    }
    format!("v{INDEX_FORMAT_VERSION}:{hash:016x}")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Audience;
    use chrono::Utc;
    use kuali_core::{browser_identifier, MeetingMeta, Speaker};

    pub(crate) fn meeting(id: &str, guild_id: u64, speakers: &[(u64, &str)]) -> Meeting {
        let mut meeting = Meeting::new(MeetingMeta {
            id: id.into(),
            display_title: None,
            guild_id,
            guild_name: "Servidor".into(),
            channel_id: 99,
            channel_name: "General".into(),
            started_at: Utc::now(),
            ended_at: None,
            tags: Vec::new(),
            folder: None,
        });
        for (user_id, name) in speakers {
            meeting.upsert_speaker(Speaker {
                user_id: *user_id,
                source_id: None,
                audio_kind: None,
                display_name: (*name).into(),
                username: name.to_lowercase(),
                avatar_url: None,
                color: "#fff".into(),
                is_bot: false,
                is_self: false,
            });
        }
        meeting
    }

    fn attendance(memory: &Memory, meeting_id: &str) -> Vec<String> {
        let mut statement = memory
            .conn
            .prepare("SELECT speaker_id FROM attendance WHERE meeting_id = ? ORDER BY speaker_id")
            .unwrap();
        statement
            .query_map(params![meeting_id], |row| row.get::<_, String>(0))
            .unwrap()
            .map(|row| row.unwrap())
            .collect()
    }

    #[test]
    fn indexing_the_same_meeting_twice_does_no_work_the_second_time() {
        let mut memory = Memory::in_memory().unwrap();
        let meeting = meeting("m1", 1, &[(10, "Ana")]);

        assert!(memory.index(&meeting).unwrap());
        assert!(!memory.index(&meeting).unwrap());
    }

    #[test]
    fn a_legacy_fingerprint_is_rebuilt_once_then_remains_idempotent() {
        let mut memory = Memory::in_memory().unwrap();
        let meeting = meeting("m1", 1, &[(10, "Ana")]);

        assert!(memory.index(&meeting).unwrap());
        let current = fingerprint(&meeting);
        let legacy = current
            .split_once(':')
            .expect("the current fingerprint carries a format version")
            .1;
        memory
            .conn
            .execute(
                "UPDATE meeting SET content_hash = ? WHERE id = ?",
                params![legacy, meeting.meta.id],
            )
            .unwrap();

        assert!(
            memory.index(&meeting).unwrap(),
            "an unversioned row must be rebuilt even when the Meeting is unchanged"
        );
        let stored: String = memory
            .conn
            .query_row(
                "SELECT content_hash FROM meeting WHERE id = ?",
                params![meeting.meta.id],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(stored, current);
        assert!(!memory.index(&meeting).unwrap());
    }

    #[test]
    fn indexed_meeting_currency_detects_current_changed_and_legacy_rows() {
        let mut memory = Memory::in_memory().unwrap();
        let mut meeting = meeting("m1", 1, &[(10, "Ana")]);
        meeting.push_utterance(kuali_core::Utterance {
            id: "u1".into(),
            speaker_id: 10,
            start_ms: 0,
            end_ms: 1_000,
            text: "contenido indexado".into(),
            confidence: None,
        });
        memory.index(&meeting).unwrap();

        assert!(memory.indexed_meeting_is_current(&meeting).unwrap());

        let mut changed = meeting.clone();
        changed.utterances[0].text = "contenido actualizado en el store".into();
        assert!(!memory.indexed_meeting_is_current(&changed).unwrap());

        let current = fingerprint(&meeting);
        let legacy = current
            .split_once(':')
            .expect("the current fingerprint carries a format version")
            .1;
        memory
            .conn
            .execute(
                "UPDATE meeting SET content_hash = ? WHERE id = ?",
                params![legacy, meeting.meta.id],
            )
            .unwrap();
        assert!(!memory.indexed_meeting_is_current(&meeting).unwrap());
    }

    #[test]
    fn a_meeting_without_a_self_speaker_is_still_indexed() {
        let mut memory = Memory::in_memory().unwrap();
        let mut meeting = meeting("m1", 1, &[(10, "Ana")]);
        meeting.push_utterance(kuali_core::Utterance {
            id: "u1".into(),
            speaker_id: 10,
            start_ms: 0,
            end_ms: 1_000,
            text: "acordamos publicar el viernes".into(),
            confidence: None,
        });

        assert!(meeting.speakers.iter().all(|speaker| !speaker.is_self));
        assert!(memory.index(&meeting).unwrap());

        let self_name: Option<String> = memory
            .conn
            .query_row(
                "SELECT self_name FROM meeting WHERE id = ?",
                params![meeting.meta.id],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(self_name, None);
        let stats = memory.meeting_index_stats("m1").unwrap().unwrap();
        assert!(stats.passages > 0);
    }

    #[test]
    fn meeting_index_stats_distinguish_absent_pending_and_embedded() {
        let mut memory = Memory::in_memory().unwrap();
        assert_eq!(memory.meeting_index_stats("missing").unwrap(), None);

        let mut meeting = meeting("m1", 1, &[(10, "Ana")]);
        meeting.push_utterance(kuali_core::Utterance {
            id: "u1".into(),
            speaker_id: 10,
            start_ms: 0,
            end_ms: 1_000,
            text: "acordamos publicar el viernes".into(),
            confidence: None,
        });
        memory.index(&meeting).unwrap();

        let pending = memory.meeting_index_stats("m1").unwrap().unwrap();
        assert!(pending.passages > 0);
        assert_eq!(pending.pending_passages, pending.passages);

        memory
            .conn
            .execute(
                "INSERT INTO embedding (chunk_id, model, vector)
                 SELECT id, ?, X'' FROM chunk WHERE meeting_id = ?",
                params![crate::embed::MODEL_ID, meeting.meta.id],
            )
            .unwrap();
        let indexed = memory.meeting_index_stats("m1").unwrap().unwrap();
        assert_eq!(indexed.passages, pending.passages);
        assert_eq!(indexed.pending_passages, 0);
    }

    #[test]
    fn coverage_requires_every_finished_meeting_but_ignores_a_live_one() {
        let mut finished = meeting("finished", 1, &[(10, "Ana")]).meta;
        finished.ended_at = Some(Utc::now());
        let live = meeting("live", 1, &[(10, "Ana")]).meta;

        assert!(!finished_ids_are_covered(
            &[finished.clone(), live.clone()],
            &HashSet::new()
        ));
        assert!(finished_ids_are_covered(
            &[finished, live],
            &HashSet::from(["finished".to_string()])
        ));
    }

    #[test]
    fn force_index_rewrites_a_meeting_even_when_its_fingerprint_matches() {
        let mut memory = Memory::in_memory().unwrap();
        let mut meeting = meeting("m1", 1, &[(10, "Ana")]);
        meeting.push_utterance(kuali_core::Utterance {
            id: "u1".into(),
            speaker_id: 10,
            start_ms: 0,
            end_ms: 1_000,
            text: "texto original".into(),
            confidence: None,
        });
        memory.index(&meeting).unwrap();
        memory
            .conn
            .execute(
                "UPDATE chunk SET text = 'índice alterado' WHERE meeting_id = ?",
                params![meeting.meta.id],
            )
            .unwrap();

        assert!(!memory.index(&meeting).unwrap());
        let altered: String = memory
            .conn
            .query_row(
                "SELECT text FROM chunk WHERE meeting_id = ? LIMIT 1",
                params![meeting.meta.id],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(altered, "índice alterado");

        memory.force_index(&meeting).unwrap();
        let rebuilt: String = memory
            .conn
            .query_row(
                "SELECT text FROM chunk WHERE meeting_id = ? LIMIT 1",
                params![meeting.meta.id],
                |row| row.get(0),
            )
            .unwrap();
        assert!(rebuilt.contains("texto original"));
    }

    #[test]
    fn failed_inference_preserves_an_existing_text_and_its_vectors() {
        let mut memory = Memory::in_memory().unwrap();
        let mut original = meeting("m1", 1, &[(10, "Ana")]);
        original.push_utterance(kuali_core::Utterance {
            id: "u1".into(),
            speaker_id: 10,
            start_ms: 0,
            end_ms: 1_000,
            text: "versión sana del índice".into(),
            confidence: None,
        });
        memory.index(&original).unwrap();
        memory
            .conn
            .execute(
                "INSERT INTO embedding (chunk_id, model, vector)
                 SELECT id, ?, X'' FROM chunk WHERE meeting_id = ?",
                params![crate::embed::MODEL_ID, original.meta.id],
            )
            .unwrap();

        let mut replacement = original.clone();
        replacement.utterances[0].text = "reemplazo que no debe quedar a medias".into();
        let result = memory.force_index_using(&replacement, |_| {
            Err(crate::MemoryError::Embedding {
                message: "inferencia simulada fallida".into(),
            })
        });
        assert!(result.is_err());

        let text: String = memory
            .conn
            .query_row(
                "SELECT text FROM chunk WHERE meeting_id = ? LIMIT 1",
                params![original.meta.id],
                |row| row.get(0),
            )
            .unwrap();
        assert!(text.contains("versión sana"));
        let stats = memory.meeting_index_stats("m1").unwrap().unwrap();
        assert_eq!(stats.pending_passages, 0);
    }

    #[test]
    fn failed_inference_refreshes_an_existing_pending_index() {
        let mut memory = Memory::in_memory().unwrap();
        let mut original = meeting("m1", 1, &[(10, "Ana")]);
        original.push_utterance(kuali_core::Utterance {
            id: "u1".into(),
            speaker_id: 10,
            start_ms: 0,
            end_ms: 1_000,
            text: "texto pendiente anterior".into(),
            confidence: None,
        });
        memory.index(&original).unwrap();
        assert!(memory
            .meeting_index_stats("m1")
            .unwrap()
            .is_some_and(|stats| stats.pending_passages > 0));

        let mut replacement = original.clone();
        replacement.utterances[0].text = "texto pendiente actualizado".into();
        memory
            .force_index_using(&replacement, |_| {
                Err(crate::MemoryError::Embedding {
                    message: "inferencia simulada fallida".into(),
                })
            })
            .expect("a pending index must retain the current text for a later retry");

        let text: String = memory
            .conn
            .query_row(
                "SELECT text FROM chunk WHERE meeting_id = ? LIMIT 1",
                params![replacement.meta.id],
                |row| row.get(0),
            )
            .unwrap();
        assert!(text.contains("texto pendiente actualizado"));
        assert!(!text.contains("texto pendiente anterior"));
        let stats = memory.meeting_index_stats("m1").unwrap().unwrap();
        assert!(stats.passages > 0);
        assert_eq!(stats.pending_passages, stats.passages);
    }

    #[test]
    fn wrong_vector_count_refreshes_an_existing_pending_index() {
        let mut memory = Memory::in_memory().unwrap();
        let mut original = meeting("m1", 1, &[(10, "Ana")]);
        original.push_utterance(kuali_core::Utterance {
            id: "u1".into(),
            speaker_id: 10,
            start_ms: 0,
            end_ms: 1_000,
            text: "texto pendiente anterior".into(),
            confidence: None,
        });
        memory.index(&original).unwrap();

        let mut replacement = original.clone();
        replacement.utterances[0].text = "texto pendiente tras cardinalidad inválida".into();
        memory
            .force_index_using(&replacement, |_| Ok(Vec::new()))
            .expect("bad cardinality must leave a pending row that can be retried");

        let text: String = memory
            .conn
            .query_row(
                "SELECT text FROM chunk WHERE meeting_id = ? LIMIT 1",
                params![replacement.meta.id],
                |row| row.get(0),
            )
            .unwrap();
        assert!(text.contains("texto pendiente tras cardinalidad inválida"));
        let stats = memory.meeting_index_stats("m1").unwrap().unwrap();
        assert!(stats.passages > 0);
        assert_eq!(stats.pending_passages, stats.passages);
    }

    #[test]
    fn wrong_vector_count_still_creates_text_for_an_absent_meeting() {
        let mut memory = Memory::in_memory().unwrap();
        let mut meeting = meeting("missing", 1, &[(10, "Ana")]);
        meeting.push_utterance(kuali_core::Utterance {
            id: "u1".into(),
            speaker_id: 10,
            start_ms: 0,
            end_ms: 1_000,
            text: "este texto sí debe sobrevivir".into(),
            confidence: None,
        });

        memory
            .force_index_using(&meeting, |_| Ok(Vec::new()))
            .unwrap();

        let stats = memory.meeting_index_stats("missing").unwrap().unwrap();
        assert!(stats.passages > 0);
        assert_eq!(stats.pending_passages, stats.passages);
    }

    #[test]
    fn an_empty_meeting_does_not_invoke_the_embedding_model() {
        let mut memory = Memory::in_memory().unwrap();
        let meeting = meeting("empty", 1, &[(10, "Ana")]);

        memory
            .force_index_using(&meeting, |_| -> Result<Vec<Vec<f32>>> {
                panic!("an empty meeting has nothing to embed")
            })
            .unwrap();

        assert_eq!(
            memory.meeting_index_stats("empty").unwrap().unwrap(),
            MeetingIndexStats::default()
        );
    }

    #[test]
    fn a_changed_transcript_is_reindexed_without_leaving_the_old_passages() {
        let mut memory = Memory::in_memory().unwrap();
        let mut meeting = meeting("m1", 1, &[(10, "Ana")]);
        meeting.push_utterance(kuali_core::Utterance {
            id: "u1".into(),
            speaker_id: 10,
            start_ms: 0,
            end_ms: 1_000,
            text: "hablamos de kafka".into(),
            confidence: None,
        });
        memory.index(&meeting).unwrap();

        meeting.utterances[0].text = "hablamos de postgres".into();
        assert!(memory.index(&meeting).unwrap());

        let found = memory.retrieve(&Audience::Everything, "kafka", 10).unwrap();
        assert!(found.is_empty(), "the corrected text replaced the old one");
        let found = memory
            .retrieve(&Audience::Everything, "postgres", 10)
            .unwrap();
        assert_eq!(found.len(), 1);
    }

    #[test]
    fn bots_and_browser_identities_never_enter_the_access_list() {
        let mut memory = Memory::in_memory().unwrap();
        let mut meeting = meeting("m1", 1, &[(10, "Ana")]);
        meeting.upsert_speaker(Speaker {
            user_id: 999,
            source_id: None,
            audio_kind: None,
            display_name: "Kuali".into(),
            username: "kuali".into(),
            avatar_url: None,
            color: "#fff".into(),
            is_bot: true,
            is_self: false,
        });
        meeting.upsert_speaker(Speaker {
            user_id: browser_identifier(4242),
            source_id: Some("meet-device-2".into()),
            audio_kind: Some("separate".into()),
            display_name: "Pivel".into(),
            username: String::new(),
            avatar_url: None,
            color: "#fff".into(),
            is_bot: false,
            is_self: false,
        });
        memory.index(&meeting).unwrap();

        assert_eq!(attendance(&memory, "m1"), vec!["10".to_string()]);
    }

    #[test]
    fn a_browser_meeting_records_no_attendance_at_all() {
        let mut memory = Memory::in_memory().unwrap();
        let meeting = meeting("web1", browser_identifier(7), &[(10, "Ana")]);
        memory.index(&meeting).unwrap();

        assert!(attendance(&memory, "web1").is_empty());
    }

    #[test]
    fn forgetting_a_meeting_removes_its_passages_from_search() {
        let mut memory = Memory::in_memory().unwrap();
        let mut meeting = meeting("m1", 1, &[(10, "Ana")]);
        meeting.push_utterance(kuali_core::Utterance {
            id: "u1".into(),
            speaker_id: 10,
            start_ms: 0,
            end_ms: 1_000,
            text: "hablamos de kafka".into(),
            confidence: None,
        });
        memory.index(&meeting).unwrap();
        assert_eq!(
            memory
                .retrieve(&Audience::Everything, "kafka", 10)
                .unwrap()
                .len(),
            1
        );

        memory.forget("m1").unwrap();
        assert!(memory
            .retrieve(&Audience::Everything, "kafka", 10)
            .unwrap()
            .is_empty());
    }
}
