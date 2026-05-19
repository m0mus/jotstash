use serde::{Deserialize, Serialize};
use std::path::PathBuf;

// ---------------------------------------------------------------------------
// Persistent app state
// ---------------------------------------------------------------------------

#[derive(Serialize, Deserialize, Default)]
pub struct AppState {
    #[serde(default)]
    pub last_ai_prompt: Option<String>,
}

fn state_path() -> Option<PathBuf> {
    dirs::config_dir().map(|d| d.join("jotstash").join("state.toml"))
}

/// Load persisted state; returns a default-empty `AppState` on any error.
pub fn load_state() -> AppState {
    let path = match state_path() {
        Some(p) => p,
        None => return AppState::default(),
    };
    let content = match std::fs::read_to_string(&path) {
        Ok(s) => s,
        Err(_) => return AppState::default(),
    };
    toml::from_str(&content).unwrap_or_default()
}

/// Persist `state`; silently swallows I/O errors (best-effort).
pub fn save_state(state: &AppState) {
    let path = match state_path() {
        Some(p) => p,
        None => return,
    };
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    if let Ok(content) = toml::to_string(state) {
        let _ = std::fs::write(&path, content);
    }
}
