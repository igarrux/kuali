//! The `capture.v1` wire format sent by the Kuali extension from Meet, Teams, or
//! Zoom to the desktop application.
//!
//! Vexa defines this Apache-2.0 format for carrying meeting audio from a browser
//! to a desktop application. Kuali implements the receiving decoder.
//!
//! Decoding is verified byte-for-byte against golden vectors published by Vexa.
//!
//! Two binary frame forms are distinguished by the high bit of the first integer:
//!
//! ```text
//! unnamed  [Int32LE track≥0][Float64LE ts][Float32LE pcm…]
//! named    [Int32LE track|0x80000000][Float64LE ts][Int32LE nameLen]
//!          [UTF-8 name, padded to 4 bytes][Float32LE pcm…]
//! ```
//!
//! Meeting events use a separate JSON text frame.

use serde::Deserialize;

/// High channel-ID bit marking a following name. Real IDs range from 0 to 1000
/// and never set it.
const NAME_FLAG: u32 = 0x8000_0000;
const AUDIO_HEADER_BYTES: usize = 12;
const NAMED_HEADER_BYTES: usize = 16;

/// `REC1` magic for combined meeting recordings sent over the same socket. This
/// is not transcription audio and is discarded.
const REC_MAGIC: u32 = 0x5245_4331;

/// Whisper uses 16 kHz mono, which the wire provides directly.
pub const CAPTURE_SAMPLE_RATE: u32 = 16_000;

#[derive(Debug, Clone, PartialEq)]
pub struct AudioFrame {
    /// Speaker channel. Channel 999 is the mixed lane used when a platform lacks
    /// participant-separated audio.
    pub speaker_index: u32,
    /// Source capture time in epoch milliseconds. The receiver preserves it so
    /// Kuali load cannot shift transcript timestamps.
    pub timestamp_ms: f64,
    pub samples: Vec<f32>,
    /// Compatibility with older capture clients that bound only a name. Modern
    /// Kuali sends `participant-upsert` with ID and avatar.
    pub speaker_name: Option<String>,
}

/// Classified socket input.
#[derive(Debug, Clone, PartialEq)]
pub enum Frame {
    Audio(AudioFrame),
    Event(MeetingEvent),
    /// Combined recording ignored by Kuali, which transcribes per-channel audio.
    Recording,
}

#[derive(Debug, Clone, PartialEq, Deserialize)]
pub struct MeetingEvent {
    pub kind: String,
    pub ts: f64,
    #[serde(default)]
    pub speaker: Option<String>,
    #[serde(default)]
    pub text: Option<String>,
    #[serde(default)]
    pub detail: Option<serde_json::Value>,
}

#[derive(Debug, thiserror::Error, PartialEq)]
pub enum WireError {
    #[error("truncated audio frame: {0} bytes")]
    Truncated(usize),
    #[error("invalid name length")]
    BadNameLength,
    #[error("speaker name is not valid UTF-8")]
    BadName,
    #[error("unreadable event: {0}")]
    BadEvent(String),
}

fn u32_le(bytes: &[u8], at: usize) -> u32 {
    u32::from_le_bytes([bytes[at], bytes[at + 1], bytes[at + 2], bytes[at + 3]])
}

fn f64_le(bytes: &[u8], at: usize) -> f64 {
    f64::from_le_bytes(bytes[at..at + 8].try_into().expect("8 bytes"))
}

/// Decodes a binary socket frame.
pub fn decode_binary(bytes: &[u8]) -> Result<Frame, WireError> {
    if bytes.len() < AUDIO_HEADER_BYTES {
        return Err(WireError::Truncated(bytes.len()));
    }

    let raw = u32_le(bytes, 0);
    // Check recordings first: their magic is a large positive integer that no
    // real channel can produce.
    if raw == REC_MAGIC {
        return Ok(Frame::Recording);
    }

    let timestamp_ms = f64_le(bytes, 4);

    if raw & NAME_FLAG == 0 {
        return Ok(Frame::Audio(AudioFrame {
            speaker_index: raw,
            timestamp_ms,
            samples: decode_pcm(&bytes[AUDIO_HEADER_BYTES..]),
            speaker_name: None,
        }));
    }

    if bytes.len() < NAMED_HEADER_BYTES {
        return Err(WireError::Truncated(bytes.len()));
    }
    let name_len = u32_le(bytes, 12) as usize;
    // Zero-pad names to a multiple of four so following PCM remains aligned.
    let padded = (name_len + 3) & !3;
    let pcm_start = NAMED_HEADER_BYTES + padded;
    if name_len > bytes.len() || pcm_start > bytes.len() {
        return Err(WireError::BadNameLength);
    }

    let name = std::str::from_utf8(&bytes[NAMED_HEADER_BYTES..NAMED_HEADER_BYTES + name_len])
        .map_err(|_| WireError::BadName)?;

    Ok(Frame::Audio(AudioFrame {
        speaker_index: raw & !NAME_FLAG,
        timestamp_ms,
        samples: decode_pcm(&bytes[pcm_start..]),
        speaker_name: (!name.is_empty()).then(|| name.to_string()),
    }))
}

/// Decodes a JSON meeting-event text frame.
pub fn decode_text(json: &str) -> Result<Frame, WireError> {
    serde_json::from_str::<MeetingEvent>(json)
        .map(Frame::Event)
        .map_err(|e| WireError::BadEvent(e.to_string()))
}

fn decode_pcm(bytes: &[u8]) -> Vec<f32> {
    bytes
        .chunks_exact(4)
        .map(|c| f32::from_le_bytes(c.try_into().expect("4 bytes")))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    // Golden vectors from Vexa's Apache-2.0
    // `modules/capture-codec/src/contracts/golden`. These bytes come from its
    // encoder and validate the real wire rather than a document interpretation.

    /// Meet lane frame with a source-bound name and 16-byte header.
    const AUDIO_NAMED: &[u8] = &[
        7, 0, 0, 128, 0, 128, 220, 121, 12, 0, 121, 66, 5, 0, 0, 0, 65, 108, 105, 99, 101, 0, 0, 0,
        0, 0, 236, 190, 0, 0, 230, 190, 0, 0, 224, 190, 0, 0, 218, 190, 0, 0, 212, 190, 0, 0, 206,
        190,
    ];

    /// Four-byte name already aligned without additional padding.
    const AUDIO_NAMED_ALIGNED: &[u8] = &[
        0, 0, 0, 128, 0, 80, 241, 121, 12, 0, 121, 66, 4, 0, 0, 0, 66, 111, 98, 33, 0, 0, 226, 190,
        0, 0, 220, 190, 0, 0, 214, 190, 0, 0, 208, 190,
    ];

    /// Mixed lane with no name and a 12-byte header.
    const AUDIO_UNNAMED: &[u8] = &[
        3, 0, 0, 0, 0, 176, 199, 121, 12, 0, 121, 66, 0, 0, 246, 190, 0, 0, 240, 190, 0, 0, 234,
        190, 0, 0, 228, 190, 0, 0, 222, 190, 0, 0, 216, 190, 0, 0, 210, 190, 0, 0, 204, 190,
    ];

    /// Empty mixed channel 999 containing only the header.
    const AUDIO_EMPTY_PCM: &[u8] = &[231, 3, 0, 0, 0, 128, 254, 121, 12, 0, 121, 66];

    fn audio(bytes: &[u8]) -> AudioFrame {
        match decode_binary(bytes).expect("message should decode") {
            Frame::Audio(frame) => frame,
            other => panic!("expected audio, got {other:?}"),
        }
    }

    #[test]
    fn a_named_meet_frame_carries_the_speaker() {
        let frame = audio(AUDIO_NAMED);
        assert_eq!(frame.speaker_index, 7);
        assert_eq!(frame.speaker_name.as_deref(), Some("Alice"));
        assert_eq!(frame.timestamp_ms, 1_718_000_000_456.0);
        assert_eq!(
            frame.samples,
            vec![
                -0.4609375,
                -0.44921875,
                -0.4375,
                -0.42578125,
                -0.4140625,
                -0.40234375
            ]
        );
    }

    #[test]
    fn a_name_that_is_already_aligned_adds_no_padding() {
        let frame = audio(AUDIO_NAMED_ALIGNED);
        assert_eq!(frame.speaker_index, 0);
        assert_eq!(frame.speaker_name.as_deref(), Some("Bob!"));
        assert_eq!(
            frame.samples,
            vec![-0.44140625, -0.4296875, -0.41796875, -0.40625]
        );
    }

    #[test]
    fn an_unnamed_frame_decodes_without_a_speaker() {
        let frame = audio(AUDIO_UNNAMED);
        assert_eq!(frame.speaker_index, 3);
        assert_eq!(frame.speaker_name, None);
        assert_eq!(frame.samples.len(), 8);
        assert_eq!(frame.samples[0], -0.48046875);
        assert_eq!(frame.timestamp_ms, 1_718_000_000_123.0);
    }

    #[test]
    fn a_header_only_frame_yields_no_samples() {
        let frame = audio(AUDIO_EMPTY_PCM);
        assert_eq!(frame.speaker_index, 999, "999 is the mixed channel");
        assert!(frame.samples.is_empty());
    }

    #[test]
    fn a_recording_chunk_is_recognised_and_not_taken_for_audio() {
        // `REC1` magic plus sequence, isFinal, and format code.
        let mut bytes = 0x5245_4331u32.to_le_bytes().to_vec();
        bytes.extend_from_slice(&1u32.to_le_bytes());
        bytes.extend_from_slice(&0u32.to_le_bytes());
        bytes.extend_from_slice(&0u32.to_le_bytes());
        assert_eq!(decode_binary(&bytes), Ok(Frame::Recording));
    }

    #[test]
    fn a_truncated_frame_is_an_error_and_not_a_panic() {
        assert_eq!(decode_binary(&[1, 2, 3]), Err(WireError::Truncated(3)));
        // Name bit set without the extended header.
        let short = [0, 0, 0, 128, 0, 0, 0, 0, 0, 0, 0, 0];
        assert_eq!(decode_binary(&short), Err(WireError::Truncated(12)));
    }

    #[test]
    fn a_name_longer_than_the_frame_is_rejected() {
        let mut bytes = (7u32 | NAME_FLAG).to_le_bytes().to_vec();
        bytes.extend_from_slice(&0f64.to_le_bytes());
        bytes.extend_from_slice(&9_000u32.to_le_bytes());
        assert_eq!(decode_binary(&bytes), Err(WireError::BadNameLength));
    }

    #[test]
    fn meeting_events_decode_from_the_text_frame() {
        let json = r#"{"kind":"speaker-joined","ts":1718000000000,"speaker":"Ana"}"#;
        let Frame::Event(event) = decode_text(json).unwrap() else {
            panic!("expected an event");
        };
        assert_eq!(event.kind, "speaker-joined");
        assert_eq!(event.speaker.as_deref(), Some("Ana"));
    }

    #[test]
    fn a_malformed_event_is_an_error_and_not_a_panic() {
        assert!(matches!(
            decode_text("{no soy json"),
            Err(WireError::BadEvent(_))
        ));
        // `kind` and `ts` are mandatory contract fields.
        assert!(matches!(
            decode_text(r#"{"ts":1}"#),
            Err(WireError::BadEvent(_))
        ));
    }
}
