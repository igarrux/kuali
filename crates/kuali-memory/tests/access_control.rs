//! What a person asking Kuali is allowed to reach.
//!
//! These tests use the public API only, because that is the surface a mistake
//! would actually escape through. They are written as claims about behaviour
//! rather than about implementation, so a rewrite of the ranking, the schema, or
//! the graph still has to satisfy them.

use chrono::{TimeZone, Utc};
use kuali_core::{
    browser_identifier, ActionItem, Meeting, MeetingMeta, MeetingSummary, Speaker, Utterance,
};
use kuali_memory::{Audience, Memory};

const ANA: u64 = 10;
const LUIS: u64 = 20;
const GUILD_WORK: u64 = 100;
const GUILD_CLIENT: u64 = 200;

fn meeting(id: &str, guild_id: u64, attendees: &[(u64, &str)], said: &str) -> Meeting {
    let mut meeting = Meeting::new(MeetingMeta {
        id: id.into(),
        display_title: Some(format!("Reunión {id}")),
        guild_id,
        guild_name: "Servidor".into(),
        channel_id: guild_id + 1,
        channel_name: "General".into(),
        started_at: Utc.with_ymd_and_hms(2026, 8, 1, 10, 0, 0).unwrap(),
        ended_at: None,
        tags: Vec::new(),
        folder: None,
    });
    for (index, (user_id, name)) in attendees.iter().enumerate() {
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
        if index == 0 {
            meeting.push_utterance(Utterance {
                id: format!("{id}-u1"),
                speaker_id: *user_id,
                start_ms: 0,
                end_ms: 1_000,
                text: said.into(),
                confidence: None,
            });
        }
    }
    meeting
}

fn titles(passages: &[kuali_memory::Passage]) -> Vec<String> {
    let mut ids: Vec<String> = passages
        .iter()
        .map(|passage| passage.meeting_id.clone())
        .collect();
    ids.sort();
    ids.dedup();
    ids
}

fn ana_at_work() -> Audience {
    Audience::DiscordParticipant {
        user_id: ANA,
        guild_id: GUILD_WORK,
    }
}

#[test]
fn a_question_only_reaches_meetings_the_asker_was_in() {
    let mut memory = Memory::in_memory().unwrap();
    memory
        .index(&meeting(
            "attended",
            GUILD_WORK,
            &[(ANA, "Ana"), (LUIS, "Luis")],
            "acordamos migrar a kafka en septiembre",
        ))
        .unwrap();
    memory
        .index(&meeting(
            "missed",
            GUILD_WORK,
            &[(LUIS, "Luis")],
            "acordamos cancelar el contrato de kafka",
        ))
        .unwrap();

    let found = memory.retrieve(&ana_at_work(), "kafka", 20).unwrap();

    assert_eq!(titles(&found), vec!["attended"]);
    assert!(
        !found
            .iter()
            .any(|passage| passage.text.contains("cancelar")),
        "content from a meeting Ana missed must never appear"
    );
}

#[test]
fn a_silent_attendee_still_counts_as_having_been_there() {
    let mut memory = Memory::in_memory().unwrap();
    // Ana is in the room but says nothing: only Luis has an utterance.
    memory
        .index(&meeting(
            "quiet",
            GUILD_WORK,
            &[(LUIS, "Luis"), (ANA, "Ana")],
            "hablamos del despliegue de kafka",
        ))
        .unwrap();

    let found = memory.retrieve(&ana_at_work(), "kafka", 20).unwrap();

    assert_eq!(titles(&found), vec!["quiet"]);
}

#[test]
fn asking_in_one_server_never_answers_with_another_servers_meeting() {
    let mut memory = Memory::in_memory().unwrap();
    memory
        .index(&meeting(
            "work",
            GUILD_WORK,
            &[(ANA, "Ana")],
            "revisamos el presupuesto interno",
        ))
        .unwrap();
    // Same person, same topic, different organization.
    memory
        .index(&meeting(
            "client",
            GUILD_CLIENT,
            &[(ANA, "Ana")],
            "revisamos el presupuesto del cliente",
        ))
        .unwrap();

    let found = memory.retrieve(&ana_at_work(), "presupuesto", 20).unwrap();

    assert_eq!(titles(&found), vec!["work"]);
}

#[test]
fn a_browser_meeting_is_unreachable_from_discord_even_with_a_matching_id() {
    let mut memory = Memory::in_memory().unwrap();
    let synthetic = browser_identifier(ANA);
    let mut web = meeting(
        "web",
        browser_identifier(1),
        &[(synthetic, "Ana")],
        "hablamos del presupuesto en meet",
    );
    web.speakers[0].source_id = Some("meet-device-2".into());
    memory.index(&web).unwrap();

    // Asking as the synthesized identifier itself, which is the strongest form
    // of the attack: the number in the access list matches exactly.
    let found = memory
        .retrieve(
            &Audience::DiscordParticipant {
                user_id: synthetic,
                guild_id: browser_identifier(1),
            },
            "presupuesto",
            20,
        )
        .unwrap();

    assert!(found.is_empty());
    // The desktop application, which is not scoped to a Discord account, still
    // reaches it — the meeting is indexed, just not attributable to Discord.
    let found = memory
        .retrieve(&Audience::Everything, "presupuesto", 20)
        .unwrap();
    assert_eq!(titles(&found), vec!["web"]);
}

#[test]
fn the_graph_widens_the_search_without_widening_the_audience() {
    let mut memory = Memory::in_memory().unwrap();

    // Ana attends the first call about the migration.
    let mut first = meeting(
        "attended",
        GUILD_WORK,
        &[(ANA, "Ana"), (LUIS, "Luis")],
        "arrancamos la migración de kafka",
    );
    first.meta.tags = vec!["migración".into()];
    memory.index(&first).unwrap();

    // The follow-up shares the tag, the channel and both people, but Ana was
    // not in it. It is a textbook neighbour, and it must stay out.
    let mut follow_up = meeting(
        "missed-follow-up",
        GUILD_WORK,
        &[(LUIS, "Luis")],
        "decidimos abortar y volver a rabbit",
    );
    follow_up.meta.tags = vec!["migración".into()];
    follow_up.meta.channel_id = first.meta.channel_id;
    memory.index(&follow_up).unwrap();

    // A third call Ana did attend, connected by the same tag, using wording the
    // question does not contain. This is what the graph is for.
    let mut related = meeting(
        "attended-related",
        GUILD_WORK,
        &[(ANA, "Ana"), (LUIS, "Luis")],
        "el plan de despliegue quedó firmado",
    );
    related.meta.tags = vec!["migración".into()];
    related.meta.channel_id = first.meta.channel_id;
    memory.index(&related).unwrap();

    let found = memory.retrieve(&ana_at_work(), "kafka", 20).unwrap();
    let reached = titles(&found);

    assert!(
        reached.contains(&"attended".to_string()),
        "the direct hit must be there"
    );
    assert!(
        reached.contains(&"attended-related".to_string()),
        "a connected meeting Ana attended is exactly what the graph is for"
    );
    assert!(
        !reached.contains(&"missed-follow-up".to_string()),
        "the graph must not become a way around attendance"
    );
    assert!(
        !found.iter().any(|passage| passage.text.contains("abortar")),
        "no wording from the missed meeting may leak through a neighbour"
    );
}

#[test]
fn a_task_reaches_its_owner_by_name() {
    let mut memory = Memory::in_memory().unwrap();
    let mut meeting = meeting(
        "planning",
        GUILD_WORK,
        &[(ANA, "Ana"), (LUIS, "Luis")],
        "repartimos el trabajo",
    );
    meeting.summary = Some(MeetingSummary {
        overview: "Repartimos el trabajo de la demo.".into(),
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
    memory.index(&meeting).unwrap();

    let found = memory
        .retrieve(&ana_at_work(), "¿qué tiene pendiente Ana?", 20)
        .unwrap();

    assert!(found
        .iter()
        .any(|passage| passage.text.contains("Preparar la demostración")));
}

#[test]
fn someone_who_attended_nothing_gets_an_empty_answer_rather_than_an_error() {
    let mut memory = Memory::in_memory().unwrap();
    memory
        .index(&meeting(
            "attended",
            GUILD_WORK,
            &[(LUIS, "Luis")],
            "acordamos migrar a kafka",
        ))
        .unwrap();

    let found = memory
        .retrieve(
            &Audience::DiscordParticipant {
                user_id: 999_999,
                guild_id: GUILD_WORK,
            },
            "kafka",
            20,
        )
        .expect("an outsider asking is a normal question, not a failure");

    assert!(found.is_empty());
}

#[test]
fn deleting_a_meeting_takes_it_out_of_every_future_answer() {
    let mut memory = Memory::in_memory().unwrap();
    memory
        .index(&meeting(
            "regretted",
            GUILD_WORK,
            &[(ANA, "Ana")],
            "dijimos algo sobre kafka",
        ))
        .unwrap();
    assert!(!memory
        .retrieve(&ana_at_work(), "kafka", 20)
        .unwrap()
        .is_empty());

    memory.forget("regretted").unwrap();

    assert!(memory
        .retrieve(&ana_at_work(), "kafka", 20)
        .unwrap()
        .is_empty());
    assert!(memory
        .retrieve(&Audience::Everything, "kafka", 20)
        .unwrap()
        .is_empty());
}

/// The semantic path is a second way into the same passages, so it needs the
/// same proof as the lexical one. These need the model on disk:
///
/// ```text
/// KUALI_EMBED_MODELS_DIR=/path/to/models cargo test -p kuali-memory -- --ignored
/// ```
fn embedder() -> Option<kuali_memory::embed::Embedder> {
    let dir = std::env::var("KUALI_EMBED_MODELS_DIR").ok()?;
    kuali_memory::embed::Embedder::load(std::path::Path::new(&dir)).ok()
}

#[test]
#[ignore = "needs the embedding model on disk"]
fn meaning_finds_a_passage_that_shares_no_words_with_the_question() {
    let mut model = embedder().expect("set KUALI_EMBED_MODELS_DIR");
    let mut memory = Memory::in_memory().unwrap();
    memory
        .index_with(
            &meeting(
                "infra",
                GUILD_WORK,
                &[(ANA, "Ana"), (LUIS, "Luis")],
                "Sebas comentó que el cortafuegos estaba bloqueando el puerto 8080",
            ),
            Some(&mut model),
        )
        .unwrap();

    // Not one word of the question appears in the meeting.
    let found = memory
        .retrieve_with(
            &ana_at_work(),
            "problemas de firewall",
            10,
            Some(&mut model),
        )
        .unwrap();

    assert!(
        found.iter().any(|p| p.text.contains("cortafuegos")),
        "meaning should reach it even though the wording does not"
    );
}

#[test]
#[ignore = "needs the embedding model on disk"]
fn meaning_is_not_a_way_around_attendance() {
    let mut model = embedder().expect("set KUALI_EMBED_MODELS_DIR");
    let mut memory = Memory::in_memory().unwrap();

    // Ana was not in this one. It is the single best semantic match there is.
    memory
        .index_with(
            &meeting(
                "missed",
                GUILD_WORK,
                &[(LUIS, "Luis")],
                "el cortafuegos estaba bloqueando el puerto y por eso falló todo",
            ),
            Some(&mut model),
        )
        .unwrap();
    // Same server, same person asking, different topic entirely.
    memory
        .index_with(
            &meeting(
                "attended",
                GUILD_WORK,
                &[(ANA, "Ana")],
                "revisamos las traducciones de la interfaz",
            ),
            Some(&mut model),
        )
        .unwrap();

    let found = memory
        .retrieve_with(
            &ana_at_work(),
            "problemas de firewall",
            10,
            Some(&mut model),
        )
        .unwrap();

    assert!(
        !found.iter().any(|p| p.meeting_id == "missed"),
        "a perfect semantic match Ana never attended must stay unreachable"
    );
    assert!(
        !found.iter().any(|p| p.text.contains("cortafuegos")),
        "no wording from the missed meeting may surface through vectors"
    );
}

#[test]
#[ignore = "needs the embedding model on disk"]
fn a_browser_meeting_stays_unreachable_through_meaning_too() {
    let mut model = embedder().expect("set KUALI_EMBED_MODELS_DIR");
    let mut memory = Memory::in_memory().unwrap();
    let synthetic = browser_identifier(ANA);
    let mut web = meeting(
        "web",
        browser_identifier(1),
        &[(synthetic, "Ana")],
        "el cortafuegos bloqueaba el puerto durante la llamada de meet",
    );
    web.speakers[0].source_id = Some("meet-device-2".into());
    memory.index_with(&web, Some(&mut model)).unwrap();

    let found = memory
        .retrieve_with(
            &Audience::DiscordParticipant {
                user_id: synthetic,
                guild_id: browser_identifier(1),
            },
            "problemas de firewall",
            10,
            Some(&mut model),
        )
        .unwrap();

    assert!(found.is_empty());
}

#[test]
#[ignore = "needs the embedding model on disk"]
fn the_pending_count_is_a_real_count_of_work_left() {
    let mut model = embedder().expect("set KUALI_EMBED_MODELS_DIR");
    let mut memory = Memory::in_memory().unwrap();

    // Indexed without the model, the way a library recorded before the feature
    // was enabled looks.
    memory
        .index(&meeting(
            "one",
            GUILD_WORK,
            &[(ANA, "Ana")],
            "hablamos de algo",
        ))
        .unwrap();
    memory
        .index(&meeting(
            "two",
            GUILD_WORK,
            &[(ANA, "Ana")],
            "y de otra cosa",
        ))
        .unwrap();

    let pending = memory.pending_embeddings().unwrap();
    assert_eq!(pending, 2, "one passage per meeting is still waiting");

    let mut seen = Vec::new();
    let done = memory
        .embed_pending(&mut model, |done, total| {
            seen.push((done, total));
            true
        })
        .unwrap();

    assert_eq!(done, pending);
    assert_eq!(memory.pending_embeddings().unwrap(), 0);
    assert_eq!(seen.last(), Some(&(2, 2)), "progress ends at the total");
}

#[test]
#[ignore = "needs the embedding model on disk"]
fn the_next_successful_index_retries_passages_left_by_an_earlier_model_failure() {
    let mut model = embedder().expect("set KUALI_EMBED_MODELS_DIR");
    let mut memory = Memory::in_memory().unwrap();

    // The first meeting reached SQLite while its model was unavailable.
    memory
        .index(&meeting(
            "failed-before",
            GUILD_WORK,
            &[(ANA, "Ana")],
            "la reunión quedó pendiente de vector",
        ))
        .unwrap();
    assert_eq!(memory.pending_embeddings().unwrap(), 1);

    // A later successful load must repair the earlier gap as well as embed the
    // new meeting, otherwise one pending passage disables the all-or-nothing
    // RAG indefinitely.
    memory
        .index_with(
            &meeting(
                "working-now",
                GUILD_WORK,
                &[(ANA, "Ana")],
                "el modelo volvió a estar disponible",
            ),
            Some(&mut model),
        )
        .unwrap();

    assert_eq!(memory.pending_embeddings().unwrap(), 0);
    assert_eq!(memory.embedded_passages().unwrap(), 2);
}

#[test]
fn a_discord_account_resolves_into_the_names_its_meetings_recorded() {
    let mut memory = Memory::in_memory().unwrap();
    memory
        .index(&meeting("one", GUILD_WORK, &[(ANA, "Ana")], "algo"))
        .unwrap();
    // The same account under a nickname it changed to later.
    let mut later = meeting("two", GUILD_WORK, &[(ANA, "Ana Ruiz")], "otra cosa");
    later.meta.started_at = Utc.with_ymd_and_hms(2026, 8, 9, 10, 0, 0).unwrap();
    memory.index(&later).unwrap();

    let names = memory.names_for_speaker(ANA).unwrap();

    // Both are real names for this person; the most recent leads.
    assert_eq!(names, vec!["Ana Ruiz".to_string(), "Ana".to_string()]);
    assert!(memory.names_for_speaker(999_999).unwrap().is_empty());
}

#[test]
fn a_browser_meeting_reports_the_participant_holding_the_microphone() {
    let mut memory = Memory::in_memory().unwrap();
    let mut web = meeting(
        "web",
        browser_identifier(1),
        &[
            (browser_identifier(50), "Garrux"),
            (browser_identifier(51), "Delphys"),
        ],
        "hablamos de algo",
    );
    web.speakers[0].source_id = Some("meet-device-1".into());
    web.speakers[0].is_self = true;
    web.speakers[1].source_id = Some("meet-device-2".into());
    memory.index(&web).unwrap();

    // No account links a browser meeting to anything, so the microphone is the
    // only thing that says which participant is the person running Kuali.
    assert_eq!(
        memory.known_self_names().unwrap(),
        vec!["Garrux".to_string()]
    );
}
