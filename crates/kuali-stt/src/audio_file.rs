//! Decode recorded audio into the same 16 kHz mono PCM used by the speech engines.
//!
//! Decoding and resampling are incremental: only the final 16 kHz waveform is
//! retained. No external process, temporary audio file, or network is involved.

use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};

use rubato::{FftFixedIn, Resampler};
use symphonia::core::{
    audio::SampleBuffer,
    codecs::{self, DecoderOptions},
    errors::Error as DecodeError,
    formats::FormatOptions,
    io::MediaSourceStream,
    meta::MetadataOptions,
    probe::Hint,
};

use crate::audio::WHISPER_SAMPLE_RATE;

/// Bounds the retained waveform to about 461 MB, independent of source bitrate.
pub const MAX_AUDIO_SECONDS: u64 = 2 * 60 * 60;
const MAX_FILE_BYTES: u64 = 4 * 1024 * 1024 * 1024;
const RESAMPLE_CHUNK: usize = 2048;

#[derive(Debug, thiserror::Error)]
pub enum AudioFileError {
    #[error("audio_import_cancelled")]
    Cancelled,
    #[error("No se pudo leer el archivo de audio: {0}")]
    Io(#[from] std::io::Error),
    #[error("No se pudo decodificar el audio: {0}. Usa WAV, MP3, M4A, AAC, FLAC u OGG Vorbis.")]
    Decode(String),
    #[error("El archivo no contiene una pista de audio compatible.")]
    NoTrack,
    #[error("El archivo no contiene muestras de audio.")]
    Empty,
    #[error("El audio debe durar como máximo 2 horas y el archivo no debe superar 4 GB.")]
    TooLarge,
    #[error("El audio cambia de frecuencia o de canales durante la grabación.")]
    FormatChanged,
    #[error(
        "El archivo de audio está incompleto o dañado. Vuelve a obtener la grabación original."
    )]
    Truncated,
    #[error("El audio contiene muestras o una frecuencia no válidas.")]
    InvalidSamples,
    #[error("No se pudo convertir la frecuencia del audio: {0}")]
    Resample(String),
}

fn check_cancel(cancel: &AtomicBool) -> Result<(), AudioFileError> {
    if cancel.load(Ordering::Relaxed) {
        Err(AudioFileError::Cancelled)
    } else {
        Ok(())
    }
}

/// Blocking; run on the import worker. Progress counts decoded source frames.
pub fn decode_audio_file(
    path: &Path,
    cancel: &AtomicBool,
    progress: impl FnMut(u64, u64),
) -> Result<Vec<f32>, AudioFileError> {
    check_cancel(cancel)?;
    let file = std::fs::File::open(path)?;
    let metadata = file.metadata()?;
    if !metadata.is_file() {
        return Err(AudioFileError::NoTrack);
    }
    if metadata.len() > MAX_FILE_BYTES {
        return Err(AudioFileError::TooLarge);
    }
    let mut hint = Hint::new();
    if let Some(extension) = path.extension().and_then(|s| s.to_str()) {
        hint.with_extension(extension);
    }
    decode_stream(
        MediaSourceStream::new(Box::new(file), Default::default()),
        &hint,
        cancel,
        progress,
    )
}

fn decode_stream(
    stream: MediaSourceStream,
    hint: &Hint,
    cancel: &AtomicBool,
    mut progress: impl FnMut(u64, u64),
) -> Result<Vec<f32>, AudioFileError> {
    check_cancel(cancel)?;
    let options = FormatOptions {
        enable_gapless: true,
        ..Default::default()
    };
    let probed = symphonia::default::get_probe()
        .format(hint, stream, &options, &MetadataOptions::default())
        .map_err(|error| AudioFileError::Decode(error.to_string()))?;
    let mut format = probed.format;
    let track = format
        .default_track()
        .filter(|t| t.codec_params.sample_rate.is_some())
        .or_else(|| {
            format
                .tracks()
                .iter()
                .find(|t| t.codec_params.sample_rate.is_some())
        })
        .ok_or(AudioFileError::NoTrack)?;
    let track_id = track.id;
    let rate = track
        .codec_params
        .sample_rate
        .ok_or(AudioFileError::NoTrack)?;
    let expected_frames = track.codec_params.n_frames.map_or(0, |frames| {
        track.codec_params.time_base.map_or(frames, |time| {
            (u128::from(frames) * u128::from(time.numer) * u128::from(rate)
                / u128::from(time.denom.max(1)))
            .min(u128::from(u64::MAX)) as u64
        })
    });
    // No lossy encoder delay or padding needs reconciling for these codecs.
    let exact_duration = matches!(
        track.codec_params.codec,
        codecs::CODEC_TYPE_FLAC
            | codecs::CODEC_TYPE_ALAC
            | codecs::CODEC_TYPE_PCM_S8
            | codecs::CODEC_TYPE_PCM_U8
            | codecs::CODEC_TYPE_PCM_S16LE
            | codecs::CODEC_TYPE_PCM_S16BE
            | codecs::CODEC_TYPE_PCM_S24LE
            | codecs::CODEC_TYPE_PCM_S24BE
            | codecs::CODEC_TYPE_PCM_S32LE
            | codecs::CODEC_TYPE_PCM_S32BE
            | codecs::CODEC_TYPE_PCM_U16LE
            | codecs::CODEC_TYPE_PCM_U16BE
            | codecs::CODEC_TYPE_PCM_U24LE
            | codecs::CODEC_TYPE_PCM_U24BE
            | codecs::CODEC_TYPE_PCM_U32LE
            | codecs::CODEC_TYPE_PCM_U32BE
            | codecs::CODEC_TYPE_PCM_F32LE
            | codecs::CODEC_TYPE_PCM_F32BE
            | codecs::CODEC_TYPE_PCM_F64LE
            | codecs::CODEC_TYPE_PCM_F64BE
            | codecs::CODEC_TYPE_PCM_ALAW
            | codecs::CODEC_TYPE_PCM_MULAW
    );
    if expected_frames > MAX_AUDIO_SECONDS * u64::from(rate) {
        return Err(AudioFileError::TooLarge);
    }
    let mut decoder = symphonia::default::get_codecs()
        .make(&track.codec_params, &DecoderOptions { verify: true })
        .map_err(|error| AudioFileError::Decode(error.to_string()))?;
    let mut normalizer = MonoNormalizer::new(rate)?;
    let mut channels = None;
    let mut decoded_frames = 0;
    progress(0, expected_frames);
    loop {
        check_cancel(cancel)?;
        let packet = match format.next_packet() {
            Ok(packet) => packet,
            Err(DecodeError::IoError(error))
                if error.kind() == std::io::ErrorKind::UnexpectedEof =>
            {
                break
            }
            Err(error) => return Err(AudioFileError::Decode(error.to_string())),
        };
        if packet.track_id() != track_id {
            continue;
        }
        // Do not skip damaged packets: silently removing samples would shift
        // every subsequent speaker timestamp away from the original recording.
        let decoded = decoder
            .decode(&packet)
            .map_err(|error| AudioFileError::Decode(error.to_string()))?;
        let spec = *decoded.spec();
        if spec.rate != rate || channels.is_some_and(|value| value != spec.channels) {
            return Err(AudioFileError::FormatChanged);
        }
        channels = Some(spec.channels);
        let count = spec.channels.count();
        if count == 0 || count > 32 {
            return Err(AudioFileError::InvalidSamples);
        }
        decoded_frames += decoded.frames() as u64;
        if decoded_frames > MAX_AUDIO_SECONDS * u64::from(rate) {
            return Err(AudioFileError::TooLarge);
        }
        let mut interleaved = SampleBuffer::<f32>::new(decoded.capacity() as u64, spec);
        interleaved.copy_interleaved_ref(decoded);
        for frame in interleaved.samples().chunks_exact(count) {
            if frame.iter().any(|sample| !sample.is_finite()) {
                return Err(AudioFileError::InvalidSamples);
            }
            let mono = frame.iter().map(|s| s.clamp(-1.0, 1.0)).sum::<f32>() / count as f32;
            normalizer.push(mono)?;
        }
        progress(
            decoded_frames,
            if expected_frames == 0 {
                0
            } else {
                expected_frames.max(decoded_frames)
            },
        );
    }
    check_cancel(cancel)?;
    if (exact_duration && expected_frames > decoded_frames)
        || decoder.finalize().verify_ok == Some(false)
    {
        return Err(AudioFileError::Truncated);
    }
    let output = normalizer.finish()?;
    if output.is_empty() {
        return Err(AudioFileError::Empty);
    }
    progress(decoded_frames, decoded_frames);
    Ok(output)
}

/// Anti-aliasing FFT resampling with delay compensation keeps diarization time
/// zero at the beginning of the source file, including very short recordings.
struct MonoNormalizer {
    rate: u32,
    resampler: Option<FftFixedIn<f32>>,
    pending: Vec<f32>,
    output: Vec<f32>,
    frames: u64,
    delay: usize,
}

impl MonoNormalizer {
    fn new(rate: u32) -> Result<Self, AudioFileError> {
        if !(8_000..=192_000).contains(&rate) {
            return Err(AudioFileError::InvalidSamples);
        }
        let resampler = if rate == WHISPER_SAMPLE_RATE {
            None
        } else {
            Some(
                FftFixedIn::new(
                    rate as usize,
                    WHISPER_SAMPLE_RATE as usize,
                    RESAMPLE_CHUNK,
                    2,
                    1,
                )
                .map_err(|e| AudioFileError::Resample(e.to_string()))?,
            )
        };
        let delay = resampler.as_ref().map_or(0, |r| r.output_delay());
        Ok(Self {
            rate,
            resampler,
            pending: Vec::with_capacity(RESAMPLE_CHUNK),
            output: Vec::new(),
            frames: 0,
            delay,
        })
    }

    fn push(&mut self, sample: f32) -> Result<(), AudioFileError> {
        self.frames += 1;
        if self.resampler.is_none() {
            self.output.push(sample);
        } else {
            self.pending.push(sample);
            if self.pending.len() == RESAMPLE_CHUNK {
                self.process_pending()?;
            }
        }
        Ok(())
    }

    fn process_pending(&mut self) -> Result<(), AudioFileError> {
        let resampler = self.resampler.as_mut().expect("resampling enabled");
        self.pending.resize(RESAMPLE_CHUNK, 0.0);
        let resampled = resampler
            .process(&[&self.pending], None)
            .map_err(|e| AudioFileError::Resample(e.to_string()))?;
        self.output.extend_from_slice(&resampled[0]);
        self.pending.clear();
        Ok(())
    }

    fn finish(mut self) -> Result<Vec<f32>, AudioFileError> {
        let expected =
            (self.frames * u64::from(WHISPER_SAMPLE_RATE) / u64::from(self.rate)) as usize;
        if self.resampler.is_some() {
            // Flush both the final partial input and the resampler's delayed tail.
            if !self.pending.is_empty() {
                self.process_pending()?;
            }
            while self.output.len() < expected + self.delay {
                self.process_pending()?;
            }
            self.output.drain(..self.delay);
        }
        self.output.truncate(expected);
        for sample in &mut self.output {
            *sample = sample.clamp(-1.0, 1.0);
        }
        Ok(self.output)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;

    fn wave(rate: u32, channels: u16, samples: &[i16]) -> Vec<u8> {
        let bytes = (samples.len() * 2) as u32;
        let mut out = Vec::new();
        out.extend_from_slice(b"RIFF");
        out.extend_from_slice(&(36 + bytes).to_le_bytes());
        out.extend_from_slice(b"WAVEfmt ");
        out.extend_from_slice(&16u32.to_le_bytes());
        out.extend_from_slice(&1u16.to_le_bytes());
        out.extend_from_slice(&channels.to_le_bytes());
        out.extend_from_slice(&rate.to_le_bytes());
        out.extend_from_slice(&(rate * u32::from(channels) * 2).to_le_bytes());
        out.extend_from_slice(&(channels * 2).to_le_bytes());
        out.extend_from_slice(&16u16.to_le_bytes());
        out.extend_from_slice(b"data");
        out.extend_from_slice(&bytes.to_le_bytes());
        for sample in samples {
            out.extend_from_slice(&sample.to_le_bytes());
        }
        out
    }

    fn decode_wave(bytes: Vec<u8>, cancelled: bool) -> Result<Vec<f32>, AudioFileError> {
        decode_stream(
            MediaSourceStream::new(Box::new(Cursor::new(bytes)), Default::default()),
            &Hint::new(),
            &AtomicBool::new(cancelled),
            |_, _| {},
        )
    }

    #[test]
    fn stereo_file_is_downmixed_without_changing_duration() {
        let samples: Vec<i16> = (0..16_000).flat_map(|_| [16_384, 0]).collect();
        let output = decode_wave(wave(16_000, 2, &samples), false).unwrap();
        assert_eq!(output.len(), 16_000);
        assert!(output.iter().all(|s| (*s - 0.25).abs() < 0.001));
    }

    #[test]
    fn resampling_preserves_duration_and_rejects_aliases() {
        for (rate, frequency, audible) in [
            (44_100, 1000.0, true),
            (48_000, 1000.0, true),
            (48_000, 12_000.0, false),
        ] {
            let samples: Vec<i16> = (0..rate)
                .map(|i| {
                    (12_000.0 * (std::f32::consts::TAU * frequency * i as f32 / rate as f32).sin())
                        as i16
                })
                .collect();
            let output = decode_wave(wave(rate, 1, &samples), false).unwrap();
            assert_eq!(output.len(), 16_000);
            let rms = crate::audio::rms(&output[100..15_900]);
            if audible {
                assert!(rms > 0.2, "lost speech-band signal: {rms}");
            } else {
                assert!(rms < 0.01, "high frequency aliased into speech: {rms}");
            }
        }
    }

    #[test]
    fn very_short_recordings_flush_the_resampler_tail() {
        let output = decode_wave(wave(48_000, 1, &[16_384; 300]), false).unwrap();
        assert_eq!(output.len(), 100);
        assert!(output[20..80].iter().all(|s| (*s - 0.5).abs() < 0.01));
    }

    #[test]
    fn cancelled_or_invalid_files_do_not_produce_a_partial_waveform() {
        assert!(matches!(
            decode_wave(wave(16_000, 1, &[0; 100]), true),
            Err(AudioFileError::Cancelled)
        ));
        assert!(decode_wave(b"not audio".to_vec(), false).is_err());
    }

    #[test]
    fn truncated_wave_is_rejected_after_earlier_packets_decoded_successfully() {
        let mut bytes = wave(16_000, 1, &[1_000; 32_000]);
        bytes.truncate(bytes.len() / 2);
        assert!(matches!(
            decode_wave(bytes, false),
            Err(AudioFileError::Truncated)
        ));
    }
}
