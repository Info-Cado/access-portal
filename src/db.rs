//! All SQL lives here. Runtime queries (no compile-time DB needed).

use anyhow::Result;
use sqlx::SqlitePool;

use crate::auth;
use crate::config::Config;
use crate::models::{App, User};

pub fn now() -> i64 {
    use std::time::{SystemTime, UNIX_EPOCH};
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

// ---- users ----

pub async fn user_by_username(pool: &SqlitePool, username: &str) -> Result<Option<User>> {
    let u = sqlx::query_as::<_, User>("SELECT * FROM users WHERE username = ?")
        .bind(username)
        .fetch_optional(pool)
        .await?;
    Ok(u)
}

pub async fn user_by_id(pool: &SqlitePool, id: i64) -> Result<Option<User>> {
    let u = sqlx::query_as::<_, User>("SELECT * FROM users WHERE id = ?")
        .bind(id)
        .fetch_optional(pool)
        .await?;
    Ok(u)
}

pub async fn list_users(pool: &SqlitePool) -> Result<Vec<User>> {
    let v = sqlx::query_as::<_, User>("SELECT * FROM users ORDER BY username")
        .fetch_all(pool)
        .await?;
    Ok(v)
}

#[allow(clippy::too_many_arguments)]
pub async fn create_user(
    pool: &SqlitePool,
    config: &Config,
    username: &str,
    portal_password: &str,
    win_username: &str,
    win_password_plain: &str,
    is_admin: bool,
    enable_totp: bool,
) -> Result<(i64, Option<String>)> {
    let hash = auth::hash_password(portal_password)?;
    let enc_win = auth::encrypt_at_rest(&config.portal_secret, win_password_plain)?;
    let totp_secret = if enable_totp {
        Some(auth::new_totp_secret())
    } else {
        None
    };

    let res = sqlx::query(
        "INSERT INTO users (username, password_hash, is_admin, totp_secret, win_username, win_password, created_at)
         VALUES (?, ?, ?, ?, ?, ?, ?)",
    )
    .bind(username)
    .bind(&hash)
    .bind(is_admin as i64)
    .bind(&totp_secret)
    .bind(win_username)
    .bind(&enc_win)
    .bind(now())
    .execute(pool)
    .await?;

    Ok((res.last_insert_rowid(), totp_secret))
}

/// Change which Windows account a portal user maps to (password stored encrypted).
pub async fn set_windows_login(
    pool: &SqlitePool,
    config: &Config,
    user_id: i64,
    win_username: &str,
    win_password_plain: &str,
) -> Result<()> {
    let enc = auth::encrypt_at_rest(&config.portal_secret, win_password_plain)?;
    sqlx::query("UPDATE users SET win_username = ?, win_password = ? WHERE id = ?")
        .bind(win_username)
        .bind(&enc)
        .bind(user_id)
        .execute(pool)
        .await?;
    Ok(())
}

/// Set a new portal password; optionally end the user's open portal logins.
pub async fn set_password(
    pool: &SqlitePool,
    user_id: i64,
    password: &str,
    end_sessions: bool,
) -> Result<()> {
    let hash = auth::hash_password(password)?;
    sqlx::query("UPDATE users SET password_hash = ?, failed_count = 0, locked_until = NULL WHERE id = ?")
        .bind(&hash)
        .bind(user_id)
        .execute(pool)
        .await?;
    if end_sessions {
        sqlx::query("DELETE FROM sessions WHERE user_id = ?")
            .bind(user_id)
            .execute(pool)
            .await?;
    }
    Ok(())
}

/// When and from where a user may sign in (JSON), or `None` for no limits.
pub async fn set_access_rules(pool: &SqlitePool, user_id: i64, rules: Option<&str>) -> Result<()> {
    sqlx::query("UPDATE users SET access_rules = ? WHERE id = ?")
        .bind(rules)
        .bind(user_id)
        .execute(pool)
        .await?;
    Ok(())
}

/// Turn two-step verification on (with this secret) or off (`None`).
pub async fn set_totp_secret(pool: &SqlitePool, user_id: i64, secret: Option<&str>) -> Result<()> {
    sqlx::query("UPDATE users SET totp_secret = ? WHERE id = ?")
        .bind(secret)
        .bind(user_id)
        .execute(pool)
        .await?;
    Ok(())
}

pub async fn delete_user(pool: &SqlitePool, id: i64) -> Result<()> {
    sqlx::query("DELETE FROM users WHERE id = ?")
        .bind(id)
        .execute(pool)
        .await?;
    Ok(())
}

pub async fn note_failed_login(pool: &SqlitePool, config: &Config, user_id: i64) -> Result<()> {
    // Increment, and if at/over the threshold set a lockout window.
    sqlx::query("UPDATE users SET failed_count = failed_count + 1 WHERE id = ?")
        .bind(user_id)
        .execute(pool)
        .await?;
    sqlx::query(
        "UPDATE users SET locked_until = ? WHERE id = ? AND failed_count >= ?",
    )
    .bind(now() + config.lockout_secs)
    .bind(user_id)
    .bind(config.max_failed)
    .execute(pool)
    .await?;
    Ok(())
}

pub async fn reset_failed_login(pool: &SqlitePool, user_id: i64) -> Result<()> {
    sqlx::query("UPDATE users SET failed_count = 0, locked_until = NULL WHERE id = ?")
        .bind(user_id)
        .execute(pool)
        .await?;
    Ok(())
}

/// Block a user until a unix time, or clear the block with `None`. Separate from
/// the automatic lockout after wrong passwords (`locked_until`).
pub async fn set_block(pool: &SqlitePool, user_id: i64, until: Option<i64>) -> Result<()> {
    sqlx::query("UPDATE users SET blocked_until = ? WHERE id = ?")
        .bind(until)
        .bind(user_id)
        .execute(pool)
        .await?;
    Ok(())
}

// ---- apps ----

pub async fn list_apps(pool: &SqlitePool) -> Result<Vec<App>> {
    let v = sqlx::query_as::<_, App>("SELECT * FROM apps ORDER BY name")
        .fetch_all(pool)
        .await?;
    Ok(v)
}

pub async fn app_by_key(pool: &SqlitePool, key: &str) -> Result<Option<App>> {
    let a = sqlx::query_as::<_, App>("SELECT * FROM apps WHERE key = ?")
        .bind(key)
        .fetch_optional(pool)
        .await?;
    Ok(a)
}

pub async fn create_app(
    pool: &SqlitePool,
    key: &str,
    name: &str,
    hostname: &str,
    port: &str,
) -> Result<i64> {
    let res = sqlx::query(
        "INSERT INTO apps (key, name, hostname, port, created_at) VALUES (?, ?, ?, ?, ?)",
    )
    .bind(key)
    .bind(name)
    .bind(hostname)
    .bind(port)
    .bind(now())
    .execute(pool)
    .await?;
    Ok(res.last_insert_rowid())
}

pub async fn delete_app(pool: &SqlitePool, id: i64) -> Result<()> {
    sqlx::query("DELETE FROM apps WHERE id = ?")
        .bind(id)
        .execute(pool)
        .await?;
    Ok(())
}

// ---- permissions ----

pub async fn apps_for_user(pool: &SqlitePool, user_id: i64) -> Result<Vec<App>> {
    let v = sqlx::query_as::<_, App>(
        "SELECT a.* FROM apps a
         JOIN user_apps ua ON ua.app_id = a.id
         WHERE ua.user_id = ?
         ORDER BY a.name",
    )
    .bind(user_id)
    .fetch_all(pool)
    .await?;
    Ok(v)
}

pub async fn user_can_access(pool: &SqlitePool, user_id: i64, app_id: i64) -> Result<bool> {
    let row: Option<(i64,)> =
        sqlx::query_as("SELECT 1 FROM user_apps WHERE user_id = ? AND app_id = ?")
            .bind(user_id)
            .bind(app_id)
            .fetch_optional(pool)
            .await?;
    Ok(row.is_some())
}

pub async fn set_user_apps(pool: &SqlitePool, user_id: i64, app_ids: &[i64]) -> Result<()> {
    let mut tx = pool.begin().await?;
    sqlx::query("DELETE FROM user_apps WHERE user_id = ?")
        .bind(user_id)
        .execute(&mut *tx)
        .await?;
    for app_id in app_ids {
        sqlx::query("INSERT INTO user_apps (user_id, app_id) VALUES (?, ?)")
            .bind(user_id)
            .bind(app_id)
            .execute(&mut *tx)
            .await?;
    }
    tx.commit().await?;
    Ok(())
}

// ---- sessions ----

pub async fn create_session(pool: &SqlitePool, config: &Config, user_id: i64) -> Result<String> {
    let token = auth::random_token();
    sqlx::query(
        "INSERT INTO sessions (token, user_id, expires_at, created_at) VALUES (?, ?, ?, ?)",
    )
    .bind(&token)
    .bind(user_id)
    .bind(now() + config.session_ttl_secs)
    .bind(now())
    .execute(pool)
    .await?;
    Ok(token)
}

pub async fn user_by_session(pool: &SqlitePool, token: &str) -> Result<Option<User>> {
    let u = sqlx::query_as::<_, User>(
        "SELECT u.* FROM users u
         JOIN sessions s ON s.user_id = u.id
         WHERE s.token = ? AND s.expires_at > ?",
    )
    .bind(token)
    .bind(now())
    .fetch_optional(pool)
    .await?;
    Ok(u)
}

pub async fn delete_session(pool: &SqlitePool, token: &str) -> Result<()> {
    sqlx::query("DELETE FROM sessions WHERE token = ?")
        .bind(token)
        .execute(pool)
        .await?;
    Ok(())
}

// ---- pending 2FA logins ----

pub async fn create_pending(pool: &SqlitePool, user_id: i64) -> Result<String> {
    let token = auth::random_token();
    sqlx::query("INSERT INTO pending_logins (token, user_id, expires_at) VALUES (?, ?, ?)")
        .bind(&token)
        .bind(user_id)
        .bind(now() + 300) // 5 minutes to enter the code
        .execute(pool)
        .await?;
    Ok(token)
}

/// Look up the pending login without consuming it (so a wrong code can be retried).
pub async fn peek_pending(pool: &SqlitePool, token: &str) -> Result<Option<i64>> {
    let row: Option<(i64,)> = sqlx::query_as(
        "SELECT user_id FROM pending_logins WHERE token = ? AND expires_at > ?",
    )
    .bind(token)
    .bind(now())
    .fetch_optional(pool)
    .await?;
    Ok(row.map(|r| r.0))
}

pub async fn delete_pending(pool: &SqlitePool, token: &str) -> Result<()> {
    sqlx::query("DELETE FROM pending_logins WHERE token = ?")
        .bind(token)
        .execute(pool)
        .await?;
    Ok(())
}

/// Remove sign-in sessions and half-finished 2FA logins that have expired.
pub async fn delete_expired(pool: &SqlitePool) -> Result<()> {
    sqlx::query("DELETE FROM sessions WHERE expires_at <= ?").bind(now()).execute(pool).await?;
    sqlx::query("DELETE FROM pending_logins WHERE expires_at <= ?").bind(now()).execute(pool).await?;
    Ok(())
}

/// End every portal sign-in of a user.
pub async fn delete_sessions_for_user(pool: &SqlitePool, user_id: i64) -> Result<()> {
    sqlx::query("DELETE FROM sessions WHERE user_id = ?").bind(user_id).execute(pool).await?;
    Ok(())
}
