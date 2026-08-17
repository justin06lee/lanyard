pub mod ax;
pub mod sessions;
pub mod store;
pub mod title;
pub mod tracker;

use std::sync::atomic::Ordering;
use std::sync::Arc;

use serde::Serialize;
use tauri::image::Image;
use tauri::menu::{CheckMenuItem, Menu, MenuItem, PredefinedMenuItem};
use tauri::tray::TrayIconBuilder;
use tauri_plugin_autostart::{MacosLauncher, ManagerExt};
use tauri_plugin_global_shortcut::{GlobalShortcutExt, ShortcutState};
use tauri_plugin_notification::NotificationExt;
use tauri_plugin_updater::UpdaterExt;
use tauri::utils::config::WindowEffectsConfig;
use tauri::utils::{WindowEffect, WindowEffectState};
use tauri::{
    AppHandle, LogicalPosition, LogicalSize, Manager, Theme, WebviewUrl, WebviewWindowBuilder,
    WindowEvent,
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

#[tauri::command]
fn open_panel(app: AppHandle, state: tauri::State<'_, Arc<State>>) -> Result<(), String> {
    let appearance = state.config.lock().unwrap().appearance;
    show_panel(&app, appearance).map_err(|e| e.to_string())
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
    for label in ["floater", "panel"] {
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
                let _ = app
                    .notification()
                    .builder()
                    .title("Lanyard")
                    .body(format!("Updating to v{}…", update.version))
                    .show();
                match update.download_and_install(|_, _| {}, || {}).await {
                    Ok(()) => app.restart(),
                    Err(e) => {
                        let _ = item.set_text("Check for Updates…");
                        let _ = app
                            .notification()
                            .builder()
                            .title("Lanyard")
                            .body(format!("Update failed: {e}"))
                            .show();
                    }
                }
            }
            Ok(None) => {
                let _ = item.set_text("Check for Updates…");
                if interactive {
                    let _ = app
                        .notification()
                        .builder()
                        .title("Lanyard")
                        .body("Lanyard is up to date.")
                        .show();
                }
            }
            // Offline is the ordinary failure here; only a clicked check
            // deserves an explanation.
            Err(e) => {
                if interactive {
                    let _ = app
                        .notification()
                        .builder()
                        .title("Lanyard")
                        .body(format!("Update check failed: {e}"))
                        .show();
                }
            }
        }
    });
}

fn build_tray(app: &AppHandle, state: Arc<State>) -> tauri::Result<()> {
    let config = state.config.lock().unwrap().clone();
    let login = app.autolaunch().is_enabled().unwrap_or(false);

    let panel_item = MenuItem::with_id(app, "panel", "Sessions…", true, None::<&str>)?;
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
    let sep2 = PredefinedMenuItem::separator(app)?;
    let ax_item = MenuItem::with_id(app, "ax", "Accessibility access…", true, None::<&str>)?;
    let update_item =
        MenuItem::with_id(app, "update", "Check for Updates…", true, None::<&str>)?;
    let quit_item = MenuItem::with_id(app, "quit", "Quit Lanyard", true, None::<&str>)?;
    let menu = Menu::with_items(
        app,
        &[
            &panel_item,
            &sep1,
            &show_item,
            &light_item,
            &notify_item,
            &titles_item,
            &login_item,
            &sep2,
            &ax_item,
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
        "ax" => {
            ax::repair_trust();
        }
        "update" => spawn_update_check(app.clone(), update_item.clone(), true),
        "quit" => app.exit(0),
        _ => {}
    })
    .build(app)?;

    Ok(())
}

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    let state = Arc::new(State::new(Config::load()));

    tauri::Builder::default()
        .plugin(tauri_plugin_updater::Builder::new().build())
        .plugin(tauri_plugin_notification::init())
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
            resize_pill,
            set_drag_offset,
            commit_drag,
            focus_floater,
            raise_session,
            open_panel
        ])
        .setup(move |app| {
            // Menu-bar app: no Dock icon, and showing the floater never steals focus.
            #[cfg(target_os = "macos")]
            app.set_activation_policy(tauri::ActivationPolicy::Accessory);

            let (appearance, hotkey) = {
                let config = state.config.lock().unwrap();
                (config.appearance, config.hotkey.trim().to_string())
            };
            let handle = app.handle().clone();
            build_floater(&handle, appearance)?;
            build_tray(&handle, state.clone())?;

            // The panel from anywhere, hands on the keyboard. An unparseable
            // or taken shortcut costs the hotkey, never the app.
            if !hotkey.is_empty() {
                let shortcut_state = state.clone();
                if let Err(e) = app.global_shortcut().on_shortcut(
                    hotkey.as_str(),
                    move |app, _shortcut, event| {
                        if event.state == ShortcutState::Pressed {
                            let appearance = shortcut_state.config.lock().unwrap().appearance;
                            toggle_panel(app, appearance);
                        }
                    },
                ) {
                    eprintln!("lanyard: could not register hotkey {hotkey:?}: {e}");
                }
            }

            if !ax::is_trusted() {
                // Self-repairing: clears any stale/denied TCC entry so the
                // prompt actually appears. The tray item re-runs it later.
                ax::repair_trust();
            }

            tracker::spawn(handle, state.clone());
            Ok(())
        })
        .on_window_event(|window, event| {
            if window.label() != "panel" {
                return;
            }
            match event {
                // Closing the panel should park it, not tear down the app.
                WindowEvent::CloseRequested { api, .. } => {
                    api.prevent_close();
                    let _ = window.hide();
                }
                // Popover semantics: as an Accessory app, Lanyard never
                // appears in ⌘-tab, so a de-focused panel would be
                // unreachable — and, pinned to every Space, it would flash
                // through Space transitions. Clicking away dismisses it;
                // the tray and the global hotkey summon it back anywhere.
                WindowEvent::Focused(false) => {
                    let _ = window.hide();
                }
                _ => {}
            }
        })
        .run(tauri::generate_context!())
        .expect("error while running Lanyard");
}
