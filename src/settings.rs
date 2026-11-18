//! Settings changed from the app (the `settings` table), as opposed to the ones
//! in `portal.env` that need a restart.

use anyhow::Result;
use serde::{Deserialize, Serialize};
use sqlx::SqlitePool;

/// The admin sign-in until it is changed.
pub const DEFAULT_ADMIN: (&str, &str) = ("admin", "admin");

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Sessions {
    /// Disconnect a browser session after this many minutes without keyboard or
    /// mouse input (the apps keep running). 0: never.
    pub idle_minutes: u32,
    /// Sign a Windows account out after its session has been disconnected this
    /// many hours (its apps close). 0: never.
    pub signout_hours: u32,
    /// Ask the user before an administrator views or controls their screen.
    pub shadow_consent: bool,
}

impl Default for Sessions {
    fn default() -> Self {
        // Disconnecting an idle browser loses nothing (the apps keep running).
        // A Windows session left disconnected for 8 hours is signed out, because
        // a forgotten session holds 0.3-1.5 GB of memory; black-screen programs
        // are only asked to close (one with unsaved work stays open). Viewing a
        // screen asks the user first unless the admin turns that off.
        Self { idle_minutes: 60, signout_hours: 8, shadow_consent: false }
    }
}

pub async fn get(pool: &SqlitePool, key: &str) -> Result<Option<String>> {
    let row: Option<(String,)> = sqlx::query_as("SELECT value FROM settings WHERE key = ?")
        .bind(key)
        .fetch_optional(pool)
        .await?;
    Ok(row.map(|r| r.0))
}

pub async fn set(pool: &SqlitePool, key: &str, value: &str) -> Result<()> {
    sqlx::query("INSERT INTO settings (key, value) VALUES (?, ?) ON CONFLICT(key) DO UPDATE SET value = excluded.value")
        .bind(key)
        .bind(value)
        .execute(pool)
        .await?;
    Ok(())
}

pub async fn sessions(pool: &SqlitePool) -> Sessions {
    match get(pool, "sessions").await {
        Ok(Some(json)) => serde_json::from_str(&json).unwrap_or_default(),
        _ => Sessions::default(),
    }
}

pub async fn set_sessions(pool: &SqlitePool, value: &Sessions) -> Result<()> {
    set(pool, "sessions", &serde_json::to_string(value)?).await
}

async fn get_json<T: serde::de::DeserializeOwned + Default>(pool: &SqlitePool, key: &str) -> T {
    match get(pool, key).await {
        Ok(Some(json)) => serde_json::from_str(&json).unwrap_or_default(),
        _ => T::default(),
    }
}

pub async fn backups(pool: &SqlitePool) -> crate::backup::Settings {
    get_json(pool, "backups").await
}

pub async fn set_backups(pool: &SqlitePool, value: &crate::backup::Settings) -> Result<()> {
    set(pool, "backups", &serde_json::to_string(value)?).await
}

/// Install new versions of the app by themselves (when nobody is connected).
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Updates {
    pub auto: bool,
}

impl Default for Updates {
    fn default() -> Self {
        Self { auto: true }
    }
}

pub async fn updates(pool: &SqlitePool) -> Updates {
    get_json(pool, "updates").await
}

pub async fn set_updates(pool: &SqlitePool, value: &Updates) -> Result<()> {
    set(pool, "updates", &serde_json::to_string(value)?).await
}

/// Where browsers may reach the portal besides its public link (see `network.rs`).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Reach {
    /// From the local network (private addresses such as 192.168.x.x).
    pub lan: bool,
    /// From devices on the computer's Tailscale network (100.64.0.0/10).
    pub tailscale: bool,
}

impl Default for Reach {
    fn default() -> Self {
        Self { lan: true, tailscale: true }
    }
}

pub async fn reach(pool: &SqlitePool) -> Reach {
    get_json(pool, "reach").await
}

pub async fn set_reach(pool: &SqlitePool, value: &Reach) -> Result<()> {
    set(pool, "reach", &serde_json::to_string(value)?).await
}

/// Phone alerts: sent to this topic on an ntfy server when set.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct Alerts {
    #[serde(default)]
    pub topic: Option<String>,
    /// The ntfy server (default `DEFAULT_NTFY`).
    #[serde(default)]
    pub server: Option<String>,
}

/// A public ntfy server that also accepts the offline alerts sent from
/// Cloudflare (ntfy.sh limits messages per address, and Cloudflare's are shared).
pub const DEFAULT_NTFY: &str = "https://ntfy.envs.net";

impl Alerts {
    pub fn server(&self) -> String {
        self.server.clone().filter(|s| !s.trim().is_empty()).unwrap_or_else(|| DEFAULT_NTFY.to_string())
    }
}

pub async fn alerts(pool: &SqlitePool) -> Alerts {
    get_json(pool, "alerts").await
}

pub async fn set_alerts(pool: &SqlitePool, value: &Alerts) -> Result<()> {
    set(pool, "alerts", &serde_json::to_string(value)?).await
}

/// The admin username, and whether the password is still the default one.
pub async fn admin_account(pool: &SqlitePool) -> Result<(String, bool)> {
    let name = get(pool, "admin_username").await?.unwrap_or_else(|| DEFAULT_ADMIN.0.to_string());
    let default_password = match get(pool, "admin_password").await? {
        None => true,
        Some(hash) => crate::auth::verify_password(DEFAULT_ADMIN.1, &hash),
    };
    Ok((name, default_password))
}

pub async fn check_admin(pool: &SqlitePool, username: &str, password: &str) -> Result<bool> {
    let name = get(pool, "admin_username").await?.unwrap_or_else(|| DEFAULT_ADMIN.0.to_string());
    if !name.eq_ignore_ascii_case(username.trim()) {
        return Ok(false);
    }
    Ok(match get(pool, "admin_password").await? {
        None => password == DEFAULT_ADMIN.1,
        Some(hash) => crate::auth::verify_password(password, &hash),
    })
}

pub async fn set_admin(pool: &SqlitePool, username: &str, password: &str) -> Result<()> {
    set(pool, "admin_username", username.trim()).await?;
    set(pool, "admin_password", &crate::auth::hash_password(password)?).await
}
