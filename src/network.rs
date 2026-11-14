//! Where browsers can reach the portal.
//!
//! The public link (Cloudflare Tunnel) and `tailscale serve` connect to the
//! portal on this computer (`BIND_ADDR`, `127.0.0.1:8088` by default). With
//! "Local network" or "Tailscale" switched on in the app, the portal also
//! listens on the computer's other addresses, on the same port, so people can
//! open `http://<this computer's address>:8088` directly:
//!
//! - every connection is checked against the switches: loopback always, private
//!   addresses (192.168.x.x, 10.x.x.x, ...) with Local network, Tailscale's
//!   100.64.0.0/10 with Tailscale; anything else is refused;
//! - a Windows Firewall rule lets exactly those in, for this program and port;
//! - only connections from this computer itself may say, in headers, where the
//!   browser really is (the tunnel does); for direct ones the portal uses the
//!   connection's own address, so access rules and sign-in limits can't be
//!   fooled.

use std::net::{IpAddr, SocketAddr};
use std::sync::{Arc, Mutex, OnceLock, RwLock};

use anyhow::{Context, Result};
use axum::extract::{ConnectInfo, Request, State};
use axum::http::{HeaderValue, StatusCode};
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};
use axum::Router;

use crate::settings::Reach;

pub struct Network {
    /// `BIND_ADDR`: where the tunnel connects.
    bind: String,
    reach: RwLock<Reach>,
    /// The listener's address and its accept loop.
    serving: Mutex<Option<(String, tokio::task::AbortHandle)>>,
    router: OnceLock<Router>,
    /// This computer's addresses, refreshed now and then: (kind, address).
    addresses: RwLock<Vec<(&'static str, String)>>,
}

impl Network {
    pub fn new(bind: String, reach: Reach) -> Arc<Self> {
        Arc::new(Self {
            bind,
            reach: RwLock::new(reach),
            serving: Mutex::new(None),
            router: OnceLock::new(),
            addresses: RwLock::new(Vec::new()),
        })
    }

    pub fn reach(&self) -> Reach {
        *self.reach.read().unwrap()
    }

    pub fn port(&self) -> u16 {
        self.bind.rsplit(':').next().and_then(|p| p.parse().ok()).unwrap_or(8088)
    }

    /// Where the listener should be for the current switches.
    fn wanted(&self) -> String {
        let reach = self.reach();
        let host = self.bind.rsplit_once(':').map(|(h, _)| h).unwrap_or("");
        let loopback = host.trim_matches(['[', ']']).parse::<IpAddr>().is_ok_and(|ip| ip.is_loopback()) || host == "localhost";
        if (reach.lan || reach.tailscale) && loopback {
            format!("0.0.0.0:{}", self.port())
        } else {
            self.bind.clone()
        }
    }

    /// Start serving `router` (the public pages) where the switches say.
    pub async fn start(self: &Arc<Self>, router: Router) -> Result<()> {
        let gated = router.layer(axum::middleware::from_fn_with_state(self.clone(), gate));
        let _ = self.router.set(gated);
        self.listen().await?;
        let me = self.clone();
        tokio::spawn(async move {
            // The firewall rule follows the switches (and the program's place).
            let reach = me.reach();
            let port = me.port();
            tokio::task::spawn_blocking(move || firewall(reach, port)).await.ok();
            let mut every = tokio::time::interval(std::time::Duration::from_secs(120));
            loop {
                every.tick().await;
                let found = tokio::task::spawn_blocking(local_addresses).await.unwrap_or_default();
                *me.addresses.write().unwrap() = found;
            }
        });
        Ok(())
    }

    /// Change the switches: refuse or allow connections at once, and move the
    /// listener and firewall rule. Open sessions are kept.
    pub async fn set_reach(self: &Arc<Self>, reach: Reach) -> Result<()> {
        *self.reach.write().unwrap() = reach;
        self.listen().await?;
        let port = self.port();
        tokio::task::spawn_blocking(move || firewall(reach, port)).await.ok();
        Ok(())
    }

    /// (Re)bind the listener if it isn't where it should be.
    async fn listen(&self) -> Result<()> {
        let wanted = self.wanted();
        {
            let mut serving = self.serving.lock().unwrap();
            if serving.as_ref().is_some_and(|(at, _)| *at == wanted) {
                return Ok(());
            }
            // Stop accepting on the old address; connections already open
            // (sessions) run on their own and keep going.
            if let Some((_, task)) = serving.take() {
                task.abort();
            }
        }
        let mut tries = 0;
        let listener = loop {
            match tokio::net::TcpListener::bind(&wanted).await {
                Ok(l) => break l,
                Err(_) if tries < 20 => {
                    tries += 1;
                    tokio::time::sleep(std::time::Duration::from_millis(100)).await;
                }
                Err(e) => {
                    return Err(e).with_context(|| format!("listening on {wanted} (is another copy of the portal running?)"))
                }
            }
        };
        let app = self.router.get().cloned().context("the portal's pages aren't ready")?;
        let task = tokio::spawn(async move {
            let service = app.into_make_service_with_connect_info::<SocketAddr>();
            if let Err(e) = axum::serve(listener, service).await {
                tracing::error!("the portal stopped serving: {e:#}");
            }
        });
        tracing::info!("access-portal listening on http://{wanted}");
        *self.serving.lock().unwrap() = Some((wanted, task.abort_handle()));
        Ok(())
    }

    /// The addresses people can type, for the app: `[{kind, url}]`.
    pub fn urls(&self) -> Vec<serde_json::Value> {
        let reach = self.reach();
        let port = self.port();
        self.addresses
            .read()
            .unwrap()
            .iter()
            .filter(|(kind, _)| (*kind == "lan" && reach.lan) || (*kind == "tailscale" && reach.tailscale))
            .map(|(kind, address)| serde_json::json!({ "kind": kind, "url": format!("http://{address}:{port}") }))
            .collect()
    }
}

/// The kind of network a browser connects from, if the portal knows it.
fn kind(ip: IpAddr) -> Option<&'static str> {
    match ip {
        IpAddr::V4(v4) => {
            let [a, b, ..] = v4.octets();
            if v4.is_loopback() {
                Some("loopback")
            } else if a == 100 && (64..128).contains(&b) {
                Some("tailscale")
            } else if v4.is_private() || v4.is_link_local() {
                Some("lan")
            } else {
                None
            }
        }
        IpAddr::V6(v6) => {
            if let Some(v4) = v6.to_ipv4_mapped() {
                return kind(IpAddr::V4(v4));
            }
            let s = v6.segments();
            if v6.is_loopback() {
                Some("loopback")
            } else if s[0] == 0xfd7a && s[1] == 0x115c && s[2] == 0xa1e0 {
                Some("tailscale")
            } else if (s[0] & 0xfe00) == 0xfc00 || (s[0] & 0xffc0) == 0xfe80 {
                Some("lan")
            } else {
                None
            }
        }
    }
}

/// Headers only a front end on this computer may set.
const FRONT_END_HEADERS: [&str; 5] = ["cf-connecting-ip", "cf-ipcountry", "x-forwarded-for", "x-forwarded-proto", "x-forwarded-host"];

/// Refuse connections from networks that aren't switched on, and make direct
/// ones carry their real address.
async fn gate(State(network): State<Arc<Network>>, mut request: Request, next: Next) -> Response {
    let Some(ConnectInfo(peer)) = request.extensions().get::<ConnectInfo<SocketAddr>>().copied() else {
        return next.run(request).await;
    };
    let reach = network.reach();
    let ok = match kind(peer.ip()) {
        Some("loopback") => true,
        Some("tailscale") => reach.tailscale,
        Some("lan") => reach.lan,
        _ => false,
    };
    if !ok {
        return (StatusCode::FORBIDDEN, "The portal isn't available from this network.").into_response();
    }
    if !peer.ip().is_loopback() {
        let headers = request.headers_mut();
        for name in FRONT_END_HEADERS {
            headers.remove(name);
        }
        if let Ok(v) = HeaderValue::from_str(&peer.ip().to_string()) {
            headers.insert("x-forwarded-for", v);
        }
    }
    next.run(request).await
}

/// This computer's local-network and Tailscale IPv4 addresses (not virtual
/// adapters such as WSL's or Hyper-V's), plus its Tailscale name if it has one.
fn local_addresses() -> Vec<(&'static str, String)> {
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        const SCRIPT: &str = "Get-NetIPAddress -AddressFamily IPv4 -AddressState Preferred -ErrorAction SilentlyContinue | \
            Where-Object { $_.InterfaceAlias -notmatch '^(vEthernet|Loopback)' } | \
            ForEach-Object { $_.IPAddress }";
        let mut found = Vec::new();
        if let Ok(out) = std::process::Command::new("powershell.exe")
            .args(["-NoProfile", "-NonInteractive", "-Command", SCRIPT])
            .creation_flags(0x0800_0000)
            .output()
        {
            for line in String::from_utf8_lossy(&out.stdout).lines() {
                if let Ok(ip) = line.trim().parse::<IpAddr>() {
                    match kind(ip) {
                        Some(k @ ("lan" | "tailscale")) => found.push((k, ip.to_string())),
                        _ => {}
                    }
                }
            }
        }
        // MagicDNS name, e.g. my-pc.tailnet-name.ts.net.
        let tailscale = std::path::Path::new(&std::env::var("ProgramFiles").unwrap_or_default()).join("Tailscale\\tailscale.exe");
        if let Ok(out) = std::process::Command::new(tailscale)
            .args(["status", "--json", "--self=true", "--peers=false"])
            .creation_flags(0x0800_0000)
            .output()
        {
            if let Ok(status) = serde_json::from_slice::<serde_json::Value>(&out.stdout) {
                if let Some(name) = status.pointer("/Self/DNSName").and_then(|n| n.as_str()) {
                    let name = name.trim_end_matches('.');
                    if !name.is_empty() {
                        found.push(("tailscale", name.to_string()));
                    }
                }
            }
        }
        found
    }
    #[cfg(not(windows))]
    {
        Vec::new()
    }
}

/// Keep the Windows Firewall rule in step with the switches: inbound TCP on the
/// portal's port, for this program, from the local subnet and/or Tailscale.
fn firewall(reach: Reach, port: u16) {
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        const NAME: &str = "Access Portal (local network and Tailscale)";
        let netsh = |args: &[String]| {
            std::process::Command::new("netsh")
                .args(["advfirewall", "firewall"])
                .args(args)
                .creation_flags(0x0800_0000)
                .output()
                .is_ok_and(|o| o.status.success())
        };
        netsh(&["delete".into(), "rule".into(), format!("name={NAME}")]);
        let mut remote = Vec::new();
        if reach.lan {
            remote.push("LocalSubnet");
        }
        if reach.tailscale {
            remote.push("100.64.0.0/10");
            remote.push("fd7a:115c:a1e0::/48");
        }
        if remote.is_empty() {
            return;
        }
        let program = std::env::current_exe().map(|p| p.display().to_string()).unwrap_or_default();
        let ok = netsh(&[
            "add".into(),
            "rule".into(),
            format!("name={NAME}"),
            "dir=in".into(),
            "action=allow".into(),
            "protocol=TCP".into(),
            format!("localport={port}"),
            format!("remoteip={}", remote.join(",")),
            format!("program={program}"),
            "profile=any".into(),
        ]);
        if !ok {
            tracing::warn!("could not add the firewall rule for the local network (the portal needs administrator rights)");
        }
    }
    #[cfg(not(windows))]
    let _ = (reach, port);
}
