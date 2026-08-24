//! Configuration: one TOML file, every key defaulted, unknown keys rejected.
//!
//! `docs/config.example.toml` is the documentation for every key; a test pins it equal to
//! `Config::default()` so the two cannot drift.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

/// The commented default file shipped in the binary and written by `byovox config --init`.
pub const EXAMPLE: &str = include_str!("../docs/config.example.toml");

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
#[serde(default, deny_unknown_fields)]
pub struct Config {
    pub stt: SttConfig,
    pub language: LanguageConfig,
    pub polish: PolishConfig,
    pub hotkey: HotkeyConfig,
    pub inject: InjectConfig,
    pub indicator: IndicatorConfig,
    pub capture_log: CaptureLogConfig,
    pub logging: LoggingConfig,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct SttConfig {
    pub base_url: String,
    pub model: String,
    pub api_key_env: String,
    pub prompt: String,
    pub timeout_s: u64,
}
impl Default for SttConfig {
    fn default() -> Self {
        Self {
            base_url: "http://your-whisper-host:8770/v1".into(),
            model: "whisper-1".into(),
            api_key_env: String::new(),
            prompt: String::new(),
            timeout_s: 30,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct LanguageConfig {
    /// "auto" or an ISO 639-1 code used when the layout is unmapped or unreadable.
    pub default: String,
    pub candidates: Vec<String>,
    /// keyboard layout language (ISO 639-1) -> explicit STT language.
    pub by_layout: std::collections::BTreeMap<String, String>,
}
impl Default for LanguageConfig {
    fn default() -> Self {
        Self {
            default: "auto".into(),
            candidates: Vec::new(),
            by_layout: Default::default(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct PolishConfig {
    pub enabled: bool,
    pub base_url: String,
    pub model: String,
    pub api_key_env: String,
    pub api_key_file: String,
    pub min_words: usize,
    pub prompt_file: String,
    pub timeout_s: u64,
}
impl Default for PolishConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            base_url: "http://your-llm-gateway:4000/v1".into(),
            model: "your-cleanup-alias".into(),
            api_key_env: String::new(),
            api_key_file: String::new(),
            min_words: 0,
            prompt_file: String::new(),
            timeout_s: 20,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct HotkeyConfig {
    /// W3C UI Events `code` name: ControlRight, AltRight, F13 ...
    pub key: String,
    /// "hold" or "toggle"
    pub mode: String,
    pub min_hold_ms: u64,
    pub cancel_key: String,
}
impl Default for HotkeyConfig {
    fn default() -> Self {
        Self {
            key: "ControlRight".into(),
            mode: "hold".into(),
            min_hold_ms: 250,
            cancel_key: "Escape".into(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct InjectConfig {
    /// auto | type | paste | clipboard-only
    pub mode: String,
    pub trailing_space: bool,
}
impl Default for InjectConfig {
    fn default() -> Self {
        Self {
            mode: "auto".into(),
            trailing_space: false,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct IndicatorConfig {
    pub pill: bool,
    pub cue: bool,
}
impl Default for IndicatorConfig {
    fn default() -> Self {
        Self {
            pill: true,
            cue: true,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
#[serde(default, deny_unknown_fields)]
pub struct CaptureLogConfig {
    pub enabled: bool,
    /// empty = <platform data dir>/byovox/capture
    pub dir: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct LoggingConfig {
    pub level: String,
}
impl Default for LoggingConfig {
    fn default() -> Self {
        Self {
            level: "info".into(),
        }
    }
}

/// Platform config dir + `config.toml`.
pub fn default_path() -> PathBuf {
    directories::ProjectDirs::from("", "", "byovox")
        .map(|d| d.config_dir().join("config.toml"))
        .unwrap_or_else(|| PathBuf::from("config.toml"))
}

/// Platform data dir for capture logs.
pub fn data_dir() -> PathBuf {
    directories::ProjectDirs::from("", "", "byovox")
        .map(|d| d.data_dir().to_path_buf())
        .unwrap_or_else(|| PathBuf::from("."))
}

/// Missing file = all defaults (a fresh install works before `config --init`).
pub fn load(path: &Path) -> Result<Config> {
    if !path.exists() {
        return Ok(Config::default());
    }
    let text =
        std::fs::read_to_string(path).with_context(|| format!("reading {}", path.display()))?;
    toml::from_str(&text).with_context(|| format!("parsing {}", path.display()))
}

/// Every leaf key in dotted form with "file" if the file sets it, else "default".
pub fn provenance(path: &Path) -> Result<Vec<(String, &'static str)>> {
    let file_table: toml::Table = if path.exists() {
        toml::from_str(&std::fs::read_to_string(path)?)?
    } else {
        toml::Table::new()
    };
    let defaults: toml::Table = toml::Table::try_from(Config::default())?;
    let mut out = Vec::new();
    walk(&defaults, &file_table, "", &mut out);
    Ok(out)
}

fn walk(
    defaults: &toml::Table,
    file: &toml::Table,
    prefix: &str,
    out: &mut Vec<(String, &'static str)>,
) {
    for (k, v) in defaults {
        let dotted = if prefix.is_empty() {
            k.clone()
        } else {
            format!("{prefix}.{k}")
        };
        match (v, file.get(k)) {
            (toml::Value::Table(dt), Some(toml::Value::Table(ft))) => walk(dt, ft, &dotted, out),
            (toml::Value::Table(dt), _) => walk(dt, &toml::Table::new(), &dotted, out),
            (_, Some(_)) => out.push((dotted, "file")),
            (_, None) => out.push((dotted, "default")),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn partial_file_keeps_defaults_for_everything_else() {
        let cfg: Config = toml::from_str("[stt]\nbase_url = \"http://x:1/v1\"\n").unwrap();
        assert_eq!(cfg.stt.base_url, "http://x:1/v1");
        assert_eq!(cfg.stt.timeout_s, 30);
        assert_eq!(cfg.hotkey.key, "ControlRight");
        assert!(cfg.polish.enabled);
    }

    #[test]
    fn unknown_key_is_an_error() {
        let err = toml::from_str::<Config>("[stt]\nbase_ur = \"typo\"\n").unwrap_err();
        assert!(err.to_string().contains("base_ur"), "{err}");
    }

    #[test]
    fn example_file_is_exactly_the_defaults() {
        let from_example: Config = toml::from_str(EXAMPLE).expect("example parses");
        assert_eq!(from_example, Config::default());
    }

    #[test]
    fn provenance_marks_file_keys() {
        let dir = tempfile_dir();
        let path = dir.join("config.toml");
        std::fs::write(&path, "[polish]\nmodel = \"x\"\n").unwrap();
        let prov = provenance(&path).unwrap();
        assert!(prov.contains(&("polish.model".to_string(), "file")));
        assert!(prov.contains(&("polish.enabled".to_string(), "default")));
    }

    fn tempfile_dir() -> PathBuf {
        let d = std::env::temp_dir().join(format!("byovox-test-{}", std::process::id()));
        std::fs::create_dir_all(&d).unwrap();
        d
    }
}
