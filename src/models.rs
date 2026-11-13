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
}

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
}

#[derive(Clone, Debug, FromRow)]
pub struct App {
    pub id: i64,
    pub key: String,
    pub name: String,
    /// RDP host the portal connects to for this app.
    pub hostname: String,
    pub port: String,
    pub created_at: i64,
}
