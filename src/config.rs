use anyhow::{Context, Result};
use serde::Deserialize;
use std::fs;
use std::path::PathBuf;

#[derive(Debug, Default, Deserialize)]
#[serde(default)]
pub struct Config {
    pub default_file: Option<PathBuf>,
    pub editor: EditorConfig,
    pub auto_sep: AutoSepConfig,
    pub ai: AiConfig,
    pub spell: SpellConfig,
    pub sync: SyncConfig,
}

#[derive(Debug, Deserialize)]
#[serde(default)]
pub struct EditorConfig {
    pub keybindings: String,
    pub line_wrap: bool,
    pub tab_width: u32,
}

impl Default for EditorConfig {
    fn default() -> Self {
        Self {
            keybindings: "default".into(),
            line_wrap: true,
            tab_width: 4,
        }
    }
}

#[derive(Debug, Default, Deserialize)]
#[serde(default)]
pub struct AutoSepConfig {
    pub date: bool,
    pub time_gap: String,
}

#[derive(Debug, Deserialize, Clone)]
#[serde(default)]
pub struct AiConfig {
    pub provider: String,
    pub model: String,
    pub base_url: String,
    pub api_key_env: Option<String>,
}

impl Default for AiConfig {
    fn default() -> Self {
        Self {
            provider: "openai".into(),
            model: "gpt-4o-mini".into(),
            base_url: "https://api.openai.com/v1".into(),
            api_key_env: Some("OPENAI_API_KEY".into()),
        }
    }
}

#[derive(Debug, Deserialize, Clone)]
#[serde(default)]
pub struct SpellConfig {
    /// Language code for the spell-check dictionary. Only "en" supported in MVP.
    pub language: String,
}

impl Default for SpellConfig {
    fn default() -> Self {
        Self {
            language: "en".into(),
        }
    }
}

#[derive(Debug, Deserialize, Clone)]
#[serde(default)]
pub struct SyncConfig {
    /// Enable GitHub-backed sync. When `true`, sync is auto-enabled if the
    /// notes file is inside a git working tree with a remote configured.
    pub enabled: bool,
    /// Try `git pull --rebase` when the file is opened (blocking, 5s timeout).
    pub pull_on_open: bool,
    /// Commit + push in the background after every successful save.
    pub push_on_save: bool,
    /// Idle interval between background pulls. `"0"` or `""` disables.
    pub idle_pull_interval: String,
}

impl Default for SyncConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            pull_on_open: true,
            push_on_save: true,
            idle_pull_interval: "5m".into(),
        }
    }
}

impl SyncConfig {
    /// Parse `idle_pull_interval` into a `Duration`. Accepts e.g. `"5m"`,
    /// `"30s"`, `"1h"`. Empty or `"0"` returns `Duration::ZERO` (disabled).
    pub fn idle_interval_duration(&self) -> std::time::Duration {
        let s = self.idle_pull_interval.trim();
        if s.is_empty() || s == "0" {
            return std::time::Duration::ZERO;
        }
        let (num, unit) = s.split_at(s.find(|c: char| !c.is_ascii_digit()).unwrap_or(s.len()));
        let n: u64 = num.parse().unwrap_or(0);
        match unit {
            "s" => std::time::Duration::from_secs(n),
            "m" => std::time::Duration::from_secs(n * 60),
            "h" => std::time::Duration::from_secs(n * 3600),
            _ => std::time::Duration::from_secs(n * 60), // default unit = minutes
        }
    }
}

impl Config {
    pub fn load() -> Result<Self> {
        let path = config_path()?;
        if !path.exists() {
            return Ok(Self::default_with_auto_sep());
        }
        let text = fs::read_to_string(&path)
            .with_context(|| format!("reading config at {}", path.display()))?;
        let cfg: Config = toml::from_str(&text)
            .with_context(|| format!("parsing config at {}", path.display()))?;
        Ok(cfg)
    }

    fn default_with_auto_sep() -> Self {
        Self {
            auto_sep: AutoSepConfig {
                date: true,
                time_gap: String::new(),
            },
            ..Self::default()
        }
    }
}

pub fn config_path() -> Result<PathBuf> {
    let base = dirs::config_dir().context("could not determine config directory")?;
    Ok(base.join("jotstash").join("config.toml"))
}

/// Default notes-file path used when neither the CLI nor the config specifies
/// one. Tries, in order:
///
/// 1. `<Documents>/jotstash/log.jot`
/// 2. `<Home>/jotstash/log.jot`
/// 3. `./log.jot` (current directory) as the last resort.
///
/// The path is *not* created on disk by this function — the caller decides
/// whether to `create_dir_all` the parent.
pub fn default_notes_path() -> PathBuf {
    if let Some(d) = dirs::document_dir() {
        return d.join("jotstash").join("log.jot");
    }
    if let Some(h) = dirs::home_dir() {
        return h.join("jotstash").join("log.jot");
    }
    PathBuf::from("log.jot")
}
