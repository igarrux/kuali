//! Whisper weight locations and downloads.
//!
//! Weights are intentionally external. Even the smallest is 78 MB and the
//! recommended model is 574 MB; embedding them would bloat the executable and
//! require rebuilding Kuali to change models. Each weight is downloaded once.

use std::io::{BufReader, Read};
use std::path::{Path, PathBuf};

use futures::StreamExt;
use kuali_core::WhisperModel;
use sha2::{Digest, Sha256};
use tokio::io::AsyncWriteExt;

const VAD_FILE_NAME: &str = "ggml-silero-v6.2.0.bin";
const VAD_DOWNLOAD_URL: &str =
    "https://huggingface.co/ggml-org/whisper-vad/resolve/main/ggml-silero-v6.2.0.bin";
const VAD_SHA256: &str = "2aa269b785eeb53a82983a20501ddf7c1d9c48e33ab63a41391ac6c9f7fb6987";
const VAD_BYTES: u64 = 885_098;

#[derive(Debug, thiserror::Error)]
pub enum ModelError {
    #[error("failed to download model: {0}")]
    Download(#[from] reqwest::Error),
    #[error("download returned HTTP {0}")]
    HttpStatus(u16),
    #[error("disk error at {path}: {source}")]
    Io {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("downloaded model is incomplete ({got} of {expected} bytes)")]
    Truncated { got: u64, expected: u64 },
    #[error("model {path} failed integrity verification (expected {expected}, got {got})")]
    HashMismatch {
        path: PathBuf,
        expected: &'static str,
        got: String,
    },
}

/// Path for a specific model.
pub fn model_path(models_dir: &Path, model: WhisperModel) -> PathBuf {
    models_dir.join(model.file_name())
}

pub fn vad_model_path(models_dir: &Path) -> PathBuf {
    models_dir.join(VAD_FILE_NAME)
}

pub fn is_vad_downloaded(models_dir: &Path) -> bool {
    std::fs::metadata(vad_model_path(models_dir))
        .map(|metadata| metadata.is_file() && metadata.len() == VAD_BYTES)
        .unwrap_or(false)
}

pub fn verify_vad_integrity(path: &Path) -> Result<(), ModelError> {
    verify_file_integrity(path, VAD_SHA256)
}

/// A model is present when its file exists and is not an obviously truncated
/// remnant of a failed download.
pub fn is_downloaded(models_dir: &Path, model: WhisperModel) -> bool {
    std::fs::metadata(model_path(models_dir, model))
        .map(|m| m.is_file() && m.len() == model.approx_bytes())
        .unwrap_or(false)
}

/// Reads the complete weight and compares it with the official SHA-256 after a
/// relocation. This intentionally blocks and is run outside the async runtime
/// so slow external storage cannot freeze Kuali.
pub fn verify_integrity(path: &Path, model: WhisperModel) -> Result<(), ModelError> {
    verify_file_integrity(path, model.sha256())
}

fn verify_file_integrity(path: &Path, expected: &'static str) -> Result<(), ModelError> {
    let file = std::fs::File::open(path).map_err(|source| ModelError::Io {
        path: path.to_path_buf(),
        source,
    })?;
    let mut reader = BufReader::with_capacity(1024 * 1024, file);
    let mut hasher = Sha256::new();
    let mut buffer = vec![0_u8; 1024 * 1024];
    loop {
        let read = reader.read(&mut buffer).map_err(|source| ModelError::Io {
            path: path.to_path_buf(),
            source,
        })?;
        if read == 0 {
            break;
        }
        hasher.update(&buffer[..read]);
    }

    verify_expected_digest(path, expected, format!("{:x}", hasher.finalize()))
}

fn verify_digest(path: &Path, model: WhisperModel, got: String) -> Result<(), ModelError> {
    verify_expected_digest(path, model.sha256(), got)
}

fn verify_expected_digest(
    path: &Path,
    expected: &'static str,
    got: String,
) -> Result<(), ModelError> {
    if got == expected {
        return Ok(());
    }
    Err(ModelError::HashMismatch {
        path: path.to_path_buf(),
        expected,
        got,
    })
}

/// Downloads the small Silero model that separates speech from noise before
/// every Whisper pass. It is validated like primary weights and stored beside them.
pub async fn ensure_vad_downloaded<F>(
    models_dir: &Path,
    mut on_progress: F,
) -> Result<PathBuf, ModelError>
where
    F: FnMut(u64, Option<u64>) + Send,
{
    let final_path = vad_model_path(models_dir);
    if is_vad_downloaded(models_dir) {
        return Ok(final_path);
    }
    if let Some(dir) = final_path.parent() {
        std::fs::create_dir_all(dir).map_err(|source| ModelError::Io {
            path: dir.to_path_buf(),
            source,
        })?;
    }

    let partial = final_path.with_extension("part");
    let response = reqwest::Client::new().get(VAD_DOWNLOAD_URL).send().await?;
    if !response.status().is_success() {
        return Err(ModelError::HttpStatus(response.status().as_u16()));
    }

    let mut hasher = Sha256::new();
    let mut file = tokio::fs::File::create(&partial)
        .await
        .map_err(|source| ModelError::Io {
            path: partial.clone(),
            source,
        })?;
    let mut downloaded = 0u64;
    let mut stream = response.bytes_stream();
    while let Some(chunk) = stream.next().await {
        let chunk = chunk?;
        hasher.update(&chunk);
        file.write_all(&chunk)
            .await
            .map_err(|source| ModelError::Io {
                path: partial.clone(),
                source,
            })?;
        downloaded += chunk.len() as u64;
        on_progress(downloaded, Some(VAD_BYTES));
    }
    file.flush().await.map_err(|source| ModelError::Io {
        path: partial.clone(),
        source,
    })?;
    drop(file);

    if downloaded != VAD_BYTES {
        let _ = std::fs::remove_file(&partial);
        return Err(ModelError::Truncated {
            got: downloaded,
            expected: VAD_BYTES,
        });
    }
    if let Err(error) =
        verify_expected_digest(&partial, VAD_SHA256, format!("{:x}", hasher.finalize()))
    {
        let _ = std::fs::remove_file(&partial);
        return Err(error);
    }
    if final_path.exists() {
        std::fs::remove_file(&final_path).map_err(|source| ModelError::Io {
            path: final_path.clone(),
            source,
        })?;
    }
    std::fs::rename(&partial, &final_path).map_err(|source| ModelError::Io {
        path: final_path.clone(),
        source,
    })?;
    Ok(final_path)
}

/// Bytes already downloaded, allowing progress to resume after an app restart.
pub fn downloaded_bytes(models_dir: &Path, model: WhisperModel) -> u64 {
    std::fs::metadata(model_path(models_dir, model))
        .map(|m| m.len())
        .unwrap_or(0)
}

/// Ensures a model exists on disk and returns its path.
///
/// Downloads to `.part` and renames only when complete. After interruption, the
/// next launch sees no model instead of loading a partial file in whisper.cpp.
pub async fn ensure_downloaded<F>(
    models_dir: &Path,
    model: WhisperModel,
    mut on_progress: F,
) -> Result<PathBuf, ModelError>
where
    F: FnMut(u64, Option<u64>) + Send,
{
    let final_path = model_path(models_dir, model);
    if is_downloaded(models_dir, model) {
        return Ok(final_path);
    }

    if let Some(dir) = final_path.parent() {
        std::fs::create_dir_all(dir).map_err(|source| ModelError::Io {
            path: dir.to_path_buf(),
            source,
        })?;
    }

    let partial = final_path.with_extension("part");
    let response = reqwest::Client::builder()
        // No global timeout: downloading 1.6 GB over a slow connection can take
        // a long time without indicating failure.
        .build()?
        .get(model.download_url())
        .send()
        .await?;

    if !response.status().is_success() {
        return Err(ModelError::HttpStatus(response.status().as_u16()));
    }

    let total = response.content_length();
    let mut hasher = Sha256::new();
    let mut file = tokio::fs::File::create(&partial)
        .await
        .map_err(|source| ModelError::Io {
            path: partial.clone(),
            source,
        })?;

    let mut downloaded = 0u64;
    let mut stream = response.bytes_stream();
    while let Some(chunk) = stream.next().await {
        let chunk = chunk?;
        hasher.update(&chunk);
        file.write_all(&chunk)
            .await
            .map_err(|source| ModelError::Io {
                path: partial.clone(),
                source,
            })?;
        downloaded += chunk.len() as u64;
        on_progress(downloaded, total);
    }
    file.flush().await.map_err(|source| ModelError::Io {
        path: partial.clone(),
        source,
    })?;
    drop(file);

    if let Some(expected) = total {
        if downloaded < expected {
            let _ = std::fs::remove_file(&partial);
            return Err(ModelError::Truncated {
                got: downloaded,
                expected,
            });
        }
    }

    if let Err(error) = verify_digest(&partial, model, format!("{:x}", hasher.finalize())) {
        let _ = std::fs::remove_file(&partial);
        return Err(error);
    }

    if final_path.exists() {
        std::fs::remove_file(&final_path).map_err(|source| ModelError::Io {
            path: final_path.clone(),
            source,
        })?;
    }
    std::fs::rename(&partial, &final_path).map_err(|source| ModelError::Io {
        path: final_path.clone(),
        source,
    })?;
    Ok(final_path)
}

/// Removes one model's weights to free storage from Settings.
pub fn remove(models_dir: &Path, model: WhisperModel) -> std::io::Result<()> {
    for path in [
        model_path(models_dir, model),
        partial_path(models_dir, model),
    ] {
        remove_file_if_present(&path)?;
    }
    Ok(())
}

/// Removes every recognized Whisper weight from a directory.
///
/// Used during factory reset. A user-selected directory may contain unrelated
/// files, so this never calls `remove_dir_all`; it touches only official names,
/// partial downloads, and interrupted-relocation files. Silero is deliberately
/// preserved as shared recognition infrastructure rather than a selectable
/// Whisper weight.
pub fn remove_all_whisper_weights(models_dir: &Path) -> std::io::Result<u64> {
    let mut removed_bytes = 0;
    for model in WhisperModel::ALL {
        let final_path = model_path(models_dir, model);
        let partial = partial_path(models_dir, model);
        for path in [
            final_path.clone(),
            partial.clone(),
            relocation_staging_path(&final_path),
            relocation_staging_path(&partial),
        ] {
            removed_bytes += std::fs::metadata(&path)
                .map(|metadata| metadata.len())
                .unwrap_or(0);
            remove_file_if_present(&path)?;
        }
    }
    Ok(removed_bytes)
}

fn remove_file_if_present(path: &Path) -> std::io::Result<()> {
    match std::fs::remove_file(path) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error),
    }
}

/// Currently installed models with their actual size.
pub fn installed(models_dir: &Path) -> Vec<(WhisperModel, u64)> {
    WhisperModel::ALL
        .iter()
        .copied()
        .filter(|m| is_downloaded(models_dir, *m))
        .map(|m| (m, downloaded_bytes(models_dir, m)))
        .collect()
}

pub fn partial_path(models_dir: &Path, model: WhisperModel) -> PathBuf {
    model_path(models_dir, model).with_extension("part")
}

/// Relocates every weight and partial download recognized by Kuali.
///
/// It first attempts an instant `rename`. Across volumes, it copies to a staged
/// destination file, verifies size, and only then removes the source. If a
/// destination copy already exists, the larger copy wins and the old duplicate
/// is removed.
pub fn relocate_models(source: &Path, destination: &Path) -> std::io::Result<usize> {
    if source == destination || !source.is_dir() {
        return Ok(0);
    }
    std::fs::create_dir_all(destination)?;

    let mut relocated = 0;
    for model in WhisperModel::ALL {
        for source_path in [model_path(source, model), partial_path(source, model)] {
            if !source_path.is_file() {
                continue;
            }
            let Some(file_name) = source_path.file_name() else {
                continue;
            };
            let target_path = destination.join(file_name);
            relocate_file(&source_path, &target_path)?;
            relocated += 1;
        }
    }
    for source_path in [
        vad_model_path(source),
        vad_model_path(source).with_extension("part"),
    ] {
        if !source_path.is_file() {
            continue;
        }
        let Some(file_name) = source_path.file_name() else {
            continue;
        };
        relocate_file(&source_path, &destination.join(file_name))?;
        relocated += 1;
    }

    // Remove the old directory only when it is truly empty. Never touch unrelated
    // files the user may store beside model weights.
    match std::fs::remove_dir(source) {
        Ok(()) => {}
        Err(error)
            if matches!(
                error.kind(),
                std::io::ErrorKind::NotFound | std::io::ErrorKind::DirectoryNotEmpty
            ) => {}
        Err(error) => return Err(error),
    }
    Ok(relocated)
}

fn relocate_file(source: &Path, target: &Path) -> std::io::Result<()> {
    if target.is_file() {
        let source_len = std::fs::metadata(source)?.len();
        let target_len = std::fs::metadata(target)?.len();
        if target_len >= source_len {
            return std::fs::remove_file(source);
        }
    }

    let staged = relocation_staging_path(target);
    if staged.exists() {
        std::fs::remove_file(&staged)?;
    }

    let source_len = std::fs::metadata(source)?.len();
    let moved_by_rename = match std::fs::rename(source, &staged) {
        Ok(()) => true,
        Err(_) => {
            let copied = std::fs::copy(source, &staged)?;
            if copied != source_len {
                let _ = std::fs::remove_file(&staged);
                return Err(std::io::Error::new(
                    std::io::ErrorKind::UnexpectedEof,
                    format!(
                        "copy of {} is incomplete ({copied} of {source_len} bytes)",
                        source.display()
                    ),
                ));
            }
            false
        }
    };

    if target.exists() {
        std::fs::remove_file(target)?;
    }
    std::fs::rename(&staged, target)?;
    if !moved_by_rename {
        std::fs::remove_file(source)?;
    }
    Ok(())
}

fn relocation_staging_path(target: &Path) -> PathBuf {
    let extension = target
        .extension()
        .and_then(|extension| extension.to_str())
        .unwrap_or("bin");
    target.with_extension(format!("{extension}.kuali-moving"))
}

/// Moves legacy weights into `~/.kuali` on the first launch with new configuration.
pub fn migrate_legacy_models() -> std::io::Result<usize> {
    let legacy = kuali_core::paths::legacy_models_dir();
    let destination = kuali_core::paths::models_dir();
    relocate_models(&legacy, &destination)
}

pub fn exists(path: &Path) -> bool {
    path.is_file()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_model_resolves_to_a_distinct_path_under_the_models_dir() {
        let dir = kuali_core::paths::models_dir();
        let mut paths: Vec<_> = WhisperModel::ALL
            .iter()
            .map(|m| model_path(&dir, *m))
            .collect();
        assert!(paths.iter().all(|p| p.starts_with(&dir)));

        let count = paths.len();
        paths.sort();
        paths.dedup();
        assert_eq!(paths.len(), count);
    }

    #[test]
    fn download_urls_point_at_the_matching_file_name() {
        for model in WhisperModel::ALL {
            assert!(
                model.download_url().contains(model.file_name()),
                "{:?} apunta a otro fichero",
                model
            );
        }
    }

    #[test]
    fn the_partial_file_is_never_the_final_file() {
        let dir = kuali_core::paths::models_dir();
        for model in WhisperModel::ALL {
            assert_ne!(partial_path(&dir, model), model_path(&dir, model));
        }
        assert_ne!(
            vad_model_path(&dir),
            vad_model_path(&dir).with_extension("part")
        );
    }

    #[test]
    fn the_official_digest_is_accepted_and_any_other_digest_is_rejected() {
        let model = WhisperModel::Tiny;
        let path = Path::new("ggml-tiny.bin");
        assert!(verify_digest(path, model, model.sha256().to_string()).is_ok());

        let error = verify_digest(path, model, "0".repeat(64)).unwrap_err();
        assert!(matches!(error, ModelError::HashMismatch { .. }));
    }

    #[test]
    fn relocating_models_moves_weights_and_partial_downloads() {
        let root = std::env::temp_dir().join(format!("kuali-model-move-{}", std::process::id()));
        let source = root.join("old");
        let destination = root.join("new");
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&source).unwrap();

        let model = WhisperModel::Tiny;
        std::fs::write(model_path(&source, model), b"pesos").unwrap();
        std::fs::write(partial_path(&source, model), b"parcial").unwrap();

        assert_eq!(relocate_models(&source, &destination).unwrap(), 2);
        assert!(!model_path(&source, model).exists());
        assert!(!partial_path(&source, model).exists());
        assert_eq!(
            std::fs::read(model_path(&destination, model)).unwrap(),
            b"pesos"
        );
        assert_eq!(
            std::fs::read(partial_path(&destination, model)).unwrap(),
            b"parcial"
        );
        std::fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn relocating_a_duplicate_keeps_the_larger_copy_and_removes_the_old_one() {
        let root =
            std::env::temp_dir().join(format!("kuali-model-duplicate-{}", std::process::id()));
        let source = root.join("old");
        let destination = root.join("new");
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&source).unwrap();
        std::fs::create_dir_all(&destination).unwrap();

        let model = WhisperModel::Tiny;
        std::fs::write(model_path(&source, model), b"copia completa").unwrap();
        std::fs::write(model_path(&destination, model), b"corta").unwrap();

        assert_eq!(relocate_models(&source, &destination).unwrap(), 1);
        assert!(!model_path(&source, model).exists());
        assert_eq!(
            std::fs::read(model_path(&destination, model)).unwrap(),
            b"copia completa"
        );
        std::fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn relocating_models_also_moves_the_shared_vad_weight() {
        let root = std::env::temp_dir().join(format!("kuali-vad-move-{}", std::process::id()));
        let source = root.join("old");
        let destination = root.join("new");
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&source).unwrap();
        std::fs::write(vad_model_path(&source), b"vad").unwrap();

        assert_eq!(relocate_models(&source, &destination).unwrap(), 1);
        assert!(!vad_model_path(&source).exists());
        assert_eq!(std::fs::read(vad_model_path(&destination)).unwrap(), b"vad");
        std::fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn removing_a_model_also_removes_an_abandoned_partial_download() {
        let root = std::env::temp_dir().join(format!("kuali-model-remove-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).unwrap();
        let model = WhisperModel::Tiny;
        std::fs::write(model_path(&root, model), b"pesos").unwrap();
        std::fs::write(partial_path(&root, model), b"parcial").unwrap();
        std::fs::write(vad_model_path(&root), b"vad").unwrap();
        std::fs::write(vad_model_path(&root).with_extension("part"), b"vad parcial").unwrap();

        remove(&root, model).unwrap();

        assert!(!model_path(&root, model).exists());
        assert!(!partial_path(&root, model).exists());
        assert!(
            vad_model_path(&root).exists(),
            "Silero is shared across models"
        );
        assert!(
            vad_model_path(&root).with_extension("part").exists(),
            "even a partial Silero download does not belong to the selected weight"
        );
        std::fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn factory_reset_removes_only_whisper_weights() {
        let root = std::env::temp_dir().join(format!("kuali-model-reset-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).unwrap();

        let first = WhisperModel::Tiny;
        let second = WhisperModel::LargeV3TurboQ5;
        let first_path = model_path(&root, first);
        let second_partial = partial_path(&root, second);
        std::fs::write(&first_path, b"peso").unwrap();
        std::fs::write(&second_partial, b"parcial").unwrap();
        std::fs::write(relocation_staging_path(&first_path), b"mudanza").unwrap();
        std::fs::write(vad_model_path(&root), b"silero").unwrap();
        std::fs::write(
            vad_model_path(&root).with_extension("part"),
            b"silero parcial",
        )
        .unwrap();
        std::fs::write(root.join("notas-del-usuario.txt"), b"no tocar").unwrap();

        assert_eq!(remove_all_whisper_weights(&root).unwrap(), 18);
        assert!(!first_path.exists());
        assert!(!second_partial.exists());
        assert!(vad_model_path(&root).exists(), "Silero must be preserved");
        assert!(vad_model_path(&root).with_extension("part").exists());
        assert!(root.join("notas-del-usuario.txt").exists());
        assert!(root.is_dir(), "an external directory is never removed wholesale");

        std::fs::remove_dir_all(&root).unwrap();
    }

    /// Manual distribution test that downloads 574 MB from the public release.
    /// It stays ignored in regular suites to avoid burdening every build and
    /// GitHub while still validating the real network path and SHA-256.
    #[tokio::test]
    #[ignore = "downloads the complete LatAm Q5 weight from GitHub"]
    async fn public_latam_q5_release_downloads_and_verifies_end_to_end() {
        let root = std::env::temp_dir().join(format!(
            "kuali-latam-release-download-{}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&root);

        let model = WhisperModel::LargeV3TurboLatamQ5;
        let path = ensure_downloaded(&root, model, |_, _| {})
            .await
            .expect("download the public LatAm Q5 release");

        assert_eq!(path, model_path(&root, model));
        assert!(is_downloaded(&root, model));
        verify_integrity(&path, model).expect("verify the downloaded SHA-256");
        std::fs::remove_dir_all(&root).unwrap();
    }
}
