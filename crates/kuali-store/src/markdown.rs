//! Markdown meeting representation for portable sharing.

use kuali_core::{format_timestamp, Meeting};

pub fn render(meeting: &Meeting) -> String {
    let mut out = String::new();

    out.push_str(&format!("# {}\n\n", meeting.meta.title()));
    out.push_str(&format!(
        "**Fecha:** {}  \n",
        meeting.meta.started_at.format("%Y-%m-%d %H:%M UTC")
    ));
    out.push_str(&format!(
        "**Duración:** {}  \n",
        format_timestamp(meeting.duration_ms())
    ));

    let participants: Vec<_> = meeting
        .speakers
        .iter()
        .filter(|s| !s.is_bot)
        .map(|s| s.display_name.as_str())
        .collect();
    if !participants.is_empty() {
        out.push_str(&format!("**Participantes:** {}\n", participants.join(", ")));
    }
    out.push('\n');

    match &meeting.summary {
        Some(summary) => {
            if !summary.overview.trim().is_empty() {
                out.push_str("## Resumen\n\n");
                out.push_str(summary.overview.trim());
                out.push_str("\n\n");
            }

            if !summary.action_items.is_empty() {
                out.push_str("## Tareas pendientes\n\n");
                for task in &summary.action_items {
                    let mark = if task.done { "x" } else { " " };
                    out.push_str(&format!("- [{mark}] {}", task.text));

                    let mut notes = Vec::new();
                    if let Some(assignee) = &task.assignee {
                        notes.push(format!("**{assignee}**"));
                    }
                    if let Some(due) = &task.due {
                        notes.push(due.clone());
                    }
                    if let Some(ms) = task.source_ms {
                        notes.push(format!("`{}`", format_timestamp(ms)));
                    }
                    if !notes.is_empty() {
                        out.push_str(&format!(" — {}", notes.join(" · ")));
                    }
                    out.push('\n');
                }
                out.push('\n');
            }

            bullet_section(&mut out, "Decisiones", &summary.decisions);
            bullet_section(&mut out, "Puntos clave", &summary.key_points);
            bullet_section(&mut out, "Preguntas abiertas", &summary.open_questions);

            if !summary.generated_by.trim().is_empty() {
                out.push_str(&format!(
                    "<sub>Resumen generado por {}</sub>\n\n",
                    summary.generated_by
                ));
            }
        }
        None => {
            out.push_str("> Todavía no hay resumen para esta reunión.\n\n");
        }
    }

    out.push_str("---\n\n## Transcripción\n\n");
    if meeting.utterances.is_empty() {
        out.push_str("_No se transcribió nada._\n");
        return out;
    }

    for utterance in &meeting.utterances {
        out.push_str(&format!(
            "**[{}] {}:** {}\n\n",
            format_timestamp(utterance.start_ms),
            meeting.speaker_name(utterance.speaker_id),
            utterance.text.trim()
        ));
    }
    out
}

/// Concise Discord summary designed to fit in chat without dumping the transcript.
pub fn render_for_discord(meeting: &Meeting) -> String {
    let Some(summary) = &meeting.summary else {
        return format!("**{}** — sin resumen disponible.", meeting.meta.title());
    };

    let mut out = format!(
        "**{}**\n-# {} · {} participantes\n\n",
        meeting.meta.title(),
        format_timestamp(meeting.duration_ms()),
        meeting.speakers.iter().filter(|s| !s.is_bot).count()
    );

    if !summary.overview.trim().is_empty() {
        out.push_str(summary.overview.trim());
        out.push_str("\n\n");
    }

    if !summary.key_points.is_empty() {
        out.push_str("**Puntos clave**\n");
        for point in &summary.key_points {
            out.push_str(&format!("- {point}\n"));
        }
        out.push('\n');
    }

    if summary.action_items.is_empty() {
        out.push_str("**Tareas:** ninguna.\n");
    } else {
        out.push_str("**Tareas pendientes**\n");
        for task in &summary.action_items {
            match &task.assignee {
                Some(assignee) => out.push_str(&format!("- {} — **{}**\n", task.text, assignee)),
                None => out.push_str(&format!("- {}\n", task.text)),
            }
        }
    }

    if !summary.decisions.is_empty() {
        out.push_str("\n**Decisiones**\n");
        for decision in &summary.decisions {
            out.push_str(&format!("- {decision}\n"));
        }
    }

    if !summary.open_questions.is_empty() {
        out.push_str("\n**Preguntas abiertas**\n");
        for question in &summary.open_questions {
            out.push_str(&format!("- {question}\n"));
        }
    }

    out
}

fn bullet_section(out: &mut String, title: &str, items: &[String]) {
    if items.is_empty() {
        return;
    }
    out.push_str(&format!("## {title}\n\n"));
    for item in items {
        out.push_str(&format!("- {item}\n"));
    }
    out.push('\n');
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::{TimeZone, Utc};
    use kuali_core::{color_for, ActionItem, MeetingMeta, MeetingSummary, Speaker, Utterance};

    fn meeting(with_summary: bool) -> Meeting {
        let mut m = Meeting::new(MeetingMeta {
            id: "m1".into(),
            display_title: None,
            guild_id: 1,
            guild_name: "Equipo".into(),
            channel_id: 2,
            channel_name: "Daily".into(),
            started_at: Utc.with_ymd_and_hms(2026, 8, 6, 9, 0, 0).unwrap(),
            ended_at: None,
        });
        m.upsert_speaker(Speaker {
            user_id: 10,
            source_id: None,
            audio_kind: None,
            display_name: "Ana".into(),
            username: "ana".into(),
            avatar_url: None,
            color: color_for(10).to_string(),
            is_bot: false,
        });
        m.upsert_speaker(Speaker {
            user_id: 99,
            source_id: None,
            audio_kind: None,
            display_name: "Kuali".into(),
            username: "kuali".into(),
            avatar_url: None,
            color: color_for(99).to_string(),
            is_bot: true,
        });
        m.push_utterance(Utterance {
            id: "u1".into(),
            speaker_id: 10,
            start_ms: 5_000,
            end_ms: 8_000,
            text: "Preparo la demo para el viernes".into(),
            confidence: Some(0.95),
        });

        if with_summary {
            m.summary = Some(MeetingSummary {
                title: "Repaso del sprint".into(),
                overview: "Repaso rápido del sprint.".into(),
                key_points: vec!["La demo va justa".into()],
                decisions: vec!["Recortamos alcance".into()],
                action_items: vec![ActionItem {
                    id: "t1".into(),
                    text: "Preparar la demo".into(),
                    assignee: Some("Ana".into()),
                    due: Some("el viernes".into()),
                    source_ms: Some(5_000),
                    done: false,
                }],
                open_questions: vec!["¿Quién presenta?".into()],
                generated_by: "Claude Code · sonnet".into(),
            });
        }
        m
    }

    #[test]
    fn markdown_carries_every_section_of_the_summary() {
        let md = render(&meeting(true));
        assert!(md.contains("# Equipo · Daily"));
        assert!(md.contains("2026-08-06 09:00 UTC"));
        assert!(md.contains("## Resumen"));
        assert!(md.contains("## Tareas pendientes"));
        assert!(md.contains("- [ ] Preparar la demo — **Ana** · el viernes · `00:05`"));
        assert!(md.contains("## Decisiones"));
        assert!(md.contains("## Preguntas abiertas"));
        assert!(md.contains("Claude Code · sonnet"));
    }

    #[test]
    fn the_bot_is_not_listed_as_a_participant() {
        let md = render(&meeting(true));
        let participants = md
            .lines()
            .find(|l| l.starts_with("**Participantes:**"))
            .expect("participants should be listed");
        assert!(participants.contains("Ana"));
        assert!(
            !participants.contains("Kuali"),
            "Kuali no participa, escucha"
        );
    }

    #[test]
    fn the_transcript_is_attributed_and_timestamped() {
        let md = render(&meeting(true));
        assert!(md.contains("**[00:05] Ana:** Preparo la demo para el viernes"));
    }

    #[test]
    fn a_meeting_without_a_summary_still_renders_its_transcript() {
        let md = render(&meeting(false));
        assert!(md.contains("Todavía no hay resumen"));
        assert!(md.contains("## Transcripción"));
        assert!(md.contains("Ana"));
    }

    #[test]
    fn a_completed_task_shows_as_ticked() {
        let mut m = meeting(true);
        m.summary.as_mut().unwrap().action_items[0].done = true;
        assert!(render(&m).contains("- [x] Preparar la demo"));
    }

    #[test]
    fn the_discord_version_leaves_the_transcript_out() {
        let text = render_for_discord(&meeting(true));
        assert!(text.starts_with("**Equipo · Daily**\n-# "));
        assert!(!text.contains("##"));
        assert!(!text.contains("📋"));
        assert!(text.contains("Preparar la demo"));
        assert!(text.contains("**Ana**"));
        assert!(text.contains("**Puntos clave**"));
        assert!(text.contains("La demo va justa"));
        assert!(text.contains("**Preguntas abiertas**"));
        assert!(text.contains("¿Quién presenta?"));
        assert!(!text.contains("ID de la reunión"));
        assert!(!text.contains("/transcription"));
        assert!(
            !text.contains("Preparo la demo para el viernes"),
            "the transcript should not be included in chat output"
        );
    }

    #[test]
    fn the_discord_version_says_so_when_there_are_no_tasks() {
        let mut m = meeting(true);
        m.summary.as_mut().unwrap().action_items.clear();
        assert!(render_for_discord(&m).contains("ninguna"));
    }
}
