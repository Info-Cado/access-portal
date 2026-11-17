//! Updates: new versions are published to the sign-up service (an R2 bucket
//! behind `provision/worker.js`, by `scripts/release.ps1`), signed with the
//! project's updater key. The app checks shortly after it starts and then
//! every 2 minutes. A new version is always installed straight away, whoever
//! is connected (their Windows sessions keep running and their pages
//! reconnect), and the app restarts into it by itself. There is no opting out.

use std::sync::Mutex;
use std::time::Duration;

use serde_json::{json, Value};
use tauri::{AppHandle, Manager};
use tauri_plugin_updater::UpdaterExt;

use crate::tunnel;

/// The public half of the updater key; installers must be signed with the other half.
pub const PUBKEY: &str = include_str!("../updater.key.pub");

#[derive(Default)]
pub struct UpdateState(Mutex<Info>);

#[derive(Default, Clone)]
struct Info {
    /// A newer version that was found, and its notes.
    available: Option<(String, String)>,
    /// Unix time of the last check.
    checked_at: Option<i64>,
    /// "idle", "checking", "installing"
    state: &'static str,
    error: Option<String>,
}

fn now() -> i64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs() as i64).unwrap_or(0)
}

pub fn status_json(app: &AppHandle) -> Value {
    let info = app.state::<UpdateState>().0.lock().unwrap().clone();
    json!({
        "current": app.package_info().version.to_string(),
        "available": info.available.as_ref().map(|(v, _)| v),
        "notes": info.available.as_ref().map(|(_, n)| n),
        "checked_at": info.checked_at,
        "state": if info.state.is_empty() { "idle" } else { info.state },
        "error": info.error,
        "enabled": tunnel::service_base().is_some(),
    })
}

fn set(app: &AppHandle, f: impl FnOnce(&mut Info)) {
    f(&mut app.state::<UpdateState>().0.lock().unwrap());
}

/// Look for a newer version; with `install`, install it (the app restarts).
pub async fn check(app: &AppHandle, install: bool) -> Result<Value, String> {
    let Some(base) = tunnel::service_base() else {
        return Err("This build has no update service.".into());
    };
    set(app, |i| {
        i.state = "checking";
        i.error = None;
    });
    let endpoint = format!("{base}/v1/update/{{{{target}}}}/{{{{arch}}}}/{{{{current_version}}}}");
    let result = async {
        let updater = app
            .updater_builder()
            .pubkey(PUBKEY)
            .endpoints(vec![endpoint.parse().map_err(|e| format!("{e}"))?])
            .map_err(|e| e.to_string())?
            .header("x-provision-key", tunnel::PROVISION_KEY.unwrap_or(""))
            .map_err(|e| e.to_string())?
            .timeout(Duration::from_secs(300))
            .build()
            .map_err(|e| e.to_string())?;
        let update = updater.check().await.map_err(|e| e.to_string())?;
        set(app, |i| {
            i.checked_at = Some(now());
            i.available = update.as_ref().map(|u| (u.version.clone(), u.body.clone().unwrap_or_default()));
        });
        if let (true, Some(update)) = (install, update) {
            tracing::info!("installing update {}", update.version);
            set(app, |i| i.state = "installing");
            update.download_and_install(|_, _| {}, || {}).await.map_err(|e| e.to_string())?;
            // Normally not reached: the installer closes the app and starts the new one.
            app.restart();
        }
        Ok::<_, String>(())
    }
    .await;
    set(app, |i| {
        i.state = "idle";
        i.error = result.as_ref().err().cloned();
    });
    result.map(|_| status_json(app))
}

/// How often to look for a new version.
const CHECK_EVERY: Duration = Duration::from_secs(2 * 60);

/// Check every 2 minutes and install whatever is found at once.
pub fn start(app: AppHandle) {
    tauri::async_runtime::spawn(async move {
        tokio::time::sleep(Duration::from_secs(30)).await;
        // The same failure every 2 minutes goes in the log once.
        let mut last_error = None;
        loop {
            let error = check(&app, true).await.err();
            if error.is_some() && error != last_error {
                tracing::warn!("update check: {}", error.as_deref().unwrap_or(""));
            }
            last_error = error;
            tokio::time::sleep(CHECK_EVERY).await;
        }
    });
}
