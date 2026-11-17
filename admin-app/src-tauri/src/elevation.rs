//! Running as administrator. The portal changes Windows accounts and Remote
//! Desktop settings, so the app needs administrator rights.
//!
//! - The app's shortcuts are marked "Run as administrator" (`fix_shortcuts`).
//! - Once the app has run as administrator, it registers two scheduled tasks
//!   that start it with administrator rights without asking. A copy started
//!   without them (by Windows sign-in, by the updater after an update, by an
//!   old shortcut) starts the elevated copy through a task and steps aside
//!   (`hand_over`), so the app keeps its rights after every update.
//! - When it still runs without them, the window offers "Restart as
//!   administrator" (`restart_as_admin`): through the task if there is one,
//!   otherwise through the usual Windows prompt.
//!
//! A copy started to take over gets `--elevated` and first waits for the copy
//! it replaces to quit (two copies would fight over the portal's port).

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

/// The argument marking a copy started to take over from another one.
pub const ELEVATED_ARG: &str = "--elevated";
/// The scheduled tasks: one opens the window, one starts in the tray.
const TASK: &str = r"Access Portal\Start as administrator";
const TASK_HIDDEN: &str = r"Access Portal\Start as administrator in the background";

/// Called first thing, before the app is built.
pub fn early_start(hidden: bool) {
    #[cfg(windows)]
    if !cfg!(debug_assertions) {
        let args: Vec<String> = std::env::args().collect();
        if args.iter().any(|a| a == ELEVATED_ARG) && is_elevated() {
            // Taking over: let the copy being replaced quit first.
            let start = Instant::now();
            while others_running() && start.elapsed() < Duration::from_secs(20) {
                std::thread::sleep(Duration::from_millis(250));
            }
            return;
        }
        if !is_elevated() && !others_running() && hand_over(hidden) {
            std::process::exit(0);
        }
    }
    #[cfg(not(windows))]
    let _ = hidden;
}

/// Start the elevated copy through its scheduled task and wait (up to 15 s) to
/// see it running. False if there is no task or the copy didn't appear.
fn hand_over(hidden: bool) -> bool {
    if !run_task(if hidden { TASK_HIDDEN } else { TASK }) {
        return false;
    }
    let start = Instant::now();
    while start.elapsed() < Duration::from_secs(15) {
        if others_running() {
            return true;
        }
        std::thread::sleep(Duration::from_millis(250));
    }
    false
}

/// Restart the app with administrator rights. On success the caller quits so
/// the new copy can take over.
pub fn restart_as_admin(hidden: bool) -> Result<(), String> {
    if run_task(if hidden { TASK_HIDDEN } else { TASK }) {
        return Ok(());
    }
    #[cfg(windows)]
    {
        imp::run_as_admin(&std::env::current_exe().map_err(|e| e.to_string())?, ELEVATED_ARG)
    }
    #[cfg(not(windows))]
    Err("Needs Windows.".into())
}

/// While running elevated: (re)register the scheduled tasks, so later copies
/// started without administrator rights can hand over without a prompt.
pub fn register_tasks() {
    // Development builds (`npm run dev`) leave the installed app's tasks alone.
    if !is_elevated() || cfg!(debug_assertions) {
        return;
    }
    let Ok(exe) = std::env::current_exe() else { return };
    let user = match (std::env::var("USERDOMAIN"), std::env::var("USERNAME")) {
        (Ok(d), Ok(u)) => format!("{d}\\{u}"),
        _ => return,
    };
    for (name, args) in [(TASK, ELEVATED_ARG.to_string()), (TASK_HIDDEN, format!("{ELEVATED_ARG} --hidden"))] {
        match register_task(name, &exe, &args, &user) {
            Ok(()) => tracing::debug!("scheduled task {name} registered"),
            Err(e) => tracing::warn!("could not register the scheduled task {name}: {e}"),
        }
    }
}

fn register_task(name: &str, exe: &Path, args: &str, user: &str) -> Result<(), String> {
    let xml = format!(
        r#"<?xml version="1.0" encoding="UTF-16"?>
<Task version="1.2" xmlns="http://schemas.microsoft.com/windows/2004/02/mit/task">
  <RegistrationInfo>
    <Description>Starts Access Portal with administrator rights (it manages Windows accounts and Remote Desktop) without asking each time. Made by Access Portal.</Description>
  </RegistrationInfo>
  <Principals>
    <Principal id="Author">
      <UserId>{user}</UserId>
      <LogonType>InteractiveToken</LogonType>
      <RunLevel>HighestAvailable</RunLevel>
    </Principal>
  </Principals>
  <Settings>
    <MultipleInstancesPolicy>Parallel</MultipleInstancesPolicy>
    <DisallowStartIfOnBatteries>false</DisallowStartIfOnBatteries>
    <StopIfGoingOnBatteries>false</StopIfGoingOnBatteries>
    <AllowHardTerminate>false</AllowHardTerminate>
    <AllowStartOnDemand>true</AllowStartOnDemand>
    <Enabled>true</Enabled>
    <ExecutionTimeLimit>PT0S</ExecutionTimeLimit>
    <Priority>4</Priority>
    <IdleSettings>
      <StopOnIdleEnd>false</StopOnIdleEnd>
      <RestartOnIdle>false</RestartOnIdle>
    </IdleSettings>
  </Settings>
  <Actions Context="Author">
    <Exec>
      <Command>{exe}</Command>
      <Arguments>{args}</Arguments>
    </Exec>
  </Actions>
</Task>
"#,
        user = xml_escape(user),
        exe = xml_escape(&exe.display().to_string()),
        args = xml_escape(args),
    );
    // schtasks wants the file in UTF-16 with a byte order mark.
    let path: PathBuf = std::env::temp_dir().join(format!("access-portal-task-{}.xml", std::process::id()));
    let mut bytes = vec![0xFF, 0xFE];
    bytes.extend(xml.encode_utf16().flat_map(|c| c.to_le_bytes()));
    std::fs::write(&path, bytes).map_err(|e| e.to_string())?;
    let out = schtasks(&["/Create", "/F", "/TN", name, "/XML", &path.display().to_string()]);
    let _ = std::fs::remove_file(&path);
    let out = out.map_err(|e| e.to_string())?;
    if out.status.success() {
        Ok(())
    } else {
        Err(String::from_utf8_lossy(&out.stderr).trim().to_string())
    }
}

fn run_task(name: &str) -> bool {
    match schtasks(&["/Run", "/TN", name]) {
        Ok(out) if out.status.success() => {
            tracing::info!("started the scheduled task {name}");
            true
        }
        _ => false,
    }
}

fn schtasks(args: &[&str]) -> std::io::Result<std::process::Output> {
    let mut command = std::process::Command::new("schtasks.exe");
    command.args(args);
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        command.creation_flags(0x0800_0000); // no console window
    }
    command.output()
}

fn xml_escape(s: &str) -> String {
    s.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;").replace('"', "&quot;")
}

/// Mark this app's shortcuts (desktop and Start menu, this user's and
/// everyone's) "Run as administrator". Everyone's need administrator rights to
/// change; those are skipped otherwise.
pub fn fix_shortcuts() {
    #[cfg(windows)]
    {
        let Ok(exe) = std::env::current_exe() else { return };
        for dir in imp::shortcut_folders() {
            let mut files = Vec::new();
            collect_links(&dir, 1, &mut files);
            for link in files {
                let Ok((target, _, _)) = crate::pickers::imp::read_shortcut(&link.display().to_string()) else { continue };
                if !Path::new(&target).as_os_str().eq_ignore_ascii_case(exe.as_os_str()) {
                    continue;
                }
                match set_run_as_admin(&link) {
                    Ok(true) => tracing::info!("shortcut {} now runs as administrator", link.display()),
                    Ok(false) => {}
                    Err(e) => tracing::debug!("could not change the shortcut {}: {e}", link.display()),
                }
            }
        }
    }
}

/// `.lnk` files in `dir`, and in its subfolders down to `depth` levels.
fn collect_links(dir: &Path, depth: u32, out: &mut Vec<PathBuf>) {
    let Ok(entries) = std::fs::read_dir(dir) else { return };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            if depth > 0 {
                collect_links(&path, depth - 1, out);
            }
        } else if path.extension().is_some_and(|e| e.eq_ignore_ascii_case("lnk")) {
            out.push(path);
        }
    }
}

/// Set the shortcut's "Run as administrator" flag (RunAsUser, bit 13 of the
/// LinkFlags at offset 20 of the shell link header). True if it changed.
fn set_run_as_admin(link: &Path) -> std::io::Result<bool> {
    const RUN_AS_USER: u32 = 0x0000_2000;
    let mut bytes = std::fs::read(link)?;
    if bytes.len() < 0x4C || bytes[..4] != [0x4C, 0, 0, 0] {
        return Err(std::io::Error::other("not a shell link"));
    }
    let flags = u32::from_le_bytes([bytes[20], bytes[21], bytes[22], bytes[23]]);
    if flags & RUN_AS_USER != 0 {
        return Ok(false);
    }
    bytes[20..24].copy_from_slice(&(flags | RUN_AS_USER).to_le_bytes());
    std::fs::write(link, bytes)?;
    Ok(true)
}

/// Does this copy have administrator rights?
pub fn is_elevated() -> bool {
    #[cfg(windows)]
    {
        imp::is_elevated()
    }
    #[cfg(not(windows))]
    true
}

/// Is another copy of this program running?
fn others_running() -> bool {
    #[cfg(windows)]
    {
        imp::others_running()
    }
    #[cfg(not(windows))]
    false
}

#[cfg(windows)]
mod imp {
    use std::path::{Path, PathBuf};

    use windows::core::{HSTRING, PCWSTR};
    use windows::Win32::System::Com::CoTaskMemFree;
    use windows::Win32::UI::Shell::{
        SHGetKnownFolderPath, ShellExecuteExW, FOLDERID_CommonPrograms, FOLDERID_Desktop, FOLDERID_Programs,
        FOLDERID_PublicDesktop, KF_FLAG_DEFAULT, SHELLEXECUTEINFOW,
    };
    use windows_sys::Win32::Foundation::{CloseHandle, INVALID_HANDLE_VALUE};
    use windows_sys::Win32::Security::{GetTokenInformation, TokenElevation, TOKEN_ELEVATION, TOKEN_QUERY};
    use windows_sys::Win32::System::Diagnostics::ToolHelp::{
        CreateToolhelp32Snapshot, Process32FirstW, Process32NextW, PROCESSENTRY32W, TH32CS_SNAPPROCESS,
    };
    use windows_sys::Win32::System::Threading::{GetCurrentProcess, OpenProcessToken};

    pub fn is_elevated() -> bool {
        unsafe {
            let mut token = std::ptr::null_mut();
            if OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut token) == 0 {
                return false;
            }
            let mut elevation = TOKEN_ELEVATION { TokenIsElevated: 0 };
            let mut len = 0u32;
            let ok = GetTokenInformation(
                token,
                TokenElevation,
                &mut elevation as *mut _ as *mut _,
                std::mem::size_of::<TOKEN_ELEVATION>() as u32,
                &mut len,
            );
            CloseHandle(token);
            ok != 0 && elevation.TokenIsElevated != 0
        }
    }

    pub fn others_running() -> bool {
        let Some(name) = std::env::current_exe().ok().and_then(|e| e.file_name().map(|n| n.to_string_lossy().to_lowercase()))
        else {
            return false;
        };
        let me = std::process::id();
        unsafe {
            let snap = CreateToolhelp32Snapshot(TH32CS_SNAPPROCESS, 0);
            if snap == INVALID_HANDLE_VALUE {
                return false;
            }
            let mut entry: PROCESSENTRY32W = std::mem::zeroed();
            entry.dwSize = std::mem::size_of::<PROCESSENTRY32W>() as u32;
            let mut found = false;
            let mut more = Process32FirstW(snap, &mut entry) != 0;
            while more {
                let len = entry.szExeFile.iter().position(|&c| c == 0).unwrap_or(entry.szExeFile.len());
                let exe = String::from_utf16_lossy(&entry.szExeFile[..len]).to_lowercase();
                if exe == name && entry.th32ProcessID != me {
                    found = true;
                    break;
                }
                more = Process32NextW(snap, &mut entry) != 0;
            }
            CloseHandle(snap);
            found
        }
    }

    /// Start `exe` with administrator rights through the Windows prompt.
    pub fn run_as_admin(exe: &Path, args: &str) -> Result<(), String> {
        let verb = HSTRING::from("runas");
        let file = HSTRING::from(exe.as_os_str());
        let params = HSTRING::from(args);
        let mut info = SHELLEXECUTEINFOW {
            cbSize: std::mem::size_of::<SHELLEXECUTEINFOW>() as u32,
            lpVerb: PCWSTR(verb.as_ptr()),
            lpFile: PCWSTR(file.as_ptr()),
            lpParameters: PCWSTR(params.as_ptr()),
            nShow: 1, // SW_SHOWNORMAL
            ..Default::default()
        };
        unsafe { ShellExecuteExW(&mut info) }.map_err(|e| {
            // ERROR_CANCELLED: "No" in the Windows prompt.
            if e.code().0 as u32 == 0x8007_04C7 {
                "Administrator rights weren't given (the Windows prompt was declined).".to_string()
            } else {
                format!("Could not restart as administrator: {e}")
            }
        })
    }

    /// Desktop and Start menu Programs folders: this user's and everyone's.
    pub fn shortcut_folders() -> Vec<PathBuf> {
        [FOLDERID_Desktop, FOLDERID_Programs, FOLDERID_PublicDesktop, FOLDERID_CommonPrograms]
            .iter()
            .filter_map(|id| unsafe {
                let p = SHGetKnownFolderPath(id, KF_FLAG_DEFAULT, None).ok()?;
                let path = p.to_string().ok().map(PathBuf::from);
                CoTaskMemFree(Some(p.0 as *const _));
                path
            })
            .collect()
    }
}

#[cfg(all(test, windows))]
mod tests {
    use super::*;

    /// Registers, runs and deletes a throwaway task. Needs an elevated shell:
    /// `cargo test --lib elevation -- --ignored`.
    #[test]
    #[ignore]
    fn task_round_trip() {
        assert!(is_elevated(), "run from an elevated shell");
        let name = r"Access Portal\test-task-delete-me";
        let user = format!("{}\\{}", std::env::var("USERDOMAIN").unwrap(), std::env::var("USERNAME").unwrap());
        register_task(name, Path::new(r"C:\Windows\System32\cmd.exe"), "/c exit 0", &user).unwrap();
        assert!(run_task(name));
        assert!(schtasks(&["/Delete", "/F", "/TN", name]).unwrap().status.success());
    }

    #[test]
    fn marks_shortcut_run_as_admin() {
        let mut lnk = vec![0u8; 0x4C];
        lnk[0] = 0x4C;
        lnk[20] = 0x9B; // some other flags
        let path = std::env::temp_dir().join(format!("ap-test-{}.lnk", std::process::id()));
        std::fs::write(&path, &lnk).unwrap();
        assert!(set_run_as_admin(&path).unwrap());
        assert!(!set_run_as_admin(&path).unwrap());
        let b = std::fs::read(&path).unwrap();
        assert_eq!(u32::from_le_bytes([b[20], b[21], b[22], b[23]]), 0x2000 | 0x9B);
        std::fs::remove_file(path).unwrap();
    }
}
