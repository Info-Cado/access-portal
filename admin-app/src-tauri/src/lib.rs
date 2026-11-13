//! Access Portal: the whole portal runs inside this app.
//!
//! On start the app starts the portal server (`access_portal::start`), which
//! serves remote users on the network. Closing the window only hides it to the
//! notification area; the portal keeps running until "Quit" is chosen from the
//! tray icon. The admin panel talks to the portal in-process, so it exists only
//! on this computer and cannot be reached over the network.

use std::path::PathBuf;
use std::sync::Mutex;

use serde_json::{json, Value};
use tauri::menu::{CheckMenuItem, Menu, MenuItem, PredefinedMenuItem};
use tauri::tray::{MouseButton, MouseButtonState, TrayIconBuilder, TrayIconEvent};
use tauri::{AppHandle, Manager, State, WindowEvent};
use tauri_plugin_autostart::{MacosLauncher, ManagerExt};

/// Started from Windows sign-in: stay in the tray instead of opening a window.
const HIDDEN_ARG: &str = "--hidden";

enum Server {
    Running(access_portal::Portal),
    /// Why the portal could not start (shown in the window).
    Failed(String),
}

/// Everything the portal keeps: settings, database, transfer folders, log.
/// Machine-wide, so it doesn't depend on which Windows account opens the app.
fn data_dir() -> PathBuf {
    std::env::var_os("ProgramData")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(r"C:\ProgramData"))
        .join("AccessPortal")
}

/// Log to `portal.log` in the data folder (the app has no console).
fn start_logging(dir: &std::path::Path) {
    let _ = std::fs::create_dir_all(dir);
    let Ok(file) = std::fs::OpenOptions::new().create(true).append(true).open(dir.join("portal.log")) else {
        return;
    };
    let filter = tracing_subscriber::EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| "access_portal=info,tower_http=warn".into());
    let _ = tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_ansi(false)
        .with_writer(Mutex::new(file))
        .try_init();
}

/// Whether the portal is running, where, and where its files are.
#[tauri::command]
fn status(server: State<'_, Server>) -> Value {
    let dir = data_dir().display().to_string();
    match server.inner() {
        Server::Running(p) => json!({ "running": true, "address": p.address, "data_dir": dir }),
        Server::Failed(e) => json!({ "running": false, "error": e, "data_dir": dir }),
    }
}

/// One admin call to the portal, in-process. Answers `{ status, data }`.
#[tauri::command]
async fn request(
    server: State<'_, Server>,
    method: String,
    path: String,
    body: Option<Value>,
) -> Result<Value, String> {
    if !path.starts_with("/api/admin/") {
        return Err("Only the portal's admin calls can be made.".into());
    }
    let portal = match server.inner() {
        Server::Running(p) => p,
        Server::Failed(e) => return Err(format!("The portal is not running: {e}")),
    };
    let (status, data) = portal.admin(&method, &path, body).await;
    Ok(json!({ "status": status, "data": data }))
}

fn show_window(app: &AppHandle) {
    if let Some(w) = app.get_webview_window("main") {
        let _ = w.unminimize();
        let _ = w.show();
        let _ = w.set_focus();
    }
}

fn build_tray(app: &AppHandle, tooltip: &str) -> tauri::Result<()> {
    let open = MenuItem::with_id(app, "open", "Open Access Portal", true, None::<&str>)?;
    let autostart_on = app.autolaunch().is_enabled().unwrap_or(false);
    let autostart = CheckMenuItem::with_id(app, "autostart", "Start with Windows", true, autostart_on, None::<&str>)?;
    let quit = MenuItem::with_id(app, "quit", "Quit (stops the portal)", true, None::<&str>)?;
    let menu = Menu::with_items(
        app,
        &[&open, &PredefinedMenuItem::separator(app)?, &autostart, &PredefinedMenuItem::separator(app)?, &quit],
    )?;

    let mut tray = TrayIconBuilder::with_id("main")
        .tooltip(tooltip)
        .menu(&menu)
        .show_menu_on_left_click(false)
        .on_menu_event(move |app, event| match event.id.as_ref() {
            "open" => show_window(app),
            "autostart" => {
                let launcher = app.autolaunch();
                let on = launcher.is_enabled().unwrap_or(false);
                let _ = if on { launcher.disable() } else { launcher.enable() };
                let _ = autostart.set_checked(launcher.is_enabled().unwrap_or(false));
            }
            "quit" => app.exit(0),
            _ => {}
        })
        .on_tray_icon_event(|tray, event| {
            if let TrayIconEvent::Click { button: MouseButton::Left, button_state: MouseButtonState::Up, .. } = event {
                show_window(tray.app_handle());
            }
        });
    if let Some(icon) = app.default_window_icon() {
        tray = tray.icon(icon.clone());
    }
    tray.build(app)?;
    Ok(())
}

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    tauri::Builder::default()
        // A second launch just brings the running copy to the front (two copies
        // would fight over the portal's port).
        .plugin(tauri_plugin_single_instance::init(|app, _args, _cwd| show_window(app)))
        .plugin(tauri_plugin_autostart::init(MacosLauncher::LaunchAgent, Some(vec![HIDDEN_ARG])))
        .setup(|app| {
            let dir = data_dir();
            start_logging(&dir);

            let server = match tauri::async_runtime::block_on(access_portal::start(&dir)) {
                Ok(portal) => Server::Running(portal),
                Err(e) => {
                    tracing::error!("the portal could not start: {e:#}");
                    Server::Failed(format!("{e:#}"))
                }
            };
            let tooltip = match &server {
                Server::Running(p) => format!("Access Portal — running on {}", p.address),
                Server::Failed(_) => "Access Portal — not running".to_string(),
            };
            app.manage(server);

            // Start with Windows by default, once; after that the tray setting
            // rules. While it is on, re-register on every start so the entry
            // follows the app if it moved (e.g. from a build folder to its
            // installed location).
            let marker = dir.join(".autostart-chosen");
            let launcher = app.autolaunch();
            if !marker.exists() {
                let _ = launcher.enable();
                let _ = std::fs::write(&marker, b"");
            } else if launcher.is_enabled().unwrap_or(false) {
                let _ = launcher.enable();
            }

            build_tray(app.handle(), &tooltip)?;

            if !std::env::args().any(|a| a == HIDDEN_ARG) {
                show_window(app.handle());
            }
            Ok(())
        })
        .on_window_event(|window, event| {
            // Closing the window keeps the portal running in the tray.
            if let WindowEvent::CloseRequested { api, .. } = event {
                api.prevent_close();
                let _ = window.hide();
            }
        })
        .invoke_handler(tauri::generate_handler![status, request])
        .run(tauri::generate_context!())
        .expect("running Access Portal");
}
