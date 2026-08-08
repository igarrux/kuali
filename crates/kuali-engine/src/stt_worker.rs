//! Whisper runs on a dedicated thread.
//!
//! Transcription blocks for hundreds of milliseconds and whisper.cpp retains
//! mutable state between calls, making async-runtime execution and task sharing
//! unsafe. A dedicated queued thread serializes requests without blocking Kuali.
//!
//! The thread stays alive between calls, but the model does not remain loaded.

use std::collections::HashMap;
use std::path::PathBuf;

use kuali_core::{DiscordUserId, WhisperConfig};
use kuali_stt::{SttError, Transcription, WhisperEngine};
use tokio::sync::{mpsc, oneshot};

enum Command {
    Load {
        path: PathBuf,
        config: Box<WhisperConfig>,
        reply: oneshot::Sender<Result<(), SttError>>,
    },
    Transcribe {
        speaker_id: DiscordUserId,
        start_ms: u64,
        end_ms: u64,
        samples: Vec<f32>,
        pass: TranscriptionPass,
        overlap_with_previous: bool,
        reply: oneshot::Sender<Result<Transcription, SttError>>,
    },
    Unload {
        reply: oneshot::Sender<()>,
    },
}

#[derive(Debug, Default)]
struct SpeakerContext {
    text: String,
    last_end_ms: u64,
}

pub(crate) type PendingTranscription = oneshot::Receiver<Result<Transcription, SttError>>;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum TranscriptionPass {
    Preview,
    LiveFinal,
}

impl TranscriptionPass {
    fn commits_context(self) -> bool {
        matches!(self, Self::LiveFinal)
    }
}

#[derive(Debug, thiserror::Error)]
pub enum WorkerError {
    #[error("transcription worker is unavailable")]
    Gone,
    #[error("no model is loaded")]
    NotLoaded,
    #[error(transparent)]
    Stt(#[from] SttError),
}

#[derive(Clone)]
pub struct SttWorker {
    tx: mpsc::UnboundedSender<Command>,
}

impl SttWorker {
    pub fn spawn() -> Self {
        let (tx, mut rx) = mpsc::unbounded_channel::<Command>();

        std::thread::Builder::new()
            .name("kuali-whisper".into())
            .spawn(move || {
                let mut engine: Option<WhisperEngine> = None;
                let mut speaker_contexts: HashMap<DiscordUserId, SpeakerContext> = HashMap::new();

                while let Some(command) = rx.blocking_recv() {
                    match command {
                        Command::Load {
                            path,
                            config,
                            reply,
                        } => {
                            // Drop the previous model before loading another to
                            // avoid holding two large models in RAM.
                            engine = None;
                            let result = WhisperEngine::load(&path, &config).map(|loaded| {
                                engine = Some(loaded);
                                speaker_contexts.clear();
                            });
                            let _ = reply.send(result);
                        }
                        Command::Transcribe {
                            speaker_id,
                            start_ms,
                            end_ms,
                            samples,
                            pass,
                            overlap_with_previous,
                            reply,
                        } => {
                            const CONTEXT_RESET_GAP_MS: u64 = 30_000;
                            let expired =
                                speaker_contexts.get(&speaker_id).is_some_and(|context| {
                                    start_ms.saturating_sub(context.last_end_ms)
                                        > CONTEXT_RESET_GAP_MS
                                });
                            if expired {
                                speaker_contexts.remove(&speaker_id);
                            }
                            let previous = speaker_contexts
                                .get(&speaker_id)
                                .map(|context| context.text.clone());
                            let result = match engine.as_mut() {
                                Some(engine) => {
                                    engine.transcribe_with_context(&samples, previous.as_deref())
                                }
                                None => Err(SttError::Transcribe("no model is loaded".into())),
                            };
                            let result = result.map(|mut transcription| {
                                if overlap_with_previous {
                                    if let Some(previous) = previous.as_deref() {
                                        transcription.text =
                                            remove_prompt_overlap(previous, &transcription.text);
                                    }
                                }
                                if pass.commits_context() {
                                    let context = speaker_contexts.entry(speaker_id).or_default();
                                    context.last_end_ms = end_ms;
                                    if !transcription.is_empty()
                                        && transcription.confidence.unwrap_or(1.0) >= 0.45
                                    {
                                        append_context(&mut context.text, &transcription.text);
                                    }
                                }
                                transcription
                            });
                            let _ = reply.send(result);
                        }
                        Command::Unload { reply } => {
                            engine = None;
                            speaker_contexts.clear();
                            tracing::debug!("Whisper model unloaded from memory");
                            // Confirm only after dropping the engine, not merely
                            // after enqueueing the command.
                            let _ = reply.send(());
                        }
                    }
                }
            })
            .expect("failed to create the transcription worker");

        Self { tx }
    }

    pub async fn load(&self, path: PathBuf, config: &WhisperConfig) -> Result<(), WorkerError> {
        let (reply, rx) = oneshot::channel();
        self.tx
            .send(Command::Load {
                path,
                config: Box::new(config.clone()),
                reply,
            })
            .map_err(|_| WorkerError::Gone)?;
        rx.await.map_err(|_| WorkerError::Gone)??;
        Ok(())
    }

    /// Enqueues work synchronously. Separating enqueue from response waiting
    /// preserves exact capture order without blocking audio reception.
    pub(crate) fn enqueue_transcription(
        &self,
        speaker_id: DiscordUserId,
        start_ms: u64,
        end_ms: u64,
        samples: Vec<f32>,
        pass: TranscriptionPass,
        overlap_with_previous: bool,
    ) -> Result<PendingTranscription, WorkerError> {
        let (reply, rx) = oneshot::channel();
        self.tx
            .send(Command::Transcribe {
                speaker_id,
                start_ms,
                end_ms,
                samples,
                pass,
                overlap_with_previous,
                reply,
            })
            .map_err(|_| WorkerError::Gone)?;
        Ok(rx)
    }

    pub(crate) async fn resolve_transcription(
        pending: PendingTranscription,
    ) -> Result<Transcription, WorkerError> {
        Ok(pending.await.map_err(|_| WorkerError::Gone)??)
    }

    /// Releases model RAM when leaving the final active call.
    pub async fn unload(&self) -> Result<(), WorkerError> {
        let (reply, rx) = oneshot::channel();
        self.tx
            .send(Command::Unload { reply })
            .map_err(|_| WorkerError::Gone)?;
        rx.await.map_err(|_| WorkerError::Gone)?;
        Ok(())
    }
}

fn append_context(context: &mut String, text: &str) {
    const MAX_CONTEXT_CHARS: usize = 1_200;
    if !context.is_empty() {
        context.push(' ');
    }
    context.push_str(text.trim());
    let chars = context.chars().count();
    if chars > MAX_CONTEXT_CHARS {
        *context = context.chars().skip(chars - MAX_CONTEXT_CHARS).collect();
        if let Some(first_space) = context.find(char::is_whitespace) {
            context.drain(..=first_space);
        }
    }
}

/// Removes a prefix already present at the end of the previous segment. This is
/// limited to known audio overlap so natural conversational repetition remains.
fn remove_prompt_overlap(previous: &str, current: &str) -> String {
    let previous_words = previous.split_whitespace().collect::<Vec<_>>();
    let current_words = current.split_whitespace().collect::<Vec<_>>();
    let max = previous_words.len().min(current_words.len()).min(20);

    for count in (1..=max).rev() {
        let left = &previous_words[previous_words.len() - count..];
        let right = &current_words[..count];
        let matches = left
            .iter()
            .zip(right)
            .all(|(a, b)| normalize_word(a) == normalize_word(b));
        let strong_enough = count >= 2
            || right
                .first()
                .is_some_and(|word| normalize_word(word).chars().count() >= 5);
        if matches && strong_enough {
            return current_words[count..].join(" ");
        }
    }
    current.trim().to_string()
}

fn normalize_word(word: &str) -> String {
    word.to_lowercase()
        .chars()
        .filter(|character| character.is_alphanumeric())
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn transcribing_without_a_model_fails_instead_of_hanging() {
        let worker = SttWorker::spawn();
        // Without an earlier `load`, return an error instead of waiting forever.
        let pending = worker
            .enqueue_transcription(
                1,
                0,
                1_000,
                vec![0.0; 16_000],
                TranscriptionPass::LiveFinal,
                false,
            )
            .unwrap();
        let result = SttWorker::resolve_transcription(pending).await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn unloading_an_empty_worker_is_harmless() {
        let worker = SttWorker::spawn();
        worker.unload().await.unwrap();
        worker.unload().await.unwrap();
    }

    #[tokio::test]
    async fn loading_a_model_that_does_not_exist_reports_an_error() {
        let worker = SttWorker::spawn();
        let result = worker
            .load(
                PathBuf::from("/no/existe/modelo.bin"),
                &WhisperConfig::default(),
            )
            .await;
        assert!(matches!(result, Err(WorkerError::Stt(_))));
    }

    #[test]
    fn only_known_audio_overlap_is_removed() {
        assert_eq!(
            remove_prompt_overlap(
                "vamos a revisar WaitingRoom mañana",
                "WaitingRoom mañana con el equipo"
            ),
            "con el equipo"
        );
        assert_eq!(
            remove_prompt_overlap("sí", "sí, eso mismo"),
            "sí, eso mismo"
        );
        assert_eq!(
            remove_prompt_overlap("probamos Redis", "Redis funciona"),
            "funciona"
        );
    }

    #[test]
    fn speaker_context_keeps_only_a_recent_tail() {
        let mut context = "a".repeat(1_500);
        append_context(&mut context, "palabras recientes");
        assert!(context.chars().count() <= 1_200);
        assert!(context.ends_with("palabras recientes"));
    }
}
