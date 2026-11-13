//! Who is connected right now: one entry per open session, added when the
//! session starts and removed when it ends (by a guard, so a session that ends
//! any way at all is removed).

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use axum::http::HeaderMap;
use serde::Serialize;

#[derive(Clone, Serialize)]
pub struct Connection {
    pub user_id: i64,
    /// The app's display name.
    pub app: String,
    /// Unix time the session started.
    pub since: i64,
    /// The browser's network address, as reported by the HTTPS front end.
    pub address: String,
    /// "iPhone · Safari", "Windows · Edge", ...
    pub device: String,
    /// "direct" (peer-to-peer) or "relayed" (through the portal's link).
    pub link: &'static str,
}

#[derive(Default)]
pub struct Presence {
    open: Mutex<HashMap<u64, Connection>>,
    next: AtomicU64,
}

/// Keeps a connection listed while it lives.
pub struct Guard {
    presence: Arc<Presence>,
    id: u64,
}

impl Drop for Guard {
    fn drop(&mut self) {
        self.presence.open.lock().unwrap().remove(&self.id);
    }
}

impl Presence {
    pub fn enter(self: &Arc<Self>, connection: Connection) -> Guard {
        let id = self.next.fetch_add(1, Ordering::Relaxed);
        self.open.lock().unwrap().insert(id, connection);
        Guard { presence: self.clone(), id }
    }

    /// Open connections, oldest first.
    pub fn snapshot(&self) -> Vec<Connection> {
        let mut all: Vec<Connection> = self.open.lock().unwrap().values().cloned().collect();
        all.sort_by_key(|c| c.since);
        all
    }
}

/// A short, human description of the browser from its User-Agent.
pub fn device(headers: &HeaderMap) -> String {
    let ua = headers
        .get(axum::http::header::USER_AGENT)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("");
    let system = if ua.contains("iPhone") {
        "iPhone"
    } else if ua.contains("iPad") {
        "iPad"
    } else if ua.contains("Android") {
        "Android"
    } else if ua.contains("Windows") {
        "Windows"
    } else if ua.contains("Mac OS X") || ua.contains("Macintosh") {
        "Mac"
    } else if ua.contains("CrOS") {
        "ChromeOS"
    } else if ua.contains("Linux") {
        "Linux"
    } else {
        "Unknown device"
    };
    // Order matters: Edge and Opera also say "Chrome"; Chrome also says "Safari".
    let browser = if ua.contains("Edg/") || ua.contains("EdgA/") || ua.contains("EdgiOS/") {
        "Edge"
    } else if ua.contains("OPR/") {
        "Opera"
    } else if ua.contains("SamsungBrowser/") {
        "Samsung Internet"
    } else if ua.contains("Firefox/") || ua.contains("FxiOS/") {
        "Firefox"
    } else if ua.contains("Chrome/") || ua.contains("CriOS/") {
        "Chrome"
    } else if ua.contains("Safari/") {
        "Safari"
    } else {
        ""
    };
    if browser.is_empty() {
        system.to_string()
    } else {
        format!("{system} · {browser}")
    }
}
