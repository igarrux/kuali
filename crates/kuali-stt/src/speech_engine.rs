//! The recognition engine that runs the configured model.

use std::path::Path;

use kuali_core::{SpeechEngineKind, WhisperConfig};

use crate::gigaam::GigaAmEngine;
use crate::whisper::{SttError, Transcription, WhisperEngine};

/// A loaded recognizer for one of the configured model's engines.
pub enum SpeechEngine {
    Whisper(WhisperEngine),
    GigaAm(GigaAmEngine),
}

impl SpeechEngine {
    /// Loads `path` with the engine that runs `config.model`.
    pub fn load(path: &Path, config: &WhisperConfig) -> Result<Self, SttError> {
        match config.model.engine() {
            SpeechEngineKind::Whisper => WhisperEngine::load(path, config).map(Self::Whisper),
            SpeechEngineKind::GigaAm => GigaAmEngine::load(path, config).map(Self::GigaAm),
        }
    }

    /// Transcribes a 16 kHz mono audio fragment.
    ///
    /// Blocking operation invoked from a dedicated thread, never the async runtime.
    pub fn transcribe(&mut self, samples: &[f32]) -> Result<Transcription, SttError> {
        self.transcribe_with_context(samples, None)
    }

    /// Transcribes with this speaker's recent text as context. Whisper uses it
    /// as a prompt; GigaAM takes no prompt.
    pub fn transcribe_with_context(
        &mut self,
        samples: &[f32],
        previous_text: Option<&str>,
    ) -> Result<Transcription, SttError> {
        match self {
            Self::Whisper(engine) => engine.transcribe_with_context(samples, previous_text),
            Self::GigaAm(engine) => engine.transcribe(samples),
        }
    }
}
