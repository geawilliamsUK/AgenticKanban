//! TOML config IO with atomic writes.

use std::path::Path;

use serde::de::DeserializeOwned;
use serde::Serialize;

use ak_core::config::{HomeConfig, ProjectPolicy};

use crate::error::RuntimeError;

pub fn load_home_config(path: &Path) -> Result<HomeConfig, RuntimeError> {
    load_or_default(path)
}

pub fn save_home_config(path: &Path, config: &HomeConfig) -> Result<(), RuntimeError> {
    save(path, config)
}

pub fn load_policy(path: &Path) -> Result<ProjectPolicy, RuntimeError> {
    load_or_default(path)
}

pub fn save_policy(path: &Path, policy: &ProjectPolicy) -> Result<(), RuntimeError> {
    save(path, policy)
}

fn load_or_default<T: DeserializeOwned + Default>(path: &Path) -> Result<T, RuntimeError> {
    match std::fs::read_to_string(path) {
        Ok(text) => toml::from_str(&text).map_err(|e| RuntimeError::Parse {
            path: path.to_path_buf(),
            message: e.to_string(),
        }),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(T::default()),
        Err(e) => Err(RuntimeError::io(path, e)),
    }
}

/// Writes to a sibling temp file and renames over the target, so a crash
/// mid-write never leaves a half-written config.
fn save<T: Serialize>(path: &Path, value: &T) -> Result<(), RuntimeError> {
    let text = toml::to_string_pretty(value).map_err(|e| RuntimeError::Serialize(e.to_string()))?;
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(|e| RuntimeError::io(parent, e))?;
    }
    let tmp = path.with_extension("toml.tmp");
    std::fs::write(&tmp, text).map_err(|e| RuntimeError::io(&tmp, e))?;
    std::fs::rename(&tmp, path).map_err(|e| RuntimeError::io(path, e))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn missing_file_loads_as_default_and_round_trips() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("nested").join("config.toml");
        let cfg = load_home_config(&path).unwrap();
        assert!(cfg.projects.is_empty());
        save_home_config(&path, &cfg).unwrap();
        assert!(path.exists());
        assert!(!path.with_extension("toml.tmp").exists());
        let back = load_home_config(&path).unwrap();
        assert_eq!(cfg, back);
    }

    #[test]
    fn garbage_is_a_parse_error_naming_the_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        std::fs::write(&path, "this is = = not toml").unwrap();
        let err = load_home_config(&path).unwrap_err();
        assert!(matches!(err, RuntimeError::Parse { .. }));
        assert!(err.to_string().contains("config.toml"));
    }
}
