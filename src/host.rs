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

use anyhow::{bail, Context, Result};
use serde::Deserialize;

/// Only one hive change at a time: two at once could load the same hive twice.
static LOCK: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

/// The account's mode: `"app"`, `"full"`, or `"missing"` when no local Windows
/// account has that name.
pub async fn desktop_mode(win_username: &str) -> Result<String> {
    remember(win_username, run("read", win_username, None, None, None).await)
}

/// [`desktop_mode`], answered from memory when it was read in the last
/// [`MODE_TTL`]. Reading it from Windows takes a second or two, too slow for
/// every page a user opens; changes made through the portal update the copy at
/// once.
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
pub async fn set_desktop_mode(
    win_username: &str,
    full: bool,
    program: Option<&str>,
    work_dir: Option<&str>,
) -> Result<String> {
    let (program, work_dir) = if full { (None, None) } else { (program, work_dir) };
    remember(
        win_username,
        run(if full { "full" } else { "app" }, win_username, None, program, work_dir).await,
    )
}

/// Create a local Windows account for a portal user: a standard user in the
/// "Portal RDP Users" group, password never expires, restricted.
pub async fn create_account(win_username: &str, password: &str) -> Result<()> {
    check_account_name(win_username)?;
    remember(win_username, run("create", win_username, Some(password), None, None).await).map(|_| ())
}

/// Delete a Windows account the portal manages (a member of "Portal RDP Users",
/// never an administrator) and its profile folder.
pub async fn delete_account(win_username: &str) -> Result<()> {
    remember(win_username, run("delete", win_username, None, None, None).await).map(|_| ())
}

/// Windows' rules for local account names.
pub fn check_account_name(name: &str) -> Result<()> {
    let bad = |c: char| c.is_control() || "\"/\\[]:;|=,+*?<>@".contains(c);
    if name.is_empty() || name.chars().count() > 20 || name.ends_with('.') || name.trim() != name || name.chars().any(bad) {
        bail!("A Windows account name has 1–20 characters, no spaces at the ends, and none of \" / \\ [ ] : ; | = , + * ? < > @");
    }
    Ok(())
}

#[derive(Deserialize)]
struct ScriptResult {
    ok: bool,
    mode: Option<String>,
    error: Option<String>,
}

#[cfg(windows)]
async fn run(
    action: &str,
    win_username: &str,
    password: Option<&str>,
    program: Option<&str>,
    work_dir: Option<&str>,
) -> Result<String> {
    use base64::Engine;
    use std::time::Duration;

    let _one_at_a_time = LOCK.lock().await;

    // -EncodedCommand takes the script as base64 UTF-16LE, which avoids every
    // quoting problem. The account name travels in an environment variable and
    // is only ever used as data inside the script.
    let utf16: Vec<u8> = SCRIPT.encode_utf16().flat_map(|u| u.to_le_bytes()).collect();
    let encoded = base64::engine::general_purpose::STANDARD.encode(utf16);

    const CREATE_NO_WINDOW: u32 = 0x0800_0000;
    let child = tokio::process::Command::new("powershell.exe")
        .args(["-NoProfile", "-NonInteractive", "-ExecutionPolicy", "Bypass", "-EncodedCommand"])
        .arg(encoded)
        .env("AP_WINUSER", win_username)
        .env("AP_ACTION", action)
        .env("AP_PASSWORD", password.unwrap_or(""))
        .env("AP_PROGRAM", program.unwrap_or(""))
        .env("AP_WORKDIR", work_dir.unwrap_or(""))
        .creation_flags(CREATE_NO_WINDOW)
        .kill_on_drop(true)
        .output();
    let out = tokio::time::timeout(Duration::from_secs(60), child)
        .await
        .context("changing the Windows account took too long")?
        .context("starting PowerShell")?;

    let stdout = String::from_utf8_lossy(&out.stdout);
    let last = stdout.lines().rev().find(|l| !l.trim().is_empty()).unwrap_or("");
    let result: ScriptResult = serde_json::from_str(last.trim()).with_context(|| {
        format!(
            "unexpected answer from the account script: {}{}",
            stdout.trim(),
            String::from_utf8_lossy(&out.stderr).trim()
        )
    })?;
    match (result.ok, result.mode, result.error) {
        (true, Some(mode), _) => Ok(mode),
        (_, _, Some(error)) => bail!(error),
        _ => bail!("the account script gave no answer"),
    }
}

#[cfg(not(windows))]
async fn run(
    _action: &str,
    _win_username: &str,
    _password: Option<&str>,
    _program: Option<&str>,
    _work_dir: Option<&str>,
) -> Result<String> {
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
    let out = run_accounts("list", "", &[]).await?;
    Ok(out.get("accounts").and_then(|a| a.as_array()).cloned().unwrap_or_default())
}

/// Changes to one account; `None` leaves that detail as it is.
#[derive(Default)]
pub struct AccountChanges {
    pub full_name: Option<String>,
    pub description: Option<String>,
    pub enabled: Option<bool>,
    pub password: Option<String>,
}

/// Change an account's details. Built-in accounts are refused, and so is
/// turning off the account the portal runs as.
pub async fn update_account(name: &str, changes: &AccountChanges) -> Result<()> {
    let mut set = Vec::new();
    let mut env = Vec::new();
    if let Some(v) = &changes.full_name {
        set.push("full_name");
        env.push(("AP_FULLNAME", v.clone()));
    }
    if let Some(v) = &changes.description {
        set.push("description");
        env.push(("AP_DESCRIPTION", v.clone()));
    }
    if let Some(v) = changes.enabled {
        set.push("enabled");
        env.push(("AP_ENABLED", if v { "1" } else { "0" }.to_string()));
    }
    if let Some(v) = &changes.password {
        set.push("password");
        env.push(("AP_PASSWORD", v.clone()));
    }
    env.push(("AP_SET", set.join(",")));
    run_accounts("update", name, &env).await.map(|_| ())
}

/// Delete any local account except Windows administrators, built-in accounts
/// and the account the portal runs as; with `files`, its profile folder too.
/// Sessions it has open are signed out first (their unsaved work is lost).
pub async fn delete_any_account(name: &str, files: bool) -> Result<()> {
    // The script refuses the protected accounts before anything is touched.
    run_accounts("can-delete", name, &[]).await?;
    let who = name.to_string();
    tokio::task::spawn_blocking(move || sign_out_blocking(&who)).await.ok();
    run_accounts("delete", name, &[("AP_FILES", if files { "1" } else { "0" }.to_string())])
        .await
        .map(|_| ())?;
    remember(name, Ok("missing".to_string())).map(|_| ())
}

/// Whether `password` is the account's Windows password. Only for local
/// accounts; a Microsoft account's password can't be checked here.
pub async fn check_password(name: &str, password: &str) -> Result<bool> {
    let out = run_accounts("check", name, &[("AP_PASSWORD", password.to_string())]).await?;
    Ok(out.get("valid").and_then(|v| v.as_bool()).unwrap_or(false))
}

#[cfg(windows)]
async fn run_accounts(action: &str, name: &str, env: &[(&str, String)]) -> Result<serde_json::Value> {
    use base64::Engine;
    use std::time::Duration;

    let _one_at_a_time = LOCK.lock().await;
    let utf16: Vec<u8> = ACCOUNTS_SCRIPT.encode_utf16().flat_map(|u| u.to_le_bytes()).collect();
    let encoded = base64::engine::general_purpose::STANDARD.encode(utf16);
    const CREATE_NO_WINDOW: u32 = 0x0800_0000;
    let mut command = tokio::process::Command::new("powershell.exe");
    command
        .args(["-NoProfile", "-NonInteractive", "-ExecutionPolicy", "Bypass", "-EncodedCommand"])
        .arg(encoded)
        .env("AP_ACTION", action)
        .env("AP_WINUSER", name)
        .creation_flags(CREATE_NO_WINDOW)
        .kill_on_drop(true);
    for (key, value) in env {
        command.env(key, value);
    }
    let out = tokio::time::timeout(Duration::from_secs(60), command.output())
        .await
        .context("reading the Windows accounts took too long")?
        .context("starting PowerShell")?;
    let stdout = String::from_utf8_lossy(&out.stdout);
    let last = stdout.lines().rev().find(|l| !l.trim().is_empty()).unwrap_or("");
    let result: serde_json::Value = serde_json::from_str(last.trim()).with_context(|| {
        format!(
            "unexpected answer from the account script: {}{}",
            stdout.trim(),
            String::from_utf8_lossy(&out.stderr).trim()
        )
    })?;
    if result.get("ok").and_then(|v| v.as_bool()) == Some(true) {
        Ok(result)
    } else {
        bail!(result.get("error").and_then(|e| e.as_str()).unwrap_or("the account script gave no answer").to_string())
    }
}

#[cfg(not(windows))]
async fn run_accounts(_action: &str, _name: &str, _env: &[(&str, String)]) -> Result<serde_json::Value> {
    bail!("Windows accounts can only be changed when the portal runs on Windows.")
}

/// Lists, changes and deletes local accounts. Prints one JSON line.
#[cfg(windows)]
const ACCOUNTS_SCRIPT: &str = r#"
$ErrorActionPreference = 'Continue'
function Finish($result) { ConvertTo-Json -InputObject $result -Compress -Depth 4; exit 0 }
function GroupSids($group) {
  @(([ADSI]"WinNT://./$group,group").psbase.Invoke('Members') | ForEach-Object {
    (New-Object Security.Principal.SecurityIdentifier ([byte[]]$_.GetType().InvokeMember('objectSid', 'GetProperty', $null, $_, $null)), 0).Value
  })
}
# Administrator, Guest, DefaultAccount, WDAGUtilityAccount.
function Builtin($sid) { $sid -match '-(500|501|503|504)$' }

try {
  $me = [Security.Principal.WindowsIdentity]::GetCurrent()
  $mySid = $me.User.Value
  if (-not ([Security.Principal.WindowsPrincipal]$me).IsInRole([Security.Principal.WindowsBuiltInRole]::Administrator)) {
    throw 'The portal is not running as an administrator, so it cannot read or change Windows accounts.'
  }
  $adminGroup = (New-Object Security.Principal.SecurityIdentifier 'S-1-5-32-544').Translate([Security.Principal.NTAccount]).Value.Split('\')[-1]
  $admins = GroupSids $adminGroup
  $portalGroup = 'Portal RDP Users'
  $portal = if (Get-LocalGroup -Name $portalGroup -ErrorAction SilentlyContinue) { GroupSids $portalGroup } else { @() }
  $action = $env:AP_ACTION

  if ($action -eq 'list') {
    $list = @(Get-LocalUser | Where-Object { -not (Builtin $_.SID.Value) } | ForEach-Object {
      $sid = $_.SID.Value
      [ordered]@{
        name = $_.Name; full_name = [string]$_.FullName; description = [string]$_.Description
        enabled = [bool]$_.Enabled; is_admin = $admins -contains $sid; portal = $portal -contains $sid
        me = $sid -eq $mySid; microsoft = ([string]$_.PrincipalSource) -eq 'MicrosoftAccount'
        last_logon = $(if ($_.LastLogon) { [int64]([DateTimeOffset]$_.LastLogon).ToUnixTimeSeconds() } else { $null })
      }
    })
    Finish @{ ok = $true; accounts = $list }
  }

  $user = Get-LocalUser -Name $env:AP_WINUSER -ErrorAction SilentlyContinue
  if (-not $user) { throw "There is no Windows account named $($env:AP_WINUSER) on this computer." }
  $sid = $user.SID.Value
  $name = $user.Name
  if (Builtin $sid) { throw "$name is one of Windows' built-in accounts; the portal leaves those alone." }

  if ($action -eq 'check') {
    Add-Type -AssemblyName System.DirectoryServices.AccountManagement
    $ctx = New-Object System.DirectoryServices.AccountManagement.PrincipalContext 'Machine'
    try { $valid = $ctx.ValidateCredentials($name, $env:AP_PASSWORD) } finally { $ctx.Dispose() }
    Finish @{ ok = $true; valid = [bool]$valid }
  }

  if ($action -eq 'update') {
    $set = @($env:AP_SET -split ',' | Where-Object { $_ })
    if ($set -contains 'enabled' -and $env:AP_ENABLED -ne '1' -and $sid -eq $mySid) {
      throw "$name is the account Access Portal runs as; it can't be turned off."
    }
    if ($set -contains 'full_name') { Set-LocalUser -SID $sid -FullName $env:AP_FULLNAME -ErrorAction Stop }
    if ($set -contains 'description') {
      if ($env:AP_DESCRIPTION.Length -gt 48) { throw 'Windows allows at most 48 characters in a description.' }
      Set-LocalUser -SID $sid -Description $env:AP_DESCRIPTION -ErrorAction Stop
    }
    if ($set -contains 'password') {
      if ($user.PrincipalSource -eq 'MicrosoftAccount') { throw "$name signs in with a Microsoft account; change its password at account.microsoft.com." }
      Set-LocalUser -SID $sid -Password (ConvertTo-SecureString $env:AP_PASSWORD -AsPlainText -Force) -ErrorAction Stop
    }
    if ($set -contains 'enabled') {
      if ($env:AP_ENABLED -eq '1') { Enable-LocalUser -SID $sid -ErrorAction Stop } else { Disable-LocalUser -SID $sid -ErrorAction Stop }
    }
    Finish @{ ok = $true }
  }

  if ($action -eq 'can-delete' -or $action -eq 'delete') {
    if ($sid -eq $mySid) { throw "$name is the account Access Portal runs as; it can't be deleted." }
    if ($admins -contains $sid) { throw "$name is a Windows administrator account; the portal never deletes those." }
    if ($action -eq 'delete') {
      Remove-LocalUser -SID $sid -ErrorAction Stop
      if ($env:AP_FILES -eq '1') {
        Get-CimInstance Win32_UserProfile | Where-Object SID -eq $sid | Remove-CimInstance -ErrorAction SilentlyContinue
      }
    }
    Finish @{ ok = $true }
  }
  throw "Unknown action $action."
}
catch {
  Finish @{ ok = $false; error = $_.Exception.Message }
}
"#;

/// Runs elevated inside the portal's process context. Prints one JSON line.
#[cfg(windows)]
const SCRIPT: &str = r#"
$ErrorActionPreference = 'Continue'
function Finish($result) { $result | ConvertTo-Json -Compress; exit 0 }

$name = $env:AP_WINUSER
$action = $env:AP_ACTION
# A restricted account can run one program on a black screen instead of the
# desktop: its shell is a small launcher (below) that starts the program the
# portal chose for it and signs the session out when the program closes. Windows
# Home delivers but refuses to launch an RDP "alternate shell", so the launch is
# done here, through the account's own shell, which Home does run.
$oldShell = '*\access-portal\*-shell.vbs*'
$marker   = 'Software\AccessPortal'
# Not in the portal's data folder: that one is admins-only (it holds the
# database). This folder is readable by every user and writable only by admins.
$launcher = Join-Path $env:ProgramData 'AccessPortal-Shell\app-shell.vbs'
# The launcher reads the program and its folder from the account's own settings
# (written below) so one launcher file serves every account.
$launcherVbs = @'
Option Explicit
Dim sh, prog, args, work
Set sh = CreateObject("WScript.Shell")
prog = ""
args = ""
work = ""
On Error Resume Next
prog = sh.RegRead("HKCU\Software\AccessPortal\Program")
args = sh.RegRead("HKCU\Software\AccessPortal\Args")
work = sh.RegRead("HKCU\Software\AccessPortal\WorkDir")
On Error GoTo 0
If prog = "" Then
  sh.Run "explorer.exe"
Else
  If work <> "" Then sh.CurrentDirectory = work
  If args <> "" Then
    sh.Run """" & prog & """ " & args, 1, True
  Else
    sh.Run """" & prog & """", 1, True
  End If
  sh.Run "shutdown.exe /l", 0, False
End If
'@

$winlogon = 'Software\Microsoft\Windows NT\CurrentVersion\Winlogon'
$explorer = 'Software\Microsoft\Windows\CurrentVersion\Policies\Explorer'
$system   = 'Software\Microsoft\Windows\CurrentVersion\Policies\System'
$cmdPol   = 'Software\Policies\Microsoft\Windows\System'
$blocked  = 'cmd.exe','powershell.exe','pwsh.exe','powershell_ise.exe','regedit.exe','mmc.exe','taskmgr.exe','wt.exe'

function GroupSids($group) {
  @(([ADSI]"WinNT://./$group,group").psbase.Invoke('Members') | ForEach-Object {
    (New-Object Security.Principal.SecurityIdentifier ([byte[]]$_.GetType().InvokeMember('objectSid', 'GetProperty', $null, $_, $null)), 0).Value
  })
}
$portalGroup = 'Portal RDP Users'

try {
  $me = [Security.Principal.WindowsPrincipal][Security.Principal.WindowsIdentity]::GetCurrent()
  $elevated = $me.IsInRole([Security.Principal.WindowsBuiltInRole]::Administrator)
  $user = Get-LocalUser -Name $name -ErrorAction SilentlyContinue

  if ($action -eq 'create') {
    if (-not $elevated) { throw 'The portal is not running as an administrator, so it cannot create Windows accounts.' }
    if ($user) { throw "A Windows account named $name already exists on this computer." }
    if (-not (Get-LocalGroup -Name $portalGroup -ErrorAction SilentlyContinue)) {
      throw "This computer has no '$portalGroup' group yet, so a new account could not sign in over Remote Desktop."
    }
    $secure = ConvertTo-SecureString $env:AP_PASSWORD -AsPlainText -Force
    $user = New-LocalUser -Name $name -Password $secure -AccountNeverExpires -PasswordNeverExpires -UserMayNotChangePassword -Description 'access-portal RDP user' -ErrorAction Stop
    Add-LocalGroupMember -Group $portalGroup -Member $name -ErrorAction Stop
    $action = 'app'   # new accounts start restricted
  }

  if (-not $user) { Finish @{ ok = $true; mode = 'missing' } }
  $sid = $user.SID.Value
  if (-not $elevated) {
    throw 'The portal is not running as an administrator, so it cannot read or change Windows accounts.'
  }
  $adminGroup = (New-Object Security.Principal.SecurityIdentifier 'S-1-5-32-544').Translate([Security.Principal.NTAccount]).Value.Split('\')[-1]
  $isAdmin = (GroupSids $adminGroup) -contains $sid

  if ($action -eq 'delete') {
    if ($isAdmin) { throw "$name is a Windows administrator account; the portal never deletes those." }
    if (-not ((GroupSids $portalGroup) -contains $sid)) { throw "$name isn't in the '$portalGroup' group, so the portal doesn't manage it and won't delete it." }
    Remove-LocalUser -SID $sid -ErrorAction Stop
    # The profile folder too; skipped while the account is still signed in.
    Get-CimInstance Win32_UserProfile | Where-Object SID -eq $sid | Remove-CimInstance -ErrorAction SilentlyContinue
    Finish @{ ok = $true; mode = 'missing' }
  }

  if ($action -eq 'app') {
    # Never lock down an administrator account.
    if ($isAdmin) { throw "$name is a Windows administrator account; it can't be restricted." }
  }

  $profileKey = "HKLM:\SOFTWARE\Microsoft\Windows NT\CurrentVersion\ProfileList\$sid"
  if (-not (Test-Path $profileKey)) {
    # Never signed in, so nothing has been set: Windows would give a full desktop.
    if ($action -ne 'app') { Finish @{ ok = $true; mode = 'full' } }
    Add-Type -Namespace AccessPortal -Name Profile -MemberDefinition '[DllImport("userenv.dll", CharSet = CharSet.Unicode)] public static extern int CreateProfile(string sid, string name, System.Text.StringBuilder path, uint size);'
    $path = New-Object System.Text.StringBuilder 260
    $hr = [AccessPortal.Profile]::CreateProfile($sid, $name, $path, 260)
    if ($hr -ne 0) { throw ('Could not create the Windows profile for {0} (0x{1:X8}).' -f $name, $hr) }
  }

  # The account's settings: already open if it is signed in, else load its file.
  $hive = $sid
  $loaded = $false
  if (-not (Test-Path "Registry::HKEY_USERS\$sid")) {
    $dir = (Get-ItemProperty $profileKey).ProfileImagePath
    $hive = "ap_$sid"
    & reg.exe load "HKU\$hive" "$dir\NTUSER.DAT" *> $null
    if ($LASTEXITCODE -ne 0) { throw "Could not open the settings of $name." }
    $loaded = $true
  }

  $hku = [Microsoft.Win32.Registry]::Users
  try {
    if ($action -eq 'app' -or $action -eq 'full') {
      $k = $hku.OpenSubKey("$hive\$winlogon", $true)
      if ($k) {
        $cur = [string]$k.GetValue('Shell')
        if ($cur -like $oldShell -or $cur -like '*\app-shell.vbs*') { $k.DeleteValue('Shell', $false) }
        $k.Dispose()
      }
    }
    if ($action -eq 'app') {
      $k = $hku.CreateSubKey("$hive\$marker"); $k.SetValue('Restricted', 1, 'DWord'); $k.Dispose()
      $k = $hku.CreateSubKey("$hive\$explorer")
      foreach ($v in 'NoRun','NoControlPanel','NoWinKeys','NoViewContextMenu','DisallowRun') { $k.SetValue($v, 1, 'DWord') }
      $k.SetValue('NoDrives', 4, 'DWord'); $k.SetValue('NoViewOnDrive', 4, 'DWord'); $k.Dispose()
      $k = $hku.CreateSubKey("$hive\$explorer\DisallowRun")
      $i = 1; foreach ($exe in $blocked) { $k.SetValue([string]$i, $exe, 'String'); $i++ }; $k.Dispose()
      $k = $hku.CreateSubKey("$hive\$system"); $k.SetValue('DisableTaskMgr', 1, 'DWord'); $k.SetValue('DisableRegistryTools', 1, 'DWord'); $k.Dispose()
      $k = $hku.CreateSubKey("$hive\$cmdPol"); $k.SetValue('DisableCMD', 2, 'DWord'); $k.Dispose()
      # With a program, the account's shell becomes the launcher (black screen,
      # just that program). Without one, no custom shell: a plain locked desktop.
      $wl = $hku.CreateSubKey("$hive\$winlogon")
      if ($env:AP_PROGRAM) {
        # A shortcut (such as one Miracle makes for a company folder) is opened
        # here, as administrator, so the account needs no access to wherever the
        # shortcut file lives; its target, arguments and folder are what it runs.
        $prog = $env:AP_PROGRAM.Trim().Trim('"')
        $progArgs = ''
        $progDir = ''
        if ($prog -like '*.lnk') {
          if (-not (Test-Path -LiteralPath $prog)) { throw "The shortcut '$prog' does not exist." }
          $lnk = (New-Object -ComObject WScript.Shell).CreateShortcut($prog)
          $prog = $lnk.TargetPath
          $progArgs = [string]$lnk.Arguments
          $progDir = [string]$lnk.WorkingDirectory
        }
        if (-not (Test-Path -LiteralPath $prog)) { throw "The program '$prog' does not exist." }
        if (-not $progDir) { $progDir = Split-Path $prog }
        $launcherDir = Split-Path $launcher
        New-Item -ItemType Directory -Path $launcherDir -Force | Out-Null
        # Users may read and run the launcher but never change it.
        & icacls $launcherDir /inheritance:r /grant:r '*S-1-5-32-544:(OI)(CI)F' '*S-1-5-18:(OI)(CI)F' '*S-1-5-32-545:(OI)(CI)RX' /Q | Out-Null
        Set-Content -LiteralPath $launcher -Value $launcherVbs -Encoding ASCII
        $m = $hku.CreateSubKey("$hive\$marker")
        $m.SetValue('Program', $prog, 'String')
        $m.SetValue('Args', $progArgs, 'String')
        $m.SetValue('WorkDir', $progDir, 'String')
        $m.Dispose()
        $wl.SetValue('Shell', 'wscript.exe "' + $launcher + '"', 'String')
      } else {
        $m = $hku.OpenSubKey("$hive\$marker", $true)
        if ($m) { foreach ($v in 'Program','Args','WorkDir') { $m.DeleteValue($v, $false) }; $m.Dispose() }
      }
      $wl.Dispose()
    }
    elseif ($action -eq 'full') {
      # Remove exactly what 'app' sets; other settings of the account stay.
      $remove = @{
        $marker   = @('Restricted','Program','Args','WorkDir')
        $explorer = @('NoRun','NoControlPanel','NoWinKeys','NoViewContextMenu','DisallowRun','NoDrives','NoViewOnDrive')
        $system   = @('DisableTaskMgr','DisableRegistryTools')
        $cmdPol   = @('DisableCMD')
      }
      foreach ($path in $remove.Keys) {
        $k = $hku.OpenSubKey("$hive\$path", $true)
        if ($k) { foreach ($v in $remove[$path]) { $k.DeleteValue($v, $false) }; $k.Dispose() }
      }
      $k = $hku.OpenSubKey("$hive\$explorer", $true)
      if ($k) { $k.DeleteSubKeyTree('DisallowRun', $false); $k.Dispose() }
    }

    $k = $hku.OpenSubKey("$hive\$marker")
    $restricted = $k -and ($k.GetValue('Restricted') -eq 1)
    if ($k) { $k.Dispose() }
    $k = $hku.OpenSubKey("$hive\$winlogon")
    $shell = if ($k) { [string]$k.GetValue('Shell') } else { '' }
    if ($k) { $k.Dispose() }
    # Accounts restricted by older versions have the old shell but no marker.
    $mode = if ($restricted -or $shell -like $oldShell) { 'app' } else { 'full' }
  }
  finally {
    if ($loaded) {
      [GC]::Collect(); [GC]::WaitForPendingFinalizers()
      for ($try = 0; $try -lt 5; $try++) {
        & reg.exe unload "HKU\$hive" *> $null
        if ($LASTEXITCODE -eq 0) { break }
        Start-Sleep -Milliseconds 400
      }
    }
  }
  Finish @{ ok = $true; mode = $mode }
}
catch {
  Finish @{ ok = $false; error = $_.Exception.Message }
}
"#;

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
        use std::os::windows::process::CommandExt;
        let settings = [
            (r"HKLM\SOFTWARE\Policies\Microsoft\Windows NT\Terminal Services", "AVCHardwareEncodePreferred", 1u32),
            (r"HKLM\SOFTWARE\Policies\Microsoft\Windows NT\Terminal Services", "bEnumerateHWBeforeSW", 1),
            (r"HKLM\SYSTEM\CurrentControlSet\Control\Terminal Server\WinStations", "DWMFRAMEINTERVAL", 15),
            // New accounts would otherwise start with Windows' privacy-settings screen.
            (r"HKLM\SOFTWARE\Policies\Microsoft\Windows\OOBE", "DisablePrivacyExperience", 1),
            // Let sessions use the user's microphone and camera, and play sound.
            (r"HKLM\SOFTWARE\Policies\Microsoft\Windows NT\Terminal Services", "fDisableAudioCapture", 0),
            (r"HKLM\SOFTWARE\Policies\Microsoft\Windows NT\Terminal Services", "fDisableCam", 0),
            (r"HKLM\SOFTWARE\Policies\Microsoft\Windows NT\Terminal Services", "fDisableCameraRedir", 0),
            // Show the last signed-in account on the sign-in screen instead of
            // a blank "Other user" box asking for a name. Only changes what the
            // screen shows: no automatic sign-in, no account or RDP changes.
            (r"HKLM\SOFTWARE\Microsoft\Windows\CurrentVersion\Policies\System", "dontdisplaylastusername", 0),
        ];
        for (key, name, value) in settings {
            let current = std::process::Command::new("reg")
                .args(["query", key, "/v", name])
                .creation_flags(0x0800_0000)
                .output()
                .ok()
                .map(|o| String::from_utf8_lossy(&o.stdout).to_string())
                .unwrap_or_default();
            if current.contains(&format!("0x{value:x}")) {
                continue;
            }
            let ok = std::process::Command::new("reg")
                .args(["add", key, "/v", name, "/t", "REG_DWORD", "/d", &value.to_string(), "/f"])
                .creation_flags(0x0800_0000)
                .status()
                .is_ok_and(|s| s.success());
            if ok {
                tracing::info!("remote desktop tuned: {name} = {value}");
            } else {
                tracing::warn!("could not set {name} (the portal needs administrator rights for this)");
            }
        }
    }
}

/// Sign a Windows account out of every session it has on this machine: its
/// apps close (unsaved work is lost). Returns how many sessions were signed out.
pub fn sign_out_blocking(win_username: &str) -> usize {
    #[cfg(windows)]
    {
        use base64::Engine;
        use std::os::windows::process::CommandExt;
        const SCRIPT: &str = r#"
Add-Type -TypeDefinition @'
using System; using System.Runtime.InteropServices; using System.Collections.Generic;
public static class ApWts {
  [StructLayout(LayoutKind.Sequential)] struct INFO { public int SessionId; [MarshalAs(UnmanagedType.LPWStr)] public string Name; public int State; }
  [DllImport("wtsapi32.dll")] static extern bool WTSEnumerateSessionsW(IntPtr h, int r, int v, out IntPtr p, out int c);
  [DllImport("wtsapi32.dll", CharSet=CharSet.Unicode)] static extern bool WTSQuerySessionInformationW(IntPtr h, int id, int cls, out IntPtr buf, out int len);
  [DllImport("wtsapi32.dll")] static extern void WTSFreeMemory(IntPtr p);
  [DllImport("wtsapi32.dll")] public static extern bool WTSLogoffSession(IntPtr h, int id, bool wait);
  public static List<int> SessionsOf(string user) {
    var r = new List<int>(); IntPtr p; int c;
    if (!WTSEnumerateSessionsW(IntPtr.Zero, 0, 1, out p, out c)) return r;
    int sz = Marshal.SizeOf(typeof(INFO));
    for (int i = 0; i < c; i++) {
      var s = (INFO)Marshal.PtrToStructure(new IntPtr(p.ToInt64() + i * sz), typeof(INFO));
      IntPtr b; int l;
      if (WTSQuerySessionInformationW(IntPtr.Zero, s.SessionId, 5, out b, out l)) {
        if (string.Equals(Marshal.PtrToStringUni(b), user, StringComparison.OrdinalIgnoreCase)) r.Add(s.SessionId);
        WTSFreeMemory(b);
      }
    }
    WTSFreeMemory(p); return r;
  }
}
'@
$n = 0
foreach ($id in [ApWts]::SessionsOf($env:AP_WINUSER)) { if ([ApWts]::WTSLogoffSession([IntPtr]::Zero, $id, $true)) { $n++ } }
$n
"#;
        let utf16: Vec<u8> = SCRIPT.encode_utf16().flat_map(|u| u.to_le_bytes()).collect();
        let encoded = base64::engine::general_purpose::STANDARD.encode(utf16);
        let out = std::process::Command::new("powershell.exe")
            .args(["-NoProfile", "-NonInteractive", "-ExecutionPolicy", "Bypass", "-EncodedCommand", &encoded])
            .env("AP_WINUSER", win_username)
            .creation_flags(0x0800_0000)
            .output();
        let n = out
            .ok()
            .and_then(|o| String::from_utf8_lossy(&o.stdout).trim().lines().last().and_then(|l| l.trim().parse().ok()))
            .unwrap_or(0);
        tracing::info!("signed {win_username} out of Windows ({n} session(s))");
        n
    }
    #[cfg(not(windows))]
    {
        let _ = win_username;
        0
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
    let out = tokio::task::spawn_blocking(move || {
        powershell_blocking(FOLDER_ACL_SCRIPT, &[("AP_WINUSER", &u), ("AP_DIR", &d), ("AP_ACTION", &a)])
    })
    .await??;
    #[derive(serde::Deserialize)]
    struct R {
        ok: bool,
        #[serde(default)]
        error: Option<String>,
    }
    let line = out.trim().lines().last().unwrap_or("").trim();
    let r: R = serde_json::from_str(line)
        .unwrap_or(R { ok: false, error: Some(format!("unexpected output: {out}")) });
    if r.ok {
        tracing::info!("folder access {action} for {win_username}: {dir}");
        Ok(())
    } else {
        anyhow::bail!(r.error.unwrap_or_else(|| "changing folder permissions failed".into()))
    }
}

#[cfg(not(windows))]
async fn folder_acl(win_username: &str, dir: &str, _action: &str) -> anyhow::Result<()> {
    let _ = (win_username, dir);
    Ok(())
}

/// Grants or removes one account's exclusive access to one data folder with
/// `icacls`. Refuses drive roots and the Windows / program / profile regions so
/// a mistyped folder can never lock the operating system.
#[cfg(windows)]
const FOLDER_ACL_SCRIPT: &str = r#"
$ErrorActionPreference = 'Stop'
function Finish($r) { $r | ConvertTo-Json -Compress; exit 0 }
$user = $env:AP_WINUSER
$dir = $env:AP_DIR
$action = $env:AP_ACTION
try {
  if ([string]::IsNullOrWhiteSpace($dir)) { throw 'No folder was given.' }
  if (-not (Test-Path -LiteralPath $dir -PathType Container)) { throw "The folder '$dir' does not exist on this computer." }
  $full = (Resolve-Path -LiteralPath $dir).Path.TrimEnd('\')
  $lower = $full.ToLower()
  if ($full.Length -le 3) { throw "Refusing to change permissions on a drive root ($full)." }
  $prot = @($env:SystemRoot, $env:ProgramFiles, ${env:ProgramFiles(x86)}, $env:ProgramData, (Join-Path $env:SystemDrive 'Users')) |
    Where-Object { $_ } | ForEach-Object { $_.TrimEnd('\').ToLower() }
  foreach ($p in $prot) {
    if ($lower -eq $p -or $p.StartsWith($lower + '\')) {
      throw "Refusing to change permissions on a system folder ($full)."
    }
  }
  $admins = '*S-1-5-32-544'   # BUILTIN\Administrators
  $system = '*S-1-5-18'       # NT AUTHORITY\SYSTEM
  if ($action -eq 'grant') {
    # Inheritable rights on the folder itself, then stop it inheriting from its
    # parent (the computer's default "Users" read). Everything inside inherits
    # from this folder, so it all follows. Only the top folder is changed: doing
    # this to every file (/T) leaves files with no permissions at all.
    & icacls $full /grant "${admins}:(OI)(CI)F" "${system}:(OI)(CI)F" "${user}:(OI)(CI)M" /C /Q | Out-Null
    if ($LASTEXITCODE -ne 0) { throw "Could not grant $user access to $full (icacls $LASTEXITCODE)." }
    & icacls $full /inheritance:r /C /Q | Out-Null
    if ($LASTEXITCODE -ne 0) { throw "Could not lock $full to its users (icacls $LASTEXITCODE)." }
    # Repair anything an earlier version left without permissions.
    Get-ChildItem -LiteralPath $full -Recurse -Force -ErrorAction SilentlyContinue | ForEach-Object {
      $acl = Get-Acl -LiteralPath $_.FullName -ErrorAction SilentlyContinue
      if ($acl -and $acl.Access.Count -eq 0) {
        & icacls $_.FullName /reset /C /Q | Out-Null
        if ($LASTEXITCODE -ne 0) {
          & takeown /F $_.FullName /A | Out-Null
          & icacls $_.FullName /reset /C /Q | Out-Null
        }
      }
    }
    Finish @{ ok = $true }
  }
  elseif ($action -eq 'revoke') {
    & icacls $full /remove:g "$user" /C /Q | Out-Null
    Finish @{ ok = $true }
  }
  else { throw "Unknown action $action." }
}
catch {
  Finish @{ ok = $false; error = $_.Exception.Message }
}
"#;

/// Run a PowerShell script (hidden window) with some environment variables and
/// return what it printed.
#[cfg(windows)]
fn powershell_blocking(script: &str, env: &[(&str, &str)]) -> anyhow::Result<String> {
    use base64::Engine;
    use std::os::windows::process::CommandExt;
    let utf16: Vec<u8> = script.encode_utf16().flat_map(|u| u.to_le_bytes()).collect();
    let encoded = base64::engine::general_purpose::STANDARD.encode(utf16);
    let mut command = std::process::Command::new("powershell.exe");
    command
        .args(["-NoProfile", "-NonInteractive", "-ExecutionPolicy", "Bypass", "-EncodedCommand", &encoded])
        .creation_flags(0x0800_0000);
    for (k, v) in env {
        command.env(k, v);
    }
    let out = command.output().context("starting PowerShell")?;
    Ok(String::from_utf8_lossy(&out.stdout).into_owned())
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

#[cfg(windows)]
const SESSIONS_SCRIPT: &str = r#"
Add-Type -TypeDefinition @'
using System; using System.Runtime.InteropServices; using System.Collections.Generic;
public static class ApSessions {
  [StructLayout(LayoutKind.Sequential)] struct SESSION { public int SessionId; [MarshalAs(UnmanagedType.LPWStr)] public string Name; public int State; }
  [StructLayout(LayoutKind.Sequential, CharSet=CharSet.Unicode)] struct INFO {
    public int State; public int SessionId; public int InB; public int OutB; public int InF; public int OutF; public int InC; public int OutC;
    [MarshalAs(UnmanagedType.ByValTStr, SizeConst=32)] public string Station;
    [MarshalAs(UnmanagedType.ByValTStr, SizeConst=17)] public string Domain;
    [MarshalAs(UnmanagedType.ByValTStr, SizeConst=21)] public string User;
    public long ConnectTime; public long DisconnectTime; public long LastInputTime; public long LogonTime; public long CurrentTime;
  }
  [DllImport("wtsapi32.dll")] static extern bool WTSEnumerateSessionsW(IntPtr h, int r, int v, out IntPtr p, out int c);
  [DllImport("wtsapi32.dll")] static extern bool WTSQuerySessionInformationW(IntPtr h, int id, int cls, out IntPtr buf, out int len);
  [DllImport("wtsapi32.dll")] static extern void WTSFreeMemory(IntPtr p);
  [DllImport("wtsapi32.dll")] public static extern bool WTSLogoffSession(IntPtr h, int id, bool wait);
  public static List<object[]> List() {
    var r = new List<object[]>(); IntPtr p; int c;
    if (!WTSEnumerateSessionsW(IntPtr.Zero, 0, 1, out p, out c)) return r;
    int sz = Marshal.SizeOf(typeof(SESSION));
    for (int i = 0; i < c; i++) {
      var s = (SESSION)Marshal.PtrToStructure(new IntPtr(p.ToInt64() + i * sz), typeof(SESSION));
      IntPtr b; int l;
      if (!WTSQuerySessionInformationW(IntPtr.Zero, s.SessionId, 24, out b, out l)) continue;
      var info = (INFO)Marshal.PtrToStructure(b, typeof(INFO));
      WTSFreeMemory(b);
      if (string.IsNullOrEmpty(info.User)) continue;
      long disc = (s.State == 4 && info.DisconnectTime > 0) ? Math.Max(0, (info.CurrentTime - info.DisconnectTime) / 10000000) : 0;
      r.Add(new object[] { s.SessionId, info.User, s.State, disc });
    }
    WTSFreeMemory(p); return r;
  }
}
'@
if ($env:AP_LOGOFF) {
  $n = 0
  foreach ($id in $env:AP_LOGOFF.Split(',')) { if ([ApSessions]::WTSLogoffSession([IntPtr]::Zero, [int]$id, $true)) { $n++ } }
  $n; exit 0
}
$adminGroup = (New-Object Security.Principal.SecurityIdentifier 'S-1-5-32-544').Translate([Security.Principal.NTAccount]).Value.Split('\')[-1]
$admins = @(([ADSI]"WinNT://./$adminGroup,group").psbase.Invoke('Members') | ForEach-Object { $_.GetType().InvokeMember('Name', 'GetProperty', $null, $_, $null) })
$list = @([ApSessions]::List() | ForEach-Object {
  [pscustomobject]@{ id = $_[0]; user = $_[1]; state = $_[2]; disconnected_secs = $_[3]; admin = ($admins -contains $_[1]) }
})
ConvertTo-Json -InputObject $list -Compress
"#;

/// Every signed-in Windows session on this computer.
pub fn windows_sessions_blocking() -> Vec<WinSession> {
    #[cfg(windows)]
    {
        let out = match powershell_blocking(SESSIONS_SCRIPT, &[]) {
            Ok(out) => out,
            Err(e) => {
                tracing::warn!("listing Windows sessions: {e:#}");
                return Vec::new();
            }
        };
        let last = out.lines().rev().find(|l| !l.trim().is_empty()).unwrap_or("[]");
        serde_json::from_str(last.trim()).unwrap_or_else(|e| {
            tracing::warn!("listing Windows sessions: unexpected answer ({e}): {}", out.trim());
            Vec::new()
        })
    }
    #[cfg(not(windows))]
    Vec::new()
}

/// Sign these Windows sessions out (their apps close).
pub fn log_off_blocking(ids: &[u32]) -> usize {
    #[cfg(windows)]
    {
        if ids.is_empty() {
            return 0;
        }
        let list = ids.iter().map(u32::to_string).collect::<Vec<_>>().join(",");
        powershell_blocking(SESSIONS_SCRIPT, &[("AP_LOGOFF", &list)])
            .ok()
            .and_then(|o| o.trim().lines().last().and_then(|l| l.trim().parse().ok()))
            .unwrap_or(0)
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
        use std::os::windows::process::CommandExt;
        let value = if ask { "1" } else { "2" };
        let ok = std::process::Command::new("reg")
            .args(["add", r"HKLM\SOFTWARE\Policies\Microsoft\Windows NT\Terminal Services", "/v", "Shadow", "/t", "REG_DWORD", "/d", value, "/f"])
            .creation_flags(0x0800_0000)
            .status()
            .is_ok_and(|s| s.success());
        if !ok {
            tracing::warn!("could not set the screen-viewing policy (needs administrator rights)");
        }
    }
    #[cfg(not(windows))]
    let _ = ask;
}

/// What this computer needs to serve portal sessions, checked (and with `fix`,
/// repaired) by `host_setup.ps1`: one JSON object per check.
pub fn setup_blocking(fix: &[String]) -> anyhow::Result<Vec<serde_json::Value>> {
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        // Too long for -EncodedCommand (the command line limit), so run it as a file.
        let path = std::env::temp_dir().join(format!("access-portal-setup-{}.ps1", std::process::id()));
        std::fs::write(&path, include_str!("host_setup.ps1")).context("writing the setup script")?;
        let ini_urls = std::env::var("ACCESS_PORTAL_SERVICE_URL")
            .map(|base| format!("{}/v1/rdpwrap.ini", base.trim_end_matches('/')))
            .unwrap_or_default();
        let out = std::process::Command::new("powershell.exe")
            .args(["-NoProfile", "-NonInteractive", "-ExecutionPolicy", "Bypass", "-File"])
            .arg(&path)
            .env("AP_ACTION", if fix.is_empty() { "check" } else { "fix" })
            .env("AP_FIX", fix.join(","))
            .env("AP_INI_URLS", ini_urls)
            .creation_flags(0x0800_0000)
            .output();
        let _ = std::fs::remove_file(&path);
        let out = out.context("starting PowerShell")?;
        let stdout = String::from_utf8_lossy(&out.stdout);
        let last = stdout.lines().rev().find(|l| l.trim_start().starts_with('[')).unwrap_or("");
        serde_json::from_str(last.trim()).with_context(|| {
            format!("unexpected answer from the setup script: {}{}", stdout.trim(), String::from_utf8_lossy(&out.stderr).trim())
        })
    }
    #[cfg(not(windows))]
    {
        let _ = fix;
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
