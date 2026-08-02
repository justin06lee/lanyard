//! Discovery of live Claude Code sessions.
//!
//! Claude Code already maintains a registry at `~/.claude/sessions/<pid>.json`
//! containing the session id, cwd, a derived name and a live busy/idle status.
//! We treat that as the source of truth and enrich each entry with the two
//! things it lacks: the controlling tty, and the terminal window id exported
//! into the process environment.

use serde::Serialize;
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::process::Command;

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Session {
    pub pid: i32,
    pub session_id: String,
    pub cwd: String,
    /// Repository name, derived from the nearest enclosing git root.
    pub repo: String,
    /// Path of `cwd` relative to the repo root ("" when at the root).
    pub subpath: String,
    /// The name Claude Code derived for itself, e.g. `gru-9d`.
    pub claude_name: Option<String>,
    /// `busy` / `idle` — whatever Claude Code last wrote.
    pub status: Option<String>,
    /// Controlling terminal, e.g. `ttys003`.
    pub tty: Option<String>,
    /// Terminal window id from the environment (Alacritty, kitty, WezTerm, …).
    pub window_id: Option<String>,
    /// True when this session was launched with CLAUDE_CODE_DISABLE_TERMINAL_TITLE
    /// set, meaning gru owns the window title outright instead of contesting it.
    pub title_disabled: bool,
    pub started_at: Option<i64>,
}

/// The slice of a session's environment gru cares about. Immutable for the
/// lifetime of the process, so it is read once and cached.
#[derive(Debug, Clone, Default)]
pub struct ProcEnv {
    pub window_id: Option<String>,
    pub title_disabled: bool,
}

pub fn home_dir() -> PathBuf {
    std::env::var_os("HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("/"))
}

fn claude_dir() -> PathBuf {
    std::env::var_os("CLAUDE_CONFIG_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|| home_dir().join(".claude"))
}

/// One `ps` sweep gives liveness, tty and command name for every process.
fn process_table() -> HashMap<i32, (Option<String>, String)> {
    let mut table = HashMap::new();
    let Ok(out) = Command::new("ps").args(["-Ao", "pid=,tty=,comm="]).output() else {
        return table;
    };
    for line in String::from_utf8_lossy(&out.stdout).lines() {
        let mut parts = line.split_whitespace();
        let (Some(pid), Some(tty)) = (parts.next(), parts.next()) else {
            continue;
        };
        let Ok(pid) = pid.parse::<i32>() else { continue };
        let comm = parts.collect::<Vec<_>>().join(" ");
        let tty = (tty != "??" && !tty.is_empty()).then(|| tty.to_string());
        table.insert(pid, (tty, comm));
    }
    table
}

/// Terminal emulators each export their own window handle; we accept any of them.
const WINDOW_ID_VARS: [&str; 5] = [
    "ALACRITTY_WINDOW_ID",
    "KITTY_WINDOW_ID",
    "WEZTERM_PANE",
    "ITERM_SESSION_ID",
    "WINDOWID",
];

fn proc_env(pid: i32) -> ProcEnv {
    let mut env = ProcEnv::default();
    let Ok(out) = Command::new("ps")
        .args(["eww", "-p", &pid.to_string()])
        .output()
    else {
        return env;
    };
    let text = String::from_utf8_lossy(&out.stdout);
    for token in text.split_whitespace() {
        let Some((key, value)) = token.split_once('=') else {
            continue;
        };
        if key == "CLAUDE_CODE_DISABLE_TERMINAL_TITLE" {
            env.title_disabled = !matches!(value, "" | "0" | "false");
        } else if env.window_id.is_none() && WINDOW_ID_VARS.contains(&key) && !value.is_empty() {
            env.window_id = Some(value.to_string());
        }
    }
    env
}

/// Walks up from `cwd` looking for a `.git` entry (dir for repos, file for worktrees).
fn git_root(cwd: &Path) -> Option<PathBuf> {
    let mut cursor = Some(cwd);
    while let Some(dir) = cursor {
        if dir.join(".git").exists() {
            return Some(dir.to_path_buf());
        }
        cursor = dir.parent();
    }
    None
}

fn basename(path: &Path) -> String {
    path.file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| path.to_string_lossy().into_owned())
}

/// Scans for live interactive sessions.
///
/// `env_cache` memoises the per-pid `ps eww` lookup: a process's environment
/// never changes, and shelling out for eight sessions every tick would be waste.
pub fn scan(env_cache: &mut HashMap<i32, ProcEnv>) -> Vec<Session> {
    let table = process_table();
    let dir = claude_dir().join("sessions");
    let Ok(entries) = std::fs::read_dir(&dir) else {
        return Vec::new();
    };

    let mut sessions = Vec::new();
    // Directory enclosing each session's repo, kept to disambiguate collisions.
    let mut enclosing = Vec::new();
    for entry in entries.flatten() {
        let path = entry.path();
        if path.extension().and_then(|e| e.to_str()) != Some("json") {
            continue;
        }
        let Ok(raw) = std::fs::read_to_string(&path) else {
            continue;
        };
        let Ok(value) = serde_json::from_str::<serde_json::Value>(&raw) else {
            continue;
        };

        let pid = value.get("pid").and_then(|v| v.as_i64()).unwrap_or(0) as i32;
        if pid <= 0 {
            continue;
        }

        // Guard against both stale files and pid reuse.
        let Some((tty, comm)) = table.get(&pid) else {
            continue;
        };
        if !comm.contains("claude") {
            continue;
        }
        if value.get("kind").and_then(|v| v.as_str()) == Some("background") {
            continue;
        }

        let cwd = value
            .get("cwd")
            .and_then(|v| v.as_str())
            .unwrap_or_default()
            .to_string();
        let cwd_path = PathBuf::from(&cwd);
        let root = git_root(&cwd_path);
        let repo = root
            .as_deref()
            .map(basename)
            .unwrap_or_else(|| basename(&cwd_path));
        let subpath = root
            .as_deref()
            .and_then(|r| cwd_path.strip_prefix(r).ok())
            .map(|p| p.to_string_lossy().into_owned())
            .unwrap_or_default();

        let env = env_cache.entry(pid).or_insert_with(|| proc_env(pid)).clone();

        enclosing.push(
            root.as_deref()
                .and_then(Path::parent)
                .map(basename)
                .unwrap_or_default(),
        );

        sessions.push(Session {
            pid,
            session_id: value
                .get("sessionId")
                .and_then(|v| v.as_str())
                .unwrap_or_default()
                .to_string(),
            cwd,
            repo,
            subpath,
            claude_name: value
                .get("name")
                .and_then(|v| v.as_str())
                .map(str::to_string),
            status: value
                .get("status")
                .and_then(|v| v.as_str())
                .map(str::to_string),
            tty: tty.clone(),
            window_id: env.window_id,
            title_disabled: env.title_disabled,
            started_at: value.get("startedAt").and_then(|v| v.as_i64()),
        });
    }

    // Drop cache entries for sessions that have exited.
    env_cache.retain(|pid, _| table.contains_key(pid));

    // A bare repo name is useless when two checkouts share it — `web-client`
    // says nothing when you have three. Qualify only the ones that collide.
    let mut counts: HashMap<String, usize> = HashMap::new();
    for session in &sessions {
        *counts.entry(session.repo.clone()).or_default() += 1;
    }
    for (session, parent) in sessions.iter_mut().zip(&enclosing) {
        if counts.get(&session.repo).copied().unwrap_or(0) > 1 && !parent.is_empty() {
            session.repo = format!("{parent}/{}", session.repo);
        }
    }

    sessions.sort_by_key(|s| (s.started_at.unwrap_or(0), s.pid));
    sessions
}
