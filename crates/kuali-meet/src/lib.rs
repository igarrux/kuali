//! Browser meetings as a second audio source.
//!
//! Kuali does not join the meeting. A browser extension captures audio within
//! Meet, Microsoft Teams, or Zoom and sends it over a local WebSocket using
//! `capture.v1` plus participant registration.
//!
//! ```text
//! Browser tab inside the meeting
//!   └─ the extension hooks available WebRTC tracks
//!        └─ 16 kHz mono PCM + channel ↔ ID/name/avatar
//!             └─ ws://127.0.0.1:9099/ingest
//!                  └─ Kuali converts it into VoiceEvent
//!                       └─ the same pipeline used by Discord
//! ```
//!
//! Individual tracks remain separate when exposed by the platform. A single Zoom
//! mix is labeled as mixed instead of receiving an invented identity. Segmentation,
//! VAD, Whisper, vocabulary, and summaries reuse the Discord pipeline.
//!
//! Vexa defines the Apache-2.0 `capture.v1` format. Kuali implements its receiver;
//! see `wire.rs`.

pub mod ingest;
pub mod wire;

pub use ingest::{default_addr, serve, IngestError, DEFAULT_PORT};
pub use wire::{AudioFrame, Frame, MeetingEvent, WireError, CAPTURE_SAMPLE_RATE};
