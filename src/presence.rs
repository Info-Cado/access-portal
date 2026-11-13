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
    /// Assigned when the connection is listed.
    pub id: u64,
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

/// Ends a connection from outside, with the message its user sees.
#[derive(Default)]
pub struct Kick {
    notify: tokio::sync::Notify,
    reason: Mutex<String>,
}

impl Kick {
    pub fn kick(&self, reason: &str) {
        *self.reason.lock().unwrap() = reason.to_string();
        self.notify.notify_one();
    }

    /// Waits for a kick and returns its message.
    pub async fn wait(&self) -> String {
        self.notify.notified().await;
        self.reason.lock().unwrap().clone()
    }
}

pub const DISCONNECTED: &str = "Your administrator disconnected this session. Your apps are still running.";
pub const SIGNED_OUT: &str = "Your administrator signed you out. Your apps were closed.";

#[derive(Default)]
pub struct Presence {
    open: Mutex<HashMap<u64, (Connection, Arc<Kick>)>>,
    next: AtomicU64,
}

/// Keeps a connection listed while it lives.
pub struct Guard {
    presence: Arc<Presence>,
    id: u64,
    /// Signalled when an administrator disconnects this connection.
    pub kick: Arc<Kick>,
}

impl Drop for Guard {
    fn drop(&mut self) {
        self.presence.open.lock().unwrap().remove(&self.id);
    }
}

impl Presence {
    pub fn enter(self: &Arc<Self>, mut connection: Connection) -> Guard {
        let id = self.next.fetch_add(1, Ordering::Relaxed) + 1;
        connection.id = id;
        let kick = Arc::new(Kick::default());
        self.open.lock().unwrap().insert(id, (connection, kick.clone()));
        Guard { presence: self.clone(), id, kick }
    }

    /// Open connections, oldest first.
    pub fn snapshot(&self) -> Vec<Connection> {
        let mut all: Vec<Connection> = self.open.lock().unwrap().values().map(|(c, _)| c.clone()).collect();
        all.sort_by_key(|c| c.since);
        all
    }

    /// Disconnect one connection. False if it is already gone.
    pub fn kick(&self, id: u64, reason: &str) -> bool {
        match self.open.lock().unwrap().get(&id) {
            Some((_, k)) => {
                k.kick(reason);
                true
            }
            None => false,
        }
    }

    /// Disconnect every connection of a portal user; returns how many.
    pub fn kick_user(&self, user_id: i64, reason: &str) -> usize {
        let open = self.open.lock().unwrap();
        let mut n = 0;
        for (c, k) in open.values() {
            if c.user_id == user_id {
                k.kick(reason);
                n += 1;
            }
        }
        n
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
