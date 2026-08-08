//! Safely resets Kuali's persistent data.
//!
//! A reset is scheduled before restart and applied on the next launch, when no
//! meeting, summary, or download can write the same files. External directories
//! are never removed wholesale.

use std::path::{Path, PathBuf};

use kuali_core::KualiConfig;
use serde::{Deserialize, Serialize};

pub const CONFIRMATION_ES: &str = "Dejar Kuali como recién instalado";
pub const CONFIRMATION_EN: &str = "Reset Kuali as if newly installed";

const PENDING_FILE: &str = "factory-reset.pending.json";
const COMPLETED_FILE: &str = "factory-reset.completed";

#[derive(Debug, Serialize, Deserialize)]
struct PendingReset {
    model_directories: Vec<PathBuf>,
}

struct ResetLocations {
    config: PathBuf,
    meetings: PathBuf,
    pending: PathBuf,
    completed: PathBuf,
}

impl ResetLocations {
    fn production() -> Self {
        Self {
            config: kuali_core::paths::config_file(),
            meetings: kuali_core::paths::meetings_dir(),
            pending: pending_file(),
            completed: completed_file(),
        }
    }
}

fn pending_file() -> PathBuf {
    kuali_core::paths::config_dir().join(PENDING_FILE)
}

fn completed_file() -> PathBuf {
    kuali_core::paths::config_dir().join(COMPLETED_FILE)
}

pub fn confirmation_matches(value: &str) -> bool {
    matches!(value, CONFIRMATION_ES | CONFIRMATION_EN)
}

/// Records model paths before deleting `config.toml` so external locations
/// remain known to the launch that performs the reset.
pub fn schedule(config: &KualiConfig) -> Result<(), String> {
    let mut model_directories = Vec::new();
    for directory in [
        config.whisper.resolved_models_directory(),
        kuali_core::paths::models_dir(),
        kuali_core::paths::legacy_models_dir(),
    ] {
        if !model_directories.contains(&directory) {
            model_directories.push(directory);
        }
    }

    let request = PendingReset { model_directories };
    let path = pending_file();
    let directory = path
        .parent()
        .ok_or_else(|| "la solicitud de restablecimiento no tiene carpeta".to_string())?;
    std::fs::create_dir_all(directory).map_err(|error| at(directory, error))?;
    remove_file_if_present(&completed_file())?;

    let temporary = path.with_extension("json.tmp");
    let bytes = serde_json::to_vec_pretty(&request).map_err(|error| error.to_string())?;
    std::fs::write(&temporary, bytes).map_err(|error| at(&temporary, error))?;
    restrict_permissions(&temporary)?;
    remove_file_if_present(&path)?;
    std::fs::rename(&temporary, &path).map_err(|error| at(&path, error))?;
    Ok(())
}

/// Applies a pending reset idempotently. If interrupted, the marker remains and
/// the next launch retries the operation.
pub fn apply_pending() -> Result<bool, String> {
    apply_pending_at(&ResetLocations::production())
}

fn apply_pending_at(locations: &ResetLocations) -> Result<bool, String> {
    let bytes = match std::fs::read(&locations.pending) {
        Ok(bytes) => bytes,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(false),
        Err(error) => return Err(at(&locations.pending, error)),
    };
    let request: PendingReset = serde_json::from_slice(&bytes)
        .map_err(|error| format!("{}: {error}", locations.pending.display()))?;

    // Remove secrets first. The marker already preserves the only preference
    // required to finish cleaning external model locations.
    remove_file_if_present(&locations.config)?;
    remove_file_if_present(&locations.config.with_extension("toml.tmp"))?;

    // Kuali owns this entire directory: meetings, transcripts, tasks, and the
    // consent-announcement audit trail all live here.
    remove_directory_if_present(&locations.meetings)?;

    for directory in request.model_directories {
        kuali_stt::model::remove_all_whisper_weights(&directory)
            .map_err(|error| at(&directory, error))?;
    }

    // The frontend consumes this marker before choosing a screen so it can also
    // clear its small onboarding state.
    std::fs::write(&locations.completed, b"completed\n")
        .map_err(|error| at(&locations.completed, error))?;
    restrict_permissions(&locations.completed)?;
    remove_file_if_present(&locations.pending)?;
    Ok(true)
}

pub fn take_completed() -> Result<bool, String> {
    let path = completed_file();
    if !path.exists() {
        return Ok(false);
    }
    remove_file_if_present(&path)?;
    Ok(true)
}

fn remove_file_if_present(path: &Path) -> Result<(), String> {
    match std::fs::remove_file(path) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(at(path, error)),
    }
}

fn remove_directory_if_present(path: &Path) -> Result<(), String> {
    match std::fs::remove_dir_all(path) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(at(path, error)),
    }
}

fn at(path: &Path, error: std::io::Error) -> String {
    format!("{}: {error}", path.display())
}

#[cfg(unix)]
fn restrict_permissions(path: &Path) -> Result<(), String> {
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))
        .map_err(|error| at(path, error))
}

#[cfg(not(unix))]
fn restrict_permissions(_path: &Path) -> Result<(), String> {
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn confirmation_is_exact_and_bilingual() {
        assert!(confirmation_matches(CONFIRMATION_ES));
        assert!(confirmation_matches(CONFIRMATION_EN));
        assert!(!confirmation_matches("dejar Kuali como recién instalado"));
        assert!(!confirmation_matches("Dejar Kuali como recién instalado "));
        assert!(!confirmation_matches("Reset Kuali"));
    }

    #[test]
    fn pending_reset_cleans_owned_data_without_touching_silero_or_foreign_files() {
        let root = std::env::temp_dir().join(format!("kuali-factory-reset-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let config_dir = root.join("config");
        let meetings = root.join("data/meetings");
        let models = root.join("external-models");
        std::fs::create_dir_all(&config_dir).unwrap();
        std::fs::create_dir_all(meetings.join("meeting-1")).unwrap();
        std::fs::create_dir_all(&models).unwrap();

        let locations = ResetLocations {
            config: config_dir.join("config.toml"),
            meetings: meetings.clone(),
            pending: config_dir.join(PENDING_FILE),
            completed: config_dir.join(COMPLETED_FILE),
        };
        std::fs::write(&locations.config, b"bot-token = 'secret'").unwrap();
        std::fs::write(locations.config.with_extension("toml.tmp"), b"secret").unwrap();
        std::fs::write(meetings.join("meeting-1/meeting.json"), b"transcript").unwrap();
        std::fs::write(meetings.join("consent-audit.jsonl"), b"consent").unwrap();

        let model = kuali_core::WhisperModel::Tiny;
        std::fs::write(kuali_stt::model_path(&models, model), b"weights").unwrap();
        std::fs::write(kuali_stt::vad_model_path(&models), b"silero").unwrap();
        std::fs::write(
            kuali_stt::vad_model_path(&models).with_extension("part"),
            b"silero partial",
        )
        .unwrap();
        std::fs::write(models.join("vacaciones.jpg"), b"foreign").unwrap();
        std::fs::write(
            &locations.pending,
            serde_json::to_vec(&PendingReset {
                model_directories: vec![models.clone()],
            })
            .unwrap(),
        )
        .unwrap();

        assert!(apply_pending_at(&locations).unwrap());
        assert!(!locations.config.exists());
        assert!(!locations.config.with_extension("toml.tmp").exists());
        assert!(!meetings.exists());
        assert!(!kuali_stt::model_path(&models, model).exists());
        assert!(kuali_stt::vad_model_path(&models).exists());
        assert!(kuali_stt::vad_model_path(&models)
            .with_extension("part")
            .exists());
        assert!(models.join("vacaciones.jpg").exists());
        assert!(!locations.pending.exists());
        assert!(locations.completed.exists());

        std::fs::remove_dir_all(&root).unwrap();
    }
}
