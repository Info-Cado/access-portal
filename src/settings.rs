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
        // Disconnecting an idle browser loses nothing (the apps keep running);
        // signing out closes apps, so that one is off until chosen. Viewing a
        // screen asks the user first unless the admin turns that off.
        Self { idle_minutes: 60, signout_hours: 0, shadow_consent: true }
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
