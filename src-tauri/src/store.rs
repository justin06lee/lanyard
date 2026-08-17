//! Persisted user overrides: custom names and floater placement.
//!
//! Names are remembered twice — once against the session id (so a rename sticks
//! to *this* session) and once against the working directory (so tomorrow's
//! session in the same checkout inherits the name you already chose).

use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::PathBuf;

use crate::sessions::home_dir;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Anchor {
    TopCenter,
    TopLeft,
    TopRight,
    BottomCenter,
    BottomLeft,
    BottomRight,
}

impl Anchor {
    /// Every position the pill can lock to; drag-and-release picks the nearest.
    pub const ALL: [Anchor; 6] = [
        Anchor::TopCenter,
        Anchor::TopLeft,
        Anchor::TopRight,
        Anchor::BottomCenter,
        Anchor::BottomLeft,
        Anchor::BottomRight,
    ];
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Appearance {
    Dark,
    Light,
}

/// Dark glass by default; light is the configurable exception.
fn default_appearance() -> Appearance {
    Appearance::Dark
}

/// Top-right by default: Claude Code's TUI is left-aligned with a wide right
/// margin at typical window widths, so this covers the least actual text.
fn default_anchor() -> Anchor {
    Anchor::TopRight
}

fn default_true() -> bool {
    true
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Config {
    /// sessionId -> custom name
    #[serde(default)]
    pub names: HashMap<String, String>,
    /// cwd -> custom name
    #[serde(default)]
    pub path_names: HashMap<String, String>,
    #[serde(default = "default_anchor")]
    pub anchor: Anchor,
    #[serde(default = "default_true")]
    pub enabled: bool,
    /// Rewrite terminal titles. Required for focus matching; exposed so it can
    /// be turned off if another tool owns the title.
    #[serde(default = "default_true")]
    pub stamp_titles: bool,
    #[serde(default = "default_appearance")]
    pub appearance: Appearance,
    /// Post a notification when a session flips to `waiting`.
    #[serde(default = "default_true")]
    pub notify: bool,
    /// Global shortcut that opens the sessions panel. Empty string disables.
    #[serde(default = "default_hotkey")]
    pub hotkey: String,
    /// Global shortcut that opens the session search. Empty string disables.
    #[serde(default = "default_search_hotkey")]
    pub search_hotkey: String,
}

fn default_hotkey() -> String {
    "ctrl+cmd+l".into()
}

fn default_search_hotkey() -> String {
    "ctrl+cmd+k".into()
}

impl Default for Config {
    fn default() -> Self {
        Self {
            names: HashMap::new(),
            path_names: HashMap::new(),
            anchor: default_anchor(),
            enabled: true,
            stamp_titles: true,
            appearance: default_appearance(),
            notify: true,
            hotkey: default_hotkey(),
            search_hotkey: default_search_hotkey(),
        }
    }
}

pub fn config_path() -> PathBuf {
    home_dir().join(".config").join("lanyard").join("config.json")
}

impl Config {
    pub fn load() -> Self {
        std::fs::read_to_string(config_path())
            .ok()
            .and_then(|raw| serde_json::from_str(&raw).ok())
            .unwrap_or_default()
    }

    pub fn save(&self) -> std::io::Result<()> {
        let path = config_path();
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let body = serde_json::to_string_pretty(self)?;
        // Write-then-rename so a crash mid-write can't truncate the config.
        let tmp = path.with_extension("json.tmp");
        std::fs::write(&tmp, body)?;
        std::fs::rename(&tmp, &path)
    }

    /// Name shown for a session: explicit rename › per-path memory › repo name.
    pub fn name_for(&self, session_id: &str, cwd: &str, repo: &str) -> String {
        self.names
            .get(session_id)
            .or_else(|| self.path_names.get(cwd))
            .cloned()
            .unwrap_or_else(|| repo.to_string())
    }

    pub fn rename(&mut self, session_id: &str, cwd: &str, name: &str) {
        let name = name.trim();
        if name.is_empty() {
            self.names.remove(session_id);
            self.path_names.remove(cwd);
        } else {
            self.names.insert(session_id.to_string(), name.to_string());
            self.path_names.insert(cwd.to_string(), name.to_string());
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn falls_back_through_the_name_chain() {
        let mut c = Config::default();
        assert_eq!(c.name_for("s1", "/tmp/a", "myrepo"), "myrepo");

        c.path_names.insert("/tmp/a".into(), "from-path".into());
        assert_eq!(c.name_for("s1", "/tmp/a", "myrepo"), "from-path");

        c.names.insert("s1".into(), "explicit".into());
        assert_eq!(c.name_for("s1", "/tmp/a", "myrepo"), "explicit");
    }

    #[test]
    fn clearing_a_name_restores_the_repo_default() {
        let mut c = Config::default();
        c.rename("s1", "/tmp/a", "custom");
        assert_eq!(c.name_for("s1", "/tmp/a", "myrepo"), "custom");
        c.rename("s1", "/tmp/a", "   ");
        assert_eq!(c.name_for("s1", "/tmp/a", "myrepo"), "myrepo");
    }

}
