pub mod ax;
pub mod notify;
pub mod sessions;
pub mod store;
pub mod title;
pub mod tracker;

use std::sync::atomic::Ordering;
use std::sync::Arc;

use serde::Serialize;
use tauri::image::Image;
use tauri::menu::{CheckMenuItem, Menu, MenuItem, PredefinedMenuItem, Submenu};
use tauri::tray::TrayIconBuilder;
use tauri_plugin_autostart::{MacosLauncher, ManagerExt};
use tauri_plugin_global_shortcut::{GlobalShortcutExt, ShortcutState};
use tauri_plugin_updater::UpdaterExt;
use tauri::utils::config::WindowEffectsConfig;
use tauri::utils::{WindowEffect, WindowEffectState};
use tauri::{
    AppHandle, Emitter, LogicalPosition, LogicalSize, Manager, Theme, WebviewUrl,
    WebviewWindowBuilder, WindowEvent,
};

use store::{Appearance, Config};
use tracker::{Snapshot, State, PILL_DEFAULT_W, PILL_H, PILL_MAX_W, PILL_MIN_W};

const TRAY_ICON: &[u8] = include_bytes!("../icons/tray.png");

fn theme_of(appearance: Appearance) -> Theme {
    match appearance {
        Appearance::Dark => Theme::Dark,
        Appearance::Light => Theme::Light,
    }
}

#[tauri::command]
fn get_state(state: tauri::State<'_, Arc<State>>) -> Snapshot {
    state.snapshot.lock().unwrap().clone()
}

#[tauri::command]
fn rename(
    session_id: String,
    cwd: String,
    name: String,
    state: tauri::State<'_, Arc<State>>,
) -> Result<(), String> {
    {
        let mut config = state.config.lock().unwrap();
        config.rename(&session_id, &cwd, &name);
        config.save().map_err(|e| e.to_string())?;
    }
    // Names live in the window title too, so push them out immediately.
    state.restamp.store(true, Ordering::SeqCst);
    Ok(())
}

#[tauri::command]
fn set_enabled(enabled: bool, state: tauri::State<'_, Arc<State>>) -> Result<(), String> {
    let mut config = state.config.lock().unwrap();
    config.enabled = enabled;
    config.save().map_err(|e| e.to_string())
}

#[tauri::command]
fn request_accessibility() -> bool {
    ax::repair_trust()
}

#[tauri::command]
fn request_notifications() {
    notify::request();
}

/// Re-asks for file access with the same self-repair trick the Accessibility
/// flow uses: clear our own entry so the consent prompt can appear again —
/// the tracker's next scan touches the folders and triggers it within a
/// second.
#[tauri::command]
fn request_file_access() {
    let _ = std::process::Command::new("/usr/bin/tccutil")
        .args(["reset", "SystemPolicyRemovableVolumes", ax::BUNDLE_ID])
        .status();
}

/// How long a pill width change takes; long enough to read as a morph,
/// short enough to keep up with rapid session switching.
const RESIZE_MS: f64 = 160.0;

/// One frame of pill geometry: size, and the anchor-derived position that
/// goes with it (a centre or right anchor's x depends on the width, so both
/// must move together or the pill visibly jumps).
fn apply_pill_frame(app: &AppHandle, state: &Arc<State>, width: f64) {
    let Some(floater) = app.get_webview_window("floater") else {
        return;
    };
    let _ = floater.set_size(LogicalSize::new(width, PILL_H));
    let win = state.last_rect.lock().unwrap().clone();
    if let Some(win) = win {
        let anchor = state.config.lock().unwrap().anchor;
        let pos = tracker::placement(anchor, &win, (width, PILL_H));
        *state.anchor_pos.lock().unwrap() = Some(pos);
        let (dx, dy) = *state.drag_offset.lock().unwrap();
        let _ = floater.set_position(LogicalPosition::new(pos.0 + dx, pos.1 + dy));
    }
}

/// Sizes the pill to its text. The UI measures the rendered name and asks for
/// exactly that width; the height never changes.
///
/// The change is animated — the capsule morphs between names instead of
/// snapping. A newer request bumps the generation and the running animation
/// yields to it mid-flight.
#[tauri::command]
fn resize_pill(
    width: f64,
    instant: Option<bool>,
    app: AppHandle,
    state: tauri::State<'_, Arc<State>>,
) {
    let target = width.clamp(PILL_MIN_W, PILL_MAX_W).round();
    let start = state.pill_size.lock().unwrap().0;
    if (start - target).abs() < 1.0 {
        return;
    }
    let gen = state.resize_gen.fetch_add(1, Ordering::SeqCst) + 1;
    if instant.unwrap_or(false) {
        // Reduce Motion: one frame, no morph.
        state.pill_size.lock().unwrap().0 = target;
        apply_pill_frame(&app, &state, target);
        return;
    }
    let state = state.inner().clone();
    std::thread::spawn(move || {
        let began = std::time::Instant::now();
        loop {
            if state.resize_gen.load(Ordering::SeqCst) != gen {
                return; // superseded by a newer name
            }
            let t = (began.elapsed().as_secs_f64() * 1000.0 / RESIZE_MS).min(1.0);
            let eased = t * t * (3.0 - 2.0 * t); // smoothstep
            let w = start + (target - start) * eased;
            state.pill_size.lock().unwrap().0 = w;
            apply_pill_frame(&app, &state, w);
            if t >= 1.0 {
                return;
            }
            std::thread::sleep(std::time::Duration::from_millis(16));
        }
    });
}

/// Displaces the floater from its anchor while the user drags it aside.
///
/// The UI calls this every frame during a drag and again as it springs to
/// `(0, 0)` on release. Repositioning here rather than waiting for the
/// tracker's next tick is what makes the drag feel attached to the cursor; the
/// tracker keeps applying the same offset so the pill still follows the
/// terminal window if it moves mid-drag.
#[tauri::command]
fn set_drag_offset(x: f64, y: f64, app: AppHandle, state: tauri::State<'_, Arc<State>>) {
    *state.drag_offset.lock().unwrap() = (x, y);
    let anchor = *state.anchor_pos.lock().unwrap();
    if let (Some(window), Some((ax, ay))) = (app.get_webview_window("floater"), anchor) {
        let _ = window.set_position(LogicalPosition::new(ax + x, ay + y));
    }
}

#[derive(Serialize)]
struct Residual {
    x: f64,
    y: f64,
}

/// Locks a released drag to whichever anchor it was thrown toward.
///
/// `(x, y)` is the offset at release, `(px, py)` the same offset with a little
/// momentum projected forward — so a flick carries the pill to the corner it
/// was aimed at, not just the nearest one. The chosen anchor is persisted, and
/// the returned residual is the pill's current displacement from its *new*
/// anchor, which the UI springs to zero.
#[tauri::command]
fn commit_drag(
    x: f64,
    y: f64,
    px: f64,
    py: f64,
    state: tauri::State<'_, Arc<State>>,
) -> Residual {
    let win = state.last_rect.lock().unwrap().clone();
    let old = *state.anchor_pos.lock().unwrap();
    let (Some(win), Some(old)) = (win, old) else {
        // Nothing to re-anchor against; spring back to where it came from.
        return Residual { x, y };
    };
    let pill = *state.pill_size.lock().unwrap();
    let (best, best_pos) = tracker::nearest_anchor((old.0 + px, old.1 + py), &win, pill);
    {
        let mut config = state.config.lock().unwrap();
        config.anchor = best;
        let _ = config.save();
    }
    *state.anchor_pos.lock().unwrap() = Some(best_pos);
    Residual {
        x: old.0 + x - best_pos.0,
        y: old.1 + y - best_pos.1,
    }
}

/// Gives the floater keyboard focus for an inline rename.
#[tauri::command]
fn focus_floater(app: AppHandle) {
    if let Some(floater) = app.get_webview_window("floater") {
        let _ = floater.set_focus();
    }
}

/// Brings a session's terminal window to the front (and its Space with it).
#[tauri::command]
fn raise_session(pid: i32) -> Result<(), String> {
    let host = tracker::host_app(pid).ok_or_else(|| "no terminal app found".to_string())?;
    ax::raise_window(host, pid)
}

/// Raises the session that's blocked waiting on you; invoked again, it cycles
/// through every waiting session in turn. With nothing waiting it opens the
/// sessions panel instead — feedback you can see, where all-grey dots *are*
/// the answer. (A notification here would depend on a permission the user may
/// never have granted; a silent no-op on a global hotkey reads as breakage.)
fn raise_waiting(app: &AppHandle, state: &Arc<State>) {
    let waiting: Vec<i32> = {
        let snapshot = state.snapshot.lock().unwrap();
        snapshot
            .sessions
            .iter()
            .filter(|s| s.status.as_deref() == Some("waiting"))
            .map(|s| s.pid)
            .collect()
    };
    if waiting.is_empty() {
        let appearance = state.config.lock().unwrap().appearance;
        let _ = show_panel(app, appearance);
        return;
    }
    let next = {
        let mut last = state.last_waiting.lock().unwrap();
        let next = match (*last).and_then(|pid| waiting.iter().position(|&p| p == pid)) {
            Some(i) => waiting[(i + 1) % waiting.len()],
            None => waiting[0],
        };
        *last = Some(next);
        next
    };
    if let Some(host) = tracker::host_app(next) {
        let _ = ax::raise_window(host, next);
    }
}

#[tauri::command]
fn open_panel(app: AppHandle, state: tauri::State<'_, Arc<State>>) -> Result<(), String> {
    let appearance = state.config.lock().unwrap().appearance;
    show_panel(&app, appearance).map_err(|e| e.to_string())
}

/// Search palette geometry. Width is fixed; height follows the content (the
/// UI measures itself and asks, the same contract the pill uses for width).
const SEARCH_W: f64 = 520.0;
const SEARCH_MIN_H: f64 = 52.0;
const SEARCH_MAX_H: f64 = 480.0;
/// Fraction of the screen height the palette sits below the top edge.
const SEARCH_TOP_RATIO: f64 = 0.22;

#[tauri::command]
fn hide_search(app: AppHandle) {
    if let Some(win) = app.get_webview_window("search") {
        let _ = win.hide();
    }
}

#[tauri::command]
fn resize_search(height: f64, app: AppHandle) {
    if let Some(win) = app.get_webview_window("search") {
        let _ = win.set_size(LogicalSize::new(
            SEARCH_W,
            height.clamp(SEARCH_MIN_H, SEARCH_MAX_H),
        ));
    }
}

fn build_search(app: &AppHandle, appearance: Appearance) -> tauri::Result<tauri::WebviewWindow> {
    let win = WebviewWindowBuilder::new(app, "search", WebviewUrl::App("search.html".into()))
        .title("Lanyard — search")
        .inner_size(SEARCH_W, SEARCH_MIN_H)
        .decorations(false)
        .transparent(true)
        .always_on_top(true)
        .skip_taskbar(true)
        .resizable(false)
        .shadow(true)
        .visible(false)
        .accept_first_mouse(true)
        .theme(Some(theme_of(appearance)))
        // The pill's glass, at palette scale: the same Popover material,
        // rounded to match the sheet the CSS draws on top of it.
        .effects(WindowEffectsConfig {
            effects: vec![WindowEffect::Popover],
            state: Some(WindowEffectState::Active),
            radius: Some(16.0),
            color: None,
        })
        .build()?;
    // Summonable on whatever Space you're on, like the panel.
    win.set_visible_on_all_workspaces(true)?;
    Ok(win)
}

/// Centres the palette on whichever display holds the cursor, Spotlight-high,
/// so it appears where the user is looking rather than on the primary screen.
fn position_search(app: &AppHandle, win: &tauri::WebviewWindow) {
    let monitor = app
        .cursor_position()
        .ok()
        .and_then(|p| app.monitor_from_point(p.x, p.y).ok().flatten())
        .or_else(|| app.primary_monitor().ok().flatten());
    let Some(monitor) = monitor else { return };
    let scale = monitor.scale_factor();
    let m_pos = monitor.position().to_logical::<f64>(scale);
    let m_size = monitor.size().to_logical::<f64>(scale);
    let x = m_pos.x + (m_size.width - SEARCH_W) / 2.0;
    let y = m_pos.y + m_size.height * SEARCH_TOP_RATIO;
    let _ = win.set_position(LogicalPosition::new(x.round(), y.round()));
}

fn show_search(app: &AppHandle, appearance: Appearance) -> tauri::Result<()> {
    let win = match app.get_webview_window("search") {
        Some(win) => win,
        None => build_search(app, appearance)?,
    };
    // Reset before it becomes visible, so the previous query never flashes.
    let _ = app.emit_to("search", "lanyard://search-open", ());
    position_search(app, &win);
    win.show()?;
    win.set_focus()?;
    Ok(())
}

fn toggle_search(app: &AppHandle, appearance: Appearance) {
    if let Some(win) = app.get_webview_window("search") {
        if win.is_visible().unwrap_or(false) {
            let _ = win.hide();
            return;
        }
    }
    let _ = show_search(app, appearance);
}

fn show_panel(app: &AppHandle, appearance: Appearance) -> tauri::Result<()> {
    if let Some(panel) = app.get_webview_window("panel") {
        panel.show()?;
        panel.set_focus()?;
        return Ok(());
    }
    let panel = WebviewWindowBuilder::new(app, "panel", WebviewUrl::App("panel.html".into()))
        .title("Lanyard — sessions")
        .inner_size(380.0, 480.0)
        .min_inner_size(340.0, 220.0)
        .resizable(true)
        .transparent(true)
        // Frameless glass: the title bar overlays the content and shows only
        // the traffic lights; a drag-region header in the page stands in for
        // the missing chrome.
        .title_bar_style(tauri::TitleBarStyle::Overlay)
        .hidden_title(true)
        .theme(Some(theme_of(appearance)))
        .effects(WindowEffectsConfig {
            effects: vec![WindowEffect::Sidebar],
            state: Some(WindowEffectState::Active),
            radius: None,
            color: None,
        })
        .build()?;
    panel.set_visible_on_all_workspaces(true)?;
    Ok(())
}

/// The permissions checklist: which grants exist, which are missing, and a
/// button that actually re-asks for each. Deliberately *not* a popover — it
/// has to survive a trip to System Settings and back, so it stays until
/// closed.
fn show_setup(app: &AppHandle, appearance: Appearance) -> tauri::Result<()> {
    if let Some(win) = app.get_webview_window("setup") {
        win.show()?;
        win.set_focus()?;
        return Ok(());
    }
    let win = WebviewWindowBuilder::new(app, "setup", WebviewUrl::App("setup.html".into()))
        .title("Lanyard — permissions")
        .inner_size(420.0, 372.0)
        .resizable(false)
        .transparent(true)
        .title_bar_style(tauri::TitleBarStyle::Overlay)
        .hidden_title(true)
        .theme(Some(theme_of(appearance)))
        .effects(WindowEffectsConfig {
            effects: vec![WindowEffect::Sidebar],
            state: Some(WindowEffectState::Active),
            radius: None,
            color: None,
        })
        .build()?;
    win.set_visible_on_all_workspaces(true)?;
    Ok(())
}

fn build_floater(app: &AppHandle, appearance: Appearance) -> tauri::Result<()> {
    let floater = WebviewWindowBuilder::new(app, "floater", WebviewUrl::App("index.html".into()))
        .title("Lanyard")
        .inner_size(PILL_DEFAULT_W, PILL_H)
        .decorations(false)
        .transparent(true)
        .always_on_top(true)
        .skip_taskbar(true)
        .resizable(false)
        .shadow(true)
        .focused(false)
        .visible(false)
        // Dragging must work from the first click, without focusing the pill.
        .accept_first_mouse(true)
        .theme(Some(theme_of(appearance)))
        // The shell layer of the glass: a real NSVisualEffectView blurring
        // whatever sits behind the window, rounded into a capsule. `Active`
        // keeps it lively even though the pill is almost never the key window.
        .effects(WindowEffectsConfig {
            effects: vec![WindowEffect::Popover],
            state: Some(WindowEffectState::Active),
            radius: Some(PILL_H / 2.0),
            color: None,
        })
        .build()?;
    // The whole point: one floater that follows you across every Space.
    floater.set_visible_on_all_workspaces(true)?;
    Ok(())
}

/// Applies the configured theme to every window; the webviews re-skin
/// themselves when the next snapshot carries the new appearance.
fn apply_theme(app: &AppHandle, appearance: Appearance) {
    for label in ["floater", "panel", "search", "setup"] {
        if let Some(window) = app.get_webview_window(label) {
            let _ = window.set_theme(Some(theme_of(appearance)));
        }
    }
}

fn toggle_panel(app: &AppHandle, appearance: Appearance) {
    if let Some(panel) = app.get_webview_window("panel") {
        if panel.is_visible().unwrap_or(false) {
            let _ = panel.hide();
            return;
        }
    }
    let _ = show_panel(app, appearance);
}

/// The three global shortcuts, unified so the tray presets and the startup
/// registration share one code path.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum HotkeyGroup {
    Panel,
    Search,
    Waiting,
}

impl HotkeyGroup {
    fn parse(s: &str) -> Option<Self> {
        match s {
            "panel" => Some(Self::Panel),
            "search" => Some(Self::Search),
            "waiting" => Some(Self::Waiting),
            _ => None,
        }
    }
}

/// The current combo for a group, straight from config.
fn hotkey_of(config: &Config, group: HotkeyGroup) -> String {
    match group {
        HotkeyGroup::Panel => config.hotkey.trim().to_string(),
        HotkeyGroup::Search => config.search_hotkey.trim().to_string(),
        HotkeyGroup::Waiting => config.waiting_hotkey.trim().to_string(),
    }
}

fn set_hotkey_of(config: &mut Config, group: HotkeyGroup, keys: &str) {
    let keys = keys.to_string();
    match group {
        HotkeyGroup::Panel => config.hotkey = keys,
        HotkeyGroup::Search => config.search_hotkey = keys,
        HotkeyGroup::Waiting => config.waiting_hotkey = keys,
    }
}

/// Registers `keys` to its group's action. An unparseable or taken shortcut
/// costs that hotkey, never the app.
fn register_hotkey_action(
    app: &AppHandle,
    state: &Arc<State>,
    group: HotkeyGroup,
    keys: &str,
) -> Result<(), String> {
    let shortcut_state = state.clone();
    app.global_shortcut()
        .on_shortcut(keys, move |app, _shortcut, event| {
            if event.state != ShortcutState::Pressed {
                return;
            }
            match group {
                HotkeyGroup::Panel => {
                    let appearance = shortcut_state.config.lock().unwrap().appearance;
                    toggle_panel(app, appearance);
                }
                HotkeyGroup::Search => {
                    let appearance = shortcut_state.config.lock().unwrap().appearance;
                    toggle_search(app, appearance);
                }
                HotkeyGroup::Waiting => raise_waiting(app, &shortcut_state),
            }
        })
        .map_err(|e| e.to_string())
}

/// One selectable preset row: its group, the combo it stands for ("" is
/// Disabled), and the check item that displays it.
type HotkeyPreset = (HotkeyGroup, String, CheckMenuItem<tauri::Wry>);

/// Applies a preset picked from the tray: swap the registration, persist, and
/// reconcile every check mark in the group from what config now says.
fn choose_hotkey(app: &AppHandle, state: &Arc<State>, choice: &str, presets: &[HotkeyPreset]) {
    let Some((group_str, keys)) = choice.split_once(':') else {
        return;
    };
    let Some(group) = HotkeyGroup::parse(group_str) else {
        return;
    };

    let old = hotkey_of(&state.config.lock().unwrap(), group);
    if keys != old {
        if !old.is_empty() {
            let _ = app.global_shortcut().unregister(old.as_str());
        }
        let registered = keys.is_empty() || register_hotkey_action(app, state, group, keys).is_ok();
        if registered {
            let mut config = state.config.lock().unwrap();
            set_hotkey_of(&mut config, group, keys);
            let _ = config.save();
        } else {
            // The combo is taken (or invalid): put the old one back and say so.
            let _ = register_hotkey_action(app, state, group, &old);
            notify::post(
                "Lanyard",
                &format!("Couldn't grab {keys} — another app may own it."),
            );
        }
    }

    let current = hotkey_of(&state.config.lock().unwrap(), group);
    for (preset_group, preset_keys, item) in presets {
        if *preset_group == group {
            let _ = item.set_checked(*preset_keys == current);
        }
    }
}

/// Checks the release feed, and — when `interactive` — installs what it finds.
///
/// The quiet launch check only retitles the tray item ("Update to vX.Y.Z…"),
/// so an update is always a deliberate click, never a surprise restart. The
/// same click re-checks first, which keeps a single code path and costs one
/// request against a feed we were about to download from anyway.
fn spawn_update_check(app: AppHandle, item: MenuItem<tauri::Wry>, interactive: bool) {
    tauri::async_runtime::spawn(async move {
        let Ok(updater) = app.updater() else { return };
        match updater.check().await {
            Ok(Some(update)) => {
                let _ = item.set_text(format!("Update to v{}…", update.version));
                if !interactive {
                    return;
                }
                notify::post("Lanyard", &format!("Updating to v{}…", update.version));
                match update.download_and_install(|_, _| {}, || {}).await {
                    Ok(()) => app.restart(),
                    Err(e) => {
                        let _ = item.set_text("Check for updates…");
                        notify::post("Lanyard", &format!("Update failed: {e}"));
                    }
                }
            }
            Ok(None) => {
                let _ = item.set_text("Check for updates…");
                if interactive {
                    notify::post("Lanyard", "Lanyard is up to date.");
                }
            }
            // Offline is the ordinary failure here; only a clicked check
            // deserves an explanation.
            Err(e) => {
                if interactive {
                    notify::post("Lanyard", &format!("Update check failed: {e}"));
                }
            }
        }
    });
}

fn build_tray(app: &AppHandle, state: Arc<State>) -> tauri::Result<()> {
    let config = state.config.lock().unwrap().clone();
    let login = app.autolaunch().is_enabled().unwrap_or(false);

    let panel_item = MenuItem::with_id(app, "panel", "Sessions…", true, None::<&str>)?;
    let search_item = MenuItem::with_id(app, "search", "Find session…", true, None::<&str>)?;
    let waiting_item =
        MenuItem::with_id(app, "waiting", "Jump to waiting session", true, None::<&str>)?;
    let sep1 = PredefinedMenuItem::separator(app)?;
    let show_item =
        CheckMenuItem::with_id(app, "toggle", "Show pill", true, config.enabled, None::<&str>)?;
    let light_item = CheckMenuItem::with_id(
        app,
        "light",
        "Light appearance",
        true,
        config.appearance == Appearance::Light,
        None::<&str>,
    )?;
    let notify_item = CheckMenuItem::with_id(
        app,
        "notify",
        "Notify when a session needs you",
        true,
        config.notify,
        None::<&str>,
    )?;
    let titles_item = CheckMenuItem::with_id(
        app,
        "titles",
        "Manage window titles",
        true,
        config.stamp_titles,
        None::<&str>,
    )?;
    let login_item =
        CheckMenuItem::with_id(app, "login", "Start at login", true, login, None::<&str>)?;

    // Shortcut presets: one submenu per hotkey, check-marked from config.
    // Any other combo can still be set in config.json; it simply shows as no
    // preset being checked.
    let preset_table: [(HotkeyGroup, &str, &[(&str, &str)]); 3] = [
        (
            HotkeyGroup::Panel,
            "Panel shortcut",
            &[
                ("⌃⌘L", "ctrl+cmd+l"),
                ("⌥⌘L", "alt+cmd+l"),
                ("⇧⌘L", "shift+cmd+l"),
                ("Disabled", ""),
            ],
        ),
        (
            HotkeyGroup::Search,
            "Find session shortcut",
            &[
                ("⌃⌘K", "ctrl+cmd+k"),
                ("⌥⌘K", "alt+cmd+k"),
                ("⇧⌘K", "shift+cmd+k"),
                ("⌥Space", "alt+space"),
                ("Disabled", ""),
            ],
        ),
        (
            HotkeyGroup::Waiting,
            "Waiting session shortcut",
            &[
                ("⌃⌘J", "ctrl+cmd+j"),
                ("⌥⌘J", "alt+cmd+j"),
                ("⇧⌘J", "shift+cmd+j"),
                ("Disabled", ""),
            ],
        ),
    ];
    let mut presets: Vec<HotkeyPreset> = Vec::new();
    let mut group_menus = Vec::new();
    for (group, title, rows) in preset_table {
        let current = hotkey_of(&config, group);
        let mut items = Vec::new();
        for (label, keys) in rows {
            let id = format!(
                "hk:{}:{keys}",
                match group {
                    HotkeyGroup::Panel => "panel",
                    HotkeyGroup::Search => "search",
                    HotkeyGroup::Waiting => "waiting",
                }
            );
            let item =
                CheckMenuItem::with_id(app, id, *label, true, *keys == current, None::<&str>)?;
            presets.push((group, keys.to_string(), item.clone()));
            items.push(item);
        }
        let refs: Vec<&dyn tauri::menu::IsMenuItem<tauri::Wry>> =
            items.iter().map(|i| i as _).collect();
        group_menus.push(Submenu::with_items(app, title, true, &refs)?);
    }
    let shortcut_refs: Vec<&dyn tauri::menu::IsMenuItem<tauri::Wry>> =
        group_menus.iter().map(|m| m as _).collect();
    let shortcuts_menu = Submenu::with_items(app, "Shortcuts", true, &shortcut_refs)?;

    let sep2 = PredefinedMenuItem::separator(app)?;
    let perms_item = MenuItem::with_id(app, "perms", "Permissions…", true, None::<&str>)?;
    let update_item =
        MenuItem::with_id(app, "update", "Check for updates…", true, None::<&str>)?;
    let quit_item = MenuItem::with_id(app, "quit", "Quit Lanyard", true, None::<&str>)?;
    let menu = Menu::with_items(
        app,
        &[
            &panel_item,
            &search_item,
            &waiting_item,
            &sep1,
            &show_item,
            &light_item,
            &notify_item,
            &titles_item,
            &login_item,
            &shortcuts_menu,
            &sep2,
            &perms_item,
            &update_item,
            &quit_item,
        ],
    )?;

    // Quietly learn about newer releases shortly after launch; a found update
    // only retitles the menu item and waits for a deliberate click.
    {
        let app = app.clone();
        let item = update_item.clone();
        std::thread::spawn(move || {
            std::thread::sleep(std::time::Duration::from_secs(10));
            spawn_update_check(app, item, false);
        });
    }

    // A template image: macOS ignores the colour and tints the alpha to suit the
    // menu bar, so one black-on-transparent glyph reads in both light and dark.
    // tray-icon scales it to 18pt, so 36px is exactly 2x on a Retina display.
    let tray = TrayIconBuilder::with_id("lanyard")
        .menu(&menu)
        .icon(Image::from_bytes(TRAY_ICON)?)
        .icon_as_template(true)
        .show_menu_on_left_click(true);

    // The check items toggle themselves; handlers read the new state back.
    tray.on_menu_event(move |app, event| match event.id().as_ref() {
        "panel" => {
            let appearance = state.config.lock().unwrap().appearance;
            let _ = show_panel(app, appearance);
        }
        "search" => {
            let appearance = state.config.lock().unwrap().appearance;
            let _ = show_search(app, appearance);
        }
        "waiting" => raise_waiting(app, &state),
        "toggle" => {
            let mut config = state.config.lock().unwrap();
            config.enabled = show_item.is_checked().unwrap_or(!config.enabled);
            let _ = config.save();
        }
        "light" => {
            let appearance = if light_item.is_checked().unwrap_or(false) {
                Appearance::Light
            } else {
                Appearance::Dark
            };
            {
                let mut config = state.config.lock().unwrap();
                config.appearance = appearance;
                let _ = config.save();
            }
            apply_theme(app, appearance);
        }
        "notify" => {
            let mut config = state.config.lock().unwrap();
            config.notify = notify_item.is_checked().unwrap_or(true);
            let _ = config.save();
        }
        "titles" => {
            let mut config = state.config.lock().unwrap();
            config.stamp_titles = titles_item.is_checked().unwrap_or(true);
            let _ = config.save();
        }
        "login" => {
            let manager = app.autolaunch();
            let result = if login_item.is_checked().unwrap_or(false) {
                manager.enable()
            } else {
                manager.disable()
            };
            if result.is_err() {
                // Reflect reality if the launcher refused.
                let _ = login_item.set_checked(manager.is_enabled().unwrap_or(false));
            }
        }
        "perms" => {
            let appearance = state.config.lock().unwrap().appearance;
            let _ = show_setup(app, appearance);
        }
        "update" => spawn_update_check(app.clone(), update_item.clone(), true),
        "quit" => app.exit(0),
        other => {
            if let Some(choice) = other.strip_prefix("hk:") {
                choose_hotkey(app, &state, choice, &presets);
            }
        }
    })
    .build(app)?;

    Ok(())
}

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    let state = Arc::new(State::new(Config::load()));

    tauri::Builder::default()
        .plugin(tauri_plugin_updater::Builder::new().build())
        .plugin(tauri_plugin_autostart::init(
            MacosLauncher::LaunchAgent,
            None,
        ))
        .plugin(tauri_plugin_global_shortcut::Builder::new().build())
        .manage(state.clone())
        .invoke_handler(tauri::generate_handler![
            get_state,
            rename,
            set_enabled,
            request_accessibility,
            request_notifications,
            request_file_access,
            resize_pill,
            set_drag_offset,
            commit_drag,
            focus_floater,
            raise_session,
            open_panel,
            hide_search,
            resize_search
        ])
        .setup(move |app| {
            // Menu-bar app: no Dock icon, and showing the floater never steals focus.
            #[cfg(target_os = "macos")]
            app.set_activation_policy(tauri::ActivationPolicy::Accessory);

            let appearance = state.config.lock().unwrap().appearance;
            let handle = app.handle().clone();
            build_floater(&handle, appearance)?;
            build_tray(&handle, state.clone())?;

            // The panel, the search and the waiting-jump from anywhere, hands
            // on the keyboard.
            for group in [HotkeyGroup::Panel, HotkeyGroup::Search, HotkeyGroup::Waiting] {
                let keys = hotkey_of(&state.config.lock().unwrap(), group);
                if keys.is_empty() {
                    continue;
                }
                if let Err(e) = register_hotkey_action(&handle, &state, group, &keys) {
                    eprintln!("lanyard: could not register hotkey {keys:?}: {e}");
                }
            }

            if !ax::is_trusted() {
                // Self-repairing: clears any stale/denied TCC entry so the
                // prompt actually appears. The tray item re-runs it later.
                ax::repair_trust();
            }

            // Notification consent, asked once; the system remembers.
            notify::init();

            tracker::spawn(handle.clone(), state.clone());

            // macOS queues its consent dialogs one behind another, and a
            // missed one is indistinguishable from a bug. If anything is
            // still ungranted once the first scans land, open the checklist
            // where it can't be missed.
            {
                let handle = handle.clone();
                let state = state.clone();
                std::thread::spawn(move || {
                    std::thread::sleep(std::time::Duration::from_secs(2));
                    let missing = {
                        let snapshot = state.snapshot.lock().unwrap();
                        !snapshot.ax_trusted || snapshot.notifications != "granted"
                    };
                    if !missing {
                        return;
                    }
                    let appearance = state.config.lock().unwrap().appearance;
                    let ui = handle.clone();
                    let _ = handle.run_on_main_thread(move || {
                        let _ = show_setup(&ui, appearance);
                    });
                });
            }
            Ok(())
        })
        .on_window_event(|window, event| {
            let label = window.label();
            let popover = label == "panel" || label == "search";
            if !popover && label != "setup" {
                return;
            }
            match event {
                // Closing any of them should park it, not tear down the app.
                WindowEvent::CloseRequested { api, .. } => {
                    api.prevent_close();
                    let _ = window.hide();
                }
                // Popover semantics: as an Accessory app, Lanyard never
                // appears in ⌘-tab, so a de-focused overlay would be
                // unreachable — and, pinned to every Space, it would flash
                // through Space transitions. Clicking away dismisses it;
                // the tray and the global hotkeys summon it back anywhere.
                // The permissions window is the exception: it must survive a
                // trip to System Settings and back, so only closing parks it.
                WindowEvent::Focused(false) if popover => {
                    let _ = window.hide();
                }
                _ => {}
            }
        })
        .run(tauri::generate_context!())
        .expect("error while running Lanyard");
}
