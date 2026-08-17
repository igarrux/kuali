//! Searchable memory of past meetings.
//!
//! The meeting library answers "open this meeting". This crate answers "what did
//! we decide about the migration?" across every meeting at once, which needs a
//! different shape: meetings are cut into passages, indexed for retrieval, and
//! handed to an LLM as evidence.
//!
//! ```text
//! memory.sqlite3
//!   meeting     one row per indexed meeting, with the hash that detects staleness
//!   attendance  who was in the call — the access-control list, nothing else
//!   chunk       retrievable passages: transcript windows plus summary items
//!   chunk_fts   FTS5 mirror of `chunk.text`, used for lexical ranking
//!   link        deterministic graph edges used to reach neighbouring meetings
//! ```
//!
//! # Access control
//!
//! Restricting results after searching everything is the wrong shape: the
//! filtering step becomes optional, and forgetting it once leaks a meeting.
//! Here the only way to reach a passage is [`Memory::retrieve`], which takes an
//! [`Audience`] and folds it into the SQL as a `visible` CTE that every other
//! clause selects from. A query that skips the check does not compile, because
//! there is no query without an audience.
//!
//! The LLM is never asked to respect a boundary. It only ever receives passages
//! that survived the SQL scope.

mod ask;
mod chunk;
pub mod embed;
mod index;
mod retrieve;

pub use ask::{answer, Answer, Asker, Citation};
pub use chunk::{ChunkKind, DraftChunk};
pub use index::SyncReport;
pub use retrieve::Passage;

use std::path::{Path, PathBuf};

use rusqlite::Connection;

#[derive(Debug, thiserror::Error)]
pub enum MemoryError {
    #[error("meeting memory is unavailable: {0}")]
    Database(#[from] rusqlite::Error),
    #[error("failed to read the meeting library: {0}")]
    Store(#[from] kuali_store::StoreError),
    #[error("the embedding model has not been downloaded yet")]
    EmbeddingModelMissing,
    #[error("{message}")]
    Embedding { message: String },
    #[error("failed to create {path}: {source}")]
    Io {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
}

pub type Result<T> = std::result::Result<T, MemoryError>;

/// Who is asking, and therefore which meetings the search is allowed to see.
///
/// This is the whole access-control model. It is an argument rather than a
/// setting because a setting can be read at the wrong moment, while an argument
/// has to be supplied at the call site every time.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Audience {
    /// The complete library. The desktop application runs on the machine that
    /// recorded these meetings, for the person who owns them, so there is
    /// nothing to withhold.
    Everything,
    /// A Discord account, limited to meetings that recorded it as present in
    /// the call, inside one server.
    ///
    /// The server bound matters even though attendance alone would be safe:
    /// answering inside server A with what was said in server B moves content
    /// across an organizational boundary the person did not ask to cross.
    DiscordParticipant { user_id: u64, guild_id: u64 },
}

impl Audience {
    /// SQL defining the `visible` set, plus its bound parameters.
    ///
    /// Every retrieval query starts with this CTE and reaches meetings only
    /// through it.
    fn visible_meetings(&self) -> (&'static str, Vec<rusqlite::types::Value>) {
        match self {
            Self::Everything => ("SELECT id FROM meeting", Vec::new()),
            Self::DiscordParticipant { user_id, guild_id } => (
                // `discord_attributable` excludes meetings whose participants
                // exist only as browser hashes: no Discord account can prove it
                // attended one, so none may reach it.
                "SELECT m.id FROM meeting m
                 JOIN attendance a ON a.meeting_id = m.id
                 WHERE m.discord_attributable = 1
                   AND m.guild_id = ?
                   AND a.speaker_id = ?",
                vec![
                    rusqlite::types::Value::Text(guild_id.to_string()),
                    rusqlite::types::Value::Text(user_id.to_string()),
                ],
            ),
        }
    }
}

/// Handle to the meeting index.
pub struct Memory {
    conn: Connection,
}

impl Memory {
    /// Opens the index beside the meeting library, creating it on first use.
    pub fn open() -> Result<Self> {
        let dir = kuali_core::paths::data_dir();
        std::fs::create_dir_all(&dir).map_err(|source| MemoryError::Io {
            path: dir.clone(),
            source,
        })?;
        Self::open_at(&dir.join("memory.sqlite3"))
    }

    pub fn open_at(path: &Path) -> Result<Self> {
        Self::from_connection(Connection::open(path)?)
    }

    /// Index that disappears with the process, used by tests.
    pub fn in_memory() -> Result<Self> {
        Self::from_connection(Connection::open_in_memory()?)
    }

    fn from_connection(conn: Connection) -> Result<Self> {
        let memory = Self { conn };
        memory.migrate()?;
        Ok(memory)
    }

    /// Creates the schema. Every statement is idempotent, so opening an existing
    /// index is the same code path as creating one.
    ///
    /// The index is a derived artifact: it can always be rebuilt from
    /// `meetings/`, which is why a future schema change may simply drop and
    /// re-sync rather than migrate data in place.
    fn migrate(&self) -> Result<()> {
        self.conn.execute_batch(
            r#"
            PRAGMA journal_mode = WAL;
            PRAGMA foreign_keys = ON;

            CREATE TABLE IF NOT EXISTS meeting (
                id                   TEXT PRIMARY KEY,
                guild_id             TEXT NOT NULL,
                guild_name           TEXT NOT NULL,
                channel_id           TEXT NOT NULL,
                channel_name         TEXT NOT NULL,
                title                TEXT NOT NULL,
                started_at           TEXT NOT NULL,
                folder               TEXT,
                -- Whether a Discord account can be matched against this
                -- meeting's participants at all. Browser meetings cannot.
                discord_attributable INTEGER NOT NULL,
                -- Display name of the participant the platform marked as the
                -- local one, when it said so. Browser meetings do; Discord
                -- leaves it null because identity comes from the command there.
                self_name            TEXT,
                -- Fingerprint of the stored meeting, so re-indexing an
                -- unchanged meeting costs one comparison.
                content_hash         TEXT NOT NULL
            );

            -- The access-control list, deliberately alone in its own table.
            -- Topical relationships live in `link`; mixing the two would put a
            -- permission check one mistyped `kind` away from failing open.
            CREATE TABLE IF NOT EXISTS attendance (
                meeting_id   TEXT NOT NULL REFERENCES meeting(id) ON DELETE CASCADE,
                speaker_id   TEXT NOT NULL,
                display_name TEXT NOT NULL,
                PRIMARY KEY (meeting_id, speaker_id)
            );
            CREATE INDEX IF NOT EXISTS attendance_by_speaker
                ON attendance(speaker_id);

            CREATE TABLE IF NOT EXISTS chunk (
                id         INTEGER PRIMARY KEY,
                meeting_id TEXT NOT NULL REFERENCES meeting(id) ON DELETE CASCADE,
                kind       TEXT NOT NULL,
                start_ms   INTEGER,
                end_ms     INTEGER,
                speakers   TEXT NOT NULL,
                text       TEXT NOT NULL
            );
            CREATE INDEX IF NOT EXISTS chunk_by_meeting ON chunk(meeting_id);

            -- `remove_diacritics 2` makes "decision" find "decisión", matching
            -- what library search already does for accented Spanish.
            CREATE VIRTUAL TABLE IF NOT EXISTS chunk_fts USING fts5(
                text,
                content='chunk',
                content_rowid='id',
                tokenize="unicode61 remove_diacritics 2"
            );

            -- One vector per passage. Kept in its own table so turning the
            -- feature off, or changing model, drops vectors without touching
            -- the passages themselves.
            CREATE TABLE IF NOT EXISTS embedding (
                chunk_id INTEGER PRIMARY KEY REFERENCES chunk(id) ON DELETE CASCADE,
                -- Vectors from different models are not comparable. Recording
                -- which produced this one turns a model change into a rebuild
                -- instead of silently meaningless scores.
                model    TEXT NOT NULL,
                vector   BLOB NOT NULL
            );

            -- An external-content FTS5 table does not observe its source on its
            -- own. These triggers are the documented way to keep the two in
            -- agreement, which matters because re-indexing a meeting rewrites
            -- all of its chunks.
            CREATE TRIGGER IF NOT EXISTS chunk_after_insert AFTER INSERT ON chunk BEGIN
                INSERT INTO chunk_fts(rowid, text) VALUES (new.id, new.text);
            END;
            CREATE TRIGGER IF NOT EXISTS chunk_after_delete AFTER DELETE ON chunk BEGIN
                INSERT INTO chunk_fts(chunk_fts, rowid, text)
                VALUES ('delete', old.id, old.text);
            END;
            CREATE TRIGGER IF NOT EXISTS chunk_after_update AFTER UPDATE ON chunk BEGIN
                INSERT INTO chunk_fts(chunk_fts, rowid, text)
                VALUES ('delete', old.id, old.text);
                INSERT INTO chunk_fts(rowid, text) VALUES (new.id, new.text);
            END;

            -- Deterministic graph edges. NOT a permission table: `person` here
            -- includes people merely named as an assignee, who may never have
            -- attended. Reaching a meeting through a link still requires that
            -- meeting to be in `visible`.
            CREATE TABLE IF NOT EXISTS link (
                meeting_id TEXT NOT NULL REFERENCES meeting(id) ON DELETE CASCADE,
                kind       TEXT NOT NULL,
                key        TEXT NOT NULL,
                PRIMARY KEY (meeting_id, kind, key)
            );
            CREATE INDEX IF NOT EXISTS link_by_key ON link(kind, key);
            "#,
        )?;
        Ok(())
    }
}

/// Folds a value into the form used for graph keys: lowercase, unaccented, and
/// whitespace-collapsed, so "Ángela" and "angela" reach the same neighbours.
///
/// FTS5 handles the equivalent folding for text search through its tokenizer;
/// this exists for the exact-match `link` keys, which never pass through it.
fn normalize_key(value: &str) -> String {
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn opening_the_index_twice_leaves_the_same_schema() {
        let dir = std::env::temp_dir().join(format!("kuali-memory-{}", uuid_like()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("memory.sqlite3");

        Memory::open_at(&path).expect("first open creates the schema");
        Memory::open_at(&path).expect("second open finds it already there");

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn graph_keys_ignore_case_accents_and_spacing() {
        assert_eq!(normalize_key("  Ángela   Ruiz "), "angela ruiz");
        assert_eq!(normalize_key("ÁNGELA ruiz"), normalize_key("angela Ruiz"));
        assert_eq!(normalize_key("cliente-acme"), "cliente acme");
    }

    #[test]
    fn a_discord_audience_never_reaches_a_browser_meeting() {
        let (sql, params) = Audience::DiscordParticipant {
            user_id: 42,
            guild_id: 7,
        }
        .visible_meetings();

        assert!(sql.contains("discord_attributable = 1"));
        assert!(sql.contains("a.speaker_id = ?"));
        assert_eq!(params.len(), 2);
    }

    fn uuid_like() -> u64 {
        use std::time::{SystemTime, UNIX_EPOCH};
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .subsec_nanos() as u64
    }
}
