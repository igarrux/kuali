//! Kuali's on-disk locations and configuration persistence.

use std::path::{Path, PathBuf};

use directories::{BaseDirs, ProjectDirs};

use crate::config::KualiConfig;

#[derive(Debug, thiserror::Error)]
pub enum ConfigError {
    #[error("no se pudo leer {path}: {source}")]
    Read {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("no se pudo escribir {path}: {source}")]
    Write {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("configuration file contains a syntax error: {0}")]
    Parse(#[from] toml::de::Error),
    #[error("failed to serialize configuration: {0}")]
    Serialize(#[from] toml::ser::Error),
}

fn project_dirs() -> Option<ProjectDirs> {
    ProjectDirs::from("com", "onwev", "Kuali")
}

fn home_fallback() -> PathBuf {
    BaseDirs::new()
        .map(|dirs| dirs.home_dir().to_path_buf())
        .or_else(|| std::env::var_os("HOME").map(PathBuf::from))
        .unwrap_or_else(|| PathBuf::from("."))
        .join(".kuali")
}

pub fn config_dir() -> PathBuf {
    project_dirs()
        .map(|d| d.config_dir().to_path_buf())
        .unwrap_or_else(home_fallback)
}

pub fn data_dir() -> PathBuf {
    project_dirs()
        .map(|d| d.data_dir().to_path_buf())
        .unwrap_or_else(home_fallback)
}

/// Whisper weights, downloaded once and reused.
pub fn models_dir() -> PathBuf {
    home_fallback()
}

/// Location used before model storage became configurable. It remains only to
/// migrate existing installations without downloading gigabytes again.
pub fn legacy_models_dir() -> PathBuf {
    data_dir().join("models")
}

/// Resolves the user preference. The picker returns absolute paths, while hand-
/// edited `config.toml` files may also use `~`.
pub fn resolve_models_dir(configured: Option<&Path>) -> PathBuf {
    let Some(path) = configured.filter(|path| !path.as_os_str().is_empty()) else {
        return models_dir();
    };
    if path == Path::new("~") {
        return home_fallback()
            .parent()
            .map(Path::to_path_buf)
            .unwrap_or_else(|| PathBuf::from("."));
    }
    if let Ok(rest) = path.strip_prefix("~") {
        let home = home_fallback()
            .parent()
            .map(Path::to_path_buf)
            .unwrap_or_else(|| PathBuf::from("."));
        return home.join(rest);
    }
    path.to_path_buf()
}

/// One directory per meeting, containing its transcript, summary, and exports.
pub fn meetings_dir() -> PathBuf {
    data_dir().join("meetings")
}

pub fn config_file() -> PathBuf {
    config_dir().join("config.toml")
}

/// Loads configuration. A missing file returns defaults, as required on first launch.
pub fn load_config() -> Result<KualiConfig, ConfigError> {
    let path = config_file();
    match std::fs::read_to_string(&path) {
        Ok(text) => Ok(toml::from_str::<KualiConfig>(&text)?.migrated()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(KualiConfig::default()),
        Err(source) => Err(ConfigError::Read { path, source }),
    }
}

/// Writes configuration atomically through a temporary file and rename, keeping
/// the previous file intact if Kuali exits during the operation.
pub fn save_config(config: &KualiConfig) -> Result<(), ConfigError> {
    let path = config_file();
    let dir = path.parent().unwrap_or(Path::new(".")).to_path_buf();
    std::fs::create_dir_all(&dir).map_err(|source| ConfigError::Write {
        path: dir.clone(),
        source,
    })?;

    let text = toml::to_string_pretty(config)?;
    let tmp = path.with_extension("toml.tmp");
    std::fs::write(&tmp, text.as_bytes()).map_err(|source| ConfigError::Write {
        path: tmp.clone(),
        source,
    })?;
    restrict_permissions(&tmp)?;
    std::fs::rename(&tmp, &path).map_err(|source| ConfigError::Write {
        path: path.clone(),
        source,
    })?;
    Ok(())
}

/// The file contains the bot token and must remain owner-readable only.
#[cfg(unix)]
fn restrict_permissions(path: &Path) -> Result<(), ConfigError> {
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600)).map_err(|source| {
        ConfigError::Write {
            path: path.to_path_buf(),
            source,
        }
    })
}

#[cfg(not(unix))]
fn restrict_permissions(_path: &Path) -> Result<(), ConfigError> {
    Ok(())
}

/// Idempotently creates Kuali's required directories.
pub fn ensure_dirs() -> std::io::Result<()> {
    std::fs::create_dir_all(config_dir())?;
    std::fs::create_dir_all(models_dir())?;
    std::fs::create_dir_all(meetings_dir())?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn models_default_to_the_dot_kuali_folder() {
        assert!(models_dir().ends_with(".kuali"));
        assert_eq!(resolve_models_dir(None), models_dir());
    }

    #[test]
    fn an_explicit_models_directory_wins_over_the_default() {
        assert_eq!(
            resolve_models_dir(Some(Path::new("/tmp/mis-modelos"))),
            PathBuf::from("/tmp/mis-modelos")
        );
    }

    #[test]
    fn a_tilde_models_directory_expands_from_home() {
        assert_eq!(
            resolve_models_dir(Some(Path::new("~/.kuali"))),
            models_dir()
        );
    }

    #[test]
    fn meetings_still_live_under_the_application_data_dir() {
        assert!(meetings_dir().starts_with(data_dir()));
    }

    #[test]
    fn config_file_sits_in_the_config_dir() {
        assert_eq!(config_file().parent().unwrap(), config_dir());
    }
}
