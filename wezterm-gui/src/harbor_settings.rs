use anyhow::{bail, Context};
use parking_lot::Mutex;
use serde::{Deserialize, Serialize};
use std::fs;
use std::io::ErrorKind;
use std::path::{Path, PathBuf};

const SCHEMA_VERSION: u32 = 1;
const DEFAULT_FONT_SCALE: f64 = 1.0;
const DEFAULT_VOICE_BASE_URL: &str = "https://openrouter.ai/api/v1";
const DEFAULT_VOICE_MODEL: &str = "deepseek/deepseek-v4-flash-0731";
const DEFAULT_VOICE_TIMEOUT_MS: u64 = 10_000;
const OPENROUTER_API_KEY_ENV_VAR: &str = "OPENROUTER_API_KEY";

fn default_voice_base_url() -> String {
    DEFAULT_VOICE_BASE_URL.to_string()
}

fn default_voice_model() -> String {
    DEFAULT_VOICE_MODEL.to_string()
}

fn default_voice_timeout_ms() -> u64 {
    DEFAULT_VOICE_TIMEOUT_MS
}

fn default_openrouter_api_key() -> String {
    String::new()
}

#[derive(Clone, Debug, Serialize, Deserialize)]
struct PersistedSettings {
    schema_version: u32,
    font_scale: f64,
    #[serde(default = "default_voice_base_url", alias = "voice_ollama_url")]
    voice_base_url: String,
    #[serde(default = "default_voice_model")]
    voice_model: String,
    #[serde(default = "default_voice_timeout_ms")]
    voice_timeout_ms: u64,
    #[serde(
        default = "default_openrouter_api_key",
        skip_serializing_if = "String::is_empty"
    )]
    openrouter_api_key: String,
}

impl Default for PersistedSettings {
    fn default() -> Self {
        Self {
            schema_version: SCHEMA_VERSION,
            font_scale: DEFAULT_FONT_SCALE,
            voice_base_url: default_voice_base_url(),
            voice_model: default_voice_model(),
            voice_timeout_ms: default_voice_timeout_ms(),
            openrouter_api_key: default_openrouter_api_key(),
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

pub struct VoiceModelSettings {
    pub base_url: String,
    pub model: String,
    pub timeout_ms: u64,
    pub api_key: String,
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

fn is_legacy_ollama_url(url: &str) -> bool {
    let base = url.trim().trim_end_matches('/');
    base == "http://127.0.0.1:11434"
        || base == "http://localhost:11434"
        || base == "http://[::1]:11434"
}

fn migrate_loaded(mut state: PersistedSettings) -> PersistedSettings {
    let was_ollama = is_legacy_ollama_url(&state.voice_base_url);
    if was_ollama || state.voice_base_url.trim().is_empty() {
        state.voice_base_url = default_voice_base_url();
    }
    if state.voice_model.trim().is_empty() || state.voice_model.trim() == "lfm2.5:latest" {
        state.voice_model = default_voice_model();
    }
    if was_ollama && state.voice_timeout_ms == 3_000 {
        state.voice_timeout_ms = DEFAULT_VOICE_TIMEOUT_MS;
    }
    state
}

pub fn parse_shell_export(contents: &str, env_var: &str) -> Option<String> {
    let prefix = format!("{env_var}=");
    for line in contents.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let rest = line.strip_prefix("export ").map(str::trim).unwrap_or(line);
        let Some(raw) = rest.strip_prefix(&prefix) else {
            continue;
        };
        let value = unquote_shell_value(raw.trim());
        if !value.is_empty() {
            return Some(value);
        }
    }
    None
}

fn unquote_shell_value(value: &str) -> String {
    let bytes = value.as_bytes();
    if bytes.len() >= 2 {
        let first = bytes[0];
        let last = bytes[bytes.len() - 1];
        if (first == b'"' && last == b'"') || (first == b'\'' && last == b'\'') {
            return value[1..value.len() - 1].to_string();
        }
    }
    value.to_string()
}

fn read_env_api_key(env_var: &str) -> Option<String> {
    if let Ok(value) = std::env::var(env_var) {
        let trimmed = value.trim();
        if !trimmed.is_empty() {
            return Some(trimmed.to_string());
        }
    }

    #[cfg(target_os = "macos")]
    {
        if let Ok(output) = std::process::Command::new("launchctl")
            .args(["getenv", env_var])
            .output()
        {
            if output.status.success() {
                if let Ok(value) = String::from_utf8(output.stdout) {
                    let trimmed = value.trim();
                    if !trimmed.is_empty() {
                        return Some(trimmed.to_string());
                    }
                }
            }
        }
    }

    None
}

fn read_zshrc_api_key(env_var: &str) -> Option<String> {
    let home = std::env::var_os("HOME")?;
    let contents = fs::read_to_string(PathBuf::from(home).join(".zshrc")).ok()?;
    parse_shell_export(&contents, env_var)
}

fn resolve_openrouter_api_key(stored: &str) -> String {
    if let Some(value) = read_env_api_key(OPENROUTER_API_KEY_ENV_VAR) {
        return value;
    }
    if let Some(value) = read_zshrc_api_key(OPENROUTER_API_KEY_ENV_VAR) {
        return value;
    }
    stored.trim().to_string()
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
            migrate_loaded(state)
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

pub fn voice_model_settings() -> VoiceModelSettings {
    let mut registry = REGISTRY.lock();
    load_if_needed(&mut registry);
    VoiceModelSettings {
        base_url: registry.state.voice_base_url.clone(),
        model: registry.state.voice_model.clone(),
        timeout_ms: registry.state.voice_timeout_ms.clamp(250, 30_000),
        api_key: resolve_openrouter_api_key(&registry.state.openrouter_api_key),
    }
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
            ..PersistedSettings::default()
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

    #[test]
    fn old_settings_gain_voice_defaults() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("settings-v1.json");
        fs::write(&path, br#"{"schema_version":1,"font_scale":1.0}"#).unwrap();
        let state = load_from_path(&path);
        assert_eq!(state.voice_base_url, DEFAULT_VOICE_BASE_URL);
        assert_eq!(state.voice_model, DEFAULT_VOICE_MODEL);
        assert_eq!(state.voice_timeout_ms, DEFAULT_VOICE_TIMEOUT_MS);
        assert!(state.openrouter_api_key.is_empty());
    }

    #[test]
    fn legacy_ollama_voice_settings_migrate() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("settings-v1.json");
        fs::write(
            &path,
            br#"{"schema_version":1,"font_scale":1.0,"voice_ollama_url":"http://127.0.0.1:11434","voice_model":"lfm2.5:latest","voice_timeout_ms":3000}"#,
        )
        .unwrap();
        let state = load_from_path(&path);
        assert_eq!(state.voice_base_url, DEFAULT_VOICE_BASE_URL);
        assert_eq!(state.voice_model, DEFAULT_VOICE_MODEL);
        assert_eq!(state.voice_timeout_ms, DEFAULT_VOICE_TIMEOUT_MS);
    }

    #[test]
    fn parse_shell_export_reads_quoted_and_skips_comments() {
        let contents = r#"
# export OPENROUTER_API_KEY=ignored
export OTHER=nope
OPENROUTER_API_KEY="sk-or-test"
"#;
        assert_eq!(
            parse_shell_export(contents, "OPENROUTER_API_KEY").as_deref(),
            Some("sk-or-test")
        );
        assert_eq!(
            parse_shell_export("export OPENROUTER_API_KEY='abc'", "OPENROUTER_API_KEY").as_deref(),
            Some("abc")
        );
        assert_eq!(parse_shell_export("", "OPENROUTER_API_KEY"), None);
    }
}
