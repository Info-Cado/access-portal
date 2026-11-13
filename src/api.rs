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
use crate::{access, activity, auth, backup, db, host, settings, AppState};

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
        .route("/api/admin/users/:id/access", put(set_access))
        .route("/api/admin/connections/:id/disconnect", post(disconnect))
        .route("/api/admin/connections/:id/shadow", post(shadow))
        .route("/api/admin/message", post(message))
        .route("/api/admin/activity", get(activity_list))
        .route("/api/admin/settings/sessions", put(set_session_settings))
        .route("/api/admin/setup", get(setup_check).post(setup_fix))
        .route("/api/admin/backups", get(backups_list).post(backup_now))
        .route("/api/admin/backups/:name/restore", post(backup_restore))
        .route("/api/admin/settings/backups", put(set_backup_settings))
        .route("/api/admin/settings/alerts", put(set_alert_settings))
        .route("/api/admin/settings/updates", put(set_update_settings))
        .route("/api/admin/health", get(health))
        .route("/api/admin/auth/verify", post(verify_admin))
        .route("/api/admin/auth", put(change_admin))
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
            "access": u.rules(),
            "access_text": access_text(&u.rules()),
        }));
    }
    let apps: Vec<Value> = db::list_apps(&state.pool)
        .await?
        .into_iter()
        .map(|a| json!({ "id": a.id, "key": a.key, "name": a.name, "hostname": a.hostname, "port": a.port }))
        .collect();
    let (admin_name, default_password) = settings::admin_account(&state.pool).await?;
    Ok(Json(json!({
        "listen": state.config.bind_addr,
        "users": users,
        "apps": apps,
        "settings": settings::sessions(&state.pool).await,
        "updates": settings::updates(&state.pool).await,
        "alerts": {
            "topic": settings::alerts(&state.pool).await.topic,
            "server": settings::alerts(&state.pool).await.server(),
        },
        "admin": { "username": admin_name, "default_password": default_password },
    })))
}

/// "Any time, from anywhere", or a short summary of the rules.
fn access_text(rules: &access::AccessRules) -> String {
    if rules.is_open() {
        return "Any time, from anywhere".into();
    }
    let mut parts = Vec::new();
    if rules.days.is_some() || rules.from.is_some() {
        let mut when = rules.describe_time();
        if let Some(first) = when.get(..1) {
            when = first.to_uppercase() + &when[1..];
        }
        parts.push(when);
    }
    if !rules.ips.is_empty() {
        parts.push(format!("from {}", rules.ips.join(", ")));
    }
    if !rules.countries.is_empty() {
        parts.push(format!("in {}", rules.countries.join(", ")));
    }
    parts.join(" · ")
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
    if let Some(user) = db::user_by_id(&state.pool, id).await? {
        let how = if input.create_windows_account { " with a new Windows account" } else { "" };
        activity::admin(&state.pool, Some(&user), &format!("Created the user{how}")).await;
    }

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
    let windows = if options.windows == Some(1) { " and their Windows account" } else { "" };
    activity::admin(&state.pool, Some(&user), &format!("Deleted the user{windows}")).await;
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
    let user = existing_user(&state, id).await?;
    db::set_password(&state.pool, id, &input.password, true).await?;
    activity::admin(&state.pool, Some(&user), "Changed the portal password").await;
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
    let connection = state.presence.get(id);
    if !state.presence.kick(id, crate::presence::DISCONNECTED) {
        return Err(fail(StatusCode::NOT_FOUND, "That session already ended."));
    }
    if let Some(c) = connection {
        let user = db::user_by_id(&state.pool, c.user_id).await?;
        activity::admin(&state.pool, user.as_ref(), &format!("Disconnected their {} session", c.app)).await;
    }
    Ok(Json(json!({})))
}

#[derive(Deserialize)]
struct ShadowRequest {
    /// Use the keyboard and mouse too, not just watch.
    #[serde(default)]
    control: bool,
}

/// Open Windows' viewer for the Windows session behind an open portal session,
/// on this computer's screen. The user's browser is told.
async fn shadow(
    State(state): State<AppState>,
    _: Admin,
    Path(id): Path<u64>,
    Json(input): Json<ShadowRequest>,
) -> ApiResult {
    let connection = state
        .presence
        .get(id)
        .ok_or_else(|| fail(StatusCode::NOT_FOUND, "That session already ended."))?;
    let user = existing_user(&state, connection.user_id).await?;
    let sessions = tokio::task::spawn_blocking(host::windows_sessions_blocking).await.unwrap_or_default();
    let mut theirs: Vec<_> = sessions
        .into_iter()
        .filter(|s| s.user.eq_ignore_ascii_case(&user.win_username))
        .collect();
    theirs.sort_by_key(|s| s.state != host::WTS_ACTIVE);
    let Some(session) = theirs.first() else {
        return Err(fail(StatusCode::CONFLICT, &format!("{} has no Windows session open right now.", user.win_username)));
    };
    let ask = settings::sessions(&state.pool).await.shadow_consent;
    host::shadow(session.id, input.control, ask).map_err(host_error)?;
    let (verb, what) = if input.control { ("is controlling", "Took control of") } else { ("is viewing", "Viewed") };
    let note = if ask {
        json!({"t": "note", "text": "Your administrator asked to see your screen. Windows will ask you to allow it."})
    } else {
        json!({"t": "note", "text": format!("Your administrator {verb} your screen.")})
    };
    state.presence.notify(|c| c.user_id == user.id, &note);
    activity::admin(&state.pool, Some(&user), &format!("{what} their screen")).await;
    Ok(Json(json!({ "session": session.id, "asked": ask })))
}

#[derive(Deserialize)]
struct MessageRequest {
    text: String,
    /// Only this user's sessions; everyone connected when neither is given.
    user_id: Option<i64>,
    /// Only this session.
    connection_id: Option<u64>,
}

/// Show a message in users' browsers, on top of their session.
async fn message(State(state): State<AppState>, _: Admin, Json(input): Json<MessageRequest>) -> ApiResult {
    let text = input.text.trim();
    if text.is_empty() {
        return Err(bad("Write a message first."));
    }
    if text.chars().count() > 2000 {
        return Err(bad("Keep the message under 2,000 characters."));
    }
    let note = json!({"t": "msg", "text": text, "at": db::now()});
    let sent = state.presence.notify(
        |c| match (input.connection_id, input.user_id) {
            (Some(id), _) => c.id == id,
            (None, Some(user)) => c.user_id == user,
            (None, None) => true,
        },
        &note,
    );
    if sent == 0 {
        return Err(fail(StatusCode::CONFLICT, "Nobody is connected to receive it."));
    }
    let user = match input.user_id {
        Some(id) => db::user_by_id(&state.pool, id).await?,
        None => None,
    };
    let to = if user.is_some() { String::new() } else { format!(" to everyone connected ({sent})") };
    activity::admin(&state.pool, user.as_ref(), &format!("Sent a message{to}: {text}")).await;
    Ok(Json(json!({ "sent": sent })))
}

#[derive(Deserialize)]
struct AccessInput {
    rules: access::AccessRules,
}

async fn set_access(
    State(state): State<AppState>,
    _: Admin,
    Path(id): Path<i64>,
    Json(input): Json<AccessInput>,
) -> ApiResult {
    let user = existing_user(&state, id).await?;
    let rules = input.rules.normalized().map_err(|e| bad(&e))?;
    let stored = if rules.is_open() { None } else { Some(serde_json::to_string(&rules).map_err(anyhow::Error::from)?) };
    db::set_access_rules(&state.pool, id, stored.as_deref()).await?;
    let text = access_text(&rules);
    activity::admin(&state.pool, Some(&user), &format!("Set when and where they can sign in: {text}")).await;
    Ok(Json(json!({ "access": rules, "access_text": text })))
}

#[derive(Deserialize)]
struct ActivityQuery {
    user: Option<i64>,
    before: Option<i64>,
    limit: Option<i64>,
}

async fn activity_list(
    State(state): State<AppState>,
    _: Admin,
    axum::extract::Query(q): axum::extract::Query<ActivityQuery>,
) -> ApiResult {
    let entries = activity::list(&state.pool, q.user, q.before, q.limit.unwrap_or(200)).await?;
    Ok(Json(json!({ "entries": entries })))
}

async fn set_session_settings(
    State(state): State<AppState>,
    _: Admin,
    Json(input): Json<settings::Sessions>,
) -> ApiResult {
    if input.idle_minutes > 24 * 60 || input.signout_hours > 24 * 30 {
        return Err(bad("That's longer than the portal allows."));
    }
    let before = settings::sessions(&state.pool).await;
    settings::set_sessions(&state.pool, &input).await?;
    if before.shadow_consent != input.shadow_consent {
        let ask = input.shadow_consent;
        tokio::task::spawn_blocking(move || host::set_shadow_policy(ask)).await.ok();
    }
    activity::admin(
        &state.pool,
        None,
        &format!(
            "Session settings: disconnect idle sessions {}, sign out disconnected Windows sessions {}, {} before viewing a screen",
            if input.idle_minutes == 0 { "never".into() } else { format!("after {}", activity::duration(i64::from(input.idle_minutes) * 60)) },
            if input.signout_hours == 0 { "never".into() } else { format!("after {}", activity::duration(i64::from(input.signout_hours) * 3600)) },
            if input.shadow_consent { "ask" } else { "don't ask" },
        ),
    )
    .await;
    Ok(Json(json!({ "settings": input })))
}

async fn backups_list(State(state): State<AppState>, _: Admin) -> ApiResult {
    Ok(Json(json!({
        "backups": backup::list(&state.data_dir),
        "settings": settings::backups(&state.pool).await,
        "folder": backup::dir(&state.data_dir),
    })))
}

async fn backup_now(State(state): State<AppState>, _: Admin) -> ApiResult {
    let settings = settings::backups(&state.pool).await;
    let name = backup::make(&state.pool, &state.data_dir, &settings)
        .await
        .map_err(|e| fail(StatusCode::INTERNAL_SERVER_ERROR, &format!("{e:#}")))?;
    activity::admin(&state.pool, None, &format!("Made a backup ({name})")).await;
    Ok(Json(json!({ "name": name })))
}

/// Restore at the next start; the app restarts itself right after.
async fn backup_restore(State(state): State<AppState>, _: Admin, Path(name): Path<String>) -> ApiResult {
    // Keep what is there now, in case the restore was a mistake.
    let settings = settings::backups(&state.pool).await;
    backup::request_restore(&state.data_dir, &name).map_err(|e| bad(&e.to_string()))?;
    let safety = backup::make_keeping_all(&state.pool, &state.data_dir, &settings).await.ok();
    activity::admin(&state.pool, None, &format!("Restoring backup {name} (current data saved as {})", safety.as_deref().unwrap_or("-"))).await;
    Ok(Json(json!({ "restart": true, "saved_as": safety })))
}

async fn set_backup_settings(State(state): State<AppState>, _: Admin, Json(input): Json<backup::Settings>) -> ApiResult {
    if !(1..=365).contains(&input.keep) {
        return Err(bad("Keep between 1 and 365 backups."));
    }
    let copy = input.copy_dir.trim();
    if !copy.is_empty() {
        let path = std::path::Path::new(copy);
        if !path.is_absolute() {
            return Err(bad("Give the full path of the folder, like D:\\Backups."));
        }
        if std::fs::create_dir_all(path.join("Access Portal backups")).is_err() {
            return Err(bad("That folder can't be written to."));
        }
    }
    let value = backup::Settings { copy_dir: copy.to_string(), keep: input.keep };
    settings::set_backups(&state.pool, &value).await?;
    activity::admin(&state.pool, None, &format!("Backups: keep {}, copy to {}", value.keep, if copy.is_empty() { "nowhere else" } else { copy })).await;
    Ok(Json(json!({ "settings": value })))
}

async fn set_update_settings(State(state): State<AppState>, _: Admin, Json(input): Json<settings::Updates>) -> ApiResult {
    settings::set_updates(&state.pool, &input).await?;
    activity::admin(&state.pool, None, if input.auto { "Turned automatic updates on" } else { "Turned automatic updates off" }).await;
    Ok(Json(json!({ "updates": input })))
}

#[derive(Deserialize)]
struct AlertSwitch {
    enabled: bool,
    /// Another ntfy server, e.g. "https://ntfy.example.com" ("" = the default).
    #[serde(default)]
    server: Option<String>,
}

/// Turn phone alerts on (a new private ntfy topic) or off.
async fn set_alert_settings(State(state): State<AppState>, _: Admin, Json(input): Json<AlertSwitch>) -> ApiResult {
    let current = settings::alerts(&state.pool).await;
    let server = match input.server.as_deref().map(str::trim) {
        None => current.server.clone(),
        Some("") => None,
        Some(s) if s.starts_with("https://") && !s.contains(char::is_whitespace) => Some(s.trim_end_matches('/').to_string()),
        Some(_) => return Err(bad("The server address starts with https://.")),
    };
    let value = settings::Alerts {
        topic: if input.enabled { current.topic.or_else(|| Some(format!("access-portal-{}", auth::random_token()[..20].to_lowercase()))) } else { None },
        server,
    };
    settings::set_alerts(&state.pool, &value).await?;
    activity::admin(&state.pool, None, if input.enabled { "Turned phone alerts on" } else { "Turned phone alerts off" }).await;
    Ok(Json(json!({ "alerts": value })))
}

/// For the app's alerts: problems the portal can see from here.
async fn health(State(state): State<AppState>, _: Admin) -> ApiResult {
    let (failures, last_failure) = state.health.recent_failures(10 * 60);
    let listening = tokio::time::timeout(
        std::time::Duration::from_secs(2),
        tokio::net::TcpStream::connect("127.0.0.1:3389"),
    )
    .await
    .is_ok_and(|r| r.is_ok());
    let alerts = settings::alerts(&state.pool).await;
    Ok(Json(json!({
        "alerts": { "topic": alerts.topic, "server": alerts.server() },
        "session_failures": failures,
        "last_failure": last_failure,
        "disk_free": host::disk_free(&state.data_dir),
        "rdp_listening": listening,
    })))
}

/// What this computer needs to serve portal sessions, and whether it has it.
async fn setup_check(_: Admin) -> ApiResult {
    let checks = tokio::task::spawn_blocking(|| host::setup_blocking(&[]))
        .await
        .map_err(anyhow::Error::from)?
        .map_err(host_error)?;
    Ok(Json(json!({ "checks": checks })))
}

#[derive(Deserialize)]
struct SetupFix {
    fix: Vec<String>,
}

async fn setup_fix(State(state): State<AppState>, _: Admin, Json(input): Json<SetupFix>) -> ApiResult {
    const KNOWN: [&str; 6] = ["rdp", "wrapper", "group", "launcher", "pdf", "policies"];
    let fix: Vec<String> = input.fix.into_iter().filter(|f| KNOWN.contains(&f.as_str())).collect();
    if fix.is_empty() {
        return Err(bad("Nothing to fix."));
    }
    let list = fix.clone();
    let checks = tokio::task::spawn_blocking(move || host::setup_blocking(&list))
        .await
        .map_err(anyhow::Error::from)?
        .map_err(host_error)?;
    let failed: Vec<String> = checks
        .iter()
        .filter_map(|c| c.get("error").and_then(Value::as_str).map(|e| format!("{}: {e}", c["id"].as_str().unwrap_or(""))))
        .collect();
    let detail = if failed.is_empty() {
        format!("Set up this computer: {}", fix.join(", "))
    } else {
        format!("Set up this computer: {} (problems: {})", fix.join(", "), failed.join("; "))
    };
    activity::admin(&state.pool, None, &detail).await;
    Ok(Json(json!({ "checks": checks })))
}

#[derive(Deserialize)]
struct AdminLogin {
    username: String,
    password: String,
}

/// Check the app's own sign-in (the app's native side asks before unlocking).
async fn verify_admin(State(state): State<AppState>, _: Admin, Json(input): Json<AdminLogin>) -> ApiResult {
    if !settings::check_admin(&state.pool, &input.username, &input.password).await? {
        return Err(fail(StatusCode::UNAUTHORIZED, "Wrong username or password."));
    }
    let (_, default_password) = settings::admin_account(&state.pool).await?;
    Ok(Json(json!({ "ok": true, "default_password": default_password })))
}

#[derive(Deserialize)]
struct AdminChange {
    current_password: String,
    username: String,
    password: String,
}

async fn change_admin(State(state): State<AppState>, _: Admin, Json(input): Json<AdminChange>) -> ApiResult {
    let (name, _) = settings::admin_account(&state.pool).await?;
    if !settings::check_admin(&state.pool, &name, &input.current_password).await? {
        return Err(fail(StatusCode::UNAUTHORIZED, "The current password is wrong."));
    }
    let username = input.username.trim();
    if username.is_empty() || username.chars().count() > 64 {
        return Err(bad("Choose a username (up to 64 characters)."));
    }
    if input.password.chars().count() < 8 {
        return Err(bad("Use at least 8 characters for the new password."));
    }
    if input.password.eq_ignore_ascii_case(settings::DEFAULT_ADMIN.1) {
        return Err(bad("Choose a password other than the default one."));
    }
    settings::set_admin(&state.pool, username, &input.password).await?;
    activity::admin(&state.pool, None, "Changed the app's admin sign-in").await;
    Ok(Json(json!({ "username": username })))
}

/// Sign a user out completely: disconnect their sessions, end their portal
/// sign-ins and sign their Windows account out (their apps close).
async fn sign_out_user(State(state): State<AppState>, _: Admin, Path(id): Path<i64>) -> ApiResult {
    let user = existing_user(&state, id).await?;
    let disconnected = state.presence.kick_user(id, crate::presence::SIGNED_OUT);
    db::delete_sessions_for_user(&state.pool, id).await?;
    activity::admin(&state.pool, Some(&user), "Signed them out of Windows").await;
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
