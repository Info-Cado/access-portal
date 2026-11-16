//! Picking an app's program, and reading a shortcut.
//!
//! The dialog plugin's file picker follows shortcuts: choosing a Miracle
//! shortcut would give back plain `miracle.exe`, without the company folder the
//! shortcut opens. This picker keeps the `.lnk` itself (FOS_NODEREFERENCELINKS).
//! Folders are picked with the dialog plugin.

use serde_json::{json, Value};

/// Ask for a program or shortcut. `None` when the person cancels.
#[tauri::command]
pub async fn pick_program(window: tauri::WebviewWindow) -> Result<Option<String>, String> {
    #[cfg(windows)]
    {
        let owner = window.hwnd().map(|h| h.0 as isize).unwrap_or(0);
        tauri::async_runtime::spawn_blocking(move || imp::pick_program(owner))
            .await
            .map_err(|e| e.to_string())?
    }
    #[cfg(not(windows))]
    {
        let _ = window;
        Err("Picking a program needs Windows.".into())
    }
}

/// What a shortcut runs: `{target, args, work_dir, data_folder}`, where
/// `data_folder` is the last existing folder named in its arguments (Miracle's
/// shortcuts end with the company folder they open), or null.
#[tauri::command]
pub async fn read_shortcut(path: String) -> Result<Value, String> {
    #[cfg(windows)]
    {
        let (target, args, work_dir) = tauri::async_runtime::spawn_blocking(move || imp::read_shortcut(&path))
            .await
            .map_err(|e| e.to_string())??;
        let data_folder = folder_in_args(&args);
        Ok(json!({ "target": target, "args": args, "work_dir": work_dir, "data_folder": data_folder }))
    }
    #[cfg(not(windows))]
    {
        let _ = path;
        Err("Reading a shortcut needs Windows.".into())
    }
}

/// The last argument that is an existing folder, e.g. `C:\Miracle-Companies\Kelvin`
/// in `02 "0" "C:\Miracle-Companies\Kelvin"`.
fn folder_in_args(args: &str) -> Option<String> {
    let mut parts = Vec::new();
    let mut cur = String::new();
    let mut quoted = false;
    for c in args.chars() {
        match c {
            '"' => quoted = !quoted,
            c if c.is_whitespace() && !quoted => {
                if !cur.is_empty() {
                    parts.push(std::mem::take(&mut cur));
                }
            }
            c => cur.push(c),
        }
    }
    if !cur.is_empty() {
        parts.push(cur);
    }
    parts
        .into_iter()
        .rev()
        .map(|p| p.trim().trim_end_matches(['\\', '/']).to_string())
        .find(|p| p.len() > 3 && std::path::Path::new(p).is_absolute() && std::path::Path::new(p).is_dir())
}

#[cfg(windows)]
mod imp {
    use windows::core::{w, Interface, HSTRING, PCWSTR};
    use windows::Win32::Foundation::{ERROR_CANCELLED, HWND};
    use windows::Win32::System::Com::{
        CoCreateInstance, CoInitializeEx, CoTaskMemFree, CoUninitialize, IPersistFile, CLSCTX_INPROC_SERVER,
        COINIT_APARTMENTTHREADED, STGM_READ,
    };
    use windows::Win32::UI::Shell::Common::COMDLG_FILTERSPEC;
    use windows::Win32::UI::Shell::{
        FileOpenDialog, IFileOpenDialog, IShellItem, IShellLinkW, SHCreateItemFromParsingName, ShellLink,
        FOS_FILEMUSTEXIST, FOS_FORCEFILESYSTEM, FOS_NODEREFERENCELINKS, SIGDN_FILESYSPATH,
    };

    /// COM for this thread, released when dropped.
    struct Com;
    impl Com {
        fn init() -> Result<Self, String> {
            unsafe { CoInitializeEx(None, COINIT_APARTMENTTHREADED).ok() }.map_err(|e| e.to_string())?;
            Ok(Com)
        }
    }
    impl Drop for Com {
        fn drop(&mut self) {
            unsafe { CoUninitialize() };
        }
    }

    pub fn pick_program(owner: isize) -> Result<Option<String>, String> {
        let _com = Com::init()?;
        unsafe {
            let dialog: IFileOpenDialog =
                CoCreateInstance(&FileOpenDialog, None, CLSCTX_INPROC_SERVER).map_err(|e| e.to_string())?;
            let options = dialog.GetOptions().map_err(|e| e.to_string())?;
            dialog
                .SetOptions(options | FOS_NODEREFERENCELINKS | FOS_FILEMUSTEXIST | FOS_FORCEFILESYSTEM)
                .map_err(|e| e.to_string())?;
            let types = [
                COMDLG_FILTERSPEC { pszName: w!("Shortcuts and programs"), pszSpec: w!("*.lnk;*.exe") },
                COMDLG_FILTERSPEC { pszName: w!("All files"), pszSpec: w!("*.*") },
            ];
            dialog.SetFileTypes(&types).map_err(|e| e.to_string())?;
            dialog.SetTitle(w!("Choose the program or Miracle shortcut")).map_err(|e| e.to_string())?;
            // Start where company shortcuts usually are, when that folder exists.
            if std::path::Path::new(r"C:\Miracle-Companies").is_dir() {
                if let Ok(folder) = SHCreateItemFromParsingName::<_, _, IShellItem>(w!(r"C:\Miracle-Companies"), None) {
                    let _ = dialog.SetFolder(&folder);
                }
            }
            let parent = (owner != 0).then(|| HWND(owner as *mut core::ffi::c_void));
            if let Err(e) = dialog.Show(parent) {
                if e.code() == ERROR_CANCELLED.to_hresult() {
                    return Ok(None);
                }
                return Err(e.to_string());
            }
            let item = dialog.GetResult().map_err(|e| e.to_string())?;
            let name = item.GetDisplayName(SIGDN_FILESYSPATH).map_err(|e| e.to_string())?;
            let path = name.to_string().map_err(|e| e.to_string());
            CoTaskMemFree(Some(name.0 as *const core::ffi::c_void));
            path.map(Some)
        }
    }

    pub fn read_shortcut(path: &str) -> Result<(String, String, String), String> {
        let _com = Com::init()?;
        unsafe {
            let link: IShellLinkW =
                CoCreateInstance(&ShellLink, None, CLSCTX_INPROC_SERVER).map_err(|e| e.to_string())?;
            let file: IPersistFile = link.cast().map_err(|e| e.to_string())?;
            file.Load(PCWSTR(HSTRING::from(path).as_ptr()), STGM_READ)
                .map_err(|e| format!("Could not open the shortcut {path}: {e}"))?;
            let mut buf = vec![0u16; 4096];
            let text = |b: &[u16]| String::from_utf16_lossy(&b[..b.iter().position(|&c| c == 0).unwrap_or(b.len())]);
            link.GetPath(&mut buf, std::ptr::null_mut(), 0).map_err(|e| e.to_string())?;
            let target = text(&buf);
            buf.fill(0);
            link.GetArguments(&mut buf).map_err(|e| e.to_string())?;
            let args = text(&buf);
            buf.fill(0);
            link.GetWorkingDirectory(&mut buf).map_err(|e| e.to_string())?;
            let work_dir = text(&buf);
            Ok((target, args, work_dir))
        }
    }
}
