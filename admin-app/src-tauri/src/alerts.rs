//! Phone alerts and the heartbeat behind the "offline" alert.
//!
//! Alerts go to a private topic on an ntfy server (made when alerts are turned
//! on in the app; the admin subscribes to it in the ntfy app). Every minute the app looks
//! at what the portal reports (`/api/admin/health`) and at the public link, and
//! sends one alert when a problem starts and one when it clears.
//!
//! Every 5 minutes the app also tells the sign-up service it is alive. If those
//! heartbeats stop (the computer is off, asleep, offline, or the app closed),
//! the service sends the "offline" alert itself.

use std::collections::HashMap;
use std::time::{Duration, Instant};

use serde_json::{json, Value};
use tauri::{AppHandle, Manager};

use crate::{tunnel, Server};

/// A problem must last this long before it is reported.
const LINK_GRACE: Duration = Duration::from_secs(10 * 60);
const RDP_GRACE: Duration = Duration::from_secs(2 * 60);
const LOW_DISK: u64 = 5 * 1024 * 1024 * 1024;

/// Send one alert. Blocking; call from a blocking thread.
pub fn send(server: &str, topic: &str, title: &str, message: &str, tags: &str, high: bool) -> Result<(), String> {
    ureq::post(&format!("{}/{topic}", server.trim_end_matches('/')))
        .header("Title", title)
        .header("Tags", tags)
        .header("Priority", if high { "high" } else { "default" })
        .send(message)
        .map(|_| ())
        .map_err(|e| format!("Couldn't reach {server}: {e}"))
}

/// This install's name in alerts: its public address, or the computer name.
pub fn install_name(app: &AppHandle) -> String {
    let link = app.state::<Option<tunnel::Tunnel>>().inner().as_ref().and_then(|t| t.status().url);
    link.map(|u| u.trim_start_matches("https://").to_string())
        .or_else(|| std::env::var("COMPUTERNAME").ok())
        .unwrap_or_else(|| "Access Portal".into())
}

struct Problem {
    since: Option<Instant>,
    reported: bool,
}

pub fn start(app: AppHandle) {
    tauri::async_runtime::spawn(async move {
        let mut problems: HashMap<&'static str, Problem> = HashMap::new();
        let mut last_heartbeat: Option<Instant> = None;
        loop {
            tokio::time::sleep(Duration::from_secs(60)).await;
            let Server::Running(portal) = app.state::<Server>().inner() else { continue };
            let (status, health) = portal.admin("GET", "/api/admin/health", None).await;
            if status != 200 {
                continue;
            }
            let topic = health["alerts"]["topic"].as_str().map(str::to_string);
            let server = health["alerts"]["server"].as_str().unwrap_or("https://ntfy.envs.net").to_string();
            let name = install_name(&app);

            if last_heartbeat.is_none_or(|t| t.elapsed() >= Duration::from_secs(5 * 60)) {
                last_heartbeat = Some(Instant::now());
                let (name, topic, server, data_dir) = (name.clone(), topic.clone(), server.clone(), portal.data_dir.clone());
                tauri::async_runtime::spawn_blocking(move || heartbeat(&data_dir, &name, topic.as_deref(), &server));
            }
            let Some(topic) = topic else {
                problems.clear();
                continue;
            };

            // What is wrong right now: (key, is it, grace, alert, all-clear).
            let link = app.state::<Option<tunnel::Tunnel>>().inner().as_ref().map(|t| t.status());
            let link_down = link.as_ref().is_some_and(|s| !matches!(s.state, "online" | "unavailable"));
            let failures = health["session_failures"].as_u64().unwrap_or(0);
            let last_failure = health["last_failure"].as_str().unwrap_or("").to_string();
            let free = health["disk_free"].as_u64();
            let rdp_up = health["rdp_listening"].as_bool().unwrap_or(true);
            let checks: [(&'static str, bool, Duration, (String, String), (String, String)); 4] = [
                (
                    "link",
                    link_down,
                    LINK_GRACE,
                    (
                        format!("{name}: the public link is down"),
                        format!(
                            "Users can't reach the portal from outside. {}",
                            link.as_ref().and_then(|s| s.error.clone()).unwrap_or_default()
                        ),
                    ),
                    (format!("{name}: the public link is back"), "Users can reach the portal again.".into()),
                ),
                (
                    "sessions",
                    failures >= 3,
                    Duration::ZERO,
                    (
                        format!("{name}: sessions are failing to start"),
                        format!("{failures} sessions failed in the last 10 minutes. Latest: {last_failure}"),
                    ),
                    (format!("{name}: sessions are starting again"), "No failed sessions in the last 10 minutes.".into()),
                ),
                (
                    "disk",
                    free.is_some_and(|f| f < LOW_DISK),
                    Duration::ZERO,
                    (
                        format!("{name}: the disk is almost full"),
                        format!(
                            "{:.1} GB free. Sessions, printing and file transfers can fail when it runs out.",
                            free.unwrap_or(0) as f64 / 1073741824.0
                        ),
                    ),
                    (format!("{name}: disk space is fine again"), "More than 5 GB free.".into()),
                ),
                (
                    "rdp",
                    !rdp_up,
                    RDP_GRACE,
                    (
                        format!("{name}: Remote Desktop is not accepting connections"),
                        "Nobody can open a session. Open Access Portal → Settings → This computer to fix it.".into(),
                    ),
                    (format!("{name}: Remote Desktop is working again"), "Sessions can be opened again.".into()),
                ),
            ];
            for (key, bad, grace, (title, message), (ok_title, ok_message)) in checks {
                let p = problems.entry(key).or_insert(Problem { since: None, reported: false });
                if bad {
                    let since = *p.since.get_or_insert_with(Instant::now);
                    if !p.reported && since.elapsed() >= grace {
                        p.reported = true;
                        let (topic, server) = (topic.clone(), server.clone());
                        tauri::async_runtime::spawn_blocking(move || {
                            if let Err(e) = send(&server, &topic, &title, &message, "warning", true) {
                                tracing::warn!("alert not sent: {e}");
                            }
                        });
                    }
                } else {
                    if p.reported {
                        let (topic, server) = (topic.clone(), server.clone());
                        tauri::async_runtime::spawn_blocking(move || {
                            let _ = send(&server, &topic, &ok_title, &ok_message, "white_check_mark", false);
                        });
                    }
                    *p = Problem { since: None, reported: false };
                }
            }
        }
    });
}

/// Tell the sign-up service this install is alive.
fn heartbeat(data_dir: &std::path::Path, name: &str, topic: Option<&str>, server: &str) {
    let (Some(base), Some(key)) = (tunnel::service_base(), tunnel::PROVISION_KEY) else { return };
    let Ok(install_id) = tunnel::install_id(data_dir) else { return };
    let body: Value = json!({ "install_id": install_id, "name": name, "topic": topic, "server": server });
    if let Err(e) = ureq::post(&format!("{base}/v1/heartbeat"))
        .header("x-provision-key", key)
        .header("content-type", "application/json")
        .send(body.to_string())
    {
        tracing::debug!("heartbeat not sent: {e}");
    }
}
