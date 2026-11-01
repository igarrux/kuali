//! Silero voice detection shared by every recognition engine.

use std::ops::Range;
use std::path::Path;

use whisper_rs::{WhisperVadContext, WhisperVadContextParams, WhisperVadParams};

use crate::audio::{ms_to_samples, WHISPER_SAMPLE_RATE};
use crate::whisper::SttError;

/// Silence placed between joined speech regions.
pub(crate) const REGION_GAP_MS: u64 = 100;

/// Silero is a gate before recognition rather than an optional
/// post-processing filter. It loads once with the primary model.
pub(crate) struct SpeechGate {
    vad: WhisperVadContext,
}

impl SpeechGate {
    pub(crate) fn load(models_dir: &Path) -> Result<Self, SttError> {
        let vad_path = crate::model::vad_model_path(models_dir);
        let vad_path = vad_path
            .to_str()
            .ok_or_else(|| SttError::BadPath(vad_path.display().to_string()))?;
        let mut vad_params = WhisperVadContextParams::default();
        vad_params.set_n_threads(1);
        vad_params.set_use_gpu(false);
        let vad = WhisperVadContext::new(vad_path, vad_params)
            .map_err(|e| SttError::Load(format!("Silero VAD: {e}")))?;
        Ok(Self { vad })
    }

    /// Silero removes anything it does not recognize as speech and preserves
    /// padding around accepted regions. VAD failures propagate instead of
    /// degrading into sending noise to the recognizer.
    pub(crate) fn speech_regions(
        &mut self,
        samples: &[f32],
    ) -> Result<Vec<Range<usize>>, SttError> {
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

        let mut regions = Vec::new();
        for segment in segments {
            let start = ((segment.start / 100.0) * WHISPER_SAMPLE_RATE as f32) as usize;
            let end = ((segment.end / 100.0) * WHISPER_SAMPLE_RATE as f32) as usize;
            let start = start.min(samples.len());
            let end = end.min(samples.len());
            if start < end {
                regions.push(start..end);
            }
        }
        Ok(regions)
    }

    pub(crate) fn speech_only(&mut self, samples: &[f32]) -> Result<Vec<f32>, SttError> {
        let regions = self.speech_regions(samples)?;
        Ok(join_regions(samples, &regions))
    }
}

/// Concatenates speech regions with a short silence between them.
pub(crate) fn join_regions(samples: &[f32], regions: &[Range<usize>]) -> Vec<f32> {
    let mut speech = Vec::new();
    for region in regions {
        if !speech.is_empty() {
            speech.resize(speech.len() + ms_to_samples(REGION_GAP_MS), 0.0);
        }
        speech.extend_from_slice(&samples[region.clone()]);
    }
    speech
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn joined_regions_keep_a_short_silence_between_them() {
        let samples: Vec<f32> = (0..10).map(|i| i as f32).collect();
        let joined = join_regions(&samples, &[1..3, 6..8]);
        let gap = ms_to_samples(REGION_GAP_MS);
        assert_eq!(joined.len(), 4 + gap);
        assert_eq!(&joined[..2], &[1.0, 2.0]);
        assert!(joined[2..2 + gap].iter().all(|sample| *sample == 0.0));
        assert_eq!(&joined[2 + gap..], &[6.0, 7.0]);
        assert!(join_regions(&samples, &[]).is_empty());
    }
}
