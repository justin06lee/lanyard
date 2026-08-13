pub mod ax;
pub mod sessions;
pub mod store;
pub mod title;
pub mod tracker;

use std::sync::atomic::Ordering;
use std::sync::Arc;

use tauri::image::Image;
use tauri::menu::{Menu, MenuItem, PredefinedMenuItem};
use tauri::tray::TrayIconBuilder;
use tauri::{
    AppHandle, LogicalPosition, Manager, WebviewUrl, WebviewWindowBuilder, WindowEvent,
};

use store::Config;
use tracker::{Snapshot, State, FLOATER_H, FLOATER_W};

const TRAY_ICON: &[u8] = include_bytes!("../icons/tray.png");

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
fn cycle_anchor(state: tauri::State<'_, Arc<State>>) -> Result<String, String> {
    let mut config = state.config.lock().unwrap();
    config.anchor = config.anchor.next();
    config.save().map_err(|e| e.to_string())?;
    Ok(format!("{:?}", config.anchor))
}

#[tauri::command]
fn set_enabled(enabled: bool, state: tauri::State<'_, Arc<State>>) -> Result<(), String> {
    let mut config = state.config.lock().unwrap();
    config.enabled = enabled;
    config.save().map_err(|e| e.to_string())
}

#[tauri::command]
fn request_accessibility() -> bool {
    ax::request_trust()
}

/// Displaces the floater from its anchor while the user drags it aside.
///
/// The UI calls this every frame during a drag and again as it springs back to
/// `(0, 0)` on release. Repositioning here rather than waiting for the tracker's
/// next tick is what makes the drag feel attached to the cursor; the tracker
/// keeps applying the same offset so the pill still follows the terminal window
/// if it moves mid-drag.
#[tauri::command]
fn set_drag_offset(x: f64, y: f64, app: AppHandle, state: tauri::State<'_, Arc<State>>) {
    *state.drag_offset.lock().unwrap() = (x, y);
    let anchor = *state.anchor_pos.lock().unwrap();
    if let (Some(window), Some((ax, ay))) = (app.get_webview_window("floater"), anchor) {
        let _ = window.set_position(LogicalPosition::new(ax + x, ay + y));
    }
}

#[tauri::command]
fn open_panel(app: AppHandle) -> Result<(), String> {
    show_panel(&app).map_err(|e| e.to_string())
}

fn show_panel(app: &AppHandle) -> tauri::Result<()> {
    if let Some(panel) = app.get_webview_window("panel") {
        panel.show()?;
        panel.set_focus()?;
        return Ok(());
    }
    let panel = WebviewWindowBuilder::new(app, "panel", WebviewUrl::App("panel.html".into()))
        .title("Lanyard — sessions")
        .inner_size(420.0, 520.0)
        .min_inner_size(360.0, 240.0)
        .resizable(true)
        .build()?;
    panel.set_visible_on_all_workspaces(true)?;
    Ok(())
}

fn build_floater(app: &AppHandle) -> tauri::Result<()> {
    let floater = WebviewWindowBuilder::new(app, "floater", WebviewUrl::App("index.html".into()))
        .title("Lanyard")
        .inner_size(FLOATER_W, FLOATER_H)
        .decorations(false)
        .transparent(true)
        .always_on_top(true)
        .skip_taskbar(true)
        .resizable(false)
        .shadow(false)
        .focused(false)
        .visible(false)
        .build()?;
    // The whole point: one floater that follows you across every Space.
    floater.set_visible_on_all_workspaces(true)?;
    Ok(())
}

fn build_tray(app: &AppHandle, state: Arc<State>) -> tauri::Result<()> {
    let panel_item = MenuItem::with_id(app, "panel", "Sessions…", true, None::<&str>)?;
    let toggle_item = MenuItem::with_id(app, "toggle", "Toggle floater", true, None::<&str>)?;
    let anchor_item = MenuItem::with_id(app, "anchor", "Move floater", true, None::<&str>)?;
    let ax_item = MenuItem::with_id(app, "ax", "Accessibility access…", true, None::<&str>)?;
    let sep = PredefinedMenuItem::separator(app)?;
    let quit_item = MenuItem::with_id(app, "quit", "Quit Lanyard", true, None::<&str>)?;
    let menu = Menu::with_items(
        app,
        &[
            &panel_item,
            &toggle_item,
            &anchor_item,
            &sep,
            &ax_item,
            &quit_item,
        ],
    )?;

    // A template image: macOS ignores the colour and tints the alpha to suit the
    // menu bar, so one black-on-transparent glyph reads in both light and dark.
    // tray-icon scales it to 18pt, so 36px is exactly 2x on a Retina display.
    let tray = TrayIconBuilder::with_id("lanyard")
        .menu(&menu)
        .icon(Image::from_bytes(TRAY_ICON)?)
        .icon_as_template(true)
        .show_menu_on_left_click(true);

    tray.on_menu_event(move |app, event| match event.id().as_ref() {
        "panel" => {
            let _ = show_panel(app);
        }
        "toggle" => {
            let mut config = state.config.lock().unwrap();
            config.enabled = !config.enabled;
            let _ = config.save();
        }
        "anchor" => {
            let mut config = state.config.lock().unwrap();
            config.anchor = config.anchor.next();
            let _ = config.save();
        }
        "ax" => {
            ax::request_trust();
        }
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
        .manage(state.clone())
        .invoke_handler(tauri::generate_handler![
            get_state,
            rename,
            cycle_anchor,
            set_enabled,
            request_accessibility,
            set_drag_offset,
            open_panel
        ])
        .setup(move |app| {
            // Menu-bar app: no Dock icon, and showing the floater never steals focus.
            #[cfg(target_os = "macos")]
            app.set_activation_policy(tauri::ActivationPolicy::Accessory);

            let handle = app.handle().clone();
            build_floater(&handle)?;
            build_tray(&handle, state.clone())?;

            if !ax::is_trusted() {
                // Ask once on first run; the tray item re-opens it later.
                ax::request_trust();
            }

            tracker::spawn(handle, state.clone());
            Ok(())
        })
        .on_window_event(|window, event| {
            // Closing the panel should park it, not tear down the app.
            if let WindowEvent::CloseRequested { api, .. } = event {
                if window.label() == "panel" {
                    api.prevent_close();
                    let _ = window.hide();
                }
            }
        })
        .run(tauri::generate_context!())
        .expect("error while running Lanyard");
}
