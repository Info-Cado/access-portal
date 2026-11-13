//! The app's native side: it holds the connection to one portal and passes the
//! page's requests to the portal's JSON admin API (`/api/...`).
//!
//! Requests go through here rather than straight from the page so that the
//! session cookie stays in native memory and the page needs no network access
//! of its own.

use std::sync::Mutex;
use std::time::Duration;

use serde_json::{json, Value};
use tauri::State;

struct Portal {
    client: reqwest::Client,
    /// Portal address without a trailing slash, e.g. `http://127.0.0.1:8088`.
    base: Mutex<String>,
}

/// Choose which portal the app talks to. Returns the cleaned-up address.
#[tauri::command]
fn set_server(portal: State<'_, Portal>, url: String) -> Result<String, String> {
    let mut url = url.trim().trim_end_matches('/').to_string();
    if url.is_empty() {
        return Err("Enter the portal's address.".into());
    }
    if !url.starts_with("http://") && !url.starts_with("https://") {
        url = format!("http://{url}");
    }
    reqwest::Url::parse(&url).map_err(|_| "That address doesn't look right.".to_string())?;
    *portal.base.lock().unwrap() = url.clone();
    Ok(url)
}

/// One call to the portal's API. Answers `{ status, data }`; `data` is the
/// portal's JSON (an `error` field on failures). Only `/api/` paths are allowed.
#[tauri::command]
async fn request(
    portal: State<'_, Portal>,
    method: String,
    path: String,
    body: Option<Value>,
) -> Result<Value, String> {
    if !path.starts_with("/api/") {
        return Err("Only the portal's API can be called.".into());
    }
    let base = portal.base.lock().unwrap().clone();
    if base.is_empty() {
        return Err("No portal address set.".into());
    }
    let method = reqwest::Method::from_bytes(method.as_bytes())
        .map_err(|_| "Unknown request method.".to_string())?;

    let mut req = portal.client.request(method, format!("{base}{path}"));
    if let Some(body) = body {
        req = req.json(&body);
    }
    let res = req.send().await.map_err(|e| {
        if e.is_timeout() {
            "The portal took too long to answer.".to_string()
        } else {
            format!("Can't reach the portal at {base}. Is it running?")
        }
    })?;

    let status = res.status().as_u16();
    let text = res.text().await.unwrap_or_default();
    let data = serde_json::from_str::<Value>(&text).unwrap_or_else(|_| {
        // Not JSON: an older portal without the admin API, or something else
        // answering on that address.
        json!({ "error": "That address answered, but not like an access-portal with the admin API." })
    });
    Ok(json!({ "status": status, "data": data }))
}

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    let client = reqwest::Client::builder()
        .cookie_store(true)
        .timeout(Duration::from_secs(20))
        .redirect(reqwest::redirect::Policy::none())
        .build()
        .expect("building the HTTP client");

    tauri::Builder::default()
        .manage(Portal {
            client,
            base: Mutex::new(String::new()),
        })
        .invoke_handler(tauri::generate_handler![set_server, request])
        .run(tauri::generate_context!())
        .expect("running the admin app");
}
