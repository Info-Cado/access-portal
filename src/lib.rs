//! access-portal as a library: the Access Portal app (`admin-app/`) runs the
//! whole portal inside itself with [`start`].
//!
//! The portal serves its user-facing pages (sign-in, the app list, sessions,
//! file transfer) on the network. The admin routes are never put on the network
//! listener; the app reaches them in-process through [`Portal::admin`], so the
//! admin panel only exists on the host.

mod access;
mod activity;
mod api;
mod auth;
mod backup;
mod clipimage;
mod clock;
mod config;
mod db;
mod devices;
mod gfx;
mod error;
mod host;
mod models;
mod network;
mod presence;
mod print;
pub mod progress;
mod rdp;
mod settings;
mod sound;
mod tiles;
mod transport;
mod video;
mod web;
#[cfg(windows)]
mod win;

use std::path::{Path, PathBuf};
use std::str::FromStr;
use std::sync::Arc;

use anyhow::Context;
use axum::body::Body;
use axum::http::{header, Method, Request};
use axum::Router;
use serde_json::{json, Value};
use sqlx::sqlite::{SqliteConnectOptions, SqlitePoolOptions};
use tower::ServiceExt as _;
use tower_http::trace::TraceLayer;

/// Shared state handed to every handler.
#[derive(Clone)]
pub struct AppState {
    pub pool: sqlx::SqlitePool,
    pub config: Arc<config::Config>,
    /// Failed sign-ins per network address.
    pub limiter: Arc<web::LoginLimiter>,
    /// Sessions open right now.
    pub presence: Arc<presence::Presence>,
    /// The folder holding the database, settings and backups.
    pub data_dir: PathBuf,
    /// Recent problems, for the app's alerts.
    pub health: Arc<Health>,
    /// Where browsers can reach the portal (local network, Tailscale).
    pub network: Arc<network::Network>,
}

/// Sessions that could not be opened recently (unix time, why).
#[derive(Default)]
pub struct Health {
    failures: std::sync::Mutex<std::collections::VecDeque<(i64, String)>>,
}

impl Health {
    pub fn session_failed(&self, why: &str) {
        let mut f = self.failures.lock().unwrap();
        f.push_back((db::now(), why.to_string()));
        while f.len() > 50 {
            f.pop_front();
        }
    }

    /// How many sessions failed in the last `secs` seconds, and the latest reason.
    pub fn recent_failures(&self, secs: i64) -> (usize, Option<String>) {
        let f = self.failures.lock().unwrap();
        let since = db::now() - secs;
        let recent: Vec<_> = f.iter().filter(|(t, _)| *t >= since).collect();
        (recent.len(), recent.last().map(|(_, why)| why.clone()))
    }
}

/// A running portal.
pub struct Portal {
    admin: Router,
    /// Where the portal accepts connections, e.g. `127.0.0.1:8088`.
    pub address: String,
    /// The folder holding the settings file, database and transfer folders.
    pub data_dir: PathBuf,
}

/// Name of the settings file inside the data folder.
pub const SETTINGS_FILE: &str = "portal.env";

/// What this computer looks like, for a problem report: the setup checks
/// (`checks`) and facts about Windows, Remote Desktop and antivirus (`system`).
/// A part that can't be read holds `{"error": "..."}`. Blocks for a few seconds.
pub fn diagnostics_blocking() -> serde_json::Value {
    let checks = host::setup_blocking(&[]).map(serde_json::Value::from);
    let system = host::system_info_blocking();
    let part = |r: anyhow::Result<serde_json::Value>| r.unwrap_or_else(|e| serde_json::json!({ "error": format!("{e:#}") }));
    serde_json::json!({ "checks": part(checks), "system": part(system) })
}

/// Start the portal with everything kept in `data_dir`: the settings file
/// (`portal.env`, created on first start with a fresh secret), the database
/// (`portal.db`) and the per-user transfer folders (`files/`).
///
/// Must be called inside a Tokio runtime; the server keeps running on it.
pub async fn start(data_dir: &Path) -> anyhow::Result<Portal> {
    std::fs::create_dir_all(data_dir)
        .with_context(|| format!("creating {}", data_dir.display()))?;
    restrict_access(data_dir);
    // A backup chosen in the app is put in place before anything is opened.
    if let Err(e) = backup::restore_if_requested(data_dir) {
        tracing::error!("restoring a backup: {e:#}");
    }
    load_settings(data_dir)?;

    let config = config::Config::from_env()?;

    // Enforce foreign keys on every connection (SQLite has them off by default).
    let connect_opts = SqliteConnectOptions::from_str(&config.database_url)
        .with_context(|| format!("reading the database setting {}", config.database_url))?
        .create_if_missing(true)
        .foreign_keys(true);
    let pool = SqlitePoolOptions::new()
        .max_connections(5)
        .connect_with(connect_opts)
        .await
        .with_context(|| format!("opening the database {}", config.database_url))?;
    sqlx::migrate!("./migrations")
        .run(&pool)
        .await
        .context("updating the database")?;

    // Windows-side settings for smooth sessions (GPU encoding, 60 fps).
    tokio::task::spawn_blocking(host::tune_remote_desktop);

    // Administrators keep everyday access to the apps' data folders.
    match db::list_apps(&pool).await {
        Ok(apps) => {
            let dirs = apps
                .into_iter()
                .filter(|a| a.program.as_deref().is_some_and(|p| !p.trim().is_empty()))
                .filter_map(|a| a.work_dir.filter(|d| !d.trim().is_empty()))
                .collect();
            tokio::task::spawn_blocking(move || host::keep_admin_folder_access(dirs));
        }
        Err(e) => tracing::warn!("listing the apps' data folders: {e:#}"),
    }

    let address = config.bind_addr.clone();
    let network = network::Network::new(address.clone(), settings::reach(&pool).await, data_dir.to_path_buf());
    let state = AppState {
        pool,
        config: Arc::new(config),
        limiter: Arc::new(web::LoginLimiter::new()),
        presence: Arc::default(),
        data_dir: data_dir.to_path_buf(),
        health: Arc::default(),
        network: network.clone(),
    };

    // A backup a day (checked hourly, so a computer that was off catches up).
    let backups = state.clone();
    tokio::spawn(async move {
        let mut hourly = tokio::time::interval(std::time::Duration::from_secs(60 * 60));
        loop {
            hourly.tick().await;
            let newest = backup::list(&backups.data_dir).first().map(|b| b.at).unwrap_or(0);
            if db::now() - newest >= 23 * 3600 {
                let settings = settings::backups(&backups.pool).await;
                if let Err(e) = backup::make(&backups.pool, &backups.data_dir, &settings).await {
                    tracing::warn!("daily backup: {e:#}");
                }
            }
        }
    });

    // Forget expired sign-ins and old activity once an hour.
    let pool = state.pool.clone();
    tokio::spawn(async move {
        let mut hourly = tokio::time::interval(std::time::Duration::from_secs(60 * 60));
        loop {
            hourly.tick().await;
            if let Err(e) = db::delete_expired(&pool).await {
                tracing::warn!("clearing expired sign-ins: {e:#}");
            }
            if let Err(e) = activity::delete_old(&pool).await {
                tracing::warn!("clearing old activity: {e:#}");
            }
        }
    });

    // Windows' screen-viewing rule follows the app's setting.
    let ask = settings::sessions(&state.pool).await.shadow_consent;
    tokio::task::spawn_blocking(move || host::set_shadow_policy(ask));

    // Sign out Windows sessions left disconnected too long (if chosen).
    let watcher = state.clone();
    tokio::spawn(async move {
        let mut every = tokio::time::interval(std::time::Duration::from_secs(5 * 60));
        loop {
            every.tick().await;
            sign_out_disconnected(&watcher).await;
        }
    });

    let public = web::router(state.clone()).layer(TraceLayer::new_for_http());
    network.start(public).await?;

    Ok(Portal {
        admin: api::router().with_state(state),
        address,
        data_dir: data_dir.to_path_buf(),
    })
}

/// Black-screen sessions already asked to close (logged once, asked every check).
static ASKED_TO_CLOSE: std::sync::Mutex<std::collections::BTreeSet<u32>> =
    std::sync::Mutex::new(std::collections::BTreeSet::new());

/// Sign out the Windows sessions of portal users that have been disconnected
/// longer than the app's setting allows. Never touches administrator accounts
/// or accounts that aren't a portal user's.
async fn sign_out_disconnected(state: &AppState) {
    let hours = settings::sessions(&state.pool).await.signout_hours;
    if hours == 0 {
        return;
    }
    let Ok(users) = db::list_users(&state.pool).await else { return };
    let sessions = tokio::task::spawn_blocking(host::windows_sessions_blocking).await.unwrap_or_default();
    let kiosk = tokio::task::spawn_blocking(host::kiosk_sessions_blocking).await.unwrap_or_default();
    let limit = u64::from(hours) * 3600;
    let mut ids = Vec::new();
    for s in sessions {
        if s.state != host::WTS_DISCONNECTED || s.admin || s.disconnected_secs < limit {
            continue;
        }
        let Some(user) = users.iter().find(|u| u.win_username.eq_ignore_ascii_case(&s.user)) else { continue };
        if kiosk.contains(&s.id) {
            // A black-screen program: ask it to close safely and never force.
            // If it has unsaved work it stays open; the next check asks again.
            let first = ASKED_TO_CLOSE.lock().unwrap().insert(s.id);
            if let Err(e) = host::request_close(&s.user) {
                tracing::warn!("could not ask {}'s program to close: {e}", s.user);
            } else if first {
                activity::log(
                    &state.pool,
                    Some(user.id),
                    &user.username,
                    "auto_signout",
                    &format!(
                        "Asked their app to close after {} disconnected (it stays open if it has unsaved work)",
                        activity::duration(s.disconnected_secs as i64)
                    ),
                    "this computer",
                )
                .await;
            }
            continue;
        }
        ids.push(s.id);
        activity::log(
            &state.pool,
            Some(user.id),
            &user.username,
            "auto_signout",
            &format!("Signed out of Windows after {} disconnected", activity::duration(s.disconnected_secs as i64)),
            "this computer",
        )
        .await;
    }
    if !ids.is_empty() {
        let n = tokio::task::spawn_blocking(move || host::log_off_blocking(&ids)).await.unwrap_or(0);
        tracing::info!("signed out {n} Windows session(s) left disconnected over {hours} h");
    }
}

impl Portal {
    /// One admin API call, made in-process. Answers `(status, json)`.
    pub async fn admin(&self, method: &str, path: &str, body: Option<Value>) -> (u16, Value) {
        self.admin_inner(method, path, body).await
    }

    /// [`Portal::admin`], with `sink` receiving the steps of a long action
    /// ("Closing Miracle…") while it runs, for the app to show.
    pub async fn admin_with_progress(
        &self,
        method: &str,
        path: &str,
        body: Option<Value>,
        sink: progress::Sink,
    ) -> (u16, Value) {
        progress::scope(sink, self.admin_inner(method, path, body)).await
    }

    async fn admin_inner(&self, method: &str, path: &str, body: Option<Value>) -> (u16, Value) {
        let Ok(method) = Method::from_str(method) else {
            return (400, json!({ "error": "Unknown request method." }));
        };
        let mut request = Request::builder().method(method).uri(path);
        let body = match body {
            Some(value) => {
                request = request.header(header::CONTENT_TYPE, "application/json");
                Body::from(value.to_string())
            }
            None => Body::empty(),
        };
        let Ok(mut request) = request.body(body) else {
            return (400, json!({ "error": "Bad request." }));
        };
        request.extensions_mut().insert(api::LocalAdmin);

        let response = match self.admin.clone().oneshot(request).await {
            Ok(r) => r,
            Err(never) => match never {},
        };
        let status = response.status().as_u16();
        let bytes = axum::body::to_bytes(response.into_body(), 16 * 1024 * 1024)
            .await
            .unwrap_or_default();
        let value = serde_json::from_slice(&bytes)
            .unwrap_or_else(|_| json!({ "error": String::from_utf8_lossy(&bytes).trim().to_string() }));
        (status, value)
    }
}

/// Only administrators, SYSTEM and the account running the portal may open the
/// data folder. It holds the secret that decrypts every stored Windows password,
/// and portal users have Windows accounts on this same machine.
fn restrict_access(data_dir: &Path) {
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        let me = match (std::env::var("USERDOMAIN"), std::env::var("USERNAME")) {
            (Ok(d), Ok(u)) => format!("{d}\\{u}:(OI)(CI)F"),
            _ => String::new(),
        };
        let mut args = vec![
            data_dir.display().to_string(),
            "/inheritance:r".into(),
            "/grant:r".into(),
            "*S-1-5-32-544:(OI)(CI)F".into(), // Administrators
            "*S-1-5-18:(OI)(CI)F".into(),     // SYSTEM
        ];
        if !me.is_empty() {
            args.push(me);
        }
        match std::process::Command::new("icacls").args(&args).creation_flags(0x0800_0000).output() {
            Ok(out) if out.status.success() => {}
            Ok(out) => tracing::warn!("restricting {}: {}", data_dir.display(), String::from_utf8_lossy(&out.stdout).trim()),
            Err(e) => tracing::warn!("restricting {}: {e}", data_dir.display()),
        }
    }
}

/// Read `portal.env` into the process environment (variables already set win),
/// creating it with defaults on first start.
fn load_settings(data_dir: &Path) -> anyhow::Result<()> {
    let file = data_dir.join(SETTINGS_FILE);
    if !file.exists() {
        std::fs::write(&file, default_settings())
            .with_context(|| format!("writing {}", file.display()))?;
    }
    let text = std::fs::read_to_string(&file).with_context(|| format!("reading {}", file.display()))?;
    for line in text.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        if let Some((key, value)) = line.split_once('=') {
            let key = key.trim();
            if std::env::var_os(key).is_none() {
                std::env::set_var(key, value.trim().trim_matches('"'));
            }
        }
    }

    // Paths default to the data folder, not wherever the app was started from.
    let db = data_dir.join("portal.db");
    if std::env::var_os("DATABASE_URL").is_none() {
        let url = format!("sqlite://{}?mode=rwc", db.display().to_string().replace('\\', "/"));
        std::env::set_var("DATABASE_URL", url);
    }
    if std::env::var_os("FILES_DIR").is_none() {
        std::env::set_var("FILES_DIR", data_dir.join("files"));
    }
    Ok(())
}

fn default_settings() -> String {
    format!(
        "# Access Portal settings. Restart the app after changing them.\n\
         \n\
         # Encrypts the stored Windows passwords. Keep it secret and back it up:\n\
         # if it is lost or changed, every Windows password must be entered again.\n\
         PORTAL_SECRET={}\n\
         \n\
         # Where users' browsers reach the portal. Put HTTPS (a reverse proxy, or\n\
         # `tailscale serve`) in front of it to reach it from other machines.\n\
         BIND_ADDR=127.0.0.1:8088\n\
         \n\
         # Set to true once the portal is served over HTTPS.\n\
         COOKIE_SECURE=false\n\
         \n\
         # More settings (session size, video, peer-to-peer, TURN, upload size,\n\
         # lockout) are listed in the project's .env.example.\n",
        auth::random_token()
    )
}
