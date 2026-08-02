//! The polling loop that keeps the floater pointed at the right session.
//!
//! Two cadences: focus is sampled every `FOCUS_TICK` (a cheap in-process AX
//! call), while the session registry is re-read once a second (it shells out to
//! `ps`, so it is the expensive half).

use serde::Serialize;
use std::collections::{HashMap, HashSet};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use tauri::{AppHandle, Emitter, LogicalPosition, Manager};

use crate::ax;
use crate::sessions::{self, ProcEnv, Session};
use crate::store::{Anchor, Config};
use crate::title;

const FOCUS_TICK: Duration = Duration::from_millis(200);
const RESCAN_EVERY: Duration = Duration::from_millis(1000);
/// Sessions started without `CLAUDE_CODE_DISABLE_TERMINAL_TITLE` keep rewriting
/// their own title, so gru has to re-stamp. Throttle that tug-of-war.
const RESTAMP_COOLDOWN: Duration = Duration::from_millis(750);

pub const FLOATER_W: f64 = 320.0;
pub const FLOATER_H: f64 = 56.0;
const MARGIN: f64 = 12.0;

#[derive(Debug, Clone, Serialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct SessionView {
    pub pid: i32,
    pub session_id: String,
    pub name: String,
    pub repo: String,
    pub subpath: String,
    pub cwd: String,
    pub claude_name: Option<String>,
    pub status: Option<String>,
    pub renamed: bool,
    pub focused: bool,
}

#[derive(Debug, Clone, Serialize, PartialEq, Default)]
#[serde(rename_all = "camelCase")]
pub struct Snapshot {
    pub sessions: Vec<SessionView>,
    pub focused: Option<SessionView>,
    pub ax_trusted: bool,
    pub enabled: bool,
    pub anchor_label: String,
    /// Sessions still competing with gru for their terminal title.
    pub title_conflicts: usize,
}

/// Shared state between the tracker thread and the Tauri commands.
pub struct State {
    pub config: Mutex<Config>,
    pub snapshot: Mutex<Snapshot>,
    /// Set after a rename (or when a title is found untagged) to force a rewrite.
    pub restamp: AtomicBool,
}

impl State {
    pub fn new(config: Config) -> Self {
        Self {
            config: Mutex::new(config),
            snapshot: Mutex::new(Snapshot::default()),
            restamp: AtomicBool::new(true),
        }
    }
}

fn anchor_label(a: Anchor) -> String {
    match a {
        Anchor::TopCenter => "top-center",
        Anchor::TopLeft => "top-left",
        Anchor::TopRight => "top-right",
        Anchor::BottomCenter => "bottom-center",
        Anchor::BottomLeft => "bottom-left",
        Anchor::BottomRight => "bottom-right",
    }
    .to_string()
}

/// Places the floater relative to the focused window's rect.
fn placement(anchor: Anchor, win: &ax::FocusedWindow) -> (f64, f64) {
    let (x, y) = match anchor {
        Anchor::TopLeft => (win.x + MARGIN, win.y + MARGIN),
        Anchor::TopCenter => (win.x + (win.w - FLOATER_W) / 2.0, win.y + MARGIN),
        Anchor::TopRight => (win.x + win.w - FLOATER_W - MARGIN, win.y + MARGIN),
        Anchor::BottomLeft => (win.x + MARGIN, win.y + win.h - FLOATER_H - MARGIN),
        Anchor::BottomCenter => (
            win.x + (win.w - FLOATER_W) / 2.0,
            win.y + win.h - FLOATER_H - MARGIN,
        ),
        Anchor::BottomRight => (
            win.x + win.w - FLOATER_W - MARGIN,
            win.y + win.h - FLOATER_H - MARGIN,
        ),
    };
    (x.round(), y.round())
}

/// Ancestors of a pid, used to recognise the terminal app that hosts a session.
fn ancestors(pid: i32) -> Vec<i32> {
    let mut out = Vec::new();
    let mut cursor = pid;
    for _ in 0..8 {
        let Ok(res) = std::process::Command::new("ps")
            .args(["-o", "ppid=", "-p", &cursor.to_string()])
            .output()
        else {
            break;
        };
        let Ok(parent) = String::from_utf8_lossy(&res.stdout).trim().parse::<i32>() else {
            break;
        };
        if parent <= 1 {
            break;
        }
        out.push(parent);
        cursor = parent;
    }
    out
}

fn views(sessions: &[Session], config: &Config, focused_pid: Option<i32>) -> Vec<SessionView> {
    sessions
        .iter()
        .map(|s| {
            let name = config.name_for(&s.session_id, &s.cwd, &s.repo);
            SessionView {
                pid: s.pid,
                session_id: s.session_id.clone(),
                renamed: name != s.repo,
                name,
                repo: s.repo.clone(),
                subpath: s.subpath.clone(),
                cwd: s.cwd.clone(),
                claude_name: s.claude_name.clone(),
                status: s.status.clone(),
                focused: Some(s.pid) == focused_pid,
            }
        })
        .collect()
}

pub fn spawn(app: AppHandle, state: Arc<State>) {
    std::thread::spawn(move || run(app, state));
}

fn run(app: AppHandle, state: Arc<State>) {
    let mut env_cache: HashMap<i32, ProcEnv> = HashMap::new();
    let mut ancestor_cache: HashMap<i32, Vec<i32>> = HashMap::new();
    let mut stamped: HashMap<i32, String> = HashMap::new();
    let mut last_restamp = Instant::now() - RESTAMP_COOLDOWN;

    let mut sessions: Vec<Session> = Vec::new();
    let mut last_scan = Instant::now() - RESCAN_EVERY;
    let mut last_focused_pid: Option<i32> = None;
    let mut last_emitted: Option<Snapshot> = None;
    let mut floater_visible = false;

    let own_pid = std::process::id() as i32;

    loop {
        let trusted = ax::is_trusted();
        let config = state.config.lock().unwrap().clone();

        if last_scan.elapsed() >= RESCAN_EVERY {
            sessions = sessions::scan(&mut env_cache);
            last_scan = Instant::now();
            ancestor_cache.retain(|pid, _| sessions.iter().any(|s| s.pid == *pid));
            stamped.retain(|pid, _| sessions.iter().any(|s| s.pid == *pid));
        }

        // Terminal apps currently hosting a Claude session.
        let terminal_pids: HashSet<i32> = sessions
            .iter()
            .flat_map(|s| {
                ancestor_cache
                    .entry(s.pid)
                    .or_insert_with(|| ancestors(s.pid))
                    .clone()
            })
            .collect();

        // Keep every session's window title tagged with its pid.
        if config.stamp_titles {
            let force = state.restamp.swap(false, Ordering::SeqCst);
            for s in &sessions {
                let Some(tty) = s.tty.as_deref() else { continue };
                let name = config.name_for(&s.session_id, &s.cwd, &s.repo);
                let desired = title::compose(&name, s.pid);
                if force || stamped.get(&s.pid) != Some(&desired) {
                    if title::stamp(tty, &desired).is_ok() {
                        stamped.insert(s.pid, desired);
                    }
                }
            }
        }

        // Resolve which session the user is actually looking at.
        let focus = ax::focused_window();
        let mut focused_pid = None;
        if let Some(win) = &focus {
            if win.pid == own_pid {
                // The user clicked the floater itself — hold the previous target.
                focused_pid = last_focused_pid;
            } else if let Some(pid) = title::parse_pid(&win.title) {
                if sessions.iter().any(|s| s.pid == pid) {
                    focused_pid = Some(pid);
                }
            } else if terminal_pids.contains(&win.pid) {
                // A terminal we know hosts sessions, but its title lost our tag
                // (Claude Code rewrote it). Re-tag, and hold the previous target
                // meanwhile so the floater doesn't blink out mid-handover.
                if last_restamp.elapsed() >= RESTAMP_COOLDOWN {
                    last_restamp = Instant::now();
                    state.restamp.store(true, Ordering::SeqCst);
                    stamped.clear();
                }
                focused_pid = last_focused_pid;
            }
        }
        last_focused_pid = focused_pid;

        let views = views(&sessions, &config, focused_pid);
        let focused_view = views.iter().find(|v| v.focused).cloned();

        let snapshot = Snapshot {
            sessions: views,
            focused: focused_view.clone(),
            ax_trusted: trusted,
            enabled: config.enabled,
            anchor_label: anchor_label(config.anchor),
            title_conflicts: sessions.iter().filter(|s| !s.title_disabled).count(),
        };

        if let Ok(mut slot) = state.snapshot.lock() {
            *slot = snapshot.clone();
        }

        if last_emitted.as_ref() != Some(&snapshot) {
            let _ = app.emit("gru://state", &snapshot);
            last_emitted = Some(snapshot);
        }

        // Move / show / hide the floater.
        if let Some(floater) = app.get_webview_window("floater") {
            let should_show = config.enabled && trusted && focused_view.is_some();
            if should_show {
                if let Some(win) = &focus {
                    // While the floater is focused its own rect is reported, so
                    // only reposition from a real terminal window.
                    if win.pid != own_pid && win.w > 0.0 && win.h > 0.0 {
                        let (x, y) = placement(config.anchor, win);
                        let _ = floater.set_position(LogicalPosition::new(x, y));
                    }
                }
                if !floater_visible {
                    let _ = floater.show();
                    let _ = floater.set_always_on_top(true);
                    floater_visible = true;
                }
            } else if floater_visible {
                let _ = floater.hide();
                floater_visible = false;
            }
        }

        std::thread::sleep(FOCUS_TICK);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn win() -> ax::FocusedWindow {
        ax::FocusedWindow {
            pid: 1,
            title: String::new(),
            x: 100.0,
            y: 200.0,
            w: 1000.0,
            h: 800.0,
        }
    }

    #[test]
    fn top_center_is_horizontally_centred() {
        let (x, y) = placement(Anchor::TopCenter, &win());
        assert_eq!(x, 100.0 + (1000.0 - FLOATER_W) / 2.0);
        assert_eq!(y, 200.0 + MARGIN);
    }

    #[test]
    fn bottom_right_stays_inside_the_window() {
        let (x, y) = placement(Anchor::BottomRight, &win());
        assert_eq!(x, 100.0 + 1000.0 - FLOATER_W - MARGIN);
        assert_eq!(y, 200.0 + 800.0 - FLOATER_H - MARGIN);
    }
}
