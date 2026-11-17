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
        .route("/api/admin/users/:id/unblock", post(unblock))
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
        .route("/api/admin/settings/network", put(set_network_settings))
        .route("/api/admin/health", get(health))
        .route("/api/admin/auth/verify", post(verify_admin))
        .route("/api/admin/auth", put(change_admin))
        .route("/api/admin/windows-accounts", get(windows_accounts))
        .route("/api/admin/windows-accounts/:name", put(update_windows_account).delete(delete_windows_account))
        .route("/api/admin/windows-accounts/:name/desktop", get(account_desktop_mode).put(set_account_desktop_mode))
        .route("/api/admin/owner-access", post(enable_owner_access).delete(disable_owner_access))
        .route("/api/admin/apps", post(create_app))
        .route("/api/admin/apps/:id", put(update_app).delete(delete_app))
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
            "blocked_until": u.blocked_until.filter(|until| *until > now),
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
        .map(|a| json!({ "id": a.id, "key": a.key, "name": a.name, "hostname": a.hostname, "port": a.port, "program": a.program, "work_dir": a.work_dir, "profile": a.profile }))
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
        "owner_user_id": owner_user(&state).await?.map(|u| u.id),
        "network": network_info(&state),
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
        // New accounts start restricted: set up their black-screen program now.
        apply_app_access(&state, &user).await?;
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
    // Disconnect any session they have open now, so deleting the portal user
    // ends their access at once even when the Windows account stays (it may be
    // shared with other portal users).
    state.presence.kick_user(id, crate::presence::DELETED);
    if options.windows == Some(1) {
        // Close their program safely first; never delete over unsaved work.
        if close_safely(&user.win_username).await == host::SafeClose::Busy {
            return Err(fail(
                StatusCode::CONFLICT,
                &format!("{} Then delete the user again.", busy_message(&user)),
            ));
        }
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
    let user = existing_user(&state, id).await?;
    db::set_user_apps(&state.pool, id, &input.app_ids).await?;
    apply_app_access(&state, &user).await?;
    Ok(Json(json!({})))
}

/// Bring a user's Windows account in line with their apps: access to the data
/// folder of each black-screen app (repeated each time, so a changed folder is
/// picked up), and, when the account is restricted, the launcher for its
/// program. Run after anything that changes a user's apps. Access is only ever
/// added here: several portal users can share one Windows account.
async fn apply_app_access(state: &AppState, user: &User) -> Result<(), ApiError> {
    let apps = db::apps_for_user(&state.pool, user.id).await?;
    let mut folders: Vec<String> = apps
        .iter()
        .filter(|a| a.program.as_deref().is_some_and(|p| !p.trim().is_empty()))
        .filter_map(|a| a.work_dir.as_deref().map(str::trim).filter(|d| !d.is_empty()).map(str::to_owned))
        .collect();
    folders.sort();
    folders.dedup();
    if folders.is_empty() && pick_launch(apps.clone()).is_none() {
        return Ok(());
    }
    let mode = host::desktop_mode(&user.win_username).await.unwrap_or_default();
    if mode == "missing" {
        return Ok(()); // no such Windows account (yet): nothing to set up
    }
    for dir in &folders {
        crate::host::grant_folder(&user.win_username, dir)
            .await
            .map_err(|e| fail(StatusCode::BAD_GATEWAY, &format!("The apps were saved, but {} couldn't be given the folder {dir}: {e:#}", user.win_username)))?;
    }
    if mode == "app" {
        let launch = pick_launch(apps);
        host::set_desktop_mode(&user.win_username, false, launch.as_ref())
            .await
            .map_err(|e| fail(StatusCode::BAD_GATEWAY, &format!("The apps were saved, but the program couldn't be set up for {}: {e:#}", user.win_username)))?;
    }
    Ok(())
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

/// Whether the user's Windows account gets the full desktop or is restricted to
/// its assigned apps: `{"mode": "app" | "full" | "missing"}`.
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
    // One black-screen program per person: the assigned app that has one. When
    // restricted, the account opens just this program; its folder is the only
    // one the account may reach.
    let launch = pick_launch(db::apps_for_user(&state.pool, id).await?);
    let mode = host::set_desktop_mode(&user.win_username, input.full, launch.as_ref())
        .await
        .map_err(host_error)?;
    if mode == "missing" {
        return Err(fail(
            StatusCode::CONFLICT,
            &format!("There is no Windows account named {} on this computer.", user.win_username),
        ));
    }
    // Lock the account to its data folder when it is a restricted single-program
    // account (standard-user rights still run Windows and the program itself).
    if mode == "app" {
        if let Some(dir) = launch.as_ref().and_then(|l| l.work_dir.as_deref()) {
            if let Err(e) = crate::host::grant_folder(&user.win_username, dir).await {
                tracing::warn!("could not scope folder {dir} for {}: {e:#}", user.win_username);
            }
        }
    }
    tracing::info!(
        "windows account {} of portal user {} set to {mode}",
        user.win_username,
        user.username
    );
    Ok(Json(json!({ "mode": mode })))
}

// ---------------------------------------------------------------------------
// Windows accounts on this computer (all of them, not only the portal's)
// ---------------------------------------------------------------------------

/// The settings key holding the id of the portal user that signs in as the
/// computer owner's own Windows account (see `enable_owner_access`).
const OWNER_USER_KEY: &str = "owner_user_id";

/// The portal user set up for the owner's own account, if it still exists.
async fn owner_user(state: &AppState) -> Result<Option<User>, ApiError> {
    let Some(id) = settings::get(&state.pool, OWNER_USER_KEY).await?.and_then(|v| v.parse::<i64>().ok()) else {
        return Ok(None);
    };
    Ok(db::user_by_id(&state.pool, id).await?)
}

/// Every local Windows account, each with the portal users that sign in as it,
/// and the owner's portal access: `{accounts: [...], owner_user_id}`.
async fn windows_accounts(State(state): State<AppState>, _: Admin) -> ApiResult {
    let users = db::list_users(&state.pool).await?;
    let mut accounts = host::list_accounts().await.map_err(host_error)?;
    for account in &mut accounts {
        let name = account.get("name").and_then(|n| n.as_str()).unwrap_or("").to_lowercase();
        let linked: Vec<Value> = users
            .iter()
            .filter(|u| u.win_username.to_lowercase() == name)
            .map(|u| json!({ "id": u.id, "username": u.username }))
            .collect();
        account["portal_users"] = json!(linked);
    }
    // The owner's account first, then by name.
    accounts.sort_by_key(|a| {
        let me = a.get("me").and_then(|m| m.as_bool()) == Some(true);
        (!me, a.get("name").and_then(|n| n.as_str()).unwrap_or("").to_lowercase())
    });
    let owner = owner_user(&state).await?;
    Ok(Json(json!({ "accounts": accounts, "owner_user_id": owner.map(|u| u.id) })))
}

#[derive(Deserialize)]
struct AccountUpdate {
    full_name: Option<String>,
    description: Option<String>,
    enabled: Option<bool>,
    password: Option<String>,
}

/// Change a Windows account's details. A new password is also stored for the
/// portal users that sign in as the account, so their sessions keep working.
async fn update_windows_account(
    State(state): State<AppState>,
    _: Admin,
    Path(name): Path<String>,
    Json(input): Json<AccountUpdate>,
) -> ApiResult {
    if input.password.as_deref() == Some("") {
        return Err(bad("The new password can't be empty."));
    }
    let changes = host::AccountChanges {
        full_name: input.full_name.map(|s| s.trim().to_string()),
        description: input.description.map(|s| s.trim().to_string()),
        enabled: input.enabled,
        password: input.password.clone(),
    };
    host::update_account(&name, &changes).await.map_err(host_error)?;
    let mut what = Vec::new();
    if changes.full_name.is_some() || changes.description.is_some() {
        what.push("details");
    }
    if let Some(on) = changes.enabled {
        what.push(if on { "turned on" } else { "turned off" });
    }
    if let Some(password) = &input.password {
        what.push("password");
        for u in db::list_users(&state.pool).await? {
            if u.win_username.eq_ignore_ascii_case(&name) {
                db::set_windows_login(&state.pool, &state.config, u.id, &u.win_username, password).await?;
            }
        }
    }
    activity::admin(&state.pool, None, &format!("Changed Windows account {name}: {}", what.join(", "))).await;
    Ok(Json(json!({})))
}

#[derive(Deserialize)]
struct AccountDelete {
    /// Also delete the account's profile folder (its files).
    #[serde(default)]
    files: Option<u8>,
}

/// Delete a Windows account. Portal users that sign in as it stay, and can't
/// connect until they are given another account.
async fn delete_windows_account(
    State(state): State<AppState>,
    _: Admin,
    Path(name): Path<String>,
    axum::extract::Query(options): axum::extract::Query<AccountDelete>,
) -> ApiResult {
    let files = options.files == Some(1);
    // Close its black-screen program safely first; never delete over unsaved work.
    if close_safely(&name).await == host::SafeClose::Busy {
        return Err(fail(
            StatusCode::CONFLICT,
            &format!("{name}'s app has a window with unsaved work open, so the account was left alone. \
                      Ask them to save and close it, then delete the account again."),
        ));
    }
    host::delete_any_account(&name, files).await.map_err(host_error)?;
    let with = if files { " and its files" } else { "" };
    activity::admin(&state.pool, None, &format!("Deleted Windows account {name}{with}")).await;
    Ok(Json(json!({})))
}

async fn account_desktop_mode(_: Admin, Path(name): Path<String>) -> ApiResult {
    let mode = host::desktop_mode(&name).await.map_err(host_error)?;
    Ok(Json(json!({ "mode": mode })))
}

/// The one black-screen program (and its folder) for a user's assigned apps.
fn pick_launch(apps: Vec<crate::models::App>) -> Option<host::Launch> {
    apps.into_iter().find_map(|a| {
        let program = a.program.filter(|p| !p.trim().is_empty())?;
        Some(host::Launch {
            program,
            work_dir: a.work_dir.filter(|d| !d.trim().is_empty()),
            profile: a.profile.filter(|p| !p.trim().is_empty()),
        })
    })
}

/// The program configured for any portal user that signs in as this Windows
/// account (several may share it; the first with a program wins).
async fn account_launch(state: &AppState, name: &str) -> Result<Option<host::Launch>, ApiError> {
    for user in db::list_users(&state.pool).await? {
        if user.win_username.eq_ignore_ascii_case(name) {
            if let Some(found) = pick_launch(db::apps_for_user(&state.pool, user.id).await?) {
                return Ok(Some(found));
            }
        }
    }
    Ok(None)
}

async fn set_account_desktop_mode(
    State(state): State<AppState>,
    _: Admin,
    Path(name): Path<String>,
    Json(input): Json<DesktopSwitch>,
) -> ApiResult {
    let launch = account_launch(&state, &name).await?;
    let mode = host::set_desktop_mode(&name, input.full, launch.as_ref()).await.map_err(host_error)?;
    if mode == "missing" {
        return Err(fail(StatusCode::CONFLICT, &format!("There is no Windows account named {name} on this computer.")));
    }
    if mode == "app" {
        if let Some(dir) = launch.as_ref().and_then(|l| l.work_dir.as_deref()) {
            if let Err(e) = crate::host::grant_folder(&name, dir).await {
                tracing::warn!("could not scope folder {dir} for {name}: {e:#}");
            }
        }
    }
    Ok(Json(json!({ "mode": mode })))
}

#[derive(Deserialize)]
struct OwnerAccess {
    /// The name to sign in to the portal with.
    username: String,
    /// The portal password.
    password: String,
    /// The owner's Windows password, for opening their desktop.
    win_password: String,
    /// The Windows sign-in name, when it isn't the account name (a Microsoft
    /// account's email address).
    #[serde(default)]
    win_username: String,
}

/// Let the computer's owner sign in to the portal and open their own Windows
/// desktop: a portal user that signs in as the account Access Portal runs as.
/// Administrator accounts always get the full desktop, so the portal opens it.
async fn enable_owner_access(State(state): State<AppState>, _: Admin, Json(input): Json<OwnerAccess>) -> ApiResult {
    let username = input.username.trim();
    if username.is_empty() || input.password.is_empty() || input.win_password.is_empty() {
        return Err(bad("A portal username, a portal password and your Windows password are all needed."));
    }
    if owner_user(&state).await?.is_some() {
        return Err(fail(StatusCode::CONFLICT, "Your account can already be opened from the portal."));
    }
    if db::user_by_username(&state.pool, username).await?.is_some() {
        return Err(fail(StatusCode::CONFLICT, "That portal username is already taken."));
    }
    let accounts = host::list_accounts().await.map_err(host_error)?;
    let Some(me) = accounts.iter().find(|a| a.get("me").and_then(|m| m.as_bool()) == Some(true)) else {
        return Err(fail(StatusCode::CONFLICT, "Access Portal isn't running as a local Windows account."));
    };
    let account = me.get("name").and_then(|n| n.as_str()).unwrap_or_default().to_string();
    let microsoft = me.get("microsoft").and_then(|m| m.as_bool()) == Some(true);
    let win_username = match input.win_username.trim() {
        "" => account.clone(),
        w => w.to_string(),
    };
    // A wrong password would only show up when the desktop fails to open.
    if !microsoft && !host::check_password(&account, &input.win_password).await.map_err(host_error)? {
        return Err(bad("Windows says that isn't the password of your account."));
    }
    let (id, _) = db::create_user(
        &state.pool,
        &state.config,
        username,
        &input.password,
        &win_username,
        &input.win_password,
        false,
        false,
    )
    .await?;
    settings::set(&state.pool, OWNER_USER_KEY, &id.to_string()).await?;
    if let Some(user) = db::user_by_id(&state.pool, id).await? {
        activity::admin(&state.pool, Some(&user), &format!("Turned on portal access to the owner's account {account}")).await;
    }
    Ok(Json(json!({ "id": id })))
}

/// Remove the owner's portal access (the portal user only; the Windows account
/// stays as it is).
async fn disable_owner_access(State(state): State<AppState>, _: Admin) -> ApiResult {
    if let Some(user) = owner_user(&state).await? {
        db::delete_user(&state.pool, user.id).await?;
        activity::admin(&state.pool, Some(&user), "Turned off portal access to the owner's account").await;
    }
    settings::set(&state.pool, OWNER_USER_KEY, "").await?;
    Ok(Json(json!({})))
}

#[derive(Deserialize)]
struct DisconnectOptions {
    /// Keep the user from reconnecting for this many minutes. 0 or absent: just
    /// disconnect, they can reconnect at once. A negative value: block until an
    /// administrator unblocks them.
    #[serde(default)]
    block_minutes: i64,
}

/// Disconnect one open session. The user's apps keep running in Windows. Without
/// a block they can reconnect at once; with one, every session of theirs is
/// disconnected and they can't sign in or connect again until it passes.
async fn disconnect(
    State(state): State<AppState>,
    _: Admin,
    Path(id): Path<u64>,
    // Optional so a call with no body means a plain disconnect, no block.
    options: Option<Json<DisconnectOptions>>,
) -> ApiResult {
    let options = options.map(|Json(o)| o).unwrap_or(DisconnectOptions { block_minutes: 0 });
    let Some(connection) = state.presence.get(id) else {
        return Err(fail(StatusCode::NOT_FOUND, "That session already ended."));
    };
    let user = db::user_by_id(&state.pool, connection.user_id).await?;

    if options.block_minutes != 0 {
        let until = if options.block_minutes < 0 {
            crate::models::BLOCK_FOREVER
        } else {
            db::now() + options.block_minutes * 60
        };
        db::set_block(&state.pool, connection.user_id, Some(until)).await?;
        // End every session of theirs, not only the one clicked, and sign them
        // out of the portal so the block takes hold right away.
        state.presence.kick_user(connection.user_id, crate::presence::BLOCKED);
        db::delete_sessions_for_user(&state.pool, connection.user_id).await?;
        let how = if options.block_minutes < 0 {
            "until unblocked".to_string()
        } else {
            format!("for {}", activity::duration(options.block_minutes * 60))
        };
        activity::admin(&state.pool, user.as_ref(), &format!("Disconnected and blocked them {how}")).await;
        return Ok(Json(json!({ "blocked_until": until })));
    }

    state.presence.kick(id, crate::presence::DISCONNECTED);
    activity::admin(&state.pool, user.as_ref(), &format!("Disconnected their {} session", connection.app)).await;
    Ok(Json(json!({})))
}

/// Lift an administrator's block (not the automatic lockout; that is `unlock`).
async fn unblock(State(state): State<AppState>, _: Admin, Path(id): Path<i64>) -> ApiResult {
    let user = existing_user(&state, id).await?;
    db::set_block(&state.pool, id, None).await?;
    activity::admin(&state.pool, Some(&user), "Unblocked them").await;
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
    let what = if input.control { "Took control of" } else { "Viewed" };
    // When consent is off the user is told nothing; with it on, Windows prompts
    // them, so the browser explains why the prompt appeared.
    if ask {
        let note = json!({"t": "note", "text": "Your administrator asked to see your screen. Windows will ask you to allow it."});
        state.presence.notify(|c| c.user_id == user.id, &note);
    }
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

/// Let browsers on the local network and/or Tailscale open the portal directly.
async fn set_network_settings(State(state): State<AppState>, _: Admin, Json(input): Json<settings::Reach>) -> ApiResult {
    settings::set_reach(&state.pool, &input).await?;
    state.network.set_reach(input).await.map_err(|e| fail(StatusCode::CONFLICT, &format!("{e:#}")))?;
    let on = |b: bool| if b { "on" } else { "off" };
    activity::admin(
        &state.pool,
        None,
        &format!("Local network access {}, Tailscale access {}", on(input.lan), on(input.tailscale)),
    )
    .await;
    Ok(Json(json!({ "network": network_info(&state) })))
}

fn network_info(state: &AppState) -> Value {
    let reach = state.network.reach();
    json!({ "lan": reach.lan, "tailscale": reach.tailscale, "port": state.network.port(), "urls": state.network.urls() })
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
    // "wrapper-repair" reinstalls RDP Wrapper even when it looks fine.
    const KNOWN: [&str; 7] = ["rdp", "wrapper", "wrapper-repair", "group", "launcher", "pdf", "policies"];
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
/// A black-screen program is closed safely first; if it has unsaved work it is
/// left open (409) unless `?force=1`, which signs Windows out regardless.
async fn sign_out_user(
    State(state): State<AppState>,
    _: Admin,
    Path(id): Path<i64>,
    axum::extract::Query(options): axum::extract::Query<ForceOption>,
) -> ApiResult {
    let user = existing_user(&state, id).await?;
    let disconnected = state.presence.kick_user(id, crate::presence::SIGNED_OUT);
    db::delete_sessions_for_user(&state.pool, id).await?;
    // Give the sessions a moment to close before Windows signs the account out.
    tokio::time::sleep(std::time::Duration::from_millis(300)).await;
    let force = options.force == Some(1);
    if !force {
        match close_safely(&user.win_username).await {
            host::SafeClose::Busy => {
                activity::admin(&state.pool, Some(&user), "Sign-out stopped: their app has unsaved work open").await;
                return Err(fail(StatusCode::CONFLICT, &busy_message(&user)));
            }
            host::SafeClose::Closed | host::SafeClose::NoSession => {
                activity::admin(&state.pool, Some(&user), "Signed them out of Windows (their app closed safely)").await;
                return Ok(Json(json!({ "disconnected": disconnected, "windows_sessions": 0, "closed_safely": true })));
            }
            host::SafeClose::NotKiosk => {} // a desktop: sign Windows out as before
        }
    }
    let how = if force { " (forced)" } else { "" };
    activity::admin(&state.pool, Some(&user), &format!("Signed them out of Windows{how}")).await;
    let win = user.win_username.clone();
    let windows_sessions = tokio::task::spawn_blocking(move || host::sign_out_blocking(&win)).await.unwrap_or(0);
    Ok(Json(json!({ "disconnected": disconnected, "windows_sessions": windows_sessions })))
}

#[derive(Deserialize)]
struct ForceOption {
    force: Option<u8>,
}

/// Close a user's black-screen program the safe way (waits up to 40 s).
async fn close_safely(win_username: &str) -> host::SafeClose {
    let win = win_username.to_string();
    tokio::task::spawn_blocking(move || host::close_safely_blocking(&win, std::time::Duration::from_secs(40)))
        .await
        .unwrap_or(host::SafeClose::Busy)
}

fn busy_message(user: &User) -> String {
    format!(
        "{}'s app has a window with unsaved work open, so it was left running (closing it now could lose that work). \
         Ask them to save and close it, or force the sign-out.",
        user.username
    )
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
    /// When set, the app opens this program on a black screen (not the desktop).
    #[serde(default)]
    program: String,
    /// The program's working folder, and the only data folder the user's Windows
    /// account is granted on the host.
    #[serde(default)]
    work_dir: String,
    /// "miracle" for Miracle; empty or "generic" for any other program.
    #[serde(default)]
    profile: String,
}

/// Trim a field; empty becomes `None` (no program / no folder set).
fn opt(s: &str) -> Option<&str> {
    let t = s.trim();
    if t.is_empty() { None } else { Some(t) }
}

/// The profile to store ("miracle" or none), after checking it fits the app.
fn check_profile(profile: &str, program: Option<&str>, work_dir: Option<&str>) -> Result<Option<&'static str>, ApiError> {
    match profile.trim().to_lowercase().as_str() {
        "" | "generic" => Ok(None),
        "miracle" => {
            let Some(program) = program else {
                return Err(bad("A Miracle app needs the Miracle program (miracle.exe) or a Miracle shortcut."));
            };
            // miracle.exe on its own opens the company folder the app gives it.
            if !program.to_lowercase().ends_with(".lnk") && work_dir.is_none() {
                return Err(bad("Choose the company folder this Miracle app opens."));
            }
            Ok(Some(host::PROFILE_MIRACLE))
        }
        other => Err(bad(&format!("Unknown app type \"{other}\"."))),
    }
}

/// A data folder inside a Windows profile (C:\Users\...) can't be used by other
/// accounts: Windows keeps them out of each profile, so the program falls back
/// to its own folder. Say so instead of letting it silently misbehave.
fn check_data_folder(work_dir: &str) -> Result<(), ApiError> {
    let drive = std::env::var("SystemDrive").unwrap_or_else(|_| "C:".into());
    let users = format!("{}\\users\\", drive.to_lowercase());
    if work_dir.trim().to_lowercase().replace('/', "\\").starts_with(&users) {
        return Err(bad(&format!(
            "Put the data folder outside {drive}\\Users (for example {drive}\\Miracle-Companies\\Kelvin): \
             other Windows accounts can't open folders inside someone's profile, such as their Desktop."
        )));
    }
    Ok(())
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
    if key.eq_ignore_ascii_case(crate::web::DESKTOP_KEY) {
        return Err(bad("\"desktop\" is kept for the full desktop; choose another key."));
    }
    let port = match input.port.trim() {
        "" => "3389",
        p if p.parse::<u16>().is_ok() => p,
        _ => return Err(bad("The port must be a number between 1 and 65535.")),
    };
    if db::app_by_key(&state.pool, key).await?.is_some() {
        return Err(fail(StatusCode::CONFLICT, "That key is already used by another app."));
    }
    if opt(&input.program).is_none() && opt(&input.work_dir).is_some() {
        return Err(bad("A black-screen app needs a program as well as a folder."));
    }
    if let Some(dir) = opt(&input.work_dir) {
        check_data_folder(dir)?;
    }
    let profile = check_profile(&input.profile, opt(&input.program), opt(&input.work_dir))?;
    let id = db::create_app(
        &state.pool, key, name, hostname, port, opt(&input.program), opt(&input.work_dir), profile,
    )
    .await?;
    Ok(Json(json!({ "id": id })))
}

#[derive(Deserialize)]
struct EditApp {
    name: String,
    hostname: String,
    #[serde(default)]
    port: String,
    #[serde(default)]
    program: String,
    #[serde(default)]
    work_dir: String,
    #[serde(default)]
    profile: String,
}

async fn update_app(
    State(state): State<AppState>,
    _: Admin,
    Path(id): Path<i64>,
    Json(input): Json<EditApp>,
) -> ApiResult {
    let (name, hostname) = (input.name.trim(), input.hostname.trim());
    if name.is_empty() || hostname.is_empty() {
        return Err(bad("Name and host are required."));
    }
    let port = match input.port.trim() {
        "" => "3389",
        p if p.parse::<u16>().is_ok() => p,
        _ => return Err(bad("The port must be a number between 1 and 65535.")),
    };
    if opt(&input.program).is_none() && opt(&input.work_dir).is_some() {
        return Err(bad("A black-screen app needs a program as well as a folder."));
    }
    if let Some(dir) = opt(&input.work_dir) {
        check_data_folder(dir)?;
    }
    let profile = check_profile(&input.profile, opt(&input.program), opt(&input.work_dir))?;
    if db::app_by_id(&state.pool, id).await?.is_none() {
        return Err(fail(StatusCode::NOT_FOUND, "That app no longer exists."));
    }
    db::update_app(
        &state.pool, id, name, hostname, port, opt(&input.program), opt(&input.work_dir), profile,
    )
    .await?;
    // Users who have this app get the new program and folder.
    for user in db::list_users(&state.pool).await? {
        if db::apps_for_user(&state.pool, user.id).await?.iter().any(|a| a.id == id) {
            apply_app_access(&state, &user).await?;
        }
    }
    Ok(Json(json!({})))
}

async fn delete_app(
    State(state): State<AppState>,
    _: Admin,
    Path(id): Path<i64>,
) -> ApiResult {
    db::delete_app(&state.pool, id).await?;
    Ok(Json(json!({})))
}
