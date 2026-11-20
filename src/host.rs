//! Changes to Windows accounts on the machine the portal runs on.
//!
//! A portal user's sessions run as a local Windows account. That account is in
//! one of two modes:
//!
//! - **app** (restricted): Run, the command prompt, PowerShell, Task Manager,
//!   the registry editor, Control Panel and drive C: are blocked by
//!   per-account policies, and the portal offers the user only the apps
//!   assigned to them, never a desktop;
//! - **full**: a normal Windows desktop, which the portal offers the user as
//!   "Desktop" next to their apps.
//!
//! The mode lives in the account's own registry hive (HKCU of that account), so
//! it is read and changed there; the portal only keeps a short-lived copy (see
//! [`desktop_mode_cached`]). A change applies the next time a session starts; a
//! session already open keeps its mode.
//!
//! Changing another account's hive needs administrator rights, so the portal
//! must run as an administrator for this to work.
//!
//! Everything here calls Windows directly (see `win.rs`); only "Fix everything"
//! (`host_setup.ps1`) and the launcher's closer still use PowerShell.

use anyhow::{bail, Context, Result};
use serde::Deserialize;

/// One change at a time per Windows account (two at once could load the same
/// hive twice). Different accounts don't wait for each other.
#[cfg_attr(not(windows), allow(dead_code))]
async fn lock_account(name: &str) -> tokio::sync::OwnedMutexGuard<()> {
    static LOCKS: std::sync::LazyLock<std::sync::Mutex<std::collections::HashMap<String, std::sync::Arc<tokio::sync::Mutex<()>>>>> =
        std::sync::LazyLock::new(Default::default);
    let lock = LOCKS.lock().unwrap().entry(name.to_lowercase()).or_default().clone();
    lock.lock_owned().await
}

/// Run blocking Windows calls off the async threads.
async fn blocking<T: Send + 'static>(f: impl FnOnce() -> T + Send + 'static) -> Result<T> {
    tokio::task::spawn_blocking(f).await.context("a Windows step stopped unexpectedly")
}

/// The account's mode: `"app"`, `"full"`, or `"missing"` when no local Windows
/// account has that name.
pub async fn desktop_mode(win_username: &str) -> Result<String> {
    remember(win_username, run("read", win_username, None, None).await)
}

/// [`desktop_mode`], answered from memory when it was read in the last
/// [`MODE_TTL`]. Changes made through the portal update the copy at once.
pub async fn desktop_mode_cached(win_username: &str) -> Result<String> {
    let key = win_username.to_lowercase();
    if let Some((mode, at)) = MODES.lock().unwrap().get(&key) {
        if at.elapsed() < MODE_TTL {
            return Ok(mode.clone());
        }
    }
    desktop_mode(win_username).await
}

/// How long a desktop mode read from Windows is trusted.
const MODE_TTL: std::time::Duration = std::time::Duration::from_secs(60);

static MODES: std::sync::LazyLock<std::sync::Mutex<std::collections::HashMap<String, (String, std::time::Instant)>>> =
    std::sync::LazyLock::new(Default::default);

fn remember(win_username: &str, mode: Result<String>) -> Result<String> {
    let key = win_username.to_lowercase();
    let mut modes = MODES.lock().unwrap();
    match &mode {
        Ok(m) => {
            modes.insert(key, (m.clone(), std::time::Instant::now()));
        }
        Err(_) => {
            modes.remove(&key);
        }
    }
    mode
}

/// Switch the account to a full desktop (`true`) or restrict it (`false`).
/// Returns the mode now in effect.
pub async fn set_desktop_mode(win_username: &str, full: bool, launch: Option<&Launch>) -> Result<String> {
    let launch = if full { None } else { launch };
    #[cfg(windows)]
    if launch.is_some() {
        // Once per run is enough (it also sets the folder's permissions).
        static INSTALLED: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);
        let present = shell_dir().join("app-shell.vbs").exists() && shell_dir().join("app-close.ps1").exists();
        if !(present && INSTALLED.load(std::sync::atomic::Ordering::Relaxed)) {
            blocking(install_launcher_files).await??;
            INSTALLED.store(true, std::sync::atomic::Ordering::Relaxed);
        }
    }
    remember(win_username, run(if full { "full" } else { "app" }, win_username, None, launch).await)
}

/// The program a restricted (black-screen) account opens.
#[derive(Clone, Debug, Default)]
pub struct Launch {
    /// A program or a shortcut to one.
    pub program: String,
    /// The app's data folder (for Miracle, the company folder it opens).
    pub work_dir: Option<String>,
    /// "miracle", or None for any other program (see [`PROFILE_MIRACLE`]).
    pub profile: Option<String>,
}

/// The Miracle accounting program: opened on the app's company folder and
/// closed the way Miracle needs (see CLOSER_PS1).
pub const PROFILE_MIRACLE: &str = "miracle";

/// Create a local Windows account for a portal user: a standard user in the
/// "Portal RDP Users" group, password never expires, restricted.
pub async fn create_account(win_username: &str, password: &str) -> Result<()> {
    check_account_name(win_username)?;
    remember(win_username, run("create", win_username, Some(password), None).await).map(|_| ())
}

/// Delete a Windows account the portal manages (a member of "Portal RDP Users",
/// never an administrator) and its profile folder. Sessions it still has are
/// signed out first. Returns the profile folder when Windows could not remove
/// it (it is left on disk).
pub async fn delete_account(win_username: &str) -> Result<Option<String>> {
    #[cfg(windows)]
    {
        let _one_at_a_time = lock_account(win_username).await;
        let name = win_username.to_string();
        let Some(user) = blocking(move || imp::managed_for_delete(&name)).await?? else {
            remember(win_username, Ok("missing".to_string()))?;
            return Ok(None);
        };
        let left = delete_signed_out(user, true).await?;
        remember(win_username, Ok("missing".to_string()))?;
        Ok(left)
    }
    #[cfg(not(windows))]
    {
        let _ = win_username;
        bail!("Windows accounts can only be changed when the portal runs on Windows.")
    }
}

/// Sign the account out of Windows, then delete it (and, with `files`, its
/// profile folder, retried for up to 10 s while Windows lets go of it).
#[cfg(windows)]
async fn delete_signed_out(user: crate::win::UserInfo, files: bool) -> Result<Option<String>> {
    let name = user.name.clone();
    if blocking(move || sign_out_blocking(&name)).await? > 0 {
        crate::progress::step(format!("Signed {} out of Windows…", user.name));
    }
    crate::progress::step(format!("Deleting the Windows account {}…", user.name));
    let left = blocking(move || imp::delete_with_profile(&user, files)).await??;
    Ok(left)
}

/// Windows' rules for local account names.
pub fn check_account_name(name: &str) -> Result<()> {
    let bad = |c: char| c.is_control() || "\"/\\[]:;|=,+*?<>@".contains(c);
    if name.is_empty() || name.chars().count() > 20 || name.ends_with('.') || name.trim() != name || name.chars().any(bad) {
        bail!("A Windows account name has 1–20 characters, no spaces at the ends, and none of \" / \\ [ ] : ; | = , + * ? < > @");
    }
    Ok(())
}

#[cfg(windows)]
async fn run(action: &str, win_username: &str, password: Option<&str>, launch: Option<&Launch>) -> Result<String> {
    let _one_at_a_time = lock_account(win_username).await;
    let (action, name) = (action.to_string(), win_username.to_string());
    let password = password.map(str::to_string);
    let launch = launch.cloned();
    blocking(move || imp::mode(&action, &name, password.as_deref(), launch.as_ref())).await?
}

#[cfg(not(windows))]
async fn run(_action: &str, _win_username: &str, _password: Option<&str>, _launch: Option<&Launch>) -> Result<String> {
    bail!("Windows accounts can only be changed when the portal runs on Windows.")
}

// ---------------------------------------------------------------------------
// Every local Windows account, not only the portal's
// ---------------------------------------------------------------------------

/// The local Windows accounts on this computer, except Windows' built-in ones
/// (Administrator, Guest, DefaultAccount, WDAGUtilityAccount). Each is
/// `{name, full_name, description, enabled, is_admin, portal, me, microsoft,
/// last_logon}`; `me` is the account the portal runs as (the computer's owner).
pub async fn list_accounts() -> Result<Vec<serde_json::Value>> {
    #[cfg(windows)]
    return blocking(imp::list).await?;
    #[cfg(not(windows))]
    bail!("Windows accounts can only be changed when the portal runs on Windows.")
}

/// Changes to one account; `None` leaves that detail as it is.
#[derive(Default, Clone)]
pub struct AccountChanges {
    pub full_name: Option<String>,
    pub description: Option<String>,
    pub enabled: Option<bool>,
    pub password: Option<String>,
}

/// Change an account's details. Built-in accounts are refused, and so is
/// turning off the account the portal runs as.
pub async fn update_account(name: &str, changes: &AccountChanges) -> Result<()> {
    #[cfg(windows)]
    {
        let _one_at_a_time = lock_account(name).await;
        let (name, changes) = (name.to_string(), changes.clone());
        blocking(move || imp::update(&name, &changes)).await?
    }
    #[cfg(not(windows))]
    {
        let _ = (name, changes);
        bail!("Windows accounts can only be changed when the portal runs on Windows.")
    }
}

/// Delete any local account except Windows administrators, built-in accounts
/// and the account the portal runs as; with `files`, its profile folder too.
/// Sessions it has open are signed out first (their unsaved work is lost).
/// Returns the profile folder when it had to be left on disk.
pub async fn delete_any_account(name: &str, files: bool) -> Result<Option<String>> {
    #[cfg(windows)]
    {
        let _one_at_a_time = lock_account(name).await;
        let who = name.to_string();
        // The protected accounts are refused before anything is touched.
        let user = blocking(move || imp::can_delete(&who)).await??;
        let left = delete_signed_out(user, files).await?;
        remember(name, Ok("missing".to_string()))?;
        Ok(left)
    }
    #[cfg(not(windows))]
    {
        let _ = (name, files);
        bail!("Windows accounts can only be changed when the portal runs on Windows.")
    }
}

/// Whether `password` is the account's Windows password. Only for local
/// accounts; a Microsoft account's password can't be checked here.
pub async fn check_password(name: &str, password: &str) -> Result<bool> {
    #[cfg(windows)]
    {
        let (name, password) = (name.to_string(), password.to_string());
        blocking(move || imp::check(&name, &password)).await?
    }
    #[cfg(not(windows))]
    {
        let _ = (name, password);
        bail!("Windows accounts can only be changed when the portal runs on Windows.")
    }
}

/// The Windows side of the account functions above. Every rule and message is
/// the one the PowerShell scripts had before.
#[cfg(windows)]
mod imp {
    use super::{shell_dir, AccountChanges, Launch};
    use crate::win::{self, Hive, Key, UserInfo};
    use anyhow::{anyhow, bail, Result};
    use serde_json::{json, Value};
    use std::path::PathBuf;

    pub const PORTAL_GROUP: &str = "Portal RDP Users";
    const NOT_ADMIN: &str = "The portal is not running as an administrator, so it cannot read or change Windows accounts.";

    /// Administrator, Guest, DefaultAccount, WDAGUtilityAccount.
    fn builtin(sid: &str) -> bool {
        ["-500", "-501", "-503", "-504"].iter().any(|rid| sid.ends_with(rid))
    }

    fn admin_sids() -> Result<Vec<String>> {
        Ok(win::administrators()?.into_iter().map(|(sid, _)| sid).collect())
    }

    fn portal_sids() -> Result<Option<Vec<String>>> {
        Ok(win::group_members(PORTAL_GROUP)?.map(|m| m.into_iter().map(|(sid, _)| sid).collect()))
    }

    /// The account, refusing missing and built-in ones (the accounts script's rules).
    fn existing(name: &str) -> Result<UserInfo> {
        let Some(user) = win::user_info(name)? else {
            bail!("There is no Windows account named {name} on this computer.");
        };
        if builtin(&user.sid) {
            bail!("{} is one of Windows' built-in accounts; the portal leaves those alone.", user.name);
        }
        Ok(user)
    }

    fn require_admin() -> Result<()> {
        if !win::is_elevated() {
            bail!(NOT_ADMIN);
        }
        Ok(())
    }

    pub fn list() -> Result<Vec<Value>> {
        require_admin()?;
        let me = win::my_sid()?;
        let admins = admin_sids()?;
        let portal = portal_sids()?.unwrap_or_default();
        let mut list = Vec::new();
        for name in win::user_names()? {
            let Some(u) = win::user_info(&name)? else { continue };
            if builtin(&u.sid) {
                continue;
            }
            list.push(json!({
                "name": u.name,
                "full_name": u.full_name,
                "description": u.comment,
                "enabled": u.enabled(),
                "is_admin": admins.contains(&u.sid),
                "portal": portal.contains(&u.sid),
                "me": u.sid == me,
                "microsoft": win::is_microsoft(&u.name),
                "last_logon": if u.last_logon == 0 { Value::Null } else { json!(i64::from(u.last_logon)) },
            }));
        }
        Ok(list)
    }

    pub fn check(name: &str, password: &str) -> Result<bool> {
        require_admin()?;
        let user = existing(name)?;
        Ok(win::check_password(&user.name, password))
    }

    pub fn update(name: &str, changes: &AccountChanges) -> Result<()> {
        require_admin()?;
        let user = existing(name)?;
        let name = user.name.as_str();
        if changes.enabled == Some(false) && user.sid == win::my_sid()? {
            bail!("{name} is the account Access Portal runs as; it can't be turned off.");
        }
        if let Some(v) = &changes.full_name {
            win::set_full_name(name, v)?;
        }
        if let Some(v) = &changes.description {
            if v.encode_utf16().count() > 48 {
                bail!("Windows allows at most 48 characters in a description.");
            }
            win::set_comment(name, v)?;
        }
        if let Some(v) = &changes.password {
            if win::is_microsoft(name) {
                bail!("{name} signs in with a Microsoft account; change its password at account.microsoft.com.");
            }
            win::set_password(name, v)?;
        }
        if let Some(on) = changes.enabled {
            win::set_enabled(name, on)?;
        }
        Ok(())
    }

    /// The accounts script's delete rules: never the portal's own account,
    /// never an administrator.
    pub fn can_delete(name: &str) -> Result<UserInfo> {
        require_admin()?;
        let user = existing(name)?;
        if user.sid == win::my_sid()? {
            bail!("{} is the account Access Portal runs as; it can't be deleted.", user.name);
        }
        if admin_sids()?.contains(&user.sid) {
            bail!("{} is a Windows administrator account; the portal never deletes those.", user.name);
        }
        Ok(user)
    }

    /// The account script's delete rules for a portal user's own account: only
    /// members of "Portal RDP Users", never administrators. `None` when the
    /// account doesn't exist (nothing to delete).
    pub fn managed_for_delete(name: &str) -> Result<Option<UserInfo>> {
        let Some(user) = win::user_info(name)? else { return Ok(None) };
        require_admin()?;
        if admin_sids()?.contains(&user.sid) {
            bail!("{name} is a Windows administrator account; the portal never deletes those.");
        }
        if !portal_sids()?.unwrap_or_default().contains(&user.sid) {
            bail!("{name} isn't in the '{PORTAL_GROUP}' group, so the portal doesn't manage it and won't delete it.");
        }
        Ok(Some(user))
    }

    /// Delete the account, then (with `files`) its profile. Windows can take a
    /// few seconds to let go of a profile after its last sign-out, so the
    /// removal is retried for up to 10 s. Returns the folder if it is still
    /// there afterwards.
    pub fn delete_with_profile(user: &UserInfo, files: bool) -> Result<Option<String>> {
        let folder = win::profile_path(&user.sid);
        win::delete_user(&user.name)?;
        if !files {
            return Ok(None);
        }
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
        let mut last_error = 0;
        while win::has_profile(&user.sid) {
            match win::delete_profile(&user.sid) {
                Ok(()) => break,
                Err(code) => last_error = code,
            }
            if std::time::Instant::now() >= deadline {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(500));
        }
        match folder.filter(|f| f.exists()) {
            Some(f) => {
                let shown = f.display().to_string();
                tracing::warn!(
                    "deleted Windows account {} but its profile folder {shown} is still there ({})",
                    user.name,
                    if last_error == 0 { "no error from Windows".to_string() } else { win::error_text(last_error) }
                );
                Ok(Some(shown))
            }
            None => {
                tracing::info!("deleted Windows account {} and its profile", user.name);
                Ok(None)
            }
        }
    }

    // --- desktop mode ------------------------------------------------------

    // A restricted account can run one program on a black screen instead of the
    // desktop: its shell is a small launcher that starts the program the portal
    // chose for it and signs the session out when the program closes. Windows
    // Home delivers but refuses to launch an RDP "alternate shell", so the
    // launch is done through the account's own shell, which Home does run.
    const OLD_SHELL: &str = r"*\access-portal\*-shell.vbs*";
    const MARKER: &str = r"Software\AccessPortal";
    const WINLOGON: &str = r"Software\Microsoft\Windows NT\CurrentVersion\Winlogon";
    const EXPLORER: &str = r"Software\Microsoft\Windows\CurrentVersion\Policies\Explorer";
    const SYSTEM: &str = r"Software\Microsoft\Windows\CurrentVersion\Policies\System";
    const CMD_POLICY: &str = r"Software\Policies\Microsoft\Windows\System";
    const RUN: &str = r"Software\Microsoft\Windows\CurrentVersion\Run";
    const BLOCKED: [&str; 8] = ["cmd.exe", "powershell.exe", "pwsh.exe", "powershell_ise.exe", "regedit.exe", "mmc.exe", "taskmgr.exe", "wt.exe"];

    /// PowerShell's `-like` for patterns with `*` only: whole string, any case.
    pub fn like(text: &str, pattern: &str) -> bool {
        let (text, pattern) = (text.to_lowercase(), pattern.to_lowercase());
        let parts: Vec<&str> = pattern.split('*').collect();
        if parts.len() == 1 {
            return text == pattern;
        }
        let (first, last) = (parts[0], parts[parts.len() - 1]);
        if !text.starts_with(first) || text.len() < first.len() + last.len() || !text.ends_with(last) {
            return false;
        }
        let mut rest = &text[first.len()..text.len() - last.len()];
        for part in &parts[1..parts.len() - 1] {
            match rest.find(part) {
                Some(i) => rest = &rest[i + part.len()..],
                None => return false,
            }
        }
        true
    }

    /// read, create, app (restricted, with or without a black-screen program)
    /// or full. Returns the mode now in effect, or "missing".
    pub fn mode(action: &str, name: &str, password: Option<&str>, launch: Option<&Launch>) -> Result<String> {
        let elevated = win::is_elevated();
        let mut user = win::user_info(name)?;
        let mut action = action;
        if action == "create" {
            if !elevated {
                bail!("The portal is not running as an administrator, so it cannot create Windows accounts.");
            }
            if user.is_some() {
                bail!("A Windows account named {name} already exists on this computer.");
            }
            if win::group_members(PORTAL_GROUP)?.is_none() {
                bail!("This computer has no '{PORTAL_GROUP}' group yet, so a new account could not sign in over Remote Desktop.");
            }
            win::add_user(name, password.unwrap_or(""), "access-portal RDP user")?;
            if let Err(e) = win::add_to_group(PORTAL_GROUP, name) {
                // Leave nothing half done.
                let _ = win::delete_user(name);
                return Err(e);
            }
            user = win::user_info(name)?;
            action = "app"; // new accounts start restricted
        }
        let Some(user) = user else { return Ok("missing".into()) };
        let sid = user.sid.as_str();
        if !elevated {
            bail!(NOT_ADMIN);
        }
        let is_admin = admin_sids()?.iter().any(|s| s == sid);
        if action == "app" && is_admin {
            // Never lock down an administrator account.
            bail!("{name} is a Windows administrator account; it can't be restricted.");
        }
        // Lean sessions, for accounts the portal manages (see `lean`).
        let managed = !is_admin && portal_sids()?.unwrap_or_default().iter().any(|s| s == sid);

        let profile_dir = if win::has_profile(sid) {
            win::profile_path(sid).ok_or_else(|| anyhow!("Could not open the settings of {name}."))?
        } else {
            // Never signed in, so nothing has been set: Windows would give a full desktop.
            if action != "app" {
                return Ok("full".into());
            }
            let made = win::create_profile(sid, name)?;
            win::profile_path(sid).unwrap_or(made)
        };

        // The account's settings: already open if it is signed in, else its file.
        let Some(hive) = Hive::open(sid, &profile_dir)? else {
            bail!("Could not open the settings of {name}.");
        };
        let root = hive.root();
        if action == "app" || action == "full" {
            if let Some(k) = root.open_sub(WINLOGON, true)? {
                let current = k.get_string("Shell").unwrap_or_default();
                if like(&current, OLD_SHELL) || like(&current, r"*\app-shell.vbs*") {
                    k.delete_value("Shell")?;
                }
            }
        }
        if action == "app" {
            restrict(root, launch)?;
        } else if action == "full" {
            unrestrict(root)?;
        }
        if managed && (action == "app" || action == "full") {
            lean(root)?;
        }

        let restricted = root
            .open_sub(MARKER, false)?
            .is_some_and(|k| k.get_dword("Restricted") == Some(1) || k.get_string("Restricted").as_deref() == Some("1"));
        let shell = root.open_sub(WINLOGON, false)?.and_then(|k| k.get_string("Shell")).unwrap_or_default();
        // Accounts restricted by older versions have the old shell but no marker.
        let mode = if restricted || like(&shell, OLD_SHELL) { "app" } else { "full" };
        drop(hive);
        Ok(mode.into())
    }

    fn restrict(root: &Key, launch: Option<&Launch>) -> Result<()> {
        root.create_sub(MARKER)?.set_dword("Restricted", 1)?;
        let k = root.create_sub(EXPLORER)?;
        for v in ["NoRun", "NoControlPanel", "NoWinKeys", "NoViewContextMenu", "DisallowRun"] {
            k.set_dword(v, 1)?;
        }
        k.set_dword("NoDrives", 4)?;
        k.set_dword("NoViewOnDrive", 4)?;
        drop(k);
        let k = root.create_sub(&format!(r"{EXPLORER}\DisallowRun"))?;
        for (i, exe) in BLOCKED.iter().enumerate() {
            k.set_string(&(i + 1).to_string(), exe)?;
        }
        drop(k);
        let k = root.create_sub(SYSTEM)?;
        k.set_dword("DisableTaskMgr", 1)?;
        k.set_dword("DisableRegistryTools", 1)?;
        drop(k);
        root.create_sub(CMD_POLICY)?.set_dword("DisableCMD", 2)?;

        // With a program, the account's shell becomes the launcher (black screen,
        // just that program). Without one, no custom shell: a plain locked desktop.
        let winlogon = root.create_sub(WINLOGON)?;
        let Some(launch) = launch.filter(|l| !l.program.is_empty()) else {
            if let Some(m) = root.open_sub(MARKER, true)? {
                for v in ["Program", "Args", "WorkDir", "Profile"] {
                    m.delete_value(v)?;
                }
            }
            return Ok(());
        };
        // A shortcut (such as one Miracle makes for a company folder) is opened
        // here, as administrator, so the account needs no access to wherever the
        // shortcut file lives; its target, arguments and folder are what it runs.
        let mut program = launch.program.trim().trim_matches('"').to_string();
        let mut args = String::new();
        let mut dir = String::new();
        if program.to_lowercase().ends_with(".lnk") {
            if !std::path::Path::new(&program).exists() {
                bail!("The shortcut '{program}' does not exist.");
            }
            (program, args, dir) = win::resolve_shortcut(std::path::Path::new(&program))?;
        }
        if program.is_empty() || !std::path::Path::new(&program).exists() {
            bail!("The program '{program}' does not exist.");
        }
        if dir.is_empty() {
            dir = std::path::Path::new(&program).parent().map(|p| p.display().to_string()).unwrap_or_default();
        }
        // Miracle opens a company folder given as its third argument (what its
        // own "create shortcut" writes), so miracle.exe plus the app's data
        // folder is enough; a shortcut's own arguments win when there are some.
        let profile = launch.profile.clone().unwrap_or_default();
        if let Some(work) = launch.work_dir.as_deref().filter(|w| !w.is_empty()) {
            if profile.eq_ignore_ascii_case("miracle") && args.is_empty() {
                args = format!("02 \"0\" \"{}\"", work.trim_end_matches('\\'));
            }
        }
        let launcher: PathBuf = shell_dir().join("app-shell.vbs");
        if !launcher.exists() {
            bail!("The launcher {} is missing.", launcher.display());
        }
        let m = root.create_sub(MARKER)?;
        m.set_string("Program", &program)?;
        m.set_string("Args", &args)?;
        m.set_string("WorkDir", &dir)?;
        m.set_string("Profile", &profile)?;
        winlogon.set_string("Shell", &format!("wscript.exe \"{}\"", launcher.display()))?;
        Ok(())
    }

    /// Remove exactly what [`restrict`] sets; other settings of the account stay
    /// (the lean-session values too).
    fn unrestrict(root: &Key) -> Result<()> {
        let remove: [(&str, &[&str]); 4] = [
            (MARKER, &["Restricted", "Program", "Args", "WorkDir", "Profile"]),
            (EXPLORER, &["NoRun", "NoControlPanel", "NoWinKeys", "NoViewContextMenu", "DisallowRun", "NoDrives", "NoViewOnDrive"]),
            (SYSTEM, &["DisableTaskMgr", "DisableRegistryTools"]),
            (CMD_POLICY, &["DisableCMD"]),
        ];
        for (path, values) in remove {
            if let Some(k) = root.open_sub(path, true)? {
                for v in values {
                    k.delete_value(v)?;
                }
            }
        }
        if let Some(k) = root.open_sub(EXPLORER, true)? {
            k.delete_tree("DisallowRun")?;
        }
        Ok(())
    }

    /// Lean sessions: a portal account's sign-in skips the programs installed
    /// for every user of the computer (the machine's Run and RunOnce lists:
    /// AnyDesk, the Tailscale tray, OneDrive setup and the like), and OneDrive
    /// doesn't start for it. Sessions start faster and use less memory. Applied
    /// in both modes and never undone by the Full desktop switch.
    fn lean(root: &Key) -> Result<()> {
        let k = root.create_sub(EXPLORER)?;
        k.set_dword("DisableLocalMachineRun", 1)?;
        k.set_dword("DisableLocalMachineRunOnce", 1)?;
        drop(k);
        if let Some(k) = root.open_sub(RUN, true)? {
            k.delete_value("OneDrive")?;
            k.delete_value("OneDriveSetup")?;
        }
        Ok(())
    }

}

// ---------------------------------------------------------------------------
// This computer's Remote Desktop settings
// ---------------------------------------------------------------------------

/// Remote Desktop settings the portal's sessions are tuned for, applied to this
/// machine on start (needs administrator rights; skipped otherwise):
///
/// - encode Windows' H.264 on the graphics card when there is one;
/// - render sessions on the graphics card instead of in software;
/// - let sessions run at 60 frames a second (Windows' default is 30);
/// - skip the privacy-settings screen a new account sees at its first sign-in;
/// - show the last signed-in account at the Windows sign-in screen (not "Other user").
///
/// Already-open sessions keep their old setup until their user signs out; the
/// frame rate applies once the Remote Desktop service restarts (a reboot).
pub fn tune_remote_desktop() {
    #[cfg(windows)]
    {
        let settings = [
            (r"SOFTWARE\Policies\Microsoft\Windows NT\Terminal Services", "AVCHardwareEncodePreferred", 1u32),
            (r"SOFTWARE\Policies\Microsoft\Windows NT\Terminal Services", "bEnumerateHWBeforeSW", 1),
            (r"SYSTEM\CurrentControlSet\Control\Terminal Server\WinStations", "DWMFRAMEINTERVAL", 15),
            // New accounts would otherwise start with Windows' privacy-settings screen.
            (r"SOFTWARE\Policies\Microsoft\Windows\OOBE", "DisablePrivacyExperience", 1),
            // Let sessions use the user's microphone and camera, and play sound.
            (r"SOFTWARE\Policies\Microsoft\Windows NT\Terminal Services", "fDisableAudioCapture", 0),
            (r"SOFTWARE\Policies\Microsoft\Windows NT\Terminal Services", "fDisableCam", 0),
            (r"SOFTWARE\Policies\Microsoft\Windows NT\Terminal Services", "fDisableCameraRedir", 0),
            // Show the last signed-in account on the sign-in screen instead of
            // a blank "Other user" box asking for a name. Only changes what the
            // screen shows: no automatic sign-in, no account or RDP changes.
            (r"SOFTWARE\Microsoft\Windows\CurrentVersion\Policies\System", "dontdisplaylastusername", 0),
        ];
        for (key, name, value) in settings {
            if set_machine_dword(key, name, value) {
                tracing::info!("remote desktop tuned: {name} = {value}");
            }
        }
    }
}

/// Set a DWORD under HKEY_LOCAL_MACHINE unless it already has that value.
/// Returns whether it was changed.
#[cfg(windows)]
fn set_machine_dword(key: &str, name: &str, value: u32) -> bool {
    use crate::win::{Key, HKEY_LOCAL_MACHINE};
    let current = Key::open(HKEY_LOCAL_MACHINE, key, false).ok().flatten().and_then(|k| k.get_dword(name));
    if current == Some(value) {
        return false;
    }
    match Key::create(HKEY_LOCAL_MACHINE, key).and_then(|k| k.set_dword(name, value)) {
        Ok(()) => true,
        Err(e) => {
            tracing::warn!("could not set {name} (the portal needs administrator rights for this): {e:#}");
            false
        }
    }
}

/// Sign a Windows account out of every session it has on this machine: its
/// apps close (unsaved work is lost). Returns how many sessions were signed out.
pub fn sign_out_blocking(win_username: &str) -> usize {
    #[cfg(windows)]
    {
        let ids: Vec<u32> = crate::win::sessions()
            .into_iter()
            .filter(|s| s.user.eq_ignore_ascii_case(win_username))
            .map(|s| s.id)
            .collect();
        let n = ids.into_iter().filter(|&id| crate::win::log_off(id, true)).count();
        tracing::info!("signed {win_username} out of Windows ({n} session(s))");
        n
    }
    #[cfg(not(windows))]
    {
        let _ = win_username;
        0
    }
}

/// The black-screen launcher, set as a restricted account's shell. It starts the
/// account's program (HKCU\Software\AccessPortal: Program, Args, WorkDir),
/// watches for the portal's close request and runs app-close.ps1 when one comes,
/// and signs the session out once the program has closed.
const LAUNCHER_VBS: &str = r##"Option Explicit
Dim sh, fso, here, prog, args, work, profile, cmd, ex, req, closer, started, asked, stamp, logPath, helper
Set sh = CreateObject("WScript.Shell")
Set fso = CreateObject("Scripting.FileSystemObject")
' A short log in the user's own temp folder, for diagnosing (admins can read it).
logPath = sh.ExpandEnvironmentStrings("%TEMP%") & "\access-portal-launcher.log"
Sub Note(text)
  On Error Resume Next
  Dim f
  Set f = fso.OpenTextFile(logPath, 8, True)
  f.WriteLine Now & "  " & text
  f.Close
End Sub
prog = ""
args = ""
work = ""
profile = ""
On Error Resume Next
prog = sh.RegRead("HKCU\Software\AccessPortal\Program")
args = sh.RegRead("HKCU\Software\AccessPortal\Args")
work = sh.RegRead("HKCU\Software\AccessPortal\WorkDir")
profile = sh.RegRead("HKCU\Software\AccessPortal\Profile")
On Error GoTo 0
If profile = "" Then profile = "generic"
If prog = "" Then
  sh.Run "explorer.exe"
  WScript.Quit
End If
If work <> "" Then sh.CurrentDirectory = work
cmd = """" & prog & """"
If args <> "" Then cmd = cmd & " " & args
' The portal asks for a safe close by writing this file (only admins can write
' there); app-close.ps1 then walks the program through its own exit.
here = fso.GetParentFolderName(WScript.ScriptFullName)
req = here & "\close\" & LCase(sh.ExpandEnvironmentStrings("%USERNAME%")) & ".req"
closer = here & "\app-close.ps1"
started = Now
asked = ""
Set ex = sh.Exec(cmd)
Note "started " & cmd & " (pid " & ex.ProcessID & ", profile " & profile & ")"
Do While ex.Status = 0
  WScript.Sleep 250
  If fso.FileExists(req) Then
    stamp = CStr(fso.GetFile(req).DateLastModified)
    If CDate(stamp) >= started And stamp <> asked Then
      asked = stamp
      Note "close requested (" & stamp & "): running app-close.ps1"
      ' Exec, not Run: Run goes through the shell, which refuses programs the
      ' account's restrictions block (powershell is one); conhost --headless
      ' keeps the closer's console from showing.
      On Error Resume Next
      Set helper = sh.Exec("conhost.exe --headless powershell.exe -NoProfile -NonInteractive -ExecutionPolicy Bypass -File """ & closer & """ -ProcessId " & ex.ProcessID & " -AppProfile " & profile)
      If Err.Number <> 0 Then
        Note "could not start app-close.ps1: " & Err.Description
        Err.Clear
      Else
        Do While helper.Status = 0
          WScript.Sleep 300
        Loop
        Note "app-close.ps1 finished (exit " & helper.ExitCode & ")"
      End If
      On Error GoTo 0
    End If
  End If
Loop
Note "program closed: signing out"
' The program has closed: end the session.
sh.Run "shutdown.exe /l", 0, False"##;

/// Closes the program the way a person would, so it shuts down cleanly (an
/// accounting program killed with its tables open can corrupt them). If the
/// program is still running afterwards it has unsaved work open, and it is left
/// alone. Per profile:
///
/// - generic: one close request (WM_CLOSE), like clicking the window's X. A
///   program that asks "save changes?" keeps waiting, so it counts as busy.
/// - miracle (tested on Miracle 9.0): Esc closes lists and reports; on a form
///   with unsaved changes it only asks "Exit without save?" and a second Esc
///   answers No, so unsaved work is never thrown away. Then the close request,
///   and one more Esc skips Miracle's backup-on-exit question (or dismisses
///   "can't close while windows are open"). Enter is never sent: it is the only
///   key that could confirm discarding work.
const CLOSER_PS1: &str = r##"param([int]$ProcessId, [string]$AppProfile = 'generic')
Add-Type -TypeDefinition @"
using System; using System.Runtime.InteropServices;
public static class ApClose {
  delegate bool EP(IntPtr h, IntPtr l);
  [DllImport("user32.dll")] static extern bool EnumWindows(EP cb, IntPtr l);
  [DllImport("user32.dll")] static extern int GetWindowThreadProcessId(IntPtr h, out int p);
  [DllImport("user32.dll")] static extern bool IsWindowVisible(IntPtr h);
  [DllImport("user32.dll")] public static extern bool PostMessage(IntPtr h, uint m, IntPtr w, IntPtr l);
  public static IntPtr MainOf(int pid) {
    IntPtr f = IntPtr.Zero;
    EnumWindows((h, l) => { int p; GetWindowThreadProcessId(h, out p); if (p == pid && IsWindowVisible(h)) { f = h; return false; } return true; }, IntPtr.Zero);
    return f;
  }
  public static void Esc(IntPtr h) {
    PostMessage(h, 0x100, (IntPtr)0x1B, (IntPtr)1);
    PostMessage(h, 0x101, (IntPtr)0x1B, (IntPtr)unchecked((int)0xC0000001));
  }
}
"@
$h = [ApClose]::MainOf($ProcessId)
if ($h -eq [IntPtr]::Zero) { exit 1 }
if ($AppProfile -eq 'miracle') {
  for ($i = 0; $i -lt 6; $i++) { [ApClose]::Esc($h); Start-Sleep -Milliseconds 800 }
  $h = [ApClose]::MainOf($ProcessId)
  if ($h -eq [IntPtr]::Zero) { exit 0 }
  [void][ApClose]::PostMessage($h, 0x10, [IntPtr]::Zero, [IntPtr]::Zero)
  Start-Sleep -Milliseconds 1500
  [ApClose]::Esc($h)
} else {
  [void][ApClose]::PostMessage($h, 0x10, [IntPtr]::Zero, [IntPtr]::Zero)
}"##;

/// The folder holding the launcher: readable by every user, writable by admins.
fn shell_dir() -> std::path::PathBuf {
    std::env::var_os("ProgramData")
        .map(std::path::PathBuf::from)
        .unwrap_or_else(|| r"C:\ProgramData".into())
        .join("AccessPortal-Shell")
}

/// Write the launcher, its closer and the close-request folder.
#[cfg(windows)]
fn install_launcher_files() -> Result<()> {
    use std::os::windows::process::CommandExt;
    let dir = shell_dir();
    std::fs::create_dir_all(dir.join("close")).with_context(|| format!("creating {}", dir.display()))?;
    // Users may read and run the launcher but never change it.
    let ok = std::process::Command::new("icacls")
        .arg(&dir)
        .args(["/inheritance:r", "/grant:r", "*S-1-5-32-544:(OI)(CI)F", "*S-1-5-18:(OI)(CI)F", "*S-1-5-32-545:(OI)(CI)RX", "/Q"])
        .creation_flags(0x0800_0000)
        .status()
        .is_ok_and(|s| s.success());
    if !ok {
        bail!("Could not set the permissions of {}.", dir.display());
    }
    let crlf = |s: &str| s.replace("\r\n", "\n").replace('\n', "\r\n");
    std::fs::write(dir.join("app-shell.vbs"), crlf(LAUNCHER_VBS)).context("writing the launcher")?;
    std::fs::write(dir.join("app-close.ps1"), crlf(CLOSER_PS1)).context("writing the launcher's closer")?;
    Ok(())
}

/// How a safe close of a user's black-screen program ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SafeClose {
    /// The account has no Windows session.
    NoSession,
    /// The program closed itself cleanly and the session ended.
    Closed,
    /// The program is still open: it has unsaved work (it refuses to close).
    Busy,
    /// The session isn't a black-screen one (a desktop): this doesn't apply.
    NotKiosk,
}

/// Where the portal asks a session's launcher to close its program.
fn close_request_path(win_username: &str) -> std::path::PathBuf {
    let base = std::env::var_os("ProgramData").map(std::path::PathBuf::from).unwrap_or_else(|| r"C:\ProgramData".into());
    base.join("AccessPortal-Shell").join("close").join(format!("{}.req", win_username.to_lowercase()))
}

/// Ask the launcher in this account's sessions to close its program safely (it
/// checks four times a second; sessions started by older versions check every
/// second). Doesn't wait. The request is a file only admins can
/// write; the launcher acts on each new one once.
pub fn request_close(win_username: &str) -> std::io::Result<()> {
    let path = close_request_path(win_username);
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    std::fs::write(&path, format!("{:?}", std::time::SystemTime::now()))
}

fn clear_close_request(win_username: &str) {
    let _ = std::fs::remove_file(close_request_path(win_username));
}

/// Session ids running the black-screen launcher: a `wscript.exe` whose
/// command line names app-shell.vbs. A wscript.exe whose command line can't be
/// read counts too, so a black-screen program is never mistaken for a desktop
/// (which would be signed out without the safe close).
pub fn kiosk_sessions_blocking() -> Vec<u32> {
    #[cfg(windows)]
    {
        let mut ids: Vec<u32> = crate::win::processes()
            .into_iter()
            .filter(|(_, exe)| exe.eq_ignore_ascii_case("wscript.exe"))
            .filter_map(|(pid, _)| {
                let session = crate::win::session_of(pid)?;
                match crate::win::command_line(pid) {
                    Some(line) => line.to_lowercase().contains("app-shell.vbs").then_some(session),
                    None => Some(session),
                }
            })
            .collect();
        ids.sort_unstable();
        ids.dedup();
        ids
    }
    #[cfg(not(windows))]
    Vec::new()
}

/// This account's sessions that run the black-screen launcher.
fn kiosk_sessions_of(win_username: &str) -> (usize, Vec<u32>) {
    let mine: Vec<u32> = windows_sessions_blocking()
        .into_iter()
        .filter(|s| s.user.eq_ignore_ascii_case(win_username))
        .map(|s| s.id)
        .collect();
    let kiosk = kiosk_sessions_blocking();
    let both = mine.iter().copied().filter(|id| kiosk.contains(id)).collect();
    (mine.len(), both)
}

/// The launcher's log (it writes to %TEMP% of the account): the profile's
/// Temp folder, and each session's own Temp subfolder (Remote Desktop gives
/// every session one).
fn launcher_logs(win_username: &str, sessions: &[u32]) -> Vec<std::path::PathBuf> {
    #[cfg(windows)]
    {
        const LOG: &str = "access-portal-launcher.log";
        let Some(dir) = crate::win::user_info(win_username).ok().flatten().and_then(|u| crate::win::profile_path(&u.sid)) else {
            return Vec::new();
        };
        let temp = dir.join("AppData").join("Local").join("Temp");
        let mut logs = vec![temp.join(LOG)];
        logs.extend(sessions.iter().map(|id| temp.join(id.to_string()).join(LOG)));
        logs
    }
    #[cfg(not(windows))]
    {
        let _ = (win_username, sessions);
        Vec::new()
    }
}

/// What was added to a log since `from` (moved on to its end).
fn log_since(path: &std::path::Path, from: &mut u64) -> String {
    use std::io::{Read, Seek, SeekFrom};
    let Ok(mut file) = std::fs::File::open(path) else { return String::new() };
    let len = file.metadata().map(|m| m.len()).unwrap_or(0);
    if len < *from {
        *from = 0; // a new log
    }
    let mut text = Vec::new();
    if file.seek(SeekFrom::Start(*from)).is_err() || file.read_to_end(&mut text).is_err() {
        return String::new();
    }
    *from += text.len() as u64;
    String::from_utf8_lossy(&text).into_owned()
}

/// How long a program may take to exit once the closer has walked it through
/// its exit; still running after that, it is holding unsaved work.
const AFTER_CLOSER: std::time::Duration = std::time::Duration::from_millis(2500);
/// The same for Miracle, which can take several seconds to save and close its
/// company files after its last window goes. Answering "unsaved work" too early
/// would invite a forced sign-out while it is still writing.
const AFTER_CLOSER_MIRACLE: std::time::Duration = std::time::Duration::from_secs(15);

/// Whether the newest program a launcher log says it started runs with the
/// Miracle profile (`started … (pid N, profile miracle)`).
fn started_miracle(log: &std::path::Path) -> bool {
    std::fs::read_to_string(log)
        .ok()
        .and_then(|text| text.lines().rev().find(|l| l.contains("  started ")).map(|l| l.contains("profile miracle)")))
        .unwrap_or(false)
}

/// Close this account's black-screen program the way a person would, and wait
/// up to `timeout` for it to end. Never forces the program: one with unsaved
/// work stays open and the answer is `Busy`. Once it has closed, the
/// account's black-screen sessions are ended too (the launcher's own sign-out
/// doesn't always end a disconnected session). `app` names the program in the
/// progress steps.
pub async fn close_safely(win_username: &str, app: &str, timeout: std::time::Duration) -> SafeClose {
    use std::time::{Duration, Instant};
    let who = win_username.to_string();
    let Ok((sessions, kiosk, logs)) = blocking(move || {
        let (sessions, kiosk) = kiosk_sessions_of(&who);
        let logs = launcher_logs(&who, &kiosk);
        (sessions, kiosk, logs)
    })
    .await
    else {
        return SafeClose::Busy;
    };
    if sessions == 0 {
        return SafeClose::NoSession;
    }
    if kiosk.is_empty() {
        return SafeClose::NotKiosk;
    }
    // Where the logs end now, so only what the launcher writes after the
    // request is read.
    let grace = if logs.iter().any(|p| started_miracle(p)) { AFTER_CLOSER_MIRACLE } else { AFTER_CLOSER };
    let mut logs: Vec<(std::path::PathBuf, u64)> = logs
        .into_iter()
        .map(|p| {
            let len = std::fs::metadata(&p).map(|m| m.len()).unwrap_or(0);
            (p, len)
        })
        .collect();
    crate::progress::step(format!("Asking {app} to close…"));
    if let Err(e) = request_close(win_username) {
        tracing::warn!("could not ask {win_username}'s program to close: {e}");
        return SafeClose::Busy;
    }
    let start = Instant::now();
    let mut closer_done: Option<Instant> = None;
    let mut program_closed = false;
    let mut shown = 0;
    let outcome = loop {
        tokio::time::sleep(Duration::from_millis(250)).await;
        let who = win_username.to_string();
        let running = blocking(move || kiosk_sessions_of(&who).1).await.unwrap_or_else(|_| vec![0]);
        if running.is_empty() {
            break SafeClose::Closed;
        }
        for (path, from) in &mut logs {
            let text = log_since(path, from);
            if text.contains("program closed") {
                program_closed = true; // the launcher is on its way out
            }
            if closer_done.is_none() && (text.contains("app-close.ps1 finished") || text.contains("could not start app-close.ps1")) {
                closer_done = Some(Instant::now());
            }
        }
        if !program_closed && closer_done.is_some_and(|at| at.elapsed() >= grace) {
            break SafeClose::Busy;
        }
        if start.elapsed() >= timeout {
            break SafeClose::Busy;
        }
        let secs = start.elapsed().as_secs();
        if secs > shown {
            shown = secs;
            crate::progress::step(format!("Waiting for {app} to close ({secs} s)…"));
        }
    };
    clear_close_request(win_username);
    tracing::info!("safe close of {win_username}'s program: {outcome:?} after {} ms", start.elapsed().as_millis());
    if outcome == SafeClose::Closed {
        end_sessions(win_username, &kiosk).await;
    }
    outcome
}

/// After a safe close: give the account's black-screen sessions 2 s to end by
/// themselves (the launcher runs `shutdown /l`), then sign out any that are
/// left (their program has already closed, so nothing is lost) and wait for
/// that. On a test laptop `shutdown /l` never ended the session, so a longer
/// wait would only add to every sign-out.
async fn end_sessions(win_username: &str, ids: &[u32]) {
    use std::time::{Duration, Instant};
    let left = || {
        let (who, ids) = (win_username.to_string(), ids.to_vec());
        blocking(move || {
            windows_sessions_blocking()
                .into_iter()
                .filter(|s| s.user.eq_ignore_ascii_case(&who) && ids.contains(&s.id))
                .map(|s| s.id)
                .collect::<Vec<u32>>()
        })
    };
    let deadline = Instant::now() + Duration::from_secs(2);
    let mut remaining = left().await.unwrap_or_default();
    while !remaining.is_empty() && Instant::now() < deadline {
        tokio::time::sleep(Duration::from_millis(250)).await;
        remaining = left().await.unwrap_or_default();
    }
    if remaining.is_empty() {
        return;
    }
    crate::progress::step(format!("Signing {win_username} out of Windows…"));
    tracing::info!("{win_username}'s program closed but session(s) {remaining:?} stayed; signing them out");
    let ids = remaining.clone();
    let n = blocking(move || log_off_blocking(&ids)).await.unwrap_or(0);
    let deadline = Instant::now() + Duration::from_secs(5);
    let mut remaining = left().await.unwrap_or_default();
    while !remaining.is_empty() && Instant::now() < deadline {
        tokio::time::sleep(Duration::from_millis(250)).await;
        remaining = left().await.unwrap_or_default();
    }
    if remaining.is_empty() {
        tracing::info!("signed {win_username} out of {n} session(s) after the safe close");
    } else {
        tracing::warn!("{win_username}'s session(s) {remaining:?} are still there after signing out");
    }
}

/// Give a Windows account exclusive access to one data folder: the account (and
/// administrators) may use it, and the folder stops inheriting the computer's
/// default "Users can read" access, so no other portal account can see inside.
/// The account keeps its normal read access to Windows and installed programs,
/// which is what lets the program run; it just cannot reach other data folders.
/// Idempotent, and additive across accounts (two accounts can share a folder).
pub async fn grant_folder(win_username: &str, dir: &str) -> anyhow::Result<()> {
    folder_acl(win_username, dir, "grant").await
}

/// Undo [`grant_folder`] for one account: remove its access to the folder. Other
/// accounts' access and the folder itself are left alone. Kept for when a folder
/// is unassigned; assignment today only grants.
#[allow(dead_code)]
pub async fn revoke_folder(win_username: &str, dir: &str) -> anyhow::Result<()> {
    folder_acl(win_username, dir, "revoke").await
}

#[cfg(windows)]
async fn folder_acl(win_username: &str, dir: &str, action: &str) -> anyhow::Result<()> {
    let (u, d, a) = (win_username.to_owned(), dir.to_owned(), action.to_owned());
    let full = blocking(move || folder_acl_blocking(&u, &d, &a)).await??;
    tracing::info!("folder access {action} for {win_username}: {dir}");
    // Repair anything an earlier version left without permissions: a walk of
    // every file, so it runs in the background, once per folder per run.
    static REPAIRED: std::sync::LazyLock<std::sync::Mutex<std::collections::HashSet<String>>> =
        std::sync::LazyLock::new(Default::default);
    if action == "grant" && REPAIRED.lock().unwrap().insert(full.to_lowercase()) {
        tokio::task::spawn_blocking(move || repair_tree(&full));
    }
    Ok(())
}

#[cfg(not(windows))]
async fn folder_acl(win_username: &str, dir: &str, _action: &str) -> anyhow::Result<()> {
    let _ = (win_username, dir);
    Ok(())
}

/// Grants or removes one account's exclusive access to one data folder with
/// `icacls`. Refuses drive roots and the Windows / program / profile regions so
/// a mistyped folder can never lock the operating system. Returns the folder's
/// full path.
#[cfg(windows)]
fn folder_acl_blocking(user: &str, dir: &str, action: &str) -> anyhow::Result<String> {
    if dir.trim().is_empty() {
        bail!("No folder was given.");
    }
    let path = std::path::Path::new(dir);
    if !path.is_dir() {
        bail!("The folder '{dir}' does not exist on this computer.");
    }
    let full = std::path::absolute(path).unwrap_or_else(|_| path.to_path_buf()).display().to_string();
    let full = full.trim_end_matches('\\').to_string();
    let lower = full.to_lowercase();
    if full.chars().count() <= 3 {
        bail!("Refusing to change permissions on a drive root ({full}).");
    }
    let var = |k: &str| std::env::var(k).ok().filter(|v| !v.is_empty());
    let protected = [
        var("SystemRoot"),
        var("ProgramFiles"),
        var("ProgramFiles(x86)"),
        var("ProgramData"),
        var("SystemDrive").map(|d| format!("{}\\Users", d.trim_end_matches('\\'))),
    ];
    for p in protected.into_iter().flatten() {
        let p = p.trim_end_matches('\\').to_lowercase();
        if lower == p || p.starts_with(&format!("{lower}\\")) {
            bail!("Refusing to change permissions on a system folder ({full}).");
        }
    }
    match action {
        "grant" => {
            // Inheritable rights on the folder itself, then stop it inheriting
            // from its parent (the computer's default "Users" read). Everything
            // inside inherits from this folder, so it all follows. Only the top
            // folder is changed: doing this to every file (/T) leaves files with
            // no permissions at all.
            // The administrators group alone is not enough: an administrator's
            // everyday programs (Explorer, Miracle) run without the group's
            // rights unless started "as administrator", so each administrator
            // account is named too, or it is locked out of its own data.
            let account = format!("{user}:(OI)(CI)M");
            let admins: Vec<String> = admin_accounts().iter().map(|sid| format!("*{sid}:(OI)(CI)F")).collect();
            let mut args = vec![full.as_str(), "/grant", "*S-1-5-32-544:(OI)(CI)F", "*S-1-5-18:(OI)(CI)F"];
            args.extend(admins.iter().map(String::as_str));
            args.extend([account.as_str(), "/C", "/Q"]);
            let code = icacls(&args);
            if code != 0 {
                bail!("Could not grant {user} access to {full} (icacls {code}).");
            }
            let code = icacls(&[&full, "/inheritance:r", "/C", "/Q"]);
            if code != 0 {
                bail!("Could not lock {full} to its users (icacls {code}).");
            }
        }
        "revoke" => {
            icacls(&[&full, "/remove:g", user, "/C", "/Q"]);
        }
        other => bail!("Unknown action {other}."),
    }
    Ok(full)
}

/// The administrator accounts that keep full use of every locked data folder:
/// the members of BUILTIN\Administrators and the account the portal runs as.
#[cfg(windows)]
fn admin_accounts() -> Vec<String> {
    let mut sids: Vec<String> = match crate::win::administrators() {
        Ok(members) => members.into_iter().map(|(sid, _)| sid).collect(),
        Err(e) => {
            tracing::warn!("reading the administrators for folder access: {e:#}");
            Vec::new()
        }
    };
    sids.extend(crate::win::my_sid().ok());
    sids.retain(|s| s.starts_with("S-1-") && s != "S-1-5-18");
    sids.sort();
    sids.dedup();
    sids
}

/// Give every administrator account back full use of the data folders an
/// earlier version locked to the administrators group only (which locked the
/// administrator out of everyday use). Only folders that are locked and still
/// miss an administrator are changed, so this does nothing once they are fixed.
#[cfg(windows)]
pub fn keep_admin_folder_access(dirs: Vec<String>) {
    let admins = admin_accounts();
    if admins.is_empty() {
        return;
    }
    let mut seen = std::collections::HashSet::new();
    for dir in dirs {
        let path = std::path::Path::new(dir.trim());
        if !path.is_dir() {
            continue;
        }
        let full = std::path::absolute(path).unwrap_or_else(|_| path.to_path_buf()).display().to_string();
        let full = full.trim_end_matches('\\').to_string();
        if !seen.insert(full.to_lowercase()) {
            continue;
        }
        let Some(access) = crate::win::folder_access(std::path::Path::new(&full)) else { continue };
        if !access.protected {
            continue; // never locked by the portal: it already follows its parent
        }
        let missing: Vec<String> =
            admins.iter().filter(|sid| !access.full.contains(sid)).map(|sid| format!("*{sid}:(OI)(CI)F")).collect();
        if missing.is_empty() {
            continue;
        }
        let mut args = vec![full.as_str(), "/grant"];
        args.extend(missing.iter().map(String::as_str));
        args.extend(["/C", "/Q"]);
        match icacls(&args) {
            0 => tracing::info!("gave {} administrator account(s) back access to {full}", missing.len()),
            code => tracing::warn!("could not give the administrators back access to {full} (icacls {code})"),
        }
    }
}

#[cfg(not(windows))]
pub fn keep_admin_folder_access(_dirs: Vec<String>) {}

/// Run a Windows tool with no window; its exit code (-1 if it didn't start).
#[cfg(windows)]
fn run_tool(program: &str, args: &[&str]) -> i32 {
    use std::os::windows::process::CommandExt;
    std::process::Command::new(program)
        .args(args)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .creation_flags(0x0800_0000)
        .status()
        .map(|s| s.code().unwrap_or(-1))
        .unwrap_or(-1)
}

#[cfg(windows)]
fn icacls(args: &[&str]) -> i32 {
    run_tool("icacls", args)
}

/// Give back normal (inherited) permissions to anything under `root` that has
/// none at all, as an earlier version could leave behind.
#[cfg(windows)]
fn repair_tree(root: &str) {
    let start = std::time::Instant::now();
    let (mut checked, mut fixed, mut failed) = (0usize, 0usize, 0usize);
    let mut stack = vec![std::path::PathBuf::from(root)];
    while let Some(dir) = stack.pop() {
        let Ok(entries) = std::fs::read_dir(&dir) else { continue };
        for entry in entries.flatten() {
            let path = entry.path();
            checked += 1;
            if crate::win::has_empty_dacl(&path) == Some(true) {
                let p = path.display().to_string();
                let mut ok = icacls(&[&p, "/reset", "/C", "/Q"]) == 0;
                if !ok {
                    run_tool("takeown", &["/F", &p, "/A"]);
                    ok = icacls(&[&p, "/reset", "/C", "/Q"]) == 0;
                }
                if ok {
                    fixed += 1;
                } else {
                    failed += 1;
                }
            }
            // Its own permissions are fixed first, so it can be listed now.
            if entry.file_type().is_ok_and(|t| t.is_dir() && !t.is_symlink()) {
                stack.push(path);
            }
        }
    }
    tracing::info!(
        "checked {checked} item(s) in {root} for missing permissions in {} ms: {fixed} repaired, {failed} could not be",
        start.elapsed().as_millis()
    );
}

/// A Windows session on this computer.
#[derive(Clone, Debug, Deserialize, serde::Serialize)]
pub struct WinSession {
    pub id: u32,
    pub user: String,
    /// 0 active, 1 connected, 4 disconnected, ... (WTS_CONNECTSTATE_CLASS).
    pub state: u32,
    /// Seconds since the session was disconnected (0 if it isn't).
    pub disconnected_secs: u64,
    /// The account is a Windows administrator.
    pub admin: bool,
}

pub const WTS_ACTIVE: u32 = 0;
pub const WTS_DISCONNECTED: u32 = 4;

/// Every signed-in Windows session on this computer.
pub fn windows_sessions_blocking() -> Vec<WinSession> {
    #[cfg(windows)]
    {
        let admins: Vec<String> = match crate::win::administrators() {
            Ok(members) => members.into_iter().map(|(_, name)| name).collect(),
            Err(e) => {
                tracing::warn!("listing Windows sessions: {e:#}");
                Vec::new()
            }
        };
        crate::win::sessions()
            .into_iter()
            .map(|s| WinSession {
                admin: admins.iter().any(|a| a.eq_ignore_ascii_case(&s.user)),
                id: s.id,
                user: s.user,
                state: s.state,
                disconnected_secs: s.disconnected_secs,
            })
            .collect()
    }
    #[cfg(not(windows))]
    Vec::new()
}

/// Sign these Windows sessions out (their apps close); waits for each to end.
pub fn log_off_blocking(ids: &[u32]) -> usize {
    #[cfg(windows)]
    {
        ids.iter().filter(|&&id| crate::win::log_off(id, true)).count()
    }
    #[cfg(not(windows))]
    {
        let _ = ids;
        0
    }
}

/// Open Windows' own viewer on a user's session, on this computer's screen:
/// watch it, or (with `control`) use its keyboard and mouse too. With
/// `ask`, the user is asked first and can refuse.
pub fn shadow(session_id: u32, control: bool, ask: bool) -> anyhow::Result<()> {
    #[cfg(windows)]
    {
        let mut command = std::process::Command::new("mstsc.exe");
        command.arg(format!("/shadow:{session_id}"));
        if control {
            command.arg("/control");
        }
        if !ask {
            command.arg("/noConsentPrompt");
        }
        command.spawn().context("starting Remote Desktop Connection")?;
        Ok(())
    }
    #[cfg(not(windows))]
    {
        let _ = (session_id, control, ask);
        bail!("Viewing a session needs Windows.")
    }
}

/// Windows' rule for viewing sessions: with the user's permission (1), or
/// without asking (2). Both allow control.
pub fn set_shadow_policy(ask: bool) {
    #[cfg(windows)]
    {
        let value = if ask { 1 } else { 2 };
        let key = r"SOFTWARE\Policies\Microsoft\Windows NT\Terminal Services";
        let ok = crate::win::Key::create(crate::win::HKEY_LOCAL_MACHINE, key).and_then(|k| k.set_dword("Shadow", value));
        if let Err(e) = ok {
            tracing::warn!("could not set the screen-viewing policy (needs administrator rights): {e:#}");
        }
    }
    #[cfg(not(windows))]
    let _ = ask;
}

/// What this computer needs to serve portal sessions, checked (and with `fix`,
/// repaired) by `host_setup.ps1`: one JSON object per check.
pub fn setup_blocking(fix: &[String]) -> anyhow::Result<Vec<serde_json::Value>> {
    let action = if fix.is_empty() { "check" } else { "fix" };
    let result = run_setup_script(action, fix, '[').and_then(|v| match v {
        serde_json::Value::Array(list) => Ok(list),
        other => bail!("unexpected answer from the setup script: {other}"),
    });
    // Into portal.log too, so a problem report shows what happened.
    match &result {
        Ok(checks) => {
            for c in checks {
                let id = c["id"].as_str().unwrap_or("?");
                let detail = c["detail"].as_str().unwrap_or("");
                if let Some(error) = c.get("error").and_then(|e| e.as_str()) {
                    tracing::warn!("setup {action} {id}: failed: {error} (now: {detail})");
                } else if fix.iter().any(|f| f.trim_end_matches("-repair") == id) {
                    tracing::info!("setup {action} {id}: ok={} ({detail})", c["ok"]);
                } else if c["ok"] != serde_json::Value::Bool(true) {
                    tracing::info!("setup {action} {id}: not ok ({detail})");
                }
            }
        }
        Err(e) => tracing::warn!("setup {action} {}: {e:#}", fix.join(",")),
    }
    result
}

/// Windows, Remote Desktop and antivirus facts for a problem report
/// (`host_setup.ps1` with `AP_ACTION=info`).
pub fn system_info_blocking() -> anyhow::Result<serde_json::Value> {
    run_setup_script("info", &[], '{')
}

/// Run `host_setup.ps1` and parse the last line of its output that starts with `open`.
fn run_setup_script(action: &str, fix: &[String], open: char) -> anyhow::Result<serde_json::Value> {
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        use std::sync::atomic::{AtomicU32, Ordering};
        static RUN: AtomicU32 = AtomicU32::new(0);
        // Too long for -EncodedCommand (the command line limit), so run it as a file.
        let path = std::env::temp_dir().join(format!(
            "access-portal-setup-{}-{}.ps1",
            std::process::id(),
            RUN.fetch_add(1, Ordering::Relaxed)
        ));
        std::fs::write(&path, include_str!("host_setup.ps1")).context("writing the setup script")?;
        let ini_urls = std::env::var("ACCESS_PORTAL_SERVICE_URL")
            .map(|base| format!("{}/v1/rdpwrap.ini", base.trim_end_matches('/')))
            .unwrap_or_default();
        let out = std::process::Command::new("powershell.exe")
            .args(["-NoProfile", "-NonInteractive", "-ExecutionPolicy", "Bypass", "-File"])
            .arg(&path)
            .env("AP_ACTION", action)
            .env("AP_FIX", fix.join(","))
            .env("AP_INI_URLS", ini_urls)
            .creation_flags(0x0800_0000)
            .output();
        let _ = std::fs::remove_file(&path);
        let out = out.context("starting PowerShell")?;
        let stdout = String::from_utf8_lossy(&out.stdout);
        let last = stdout.lines().rev().find(|l| l.trim_start().starts_with(open)).unwrap_or("");
        serde_json::from_str(last.trim()).with_context(|| {
            format!("unexpected answer from the setup script: {}{}", stdout.trim(), String::from_utf8_lossy(&out.stderr).trim())
        })
    }
    #[cfg(not(windows))]
    {
        let _ = (action, fix, open);
        bail!("Setting up the computer needs Windows.")
    }
}

/// Free space on the drive holding `path`, in bytes (`None` if unknown).
pub fn disk_free(path: &std::path::Path) -> Option<u64> {
    #[cfg(windows)]
    {
        use std::os::windows::ffi::OsStrExt;
        extern "system" {
            fn GetDiskFreeSpaceExW(dir: *const u16, free_to_caller: *mut u64, total: *mut u64, free: *mut u64) -> i32;
        }
        let wide: Vec<u16> = path.as_os_str().encode_wide().chain(std::iter::once(0)).collect();
        let (mut avail, mut total, mut free) = (0u64, 0u64, 0u64);
        let ok = unsafe { GetDiskFreeSpaceExW(wide.as_ptr(), &mut avail, &mut total, &mut free) };
        (ok != 0).then_some(avail)
    }
    #[cfg(not(windows))]
    {
        let _ = path;
        None
    }
}
