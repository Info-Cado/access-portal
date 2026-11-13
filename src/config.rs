use anyhow::Context;

/// All configuration comes from environment variables so nothing secret is baked
/// into the binary. See `.env.example` for the full list.
#[derive(Clone)]
pub struct Config {
    pub database_url: String,
    pub bind_addr: String,

    /// Master secret used to encrypt Windows passwords at rest (and nothing else).
    pub portal_secret: String,

    /// Folder holding each user's transfer files (shown in sessions as the
    /// `\\tsclient\P` drive). Always absolute.
    pub files_dir: std::path::PathBuf,
    /// Largest single upload accepted from the browser, in bytes.
    pub max_upload_bytes: u64,

    /// Try a direct peer-to-peer link (WebRTC) before falling back to WebSocket.
    pub p2p: bool,
    /// STUN/TURN servers used to find a direct path, e.g. `stun:host:3478`.
    pub ice_urls: Vec<String>,
    /// Credentials for TURN servers in `ice_urls`, if any.
    pub ice_username: String,
    pub ice_credential: String,

    /// Fixed session resolution, or `None` to follow the browser window.
    pub session_size: Option<(u16, u16)>,

    /// Allow the H.264 motion path (still needs a browser that can decode it).
    pub video: bool,

    pub session_ttl_secs: i64,
    pub max_failed: i64,
    pub lockout_secs: i64,
    pub cookie_secure: bool,

    pub seed_admin_user: String,
    pub seed_admin_pass: String,
}

fn var(key: &str, default: &str) -> String {
    std::env::var(key).unwrap_or_else(|_| default.to_string())
}

impl Config {
    pub fn from_env() -> anyhow::Result<Self> {
        let portal_secret = std::env::var("PORTAL_SECRET")
            .context("PORTAL_SECRET must be set (any long random string)")?;

        Ok(Self {
            database_url: var("DATABASE_URL", "sqlite://portal.db?mode=rwc"),
            bind_addr: var("BIND_ADDR", "127.0.0.1:8088"),
            portal_secret,
            files_dir: std::path::absolute(var("FILES_DIR", "files"))
                .context("resolving FILES_DIR")?,
            max_upload_bytes: var("MAX_UPLOAD_MB", "2048").parse::<u64>().unwrap_or(2048) * 1024 * 1024,
            p2p: var("P2P", "on") != "off",
            ice_urls: var("ICE_SERVERS", "stun:stun.l.google.com:19302")
                .split(',')
                .map(|u| u.trim().to_string())
                .filter(|u| !u.is_empty())
                .collect(),
            ice_username: var("ICE_USERNAME", ""),
            ice_credential: var("ICE_CREDENTIAL", ""),
            session_size: {
                let v = var("SESSION_SIZE", "1920x1080");
                v.split_once(['x', 'X'])
                    .and_then(|(w, h)| Some((w.trim().parse().ok()?, h.trim().parse().ok()?)))
            },
            video: var("VIDEO", "on") != "off",
            session_ttl_secs: var("SESSION_TTL_SECS", "28800").parse().unwrap_or(28800),
            max_failed: var("MAX_FAILED", "5").parse().unwrap_or(5),
            lockout_secs: var("LOCKOUT_SECS", "300").parse().unwrap_or(300),
            cookie_secure: var("COOKIE_SECURE", "false") == "true",
            seed_admin_user: var("SEED_ADMIN_USER", "admin"),
            seed_admin_pass: var("SEED_ADMIN_PASS", "changeme"),
        })
    }
}
