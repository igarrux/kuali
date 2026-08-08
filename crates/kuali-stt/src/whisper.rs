//! Transcription engine wrapping whisper.cpp.
//!
//! The model loads when Kuali enters a call and is released after leaving, so it
//! consumes no RAM between meetings.

use std::collections::HashSet;
use std::path::Path;

use kuali_core::WhisperConfig;
use whisper_rs::{
    FullParams, SamplingStrategy, WhisperContext, WhisperContextParameters, WhisperState,
    WhisperTokenId, WhisperVadContext, WhisperVadContextParams, WhisperVadParams,
};

use crate::audio::{ms_to_samples, WHISPER_SAMPLE_RATE};

#[derive(Debug, thiserror::Error)]
pub enum SttError {
    #[error("failed to load the Whisper model: {0}")]
    Load(String),
    #[error("transcription failed: {0}")]
    Transcribe(String),
    #[error("model path is not valid text: {0}")]
    BadPath(String),
}

#[derive(Debug, Clone, PartialEq)]
pub struct Transcription {
    pub text: String,
    /// Approximate confidence from 0.0 to 1.0 derived from the model's certainty
    /// that speech was present.
    pub confidence: Option<f32>,
}

impl Transcription {
    pub fn is_empty(&self) -> bool {
        self.text.trim().is_empty()
    }
}

/// Whisper hallucinates frequent training-caption phrases over silence or noise.
/// These recurring outputs are compared in lowercase without punctuation or
/// apostrophes, matching normalized text such as `well be right back`.
const HALLUCINATIONS: &[&str] = &[
    "subtítulos realizados por la comunidad de amaraorg",
    "subtítulos por la comunidad de amaraorg",
    "subtitulos realizados por la comunidad de amaraorg",
    "más información en wwwaraorg",
    "gracias por ver el video",
    "gracias por ver el vídeo",
    "no olvides suscribirte",
    "suscríbete al canal",
    "hasta la próxima",
    "thanks for watching",
    "thank you for watching",
    "thank you",
    "thank you sir",
    "thanks",
    "please subscribe to my channel",
    "subscribe to my channel",
    "well be right back",
    "ill see you next time",
    "see you",
    "goodbye",
    "bye",
    "you",
    "amén",
    "gracias",
];

/// whisper.cpp uses 30-second windows and handles very short input poorly, so
/// fragments are padded with silence to this minimum.
const MIN_INPUT_MS: u64 = 1_000;
const MAX_VOCABULARY_TERMS: usize = 64;
const MAX_VOCABULARY_TERM_CHARS: usize = 80;
const MAX_VOCABULARY_PROMPT_CHARS: usize = 1_024;
const MAX_VOCABULARY_PROMPT_TOKENS: usize = 223;

pub struct WhisperEngine {
    state: WhisperState,
    /// Language code or `auto` for Whisper detection. Never empty: whisper.cpp
    /// recognizes `auto` and detects before transcribing.
    language: String,
    /// Characters expected from the configured language. `None` when its
    /// alphabet is unknown and cannot be checked.
    allowed_letters: Option<HashSet<char>>,
    translate: bool,
    threads: i32,
    vocabulary_prompt_tokens: Vec<WhisperTokenId>,
    /// Silero is a gate before Whisper rather than an optional post-processing
    /// filter. It loads once with the primary model.
    vad: WhisperVadContext,
    /// Retained so the context lives as long as its state.
    _context: WhisperContext,
}

impl WhisperEngine {
    pub fn load(path: &Path, config: &WhisperConfig) -> Result<Self, SttError> {
        let path_str = path
            .to_str()
            .ok_or_else(|| SttError::BadPath(path.display().to_string()))?;

        let mut params = WhisperContextParameters::default();
        params.use_gpu(config.gpu);

        let context = WhisperContext::new_with_params(path_str, params)
            .map_err(|e| SttError::Load(e.to_string()))?;

        let language = match config.language.trim() {
            "" => "auto".to_string(),
            lang => lang.to_lowercase(),
        };

        let mut vocabulary_prompt_tokens =
            match build_vocabulary_prompt(&config.custom_vocabulary, &language) {
                Some(prompt) => context
                    .tokenize(&prompt, 8_192)
                    .map_err(|e| SttError::Load(format!("vocabulario personalizado: {e}")))?,
                None => Vec::new(),
            };
        vocabulary_prompt_tokens.truncate(MAX_VOCABULARY_PROMPT_TOKENS);
        let state = context
            .create_state()
            .map_err(|e| SttError::Load(e.to_string()))?;

        let vad_path = crate::model::vad_model_path(&config.resolved_models_directory());
        let vad_path = vad_path
            .to_str()
            .ok_or_else(|| SttError::BadPath(vad_path.display().to_string()))?;
        let mut vad_params = WhisperVadContextParams::default();
        vad_params.set_n_threads(1);
        vad_params.set_use_gpu(false);
        let vad = WhisperVadContext::new(vad_path, vad_params)
            .map_err(|e| SttError::Load(format!("Silero VAD: {e}")))?;

        Ok(Self {
            state,
            allowed_letters: allowed_letters(&language, &config.custom_vocabulary),
            language,
            translate: config.translate,
            threads: resolve_threads(config.threads),
            vocabulary_prompt_tokens,
            vad,
            _context: context,
        })
    }

    /// Transcribes a 16 kHz mono audio fragment.
    ///
    /// Blocking operation invoked from a dedicated thread, never the async runtime.
    pub fn transcribe(&mut self, samples: &[f32]) -> Result<Transcription, SttError> {
        self.transcribe_with_context(samples, None)
    }

    pub fn transcribe_with_context(
        &mut self,
        samples: &[f32],
        previous_text: Option<&str>,
    ) -> Result<Transcription, SttError> {
        // The segmenter's RMS threshold only decides when to form a block.
        // Silero decides whether it contains human speech on every pass, keeping
        // keyboards, fans, and music out of even live draft decoding.
        let vad_input;
        let samples = if samples.len() < ms_to_samples(MIN_INPUT_MS) {
            let mut padded = samples.to_vec();
            padded.resize(ms_to_samples(MIN_INPUT_MS), 0.0);
            vad_input = padded;
            &vad_input[..]
        } else {
            samples
        };
        let vad_samples = self.speech_only(samples)?;
        if vad_samples.is_empty() {
            return Ok(Transcription {
                text: String::new(),
                confidence: None,
            });
        };
        let samples = &vad_samples[..];

        let padded;
        let input = if samples.len() < ms_to_samples(MIN_INPUT_MS) {
            let mut buf = samples.to_vec();
            buf.resize(ms_to_samples(MIN_INPUT_MS), 0.0);
            padded = buf;
            &padded[..]
        } else {
            samples
        };

        let mut params = FullParams::new(SamplingStrategy::Greedy { best_of: 1 });
        params.set_n_threads(self.threads);
        params.set_translate(self.translate);

        // With `auto`, whisper.cpp detects the language and continues decoding.
        // `detect_language` instead returns after detection without transcribing,
        // which would leave the entire meeting blank.
        params.set_language(Some(&self.language));
        let prompt_tokens = self.prompt_tokens(previous_text)?;
        if !prompt_tokens.is_empty() {
            params.set_tokens(&prompt_tokens);
        }

        // The worker interleaves fragments from many participants. whisper.cpp's
        // implicit state would mix them, so inject explicit per-speaker context.
        params.set_no_context(true);
        params.set_suppress_blank(true);
        // Suppress non-speech annotations such as `(music)` and `[laughter]`.
        params.set_suppress_nst(true);
        params.set_temperature(0.0);
        params.set_no_speech_thold(0.6);
        // Keep `temperature_inc`. Disabling higher-temperature retries appears
        // helpful for hallucinations but performs worse on difficult fragments,
        // producing long repeated-phrase loops.

        // whisper.cpp writes to stdout unless explicitly suppressed.
        params.set_print_special(false);
        params.set_print_progress(false);
        params.set_print_realtime(false);
        params.set_print_timestamps(false);

        self.state
            .full(params, input)
            .map_err(|e| SttError::Transcribe(e.to_string()))?;

        let mut text = String::new();
        let mut speech_confidence: Vec<f32> = Vec::new();

        for segment in self.state.as_iter() {
            let chunk = segment
                .to_str_lossy()
                .map_err(|e| SttError::Transcribe(e.to_string()))?;
            let chunk = chunk.trim();
            if chunk.is_empty() {
                continue;
            }
            if !text.is_empty() {
                text.push(' ');
            }
            text.push_str(chunk);
            speech_confidence.push(1.0 - segment.no_speech_probability());
        }

        let confidence = if speech_confidence.is_empty() {
            None
        } else {
            Some(speech_confidence.iter().sum::<f32>() / speech_confidence.len() as f32)
        };

        let text = text.trim().to_string();
        if is_hallucination(&text) || speaks_another_language(&text, self.allowed_letters.as_ref())
        {
            return Ok(Transcription {
                text: String::new(),
                confidence,
            });
        }

        Ok(Transcription { text, confidence })
    }

    /// Combines fixed vocabulary with the end of this speaker's previous text.
    /// It never uses whisper.cpp's implicit state because the worker interleaves
    /// many speakers and global context would mix them.
    fn prompt_tokens(&self, previous_text: Option<&str>) -> Result<Vec<WhisperTokenId>, SttError> {
        let mut tokens = self.vocabulary_prompt_tokens.clone();
        let Some(previous_text) = previous_text.filter(|text| !text.trim().is_empty()) else {
            return Ok(tokens);
        };

        let mut previous = self
            ._context
            .tokenize(previous_text, 8_192)
            .map_err(|e| SttError::Transcribe(format!("speaker context: {e}")))?;
        let available = MAX_VOCABULARY_PROMPT_TOKENS.saturating_sub(tokens.len());
        if previous.len() > available {
            previous.drain(..previous.len() - available);
        }
        tokens.extend(previous);
        Ok(tokens)
    }

    /// Silero removes anything it does not recognize as speech and preserves
    /// padding around accepted regions. VAD failures propagate instead of
    /// degrading into sending noise to Whisper.
    fn speech_only(&mut self, samples: &[f32]) -> Result<Vec<f32>, SttError> {
        let mut params = WhisperVadParams::default();
        params.set_threshold(0.5);
        params.set_min_speech_duration(250);
        params.set_min_silence_duration(250);
        params.set_speech_pad(80);
        params.set_samples_overlap(0.1);
        let segments = self
            .vad
            .segments_from_samples(params, samples)
            .map_err(|e| SttError::Transcribe(format!("Silero VAD: {e}")))?;

        let mut speech = Vec::new();
        for segment in segments {
            let start = ((segment.start / 100.0) * WHISPER_SAMPLE_RATE as f32) as usize;
            let end = ((segment.end / 100.0) * WHISPER_SAMPLE_RATE as f32) as usize;
            let start = start.min(samples.len());
            let end = end.min(samples.len());
            if start >= end {
                continue;
            }
            if !speech.is_empty() {
                speech.resize(speech.len() + ms_to_samples(100), 0.0);
            }
            speech.extend_from_slice(&samples[start..end]);
        }
        Ok(speech)
    }

    pub fn sample_rate(&self) -> u32 {
        WHISPER_SAMPLE_RATE
    }
}

/// Character set for each language Kuali knows how to validate.
///
/// Neighboring-language diacritics remain valid for names and brands. Clearly
/// incompatible characters such as Icelandic `þ` or Korean `뉴스` indicate model
/// invention over noise rather than distorted text in the expected language.
fn alphabet_of(language: &str) -> Option<&'static str> {
    match language {
        "es" | "en" => Some("abcdefghijklmnopqrstuvwxyzáéíóúüñàèìòùâêîôûäëïöçãõ"),
        // Unknown alphabets are not checked; preserving possible speech is safer
        // than deleting it blindly.
        _ => None,
    }
}

/// Characters allowed by the configured language plus custom vocabulary, so a
/// legitimate term such as `Ångström` cannot invalidate an entire sentence.
fn allowed_letters(language: &str, vocabulary: &[String]) -> Option<HashSet<char>> {
    let alphabet = alphabet_of(language)?;
    let mut letters: HashSet<char> = alphabet.chars().collect();
    letters.extend(
        vocabulary
            .iter()
            .flat_map(|term| term.chars())
            .filter(|c| c.is_alphabetic())
            .flat_map(|c| c.to_lowercase()),
    );
    Some(letters)
}

/// Whether output uses an alphabet incompatible with the configured language.
///
/// Pinning a whisper.cpp language does not prevent unrelated output over noise;
/// tests with Spanish selected still produced high-confidence Korean and
/// Icelandic text. Validation therefore inspects characters rather than model
/// confidence.
fn speaks_another_language(text: &str, allowed: Option<&HashSet<char>>) -> bool {
    let Some(allowed) = allowed else {
        return false;
    };
    text.chars()
        .filter(|c| c.is_alphabetic())
        .flat_map(|c| c.to_lowercase())
        .any(|c| !allowed.contains(&c))
}

/// Converts the UI vocabulary list into decoder context. Whisper does not follow
/// instructions; it receives expected spellings as a brief preceding sequence.
///
/// The list is wrapped in a carrier sentence using the configured language. A
/// bare list of English identifiers can overpower a pinned language: across 24
/// noisy Spanish fragments it produced eight `Thank you` phrases and one Korean
/// headline, while a Spanish carrier kept all 24 outputs in Spanish.
fn build_vocabulary_prompt(terms: &[String], language: &str) -> Option<String> {
    let mut seen = HashSet::new();
    let mut clean = Vec::new();

    for term in terms {
        let normalized = term
            .replace('\0', " ")
            .split_whitespace()
            .collect::<Vec<_>>()
            .join(" ");
        let normalized: String = normalized.chars().take(MAX_VOCABULARY_TERM_CHARS).collect();
        if normalized.is_empty() || !seen.insert(normalized.to_lowercase()) {
            continue;
        }
        clean.push(normalized);
        let prompt_chars = clean
            .iter()
            .map(|term| term.chars().count() + 2)
            .sum::<usize>();
        if prompt_chars >= MAX_VOCABULARY_PROMPT_CHARS {
            break;
        }
        if clean.len() == MAX_VOCABULARY_TERMS {
            break;
        }
    }

    if clean.is_empty() {
        return None;
    }

    let list = clean.join(", ");
    Some(match language {
        "es" => format!("En esta reunión hablamos de {list}."),
        "en" => format!("In this meeting we talk about {list}."),
        // Without a pinned language there is no carrier language; use the list alone.
        _ => format!("{list}."),
    })
}

fn resolve_threads(configured: u16) -> i32 {
    if configured > 0 {
        return configured as i32;
    }
    // Use half the cores, leaving capacity for audio reception, Discord, and UI
    // rendering during transcription.
    let cores = std::thread::available_parallelism()
        .map(|n| n.get())
        .unwrap_or(4);
    (cores / 2).max(2) as i32
}

/// Whether Whisper likely invented this phrase over silence.
pub fn is_hallucination(text: &str) -> bool {
    let normalized: String = text
        .to_lowercase()
        .chars()
        .filter(|c| c.is_alphanumeric() || c.is_whitespace())
        .collect();
    let normalized = normalized.split_whitespace().collect::<Vec<_>>().join(" ");

    if normalized.is_empty() {
        return true;
    }
    HALLUCINATIONS.iter().any(|h| normalized == *h)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn thread_count_falls_back_to_something_sane() {
        assert_eq!(resolve_threads(8), 8);
        assert!(resolve_threads(0) >= 2);
    }

    #[test]
    fn the_usual_whisper_hallucinations_are_caught() {
        assert!(is_hallucination("Gracias por ver el video"));
        assert!(is_hallucination("¡Gracias por ver el vídeo!"));
        assert!(is_hallucination(
            "Subtítulos realizados por la comunidad de Amara.org"
        ));
        assert!(is_hallucination("Thanks for watching!"));
        assert!(is_hallucination(""));
        assert!(is_hallucination("   "));
    }

    #[test]
    fn real_speech_is_not_mistaken_for_a_hallucination() {
        assert!(!is_hallucination(
            "Gracias por el resumen, lo reviso mañana"
        ));
        assert!(!is_hallucination(
            "Vale, entonces lo dejamos para el viernes"
        ));
        assert!(!is_hallucination("El deploy falló otra vez"));
    }

    #[test]
    fn a_bare_thanks_is_treated_as_noise() {
        // An isolated expression of thanks is usually a silence hallucination;
        // dropping it costs less than polluting the transcript.
        assert!(is_hallucination("Gracias."));
        assert!(is_hallucination("Thank you."));
        assert!(!is_hallucination("Gracias Ana, lo miro"));
        assert!(!is_hallucination("Thank you Ana, I'll check it"));
    }

    #[test]
    fn an_empty_transcription_reports_itself_as_empty() {
        let t = Transcription {
            text: "  ".into(),
            confidence: Some(0.9),
        };
        assert!(t.is_empty());
    }

    #[test]
    fn custom_vocabulary_becomes_a_clean_deduplicated_prompt() {
        let terms = vec![
            " WaitingRoom ".into(),
            "Kuali".into(),
            "kuali".into(),
            "Reddis\0Kafka".into(),
            "   ".into(),
        ];
        assert_eq!(
            build_vocabulary_prompt(&terms, "es").as_deref(),
            Some("En esta reunión hablamos de WaitingRoom, Kuali, Reddis Kafka.")
        );
    }

    #[test]
    fn the_vocabulary_prompt_is_written_in_the_configured_language() {
        // The prompt is the decoder's most recent context. An English prompt can
        // pull output toward English even when Spanish is pinned.
        let terms = vec!["Redis".to_string(), "Golang".to_string()];
        assert_eq!(
            build_vocabulary_prompt(&terms, "en").as_deref(),
            Some("In this meeting we talk about Redis, Golang.")
        );
        assert_eq!(
            build_vocabulary_prompt(&terms, "auto").as_deref(),
            Some("Redis, Golang.")
        );
    }

    #[test]
    fn an_empty_custom_vocabulary_does_not_add_a_prompt() {
        assert_eq!(build_vocabulary_prompt(&[], "es"), None);
    }

    /// Phrases Whisper invented during a real meeting with Spanish pinned.
    #[test]
    fn text_in_another_language_is_discarded() {
        let allowed = allowed_letters("es", &[]).unwrap();
        for invented in [
            "Íttu stað það sínda að kveldi?",
            "En hún er hún með hún. Hún er hún með hún.",
            "MBC 뉴스 김재경입니다.",
            "Пока.",
            "Sačuri.",
            "Það er sællug lík.",
        ] {
            assert!(
                speaks_another_language(invented, Some(&allowed)),
                "{invented:?} should be discarded"
            );
        }
    }

    #[test]
    fn real_spanish_survives_the_language_check() {
        let allowed = allowed_letters("es", &[]).unwrap();
        for said in [
            "Bueno, entonces lo dejamos para el viernes y revisamos el despliegue.",
            "¿Y dónde más es que se ejecutan, en la voz de datos de algún lado?",
            "Según ella, pero es que ella es muy apegada a lo que todo el mundo hace.",
            // Isolated English words inside Spanish text are ASCII and should
            // not trigger alphabet rejection.
            "El deploy del backend falló otra vez.",
        ] {
            assert!(
                !speaks_another_language(said, Some(&allowed)),
                "{said:?} should not be discarded"
            );
        }
    }

    #[test]
    fn the_custom_vocabulary_widens_the_alphabet() {
        // A user who works at `Ångström` legitimately needs that character.
        let vocabulary = vec!["Ångström".to_string()];
        let allowed = allowed_letters("es", &vocabulary).unwrap();
        assert!(!speaks_another_language(
            "Lo vimos en Ångström.",
            Some(&allowed)
        ));
        assert!(speaks_another_language("Það er.", Some(&allowed)));
    }

    #[test]
    fn without_a_fixed_language_nothing_is_discarded_by_alphabet() {
        // In `auto` mode there is no expected alphabet, so filtering by
        // characters would delete blindly.
        assert!(allowed_letters("auto", &[]).is_none());
        assert!(!speaks_another_language("MBC 뉴스", None));
    }

    #[test]
    fn the_canned_subtitle_phrases_from_a_real_meeting_are_caught() {
        for invented in ["Thanks.", "We'll be right back.", "Bye.", "See you."] {
            assert!(
                is_hallucination(invented),
                "{invented:?} should be discarded"
            );
        }
    }
}
