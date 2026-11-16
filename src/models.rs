use sqlx::FromRow;

#[derive(Clone, Debug, FromRow)]
pub struct User {
    pub id: i64,
    pub username: String,
    pub password_hash: String,
    pub is_admin: i64,
    pub totp_secret: Option<String>,
    pub win_username: String,
    pub win_password: String, // encrypted at rest
    pub failed_count: i64,
    pub locked_until: Option<i64>,
    pub created_at: i64,
    /// When and from where the user may sign in (JSON, see `access.rs`).
    pub access_rules: Option<String>,
    /// Until when an administrator has blocked this user (unix seconds); past or
    /// `None` means not blocked. `BLOCK_FOREVER` means until unblocked by hand.
    pub blocked_until: Option<i64>,
}

/// A `blocked_until` far enough ahead to mean "until an administrator unblocks"
/// (year 9999). Shown in the app as an indefinite block, not a date.
pub const BLOCK_FOREVER: i64 = 253_402_300_799;

impl User {
    pub fn rules(&self) -> crate::access::AccessRules {
        crate::access::AccessRules::parse(self.access_rules.as_deref())
    }
    pub fn is_admin(&self) -> bool {
        self.is_admin != 0
    }
    pub fn totp_enabled(&self) -> bool {
        self.totp_secret.as_deref().map(|s| !s.is_empty()).unwrap_or(false)
    }
    /// A reason to refuse the user now, if an administrator has blocked them.
    pub fn block_reason(&self) -> Option<String> {
        let until = self.blocked_until?;
        if until <= crate::db::now() {
            return None;
        }
        if until >= BLOCK_FOREVER {
            Some("Your administrator has blocked your access to the portal.".to_string())
        } else {
            Some("Your administrator has paused your access to the portal. Try again later.".to_string())
        }
    }
}

#[derive(Clone, Debug, FromRow)]
pub struct App {
    pub id: i64,
    pub key: String,
    pub name: String,
    /// RDP host the portal connects to for this app.
    pub hostname: String,
    pub port: String,
    /// When set, the session runs this program on a black screen (RDP alternate
    /// shell) instead of the Windows desktop; the session ends when it closes.
    pub program: Option<String>,
    /// The program's working folder, and the only data folder the user's Windows
    /// account is granted on the host (see `host::scope_folder`).
    pub work_dir: Option<String>,
    pub created_at: i64,
}
