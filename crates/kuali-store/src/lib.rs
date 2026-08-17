//! Meeting persistence and export.
//!
//! One directory per meeting under Kuali's data directory:
//!
//! ```text
//! meetings/<id>/meta.json      header: title, date, duration
//! meetings/<id>/meeting.json   complete meeting, source of truth
//! ```
//!
//! Separate metadata avoids reading complete transcripts when listing meetings.
//!
//! Markdown and JSON exports are written to user-selected paths for portability,
//! not as application state.

pub mod markdown;

use std::path::{Path, PathBuf};

use kuali_core::{Meeting, MeetingMeta};

#[derive(Debug, thiserror::Error)]
pub enum StoreError {
    #[error("failed to access {path}: {source}")]
    Io {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("meeting file is corrupt: {0}")]
    Corrupt(#[from] serde_json::Error),
    #[error("no meeting exists with id `{0}`")]
    NotFound(String),
}

type Result<T> = std::result::Result<T, StoreError>;

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct MeetingSearchResult {
    #[serde(flatten)]
    pub meta: MeetingMeta,
    pub search_match: Option<SearchMatch>,
}

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SearchMatch {
    pub before: String,
    pub matched: String,
    pub after: String,
    pub source: String,
}

fn io(path: impl Into<PathBuf>) -> impl FnOnce(std::io::Error) -> StoreError {
    let path = path.into();
    move |source| StoreError::Io { path, source }
}

pub fn meeting_dir(id: &str) -> PathBuf {
    kuali_core::paths::meetings_dir().join(id)
}

fn meeting_file(id: &str) -> PathBuf {
    meeting_dir(id).join("meeting.json")
}

fn meta_file(id: &str) -> PathBuf {
    meeting_dir(id).join("meta.json")
}

/// Saves a meeting atomically on each utterance, preserving the previous valid
/// record if Kuali exits during a write.
pub fn save(meeting: &Meeting) -> Result<()> {
    let dir = meeting_dir(&meeting.meta.id);
    std::fs::create_dir_all(&dir).map_err(io(&dir))?;

    write_atomic(
        &meeting_file(&meeting.meta.id),
        &serde_json::to_vec_pretty(meeting)?,
    )?;
    write_atomic(
        &meta_file(&meeting.meta.id),
        &serde_json::to_vec_pretty(&meeting.meta)?,
    )?;
    Ok(())
}

pub fn load(id: &str) -> Result<Meeting> {
    let path = meeting_file(id);
    let bytes = match std::fs::read(&path) {
        Ok(bytes) => bytes,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            return Err(StoreError::NotFound(id.to_string()))
        }
        Err(source) => return Err(StoreError::Io { path, source }),
    };
    Ok(serde_json::from_slice(&bytes)?)
}

/// Saved meetings from newest to oldest.
///
/// Unreadable directories are skipped so one corrupted file cannot hide the
/// entire history.
pub fn list() -> Result<Vec<MeetingMeta>> {
    let root = kuali_core::paths::meetings_dir();
    let entries = match std::fs::read_dir(&root) {
        Ok(entries) => entries,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(source) => return Err(StoreError::Io { path: root, source }),
    };

    let mut metas: Vec<MeetingMeta> = entries
        .filter_map(|entry| entry.ok())
        .filter(|entry| entry.path().is_dir())
        .filter_map(|entry| {
            let name = entry.file_name().to_string_lossy().into_owned();
            let bytes = std::fs::read(meta_file(&name)).ok()?;
            serde_json::from_slice(&bytes).ok()
        })
        .collect();

    metas.sort_by(|a, b| b.started_at.cmp(&a.started_at));
    Ok(metas)
}

/// Searches complete meetings rather than only library metadata. Tauri runs this
/// on a blocking thread because large histories may require opening many files.
pub fn search(query: &str) -> Result<Vec<MeetingSearchResult>> {
    let query = normalize_search(query);
    let terms = query.split_whitespace().collect::<Vec<_>>();
    if terms.is_empty() {
        return Ok(list()?
            .into_iter()
            .map(|meta| MeetingSearchResult {
                meta,
                search_match: None,
            })
            .collect());
    }

    Ok(list()?
        .into_iter()
        .filter_map(|meta| {
            // Search retains the library's resilience by skipping unreadable meetings.
            let meeting = load(&meta.id).ok()?;
            meeting_matches_query(&meeting, &terms).then(|| MeetingSearchResult {
                search_match: find_search_match(&meeting, &terms),
                meta,
            })
        })
        .collect())
}

fn find_search_match(meeting: &Meeting, terms: &[&str]) -> Option<SearchMatch> {
    // Transcript matches provide the most useful context without opening a meeting.
    for utterance in &meeting.utterances {
        let source = format!(
            "{} · {}",
            meeting.speaker_name(utterance.speaker_id),
            kuali_core::format_timestamp(utterance.start_ms)
        );
        if let Some(found) = search_match_in(&utterance.text, terms, source) {
            return Some(found);
        }
    }

    if let Some(summary) = &meeting.summary {
        if let Some(found) = search_match_in(&summary.overview, terms, "Resumen") {
            return Some(found);
        }
        for (source, items) in [
            ("Punto clave", &summary.key_points),
            ("Decisión", &summary.decisions),
            ("Pregunta abierta", &summary.open_questions),
        ] {
            for item in items {
                if let Some(found) = search_match_in(item, terms, source) {
                    return Some(found);
                }
            }
        }
        for task in &summary.action_items {
            let text = [
                task.assignee.as_deref(),
                Some(task.text.as_str()),
                task.due.as_deref(),
            ]
            .into_iter()
            .flatten()
            .collect::<Vec<_>>()
            .join(" · ");
            if let Some(found) = search_match_in(&text, terms, "Tarea") {
                return Some(found);
            }
        }
    }

    // For participant searches, show one of that person's utterances as context.
    for speaker in meeting.speakers.iter().filter(|speaker| !speaker.is_bot) {
        let identity = format!("{} · {}", speaker.display_name, speaker.username);
        if !terms
            .iter()
            .any(|term| normalize_search(&identity).contains(term))
        {
            continue;
        }
        let Some(utterance) = meeting
            .utterances
            .iter()
            .find(|utterance| utterance.speaker_id == speaker.user_id)
        else {
            continue;
        };
        let text = format!("{}: {}", speaker.display_name, utterance.text);
        if let Some(found) = search_match_in(&text, terms, "Participante") {
            return Some(found);
        }
    }

    None
}

fn search_match_in(
    text: &str,
    terms: &[&str],
    source: impl Into<String> + Clone,
) -> Option<SearchMatch> {
    if text.trim().is_empty() {
        return None;
    }

    let units = text
        .char_indices()
        .flat_map(|(start, character)| {
            let end = start + character.len_utf8();
            character
                .to_lowercase()
                .map(move |lower| (fold_search_char(lower), start, end))
        })
        .collect::<Vec<_>>();

    for term in terms {
        let needle = term.chars().collect::<Vec<_>>();
        if needle.is_empty() || needle.len() > units.len() {
            continue;
        }
        let Some(at) = units
            .windows(needle.len())
            .position(|window| window.iter().map(|unit| unit.0).eq(needle.iter().copied()))
        else {
            continue;
        };
        let start = units[at].1;
        let end = units[at + needle.len() - 1].2;
        return Some(excerpt(text, start, end, source.clone().into()));
    }
    None
}

fn excerpt(text: &str, start: usize, end: usize, source: String) -> SearchMatch {
    const BEFORE: usize = 42;
    const AFTER: usize = 72;

    let full_before = &text[..start];
    let full_after = &text[end..];
    let mut before = full_before.chars().rev().take(BEFORE).collect::<Vec<_>>();
    before.reverse();
    let before = before.into_iter().collect::<String>();
    let after = full_after.chars().take(AFTER).collect::<String>();
    let before = compact_excerpt(&before);
    let after = compact_excerpt(&after);
    let space_before = full_before.chars().last().is_some_and(char::is_whitespace);
    let space_after = full_after.chars().next().is_some_and(char::is_whitespace);

    SearchMatch {
        before: format!(
            "{}{}{}",
            if full_before.chars().count() > BEFORE {
                "…"
            } else {
                ""
            },
            before,
            if space_before && !before.is_empty() {
                " "
            } else {
                ""
            }
        ),
        matched: text[start..end].to_string(),
        after: format!(
            "{}{}{}",
            if space_after && !after.is_empty() {
                " "
            } else {
                ""
            },
            after,
            if full_after.chars().count() > AFTER {
                "…"
            } else {
                ""
            }
        ),
        source,
    }
}

fn compact_excerpt(value: &str) -> String {
    value.split_whitespace().collect::<Vec<_>>().join(" ")
}

fn meeting_matches_query(meeting: &Meeting, terms: &[&str]) -> bool {
    let local_started = meeting.meta.started_at.with_timezone(&chrono::Local);
    let mut searchable = format!(
        "{} {} {} {} {} {} {} {} {} {} ",
        meeting.meta.display_title.as_deref().unwrap_or(""),
        meeting.meta.guild_name,
        meeting.meta.channel_name,
        meeting.meta.started_at.to_rfc3339(),
        meeting.meta.started_at.format("%Y-%m-%d"),
        meeting.meta.started_at.format("%d/%m/%Y"),
        spanish_date(&meeting.meta.started_at),
        local_started.format("%Y-%m-%d"),
        local_started.format("%d/%m/%Y"),
        spanish_date(&local_started),
    );

    for tag in &meeting.meta.tags {
        searchable.push_str(tag);
        searchable.push(' ');
    }
    if let Some(folder) = &meeting.meta.folder {
        searchable.push_str(folder);
        searchable.push(' ');
    }
    for speaker in meeting.speakers.iter().filter(|speaker| !speaker.is_bot) {
        searchable.push_str(&speaker.display_name);
        searchable.push(' ');
        searchable.push_str(&speaker.username);
        searchable.push(' ');
    }
    for utterance in &meeting.utterances {
        searchable.push_str(&utterance.text);
        searchable.push(' ');
    }
    if let Some(summary) = &meeting.summary {
        searchable.push_str(&summary.overview);
        searchable.push(' ');
        for item in summary
            .key_points
            .iter()
            .chain(&summary.decisions)
            .chain(&summary.open_questions)
        {
            searchable.push_str(item);
            searchable.push(' ');
        }
        for task in &summary.action_items {
            searchable.push_str(&task.text);
            searchable.push(' ');
            if let Some(assignee) = &task.assignee {
                searchable.push_str(assignee);
                searchable.push(' ');
            }
            if let Some(due) = &task.due {
                searchable.push_str(due);
                searchable.push(' ');
            }
        }
    }

    let searchable = normalize_search(&searchable);
    terms.iter().all(|term| searchable.contains(term))
}

fn normalize_search(value: &str) -> String {
    value
        .to_lowercase()
        .chars()
        .map(fold_search_char)
        .collect::<String>()
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
}

fn fold_search_char(character: char) -> char {
    match character {
        'á' | 'à' | 'ä' | 'â' => 'a',
        'é' | 'è' | 'ë' | 'ê' => 'e',
        'í' | 'ì' | 'ï' | 'î' => 'i',
        'ó' | 'ò' | 'ö' | 'ô' => 'o',
        'ú' | 'ù' | 'ü' | 'û' => 'u',
        'ñ' => 'n',
        character if character.is_alphanumeric() => character,
        _ => ' ',
    }
}

fn spanish_date(date: &impl chrono::Datelike) -> String {
    const MONTHS: [&str; 12] = [
        "enero",
        "febrero",
        "marzo",
        "abril",
        "mayo",
        "junio",
        "julio",
        "agosto",
        "septiembre",
        "octubre",
        "noviembre",
        "diciembre",
    ];
    format!(
        "{} {} {}",
        date.day(),
        MONTHS[date.month0() as usize],
        date.year()
    )
}

/// Server identities from Discord, kept apart from meetings so the library can
/// show an icon for calls recorded before Kuali knew about it.
fn guilds_file() -> PathBuf {
    kuali_core::paths::data_dir().join("guilds.json")
}

pub fn guilds() -> Vec<kuali_core::GuildInfo> {
    std::fs::read(guilds_file())
        .ok()
        .and_then(|bytes| serde_json::from_slice(&bytes).ok())
        .unwrap_or_default()
}

/// Merges what the bot reported with what was already known: a server the bot
/// no longer belongs to keeps its icon for its old meetings.
pub fn remember_guilds(reported: Vec<kuali_core::GuildInfo>) -> Result<()> {
    if reported.is_empty() {
        return Ok(());
    }
    let mut known = guilds();
    for guild in reported {
        match known.iter_mut().find(|entry| entry.id == guild.id) {
            Some(entry) => *entry = guild,
            None => known.push(guild),
        }
    }
    let path = guilds_file();
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(io(parent))?;
    }
    write_atomic(&path, &serde_json::to_vec_pretty(&known)?)
}

/// Folder names live in their own file so an empty folder still exists after
/// its last meeting is moved out or deleted.
fn folders_file() -> PathBuf {
    kuali_core::paths::data_dir().join("folders.json")
}

fn read_folder_catalog() -> Vec<String> {
    std::fs::read(folders_file())
        .ok()
        .and_then(|bytes| serde_json::from_slice::<Vec<String>>(&bytes).ok())
        .unwrap_or_default()
}

fn write_folder_catalog(folders: &[String]) -> Result<()> {
    let path = folders_file();
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(io(parent))?;
    }
    write_atomic(&path, &serde_json::to_vec_pretty(folders)?)
}

/// Every folder, in the order the user created them, including folders whose
/// meetings were all moved elsewhere.
pub fn list_folders() -> Result<Vec<String>> {
    let mut folders = read_folder_catalog();
    let known: std::collections::HashSet<String> =
        folders.iter().map(|name| name.to_lowercase()).collect();
    // Folders assigned to a meeting but missing from the catalog (an older
    // Kuali, a restored backup) are adopted instead of disappearing.
    let mut adopted = Vec::new();
    for meta in list()? {
        if let Some(folder) = meta.folder {
            if !known.contains(&folder.to_lowercase())
                && !adopted
                    .iter()
                    .any(|name: &String| name.eq_ignore_ascii_case(&folder))
            {
                adopted.push(folder);
            }
        }
    }
    folders.extend(adopted);
    Ok(folders)
}

/// Adds a folder and returns the catalog. Creating one that already exists is
/// not an error: the user gets the folder they asked for either way.
pub fn create_folder(name: &str) -> Result<Vec<String>> {
    let Some(name) = kuali_core::sanitize_folder(name) else {
        return list_folders();
    };
    let mut folders = list_folders()?;
    if !folders
        .iter()
        .any(|folder| folder.eq_ignore_ascii_case(&name))
    {
        folders.push(name);
        write_folder_catalog(&folders)?;
    }
    Ok(folders)
}

pub fn rename_folder(from: &str, to: &str) -> Result<Vec<String>> {
    let Some(to) = kuali_core::sanitize_folder(to) else {
        return list_folders();
    };
    let folders: Vec<String> = list_folders()?
        .into_iter()
        .map(|folder| {
            if folder.eq_ignore_ascii_case(from) {
                to.clone()
            } else {
                folder
            }
        })
        .collect();
    write_folder_catalog(&folders)?;

    for meta in list()? {
        if meta
            .folder
            .as_deref()
            .is_some_and(|folder| folder.eq_ignore_ascii_case(from))
        {
            let mut meeting = load(&meta.id)?;
            meeting.meta.folder = Some(to.clone());
            save(&meeting)?;
        }
    }
    Ok(folders)
}

/// Removes the folder. Its meetings are not deleted: they return to the
/// unfiled group.
pub fn delete_folder(name: &str) -> Result<Vec<String>> {
    let folders: Vec<String> = list_folders()?
        .into_iter()
        .filter(|folder| !folder.eq_ignore_ascii_case(name))
        .collect();
    write_folder_catalog(&folders)?;

    for meta in list()? {
        if meta
            .folder
            .as_deref()
            .is_some_and(|folder| folder.eq_ignore_ascii_case(name))
        {
            let mut meeting = load(&meta.id)?;
            meeting.meta.folder = None;
            save(&meeting)?;
        }
    }
    Ok(folders)
}

/// Files meetings under a folder, or takes them out of any folder with `None`.
pub fn set_folder(ids: &[String], folder: Option<&str>) -> Result<()> {
    let folder = folder.and_then(kuali_core::sanitize_folder);
    if let Some(name) = &folder {
        create_folder(name)?;
    }
    for id in ids {
        let mut meeting = load(id)?;
        meeting.meta.folder = folder.clone();
        save(&meeting)?;
    }
    Ok(())
}

/// Replaces a meeting's tags and returns what was actually stored after
/// sanitizing. Writing through `save` keeps `meeting.json` and `meta.json` in
/// agreement, which `list` depends on.
pub fn set_tags(id: &str, tags: Vec<String>) -> Result<Vec<String>> {
    let mut meeting = load(id)?;
    meeting.meta.tags = kuali_core::sanitize_tags(tags);
    save(&meeting)?;
    Ok(meeting.meta.tags)
}

/// Every tag in use, ordered by how often it appears so suggestions start with
/// the labels the user actually relies on.
pub fn all_tags() -> Result<Vec<String>> {
    let mut counts: std::collections::HashMap<String, (String, usize)> =
        std::collections::HashMap::new();
    for meta in list()? {
        for tag in meta.tags {
            let entry = counts
                .entry(tag.to_lowercase())
                .or_insert_with(|| (tag.clone(), 0));
            entry.1 += 1;
        }
    }
    let mut tags: Vec<(String, usize)> = counts.into_values().collect();
    tags.sort_by(|a, b| {
        b.1.cmp(&a.1)
            .then_with(|| a.0.to_lowercase().cmp(&b.0.to_lowercase()))
    });
    Ok(tags.into_iter().map(|(tag, _)| tag).collect())
}

pub fn delete(id: &str) -> Result<()> {
    let dir = meeting_dir(id);
    match std::fs::remove_dir_all(&dir) {
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Err(StoreError::NotFound(id.into())),
        Err(source) => Err(StoreError::Io { path: dir, source }),
        Ok(()) => Ok(()),
    }
}

/// Exports Markdown to the provided path.
pub fn export_markdown(meeting: &Meeting, path: &Path) -> Result<()> {
    write_atomic(path, markdown::render(meeting).as_bytes())
}

/// Exports JSON to the provided path.
pub fn export_json(meeting: &Meeting, path: &Path) -> Result<()> {
    write_atomic(path, &serde_json::to_vec_pretty(meeting)?)
}

/// Produces a portable export filename without troublesome filesystem characters.
pub fn suggested_filename(meeting: &Meeting, extension: &str) -> String {
    let slug: String = meeting
        .meta
        .title()
        .chars()
        .map(|c| if c.is_alphanumeric() { c } else { '-' })
        .collect();
    let slug = slug
        .split('-')
        .filter(|part| !part.is_empty())
        .collect::<Vec<_>>()
        .join("-")
        .to_lowercase();

    format!(
        "{}-{}.{extension}",
        meeting.meta.started_at.format("%Y%m%d-%H%M"),
        if slug.is_empty() {
            "reunion".into()
        } else {
            slug
        }
    )
}

fn write_atomic(path: &Path, bytes: &[u8]) -> Result<()> {
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir).map_err(io(dir))?;
    }
    let tmp = path.with_extension(format!(
        "{}.tmp",
        path.extension().and_then(|e| e.to_str()).unwrap_or("dat")
    ));
    std::fs::write(&tmp, bytes).map_err(io(&tmp))?;
    std::fs::rename(&tmp, path).map_err(io(path))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::{TimeZone, Utc};
    use kuali_core::{ActionItem, MeetingMeta, MeetingSummary, Speaker, Utterance};

    fn sample() -> Meeting {
        Meeting::new(MeetingMeta {
            id: "abc123".into(),
            display_title: None,
            guild_id: 1,
            guild_name: "Mi Servidor".into(),
            channel_id: 2,
            channel_name: "Sala 1".into(),
            started_at: Utc.with_ymd_and_hms(2026, 8, 6, 14, 30, 0).unwrap(),
            ended_at: None,
            tags: Vec::new(),
            folder: None,
        })
    }

    #[test]
    fn tags_and_folders_take_part_in_search_like_any_other_field() {
        let mut meeting = sample();
        meeting.meta.tags = vec!["Cliente Acme".into()];
        meeting.meta.folder = Some("Clientes clave".into());
        assert!(meeting_matches_query(&meeting, &["acme"]));
        assert!(meeting_matches_query(&meeting, &["clientes", "clave"]));
        assert!(!meeting_matches_query(&sample(), &["acme"]));
    }

    #[test]
    fn a_meeting_round_trips_through_json() {
        let meeting = sample();
        let bytes = serde_json::to_vec(&meeting).unwrap();
        let back: Meeting = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(back.meta.id, meeting.meta.id);
        assert_eq!(back.meta.title(), "Mi Servidor · Sala 1");
    }

    #[test]
    fn export_names_are_sortable_and_filesystem_safe() {
        let name = suggested_filename(&sample(), "md");
        assert_eq!(name, "20260806-1430-mi-servidor-sala-1.md");
        assert!(!name.contains('·'));
        assert!(!name.contains(' '));
        assert!(!name.contains('/'));
    }

    #[test]
    fn a_meeting_with_an_unprintable_title_still_gets_a_name() {
        let mut meeting = sample();
        meeting.meta.guild_name = "···".into();
        meeting.meta.channel_name = "···".into();
        assert_eq!(
            suggested_filename(&meeting, "json"),
            "20260806-1430-reunion.json"
        );
    }

    #[test]
    fn every_meeting_file_lives_under_its_own_directory() {
        assert!(meeting_file("abc").starts_with(meeting_dir("abc")));
        assert!(meta_file("abc").starts_with(meeting_dir("abc")));
        assert_ne!(meeting_file("abc"), meta_file("abc"));
    }

    #[test]
    fn loading_a_meeting_that_was_never_saved_says_so_clearly() {
        match load("no-existe-jamas-4242") {
            Err(StoreError::NotFound(id)) => assert_eq!(id, "no-existe-jamas-4242"),
            other => panic!("expected NotFound, got {other:?}"),
        }
    }

    #[test]
    fn search_matches_channel_date_transcript_participant_and_tasks() {
        let mut meeting = sample();
        meeting.speakers.push(Speaker {
            user_id: 10,
            source_id: None,
            audio_kind: None,
            display_name: "Ángela".into(),
            username: "angela.dev".into(),
            avatar_url: None,
            color: "#fff".into(),
            is_bot: false,
            is_self: false,
        });
        meeting.utterances.push(Utterance {
            id: "u1".into(),
            speaker_id: 10,
            start_ms: 0,
            end_ms: 1_000,
            text: "Hay que revisar el despliegue de Kafka".into(),
            confidence: Some(0.9),
        });
        meeting.summary = Some(MeetingSummary {
            action_items: vec![ActionItem {
                id: "t1".into(),
                text: "Preparar la demostración".into(),
                assignee: Some("Ángela".into()),
                due: Some("el viernes".into()),
                source_ms: Some(0),
                done: false,
            }],
            ..Default::default()
        });

        for query in [
            "sala 1",
            "06/08/2026",
            "6 agosto 2026",
            "kafka",
            "angela",
            "demostracion viernes",
        ] {
            let normalized = normalize_search(query);
            let terms = normalized.split_whitespace().collect::<Vec<_>>();
            assert!(
                meeting_matches_query(&meeting, &terms),
                "query did not match: {query}"
            );
        }
        let terms = ["postgres"];
        assert!(!meeting_matches_query(&meeting, &terms));
    }

    #[test]
    fn search_excerpt_keeps_the_original_text_and_marks_an_accented_match() {
        let mut meeting = sample();
        meeting.speakers.push(Speaker {
            user_id: 10,
            source_id: None,
            audio_kind: None,
            display_name: "Ángela".into(),
            username: "angela.dev".into(),
            avatar_url: None,
            color: "#fff".into(),
            is_bot: false,
            is_self: false,
        });
        meeting.utterances.push(Utterance {
            id: "u1".into(),
            speaker_id: 10,
            start_ms: 62_000,
            end_ms: 64_000,
            text: "Hola, ¿cómo estás? Aquí hablamos de Kafka.".into(),
            confidence: Some(0.9),
        });

        let found = find_search_match(&meeting, &["como"]).expect("search context should exist");

        assert_eq!(found.before, "Hola, ¿");
        assert_eq!(found.matched, "cómo");
        assert_eq!(found.after, " estás? Aquí hablamos de Kafka.");
        assert_eq!(found.source, "Ángela · 01:02");
    }
}
