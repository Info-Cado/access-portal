//! The activity log: sign-ins, sessions, print jobs, transfers and admin
//! actions, shown on the app's Activity screen. Kept for `KEEP_DAYS`.
//!
//! Writing an entry never fails the action it describes: errors are logged.

use serde::Serialize;
use sqlx::SqlitePool;

pub const KEEP_DAYS: i64 = 180;

/// Who did something, and from where; cheap to clone into a session.
#[derive(Clone)]
pub struct Actor {
    pub pool: SqlitePool,
    pub user_id: Option<i64>,
    pub username: String,
    pub address: String,
}

impl Actor {
    pub fn new(pool: &SqlitePool, user: &crate::models::User, address: &str) -> Self {
        Self { pool: pool.clone(), user_id: Some(user.id), username: user.username.clone(), address: address.to_string() }
    }

    pub async fn log(&self, kind: &str, detail: &str) {
        log(&self.pool, self.user_id, &self.username, kind, detail, &self.address).await;
    }

    /// For places that can't wait (synchronous code, or mid-stream).
    pub fn spawn(&self, kind: &str, detail: impl Into<String>) {
        let me = self.clone();
        let (kind, detail) = (kind.to_string(), detail.into());
        tokio::spawn(async move { me.log(&kind, &detail).await });
    }
}

pub async fn log(pool: &SqlitePool, user_id: Option<i64>, username: &str, kind: &str, detail: &str, address: &str) {
    let result = sqlx::query("INSERT INTO activity (at, user_id, username, kind, detail, address) VALUES (?, ?, ?, ?, ?, ?)")
        .bind(crate::db::now())
        .bind(user_id)
        .bind(username)
        .bind(kind)
        .bind(detail)
        .bind(address)
        .execute(pool)
        .await;
    if let Err(e) = result {
        tracing::warn!("writing the activity log: {e:#}");
    }
}

/// An administrator's action on a user (or on the portal, without one).
pub async fn admin(pool: &SqlitePool, user: Option<&crate::models::User>, detail: &str) {
    log(pool, user.map(|u| u.id), user.map(|u| u.username.as_str()).unwrap_or(""), "admin", detail, "this computer").await;
}

#[derive(Serialize, sqlx::FromRow)]
pub struct Entry {
    pub id: i64,
    pub at: i64,
    pub user_id: Option<i64>,
    pub username: String,
    pub kind: String,
    pub detail: String,
    pub address: String,
}

/// Newest first; `before` pages back from an entry id.
pub async fn list(pool: &SqlitePool, user_id: Option<i64>, before: Option<i64>, limit: i64) -> anyhow::Result<Vec<Entry>> {
    let rows = sqlx::query_as::<_, Entry>(
        "SELECT id, at, user_id, username, kind, detail, address FROM activity
         WHERE (?1 IS NULL OR user_id = ?1) AND (?2 IS NULL OR id < ?2)
         ORDER BY id DESC LIMIT ?3",
    )
    .bind(user_id)
    .bind(before)
    .bind(limit.clamp(1, 5000))
    .fetch_all(pool)
    .await?;
    Ok(rows)
}

pub async fn delete_old(pool: &SqlitePool) -> anyhow::Result<()> {
    sqlx::query("DELETE FROM activity WHERE at < ?")
        .bind(crate::db::now() - KEEP_DAYS * 86_400)
        .execute(pool)
        .await?;
    Ok(())
}

/// "2 h 5 min", "4 min", "40 s".
pub fn duration(secs: i64) -> String {
    let secs = secs.max(0);
    if secs < 60 {
        return format!("{secs} s");
    }
    let m = secs / 60;
    if m < 60 {
        return format!("{m} min");
    }
    if m % 60 == 0 {
        format!("{} h", m / 60)
    } else {
        format!("{} h {} min", m / 60, m % 60)
    }
}
