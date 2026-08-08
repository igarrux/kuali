//! Audio utilities.
//!
//! Kuali asks Songbird to decode Discord Opus directly to **16 kHz mono**, the
//! exact format Whisper expects. libopus performs this internally and avoids
//! decoding at 48 kHz only to filter and decimate afterward.
//!
//! No resampler is needed here; only sample conversion and measurement remain.

/// Sample rate expected by Whisper and requested from Songbird.
pub const WHISPER_SAMPLE_RATE: u32 = 16_000;

/// Duration in milliseconds represented by these 16 kHz samples.
pub fn samples_to_ms(samples: usize) -> u64 {
    samples as u64 * 1_000 / WHISPER_SAMPLE_RATE as u64
}

pub fn ms_to_samples(ms: u64) -> usize {
    (ms * WHISPER_SAMPLE_RATE as u64 / 1_000) as usize
}

/// Converts 16-bit PCM into the `[-1, 1]` floating-point range whisper.cpp expects.
pub fn i16_to_f32(input: &[i16]) -> Vec<f32> {
    const SCALE: f32 = 1.0 / 32_768.0;
    input.iter().map(|&s| s as f32 * SCALE).collect()
}

/// Measures block energy so digital silence can be discarded before Whisper,
/// which tends to invent text when given silent input.
pub fn rms(samples: &[f32]) -> f32 {
    if samples.is_empty() {
        return 0.0;
    }
    let sum: f32 = samples.iter().map(|s| s * s).sum();
    (sum / samples.len() as f32).sqrt()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pcm_scales_into_the_unit_range() {
        let converted = i16_to_f32(&[i16::MAX, i16::MIN, 0]);
        assert!((converted[0] - 1.0).abs() < 0.001);
        assert!((converted[1] + 1.0).abs() < 0.001);
        assert_eq!(converted[2], 0.0);
    }

    #[test]
    fn an_empty_packet_converts_to_nothing_without_panicking() {
        assert!(i16_to_f32(&[]).is_empty());
    }

    #[test]
    fn sample_and_millisecond_conversions_are_inverses() {
        assert_eq!(samples_to_ms(16_000), 1_000);
        assert_eq!(ms_to_samples(1_000), 16_000);
        assert_eq!(samples_to_ms(ms_to_samples(350)), 350);
    }

    #[test]
    fn a_twenty_millisecond_discord_packet_is_320_samples() {
        // This is the size of each VoiceTick. A change would make segment timing
        // inaccurate.
        assert_eq!(ms_to_samples(20), 320);
    }

    #[test]
    fn rms_of_silence_is_zero_and_of_a_sine_is_the_expected_value() {
        assert_eq!(rms(&[]), 0.0);
        assert_eq!(rms(&[0.0; 100]), 0.0);

        let sine: Vec<f32> = (0..16_000)
            .map(|i| (2.0 * std::f32::consts::PI * 1_000.0 * i as f32 / 16_000.0).sin())
            .collect();
        let energy = rms(&sine);
        assert!((energy - 0.707).abs() < 0.01, "sine-wave RMS: {energy}");
    }
}
