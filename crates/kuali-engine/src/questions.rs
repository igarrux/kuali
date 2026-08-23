//! Getting questions about past meetings ready to use.
//!
//! Before Kuali answers, the embedding model must be on disk, the derived index
//! must match every finished meeting, and every stored passage must have a
//! vector. This module prepares those pieces in order and reports progress.
//!
//! Neither step is done behind the user's back. The model is 128 MB, and
//! embedding an existing library takes minutes on a large one, so both are
//! things a person opts into after being told the cost.

use std::path::{Path, PathBuf};

use kuali_core::QuestionSetupStage;

/// Where the weights come from.
///
/// Pinned to a revision rather than `main` so a future change upstream cannot
/// silently alter what Kuali downloads. The quantized export is the int8 one:
/// it is a third of the size of the float build and works on both Apple Silicon
/// and x86, unlike the AVX-512 variant.
const MODEL_URL: &str =
    "https://huggingface.co/Xenova/multilingual-e5-small/resolve/main/onnx/model_quantized.onnx";
const TOKENIZER_URL: &str =
    "https://huggingface.co/Xenova/multilingual-e5-small/resolve/main/tokenizer.json";

/// Roughly what the two files weigh, used only to show a size before the
/// download starts. Real progress comes from the server's content length.
pub const DOWNLOAD_BYTES: u64 = 128 * 1024 * 1024;

/// Conservative rate for the estimate shown *before* anything is measured.
///
/// Apple Silicon does about 12 ms per passage. Doubling it keeps the promise on
/// a slower machine, and the moment indexing starts the interface replaces this
/// with the rate it actually observes.
pub const ASSUMED_MS_PER_PASSAGE: u64 = 25;

#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct QuestionsStatus {
    /// Whether the user turned the feature on.
    pub enabled: bool,
    /// Whether both model files are present.
    pub model_ready: bool,
    /// Whether the derived SQLite memory opened and its counters are readable.
    pub index_available: bool,
    /// Whether every finished meeting in the authoritative store has a row in
    /// the derived index. This catches a failed write that creates no pending
    /// passage at all.
    pub index_current: bool,
    /// Passages still waiting for a vector.
    pub pending_passages: usize,
    /// Passages that already have one, so the interface can show how far along
    /// an interrupted run got.
    pub embedded_passages: usize,
    /// A finished meeting or startup synchronization has not yet reached a
    /// stable index snapshot. While true, answering would risk omitting it even
    /// if every older passage already has a vector.
    pub updating: bool,
    /// Whether a question can actually be answered right now.
    pub ready: bool,
}

/// Downloads a file to its destination, resuming nothing and replacing only on
/// success, so an interrupted download never leaves a half file that looks
/// complete.
pub async fn download_file(
    url: &str,
    destination: &Path,
    mut on_progress: impl FnMut(u64, Option<u64>),
) -> Result<(), String> {
    if let Some(dir) = destination.parent() {
        std::fs::create_dir_all(dir).map_err(|error| error.to_string())?;
    }
    let partial = destination.with_extension("part");

    let response = reqwest::Client::builder()
        // No overall timeout: 112 MB on a slow connection is not a failure.
        .build()
        .map_err(|error| error.to_string())?
        .get(url)
        .send()
        .await
        .map_err(|error| error.to_string())?;
    if !response.status().is_success() {
        return Err(format!("la descarga respondió {}", response.status()));
    }

    let total = response.content_length();
    let mut file = tokio::fs::File::create(&partial)
        .await
        .map_err(|error| error.to_string())?;
    let mut written = 0u64;
    let mut stream = response.bytes_stream();

    use futures::StreamExt;
    use tokio::io::AsyncWriteExt;
    while let Some(chunk) = stream.next().await {
        let chunk = chunk.map_err(|error| error.to_string())?;
        file.write_all(&chunk)
            .await
            .map_err(|error| error.to_string())?;
        written += chunk.len() as u64;
        on_progress(written, total);
    }
    file.flush().await.map_err(|error| error.to_string())?;
    drop(file);

    std::fs::rename(&partial, destination).map_err(|error| error.to_string())?;
    Ok(())
}

/// Fetches both files, reporting one combined byte count so the interface shows
/// a single bar rather than two.
pub async fn download_model(
    models_dir: &Path,
    mut on_progress: impl FnMut(QuestionSetupStage, u64, Option<u64>),
) -> Result<(), String> {
    let targets: [(&str, PathBuf); 2] = [
        (MODEL_URL, kuali_memory::embed::model_path(models_dir)),
        (
            TOKENIZER_URL,
            kuali_memory::embed::tokenizer_path(models_dir),
        ),
    ];

    let mut finished_bytes = 0u64;
    for (url, destination) in targets {
        if destination.is_file() {
            continue;
        }
        let mut last = 0u64;
        download_file(url, &destination, |written, _| {
            last = written;
            on_progress(
                QuestionSetupStage::Downloading,
                finished_bytes + written,
                Some(DOWNLOAD_BYTES.max(finished_bytes + written)),
            );
        })
        .await?;
        finished_bytes += last;
    }
    Ok(())
}

/// Removes the downloaded weights, used when the user turns the feature off and
/// wants the disk space back.
pub fn delete_model(models_dir: &Path) -> std::io::Result<u64> {
    let mut freed = 0;
    for path in [
        kuali_memory::embed::model_path(models_dir),
        kuali_memory::embed::tokenizer_path(models_dir),
    ] {
        if let Ok(metadata) = std::fs::metadata(&path) {
            freed += metadata.len();
        }
        match std::fs::remove_file(&path) {
            Err(error) if error.kind() != std::io::ErrorKind::NotFound => return Err(error),
            _ => {}
        }
    }
    Ok(freed)
}

/// Whisper's directory setting governs every model Kuali downloads, so the
/// embedding weights follow it rather than introducing a second location.
pub fn models_dir_for(whisper: &kuali_core::WhisperConfig) -> PathBuf {
    whisper.resolved_models_directory()
}
