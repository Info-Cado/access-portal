//! A problem report: what this computer looks like, sent to the sign-up
//! service (`provision/worker.js`, `POST /v1/report`) when the admin clicks
//! "Send report" on the This computer card, so a failed setup can be looked at
//! without asking for screenshots. Nothing is sent otherwise.
//!
//! It holds the setup checks, facts about Windows, Remote Desktop and antivirus
//! (`access_portal::diagnostics_blocking`), the portal's health, the admin's
//! recent actions (setup results among them) and the end of `portal.log`.

use std::io::{Read, Seek, SeekFrom};
use std::path::Path;

use serde_json::{json, Value};
use tauri::Manager;

use crate::tunnel;

/// How much of the end of `portal.log` goes in.
const LOG_TAIL_BYTES: u64 = 192 * 1024;

/// Gather the report and send it; answers the report's id.
pub async fn send(app: &tauri::AppHandle, portal: Option<&access_portal::Portal>, data_dir: &Path) -> Result<String, String> {
    let (Some(base), Some(key)) = (tunnel::service_base(), tunnel::PROVISION_KEY) else {
        return Err("This build can't send reports (it has no sign-up service).".into());
    };
    let install_id = tunnel::install_id(data_dir).map_err(|e| format!("Reading the install id: {e}"))?;

    let diagnostics = tauri::async_runtime::spawn_blocking(access_portal::diagnostics_blocking)
        .await
        .map_err(|e| e.to_string())?;
    let (health, activity) = match portal {
        Some(p) => {
            let (_, health) = p.admin("GET", "/api/admin/health", None).await;
            let (_, list) = p.admin("GET", "/api/admin/activity?limit=500", None).await;
            let admin: Vec<Value> = list["entries"]
                .as_array()
                .map(|e| e.iter().filter(|x| x["kind"] == "admin").take(60).cloned().collect())
                .unwrap_or_default();
            (health, Value::from(admin))
        }
        None => (Value::Null, Value::Null),
    };

    let public = app.state::<Option<tunnel::Tunnel>>().inner().as_ref().map(|t| t.status().to_json());
    let portal_state = match portal {
        Some(p) => json!({ "running": true, "address": p.address }),
        None => json!({ "running": false }),
    };
    let body = json!({
        "install_id": install_id,
        "name": crate::alerts::install_name(app),
        "computer": std::env::var("COMPUTERNAME").unwrap_or_default(),
        "version": app.package_info().version.to_string(),
        "portal": portal_state,
        "public": public,
        "checks": diagnostics["checks"],
        "system": diagnostics["system"],
        "health": health,
        "activity": activity,
        "log": log_tail(&data_dir.join("portal.log")),
    });

    let url = format!("{base}/v1/report");
    let answer = tauri::async_runtime::spawn_blocking(move || -> Result<Value, String> {
        let mut res = ureq::post(&url)
            .header("x-provision-key", key)
            .header("content-type", "application/json")
            .send(body.to_string())
            .map_err(|e| format!("Sending the report: {e}"))?;
        res.body_mut().read_json::<Value>().map_err(|e| format!("Sending the report: {e}"))
    })
    .await
    .map_err(|e| e.to_string())??;
    let id = answer["id"].as_str().ok_or("The report service gave no report number.")?.to_string();
    tracing::info!("problem report sent: {id}");
    Ok(id)
}

/// The end of the log, starting at a whole line, with tunnel tokens hidden.
fn log_tail(path: &Path) -> String {
    let Ok(mut file) = std::fs::File::open(path) else { return String::new() };
    let len = file.metadata().map(|m| m.len()).unwrap_or(0);
    let start = len.saturating_sub(LOG_TAIL_BYTES);
    let mut bytes = Vec::new();
    if file.seek(SeekFrom::Start(start)).is_err() || file.read_to_end(&mut bytes).is_err() {
        return String::new();
    }
    let text = String::from_utf8_lossy(&bytes);
    let text = if start > 0 { text.split_once('\n').map(|(_, rest)| rest).unwrap_or("") } else { &text };
    redact(text)
}

/// Hide anything shaped like a tunnel token (base64 JSON: `eyJ...`).
fn redact(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    while let Some(i) = rest.find("eyJ") {
        out.push_str(&rest[..i]);
        let tail = &rest[i..];
        let end = tail
            .find(|c: char| !(c.is_ascii_alphanumeric() || c == '-' || c == '_' || c == '=' || c == '+' || c == '/'))
            .unwrap_or(tail.len());
        if end >= 40 {
            out.push_str("[hidden]");
        } else {
            out.push_str(&tail[..end]);
        }
        rest = &tail[end..];
    }
    out.push_str(rest);
    out
}

#[cfg(test)]
mod tests {
    #[test]
    fn hides_tokens() {
        let token = format!("eyJ{}", "a".repeat(60));
        assert_eq!(super::redact(&format!("x --token {token} y eyJshort")), "x --token [hidden] y eyJshort");
    }
}
