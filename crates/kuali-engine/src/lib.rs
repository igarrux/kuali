//! Kuali engine.
//!
//! Coordinates Discord input, per-speaker segmentation, Whisper transcription,
//! meeting persistence, and LLM follow-up after hangup.
//!
//! The interface communicates only with `Engine`; all activity is emitted over
//! the `KualiEvent` channel returned by `Engine::new`.

pub mod engine;
pub mod questions;
pub mod stt_worker;
mod webhooks;

pub use engine::{Engine, EngineError};
pub use questions::QuestionsStatus;
pub use stt_worker::{SttWorker, WorkerError};
