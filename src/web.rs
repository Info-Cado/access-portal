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

use crate::activity::Actor;
use crate::error::AppError;
use crate::models::User;
use crate::{access, activity, auth, db, rdp, settings, transport, AppState};

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
        .layer(axum::middleware::from_fn(refuse_cross_site))
        .with_state(state)
}

/// Browsers send `Origin` with every POST; refuse any that names another site,
/// so a page elsewhere cannot submit forms (sign in, sign out, uploads) into the
/// portal with the user's cookies.
async fn refuse_cross_site(request: axum::extract::Request, next: axum::middleware::Next) -> Response {
    if request.method() != axum::http::Method::GET
        && request.method() != axum::http::Method::HEAD
        && !same_origin(request.headers())
    {
        return (StatusCode::FORBIDDEN, "Cross-site request refused.").into_response();
    }
    next.run(request).await
}

// ---------------------------------------------------------------------------
// Sign-in limits per network address
// ---------------------------------------------------------------------------

/// Counts failed sign-ins per client address, on top of the per-account
/// lockout, so one address cannot try passwords across many accounts.
pub struct LoginLimiter {
    failures: std::sync::Mutex<std::collections::HashMap<String, (u32, std::time::Instant)>>,
}

const ADDRESS_MAX_FAILURES: u32 = 20;
const ADDRESS_WINDOW: std::time::Duration = std::time::Duration::from_secs(15 * 60);

impl LoginLimiter {
    pub fn new() -> Self {
        Self { failures: std::sync::Mutex::new(std::collections::HashMap::new()) }
    }

    /// True when this address has failed too often recently.
    fn blocked(&self, address: &str) -> bool {
        let mut map = self.failures.lock().unwrap();
        match map.get(address) {
            Some((_, since)) if since.elapsed() > ADDRESS_WINDOW => {
                map.remove(address);
                false
            }
            Some((count, _)) => *count >= ADDRESS_MAX_FAILURES,
            None => false,
        }
    }

    fn failed(&self, address: &str) {
        let mut map = self.failures.lock().unwrap();
        map.retain(|_, (_, since)| since.elapsed() <= ADDRESS_WINDOW);
        let entry = map.entry(address.to_string()).or_insert((0, std::time::Instant::now()));
        entry.0 += 1;
    }
}

/// The browser's address as reported by the HTTPS front end in front of the
/// portal (Cloudflare, then any other proxy). For browsers on the local network
/// or Tailscale, which connect directly, `network.rs` replaces these headers
/// with the connection's own address, so they can't be made up.
fn client_address(headers: &HeaderMap) -> String {
    let header = |name: &str| headers.get(name).and_then(|v| v.to_str().ok()).map(str::trim);
    header("cf-connecting-ip")
        .or_else(|| header("x-forwarded-for").and_then(|v| v.split(',').next()).map(str::trim))
        .filter(|v| !v.is_empty())
        .unwrap_or("local")
        .to_string()
}

/// The country Cloudflare places the browser in (two letters), if it said.
fn client_country(headers: &HeaderMap) -> Option<String> {
    headers
        .get("cf-ipcountry")
        .and_then(|v| v.to_str().ok())
        .map(|v| v.trim().to_ascii_uppercase())
        .filter(|v| v.len() == 2 && v != "XX")
}

/// Whether `user` may use the portal now, from where this request came from.
fn allowed(user: &User, headers: &HeaderMap) -> Result<(), String> {
    let address = client_address(headers);
    let country = client_country(headers);
    user.rules().check(&access::Origin { address: &address, country: country.as_deref() })
}

/// True when the browser reached the portal over HTTPS through a front end.
fn behind_https(headers: &HeaderMap) -> bool {
    headers
        .get("x-forwarded-proto")
        .and_then(|v| v.to_str().ok())
        .is_some_and(|v| v.eq_ignore_ascii_case("https"))
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

/// Cookies are marked Secure (HTTPS only) whenever the browser came in over
/// HTTPS, or always with `COOKIE_SECURE=true`.
fn cookie(state: &AppState, headers: &HeaderMap, name: &'static str, token: String) -> Cookie<'static> {
    Cookie::build((name, token))
        .path("/")
        .http_only(true)
        .same_site(SameSite::Lax)
        .secure(state.config.cookie_secure || behind_https(headers))
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
    let desktop = full_desktop(&user).await;

    // A regular user with only one thing to open has nothing to choose: open it directly.
    if !user.is_admin() {
        match (apps.as_slice(), desktop) {
            ([], true) => return Ok(Redirect::to(&format!("/connect/{DESKTOP_KEY}")).into_response()),
            ([app], false) => return Ok(Redirect::to(&format!("/connect/{}", app.key)).into_response()),
            _ => {}
        }
    }

    let mut list = String::new();
    if apps.is_empty() && !desktop {
        list.push_str(
            "<div class=\"card\"><strong>Nothing is assigned to you</strong>\
             <p class=\"muted\">Ask your administrator to give you access to an app or the desktop.</p></div>",
        );
    }
    if desktop {
        list.push_str(&format!(
            "<div class=\"card app\"><div><strong>Desktop</strong></div>\
             <a class=\"btn\" href=\"/connect/{DESKTOP_KEY}\" target=\"_blank\" rel=\"noopener\">Open</a></div>"
        ));
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

/// The key under which a user with the full desktop opens it, in the same
/// links as apps (`/connect/desktop`, `/ws/desktop`). No app may use it.
pub const DESKTOP_KEY: &str = "desktop";

/// Whether the user's Windows account has the full desktop switched on, so the
/// portal offers them "Desktop". False when it can't be told (no account, or
/// the portal is not running as an administrator).
async fn full_desktop(user: &User) -> bool {
    match crate::host::desktop_mode_cached(&user.win_username).await {
        Ok(mode) => mode == "full",
        Err(e) => {
            tracing::warn!("could not read the desktop mode of {}: {e:#}", user.win_username);
            false
        }
    }
}

/// The app `key` names, if `user` may open it: "Desktop" (this computer, see
/// `DESKTOP_HOST`) when the key is [`DESKTOP_KEY`] and their account has the
/// full desktop, else an app assigned to them.
async fn allowed_app(state: &AppState, user: &User, key: &str) -> Result<Result<crate::models::App, StatusCode>, AppError> {
    if key == DESKTOP_KEY {
        if !full_desktop(user).await {
            return Ok(Err(StatusCode::FORBIDDEN));
        }
        let (hostname, port) = state.config.desktop_host.clone();
        return Ok(Ok(crate::models::App {
            id: 0,
            key: DESKTOP_KEY.to_string(),
            name: "Desktop".to_string(),
            hostname,
            port,
            created_at: 0,
        }));
    }
    let Some(app) = db::app_by_key(&state.pool, key).await? else {
        return Ok(Err(StatusCode::NOT_FOUND));
    };
    if !db::user_can_access(&state.pool, user.id, app.id).await? {
        return Ok(Err(StatusCode::FORBIDDEN));
    }
    Ok(Ok(app))
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
    headers: HeaderMap,
    Form(input): Form<LoginInput>,
) -> Result<Response, AppError> {
    let address = client_address(&headers);
    if state.limiter.blocked(&address) {
        return Ok(login_page(Some("Too many failed sign-ins from your network. Try again in 15 minutes.")).into_response());
    }
    let user = db::user_by_username(&state.pool, input.username.trim()).await?;

    let Some(user) = user else {
        // Same message whether or not the user exists.
        state.limiter.failed(&address);
        let typed: String = input.username.trim().chars().take(64).collect();
        activity::log(&state.pool, None, &typed, "signin_failed", "Unknown username", &address).await;
        return Ok(login_page(Some("Invalid username or password.")).into_response());
    };
    let actor = Actor::new(&state.pool, &user, &address);

    if let Some(until) = user.locked_until {
        if until > db::now() {
            actor.log("signin_failed", "Account locked after too many attempts").await;
            return Ok(login_page(Some(
                "Account temporarily locked after too many attempts. Try again later.",
            ))
            .into_response());
        }
    }

    if !auth::verify_password(&input.password, &user.password_hash) {
        state.limiter.failed(&address);
        db::note_failed_login(&state.pool, &state.config, user.id).await?;
        actor.log("signin_failed", "Wrong password").await;
        return Ok(login_page(Some("Invalid username or password.")).into_response());
    }

    db::reset_failed_login(&state.pool, user.id).await?;

    if let Err(why) = allowed(&user, &headers) {
        actor.log("signin_denied", &why).await;
        return Ok(login_page(Some(&why)).into_response());
    }

    if user.totp_enabled() {
        let token = db::create_pending(&state.pool, user.id).await?;
        let jar = jar.add(cookie(&state, &headers, "pending", token));
        return Ok((jar, Redirect::to("/login/totp")).into_response());
    }

    let token = db::create_session(&state.pool, &state.config, user.id).await?;
    actor.log("signin", "Signed in").await;
    let jar = jar.add(cookie(&state, &headers, "session", token));
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
    headers: HeaderMap,
    Form(input): Form<TotpInput>,
) -> Result<Response, AppError> {
    let address = client_address(&headers);
    if state.limiter.blocked(&address) {
        return Ok(totp_page(Some("Too many failed sign-ins from your network. Try again in 15 minutes.")).into_response());
    }
    let Some(token) = jar.get("pending").map(|c| c.value().to_string()) else {
        return Ok(Redirect::to("/login").into_response());
    };
    let Some(user_id) = db::peek_pending(&state.pool, &token).await? else {
        return Ok(Redirect::to("/login").into_response());
    };
    let Some(user) = db::user_by_id(&state.pool, user_id).await? else {
        return Ok(Redirect::to("/login").into_response());
    };

    let actor = Actor::new(&state.pool, &user, &address);
    let secret = user.totp_secret.clone().unwrap_or_default();
    if !auth::verify_totp(&secret, &input.code) {
        state.limiter.failed(&address);
        actor.log("signin_failed", "Wrong two-step code").await;
        return Ok(totp_page(Some("Incorrect code. Try again.")).into_response());
    }

    db::delete_pending(&state.pool, &token).await?;
    let session = db::create_session(&state.pool, &state.config, user.id).await?;
    actor.log("signin", "Signed in (with two-step code)").await;
    let jar = jar
        .remove(removal_cookie("pending"))
        .add(cookie(&state, &headers, "session", session));
    Ok((jar, Redirect::to("/")).into_response())
}

async fn logout(
    State(state): State<AppState>,
    jar: CookieJar,
    headers: HeaderMap,
) -> Result<Response, AppError> {
    if let Some(token) = jar.get("session").map(|c| c.value().to_string()) {
        if let Ok(Some(user)) = db::user_by_session(&state.pool, &token).await {
            Actor::new(&state.pool, &user, &client_address(&headers)).log("signout", "Signed out of the portal").await;
        }
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
    headers: HeaderMap,
) -> Result<Response, AppError> {
    if let Err(why) = allowed(&user, &headers) {
        let body = format!(
            "<h1>Not available right now</h1><div class=\"err\">{}</div>\
             <form method=\"post\" action=\"/logout\"><button class=\"btn sec\">Sign out</button></form>",
            esc(&why)
        );
        return Ok((StatusCode::FORBIDDEN, page("Not available", &body)).into_response());
    }
    let app = match allowed_app(&state, &user, &key).await? {
        Ok(app) => app,
        Err(StatusCode::NOT_FOUND) => {
            return Ok((StatusCode::NOT_FOUND, Html("<h1>Unknown app</h1>".to_string())).into_response())
        }
        Err(status) => {
            return Ok((status, Html("<h1>You don't have access to this app</h1>".to_string())).into_response())
        }
    };

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
    /// "aac" when the browser can decode AAC sound.
    ad: Option<String>,
    /// 1: open the session at the size in `w` and `h` (the page chose it: the
    /// window's size, or smaller on a slow link) instead of the fixed size.
    sz: Option<u8>,
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

    if let Err(why) = allowed(&user, &headers) {
        // Say why on the page instead of failing silently.
        return Ok(upgrade.on_upgrade(move |mut socket| async move {
            use axum::extract::ws::Message;
            let _ = socket.send(Message::Text(serde_json::json!({"t": "closed", "m": why}).to_string())).await;
            tokio::time::sleep(std::time::Duration::from_millis(300)).await;
        }));
    }
    let target = match session_target(&state, &user, &key, &viewport, &headers).await? {
        Ok(target) => target,
        Err(status) => return Ok(status.into_response()),
    };
    let connection = connection_info(&state, &user, &key, &headers, "relayed").await;
    let presence = state.presence.clone();
    let health = state.health.clone();
    Ok(upgrade.on_upgrade(move |socket| async move {
        let listed = presence.enter(connection.clone());
        run_logged(target, listed, connection, health, Box::new(transport::WsTransport(socket))).await;
    }))
}

/// Run a session with its presence entry, and note its start and end in the
/// activity log.
async fn run_logged(
    mut target: rdp::Target,
    mut listed: crate::presence::Guard,
    connection: crate::presence::Connection,
    health: std::sync::Arc<crate::Health>,
    io: Box<dyn transport::Transport>,
) {
    target.kick = Some(listed.kick.clone());
    target.notices = listed.notices.take();
    let actor = target.actor.clone();
    if let Some(a) = &actor {
        a.log("session_start", &format!("Opened {} on {} ({})", connection.app, connection.device, connection.link)).await;
    }
    let started = db::now();
    let (reason, failure) = rdp::run(target, io).await;
    if let Some(why) = &failure {
        health.session_failed(why);
    }
    if let Some(a) = &actor {
        match &failure {
            Some(why) => {
                a.log("session_failed", &format!("{} failed after {}: {why}", connection.app, activity::duration(db::now() - started)))
                    .await
            }
            None => {
                let why = match reason.as_str() {
                    "browser closed" => "the browser was closed".to_string(),
                    other => other.trim_end_matches('.').to_string(),
                };
                a.log(
                    "session_end",
                    &format!("Closed {} after {}: {why}", connection.app, activity::duration(db::now() - started)),
                )
                .await
            }
        }
    }
    drop(listed);
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
    if !same_origin(&headers) || allowed(&user, &headers).is_err() {
        // A refused direct link falls back to the WebSocket, which says why.
        return Ok(StatusCode::FORBIDDEN.into_response());
    }
    let target = match session_target(&state, &user, &key, &viewport, &headers).await? {
        Ok(target) => target,
        Err(status) => return Ok(status.into_response()),
    };
    let username = target.username.clone();
    let connection = connection_info(&state, &user, &key, &headers, "direct").await;
    let presence = state.presence.clone();
    let health = state.health.clone();
    let servers = vec![webrtc::peer_connection::RTCIceServer {
        urls: state.config.ice_urls.clone(),
        username: state.config.ice_username.clone(),
        credential: state.config.ice_credential.clone(),
        ..Default::default()
    }];
    let on_open: transport::OnOpen = Box::new(move |link| {
        tokio::spawn(async move {
            let listed = presence.enter(connection.clone());
            run_logged(target, listed, connection, health, Box::new(link)).await;
        });
    });
    match transport::answer_offer(offer, servers, on_open).await {
        Ok(answer) => Ok(Json(answer).into_response()),
        Err(e) => {
            tracing::warn!("direct link setup failed for {username}: {e:#}");
            Ok(StatusCode::BAD_GATEWAY.into_response())
        }
    }
}

/// What the admin panel shows about an open session.
async fn connection_info(
    state: &AppState,
    user: &User,
    key: &str,
    headers: &HeaderMap,
    link: &'static str,
) -> crate::presence::Connection {
    let app = if key == DESKTOP_KEY {
        "Desktop".to_string()
    } else {
        db::app_by_key(&state.pool, key)
            .await
            .ok()
            .flatten()
            .map(|a| a.name)
            .unwrap_or_else(|| key.to_string())
    };
    crate::presence::Connection {
        id: 0,
        user_id: user.id,
        app,
        since: db::now(),
        address: client_address(headers),
        device: crate::presence::device(headers),
        link,
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
    let fixed = config.session_size.map(|(w, h)| serde_json::json!([w, h]));
    serde_json::json!({ "p2p": config.p2p, "iceServers": servers, "fixed": fixed })
        .to_string()
        .replace("</", "<\\/")
}

/// Check that `user` may open app `key` and work out where and how to connect.
async fn session_target(
    state: &AppState,
    user: &User,
    key: &str,
    viewport: &ViewportQuery,
    headers: &HeaderMap,
) -> Result<Result<rdp::Target, StatusCode>, AppError> {
    let app = match allowed_app(state, user, key).await? {
        Ok(app) => app,
        Err(status) => return Ok(Err(status)),
    };

    let password = auth::decrypt_at_rest(&state.config.portal_secret, &user.win_password)?;
    // A fixed size (the default, 1920x1080) is scaled to fit by the browser;
    // otherwise the session takes the size of the browser window.
    let (width, height) = match (state.config.session_size, viewport.sz) {
        (Some((w, h)), None) => rdp::sane_size(u32::from(w), u32::from(h)),
        _ => rdp::sane_size(viewport.w.unwrap_or(1280), viewport.h.unwrap_or(800)),
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
        // The page asks for a new size when the window changes (with "Fit to
        // window" on) or the link gets slow or fast again.
        resizable: true,
        // The browser advertises H.264 decode support with ?vd=1.
        video_ok: state.config.video && matches!(viewport.vd, Some(1 | 2)),
        files_dir: user_files_dir(state, user).await.ok(),
        // The browser advertises it can decode Windows' H.264 (High profile) with ?vd=2.
        egfx: state.config.egfx && viewport.vd == Some(2),
        kick: None,
        notices: None,
        idle: match settings::sessions(&state.pool).await.idle_minutes {
            0 => None,
            m => Some(std::time::Duration::from_secs(u64::from(m) * 60)),
        },
        rules: Some(user.rules()).filter(|r| r.from.is_some() || r.days.is_some()),
        actor: Some(Actor::new(&state.pool, user, &client_address(headers))),
        clip_dir: clipboard_dir(state, user).await.ok(),
        sound_aac: viewport.ad.as_deref() == Some("aac"),
    };
    Ok(Ok(target))
}

/// True unless the request names an `Origin` other than the portal itself.
fn same_origin(headers: &HeaderMap) -> bool {
    let Some(origin) = headers.get(header::ORIGIN).and_then(|v| v.to_str().ok()) else {
        return true;
    };
    let origin_host = origin.split("://").nth(1).unwrap_or("");
    // The Host the portal sees, or the public one a front end reports.
    [header::HOST.as_str(), "x-forwarded-host"]
        .iter()
        .filter_map(|name| headers.get(*name).and_then(|v| v.to_str().ok()))
        .any(|host| !host.is_empty() && host.eq_ignore_ascii_case(origin_host))
}

// ---------------------------------------------------------------------------
// Files: each user has one transfer folder on the portal host. Sessions see it
// as the "Portal" drive; the browser can upload into it and download from it.
// ---------------------------------------------------------------------------

/// Reduce an untrusted name to a single safe Windows file name, or `None`.
pub(crate) fn safe_file_name(name: &str) -> Option<String> {
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

/// Where files and pictures the user pastes into a session wait for Windows to
/// read them. Outside the user's folder, so they don't show up in it.
async fn clipboard_dir(state: &AppState, user: &User) -> anyhow::Result<std::path::PathBuf> {
    let name = safe_file_name(&user.username)
        .ok_or_else(|| anyhow::anyhow!("username cannot be used as a folder name"))?;
    let dir = state.config.files_dir.join(".clipboard").join(name);
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
        if name.starts_with(".upload-") || name.ends_with(".uploading") {
            let abandoned = meta.modified().ok().and_then(|t| t.elapsed().ok()).is_some_and(|age| age > STALE_UPLOAD);
            if abandoned {
                let _ = tokio::fs::remove_file(entry.path()).await;
            }
            continue;
        }
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
    /// Upload id chosen by the browser; the same for every chunk of one file.
    /// Absent for a file sent in one request.
    id: Option<String>,
    /// Where this chunk starts in the file.
    #[serde(default)]
    offset: u64,
    /// 1 on the file's last chunk.
    #[serde(default)]
    last: Option<u8>,
    /// A clipboard batch id: the file is being pasted into a session, so it goes
    /// to the clipboard folder instead of the user's folder.
    clip: Option<String>,
}

/// Largest single chunk accepted. Kept well under the 100 MB request limit of
/// Cloudflare's free plan, which sits in front of public portals.
const MAX_CHUNK_BYTES: u64 = 32 * 1024 * 1024;
/// Half-sent uploads older than this are removed.
const STALE_UPLOAD: std::time::Duration = std::time::Duration::from_secs(24 * 60 * 60);

/// Receive a file from the browser into the user's folder, in one request or in
/// chunks (`id`, `offset`, `last=1` on the final one). Chunks are appended to a
/// hidden `.upload-<id>` file that only becomes visible once complete.
async fn files_upload(
    State(state): State<AppState>,
    CurrentUser(user): CurrentUser,
    Query(query): Query<UploadQuery>,
    headers: HeaderMap,
    body: Body,
) -> Result<Response, AppError> {
    let Some(name) = safe_file_name(&query.name) else {
        return Ok((StatusCode::BAD_REQUEST, "That file name can't be used.").into_response());
    };
    let single = query.id.is_none();
    let id = match &query.id {
        Some(id) if (8..=64).contains(&id.len()) && id.bytes().all(|b| b.is_ascii_hexdigit()) => id.clone(),
        Some(_) => return Ok((StatusCode::BAD_REQUEST, "Bad upload id.").into_response()),
        None => auth::random_token(),
    };
    let last = single || query.last == Some(1);
    let dir = match &query.clip {
        None => user_files_dir(&state, &user).await?,
        Some(batch) if (8..=64).contains(&batch.len()) && batch.bytes().all(|b| b.is_ascii_hexdigit()) => {
            let dir = clipboard_dir(&state, &user).await?.join(batch);
            tokio::fs::create_dir_all(&dir).await?;
            dir
        }
        Some(_) => return Ok((StatusCode::BAD_REQUEST, "Bad clipboard id.").into_response()),
    };
    let partial = dir.join(format!(".upload-{id}"));

    // A chunk must continue exactly where the file got to.
    let have = match tokio::fs::metadata(&partial).await {
        Ok(meta) => meta.len(),
        Err(_) => 0,
    };
    if query.offset != have {
        return Ok((StatusCode::CONFLICT, Json(serde_json::json!({"error": "out of order", "have": have}))).into_response());
    }
    let mut file = tokio::fs::OpenOptions::new().create(true).append(true).open(&partial).await?;

    let chunk_limit = if single { state.config.max_upload_bytes } else { MAX_CHUNK_BYTES };
    let mut stream = body.into_data_stream();
    let mut received: u64 = 0;
    while let Some(piece) = stream.next().await {
        let Ok(piece) = piece else {
            drop(file);
            if single {
                let _ = tokio::fs::remove_file(&partial).await;
            }
            return Ok((StatusCode::BAD_REQUEST, "The upload was interrupted.").into_response());
        };
        received += piece.len() as u64;
        if received > chunk_limit || have + received > state.config.max_upload_bytes {
            drop(file);
            let _ = tokio::fs::remove_file(&partial).await;
            return Ok((StatusCode::PAYLOAD_TOO_LARGE, "That file is too large.").into_response());
        }
        file.write_all(&piece).await?;
    }
    file.flush().await?;
    drop(file);
    let total = have + received;
    if !last {
        return Ok(Json(serde_json::json!({"have": total})).into_response());
    }

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
    tokio::fs::rename(&partial, dir.join(&final_name)).await?;
    if query.clip.is_none() {
        Actor::new(&state.pool, &user, &client_address(&headers))
            .log("upload", &format!("Uploaded {final_name} ({})", size_text(total)))
            .await;
    }
    Ok(Json(serde_json::json!({"name": final_name, "size": total})).into_response())
}

#[derive(Deserialize)]
struct DownloadQuery {
    /// 1 shows a PDF in the browser (to print it) instead of downloading it.
    inline: Option<u8>,
}

fn size_text(n: u64) -> String {
    match n {
        n if n < 1024 => format!("{n} B"),
        n if n < 1024 * 1024 => format!("{:.1} KB", n as f64 / 1024.0),
        n if n < 1024 * 1024 * 1024 => format!("{:.1} MB", n as f64 / 1048576.0),
        n => format!("{:.2} GB", n as f64 / 1073741824.0),
    }
}

async fn files_download(
    State(state): State<AppState>,
    CurrentUser(user): CurrentUser,
    Path(name): Path<String>,
    Query(query): Query<DownloadQuery>,
    headers: HeaderMap,
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
    let show_pdf = query.inline == Some(1) && name.to_ascii_lowercase().ends_with(".pdf");
    Actor::new(&state.pool, &user, &client_address(&headers))
        .spawn("download", format!("{} {name} ({})", if show_pdf { "Opened" } else { "Downloaded" }, size_text(meta.len())));
    let response = Response::builder()
        .header(header::CONTENT_TYPE, if show_pdf { "application/pdf" } else { "application/octet-stream" })
        .header(header::CONTENT_LENGTH, meta.len())
        .header(
            header::CONTENT_DISPOSITION,
            format!("{}; filename*=UTF-8''{encoded}", if show_pdf { "inline" } else { "attachment" }),
        )
        .header("X-Content-Type-Options", "nosniff")
        .body(Body::from_stream(tokio_util::io::ReaderStream::new(file)))
        .map_err(|e| anyhow::anyhow!("building download response: {e}"))?;
    Ok(response)
}
