//! HTTP layer: router, auth extractors, pages, and handlers.

use async_trait::async_trait;
use axum::body::Body;
use axum::extract::ws::WebSocketUpgrade;
use axum::extract::DefaultBodyLimit;
use axum::extract::{FromRequestParts, Path, Query, State};
use axum::http::request::Parts;
use axum::http::{header, HeaderMap, StatusCode};
use axum::response::{Html, IntoResponse, Redirect, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use axum_extra::extract::cookie::{Cookie, CookieJar, SameSite};
use axum_extra::extract::Form;
use futures_util::StreamExt as _;
use serde::Deserialize;
use tokio::io::AsyncWriteExt as _;

use crate::error::AppError;
use crate::models::User;
use crate::{auth, db, rdp, transport, AppState};

pub fn router(state: AppState) -> Router {
    Router::new()
        .route("/", get(dashboard))
        .route("/login", get(login_form).post(login_submit))
        .route("/login/totp", get(totp_form).post(totp_submit))
        .route("/logout", post(logout))
        .route("/connect/:key", get(connect))
        .route("/ws/:key", get(rdp_socket))
        .route("/rtc/:key", post(rtc_offer))
        .route("/files", get(files_list))
        .route("/files/upload", post(files_upload).layer(DefaultBodyLimit::disable()))
        .route("/files/dl/:name", get(files_download))
        .with_state(state)
}

// ---------------------------------------------------------------------------
// Extractors
// ---------------------------------------------------------------------------

/// A logged-in user. Rejection redirects to the login page.
pub struct CurrentUser(pub User);

#[async_trait]
impl FromRequestParts<AppState> for CurrentUser {
    type Rejection = Redirect;

    async fn from_request_parts(parts: &mut Parts, state: &AppState) -> Result<Self, Redirect> {
        let jar = CookieJar::from_headers(&parts.headers);
        if let Some(token) = jar.get("session").map(|c| c.value().to_string()) {
            if let Ok(Some(user)) = db::user_by_session(&state.pool, &token).await {
                return Ok(CurrentUser(user));
            }
        }
        Err(Redirect::to("/login"))
    }
}

// ---------------------------------------------------------------------------
// HTML helpers
// ---------------------------------------------------------------------------

fn esc(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
}

const STYLE: &str = r#"
*{box-sizing:border-box}
body{font-family:system-ui,-apple-system,Segoe UI,sans-serif;margin:0;background:#0e1220;color:#e8ecf6}
.wrap{max-width:860px;margin:0 auto;padding:32px 20px 64px}
a{color:#7c9bff}
h1,h2{font-weight:700;letter-spacing:-.01em}
.card{background:#171c2e;border:1px solid #2a3149;border-radius:12px;padding:20px;margin:16px 0}
.app{display:flex;justify-content:space-between;align-items:center;gap:12px;flex-wrap:wrap}
.btn{display:inline-block;background:#2453d6;color:#fff;border:0;border-radius:8px;padding:9px 16px;font-size:.95rem;cursor:pointer;text-decoration:none}
.btn.sec{background:#2a3149}
.btn.danger{background:#b3261e}
input,select{width:100%;padding:9px 11px;border-radius:8px;border:1px solid #2a3149;background:#0e1220;color:#e8ecf6;font-size:.95rem}
label{display:block;margin:10px 0 4px;font-size:.85rem;color:#9aa3ba}
.row{display:flex;gap:12px;flex-wrap:wrap}
.row>div{flex:1;min-width:160px}
.err{background:#351a1a;color:#ff8a80;border:1px solid #b3261e;border-radius:8px;padding:10px 12px;margin:12px 0}
.ok{background:#132a22;color:#5fd39a;border:1px solid #137a4b;border-radius:8px;padding:10px 12px;margin:12px 0}
table{width:100%;border-collapse:collapse}
td,th{text-align:left;padding:8px;border-bottom:1px solid #2a3149;font-size:.9rem}
.muted{color:#9aa3ba;font-size:.85rem}
code{background:#0e1220;border:1px solid #2a3149;border-radius:5px;padding:1px 5px}
.top{display:flex;justify-content:space-between;align-items:center;gap:12px}
"#;

fn page(title: &str, body: &str) -> Html<String> {
    Html(format!(
        "<!doctype html><html><head><meta charset=\"utf-8\">\
<meta name=\"viewport\" content=\"width=device-width,initial-scale=1\">\
<title>{}</title><style>{}</style></head><body><div class=\"wrap\">{}</div></body></html>",
        esc(title),
        STYLE,
        body
    ))
}

pub(crate) fn session_cookie(state: &AppState, token: String) -> Cookie<'static> {
    Cookie::build(("session", token))
        .path("/")
        .http_only(true)
        .same_site(SameSite::Lax)
        .secure(state.config.cookie_secure)
        .build()
}

fn pending_cookie(state: &AppState, token: String) -> Cookie<'static> {
    Cookie::build(("pending", token))
        .path("/")
        .http_only(true)
        .same_site(SameSite::Lax)
        .secure(state.config.cookie_secure)
        .build()
}

/// A removal cookie must carry the same path as the one that was set, or some
/// browsers keep the original.
fn removal_cookie(name: &'static str) -> Cookie<'static> {
    Cookie::build((name, "")).path("/").build()
}

// ---------------------------------------------------------------------------
// Dashboard
// ---------------------------------------------------------------------------

async fn dashboard(
    State(state): State<AppState>,
    CurrentUser(user): CurrentUser,
) -> Result<Response, AppError> {
    let apps = db::apps_for_user(&state.pool, user.id).await?;

    // A regular user with exactly one app has nothing to choose: open it directly.
    if !user.is_admin() && apps.len() == 1 {
        return Ok(Redirect::to(&format!("/connect/{}", apps[0].key)).into_response());
    }

    let mut list = String::new();
    if apps.is_empty() {
        list.push_str("<p class=\"muted\">No apps assigned yet. Ask your administrator.</p>");
    }
    for app in &apps {
        list.push_str(&format!(
            "<div class=\"card app\"><div><strong>{}</strong></div>\
             <a class=\"btn\" href=\"/connect/{}\" target=\"_blank\" rel=\"noopener\">Open</a></div>",
            esc(&app.name),
            esc(&app.key),
        ));
    }

    let body = format!(
        "<div class=\"top\"><h1>Your apps</h1>\
         <form method=\"post\" action=\"/logout\"><button class=\"btn sec\">Log out</button></form></div>\
         <p class=\"muted\">Signed in as {}</p>{}",
        esc(&user.username),
        list
    );
    Ok(page("Your apps", &body).into_response())
}

// ---------------------------------------------------------------------------
// Login (password step)
// ---------------------------------------------------------------------------

#[derive(Deserialize)]
struct LoginInput {
    username: String,
    password: String,
}

async fn login_form() -> Html<String> {
    login_page(None)
}

fn login_page(error: Option<&str>) -> Html<String> {
    let err = error
        .map(|e| format!("<div class=\"err\">{}</div>", esc(e)))
        .unwrap_or_default();
    let body = format!(
        "<h1>Sign in</h1>{}\
         <form method=\"post\" action=\"/login\" class=\"card\">\
         <label>Username</label><input name=\"username\" autofocus autocomplete=\"username\">\
         <label>Password</label><input name=\"password\" type=\"password\" autocomplete=\"current-password\">\
         <div style=\"margin-top:16px\"><button class=\"btn\">Continue</button></div></form>",
        err
    );
    page("Sign in", &body)
}

async fn login_submit(
    State(state): State<AppState>,
    jar: CookieJar,
    Form(input): Form<LoginInput>,
) -> Result<Response, AppError> {
    let user = db::user_by_username(&state.pool, input.username.trim()).await?;

    let Some(user) = user else {
        // Same message whether or not the user exists.
        return Ok(login_page(Some("Invalid username or password.")).into_response());
    };

    if let Some(until) = user.locked_until {
        if until > db::now() {
            return Ok(login_page(Some(
                "Account temporarily locked after too many attempts. Try again later.",
            ))
            .into_response());
        }
    }

    if !auth::verify_password(&input.password, &user.password_hash) {
        db::note_failed_login(&state.pool, &state.config, user.id).await?;
        return Ok(login_page(Some("Invalid username or password.")).into_response());
    }

    db::reset_failed_login(&state.pool, user.id).await?;

    if user.totp_enabled() {
        let token = db::create_pending(&state.pool, user.id).await?;
        let jar = jar.add(pending_cookie(&state, token));
        return Ok((jar, Redirect::to("/login/totp")).into_response());
    }

    let token = db::create_session(&state.pool, &state.config, user.id).await?;
    let jar = jar.add(session_cookie(&state, token));
    Ok((jar, Redirect::to("/")).into_response())
}

// ---------------------------------------------------------------------------
// Login (TOTP step)
// ---------------------------------------------------------------------------

#[derive(Deserialize)]
struct TotpInput {
    code: String,
}

async fn totp_form(
    State(state): State<AppState>,
    jar: CookieJar,
) -> Result<Response, AppError> {
    // Only reachable with a valid pending token.
    let Some(token) = jar.get("pending").map(|c| c.value().to_string()) else {
        return Ok(Redirect::to("/login").into_response());
    };
    if db::peek_pending(&state.pool, &token).await?.is_none() {
        return Ok(Redirect::to("/login").into_response());
    }
    Ok(totp_page(None).into_response())
}

fn totp_page(error: Option<&str>) -> Html<String> {
    let err = error
        .map(|e| format!("<div class=\"err\">{}</div>", esc(e)))
        .unwrap_or_default();
    let body = format!(
        "<h1>Two-step verification</h1>\
         <p class=\"muted\">Enter the 6-digit code from your authenticator app.</p>{}\
         <form method=\"post\" action=\"/login/totp\" class=\"card\">\
         <label>Code</label><input name=\"code\" inputmode=\"numeric\" autocomplete=\"one-time-code\" autofocus>\
         <div style=\"margin-top:16px\"><button class=\"btn\">Verify</button></div></form>",
        err
    );
    page("Two-step verification", &body)
}

async fn totp_submit(
    State(state): State<AppState>,
    jar: CookieJar,
    Form(input): Form<TotpInput>,
) -> Result<Response, AppError> {
    let Some(token) = jar.get("pending").map(|c| c.value().to_string()) else {
        return Ok(Redirect::to("/login").into_response());
    };
    let Some(user_id) = db::peek_pending(&state.pool, &token).await? else {
        return Ok(Redirect::to("/login").into_response());
    };
    let Some(user) = db::user_by_id(&state.pool, user_id).await? else {
        return Ok(Redirect::to("/login").into_response());
    };

    let secret = user.totp_secret.clone().unwrap_or_default();
    if !auth::verify_totp(&secret, &input.code) {
        return Ok(totp_page(Some("Incorrect code. Try again.")).into_response());
    }

    db::delete_pending(&state.pool, &token).await?;
    let session = db::create_session(&state.pool, &state.config, user.id).await?;
    let jar = jar
        .remove(removal_cookie("pending"))
        .add(session_cookie(&state, session));
    Ok((jar, Redirect::to("/")).into_response())
}

async fn logout(
    State(state): State<AppState>,
    jar: CookieJar,
) -> Result<Response, AppError> {
    if let Some(token) = jar.get("session").map(|c| c.value().to_string()) {
        db::delete_session(&state.pool, &token).await?;
    }
    let jar = jar.remove(removal_cookie("session"));
    Ok((jar, Redirect::to("/login")).into_response())
}

// ---------------------------------------------------------------------------
// Connect: serve the session page for one allowed app
// ---------------------------------------------------------------------------

async fn connect(
    State(state): State<AppState>,
    CurrentUser(user): CurrentUser,
    Path(key): Path<String>,
) -> Result<Response, AppError> {
    let Some(app) = db::app_by_key(&state.pool, &key).await? else {
        return Ok((StatusCode::NOT_FOUND, Html("<h1>Unknown app</h1>".to_string()))
            .into_response());
    };

    if !db::user_can_access(&state.pool, user.id, app.id).await? {
        return Ok((
            StatusCode::FORBIDDEN,
            Html("<h1>You don't have access to this app</h1>".to_string()),
        )
            .into_response());
    }

    // The page below connects back over a WebSocket; the portal is the RDP client.
    let html = include_str!("session.html")
        .replace("__TITLE__", &esc(&app.name))
        .replace("__KEY__", &js_str(&app.key))
        .replace("__RTC__", &rtc_page_config(&state.config));
    Ok(Html(html).into_response())
}

/// Make a value safe to place inside a JavaScript string literal by keeping
/// only characters that need no escaping (app keys are simple slugs).
fn js_str(s: &str) -> String {
    s.chars()
        .filter(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.'))
        .collect()
}

#[derive(Deserialize)]
struct ViewportQuery {
    w: Option<u32>,
    h: Option<u32>,
    /// 1 when the browser can decode H.264 (WebCodecs).
    vd: Option<u8>,
}

/// WebSocket endpoint behind the built-in RDP client page.
async fn rdp_socket(
    State(state): State<AppState>,
    CurrentUser(user): CurrentUser,
    Path(key): Path<String>,
    Query(viewport): Query<ViewportQuery>,
    headers: HeaderMap,
    upgrade: WebSocketUpgrade,
) -> Result<Response, AppError> {
    // Browsers attach cookies to cross-site WebSocket handshakes, so refuse
    // any page that is not the portal itself.
    if !same_origin(&headers) {
        return Ok(StatusCode::FORBIDDEN.into_response());
    }

    let target = match session_target(&state, &user, &key, &viewport).await? {
        Ok(target) => target,
        Err(status) => return Ok(status.into_response()),
    };
    Ok(upgrade.on_upgrade(move |socket| {
        rdp::run(target, Box::new(transport::WsTransport(socket)))
    }))
}

/// Signalling for the direct (peer-to-peer) link: the browser posts its WebRTC
/// offer, the portal replies with its answer, and the session starts as soon as
/// the browser's data channel opens. If it never opens, the browser falls back
/// to the WebSocket endpoint above.
async fn rtc_offer(
    State(state): State<AppState>,
    CurrentUser(user): CurrentUser,
    Path(key): Path<String>,
    Query(viewport): Query<ViewportQuery>,
    headers: HeaderMap,
    Json(offer): Json<webrtc::peer_connection::RTCSessionDescription>,
) -> Result<Response, AppError> {
    if !state.config.p2p {
        return Ok(StatusCode::NOT_FOUND.into_response());
    }
    if !same_origin(&headers) {
        return Ok(StatusCode::FORBIDDEN.into_response());
    }
    let target = match session_target(&state, &user, &key, &viewport).await? {
        Ok(target) => target,
        Err(status) => return Ok(status.into_response()),
    };
    let username = target.username.clone();
    let servers = vec![webrtc::peer_connection::RTCIceServer {
        urls: state.config.ice_urls.clone(),
        username: state.config.ice_username.clone(),
        credential: state.config.ice_credential.clone(),
        ..Default::default()
    }];
    let on_open: transport::OnOpen = Box::new(move |link| {
        tokio::spawn(rdp::run(target, Box::new(link)));
    });
    match transport::answer_offer(offer, servers, on_open).await {
        Ok(answer) => Ok(Json(answer).into_response()),
        Err(e) => {
            tracing::warn!("direct link setup failed for {username}: {e:#}");
            Ok(StatusCode::BAD_GATEWAY.into_response())
        }
    }
}

/// The WebRTC settings the session page needs, as a JSON literal for its script.
fn rtc_page_config(config: &crate::config::Config) -> String {
    let mut server = serde_json::json!({ "urls": config.ice_urls });
    if !config.ice_username.is_empty() {
        server["username"] = config.ice_username.clone().into();
        server["credential"] = config.ice_credential.clone().into();
    }
    let servers = if config.ice_urls.is_empty() {
        serde_json::json!([])
    } else {
        serde_json::json!([server])
    };
    serde_json::json!({ "p2p": config.p2p, "iceServers": servers })
        .to_string()
        .replace("</", "<\\/")
}

/// Check that `user` may open app `key` and work out where and how to connect.
async fn session_target(
    state: &AppState,
    user: &User,
    key: &str,
    viewport: &ViewportQuery,
) -> Result<Result<rdp::Target, StatusCode>, AppError> {
    let Some(app) = db::app_by_key(&state.pool, key).await? else {
        return Ok(Err(StatusCode::NOT_FOUND));
    };
    if !db::user_can_access(&state.pool, user.id, app.id).await? {
        return Ok(Err(StatusCode::FORBIDDEN));
    }

    let password = auth::decrypt_at_rest(&state.config.portal_secret, &user.win_password)?;
    // A fixed size (the default, 1920x1080) is scaled to fit by the browser;
    // otherwise the session takes the size of the browser window.
    let (width, height) = match state.config.session_size {
        Some((w, h)) => rdp::sane_size(u32::from(w), u32::from(h)),
        None => rdp::sane_size(viewport.w.unwrap_or(1280), viewport.h.unwrap_or(800)),
    };
    let target = rdp::Target {
        host: app.hostname.clone(),
        port: app.port.parse().unwrap_or(3389),
        username: user.win_username.clone(),
        password,
        width,
        height,
        // Only admins are shown why a connection failed; everyone else gets a
        // generic message and the detail goes to the log.
        show_errors: user.is_admin(),
        resizable: state.config.session_size.is_none(),
        // The browser advertises H.264 decode support with ?vd=1.
        video_ok: state.config.video && viewport.vd == Some(1),
        files_dir: user_files_dir(state, user).await.ok(),
    };
    Ok(Ok(target))
}

/// True unless the request names an `Origin` other than the portal itself.
fn same_origin(headers: &HeaderMap) -> bool {
    let host = headers.get(header::HOST).and_then(|v| v.to_str().ok()).unwrap_or("");
    match headers.get(header::ORIGIN).and_then(|v| v.to_str().ok()) {
        Some(origin) => origin.split("://").nth(1).unwrap_or("") == host,
        None => true,
    }
}

// ---------------------------------------------------------------------------
// Files: each user has one transfer folder on the portal host. Sessions see it
// as the "Portal" drive; the browser can upload into it and download from it.
// ---------------------------------------------------------------------------

/// Reduce an untrusted name to a single safe Windows file name, or `None`.
fn safe_file_name(name: &str) -> Option<String> {
    let last = name.rsplit(['/', '\\']).next().unwrap_or("");
    let cleaned: String = last
        .chars()
        .map(|c| {
            if c.is_control() || matches!(c, '<' | '>' | ':' | '"' | '/' | '\\' | '|' | '?' | '*') {
                '_'
            } else {
                c
            }
        })
        .collect();
    let cleaned = cleaned.trim().trim_end_matches(['.', ' ']).to_string();
    if cleaned.is_empty()
        || cleaned.starts_with('.')
        || cleaned.len() > 200
        || ironrdp_cliprdr::is_windows_device_name(&cleaned)
    {
        return None;
    }
    Some(cleaned)
}

/// The user's transfer folder, created on first use.
async fn user_files_dir(state: &AppState, user: &User) -> anyhow::Result<std::path::PathBuf> {
    let name = safe_file_name(&user.username)
        .ok_or_else(|| anyhow::anyhow!("username cannot be used as a folder name"))?;
    let dir = state.config.files_dir.join(name);
    tokio::fs::create_dir_all(&dir).await?;
    Ok(dir)
}

async fn files_list(
    State(state): State<AppState>,
    CurrentUser(user): CurrentUser,
) -> Result<Response, AppError> {
    let dir = user_files_dir(&state, &user).await?;
    let mut files = Vec::new();
    let mut entries = tokio::fs::read_dir(&dir).await?;
    while let Some(entry) = entries.next_entry().await? {
        let Ok(meta) = entry.metadata().await else { continue };
        let name = entry.file_name().to_string_lossy().into_owned();
        if !meta.is_file() || name.starts_with('.') {
            continue;
        }
        let modified = meta
            .modified()
            .ok()
            .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
            .map(|d| d.as_secs())
            .unwrap_or(0);
        files.push((modified, name, meta.len()));
    }
    files.sort_by(|a, b| b.0.cmp(&a.0));
    let list: Vec<_> = files
        .into_iter()
        .map(|(modified, name, size)| serde_json::json!({"name": name, "size": size, "modified": modified}))
        .collect();
    Ok(Json(serde_json::json!({"drive": rdp::DRIVE_NAME, "files": list})).into_response())
}

#[derive(Deserialize)]
struct UploadQuery {
    name: String,
}

/// Receive one file from the browser (raw request body) into the user's folder.
async fn files_upload(
    State(state): State<AppState>,
    CurrentUser(user): CurrentUser,
    Query(query): Query<UploadQuery>,
    headers: HeaderMap,
    body: Body,
) -> Result<Response, AppError> {
    if !same_origin(&headers) {
        return Ok(StatusCode::FORBIDDEN.into_response());
    }
    let Some(name) = safe_file_name(&query.name) else {
        return Ok((StatusCode::BAD_REQUEST, "That file name can't be used.").into_response());
    };
    let dir = user_files_dir(&state, &user).await?;

    // Never overwrite: "report.pdf" becomes "report (1).pdf" and so on.
    let (stem, ext) = match name.rsplit_once('.') {
        Some((stem, ext)) if !stem.is_empty() => (stem.to_string(), format!(".{ext}")),
        _ => (name.clone(), String::new()),
    };
    let mut final_name = name.clone();
    let mut n = 1;
    while tokio::fs::try_exists(dir.join(&final_name)).await.unwrap_or(false) {
        final_name = format!("{stem} ({n}){ext}");
        n += 1;
    }

    // Write under a dot-name first so a half-sent file is never listed.
    let partial = dir.join(format!(".{final_name}.uploading"));
    let mut file = tokio::fs::File::create(&partial).await?;
    let mut stream = body.into_data_stream();
    let mut total: u64 = 0;
    while let Some(chunk) = stream.next().await {
        let Ok(chunk) = chunk else {
            drop(file);
            let _ = tokio::fs::remove_file(&partial).await;
            return Ok((StatusCode::BAD_REQUEST, "The upload was interrupted.").into_response());
        };
        total += chunk.len() as u64;
        if total > state.config.max_upload_bytes {
            drop(file);
            let _ = tokio::fs::remove_file(&partial).await;
            return Ok((StatusCode::PAYLOAD_TOO_LARGE, "That file is too large.").into_response());
        }
        file.write_all(&chunk).await?;
    }
    file.flush().await?;
    drop(file);
    tokio::fs::rename(&partial, dir.join(&final_name)).await?;

    Ok(Json(serde_json::json!({"name": final_name, "size": total})).into_response())
}

async fn files_download(
    State(state): State<AppState>,
    CurrentUser(user): CurrentUser,
    Path(name): Path<String>,
) -> Result<Response, AppError> {
    // Only a plain file name that survives sanitising unchanged is accepted,
    // so nothing outside the user's folder can be named.
    if safe_file_name(&name).as_deref() != Some(name.as_str()) {
        return Ok(StatusCode::NOT_FOUND.into_response());
    }
    let path = user_files_dir(&state, &user).await?.join(&name);
    let Ok(meta) = tokio::fs::metadata(&path).await else {
        return Ok(StatusCode::NOT_FOUND.into_response());
    };
    if !meta.is_file() {
        return Ok(StatusCode::NOT_FOUND.into_response());
    }
    let file = tokio::fs::File::open(&path).await?;
    let encoded: String = name
        .bytes()
        .map(|b| {
            if b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.') {
                (b as char).to_string()
            } else {
                format!("%{b:02X}")
            }
        })
        .collect();
    let response = Response::builder()
        .header(header::CONTENT_TYPE, "application/octet-stream")
        .header(header::CONTENT_LENGTH, meta.len())
        .header(header::CONTENT_DISPOSITION, format!("attachment; filename*=UTF-8''{encoded}"))
        .header("X-Content-Type-Options", "nosniff")
        .body(Body::from_stream(tokio_util::io::ReaderStream::new(file)))
        .map_err(|e| anyhow::anyhow!("building download response: {e}"))?;
    Ok(response)
}
