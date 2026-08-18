//! The loop that keeps the floater pointed at the right session.
//!
//! Event-driven where it can be: an AXObserver on each terminal app wakes the
//! loop the instant focus, geometry or titles change, so switching windows or
//! dragging a terminal moves the pill immediately. Behind the events sits a
//! slow fallback tick (`FALLBACK_TICK`) that re-reads the session registry —
//! a directory read plus a few `proc_pidinfo` syscalls, microseconds in
//! total — and catches anything an observer missed. Apps that refuse an
//! observer are covered by polling at the old `DEGRADED_TICK` rate instead.

use serde::Serialize;
use std::collections::{HashMap, HashSet};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use tauri::image::Image;
use tauri::{AppHandle, Emitter, LogicalPosition, Manager};

use crate::ax;
use crate::notify;
use crate::sessions::{Scanner, Session};
use crate::store::{Anchor, Config};
use crate::title;

const TRAY_ICON: &[u8] = include_bytes!("../icons/tray.png");
const TRAY_ALERT: &[u8] = include_bytes!("../icons/tray-alert.png");

/// The safety net behind the AX observers: how long the loop sleeps when no
/// event arrives. Also the registry-rescan cadence, so session status stays
/// fresh within a second either way.
const FALLBACK_TICK: Duration = Duration::from_millis(1000);
/// The poll rate when some terminal app couldn't be observed — the pre-event
/// behaviour, kept so focus tracking never visibly degrades.
const DEGRADED_TICK: Duration = Duration::from_millis(200);
const RESCAN_EVERY: Duration = Duration::from_millis(1000);
/// Sessions started without `CLAUDE_CODE_DISABLE_TERMINAL_TITLE` keep rewriting
/// their own title, so Lanyard has to re-stamp. Throttle that tug-of-war.
const RESTAMP_COOLDOWN: Duration = Duration::from_millis(750);
/// Re-stamp every title periodically, not just when a bad one is noticed.
///
/// Lanyard can only detect a clobbered title on the window it can see — the focused
/// one. A background window whose title Claude Code overwrote would stay stale
/// until you switched to it, and Lanyard would then briefly assert the *previous*
/// session's name. Showing the wrong name is worse than showing none, so keep
/// every title fresh instead. Writing an identical title is a no-op for the
/// emulator, so this costs nothing when nothing is contesting it.
const STAMP_HEARTBEAT: Duration = Duration::from_secs(2);

/// Pill geometry. The window *is* the pill — its native rounded vibrancy view
/// carries the glass and shadow, so there is no shadow padding. Width follows
/// the rendered name (the UI measures and asks for a resize); height is fixed.
pub const PILL_H: f64 = 30.0;
pub const PILL_DEFAULT_W: f64 = 160.0;
pub const PILL_MIN_W: f64 = 56.0;
pub const PILL_MAX_W: f64 = 320.0;
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
    /// Claude Code's auto-generated summary of what this session is doing.
    pub ai_title: Option<String>,
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
    /// `"dark"` or `"light"`; the UI mirrors it onto `data-theme`.
    pub appearance: String,
    /// Sessions still competing with Lanyard for their terminal title.
    pub title_conflicts: usize,
    /// Registry files that no longer parse the way Lanyard expects — the
    /// canary for a Claude Code format change.
    pub registry_errors: usize,
}

/// Shared state between the tracker thread and the Tauri commands.
pub struct State {
    pub config: Mutex<Config>,
    pub snapshot: Mutex<Snapshot>,
    /// Set after a rename (or when a title is found untagged) to force a rewrite.
    pub restamp: AtomicBool,
    /// Live drag displacement from the anchored position, in logical points.
    /// The UI writes this while you drag and springs it back to zero on release.
    pub drag_offset: Mutex<(f64, f64)>,
    /// Where the tracker last anchored the floater, so a drag can reposition
    /// immediately instead of waiting for the next tick.
    pub anchor_pos: Mutex<Option<(f64, f64)>>,
    /// Current pill size in logical points; the UI resizes it to fit the name.
    pub pill_size: Mutex<(f64, f64)>,
    /// Bumped on every resize request; a running width animation aborts the
    /// moment it is superseded.
    pub resize_gen: AtomicU64,
    /// The focused terminal's last known rect, kept so a resize or a drag
    /// release can re-derive placement without waiting for the next AX read.
    pub last_rect: Mutex<Option<ax::FocusedWindow>>,
    /// The waiting session most recently raised via the jump hotkey, so
    /// pressing it again cycles onward instead of bouncing on one session.
    pub last_waiting: Mutex<Option<i32>>,
}

impl State {
    pub fn new(config: Config) -> Self {
        Self {
            config: Mutex::new(config),
            snapshot: Mutex::new(Snapshot::default()),
            restamp: AtomicBool::new(true),
            drag_offset: Mutex::new((0.0, 0.0)),
            anchor_pos: Mutex::new(None),
            pill_size: Mutex::new((PILL_DEFAULT_W, PILL_H)),
            resize_gen: AtomicU64::new(0),
            last_rect: Mutex::new(None),
            last_waiting: Mutex::new(None),
        }
    }
}

/// Places the pill relative to the focused window's rect.
pub fn placement(anchor: Anchor, win: &ax::FocusedWindow, pill: (f64, f64)) -> (f64, f64) {
    let (pw, ph) = pill;
    let (x, y) = match anchor {
        Anchor::TopLeft => (win.x + MARGIN, win.y + MARGIN),
        Anchor::TopCenter => (win.x + (win.w - pw) / 2.0, win.y + MARGIN),
        Anchor::TopRight => (win.x + win.w - pw - MARGIN, win.y + MARGIN),
        Anchor::BottomLeft => (win.x + MARGIN, win.y + win.h - ph - MARGIN),
        Anchor::BottomCenter => (win.x + (win.w - pw) / 2.0, win.y + win.h - ph - MARGIN),
        Anchor::BottomRight => (win.x + win.w - pw - MARGIN, win.y + win.h - ph - MARGIN),
    };
    (x.round(), y.round())
}

/// The anchor whose placement lies closest to `target` — where a dragged pill
/// locks when released. The throw's momentum is already projected into
/// `target` by the caller, so a flick toward a corner lands in that corner.
pub fn nearest_anchor(
    target: (f64, f64),
    win: &ax::FocusedWindow,
    pill: (f64, f64),
) -> (Anchor, (f64, f64)) {
    let mut best = (Anchor::TopRight, placement(Anchor::TopRight, win, pill));
    let mut best_d = f64::MAX;
    for anchor in Anchor::ALL {
        let pos = placement(anchor, win, pill);
        let d = (pos.0 - target.0).powi(2) + (pos.1 - target.1).powi(2);
        if d < best_d {
            best_d = d;
            best = (anchor, pos);
        }
    }
    best
}

/// The top-level application hosting a session, found by walking up from the
/// Claude process through the shell and `login` to the terminal emulator.
pub fn host_app(pid: i32) -> Option<i32> {
    ancestors(pid).last().copied()
}

/// Ancestors of a pid, nearest first, walked via `proc_pidinfo`.
fn ancestors(pid: i32) -> Vec<i32> {
    let mut out = Vec::new();
    let mut cursor = pid;
    for _ in 0..8 {
        let Some(parent) = crate::sessions::parent_of(cursor) else {
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
                ai_title: s.ai_title.clone(),
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
    let mut scanner = Scanner::default();
    let mut ancestor_cache: HashMap<i32, Option<i32>> = HashMap::new();
    let mut stamped: HashMap<i32, String> = HashMap::new();
    let mut last_restamp = Instant::now() - RESTAMP_COOLDOWN;
    let mut last_heartbeat = Instant::now() - STAMP_HEARTBEAT;

    // AX events land here; the loop sleeps on this channel instead of a timer.
    let (wake_tx, wake_rx) = mpsc::channel::<()>();
    ax::install_wake_sender(wake_tx);
    // The terminal-app set the observers were last synced to.
    let mut synced_pids: HashSet<i32> = HashSet::new();

    let mut sessions: Vec<Session> = Vec::new();
    let mut last_scan = Instant::now() - RESCAN_EVERY;
    let mut last_focused_pid: Option<i32> = None;
    let mut last_emitted: Option<Snapshot> = None;
    let mut floater_visible = false;

    // Waiting-state bookkeeping: the tray badge, and one notification per
    // idle→waiting transition.
    let mut prev_status: HashMap<i32, String> = HashMap::new();
    let mut tray_alerted = false;
    let mut first_scan = true;

    let own_pid = std::process::id() as i32;

    loop {
        let trusted = ax::is_trusted();
        let config = state.config.lock().unwrap().clone();

        if last_scan.elapsed() >= RESCAN_EVERY {
            sessions = scanner.scan();
            last_scan = Instant::now();
            ancestor_cache.retain(|pid, _| sessions.iter().any(|s| s.pid == *pid));
            stamped.retain(|pid, _| sessions.iter().any(|s| s.pid == *pid));

            // The pill shows nothing but the name, so "a session needs you"
            // is surfaced globally instead: a badge on the menu bar glyph…
            let waiting = sessions
                .iter()
                .any(|s| s.status.as_deref() == Some("waiting"));
            if waiting != tray_alerted {
                tray_alerted = waiting;
                if let Some(tray) = app.tray_by_id("lanyard") {
                    let bytes: &[u8] = if waiting { TRAY_ALERT } else { TRAY_ICON };
                    if let Ok(icon) = Image::from_bytes(bytes) {
                        let _ = tray.set_icon(Some(icon));
                        let _ = tray.set_icon_as_template(true);
                    }
                }
            }
            // …and a notification on the moment of transition. The first scan
            // is exempt so launching Lanyard doesn't fire a backlog at once.
            if config.notify && !first_scan {
                for s in &sessions {
                    let now_waiting = s.status.as_deref() == Some("waiting");
                    let was_waiting =
                        prev_status.get(&s.pid).map(String::as_str) == Some("waiting");
                    if now_waiting && !was_waiting {
                        let name = config.name_for(&s.session_id, &s.cwd, &s.repo);
                        notify::post(&name, "Claude is waiting on you");
                    }
                }
            }
            first_scan = false;
            prev_status = sessions
                .iter()
                .filter_map(|s| s.status.clone().map(|status| (s.pid, status)))
                .collect();
        }

        // Terminal apps currently hosting a Claude session.
        let terminal_pids: HashSet<i32> = sessions
            .iter()
            .filter_map(|s| *ancestor_cache.entry(s.pid).or_insert_with(|| host_app(s.pid)))
            .collect();

        // Keep an AXObserver on each of them. Observers are main-thread
        // objects, so reconciliation happens over there.
        if trusted && terminal_pids != synced_pids {
            synced_pids = terminal_pids.clone();
            let pids: Vec<i32> = terminal_pids.iter().copied().collect();
            let _ = app.run_on_main_thread(move || ax::sync_observers(&pids));
        }

        // Lanyard's own window counts as "still on this session" so that clicking
        // the floater to rename doesn't read as focus leaving the terminal.
        let mut candidates: Vec<i32> = terminal_pids.iter().copied().collect();
        candidates.push(own_pid);

        // Keep every session's window title tagged with its pid.
        if config.stamp_titles {
            let heartbeat = last_heartbeat.elapsed() >= STAMP_HEARTBEAT;
            if heartbeat {
                last_heartbeat = Instant::now();
            }
            let force = state.restamp.swap(false, Ordering::SeqCst) || heartbeat;
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
        let focus = ax::focused_window_among(&candidates);
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
            appearance: match config.appearance {
                crate::store::Appearance::Dark => "dark".into(),
                crate::store::Appearance::Light => "light".into(),
            },
            title_conflicts: sessions.iter().filter(|s| !s.title_disabled).count(),
            registry_errors: scanner.registry_errors(),
        };

        if let Ok(mut slot) = state.snapshot.lock() {
            *slot = snapshot.clone();
        }

        if last_emitted.as_ref() != Some(&snapshot) {
            let _ = app.emit("lanyard://state", &snapshot);
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
                        let pill = *state.pill_size.lock().unwrap();
                        let anchor = placement(config.anchor, win, pill);
                        *state.last_rect.lock().unwrap() = Some(win.clone());
                        *state.anchor_pos.lock().unwrap() = Some(anchor);
                        // Honour any in-flight drag: the anchor keeps tracking
                        // the terminal window even while the pill is displaced.
                        let (dx, dy) = *state.drag_offset.lock().unwrap();
                        let _ = floater
                            .set_position(LogicalPosition::new(anchor.0 + dx, anchor.1 + dy));
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

        // Sleep until an AX event wakes us or the fallback elapses. Whole
        // bursts (a window drag streams move events) coalesce into one
        // resolution per pass. While any terminal app lacks an observer,
        // poll at the old rate so tracking never visibly degrades.
        let observed = ax::observed_pids();
        let all_observed = terminal_pids.iter().all(|pid| observed.contains(pid));
        let timeout = if trusted && all_observed {
            FALLBACK_TICK
        } else {
            DEGRADED_TICK
        };
        if wake_rx.recv_timeout(timeout).is_ok() {
            while wake_rx.try_recv().is_ok() {}
        }
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

    const PILL: (f64, f64) = (200.0, PILL_H);

    #[test]
    fn top_center_is_horizontally_centred() {
        let (x, y) = placement(Anchor::TopCenter, &win(), PILL);
        assert_eq!(x, 100.0 + (1000.0 - PILL.0) / 2.0);
        assert_eq!(y, 200.0 + MARGIN);
    }

    #[test]
    fn bottom_right_stays_inside_the_window() {
        let (x, y) = placement(Anchor::BottomRight, &win(), PILL);
        assert_eq!(x, 100.0 + 1000.0 - PILL.0 - MARGIN);
        assert_eq!(y, 200.0 + 800.0 - PILL_H - MARGIN);
    }

    #[test]
    fn a_throw_toward_a_corner_locks_to_that_corner() {
        let w = win();
        // Released near the bottom-left of the terminal's rect.
        let (anchor, _) = nearest_anchor((130.0, 950.0), &w, PILL);
        assert_eq!(anchor, Anchor::BottomLeft);
    }

    #[test]
    fn a_drop_near_the_top_middle_locks_to_top_center() {
        let w = win();
        let (anchor, pos) = nearest_anchor((480.0, 230.0), &w, PILL);
        assert_eq!(anchor, Anchor::TopCenter);
        assert_eq!(pos, placement(Anchor::TopCenter, &w, PILL));
    }
}
