//! Changes to Windows accounts on the machine the portal runs on.
//!
//! A portal user's sessions run as a local Windows account. That account is in
//! one of two modes:
//!
//! - **app**: its desktop shell is the Miracle launcher, so a session shows
//!   only Miracle and signs out when it closes; Run, the command prompt,
//!   PowerShell, Task Manager, the registry editor, Control Panel and drive C:
//!   are blocked by per-account policies;
//! - **full**: a normal Windows desktop.
//!
//! The mode lives in the account's own registry hive (HKCU of that account), so
//! it is read and changed there; the portal keeps no copy. A change applies the
//! next time a session starts; a session already open keeps its mode.
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
    run("read", win_username, None).await
}

/// Switch the account to a full desktop (`true`) or to Miracle only (`false`).
/// Returns the mode now in effect.
pub async fn set_desktop_mode(win_username: &str, full: bool) -> Result<String> {
    run(if full { "full" } else { "app" }, win_username, None).await
}

/// Create a local Windows account for a portal user: a standard user in the
/// "Portal RDP Users" group, password never expires, limited to Miracle.
pub async fn create_account(win_username: &str, password: &str) -> Result<()> {
    check_account_name(win_username)?;
    run("create", win_username, Some(password)).await.map(|_| ())
}

/// Delete a Windows account the portal manages (a member of "Portal RDP Users",
/// never an administrator) and its profile folder.
pub async fn delete_account(win_username: &str) -> Result<()> {
    run("delete", win_username, None).await.map(|_| ())
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
async fn run(action: &str, win_username: &str, password: Option<&str>) -> Result<String> {
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
async fn run(_action: &str, _win_username: &str, _password: Option<&str>) -> Result<String> {
    bail!("Windows accounts can only be changed when the portal runs on Windows.")
}

/// Runs elevated inside the portal's process context. Prints one JSON line.
#[cfg(windows)]
const SCRIPT: &str = r#"
$ErrorActionPreference = 'Continue'
function Finish($result) { $result | ConvertTo-Json -Compress; exit 0 }

$name = $env:AP_WINUSER
$action = $env:AP_ACTION
$launcher = 'C:\Program Files\access-portal\miracle-shell.vbs'
$shell = 'wscript.exe "' + $launcher + '"'

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
    $action = 'app'   # new accounts start limited to Miracle
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
    # Never lock an administrator account into one app.
    if ($isAdmin) { throw "$name is a Windows administrator account; it can't be limited to one app." }
    if (-not (Test-Path -LiteralPath $launcher)) { throw "The Miracle launcher is missing ($launcher)." }
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
    if ($action -eq 'app') {
      $k = $hku.CreateSubKey("$hive\$winlogon"); $k.SetValue('Shell', $shell, 'String'); $k.Dispose()
      $k = $hku.CreateSubKey("$hive\$explorer")
      foreach ($v in 'NoRun','NoControlPanel','NoWinKeys','NoViewContextMenu','DisallowRun') { $k.SetValue($v, 1, 'DWord') }
      $k.SetValue('NoDrives', 4, 'DWord'); $k.SetValue('NoViewOnDrive', 4, 'DWord'); $k.Dispose()
      $k = $hku.CreateSubKey("$hive\$explorer\DisallowRun")
      $i = 1; foreach ($exe in $blocked) { $k.SetValue([string]$i, $exe, 'String'); $i++ }; $k.Dispose()
      $k = $hku.CreateSubKey("$hive\$system"); $k.SetValue('DisableTaskMgr', 1, 'DWord'); $k.SetValue('DisableRegistryTools', 1, 'DWord'); $k.Dispose()
      $k = $hku.CreateSubKey("$hive\$cmdPol"); $k.SetValue('DisableCMD', 2, 'DWord'); $k.Dispose()
    }
    elseif ($action -eq 'full') {
      # Remove exactly what 'app' sets; other settings of the account stay.
      $remove = @{
        $winlogon = @('Shell')
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

    $k = $hku.OpenSubKey("$hive\$winlogon")
    $current = if ($k) { [string]$k.GetValue('Shell') } else { '' }
    if ($k) { $k.Dispose() }
    $mode = if ($current -like '*miracle-shell.vbs*') { 'app' } else { 'full' }
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
/// - let sessions run at 60 frames a second (Windows' default is 30).
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
