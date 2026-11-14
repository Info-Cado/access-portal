//! Where browsers can reach the portal.
//!
//! The portal always serves plain HTTP on this computer only (`BIND_ADDR`,
//! `127.0.0.1:8088` by default); the public link (Cloudflare Tunnel) and
//! `tailscale serve` connect there.
//!
//! With "Local network" or "Tailscale" switched on in the app, the portal also
//! serves **HTTPS** on every interface, on the TLS port (`8443` by default),
//! with a self-signed certificate it generates once. Browsers only turn on the
//! efficient H.264/AAC paths in a secure context (HTTPS or localhost), so the
//! local addresses have to be HTTPS to match the public link's quality; the
//! certificate is self-signed, so each device shows a one-time warning the
//! first time. People open `https://<this computer's address>:8443`.
//!
//! - every connection is checked against the switches: loopback always, private
//!   addresses (192.168.x.x, 10.x.x.x, ...) with Local network, Tailscale's
//!   100.64.0.0/10 with Tailscale; anything else is refused;
//! - a Windows Firewall rule lets exactly those in, for this program and the
//!   TLS port;
//! - only connections from this computer itself may say, in headers, where the
//!   browser really is (the tunnel does); for direct ones the portal uses the
//!   connection's own address, so access rules and sign-in limits can't be
//!   fooled.

use std::net::{IpAddr, SocketAddr};
use std::path::PathBuf;
use std::sync::{Arc, Mutex, OnceLock, RwLock};

use anyhow::{Context, Result};
use axum::extract::{ConnectInfo, Request, State};
use axum::http::{HeaderValue, StatusCode};
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};
use axum::Router;
use axum_server::tls_rustls::RustlsConfig;
use axum_server::Handle;

use crate::settings::Reach;

/// The HTTPS port for the local network and Tailscale (plain HTTP stays on
/// loopback for the tunnel). Overridable with `TLS_PORT`.
fn tls_port() -> u16 {
    std::env::var("TLS_PORT").ok().and_then(|v| v.parse().ok()).unwrap_or(8443)
}

pub struct Network {
    /// `BIND_ADDR`: the loopback HTTP address the tunnel connects to.
    bind: String,
    tls_port: u16,
    data_dir: PathBuf,
    reach: RwLock<Reach>,
    router: OnceLock<Router>,
    /// The running HTTPS listener (its shutdown handle), or `None` when neither
    /// switch is on.
    tls: Mutex<Option<Handle>>,
    /// This computer's addresses, refreshed now and then: (kind, address).
    addresses: RwLock<Vec<(&'static str, String)>>,
}

impl Network {
    pub fn new(bind: String, reach: Reach, data_dir: PathBuf) -> Arc<Self> {
        Arc::new(Self {
            bind,
            tls_port: tls_port(),
            data_dir,
            reach: RwLock::new(reach),
            router: OnceLock::new(),
            tls: Mutex::new(None),
            addresses: RwLock::new(Vec::new()),
        })
    }

    pub fn reach(&self) -> Reach {
        *self.reach.read().unwrap()
    }

    /// The HTTP port the tunnel connects to (loopback).
    pub fn port(&self) -> u16 {
        self.bind.rsplit(':').next().and_then(|p| p.parse().ok()).unwrap_or(8088)
    }

    /// Start serving `router` (the public pages): HTTP on loopback always, and
    /// HTTPS on every interface while a switch is on.
    pub async fn start(self: &Arc<Self>, router: Router) -> Result<()> {
        // rustls needs a process-wide crypto provider; harmless if already set.
        let _ = rustls::crypto::aws_lc_rs::default_provider().install_default();

        let gated = router.layer(axum::middleware::from_fn_with_state(self.clone(), gate));
        let _ = self.router.set(gated.clone());

        // The loopback HTTP listener never moves; it carries the tunnel.
        let http_app = gated.into_make_service_with_connect_info::<SocketAddr>();
        let listener = tokio::net::TcpListener::bind(&self.bind)
            .await
            .with_context(|| format!("listening on {} (is another copy of the portal running?)", self.bind))?;
        tracing::info!("access-portal listening on http://{}", self.bind);
        tokio::spawn(async move {
            if let Err(e) = axum::serve(listener, http_app).await {
                tracing::error!("the portal stopped serving: {e:#}");
            }
        });

        self.apply().await;

        let me = self.clone();
        tokio::spawn(async move {
            let mut every = tokio::time::interval(std::time::Duration::from_secs(120));
            loop {
                every.tick().await;
                let found = tokio::task::spawn_blocking(local_addresses).await.unwrap_or_default();
                *me.addresses.write().unwrap() = found;
            }
        });
        Ok(())
    }

    /// Change the switches: refuse or allow connections at once (the gate reads
    /// them live), move the HTTPS listener and firewall rule to match. Open
    /// sessions are kept.
    pub async fn set_reach(self: &Arc<Self>, reach: Reach) -> Result<()> {
        *self.reach.write().unwrap() = reach;
        self.apply().await;
        Ok(())
    }

    /// Bring the HTTPS listener and firewall rule in line with the switches.
    async fn apply(self: &Arc<Self>) {
        let reach = self.reach();
        let want_tls = reach.lan || reach.tailscale;

        let running = self.tls.lock().unwrap().is_some();
        if want_tls && !running {
            match self.start_tls().await {
                Ok(handle) => *self.tls.lock().unwrap() = Some(handle),
                Err(e) => tracing::error!("could not start the local HTTPS listener: {e:#}"),
            }
        } else if !want_tls && running {
            if let Some(handle) = self.tls.lock().unwrap().take() {
                handle.shutdown();
            }
        }

        let port = self.tls_port;
        tokio::task::spawn_blocking(move || firewall(reach, port)).await.ok();
    }

    /// Spawn the HTTPS listener on every interface and return its shutdown handle.
    async fn start_tls(self: &Arc<Self>) -> Result<Handle> {
        let config = self.tls_config().await?;
        let app = self.router.get().cloned().context("the portal's pages aren't ready")?;
        let service = app.into_make_service_with_connect_info::<SocketAddr>();
        let addr: SocketAddr = ([0, 0, 0, 0], self.tls_port).into();
        let handle = Handle::new();
        let h = handle.clone();
        tokio::spawn(async move {
            if let Err(e) = axum_server::bind_rustls(addr, config).handle(h).serve(service).await {
                tracing::error!("the portal's HTTPS listener stopped: {e:#}");
            }
        });
        tracing::info!("access-portal listening on https://0.0.0.0:{}", self.tls_port);
        Ok(handle)
    }

    /// The self-signed certificate for the local addresses, made once and kept
    /// in the data folder so devices don't have to trust a new one each start.
    ///
    /// Serves **HTTP/1.1 only** (ALPN): HTTP/2 carries the host in the
    /// `:authority` pseudo-header and sends no `Host` header, which the
    /// cross-site check needs, so an h2 browser would be refused.
    async fn tls_config(&self) -> Result<RustlsConfig> {
        let cert_path = self.data_dir.join("local-cert.pem");
        let key_path = self.data_dir.join("local-key.pem");
        if !cert_path.exists() || !key_path.exists() {
            let (cert_pem, key_pem) = generate_cert().context("generating the local certificate")?;
            std::fs::write(&cert_path, cert_pem).context("writing the local certificate")?;
            std::fs::write(&key_path, key_pem).context("writing the local certificate key")?;
        }
        let cert_bytes = std::fs::read(&cert_path).context("reading the local certificate")?;
        let key_bytes = std::fs::read(&key_path).context("reading the local certificate key")?;
        let certs = rustls_pemfile::certs(&mut &cert_bytes[..])
            .collect::<Result<Vec<_>, _>>()
            .context("parsing the local certificate")?;
        let key = rustls_pemfile::private_key(&mut &key_bytes[..])
            .context("parsing the local certificate key")?
            .context("the local certificate key file has no private key")?;
        let mut config = rustls::ServerConfig::builder()
            .with_no_client_auth()
            .with_single_cert(certs, key)
            .context("building the TLS configuration")?;
        config.alpn_protocols = vec![b"http/1.1".to_vec()];
        Ok(RustlsConfig::from_config(std::sync::Arc::new(config)))
    }

    /// The addresses people can type, for the app: `[{kind, url}]`.
    pub fn urls(&self) -> Vec<serde_json::Value> {
        let reach = self.reach();
        let port = self.tls_port;
        self.addresses
            .read()
            .unwrap()
            .iter()
            .filter(|(kind, _)| (*kind == "lan" && reach.lan) || (*kind == "tailscale" && reach.tailscale))
            .map(|(kind, address)| serde_json::json!({ "kind": kind, "url": format!("https://{address}:{port}") }))
            .collect()
    }
}

/// A self-signed certificate (PEM cert, PEM key) covering this computer's local
/// names and addresses. Self-signed, so browsers warn once per device whatever
/// the names are; the names just keep the warning to a single click.
fn generate_cert() -> Result<(String, String)> {
    let mut names = vec!["localhost".to_string(), "127.0.0.1".to_string()];
    for (_, address) in local_addresses() {
        if !names.contains(&address) {
            names.push(address);
        }
    }
    let mut params = rcgen::CertificateParams::new(names)?;
    params.distinguished_name = rcgen::DistinguishedName::new();
    params
        .distinguished_name
        .push(rcgen::DnType::CommonName, "Access Portal (local)");
    let key = rcgen::KeyPair::generate()?;
    let cert = params.self_signed(&key)?;
    Ok((cert.pem(), key.serialize_pem()))
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
        // HTTPS from the local network: tell the app so cookies are marked Secure.
        if let Ok(v) = HeaderValue::from_str("https") {
            headers.insert("x-forwarded-proto", v);
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
/// TLS port, for this program, from the local subnet and/or Tailscale.
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
