//! access-portal as a library: the Access Portal app (`admin-app/`) runs the
//! whole portal inside itself with [`start`].
//!
//! The portal serves its user-facing pages (sign-in, the app list, sessions,
//! file transfer) on the network. The admin routes are never put on the network
//! listener; the app reaches them in-process through [`Portal::admin`], so the
//! admin panel only exists on the host.

mod api;
mod auth;
mod config;
mod db;
mod error;
mod host;
mod models;
mod presence;
mod rdp;
mod tiles;
mod transport;
mod video;
mod web;

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

/// Start the portal with everything kept in `data_dir`: the settings file
/// (`portal.env`, created on first start with a fresh secret), the database
/// (`portal.db`) and the per-user transfer folders (`files/`).
///
/// Must be called inside a Tokio runtime; the server keeps running on it.
pub async fn start(data_dir: &Path) -> anyhow::Result<Portal> {
    std::fs::create_dir_all(data_dir)
        .with_context(|| format!("creating {}", data_dir.display()))?;
    restrict_access(data_dir);
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

    let address = config.bind_addr.clone();
    let state = AppState {
        pool,
        config: Arc::new(config),
        limiter: Arc::new(web::LoginLimiter::new()),
        presence: Arc::default(),
    };

    // Forget expired sign-ins once an hour.
    let pool = state.pool.clone();
    tokio::spawn(async move {
        let mut hourly = tokio::time::interval(std::time::Duration::from_secs(60 * 60));
        loop {
            hourly.tick().await;
            if let Err(e) = db::delete_expired(&pool).await {
                tracing::warn!("clearing expired sign-ins: {e:#}");
            }
        }
    });

    let listener = tokio::net::TcpListener::bind(&address)
        .await
        .with_context(|| format!("listening on {address} (is another copy of the portal running?)"))?;
    let public = web::router(state.clone()).layer(TraceLayer::new_for_http());
    tokio::spawn(async move {
        if let Err(e) = axum::serve(listener, public).await {
            tracing::error!("the portal stopped serving: {e:#}");
        }
    });
    tracing::info!("access-portal listening on http://{address}");

    Ok(Portal {
        admin: api::router().with_state(state),
        address,
        data_dir: data_dir.to_path_buf(),
    })
}

impl Portal {
    /// One admin API call, made in-process. Answers `(status, json)`.
    pub async fn admin(&self, method: &str, path: &str, body: Option<Value>) -> (u16, Value) {
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
