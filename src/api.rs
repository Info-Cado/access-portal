//! JSON admin API, used by the desktop admin app (`admin-app/`).
//!
//! Same data and rules as the HTML admin page, but every answer is JSON and
//! every failure is `{"error": "..."}` with a matching status code. Requests
//! carry the normal `session` cookie; bodies must be `application/json`, which
//! a cross-site form cannot send, so the cookie cannot be abused from another
//! site.

use async_trait::async_trait;
use axum::extract::{FromRequestParts, Path, State};
use axum::http::request::Parts;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::routing::{delete, get, post, put};
use axum::{Json, Router};
use axum_extra::extract::cookie::CookieJar;
use serde::Deserialize;
use serde_json::{json, Value};

use crate::models::User;
use crate::{auth, db, web, AppState};

pub fn router() -> Router<AppState> {
    Router::new()
        .route("/api/login", post(login))
        .route("/api/logout", post(logout))
        .route("/api/admin/state", get(state))
        .route("/api/admin/users", post(create_user))
        .route("/api/admin/users/:id", delete(delete_user))
        .route("/api/admin/users/:id/apps", put(set_apps))
        .route("/api/admin/users/:id/windows", put(set_windows_login))
        .route("/api/admin/users/:id/password", put(set_password))
        .route("/api/admin/users/:id/unlock", post(unlock))
        .route("/api/admin/users/:id/totp", put(set_totp))
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

/// A signed-in administrator; anyone else gets a JSON 401/403.
struct Admin(User);

#[async_trait]
impl FromRequestParts<AppState> for Admin {
    type Rejection = ApiError;

    async fn from_request_parts(parts: &mut Parts, state: &AppState) -> Result<Self, ApiError> {
        let jar = CookieJar::from_headers(&parts.headers);
        let token = jar
            .get("session")
            .map(|c| c.value().to_string())
            .ok_or_else(|| fail(StatusCode::UNAUTHORIZED, "Not signed in."))?;
        let user = db::user_by_session(&state.pool, &token)
            .await?
            .ok_or_else(|| fail(StatusCode::UNAUTHORIZED, "Not signed in."))?;
        if !user.is_admin() {
            return Err(fail(StatusCode::FORBIDDEN, "Administrators only."));
        }
        Ok(Admin(user))
    }
}

async fn existing_user(state: &AppState, id: i64) -> Result<User, ApiError> {
    db::user_by_id(&state.pool, id)
        .await?
        .ok_or_else(|| fail(StatusCode::NOT_FOUND, "That user no longer exists."))
}

// ---------------------------------------------------------------------------
// Sign in / out
// ---------------------------------------------------------------------------

#[derive(Deserialize)]
struct LoginInput {
    username: String,
    password: String,
    /// 6-digit code, needed only when the account has two-step verification.
    code: Option<String>,
}

async fn login(
    State(state): State<AppState>,
    jar: CookieJar,
    Json(input): Json<LoginInput>,
) -> Result<Response, ApiError> {
    let denied = || fail(StatusCode::UNAUTHORIZED, "Invalid username or password.");
    let user = db::user_by_username(&state.pool, input.username.trim())
        .await?
        .ok_or_else(denied)?;

    if user.locked_until.is_some_and(|until| until > db::now()) {
        return Err(fail(
            StatusCode::TOO_MANY_REQUESTS,
            "Account temporarily locked after too many attempts. Try again later.",
        ));
    }
    if !auth::verify_password(&input.password, &user.password_hash) {
        db::note_failed_login(&state.pool, &state.config, user.id).await?;
        return Err(denied());
    }
    if !user.is_admin() {
        return Err(fail(StatusCode::FORBIDDEN, "Administrators only."));
    }
    if user.totp_enabled() {
        let code = input.code.as_deref().unwrap_or("").trim();
        if code.is_empty() {
            return Ok((
                StatusCode::UNAUTHORIZED,
                Json(json!({ "error": "Enter your two-step verification code.", "need_code": true })),
            )
                .into_response());
        }
        if !auth::verify_totp(user.totp_secret.as_deref().unwrap_or(""), code) {
            db::note_failed_login(&state.pool, &state.config, user.id).await?;
            return Ok((
                StatusCode::UNAUTHORIZED,
                Json(json!({ "error": "Incorrect code. Try again.", "need_code": true })),
            )
                .into_response());
        }
    }

    db::reset_failed_login(&state.pool, user.id).await?;
    let token = db::create_session(&state.pool, &state.config, user.id).await?;
    let jar = jar.add(web::session_cookie(&state, token));
    Ok((jar, Json(json!({ "username": user.username }))).into_response())
}

async fn logout(State(state): State<AppState>, jar: CookieJar) -> ApiResult {
    if let Some(token) = jar.get("session").map(|c| c.value().to_string()) {
        db::delete_session(&state.pool, &token).await?;
    }
    Ok(Json(json!({})))
}

// ---------------------------------------------------------------------------
// Everything the app shows, in one read
// ---------------------------------------------------------------------------

async fn state(State(state): State<AppState>, Admin(me): Admin) -> ApiResult {
    let now = db::now();
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
        }));
    }
    let apps: Vec<Value> = db::list_apps(&state.pool)
        .await?
        .into_iter()
        .map(|a| json!({ "id": a.id, "key": a.key, "name": a.name, "hostname": a.hostname, "port": a.port }))
        .collect();
    Ok(Json(json!({
        "me": { "id": me.id, "username": me.username },
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
}

fn totp_details(secret: &str, username: &str) -> Value {
    json!({
        "secret": secret,
        "uri": auth::totp_provisioning_uri(secret, username).unwrap_or_default(),
    })
}

async fn create_user(
    State(state): State<AppState>,
    Admin(_): Admin,
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
    let win_password = if input.win_password.is_empty() {
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

async fn delete_user(
    State(state): State<AppState>,
    Admin(me): Admin,
    Path(id): Path<i64>,
) -> ApiResult {
    if id == me.id {
        return Err(bad("You can't delete your own account."));
    }
    db::delete_user(&state.pool, id).await?;
    Ok(Json(json!({})))
}

#[derive(Deserialize)]
struct AppIds {
    #[serde(default)]
    app_ids: Vec<i64>,
}

async fn set_apps(
    State(state): State<AppState>,
    Admin(_): Admin,
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
    Admin(_): Admin,
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

/// Sets a new portal password. The user's open portal logins are ended (the
/// admin's own, when changing their own password, are kept).
async fn set_password(
    State(state): State<AppState>,
    Admin(me): Admin,
    Path(id): Path<i64>,
    Json(input): Json<NewPassword>,
) -> ApiResult {
    if input.password.is_empty() {
        return Err(bad("The new password can't be empty."));
    }
    existing_user(&state, id).await?;
    db::set_password(&state.pool, id, &input.password, id != me.id).await?;
    Ok(Json(json!({})))
}

async fn unlock(
    State(state): State<AppState>,
    Admin(_): Admin,
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
    Admin(_): Admin,
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
    Admin(_): Admin,
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
    Admin(_): Admin,
    Path(id): Path<i64>,
) -> ApiResult {
    db::delete_app(&state.pool, id).await?;
    Ok(Json(json!({})))
}
