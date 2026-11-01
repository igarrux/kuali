//! Russian transcription with Sber's GigaAM-v3 through sherpa-onnx.
//!
//! The end-to-end CTC export returns punctuated, normalized Russian text. It
//! knows no other language and takes no prompt, so Whisper's language checks,
//! hallucination list and vocabulary prompt do not apply.

use std::collections::hash_map::RandomState;
use std::fs::OpenOptions;
use std::hash::{BuildHasher, Hasher};
use std::io::Write;
use std::ops::Range;
use std::path::{Path, PathBuf};

use kuali_core::WhisperConfig;
use sherpa_onnx::{OfflineRecognizer, OfflineRecognizerConfig};

use crate::audio::{ms_to_samples, WHISPER_SAMPLE_RATE};
use crate::vad::{join_regions, SpeechGate, REGION_GAP_MS};
use crate::whisper::{resolve_threads, SttError, Transcription, MIN_INPUT_MS};

/// Vocabulary of the pinned export, kept beside the code because sherpa-onnx
/// 1.13.7 reads tokens only from a file.
const TOKENS: &str = include_str!("../assets/gigaam-v3-e2e-ctc.tokens.txt");

/// GigaAM accepts at most 25 s per call; longer speech is decoded in chunks.
const MAX_CHUNK_MS: u64 = 20_000;

pub struct GigaAmEngine {
    recognizer: OfflineRecognizer,
    vad: SpeechGate,
    _tokens: TokensFile,
}

impl GigaAmEngine {
    pub fn load(path: &Path, config: &WhisperConfig) -> Result<Self, SttError> {
        let model = path
            .to_str()
            .ok_or_else(|| SttError::BadPath(path.display().to_string()))?;
        // sherpa-onnx aborts the process on a malformed model instead of
        // returning an error, so the weight is checked before it reaches it.
        crate::model::verify_integrity(path, config.model)
            .map_err(|e| SttError::Load(format!("GigaAM: {e}")))?;
        let tokens = TokensFile::write()?;

        let mut recognizer_config = OfflineRecognizerConfig::default();
        // sherpa-onnx also reads this from the model metadata.
        recognizer_config.feat_config.feature_dim = 64;
        recognizer_config.model_config.nemo_ctc.model = Some(model.to_string());
        recognizer_config.model_config.tokens = Some(tokens.path_str()?.to_string());
        recognizer_config.model_config.num_threads = resolve_threads(config.threads);
        recognizer_config.model_config.provider = Some("cpu".into());
        let recognizer = OfflineRecognizer::create(&recognizer_config)
            .ok_or_else(|| SttError::Load(format!("GigaAM: {}", path.display())))?;

        let vad = SpeechGate::load(&config.resolved_models_directory())?;
        Ok(Self {
            recognizer,
            vad,
            _tokens: tokens,
        })
    }

    /// Transcribes a 16 kHz mono audio fragment.
    pub fn transcribe(&mut self, samples: &[f32]) -> Result<Transcription, SttError> {
        let padded;
        let samples = if samples.len() < ms_to_samples(MIN_INPUT_MS) {
            let mut buf = samples.to_vec();
            buf.resize(ms_to_samples(MIN_INPUT_MS), 0.0);
            padded = buf;
            &padded[..]
        } else {
            samples
        };
        let regions = self.vad.speech_regions(samples)?;

        let mut text = String::new();
        for chunk in chunk_regions(&regions, ms_to_samples(MAX_CHUNK_MS)) {
            let speech = join_regions(samples, &chunk);
            let stream = self.recognizer.create_stream();
            stream.accept_waveform(WHISPER_SAMPLE_RATE as i32, &speech);
            self.recognizer.decode(&stream);
            let result = stream
                .get_result()
                .ok_or_else(|| SttError::Transcribe("GigaAM returned no result".into()))?;
            let piece = result.text.trim();
            if piece.is_empty() {
                continue;
            }
            if !text.is_empty() {
                text.push(' ');
            }
            text.push_str(piece);
        }

        // Punctuation alone is not speech.
        if !text.chars().any(char::is_alphanumeric) {
            text.clear();
        }
        Ok(Transcription {
            text,
            confidence: None,
        })
    }
}

/// Groups speech regions into chunks no longer than `max_samples` once joined,
/// splitting a region that is longer by itself into equal pieces.
fn chunk_regions(regions: &[Range<usize>], max_samples: usize) -> Vec<Vec<Range<usize>>> {
    let gap = ms_to_samples(REGION_GAP_MS);
    let mut chunks = Vec::new();
    let mut current: Vec<Range<usize>> = Vec::new();
    let mut current_len = 0;
    for region in regions {
        // Equal pieces avoid a remainder too short to yield feature frames.
        let pieces = region.len().div_ceil(max_samples);
        let piece_len = region.len().div_ceil(pieces.max(1));
        let mut start = region.start;
        while start < region.end {
            let piece = start..region.end.min(start + piece_len);
            start = piece.end;
            if !current.is_empty() && current_len + gap + piece.len() > max_samples {
                chunks.push(std::mem::take(&mut current));
                current_len = 0;
            }
            if !current.is_empty() {
                current_len += gap;
            }
            current_len += piece.len();
            current.push(piece);
        }
    }
    if !current.is_empty() {
        chunks.push(current);
    }
    chunks
}

/// Private copy of [`TOKENS`] for sherpa-onnx, removed with the engine.
struct TokensFile(PathBuf);

impl TokensFile {
    fn write() -> Result<Self, SttError> {
        let directory = std::env::temp_dir();
        let mut attempts = 0;
        loop {
            // An unpredictable name keeps other local users from squatting it.
            let random = RandomState::new().build_hasher().finish();
            let path = directory.join(format!(
                "kuali-gigaam-tokens-{}-{random:016x}.txt",
                std::process::id()
            ));
            // Never follow or reuse a file someone else left at this name.
            match OpenOptions::new().write(true).create_new(true).open(&path) {
                Ok(mut file) => {
                    let tokens = Self(path);
                    file.write_all(TOKENS.as_bytes())
                        .map_err(|e| SttError::Load(format!("GigaAM tokens: {e}")))?;
                    return Ok(tokens);
                }
                Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists && attempts < 16 => {
                    attempts += 1;
                }
                Err(e) => return Err(SttError::Load(format!("GigaAM tokens: {e}"))),
            }
        }
    }

    fn path_str(&self) -> Result<&str, SttError> {
        self.0
            .to_str()
            .ok_or_else(|| SttError::BadPath(self.0.display().to_string()))
    }
}

impl Drop for TokensFile {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.0);
    }
}

#[cfg(test)]
mod tests {
    use sha2::{Digest, Sha256};

    use super::*;

    #[test]
    fn the_bundled_tokens_match_the_pinned_export() {
        let digest = Sha256::digest(TOKENS.as_bytes());
        let hex: String = digest.iter().map(|byte| format!("{byte:02x}")).collect();
        assert_eq!(
            hex,
            "142de7570b3de5b3035ce111a89c228e80e6085273731d944093ddf24fa539cd"
        );
        assert_eq!(TOKENS.lines().count(), 257);
        assert_eq!(TOKENS.lines().last(), Some("<blk> 256"));
    }

    #[test]
    fn the_tokens_file_lives_only_as_long_as_its_owner() {
        let tokens = TokensFile::write().expect("tokens file");
        let path = tokens.0.clone();
        assert_eq!(std::fs::read_to_string(&path).unwrap(), TOKENS);
        let other = TokensFile::write().expect("second tokens file");
        assert_ne!(other.0, path);
        drop(tokens);
        assert!(!path.exists());
        assert!(other.0.exists());
    }

    #[test]
    fn a_damaged_weight_is_rejected_before_sherpa_onnx_reads_it() {
        let random = RandomState::new().build_hasher().finish();
        let directory = std::env::temp_dir().join(format!("kuali-gigaam-{random:016x}"));
        std::fs::create_dir_all(&directory).unwrap();
        let config = WhisperConfig {
            model: kuali_core::WhisperModel::GigaamV3E2eCtc,
            models_directory: Some(directory.clone()),
            ..WhisperConfig::default()
        };
        let path = crate::model::model_path(&directory, config.model);
        std::fs::write(&path, b"not an onnx model").unwrap();

        let error = GigaAmEngine::load(&path, &config)
            .err()
            .expect("damaged weight");
        assert!(matches!(error, SttError::Load(_)), "{error}");

        std::fs::remove_dir_all(&directory).unwrap();
    }

    #[test]
    fn short_speech_is_decoded_in_one_chunk() {
        let second = ms_to_samples(1_000);
        let regions = vec![0..second, 2 * second..3 * second];
        assert_eq!(
            chunk_regions(&regions, ms_to_samples(MAX_CHUNK_MS)),
            vec![regions.clone()]
        );
        assert!(chunk_regions(&[], ms_to_samples(MAX_CHUNK_MS)).is_empty());
    }

    #[test]
    fn long_speech_is_split_at_region_boundaries() {
        let second = ms_to_samples(1_000);
        let max = ms_to_samples(MAX_CHUNK_MS);
        let regions = vec![0..12 * second, 13 * second..25 * second];
        let chunks = chunk_regions(&regions, max);
        assert_eq!(
            chunks,
            vec![vec![regions[0].clone()], vec![regions[1].clone()]]
        );
    }

    #[test]
    fn a_region_longer_than_a_chunk_is_split_into_equal_pieces() {
        let second = ms_to_samples(1_000);
        let max = ms_to_samples(MAX_CHUNK_MS);
        let region = 0..45 * second;
        let chunks = chunk_regions(std::slice::from_ref(&region), max);
        assert_eq!(
            chunks,
            vec![
                vec![0..15 * second],
                vec![15 * second..30 * second],
                vec![30 * second..45 * second]
            ]
        );

        // A region just over the limit must not leave a sliver behind.
        let region = 0..max + 50;
        let chunks = chunk_regions(std::slice::from_ref(&region), max);
        assert_eq!(chunks.len(), 2);
        assert!(chunks.iter().flatten().all(|piece| piece.len() >= max / 2));
    }

    #[test]
    fn no_chunk_exceeds_the_limit_once_joined() {
        let second = ms_to_samples(1_000);
        let max = ms_to_samples(MAX_CHUNK_MS);
        let regions: Vec<_> = (0..40)
            .map(|i| i * 2 * second..i * 2 * second + second + second / 3)
            .collect();
        let chunks = chunk_regions(&regions, max);
        let gap = ms_to_samples(REGION_GAP_MS);
        for chunk in &chunks {
            let joined: usize =
                chunk.iter().map(|r| r.len()).sum::<usize>() + gap * (chunk.len() - 1);
            assert!(joined <= max);
        }
        let flattened: Vec<_> = chunks.into_iter().flatten().collect();
        assert_eq!(flattened, regions);
    }
}
