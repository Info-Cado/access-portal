//! The public link: a Cloudflare Tunnel from this machine to `name.<domain>`.
//!
//! On first start the app asks the sign-up service (`provision/worker.js`) for
//! a tunnel, sending a random install id kept in the data folder. The answer
//! (public name and tunnel token) is saved, and Cloudflare's `cloudflared`,
//! shipped next to the app, is kept running with that token. The tunnel only
//! makes outgoing connections, so no router or firewall setup is needed.

use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use serde_json::{json, Value};
use tokio::io::{AsyncBufReadExt, BufReader};

const PROVISION_URL: Option<&str> = option_env!("ACCESS_PORTAL_PROVISION_URL");
const PROVISION_KEY: Option<&str> = option_env!("ACCESS_PORTAL_PROVISION_KEY");

/// What the window shows about the public link.
#[derive(Clone, Default)]
pub struct Status {
    /// `unavailable` (not set up in this build), `setting-up`, `connecting`,
    /// `online`, or `error`.
    pub state: &'static str,
    pub url: Option<String>,
    pub error: Option<String>,
}

impl Status {
    pub fn to_json(&self) -> Value {
        json!({ "state": self.state, "url": self.url, "error": self.error })
    }
}

#[derive(Clone)]
pub struct Tunnel {
    status: Arc<Mutex<Status>>,
    /// The running cloudflared, so it can be stopped when the app quits.
    child: Arc<Mutex<Option<u32>>>,
}

impl Tunnel {
    pub fn status(&self) -> Status {
        self.status.lock().unwrap().clone()
    }

    fn set(&self, state: &'static str, url: Option<String>, error: Option<String>) {
        *self.status.lock().unwrap() = Status { state, url, error };
    }

    /// Stop cloudflared (called when the app quits).
    pub fn stop(&self) {
        if let Some(pid) = self.child.lock().unwrap().take() {
            kill(pid);
        }
    }
}

/// Start keeping the public link up, in the background. `port` is where the
/// portal listens on this machine.
pub fn start(data_dir: PathBuf, port: u16, exe_dir: PathBuf) -> Tunnel {
    let tunnel = Tunnel { status: Arc::default(), child: Arc::default() };
    let (Some(url), Some(key)) = (PROVISION_URL, PROVISION_KEY) else {
        tunnel.set("unavailable", None, Some("This build has no public link service configured.".into()));
        return tunnel;
    };
    tunnel.set("setting-up", None, None);
    let t = tunnel.clone();
    tauri::async_runtime::spawn(async move {
        let cloudflared = exe_dir.join("cloudflared.exe");
        kill_strays(&cloudflared);
        // Get (or reuse) this install's tunnel.
        let info = loop {
            match get_tunnel(&data_dir, url, key, port).await {
                Ok(info) => break info,
                Err(e) => {
                    tracing::warn!("public link setup failed: {e:#}");
                    t.set("error", None, Some(format!("Setting up the public link failed: {e}. Retrying…")));
                    tokio::time::sleep(Duration::from_secs(30)).await;
                }
            }
        };
        keep_running(&t, &cloudflared, &info).await;
    });
    tunnel
}

struct TunnelInfo {
    url: String,
    token: String,
}

/// The saved tunnel, or a new one from the sign-up service.
async fn get_tunnel(data_dir: &Path, url: &str, key: &str, port: u16) -> anyhow::Result<TunnelInfo> {
    let saved = data_dir.join("tunnel.json");
    if let Ok(text) = std::fs::read_to_string(&saved) {
        if let Ok(v) = serde_json::from_str::<Value>(&text) {
            if let (Some(u), Some(t)) = (v["url"].as_str(), v["token"].as_str()) {
                return Ok(TunnelInfo { url: u.to_string(), token: t.to_string() });
            }
        }
    }

    let install_id = install_id(data_dir)?;
    let name_hint = std::env::var("COMPUTERNAME").unwrap_or_default();
    let body = json!({ "install_id": install_id, "port": port, "name_hint": name_hint }).to_string();
    let url = url.to_string();
    let key = key.to_string();
    // A plain blocking HTTPS call on a worker thread keeps dependencies small.
    let answer: Value = tokio::task::spawn_blocking(move || -> anyhow::Result<Value> {
        let response = ureq::post(&url)
            .header("x-provision-key", &key)
            .header("content-type", "application/json")
            .config()
            .http_status_as_error(false)
            .timeout_global(Some(Duration::from_secs(60)))
            .build()
            .send(body)?;
        let status = response.status().as_u16();
        let value: Value = response.into_body().read_json().unwrap_or(Value::Null);
        if status != 200 {
            anyhow::bail!("{}", value["error"].as_str().unwrap_or(&format!("the service answered {status}")));
        }
        Ok(value)
    })
    .await??;

    let (Some(u), Some(t)) = (answer["url"].as_str(), answer["token"].as_str()) else {
        anyhow::bail!("the service gave an incomplete answer");
    };
    std::fs::write(&saved, json!({ "url": u, "token": t }).to_string())?;
    Ok(TunnelInfo { url: u.to_string(), token: t.to_string() })
}

/// A random id for this install, made once and kept.
fn install_id(data_dir: &Path) -> anyhow::Result<String> {
    let file = data_dir.join("install-id");
    if let Ok(id) = std::fs::read_to_string(&file) {
        let id = id.trim();
        if id.len() == 32 && id.bytes().all(|b| b.is_ascii_hexdigit()) {
            return Ok(id.to_ascii_lowercase());
        }
    }
    let mut bytes = [0u8; 16];
    getrandom::fill(&mut bytes).map_err(|e| anyhow::anyhow!("random: {e}"))?;
    let id: String = bytes.iter().map(|b| format!("{b:02x}")).collect();
    std::fs::write(&file, &id)?;
    Ok(id)
}

/// Run cloudflared with the tunnel token; restart it whenever it stops.
async fn keep_running(t: &Tunnel, cloudflared: &Path, info: &TunnelInfo) {
    let mut wait = 2;
    loop {
        t.set("connecting", Some(info.url.clone()), None);
        let mut command = tokio::process::Command::new(cloudflared);
        command
            .args(["tunnel", "--no-autoupdate", "run"])
            // The token in the environment, not on the command line.
            .env("TUNNEL_TOKEN", &info.token)
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            .kill_on_drop(true);
        #[cfg(windows)]
        command.creation_flags(0x0800_0000); // no console window
        let mut child = match command.spawn() {
            Ok(c) => c,
            Err(e) => {
                t.set("error", Some(info.url.clone()), Some(format!("Could not start the tunnel program: {e}")));
                tokio::time::sleep(Duration::from_secs(30)).await;
                continue;
            }
        };
        *t.child.lock().unwrap() = child.id();
        #[cfg(windows)]
        if let Some(handle) = child.raw_handle() {
            tie_to_app(handle);
        }

        // cloudflared logs to stderr; a registered connection means the link is up.
        if let Some(stderr) = child.stderr.take() {
            let mut lines = BufReader::new(stderr).lines();
            while let Ok(Some(line)) = lines.next_line().await {
                if line.contains("Registered tunnel connection") {
                    t.set("online", Some(info.url.clone()), None);
                    wait = 2;
                } else if line.contains(" ERR ") {
                    tracing::warn!("cloudflared: {line}");
                }
            }
        }
        let _ = child.wait().await;
        *t.child.lock().unwrap() = None;
        tracing::warn!("cloudflared stopped; restarting in {wait} s");
        t.set("connecting", Some(info.url.clone()), Some("The tunnel stopped; reconnecting…".into()));
        tokio::time::sleep(Duration::from_secs(wait)).await;
        wait = (wait * 2).min(60);
    }
}

/// Put a child process in a job that Windows closes when the app's process
/// ends for any reason (quit, crash, killed), which ends the child with it.
#[cfg(windows)]
fn tie_to_app(child: std::os::windows::io::RawHandle) {
    use std::sync::OnceLock;
    use windows_sys::Win32::System::JobObjects::{
        AssignProcessToJobObject, CreateJobObjectW, JobObjectExtendedLimitInformation, SetInformationJobObject,
        JOBOBJECT_EXTENDED_LIMIT_INFORMATION, JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE,
    };
    // One job for the app's lifetime; its handle is never closed by the app.
    static JOB: OnceLock<usize> = OnceLock::new();
    let job = *JOB.get_or_init(|| unsafe {
        let job = CreateJobObjectW(std::ptr::null(), std::ptr::null());
        if job.is_null() {
            return 0;
        }
        let mut info: JOBOBJECT_EXTENDED_LIMIT_INFORMATION = std::mem::zeroed();
        info.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;
        SetInformationJobObject(
            job,
            JobObjectExtendedLimitInformation,
            &info as *const _ as *const core::ffi::c_void,
            std::mem::size_of::<JOBOBJECT_EXTENDED_LIMIT_INFORMATION>() as u32,
        );
        job as usize
    });
    if job != 0 {
        unsafe {
            AssignProcessToJobObject(job as _, child as _);
        }
    }
}

fn kill(pid: u32) {
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        let _ = std::process::Command::new("taskkill")
            .args(["/PID", &pid.to_string(), "/T", "/F"])
            .creation_flags(0x0800_0000)
            .status();
    }
}

/// A cloudflared from an earlier run of this app that was not stopped (the app
/// crashed or was killed) would keep a second connection open; end it.
fn kill_strays(cloudflared: &Path) {
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        let filter = format!(
            "Get-CimInstance Win32_Process -Filter \"Name='cloudflared.exe'\" | Where-Object {{ $_.ExecutablePath -eq '{}' }} | ForEach-Object {{ Stop-Process -Id $_.ProcessId -Force }}",
            cloudflared.display().to_string().replace('\'', "''")
        );
        let _ = std::process::Command::new("powershell.exe")
            .args(["-NoProfile", "-NonInteractive", "-Command", &filter])
            .creation_flags(0x0800_0000)
            .status();
    }
}
