//! Admin API, used by the Access Portal app on the host (`admin-app/`).
//!
//! These routes are never served on the network: the app calls them in-process
//! (see `Portal::admin` in `lib.rs`), so the admin panel can only be used on the
//! host itself. Every answer is JSON and every failure is `{"error": "..."}`
//! with a matching status code.

use async_trait::async_trait;
use axum::extract::{FromRequestParts, Path, State};
use axum::http::request::Parts;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::routing::{delete, get, post, put};
use axum::{Json, Router};
use serde::Deserialize;
use serde_json::{json, Value};

use crate::models::User;
use crate::{auth, db, host, AppState};

/// The admin routes. Not for the network listener: see the module notes.
pub fn router() -> Router<AppState> {
    Router::new()
        .route("/api/admin/state", get(state))
        .route("/api/admin/users", post(create_user))
        .route("/api/admin/users/:id", delete(delete_user))
        .route("/api/admin/users/:id/apps", put(set_apps))
        .route("/api/admin/users/:id/windows", put(set_windows_login))
        .route("/api/admin/users/:id/password", put(set_password))
        .route("/api/admin/users/:id/unlock", post(unlock))
        .route("/api/admin/users/:id/totp", put(set_totp))
        .route("/api/admin/users/:id/desktop", get(desktop_mode).put(set_desktop_mode))
        .route("/api/admin/users/:id/windows-account", post(create_windows_account))
        .route("/api/admin/users/:id/sign-out", post(sign_out_user))
        .route("/api/admin/connections/:id/disconnect", post(disconnect))
        .route("/api/admin/apps", post(create_app))
        .route("/api/admin/apps/:id", delete(delete_app))
}

// ---------------------------------------------------------------------------
// Errors and the admin extractor
// ---------------------------------------------------------------------------

pub struct ApiError(StatusCode, String);

fn fail(status: StatusCode, message: &str) -> ApiError {
    ApiError(status, message.to_string())
}

fn bad(message: &str) -> ApiError {
    fail(StatusCode::BAD_REQUEST, message)
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        (self.0, Json(json!({ "error": self.1 }))).into_response()
    }
}

impl From<anyhow::Error> for ApiError {
    fn from(e: anyhow::Error) -> Self {
        tracing::error!("api request failed: {e:#}");
        fail(StatusCode::INTERNAL_SERVER_ERROR, "Something went wrong on the server.")
    }
}

type ApiResult = Result<Json<Value>, ApiError>;

/// Marks a request as coming from the app on the host. It is attached as a
/// request extension, which nothing arriving over the network can carry.
#[derive(Clone, Copy)]
pub struct LocalAdmin;

/// Proof that a request came from the app; anything else is refused.
struct Admin;

#[async_trait]
impl FromRequestParts<AppState> for Admin {
    type Rejection = ApiError;

    async fn from_request_parts(parts: &mut Parts, _state: &AppState) -> Result<Self, ApiError> {
        match parts.extensions.get::<LocalAdmin>() {
            Some(_) => Ok(Admin),
            None => Err(fail(StatusCode::FORBIDDEN, "The admin panel is only available on the host.")),
        }
    }
}

async fn existing_user(state: &AppState, id: i64) -> Result<User, ApiError> {
    db::user_by_id(&state.pool, id)
        .await?
        .ok_or_else(|| fail(StatusCode::NOT_FOUND, "That user no longer exists."))
}

// ---------------------------------------------------------------------------
// Everything the app shows, in one read
// ---------------------------------------------------------------------------

async fn state(State(state): State<AppState>, _: Admin) -> ApiResult {
    let now = db::now();
    let open = state.presence.snapshot();
    let mut users = Vec::new();
    for u in db::list_users(&state.pool).await? {
        let app_ids: Vec<i64> = db::apps_for_user(&state.pool, u.id)
            .await?
            .iter()
            .map(|a| a.id)
            .collect();
        users.push(json!({
            "id": u.id,
            "username": u.username,
            "is_admin": u.is_admin(),
            "totp": u.totp_enabled(),
            "win_username": u.win_username,
            "locked": u.locked_until.is_some_and(|until| until > now),
            "failed_count": u.failed_count,
            "created_at": u.created_at,
            "app_ids": app_ids,
            "sessions": open.iter().filter(|c| c.user_id == u.id).collect::<Vec<_>>(),
        }));
    }
    let apps: Vec<Value> = db::list_apps(&state.pool)
        .await?
        .into_iter()
        .map(|a| json!({ "id": a.id, "key": a.key, "name": a.name, "hostname": a.hostname, "port": a.port }))
        .collect();
    Ok(Json(json!({
        "listen": state.config.bind_addr,
        "users": users,
        "apps": apps,
    })))
}

// ---------------------------------------------------------------------------
// Users
// ---------------------------------------------------------------------------

#[derive(Deserialize)]
struct NewUser {
    username: String,
    password: String,
    #[serde(default)]
    win_username: String,
    #[serde(default)]
    win_password: String,
    #[serde(default)]
    is_admin: bool,
    #[serde(default)]
    enable_totp: bool,
    #[serde(default)]
    app_ids: Vec<i64>,
    /// Make a new Windows account for the user (named `win_username`, or the
    /// username) with a random password nobody needs to know.
    #[serde(default)]
    create_windows_account: bool,
}

/// A long random Windows password that also meets Windows' complexity rules.
fn new_windows_password() -> String {
    format!("{}aA1!", auth::random_windows_password())
}

fn totp_details(secret: &str, username: &str) -> Value {
    json!({
        "secret": secret,
        "uri": auth::totp_provisioning_uri(secret, username).unwrap_or_default(),
    })
}

async fn create_user(
    State(state): State<AppState>,
    _: Admin,
    Json(input): Json<NewUser>,
) -> ApiResult {
    let username = input.username.trim();
    if username.is_empty() || input.password.is_empty() {
        return Err(bad("Username and password are required."));
    }
    if db::user_by_username(&state.pool, username).await?.is_some() {
        return Err(fail(StatusCode::CONFLICT, "That username is already taken."));
    }
    let win_username = match input.win_username.trim() {
        "" => username,
        w => w,
    };
    let win_password = if input.create_windows_account {
        // The account is made first, so a failure leaves nothing half done.
        let password = new_windows_password();
        host::create_account(win_username, &password).await.map_err(host_error)?;
        password
    } else if input.win_password.is_empty() {
        auth::random_windows_password()
    } else {
        input.win_password
    };

    let (id, secret) = db::create_user(
        &state.pool,
        &state.config,
        username,
        &input.password,
        win_username,
        &win_password,
        input.is_admin,
        input.enable_totp,
    )
    .await?;
    db::set_user_apps(&state.pool, id, &input.app_ids).await?;

    Ok(Json(json!({
        "id": id,
        "totp": secret.map(|s| totp_details(&s, username)),
    })))
}

#[derive(Deserialize)]
struct DeleteOptions {
    /// Also delete the user's Windows account (only ones the portal manages).
    #[serde(default)]
    windows: Option<u8>,
}

async fn delete_user(
    State(state): State<AppState>,
    _: Admin,
    Path(id): Path<i64>,
    axum::extract::Query(options): axum::extract::Query<DeleteOptions>,
) -> ApiResult {
    let user = existing_user(&state, id).await?;
    if options.windows == Some(1) {
        host::delete_account(&user.win_username).await.map_err(host_error)?;
    }
    db::delete_user(&state.pool, id).await?;
    Ok(Json(json!({})))
}

/// Make the Windows account a portal user is set to sign in as, when it does
/// not exist yet, with a new random password.
async fn create_windows_account(
    State(state): State<AppState>,
    _: Admin,
    Path(id): Path<i64>,
) -> ApiResult {
    let user = existing_user(&state, id).await?;
    let password = new_windows_password();
    host::create_account(&user.win_username, &password).await.map_err(host_error)?;
    db::set_windows_login(&state.pool, &state.config, id, &user.win_username, &password).await?;
    tracing::info!("created windows account {} for portal user {}", user.win_username, user.username);
    Ok(Json(json!({ "mode": "app" })))
}

#[derive(Deserialize)]
struct AppIds {
    #[serde(default)]
    app_ids: Vec<i64>,
}

async fn set_apps(
    State(state): State<AppState>,
    _: Admin,
    Path(id): Path<i64>,
    Json(input): Json<AppIds>,
) -> ApiResult {
    existing_user(&state, id).await?;
    db::set_user_apps(&state.pool, id, &input.app_ids).await?;
    Ok(Json(json!({})))
}

#[derive(Deserialize)]
struct WindowsLogin {
    win_username: String,
    win_password: String,
}

async fn set_windows_login(
    State(state): State<AppState>,
    _: Admin,
    Path(id): Path<i64>,
    Json(input): Json<WindowsLogin>,
) -> ApiResult {
    if input.win_username.trim().is_empty() || input.win_password.is_empty() {
        return Err(bad("Windows username and password are both required."));
    }
    existing_user(&state, id).await?;
    db::set_windows_login(
        &state.pool,
        &state.config,
        id,
        input.win_username.trim(),
        &input.win_password,
    )
    .await?;
    Ok(Json(json!({})))
}

#[derive(Deserialize)]
struct NewPassword {
    password: String,
}

/// Sets a new portal password and ends the user's open portal logins.
async fn set_password(
    State(state): State<AppState>,
    _: Admin,
    Path(id): Path<i64>,
    Json(input): Json<NewPassword>,
) -> ApiResult {
    if input.password.is_empty() {
        return Err(bad("The new password can't be empty."));
    }
    existing_user(&state, id).await?;
    db::set_password(&state.pool, id, &input.password, true).await?;
    Ok(Json(json!({})))
}

async fn unlock(
    State(state): State<AppState>,
    _: Admin,
    Path(id): Path<i64>,
) -> ApiResult {
    existing_user(&state, id).await?;
    db::reset_failed_login(&state.pool, id).await?;
    Ok(Json(json!({})))
}

#[derive(Deserialize)]
struct TotpSwitch {
    enabled: bool,
}

/// Turns two-step verification on (a fresh secret, returned once) or off.
async fn set_totp(
    State(state): State<AppState>,
    _: Admin,
    Path(id): Path<i64>,
    Json(input): Json<TotpSwitch>,
) -> ApiResult {
    let user = existing_user(&state, id).await?;
    let secret = input.enabled.then(auth::new_totp_secret);
    db::set_totp_secret(&state.pool, id, secret.as_deref()).await?;
    Ok(Json(json!({
        "totp": secret.map(|s| totp_details(&s, &user.username)),
    })))
}

/// Errors from the host script are written for people, so pass them on as-is.
fn host_error(e: anyhow::Error) -> ApiError {
    tracing::warn!("windows account change failed: {e:#}");
    fail(StatusCode::CONFLICT, &format!("{e}"))
}

/// Whether the user's Windows account gets the full desktop or Miracle only:
/// `{"mode": "app" | "full" | "missing"}`.
async fn desktop_mode(
    State(state): State<AppState>,
    _: Admin,
    Path(id): Path<i64>,
) -> ApiResult {
    let user = existing_user(&state, id).await?;
    let mode = host::desktop_mode(&user.win_username).await.map_err(host_error)?;
    Ok(Json(json!({ "mode": mode })))
}

#[derive(Deserialize)]
struct DesktopSwitch {
    full: bool,
}

async fn set_desktop_mode(
    State(state): State<AppState>,
    _: Admin,
    Path(id): Path<i64>,
    Json(input): Json<DesktopSwitch>,
) -> ApiResult {
    let user = existing_user(&state, id).await?;
    let mode = host::set_desktop_mode(&user.win_username, input.full)
        .await
        .map_err(host_error)?;
    if mode == "missing" {
        return Err(fail(
            StatusCode::CONFLICT,
            &format!("There is no Windows account named {} on this computer.", user.win_username),
        ));
    }
    tracing::info!(
        "windows account {} of portal user {} set to {mode}",
        user.win_username,
        user.username
    );
    Ok(Json(json!({ "mode": mode })))
}

/// Disconnect one open session. The user's apps keep running in Windows and the
/// user can connect again.
async fn disconnect(State(state): State<AppState>, _: Admin, Path(id): Path<u64>) -> ApiResult {
    if !state.presence.kick(id, crate::presence::DISCONNECTED) {
        return Err(fail(StatusCode::NOT_FOUND, "That session already ended."));
    }
    Ok(Json(json!({})))
}

/// Sign a user out completely: disconnect their sessions, end their portal
/// sign-ins and sign their Windows account out (their apps close).
async fn sign_out_user(State(state): State<AppState>, _: Admin, Path(id): Path<i64>) -> ApiResult {
    let user = existing_user(&state, id).await?;
    let disconnected = state.presence.kick_user(id, crate::presence::SIGNED_OUT);
    db::delete_sessions_for_user(&state.pool, id).await?;
    // Give the sessions a moment to close before Windows signs the account out.
    tokio::time::sleep(std::time::Duration::from_millis(300)).await;
    let win = user.win_username.clone();
    let windows_sessions = tokio::task::spawn_blocking(move || host::sign_out_blocking(&win)).await.unwrap_or(0);
    Ok(Json(json!({ "disconnected": disconnected, "windows_sessions": windows_sessions })))
}

// ---------------------------------------------------------------------------
// Apps
// ---------------------------------------------------------------------------

#[derive(Deserialize)]
struct NewApp {
    name: String,
    key: String,
    hostname: String,
    #[serde(default)]
    port: String,
}

async fn create_app(
    State(state): State<AppState>,
    _: Admin,
    Json(input): Json<NewApp>,
) -> ApiResult {
    let (name, key, hostname) = (input.name.trim(), input.key.trim(), input.hostname.trim());
    if name.is_empty() || key.is_empty() || hostname.is_empty() {
        return Err(bad("Name, key and host are required."));
    }
    if !key.chars().all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_') {
        return Err(bad("The key may only contain letters, digits, '-' and '_'."));
    }
    let port = match input.port.trim() {
        "" => "3389",
        p if p.parse::<u16>().is_ok() => p,
        _ => return Err(bad("The port must be a number between 1 and 65535.")),
    };
    if db::app_by_key(&state.pool, key).await?.is_some() {
        return Err(fail(StatusCode::CONFLICT, "That key is already used by another app."));
    }
    let id = db::create_app(&state.pool, key, name, hostname, port).await?;
    Ok(Json(json!({ "id": id })))
}

async fn delete_app(
    State(state): State<AppState>,
    _: Admin,
    Path(id): Path<i64>,
) -> ApiResult {
    db::delete_app(&state.pool, id).await?;
    Ok(Json(json!({})))
}
