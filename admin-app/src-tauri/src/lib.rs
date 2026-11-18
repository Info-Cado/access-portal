//! Access Portal: the whole portal runs inside this app.
//!
//! On start the app starts the portal server (`access_portal::start`), which
//! serves remote users on the network. Closing the window only hides it to the
//! notification area; the portal keeps running until "Quit" is chosen from the
//! tray icon. The admin panel talks to the portal in-process, so it exists only
//! on this computer and cannot be reached over the network.

mod alerts;
mod elevation;
mod pickers;
mod report;
mod tunnel;
mod updates;

use std::path::PathBuf;
use std::sync::Mutex;

use serde_json::{json, Value};
use tauri::menu::{CheckMenuItem, Menu, MenuItem, PredefinedMenuItem};
use tauri::tray::{MouseButton, MouseButtonState, TrayIconBuilder, TrayIconEvent};
use tauri::{AppHandle, Manager, RunEvent, State, WindowEvent};
use tauri_plugin_autostart::{MacosLauncher, ManagerExt};

/// Started from Windows sign-in: stay in the tray instead of opening a window.
const HIDDEN_ARG: &str = "--hidden";

pub(crate) enum Server {
    Running(access_portal::Portal),
    /// Why the portal could not start (shown in the window).
    Failed(String),
}

/// The window shows nothing until the admin signs in, and locks again when it
/// is closed to the tray. Wrong passwords slow down after a few tries.
#[derive(Default)]
struct AppLock(Mutex<LockState>);

#[derive(Default)]
struct LockState {
    unlocked: bool,
    failures: u32,
    wait_until: Option<std::time::Instant>,
}

impl AppLock {
    fn is_unlocked(&self) -> bool {
        self.0.lock().unwrap().unlocked
    }
    fn lock(&self) {
        self.0.lock().unwrap().unlocked = false;
    }
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

/// Whether the portal is running, where, where its files are, and the public link.
#[tauri::command]
fn status(server: State<'_, Server>, tunnel: State<'_, Option<tunnel::Tunnel>>, lock: State<'_, AppLock>) -> Value {
    let dir = data_dir().display().to_string();
    let public = tunnel.inner().as_ref().map(|t| t.status().to_json()).unwrap_or(json!({ "state": "unavailable" }));
    let locked = !lock.is_unlocked();
    match server.inner() {
        Server::Running(p) => json!({ "running": true, "address": p.address, "data_dir": dir, "public": public, "locked": locked, "elevated": elevation::is_elevated() }),
        Server::Failed(e) => json!({ "running": false, "error": e, "data_dir": dir, "public": public, "locked": locked, "elevated": elevation::is_elevated() }),
    }
}

/// Sign in to the app.
#[tauri::command]
async fn unlock(server: State<'_, Server>, lock: State<'_, AppLock>, username: String, password: String) -> Result<Value, String> {
    let portal = match server.inner() {
        Server::Running(p) => p,
        Server::Failed(e) => return Err(format!("The portal is not running: {e}")),
    };
    if let Some(until) = lock.0.lock().unwrap().wait_until {
        let now = std::time::Instant::now();
        if until > now {
            return Err(format!("Too many wrong passwords. Try again in {} s.", (until - now).as_secs() + 1));
        }
    }
    let (status, data) = portal
        .admin("POST", "/api/admin/auth/verify", Some(json!({ "username": username, "password": password })))
        .await;
    let mut state = lock.0.lock().unwrap();
    if status == 200 {
        *state = LockState { unlocked: true, failures: 0, wait_until: None };
        return Ok(data);
    }
    state.failures += 1;
    if state.failures >= 5 {
        state.wait_until = Some(std::time::Instant::now() + std::time::Duration::from_secs(30));
    }
    Err(data.get("error").and_then(Value::as_str).unwrap_or("Wrong username or password.").to_string())
}

#[tauri::command]
fn lock(lock: State<'_, AppLock>) {
    lock.lock();
}

/// Send a test phone alert to the topic set in the app.
#[tauri::command]
async fn test_alert(app: AppHandle, lock: State<'_, AppLock>, server: String, topic: String) -> Result<(), String> {
    if !lock.is_unlocked() {
        return Err("The app is locked.".into());
    }
    let name = alerts::install_name(&app);
    tauri::async_runtime::spawn_blocking(move || {
        alerts::send(&server, &topic, &format!("{name}: test alert"), "Alerts from Access Portal will arrive like this.", "bell", false)
    })
    .await
    .map_err(|e| e.to_string())?
}

/// Send a problem report (see `report.rs`); answers its number.
#[tauri::command]
async fn send_report(app: AppHandle, server: State<'_, Server>, lock: State<'_, AppLock>) -> Result<String, String> {
    if !lock.is_unlocked() {
        return Err("The app is locked.".into());
    }
    let portal = match server.inner() {
        Server::Running(p) => Some(p),
        Server::Failed(_) => None,
    };
    report::send(&app, portal, &data_dir()).await
}

/// Restart the app with administrator rights (asking Windows if needed); this
/// copy quits once the new one is on its way.
#[tauri::command]
fn restart_as_admin(app: AppHandle, lock: State<'_, AppLock>) -> Result<(), String> {
    if !lock.is_unlocked() {
        return Err("The app is locked.".into());
    }
    if elevation::is_elevated() {
        return Err("The app already runs as administrator.".into());
    }
    elevation::restart_as_admin(false)?;
    tracing::info!("restarting as administrator");
    app.exit(0);
    Ok(())
}

/// Restart the app (and the portal in it), e.g. to restore a backup.
#[tauri::command]
fn restart_app(app: AppHandle, lock: State<'_, AppLock>) -> Result<(), String> {
    if !lock.is_unlocked() {
        return Err("The app is locked.".into());
    }
    app.restart()
}

#[tauri::command]
fn update_status(app: AppHandle) -> Value {
    updates::status_json(&app)
}

/// Check for an update now; with `install`, install it (the app restarts).
#[tauri::command]
async fn update_check(app: AppHandle, lock: State<'_, AppLock>, install: bool) -> Result<Value, String> {
    if !lock.is_unlocked() {
        return Err("The app is locked.".into());
    }
    updates::check(&app, install).await
}

/// Save a file to the Downloads folder and show it in Explorer.
#[tauri::command]
fn save_download(lock: State<'_, AppLock>, name: String, contents: String) -> Result<String, String> {
    if !lock.is_unlocked() {
        return Err("The app is locked.".into());
    }
    let safe: String = name
        .chars()
        .map(|c| if c.is_control() || "<>:\"/\\|?*".contains(c) { '_' } else { c })
        .collect();
    let dir = std::env::var_os("USERPROFILE").map(PathBuf::from).unwrap_or_else(std::env::temp_dir).join("Downloads");
    std::fs::create_dir_all(&dir).map_err(|e| e.to_string())?;
    let path = dir.join(safe);
    std::fs::write(&path, contents.as_bytes()).map_err(|e| format!("Couldn't save {}: {e}", path.display()))?;
    let _ = tauri_plugin_opener::reveal_item_in_dir(&path);
    Ok(path.display().to_string())
}

/// One admin call to the portal, in-process. Answers `{ status, data }`.
#[tauri::command]
async fn request(
    server: State<'_, Server>,
    lock: State<'_, AppLock>,
    method: String,
    path: String,
    body: Option<Value>,
    progress: tauri::ipc::Channel<String>,
) -> Result<Value, String> {
    if !path.starts_with("/api/admin/") || path.starts_with("/api/admin/auth/verify") {
        return Err("Only the portal's admin calls can be made.".into());
    }
    if !lock.is_unlocked() {
        return Ok(json!({ "status": 423, "data": { "error": "The app is locked. Sign in again." } }));
    }
    let portal = match server.inner() {
        Server::Running(p) => p,
        Server::Failed(e) => return Err(format!("The portal is not running: {e}")),
    };
    // Steps of a long action ("Closing Miracle…") go straight to the caller.
    let sink: access_portal::progress::Sink = std::sync::Arc::new(move |text: String| {
        let _ = progress.send(text);
    });
    let (status, data) = portal.admin_with_progress(&method, &path, body, sink).await;
    Ok(json!({ "status": status, "data": data }))
}

/// How long a closed window stays in memory, so reopening it soon is instant.
/// After that it is destroyed: its web view holds about 130 MB.
const KEEP_CLOSED_WINDOW: std::time::Duration = std::time::Duration::from_secs(120);

/// Bumped whenever the window is shown, so a pending "destroy the closed
/// window" from an earlier close knows to leave it alone.
static WINDOW_SHOWN: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

/// Show the window, creating it first if it doesn't exist (it is only created
/// when someone opens it, and destroyed a while after it is closed).
fn show_window(app: &AppHandle) {
    WINDOW_SHOWN.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
    let window = match app.get_webview_window("main") {
        Some(w) => w,
        None => {
            let Some(config) = app.config().app.windows.iter().find(|w| w.label == "main") else { return };
            match tauri::WebviewWindowBuilder::from_config(app, config).and_then(|b| b.build()) {
                Ok(w) => w,
                Err(e) => {
                    tracing::warn!("opening the window: {e}");
                    return;
                }
            }
        }
    };
    let _ = window.unminimize();
    let _ = window.show();
    let _ = window.set_focus();
}

/// Destroy the closed window after a while unless it was opened again.
fn forget_closed_window(window: &tauri::Window) {
    let shown = WINDOW_SHOWN.load(std::sync::atomic::Ordering::SeqCst);
    let app = window.app_handle().clone();
    std::thread::spawn(move || {
        std::thread::sleep(KEEP_CLOSED_WINDOW);
        if WINDOW_SHOWN.load(std::sync::atomic::Ordering::SeqCst) != shown {
            return;
        }
        if let Some(w) = app.get_webview_window("main") {
            if !w.is_visible().unwrap_or(true) {
                let _ = w.destroy();
            }
        }
    });
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
    // Without administrator rights, hand over to a copy that has them (see elevation.rs).
    elevation::early_start(std::env::args().any(|a| a == HIDDEN_ARG));
    tauri::Builder::default()
        // A second launch just brings the running copy to the front (two copies
        // would fight over the portal's port).
        // Running it again with `--update` makes the running copy install an
        // available update right away (for scripts and support).
        .plugin(tauri_plugin_single_instance::init(|app, args, _cwd| {
            if args.iter().any(|a| a == "--update") {
                let app = app.clone();
                tauri::async_runtime::spawn(async move {
                    if let Err(e) = updates::check(&app, true).await {
                        tracing::warn!("update on request: {e}");
                    }
                });
            } else if !args.iter().any(|a| a == HIDDEN_ARG) {
                show_window(app)
            }
        }))
        .plugin(tauri_plugin_autostart::init(MacosLauncher::LaunchAgent, Some(vec![HIDDEN_ARG])))
        .plugin(tauri_plugin_opener::init())
        // File and folder pickers (an app's program and data folder).
        .plugin(tauri_plugin_dialog::init())
        .plugin(tauri_plugin_updater::Builder::new().pubkey(updates::PUBKEY).build())
        .setup(|app| {
            let dir = data_dir();
            start_logging(&dir);
            // Where the portal fetches RDP Wrapper settings from (see host_setup.ps1).
            if let Some(base) = tunnel::service_base() {
                std::env::set_var("ACCESS_PORTAL_SERVICE_URL", base);
            }

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

            // The public link, once the portal is up.
            let public = match &server {
                Server::Running(p) => {
                    let port = p.address.rsplit(':').next().and_then(|v| v.parse().ok()).unwrap_or(8088);
                    let exe_dir = std::env::current_exe().ok().and_then(|e| e.parent().map(PathBuf::from)).unwrap_or_default();
                    Some(tunnel::start(dir.clone(), port, exe_dir))
                }
                Server::Failed(_) => None,
            };
            app.manage(server);
            app.manage(public);
            app.manage(AppLock::default());
            app.manage(updates::UpdateState::default());
            alerts::start(app.handle().clone());
            updates::start(app.handle().clone());
            // Shortcuts that run as administrator, and the tasks that let later
            // copies get administrator rights without asking.
            std::thread::spawn(|| {
                tracing::info!("running as administrator: {}", elevation::is_elevated());
                elevation::fix_shortcuts();
                elevation::register_tasks();
            });

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
                // Whoever opens it next signs in again.
                window.app_handle().state::<AppLock>().lock();
                forget_closed_window(window);
            }
        })
        .invoke_handler(tauri::generate_handler![
            status,
            request,
            unlock,
            lock,
            save_download,
            test_alert,
            send_report,
            restart_as_admin,
            restart_app,
            update_status,
            update_check,
            pickers::pick_program,
            pickers::read_shortcut,
            pickers::find_miracle
        ])
        .build(tauri::generate_context!())
        .expect("starting Access Portal")
        .run(|app, event| {
            // The portal keeps running in the tray when the window is destroyed
            // (only Quit, which gives an exit code, ends the app).
            if let RunEvent::ExitRequested { api, code: None, .. } = &event {
                api.prevent_exit();
            }
            // Quitting stops the tunnel program too (it is a separate process).
            if let RunEvent::Exit = event {
                if let Some(t) = app.state::<Option<tunnel::Tunnel>>().inner() {
                    t.stop();
                }
            }
        });
}
