use anyhow::{bail, Context};
use parking_lot::Mutex;
use serde::{Deserialize, Serialize};
use std::fs;
use std::io::ErrorKind;
use std::path::{Path, PathBuf};

const SCHEMA_VERSION: u32 = 1;
const DEFAULT_FONT_SCALE: f64 = 1.0;

#[derive(Clone, Debug, Serialize, Deserialize)]
struct PersistedSettings {
    schema_version: u32,
    font_scale: f64,
}

impl Default for PersistedSettings {
    fn default() -> Self {
        Self {
            schema_version: SCHEMA_VERSION,
            font_scale: DEFAULT_FONT_SCALE,
        }
    }
}

#[derive(Default)]
struct SettingsRegistry {
    state: PersistedSettings,
    loaded: bool,
}

lazy_static::lazy_static! {
    static ref REGISTRY: Mutex<SettingsRegistry> = Mutex::new(SettingsRegistry::default());
}

fn state_dir() -> PathBuf {
    dirs_next::data_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join("terminal-harbor")
}

fn settings_path() -> PathBuf {
    state_dir().join("settings-v1.json")
}

fn valid_font_scale(font_scale: f64) -> bool {
    font_scale.is_finite() && font_scale > 0.0
}

fn load_from_path(path: &Path) -> PersistedSettings {
    let data = match fs::read(path) {
        Ok(data) => data,
        Err(err) if err.kind() == ErrorKind::NotFound => return PersistedSettings::default(),
        Err(err) => {
            log::warn!(
                "loading Terminal Harbor settings from {}: {err:#}",
                path.display()
            );
            return PersistedSettings::default();
        }
    };

    match serde_json::from_slice::<PersistedSettings>(&data) {
        Ok(state)
            if state.schema_version == SCHEMA_VERSION && valid_font_scale(state.font_scale) =>
        {
            state
        }
        Ok(_) => {
            log::warn!("ignoring unsupported or invalid Terminal Harbor settings");
            PersistedSettings::default()
        }
        Err(err) => {
            log::warn!(
                "parsing Terminal Harbor settings from {}: {err:#}",
                path.display()
            );
            PersistedSettings::default()
        }
    }
}

fn save_to_path(path: &Path, state: &PersistedSettings) -> anyhow::Result<()> {
    let dir = path
        .parent()
        .context("Terminal Harbor settings path has no parent directory")?;
    fs::create_dir_all(dir)?;
    let temp = path.with_extension("json.tmp");
    fs::write(&temp, serde_json::to_vec_pretty(state)?)?;
    fs::rename(temp, path)?;
    Ok(())
}

fn load_if_needed(registry: &mut SettingsRegistry) {
    if registry.loaded {
        return;
    }
    registry.loaded = true;
    registry.state = load_from_path(&settings_path());
}

pub fn font_scale() -> f64 {
    let mut registry = REGISTRY.lock();
    load_if_needed(&mut registry);
    registry.state.font_scale
}

pub fn set_font_scale(font_scale: f64) -> anyhow::Result<()> {
    if !valid_font_scale(font_scale) {
        bail!("invalid font scale {font_scale}");
    }

    let mut registry = REGISTRY.lock();
    load_if_needed(&mut registry);
    registry.state.font_scale = font_scale;
    save_to_path(&settings_path(), &registry.state)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn missing_settings_use_default_font_scale() {
        let dir = tempfile::tempdir().unwrap();
        let state = load_from_path(&dir.path().join("missing.json"));
        assert_eq!(state.font_scale, DEFAULT_FONT_SCALE);
    }

    #[test]
    fn font_scale_round_trips() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("settings-v1.json");
        let state = PersistedSettings {
            schema_version: SCHEMA_VERSION,
            font_scale: 1.21,
        };
        save_to_path(&path, &state).unwrap();
        assert_eq!(load_from_path(&path).font_scale, 1.21);
    }

    #[test]
    fn invalid_settings_use_default_font_scale() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("settings-v1.json");

        fs::write(&path, b"not json").unwrap();
        assert_eq!(load_from_path(&path).font_scale, DEFAULT_FONT_SCALE);

        fs::write(&path, br#"{"schema_version":1,"font_scale":0.0}"#).unwrap();
        assert_eq!(load_from_path(&path).font_scale, DEFAULT_FONT_SCALE);

        assert!(!valid_font_scale(f64::NAN));
        assert!(!valid_font_scale(f64::INFINITY));
    }
}
