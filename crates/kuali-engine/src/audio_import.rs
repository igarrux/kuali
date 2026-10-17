//! Local file processing, separate from capture and its delivery automation.
//!
//! The diarizer works on PCM and timestamps; this coordinator supplies file
//! decoding, transcription, and a completed meeting for the library.

use std::collections::{BTreeMap, HashMap};
use std::hash::{Hash, Hasher};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use chrono::Utc;
use kuali_core::{browser_identifier, KualiConfig, Meeting, MeetingMeta, Speaker, Utterance};
use kuali_stt::diarization::{DiarizationOptions, Diarizer, SpeakerSegment};
use parking_lot::Mutex;
use serde::Serialize;

pub const CANCELLED: &str = "audio_import_cancelled";
const OVERLAP_SPEAKER: u32 = u32::MAX;

#[derive(Debug, Clone, Copy, Serialize)]
#[serde(rename_all = "camelCase")]
pub enum AudioImportStage {
    Downloading,
    Decoding,
    Diarizing,
    Transcribing,
    Saving,
    Analyzing,
}

#[derive(Debug, Clone, Copy, Serialize)]
pub struct AudioImportProgress {
    pub stage: AudioImportStage,
    pub done: u64,
    pub total: u64,
}

pub(crate) type Progress = Arc<dyn Fn(AudioImportProgress) + Send + Sync>;

struct ActiveJob {
    cancel: Arc<AtomicBool>,
    committing: bool,
}

#[derive(Default)]
pub(crate) struct AudioImportState {
    active: Mutex<Option<ActiveJob>>,
}

impl AudioImportState {
    pub(crate) fn begin(self: &Arc<Self>) -> Result<AudioImportGuard, String> {
        let mut active = self.active.lock();
        if active.is_some() {
            return Err(
                "Ya hay un archivo de audio en proceso. Espera a que termine o cancélalo.".into(),
            );
        }
        let cancel = Arc::new(AtomicBool::new(false));
        *active = Some(ActiveJob {
            cancel: Arc::clone(&cancel),
            committing: false,
        });
        Ok(AudioImportGuard {
            state: Arc::clone(self),
            cancel,
        })
    }

    pub(crate) fn is_active(&self) -> bool {
        self.active.lock().is_some()
    }

    pub(crate) fn cancel(&self) {
        if let Some(job) = self.active.lock().as_ref() {
            // Saving is the commit point. A late cancel must not report that a
            // successfully saved meeting disappeared.
            if !job.committing {
                job.cancel.store(true, Ordering::Release);
            }
        }
    }
}

pub(crate) struct AudioImportGuard {
    state: Arc<AudioImportState>,
    pub(crate) cancel: Arc<AtomicBool>,
}

impl AudioImportGuard {
    pub(crate) fn begin_commit(&self) -> Result<(), String> {
        let mut active = self.state.active.lock();
        check_cancelled(&self.cancel)?;
        if let Some(job) = active.as_mut() {
            job.committing = true;
        }
        Ok(())
    }
}

impl Drop for AudioImportGuard {
    fn drop(&mut self) {
        self.state.active.lock().take();
    }
}

fn check_cancelled(cancel: &AtomicBool) -> Result<(), String> {
    if cancel.load(Ordering::Acquire) {
        Err(CANCELLED.into())
    } else {
        Ok(())
    }
}

fn report(progress: &Progress, stage: AudioImportStage, done: u64, total: u64) {
    progress(AudioImportProgress { stage, done, total });
}

/// Runs entirely locally after any missing diarization weights are downloaded.
/// No meeting is persisted until this returns successfully.
pub(crate) async fn process(
    path: PathBuf,
    num_speakers: Option<u32>,
    config: KualiConfig,
    cancel: Arc<AtomicBool>,
    progress: Progress,
) -> Result<Meeting, String> {
    if num_speakers.is_some_and(|count| !(1..=32).contains(&count)) {
        return Err("El número de participantes debe estar entre 1 y 32.".into());
    }
    check_cancelled(&cancel)?;
    let models_dir = config.whisper.resolved_models_directory();
    if !kuali_stt::is_downloaded(&models_dir, config.whisper.model)
        || !kuali_stt::is_vad_downloaded(&models_dir)
    {
        return Err("Descarga el modelo de transcripción y su detector de voz desde Ajustes antes de importar audio.".into());
    }
    // Decode first so invalid files do not trigger an unnecessary model download.
    report(&progress, AudioImportStage::Decoding, 0, 0);
    let decode_cancel = Arc::clone(&cancel);
    let decode_progress = Arc::clone(&progress);
    let file_name = path
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("Audio importado")
        .to_string();
    let samples = tokio::task::spawn_blocking(move || {
        kuali_stt::audio_file::decode_audio_file(&path, &decode_cancel, |done, total| {
            report(&decode_progress, AudioImportStage::Decoding, done, total);
        })
        .map_err(|error| {
            if decode_cancel.load(Ordering::Acquire) {
                CANCELLED.to_string()
            } else {
                error.to_string()
            }
        })
    })
    .await
    .map_err(|error| format!("No se pudo decodificar el audio: {error}"))??;
    check_cancelled(&cancel)?;
    report(&progress, AudioImportStage::Downloading, 0, 0);
    kuali_stt::diarization::ensure_models(&models_dir, &cancel, |done, total| {
        report(&progress, AudioImportStage::Downloading, done, total);
    })
    .await
    .map_err(|error| {
        if cancel.load(Ordering::Acquire) {
            CANCELLED.to_string()
        } else {
            error.to_string()
        }
    })?;
    check_cancelled(&cancel)?;
    tokio::task::spawn_blocking(move || {
        let diarizer = Diarizer::load(&models_dir, DiarizationOptions { num_speakers })
            .map_err(|error| error.to_string())?;
        check_cancelled(&cancel)?;
        report(&progress, AudioImportStage::Diarizing, 0, 0);
        let segments = diarizer.diarize(&samples, 0, &cancel, |done, total| {
            report(&progress, AudioImportStage::Diarizing, done, total);
        }).map_err(|error| {
            if cancel.load(Ordering::Acquire) { CANCELLED.to_string() }
            else { error.to_string() }
        })?;
        drop(diarizer);
        check_cancelled(&cancel)?;
        let duration_ms = samples.len() as u64 * 1_000 / 16_000;
        let turns = transcription_turns(&segments, duration_ms);
        if turns.is_empty() {
            return Err("No se detectaron voces en este archivo de audio.".into());
        }
        let model_path = kuali_stt::model_path(&models_dir, config.whisper.model);
        report(&progress, AudioImportStage::Transcribing, 0, turns.len() as u64);
        let mut whisper = match kuali_stt::WhisperEngine::load(&model_path, &config.whisper) {
            Ok(whisper) => whisper,
            Err(error) if config.whisper.gpu => {
                tracing::warn!(%error, "import Whisper load failed with GPU; retrying on CPU");
                let mut cpu = config.whisper.clone();
                cpu.gpu = false;
                kuali_stt::WhisperEngine::load(&model_path, &cpu).map_err(|error| error.to_string())?
            }
            Err(error) => return Err(error.to_string()),
        };
        let mut meeting = imported_meeting(file_name, duration_ms, &segments, num_speakers);
        let mut contexts: HashMap<u32, (u64, String)> = HashMap::new();
        for (index, turn) in turns.iter().enumerate() {
            check_cancelled(&cancel)?;
            let start = (turn.start_ms as usize * 16).min(samples.len());
            let end = (turn.end_ms as usize * 16).min(samples.len());
            // Different overlaps can contain different people, so the mixed
            // group never inherits a previous conversation context.
            let previous = contexts.get(&turn.speaker_id)
                .filter(|_| turn.speaker_id != OVERLAP_SPEAKER)
                .filter(|(last_end, _)| turn.start_ms.saturating_sub(*last_end) <= 30_000)
                .map(|(_, text)| text.as_str());
            let transcription = whisper.transcribe_with_context(&samples[start..end], previous)
                .map_err(|error| error.to_string())?;
            check_cancelled(&cancel)?;
            if !transcription.is_empty() {
                let speaker_id = anonymous_id(&meeting.meta.id, turn.speaker_id);
                if turn.speaker_id == OVERLAP_SPEAKER && meeting.speaker(speaker_id).is_none() {
                    let mut speaker = Speaker::unknown(speaker_id);
                    speaker.display_name = "Voces superpuestas".into();
                    speaker.audio_kind = Some("overlap".into());
                    speaker.source_id = Some("overlap".into());
                    meeting.upsert_speaker(speaker);
                }
                if turn.speaker_id != OVERLAP_SPEAKER && transcription.confidence.unwrap_or(1.0) >= 0.45 {
                    let tail = transcription.text.chars().rev().take(1_200)
                        .collect::<String>().chars().rev().collect();
                    contexts.insert(turn.speaker_id, (turn.end_ms, tail));
                }
                meeting.upsert_utterance(Utterance {
                    id: format!("{}-segment-{index}", meeting.meta.id),
                    speaker_id,
                    start_ms: turn.start_ms,
                    end_ms: turn.end_ms,
                    text: transcription.text,
                    confidence: transcription.confidence,
                });
            }
            report(&progress, AudioImportStage::Transcribing, index as u64 + 1, turns.len() as u64);
        }
        check_cancelled(&cancel)?;
        if meeting.utterances.is_empty() {
            return Err("Se detectaron voces, pero no se pudo obtener una transcripción. Revisa el idioma de audio en Ajustes.".into());
        }
        Ok(meeting)
    }).await.map_err(|error| format!("No se pudo procesar el audio: {error}"))?
}

/// Schedule each speech sample once. Simultaneous voices form an explicitly
/// ambiguous group: diarization identifies activity, not isolated voice audio.
fn transcription_turns(segments: &[SpeakerSegment], duration_ms: u64) -> Vec<SpeakerSegment> {
    let mut boundaries: BTreeMap<u64, Vec<(u32, i32)>> = BTreeMap::new();
    for segment in segments {
        let start = segment.start_ms.min(duration_ms);
        let end = segment.end_ms.min(duration_ms);
        if start < end {
            boundaries
                .entry(start)
                .or_default()
                .push((segment.speaker_id, 1));
            boundaries
                .entry(end)
                .or_default()
                .push((segment.speaker_id, -1));
        }
    }
    let mut turns = Vec::new();
    let mut active: HashMap<u32, i32> = HashMap::new();
    let mut previous = 0;
    for (time, changes) in boundaries {
        let speaker = match active.len() {
            0 => None,
            1 => active.keys().next().copied(),
            _ => Some(OVERLAP_SPEAKER),
        };
        if let Some(speaker_id) = speaker {
            append_transcription_turn(&mut turns, speaker_id, previous, time);
        }
        for (speaker_id, change) in changes {
            let count = active.entry(speaker_id).or_default();
            *count += change;
            if *count == 0 {
                active.remove(&speaker_id);
            }
        }
        previous = time;
    }
    turns
}

fn append_transcription_turn(
    turns: &mut Vec<SpeakerSegment>,
    speaker_id: u32,
    mut start_ms: u64,
    end_ms: u64,
) {
    if let Some(last) = turns.last_mut() {
        if last.speaker_id == speaker_id && last.end_ms == start_ms {
            last.end_ms = end_ms.min(last.start_ms + 30_000);
            start_ms = last.end_ms;
        }
    }
    while start_ms < end_ms {
        let next = (start_ms + 30_000).min(end_ms);
        turns.push(SpeakerSegment {
            speaker_id,
            start_ms,
            end_ms: next,
        });
        start_ms = next;
    }
}

fn anonymous_id(meeting_id: &str, speaker: u32) -> u64 {
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    meeting_id.hash(&mut hasher);
    speaker.hash(&mut hasher);
    // Use the existing synthesized-ID namespace so Discord-scoped access can
    // never interpret an anonymous imported voice as a real account.
    browser_identifier(hasher.finish())
}

fn imported_meeting(
    file_name: String,
    duration_ms: u64,
    segments: &[SpeakerSegment],
    requested_speakers: Option<u32>,
) -> Meeting {
    let imported_at = Utc::now();
    let started_at = imported_at - chrono::Duration::milliseconds(duration_ms as i64);
    let id = uuid::Uuid::new_v4().to_string();
    let mut meeting = Meeting::new(MeetingMeta {
        channel_id: anonymous_id(&id, u32::MAX),
        id,
        display_title: Some(file_name.clone()),
        guild_id: browser_identifier(0x0041_5544_494f),
        guild_name: "Archivos de audio".into(),
        channel_name: file_name.clone(),
        started_at,
        ended_at: Some(imported_at),
        tags: Vec::new(),
        folder: None,
    });
    let mut speaker_turns = Vec::new();
    for segment in segments {
        let start_ms = segment.start_ms.min(duration_ms);
        let end_ms = segment.end_ms.min(duration_ms);
        if start_ms >= end_ms {
            continue;
        }
        let speaker_id = anonymous_id(&meeting.meta.id, segment.speaker_id);
        if meeting.speaker(speaker_id).is_none() {
            let mut speaker = Speaker::unknown(speaker_id);
            speaker.display_name = format!("Participante {}", meeting.speakers.len() + 1);
            speaker.audio_kind = Some("mixed".into());
            speaker.source_id = Some(format!("speaker-{}", segment.speaker_id));
            meeting.upsert_speaker(speaker);
        }
        speaker_turns.push(kuali_core::meeting::AudioSpeakerTurn {
            speaker_id,
            start_ms,
            end_ms,
        });
    }
    meeting.source = Some(kuali_core::meeting::MeetingSource::AudioFile {
        file_name,
        requested_speakers,
        imported_at: Some(imported_at),
        speaker_turns,
    });
    meeting
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cancelling_keeps_the_slot_until_the_worker_releases_it() {
        let state = Arc::new(AudioImportState::default());
        let job = state.begin().unwrap();
        state.cancel();
        assert_eq!(job.begin_commit().unwrap_err(), CANCELLED);
        assert!(state.begin().is_err());
        drop(job);
        assert!(state.begin().is_ok());
    }

    #[test]
    fn cancellation_cannot_turn_a_committed_save_into_a_cancelled_job() {
        let state = Arc::new(AudioImportState::default());
        let job = state.begin().unwrap();
        job.begin_commit().unwrap();
        state.cancel();
        assert!(!job.cancel.load(Ordering::Acquire));
        assert!(state.is_active());
    }

    #[test]
    fn simultaneous_speech_is_transcribed_once_without_fabricated_attribution() {
        let turns = transcription_turns(
            &[
                SpeakerSegment {
                    speaker_id: 3,
                    start_ms: 5_000,
                    end_ms: 70_000,
                },
                SpeakerSegment {
                    speaker_id: 7,
                    start_ms: 6_000,
                    end_ms: 8_000,
                },
            ],
            65_001,
        );
        assert_eq!(turns.len(), 4);
        assert_eq!(turns[1].speaker_id, OVERLAP_SPEAKER);
        assert_eq!((turns[1].start_ms, turns[1].end_ms), (6_000, 8_000));
        assert!(turns
            .windows(2)
            .all(|pair| pair[0].end_ms <= pair[1].start_ms));
        assert_eq!(
            turns
                .iter()
                .map(|turn| turn.end_ms - turn.start_ms)
                .sum::<u64>(),
            60_001
        );
        let long: Vec<_> = turns.iter().filter(|turn| turn.speaker_id == 3).collect();
        assert_eq!(long[0].start_ms, 5_000);
        assert_eq!(long[2].end_ms, 65_001);
        assert!(long
            .iter()
            .all(|turn| turn.end_ms - turn.start_ms <= 30_000));
        assert_eq!(long[0].end_ms, 6_000);
        assert_eq!(long[1].start_ms, 8_000);
        assert_eq!(long[1].end_ms, long[2].start_ms);
    }

    #[test]
    fn imported_sources_roundtrip_and_legacy_records_remain_readable() {
        let meeting = imported_meeting("entrevista.wav".into(), 10_000, &[], Some(4));
        assert!(kuali_core::is_browser_identifier(anonymous_id(
            &meeting.meta.id,
            0
        )));
        let mut json = serde_json::to_value(&meeting).unwrap();
        assert_eq!(json["source"]["kind"], "audioFile");
        assert_eq!(json["source"]["fileName"], "entrevista.wav");
        assert_eq!(json["source"]["requestedSpeakers"], 4);
        let roundtrip: Meeting = serde_json::from_value(json.clone()).unwrap();
        assert_eq!(roundtrip.source, meeting.source);
        json["source"]
            .as_object_mut()
            .unwrap()
            .remove("speakerTurns");
        json["source"].as_object_mut().unwrap().remove("importedAt");
        json["source"]
            .as_object_mut()
            .unwrap()
            .remove("requestedSpeakers");
        let older_import: Meeting = serde_json::from_value(json.clone()).unwrap();
        assert!(
            matches!(older_import.source, Some(kuali_core::meeting::MeetingSource::AudioFile { imported_at: None, requested_speakers: None, speaker_turns, .. }) if speaker_turns.is_empty())
        );
        json.as_object_mut().unwrap().remove("source");
        let legacy: Meeting = serde_json::from_value(json).unwrap();
        assert_eq!(legacy.source, None);
    }

    #[test]
    fn raw_overlap_and_all_real_speakers_survive_before_any_transcription() {
        let segments = vec![
            SpeakerSegment {
                speaker_id: 0,
                start_ms: 0,
                end_ms: 10_000,
            },
            SpeakerSegment {
                speaker_id: 1,
                start_ms: 5_000,
                end_ms: 8_000,
            },
        ];
        let meeting = imported_meeting("dos-voces.wav".into(), 10_000, &segments, None);
        assert_eq!(meeting.speakers.len(), 2);
        assert!(meeting.utterances.is_empty());
        let Some(kuali_core::meeting::MeetingSource::AudioFile {
            speaker_turns,
            imported_at,
            ..
        }) = &meeting.source
        else {
            panic!("missing source")
        };
        assert_eq!(speaker_turns.len(), 2);
        assert_eq!(speaker_turns[0].end_ms, 10_000);
        assert_eq!(speaker_turns[1].start_ms, 5_000);
        assert_eq!(speaker_turns[1].end_ms, 8_000);
        assert_ne!(speaker_turns[0].speaker_id, speaker_turns[1].speaker_id);
        assert_eq!(meeting.meta.ended_at, *imported_at);
        assert!(meeting.meta.ended_at.unwrap() <= Utc::now());
    }

    #[test]
    fn requested_count_preserves_actual_voices_without_fabricating_missing_speakers() {
        let segments: Vec<_> = (0..3)
            .map(|speaker_id| SpeakerSegment {
                speaker_id,
                start_ms: u64::from(speaker_id) * 1_000,
                end_ms: u64::from(speaker_id + 1) * 1_000,
            })
            .collect();
        let meeting = imported_meeting("reunion.wav".into(), 3_000, &segments, Some(4));
        assert_eq!(meeting.speakers.len(), 3);
        let json = serde_json::to_value(&meeting).unwrap();
        assert_eq!(json["source"]["requestedSpeakers"], 4);
        assert_eq!(json["source"]["speakerTurns"].as_array().unwrap().len(), 3);
        let reloaded: Meeting = serde_json::from_value(json).unwrap();
        assert_eq!(reloaded, meeting);

        let automatic = imported_meeting("reunion.wav".into(), 3_000, &segments, None);
        let json = serde_json::to_value(automatic).unwrap();
        assert!(json["source"].get("requestedSpeakers").is_none());
    }

    #[test]
    fn duplicate_ranges_for_one_speaker_do_not_invent_an_overlap_group() {
        let turns = transcription_turns(
            &[
                SpeakerSegment {
                    speaker_id: 0,
                    start_ms: 0,
                    end_ms: 5_000,
                },
                SpeakerSegment {
                    speaker_id: 0,
                    start_ms: 2_000,
                    end_ms: 8_000,
                },
            ],
            8_000,
        );
        assert_eq!(
            turns,
            vec![SpeakerSegment {
                speaker_id: 0,
                start_ms: 0,
                end_ms: 8_000
            }]
        );
    }
}
