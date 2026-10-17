//! Local, full-recording speaker diarization over 16 kHz mono PCM.
//!
//! Only model acquisition uses the network. The model and result types are
//! independent of audio decoding and transcription. Offsets describe the input
//! recording's timeline; they do not make independently processed chunks share
//! speaker identities or provide streaming diarization.

use std::collections::HashMap;
use std::future::Future;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::Duration;

use futures::StreamExt;
use sha2::{Digest, Sha256};
use sherpa_onnx::{
    FastClusteringConfig, OfflineSpeakerDiarization, OfflineSpeakerDiarizationConfig,
    OfflineSpeakerDiarizationSegment, OfflineSpeakerSegmentationModelConfig,
    OfflineSpeakerSegmentationPyannoteModelConfig, SpeakerEmbeddingExtractorConfig,
};
use tokio::io::{AsyncReadExt, AsyncWriteExt};

pub const SAMPLE_RATE: u32 = 16_000;
/// Bounds the full-recording native algorithm's memory and clustering cost.
pub const MAX_AUDIO_DURATION_MS: u64 = 2 * 60 * 60 * 1_000;
pub const MAX_SPEAKERS: u32 = 32;

// Cosine-distance cutoff for normalized WeSpeaker features. Sherpa's generic
// 0.5 default fragments repeated voices on the public reference fixtures;
// 0.9 preserves their attributed turns. A supplied count bypasses this cutoff.
const AUTO_CLUSTERING_THRESHOLD: f32 = 0.9;

struct ModelAsset {
    file_name: &'static str,
    url: &'static str,
    bytes: u64,
    sha256: &'static str,
}

const SEGMENTATION: ModelAsset = ModelAsset {
    file_name: "pyannote-segmentation-3.0.onnx",
    url: "https://huggingface.co/csukuangfj/sherpa-onnx-pyannote-segmentation-3-0/resolve/9403a6902bb58e3d5ae8c7e77c3422de279db2e0/model.onnx",
    bytes: 5_992_913,
    sha256: "220ad67ca923bef2fa91f2390c786097bf305bceb5e261d4af67b38e938e1079",
};

const EMBEDDING: ModelAsset = ModelAsset {
    file_name: "wespeaker-resnet34-lm.onnx",
    url: "https://github.com/k2-fsa/sherpa-onnx/releases/download/speaker-recongition-models/wespeaker_en_voxceleb_resnet34_LM.onnx",
    bytes: 26_530_550,
    sha256: "e9848563da86f263117134dfd7ad63c92355b37de492b55e325400c9d9c39012",
};

// This pinned WeSpeaker export expects temporal mean normalization of the
// filterbank features, but omits Sherpa's feature_normalize_type metadata.
// Preserve the verified download and cache a deterministic metadata-only copy.
// The weights and computation graph are unchanged. Both hashes are pinned.
const NORMALIZED_EMBEDDING: ModelAsset = ModelAsset {
    file_name: "wespeaker-resnet34-lm-cmn.onnx",
    url: EMBEDDING.url,
    bytes: 26_530_589,
    sha256: "ce2c5db3fb2c438f1d58de35577c951bc27ea0d41ef74968de4a480a1a59e39b",
};

// ONNX ModelProto.metadata_props (field 14), containing StringStringEntryProto
// key (field 1) and value (field 2). Appending a repeated protobuf entry is valid
// for this exact SHA-verified model; this is not a general ONNX patcher.
const GLOBAL_MEAN_METADATA: &[u8] = b"\x72\x25\x0a\x16feature_normalize_type\x12\x0bglobal-mean";
static NEXT_MODEL_TEMP_ID: AtomicU64 = AtomicU64::new(0);

const MODEL_BYTES: u64 = SEGMENTATION.bytes + EMBEDDING.bytes;

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct DiarizationOptions {
    /// `None` estimates the number of speakers; otherwise guide clustering
    /// toward 1 through 32 speakers. The surviving turns may contain fewer.
    pub num_speakers: Option<u32>,
}

impl DiarizationOptions {
    pub fn validate(self) -> Result<Self, DiarizationError> {
        if let Some(count) = self.num_speakers {
            if !(1..=MAX_SPEAKERS).contains(&count) {
                return Err(DiarizationError::InvalidSpeakerCount(count));
            }
        }
        Ok(self)
    }
}

/// A speaker turn on the recording timeline. Turns may overlap.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SpeakerSegment {
    /// Zero-based label, canonicalized by first appearance within this result.
    pub speaker_id: u32,
    pub start_ms: u64,
    pub end_ms: u64,
}

#[derive(Debug, thiserror::Error)]
pub enum DiarizationError {
    #[error("audio diarization cancelled")]
    Cancelled,
    #[error("speaker count must be between 1 and 32 (got {0})")]
    InvalidSpeakerCount(u32),
    #[error("invalid diarization audio: {0}")]
    InvalidAudio(&'static str),
    #[error("invalid diarization model path: {0}")]
    InvalidModelPath(PathBuf),
    #[error("disk error at {path}: {source}")]
    Io {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("failed to download diarization model: {0}")]
    Download(#[from] reqwest::Error),
    #[error("model {path} has {got} bytes; expected {expected}")]
    ModelSize {
        path: PathBuf,
        got: u64,
        expected: u64,
    },
    #[error("model {path} failed SHA-256 verification")]
    ModelChecksum { path: PathBuf },
    #[error("could not initialize the local diarization models")]
    Initialization,
    #[error("local speaker diarization failed")]
    Inference,
}

fn io_error(path: &Path, source: std::io::Error) -> DiarizationError {
    DiarizationError::Io {
        path: path.to_owned(),
        source,
    }
}

fn check_cancel(cancel: &AtomicBool) -> Result<(), DiarizationError> {
    if cancel.load(Ordering::Relaxed) {
        Err(DiarizationError::Cancelled)
    } else {
        Ok(())
    }
}

// Polling also makes stalled headers/bodies cancellable without requiring a
// second cancellation primitive at the engine boundary.
async fn cancellable<F: Future>(
    cancel: &AtomicBool,
    operation: F,
) -> Result<F::Output, DiarizationError> {
    check_cancel(cancel)?;
    tokio::select! {
        output = operation => {
            check_cancel(cancel)?;
            Ok(output)
        }
        _ = async {
            while !cancel.load(Ordering::Relaxed) {
                tokio::time::sleep(Duration::from_millis(100)).await;
            }
        } => Err(DiarizationError::Cancelled),
    }
}

fn verify_model(
    path: &Path,
    model: &ModelAsset,
    cancel: &AtomicBool,
) -> Result<(), DiarizationError> {
    check_cancel(cancel)?;
    let mut file = std::fs::File::open(path).map_err(|error| io_error(path, error))?;
    let length = file
        .metadata()
        .map_err(|error| io_error(path, error))?
        .len();
    if length != model.bytes {
        return Err(DiarizationError::ModelSize {
            path: path.to_owned(),
            got: length,
            expected: model.bytes,
        });
    }
    let mut hasher = Sha256::new();
    let mut buffer = vec![0; 64 * 1024];
    loop {
        check_cancel(cancel)?;
        let read = file
            .read(&mut buffer)
            .map_err(|error| io_error(path, error))?;
        if read == 0 {
            break;
        }
        hasher.update(&buffer[..read]);
    }
    if format!("{:x}", hasher.finalize()) != model.sha256 {
        return Err(DiarizationError::ModelChecksum {
            path: path.to_owned(),
        });
    }
    check_cancel(cancel)
}

// Cached models can live on slow or external disks. Keep their verification off
// the async executor's worker thread, including cancellation while awaiting I/O.
async fn verify_model_async(
    path: &Path,
    model: &ModelAsset,
    cancel: &AtomicBool,
) -> Result<(), DiarizationError> {
    let mut file = cancellable(cancel, tokio::fs::File::open(path))
        .await?
        .map_err(|error| io_error(path, error))?;
    let length = cancellable(cancel, file.metadata())
        .await?
        .map_err(|error| io_error(path, error))?
        .len();
    if length != model.bytes {
        return Err(DiarizationError::ModelSize {
            path: path.to_owned(),
            got: length,
            expected: model.bytes,
        });
    }
    let mut hasher = Sha256::new();
    let mut buffer = vec![0; 64 * 1024];
    loop {
        let read = cancellable(cancel, file.read(&mut buffer))
            .await?
            .map_err(|error| io_error(path, error))?;
        if read == 0 {
            break;
        }
        hasher.update(&buffer[..read]);
    }
    if format!("{:x}", hasher.finalize()) != model.sha256 {
        return Err(DiarizationError::ModelChecksum {
            path: path.to_owned(),
        });
    }
    check_cancel(cancel)
}

struct PartialDownload(PathBuf);

impl Drop for PartialDownload {
    fn drop(&mut self) {
        if let Err(error) = std::fs::remove_file(&self.0) {
            if error.kind() != std::io::ErrorKind::NotFound {
                tracing::warn!(path = %self.0.display(), %error, "could not remove partial diarization model");
            }
        }
    }
}

async fn download_model<F>(
    client: &reqwest::Client,
    dir: &Path,
    model: &ModelAsset,
    cancel: &AtomicBool,
    mut progress: F,
) -> Result<(), DiarizationError>
where
    F: FnMut(u64),
{
    let final_path = dir.join(model.file_name);
    let partial = PartialDownload(dir.join(format!("{}.part", model.file_name)));
    let response = cancellable(cancel, client.get(model.url).send())
        .await??
        .error_for_status()?;
    if let Some(length) = response.content_length() {
        if length != model.bytes {
            return Err(DiarizationError::ModelSize {
                path: final_path,
                got: length,
                expected: model.bytes,
            });
        }
    }
    let mut file = tokio::fs::File::create(&partial.0)
        .await
        .map_err(|error| io_error(&partial.0, error))?;
    let mut stream = response.bytes_stream();
    let mut hasher = Sha256::new();
    let mut downloaded = 0u64;
    while let Some(chunk) = cancellable(cancel, stream.next()).await? {
        let chunk = chunk?;
        downloaded = downloaded.saturating_add(chunk.len() as u64);
        if downloaded > model.bytes {
            return Err(DiarizationError::ModelSize {
                path: final_path,
                got: downloaded,
                expected: model.bytes,
            });
        }
        cancellable(cancel, file.write_all(&chunk))
            .await?
            .map_err(|error| io_error(&partial.0, error))?;
        hasher.update(&chunk);
        progress(downloaded);
    }
    if downloaded != model.bytes {
        return Err(DiarizationError::ModelSize {
            path: final_path,
            got: downloaded,
            expected: model.bytes,
        });
    }
    if format!("{:x}", hasher.finalize()) != model.sha256 {
        return Err(DiarizationError::ModelChecksum { path: final_path });
    }
    cancellable(cancel, file.sync_all())
        .await?
        .map_err(|error| io_error(&partial.0, error))?;
    drop(file);
    check_cancel(cancel)?;
    // The destination, when present, was rejected by verification. Keep it until
    // the replacement is complete; Windows cannot rename over an existing file.
    match tokio::fs::remove_file(&final_path).await {
        Ok(()) => {}
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => return Err(io_error(&final_path, error)),
    }
    tokio::fs::rename(&partial.0, &final_path)
        .await
        .map_err(|error| io_error(&final_path, error))?;
    check_cancel(cancel)
}

/// Acquire and verify both pinned models under `dir`.
///
/// Progress counts verified or downloaded bytes across both model files. A
/// complete cache needs no network. The caller serializes access to this model
/// directory. Downloads have a 15-minute total timeout, including their bodies.
pub async fn ensure_models<F>(
    dir: &Path,
    cancel: &AtomicBool,
    mut progress: F,
) -> Result<(), DiarizationError>
where
    F: FnMut(u64, u64) + Send,
{
    check_cancel(cancel)?;
    tokio::fs::create_dir_all(dir)
        .await
        .map_err(|error| io_error(dir, error))?;
    let client = reqwest::Client::builder()
        .connect_timeout(Duration::from_secs(15))
        .timeout(Duration::from_secs(15 * 60))
        .build()?;
    let mut completed = 0;
    progress(0, MODEL_BYTES);
    for model in [&SEGMENTATION, &EMBEDDING] {
        match verify_model_async(&dir.join(model.file_name), model, cancel).await {
            Ok(()) => {}
            Err(DiarizationError::ModelSize { .. } | DiarizationError::ModelChecksum { .. }) => {
                download_model(&client, dir, model, cancel, |downloaded| {
                    progress(completed + downloaded, MODEL_BYTES);
                })
                .await?;
            }
            Err(DiarizationError::Io { source, .. })
                if source.kind() == std::io::ErrorKind::NotFound =>
            {
                download_model(&client, dir, model, cancel, |downloaded| {
                    progress(completed + downloaded, MODEL_BYTES);
                })
                .await?;
            }
            Err(error) => return Err(error),
        }
        completed += model.bytes;
        progress(completed, MODEL_BYTES);
        check_cancel(cancel)?;
    }
    Ok(())
}

fn prepare_normalized_embedding(
    dir: &Path,
    source: &ModelAsset,
    prepared: &ModelAsset,
    cancel: &AtomicBool,
) -> Result<PathBuf, DiarizationError> {
    let source_path = dir.join(source.file_name);
    let final_path = dir.join(prepared.file_name);
    verify_model(&source_path, source, cancel)?;
    match verify_model(&final_path, prepared, cancel) {
        Ok(()) => return Ok(final_path),
        Err(DiarizationError::ModelSize { .. } | DiarizationError::ModelChecksum { .. }) => {}
        Err(DiarizationError::Io { source, .. })
            if source.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => return Err(error),
    }

    // Bound the read even if another process replaces the source after its
    // verification. The derived checksum also detects a changed source.
    let mut bytes = Vec::with_capacity(prepared.bytes as usize);
    std::fs::File::open(&source_path)
        .map_err(|error| io_error(&source_path, error))?
        .take(source.bytes + 1)
        .read_to_end(&mut bytes)
        .map_err(|error| io_error(&source_path, error))?;
    bytes.extend_from_slice(GLOBAL_MEAN_METADATA);
    if bytes.len() as u64 != prepared.bytes {
        return Err(DiarizationError::ModelSize {
            path: final_path,
            got: bytes.len() as u64,
            expected: prepared.bytes,
        });
    }
    if format!("{:x}", Sha256::digest(&bytes)) != prepared.sha256 {
        return Err(DiarizationError::ModelChecksum { path: final_path });
    }
    check_cancel(cancel)?;

    // Unique create_new files prevent concurrent loaders from truncating each
    // other's prepared models. A stale partial file is harmless and skipped.
    let (partial, mut file) = loop {
        let id = NEXT_MODEL_TEMP_ID.fetch_add(1, Ordering::Relaxed);
        let path = dir.join(format!(
            "{}.{}.{}.part",
            prepared.file_name,
            std::process::id(),
            id
        ));
        match std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&path)
        {
            Ok(file) => break (PartialDownload(path), file),
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {
                check_cancel(cancel)?;
            }
            Err(error) => return Err(io_error(&path, error)),
        }
    };
    for chunk in bytes.chunks(64 * 1024) {
        check_cancel(cancel)?;
        file.write_all(chunk)
            .map_err(|error| io_error(&partial.0, error))?;
    }
    file.sync_all()
        .map_err(|error| io_error(&partial.0, error))?;
    drop(file);
    verify_model(&partial.0, prepared, cancel)?;
    std::fs::rename(&partial.0, &final_path).map_err(|error| io_error(&final_path, error))?;
    check_cancel(cancel)?;
    Ok(final_path)
}

/// CPU diarizer for one complete recording per invocation.
///
/// Run `load` and `diarize` on a blocking worker. Inference cannot be interrupted
/// by the safe native API: cancellation discards its output after it returns.
pub struct Diarizer {
    inner: OfflineSpeakerDiarization,
}

impl Diarizer {
    /// Verify file lengths and hashes before any model enters native inference.
    pub fn load(dir: &Path, options: DiarizationOptions) -> Result<Self, DiarizationError> {
        let options = options.validate()?;
        let cancel = AtomicBool::new(false);
        verify_model(&dir.join(SEGMENTATION.file_name), &SEGMENTATION, &cancel)?;
        prepare_normalized_embedding(dir, &EMBEDDING, &NORMALIZED_EMBEDDING, &cancel)?;
        let model_path = |model: &ModelAsset| -> Result<String, DiarizationError> {
            let path = dir.join(model.file_name);
            match path.to_str() {
                Some(text) if !text.contains('\0') => Ok(text.to_owned()),
                _ => Err(DiarizationError::InvalidModelPath(path)),
            }
        };
        let threads =
            std::thread::available_parallelism().map_or(2, |count| count.get().min(4)) as i32;
        let config = OfflineSpeakerDiarizationConfig {
            segmentation: OfflineSpeakerSegmentationModelConfig {
                pyannote: OfflineSpeakerSegmentationPyannoteModelConfig {
                    model: Some(model_path(&SEGMENTATION)?),
                    window_shift_ratio: 0.1,
                },
                num_threads: threads,
                ..Default::default()
            },
            embedding: SpeakerEmbeddingExtractorConfig {
                model: Some(model_path(&NORMALIZED_EMBEDDING)?),
                num_threads: threads,
                ..Default::default()
            },
            clustering: FastClusteringConfig {
                num_clusters: options.num_speakers.map_or(-1, |count| count as i32),
                threshold: AUTO_CLUSTERING_THRESHOLD,
            },
            ..Default::default()
        };
        let inner =
            OfflineSpeakerDiarization::create(&config).ok_or(DiarizationError::Initialization)?;
        if inner.sample_rate() != SAMPLE_RATE as i32 {
            return Err(DiarizationError::Initialization);
        }
        Ok(Self { inner })
    }

    /// Input must be finite, normalized (`-1..=1`), mono PCM at 16 kHz.
    /// `offset_ms` is added to every output interval after clipping to the input.
    /// Speaker IDs are local to this invocation, including when using offsets.
    pub fn diarize<F>(
        &self,
        samples: &[f32],
        offset_ms: u64,
        cancel: &AtomicBool,
        mut progress: F,
    ) -> Result<Vec<SpeakerSegment>, DiarizationError>
    where
        F: FnMut(u64, u64),
    {
        check_cancel(cancel)?;
        let duration_ms = validate_audio(samples, offset_ms, cancel)?;
        progress(0, 0);
        check_cancel(cancel)?;
        let native = self.inner.process(samples);
        check_cancel(cancel)?;
        let native = native.ok_or(DiarizationError::Inference)?;
        let segments = normalize_segments(native.sort_by_start_time(), duration_ms, offset_ms);
        // The requested count guides native clustering. Window voting and
        // short-turn filtering can leave fewer speakers in the final result;
        // keep those turns instead of discarding an otherwise valid import.
        progress(1, 1);
        check_cancel(cancel)?;
        Ok(segments)
    }
}

fn validate_audio(
    samples: &[f32],
    offset_ms: u64,
    cancel: &AtomicBool,
) -> Result<u64, DiarizationError> {
    if samples.is_empty() {
        return Err(DiarizationError::InvalidAudio("the recording is empty"));
    }
    let max_samples = MAX_AUDIO_DURATION_MS * u64::from(SAMPLE_RATE) / 1_000;
    if samples.len() as u64 > max_samples || samples.len() > i32::MAX as usize {
        return Err(DiarizationError::InvalidAudio(
            "the recording exceeds two hours",
        ));
    }
    for chunk in samples.chunks(64 * 1024) {
        check_cancel(cancel)?;
        if chunk
            .iter()
            .any(|sample| !sample.is_finite() || sample.abs() > 1.0)
        {
            return Err(DiarizationError::InvalidAudio(
                "PCM must contain finite values between -1 and 1",
            ));
        }
    }
    // Integer milliseconds cannot represent a sub-millisecond tail; rounding
    // down keeps every output timestamp inside the provided waveform.
    let duration_ms = samples.len() as u64 * 1_000 / u64::from(SAMPLE_RATE);
    offset_ms
        .checked_add(duration_ms)
        .ok_or(DiarizationError::InvalidAudio(
            "the timeline offset overflows",
        ))?;
    Ok(duration_ms)
}

fn normalize_segments(
    mut raw: Vec<OfflineSpeakerDiarizationSegment>,
    duration_ms: u64,
    offset_ms: u64,
) -> Vec<SpeakerSegment> {
    raw.retain(|turn| {
        turn.speaker >= 0 && turn.start.is_finite() && turn.end.is_finite() && turn.end > turn.start
    });
    raw.sort_by(|left, right| {
        left.start
            .total_cmp(&right.start)
            .then(left.end.total_cmp(&right.end))
            .then(left.speaker.cmp(&right.speaker))
    });
    let mut labels = HashMap::new();
    let mut result = Vec::with_capacity(raw.len());
    for turn in raw {
        let start_ms = ((f64::from(turn.start.max(0.0)) * 1_000.0).round() as u64).min(duration_ms);
        let end_ms = ((f64::from(turn.end.max(0.0)) * 1_000.0).round() as u64).min(duration_ms);
        if start_ms >= end_ms {
            continue;
        }
        let next_id = labels.len() as u32;
        let speaker_id = *labels.entry(turn.speaker).or_insert(next_id);
        result.push(SpeakerSegment {
            speaker_id,
            start_ms: offset_ms + start_ms,
            end_ms: offset_ms + end_ms,
        });
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;

    struct ModelTestDir(PathBuf);

    impl ModelTestDir {
        fn new() -> Self {
            let id = NEXT_MODEL_TEMP_ID.fetch_add(1, Ordering::Relaxed);
            let path = std::env::temp_dir()
                .join(format!("kuali-prepared-model-{}-{id}", std::process::id()));
            std::fs::create_dir(&path).unwrap();
            Self(path)
        }
    }

    impl Drop for ModelTestDir {
        fn drop(&mut self) {
            std::fs::remove_dir_all(&self.0).unwrap();
        }
    }

    const MODEL_FIXTURE: ModelAsset = ModelAsset {
        file_name: "original.onnx",
        url: "unused",
        bytes: 3,
        sha256: "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad",
    };
    const PREPARED_FIXTURE: ModelAsset = ModelAsset {
        file_name: "prepared.onnx",
        url: "unused",
        bytes: 42,
        sha256: "f12511e2008d333ddcb0f417e795b981960fd4ace53d41a03e5fb1d75b0b6528",
    };

    #[test]
    fn prepared_model_is_verified_reused_and_keeps_the_original_untouched() {
        let dir = ModelTestDir::new();
        let original = dir.0.join(MODEL_FIXTURE.file_name);
        std::fs::write(&original, b"abc").unwrap();
        let cancel = AtomicBool::new(false);
        let path = prepare_normalized_embedding(&dir.0, &MODEL_FIXTURE, &PREPARED_FIXTURE, &cancel)
            .unwrap();
        verify_model(&path, &PREPARED_FIXTURE, &cancel).unwrap();
        let modified = std::fs::metadata(&path).unwrap().modified().unwrap();
        let reused =
            prepare_normalized_embedding(&dir.0, &MODEL_FIXTURE, &PREPARED_FIXTURE, &cancel)
                .unwrap();
        assert_eq!(reused, path);
        assert_eq!(
            std::fs::metadata(&path).unwrap().modified().unwrap(),
            modified
        );
        assert_eq!(std::fs::read(original).unwrap(), b"abc");
        assert_eq!(std::fs::read_dir(&dir.0).unwrap().count(), 2);
    }

    #[test]
    fn corrupt_prepared_model_is_rebuilt_but_a_corrupt_original_is_rejected() {
        let dir = ModelTestDir::new();
        let original = dir.0.join(MODEL_FIXTURE.file_name);
        let prepared = dir.0.join(PREPARED_FIXTURE.file_name);
        let cancel = AtomicBool::new(false);
        std::fs::write(&original, b"abc").unwrap();
        for corruption in [vec![0; PREPARED_FIXTURE.bytes as usize], vec![0]] {
            std::fs::write(&prepared, corruption).unwrap();
            prepare_normalized_embedding(&dir.0, &MODEL_FIXTURE, &PREPARED_FIXTURE, &cancel)
                .unwrap();
            verify_model(&prepared, &PREPARED_FIXTURE, &cancel).unwrap();
        }
        std::fs::write(&original, b"abd").unwrap();
        assert!(matches!(
            prepare_normalized_embedding(&dir.0, &MODEL_FIXTURE, &PREPARED_FIXTURE, &cancel),
            Err(DiarizationError::ModelChecksum { .. })
        ));
        assert_eq!(std::fs::read(original).unwrap(), b"abd");
        verify_model(&prepared, &PREPARED_FIXTURE, &cancel).unwrap();
        assert_eq!(std::fs::read_dir(&dir.0).unwrap().count(), 2);
    }

    #[test]
    fn cancelled_model_preparation_does_not_publish_a_partial_model() {
        let dir = ModelTestDir::new();
        std::fs::write(dir.0.join(MODEL_FIXTURE.file_name), b"abc").unwrap();
        let cancel = AtomicBool::new(true);
        assert!(matches!(
            prepare_normalized_embedding(&dir.0, &MODEL_FIXTURE, &PREPARED_FIXTURE, &cancel),
            Err(DiarizationError::Cancelled)
        ));
        assert_eq!(std::fs::read_dir(&dir.0).unwrap().count(), 1);
    }

    #[test]
    fn concurrent_preparation_publishes_one_valid_model_without_partial_files() {
        let dir = ModelTestDir::new();
        std::fs::write(dir.0.join(MODEL_FIXTURE.file_name), b"abc").unwrap();
        let cancel = AtomicBool::new(false);
        let ready = std::sync::Barrier::new(8);
        std::thread::scope(|scope| {
            let workers: Vec<_> = (0..8)
                .map(|_| {
                    scope.spawn(|| {
                        ready.wait();
                        let path = prepare_normalized_embedding(
                            &dir.0,
                            &MODEL_FIXTURE,
                            &PREPARED_FIXTURE,
                            &cancel,
                        )
                        .unwrap();
                        verify_model(&path, &PREPARED_FIXTURE, &cancel).unwrap();
                    })
                })
                .collect();
            for worker in workers {
                worker.join().unwrap();
            }
        });
        assert_eq!(std::fs::read_dir(&dir.0).unwrap().count(), 2);
        assert_eq!(
            std::fs::read(dir.0.join(MODEL_FIXTURE.file_name)).unwrap(),
            b"abc"
        );
    }

    #[test]
    fn speaker_count_accepts_auto_and_rejects_values_outside_supported_bounds() {
        for count in [None, Some(1), Some(MAX_SPEAKERS)] {
            assert!(DiarizationOptions {
                num_speakers: count
            }
            .validate()
            .is_ok());
        }
        for count in [0, MAX_SPEAKERS + 1, u32::MAX] {
            assert!(matches!(
                DiarizationOptions {
                    num_speakers: Some(count)
                }
                .validate(),
                Err(DiarizationError::InvalidSpeakerCount(_))
            ));
        }
    }

    #[test]
    fn native_turns_are_clipped_sorted_and_keep_overlap_and_repeated_speakers() {
        let make = |start, end, speaker| OfflineSpeakerDiarizationSegment {
            start,
            end,
            speaker,
        };
        let turns = normalize_segments(
            vec![
                make(3.0, 12.0, 8),
                make(-1.0, 2.0, 8),
                make(1.0, 3.0, 3),
                make(12.0, 15.0, 4),
                make(f32::NAN, 1.0, 2),
                make(1.0, f32::INFINITY, 2),
                make(3.0, 2.0, 6),
                make(-2.0, -1.0, 9),
                make(0.0, 1.0, -1),
            ],
            10_000,
            5_000,
        );
        assert_eq!(
            turns,
            vec![
                SpeakerSegment {
                    speaker_id: 0,
                    start_ms: 5_000,
                    end_ms: 7_000
                },
                SpeakerSegment {
                    speaker_id: 1,
                    start_ms: 6_000,
                    end_ms: 8_000
                },
                SpeakerSegment {
                    speaker_id: 0,
                    start_ms: 8_000,
                    end_ms: 15_000
                },
            ]
        );
    }

    #[test]
    fn audio_validation_rejects_invalid_pcm_and_overflow_before_native_inference() {
        let cancel = AtomicBool::new(false);
        for samples in [&[][..], &[f32::NAN], &[f32::INFINITY], &[1.01], &[-1.01]] {
            assert!(matches!(
                validate_audio(samples, 0, &cancel),
                Err(DiarizationError::InvalidAudio(_))
            ));
        }
        let valid = vec![0.0; SAMPLE_RATE as usize];
        assert_eq!(validate_audio(&valid, 42, &cancel).unwrap(), 1_000);
        assert!(matches!(
            validate_audio(&valid, u64::MAX, &cancel),
            Err(DiarizationError::InvalidAudio(_))
        ));
        cancel.store(true, Ordering::Relaxed);
        assert!(matches!(
            validate_audio(&valid, 0, &cancel),
            Err(DiarizationError::Cancelled)
        ));
    }

    #[tokio::test]
    async fn cancelled_model_acquisition_does_not_create_a_directory_or_use_the_network() {
        let dir = std::env::temp_dir().join(format!(
            "kuali-cancelled-diarization-{}",
            std::process::id()
        ));
        let cancel = AtomicBool::new(true);
        let result = ensure_models(&dir, &cancel, |_, _| panic!("unexpected progress")).await;
        assert!(matches!(result, Err(DiarizationError::Cancelled)));
        assert!(!dir.exists());
    }

    #[tokio::test]
    async fn cancellation_interrupts_a_pending_download_operation() {
        let cancel = AtomicBool::new(false);
        let waiting = cancellable(&cancel, std::future::pending::<()>());
        let cancelling = async {
            tokio::task::yield_now().await;
            cancel.store(true, Ordering::Relaxed);
        };
        let (result, ()) = tokio::join!(waiting, cancelling);
        assert!(matches!(result, Err(DiarizationError::Cancelled)));
    }

    #[tokio::test]
    async fn checksum_mismatch_is_rejected_even_when_the_file_length_matches() {
        let path = std::env::temp_dir().join(format!(
            "kuali-diarization-integrity-{}",
            std::process::id()
        ));
        std::fs::write(&path, b"abc").unwrap();
        let cleanup = PartialDownload(path.clone());
        let fixture = ModelAsset {
            file_name: "fixture",
            url: "unused",
            bytes: 3,
            sha256: "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad",
        };
        let cancel = AtomicBool::new(false);
        verify_model(&path, &fixture, &cancel).unwrap();
        verify_model_async(&path, &fixture, &cancel).await.unwrap();
        std::fs::write(&path, b"abd").unwrap();
        assert!(matches!(
            verify_model(&path, &fixture, &cancel),
            Err(DiarizationError::ModelChecksum { .. })
        ));
        assert!(matches!(
            verify_model_async(&path, &fixture, &cancel).await,
            Err(DiarizationError::ModelChecksum { .. })
        ));
        cancel.store(true, Ordering::Relaxed);
        assert!(matches!(
            verify_model_async(&path, &fixture, &cancel).await,
            Err(DiarizationError::Cancelled)
        ));
        drop(cleanup);
        assert!(!path.exists());
    }
}
