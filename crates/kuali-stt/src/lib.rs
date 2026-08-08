//! Speaker-attributed transcription from incoming voice packets.
//!
//! The complete pipeline is:
//!
//! ```text
//! Discord (Opus, one RTP stream per participant)
//!   → Songbird         (libopus decodes directly to 16 kHz mono i16)
//!   → Segmenter        (cuts turns on silence or duration)
//!   → Silero VAD       (passes human speech only)
//!   → WhisperEngine    (whisper.cpp with Metal on Apple Silicon)
//!   → Utterance        (text + speaker + timestamp)
//! ```
//!
//! There is no resampling stage because libopus decodes directly to the target
//! format.
//!
//! The model remains in memory only while Kuali has an active call.

pub mod audio;
pub mod model;
pub mod segmenter;
pub mod whisper;

pub use audio::{i16_to_f32, ms_to_samples, samples_to_ms, WHISPER_SAMPLE_RATE};
pub use model::{
    ensure_downloaded, ensure_vad_downloaded, is_downloaded, is_vad_downloaded, model_path,
    vad_model_path, verify_integrity, verify_vad_integrity, ModelError,
};
pub use segmenter::{PushResult, Segment, Segmenter};
pub use whisper::{SttError, Transcription, WhisperEngine};
