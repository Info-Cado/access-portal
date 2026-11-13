-- Portal schema.
-- A "portal user" logs into this app. It maps to a Windows account on the RDS host.
-- Users never see their Windows password; the portal stores it encrypted and hands it
-- to the gateway behind the scenes.

CREATE TABLE users (
    id            INTEGER PRIMARY KEY AUTOINCREMENT,
    username      TEXT    NOT NULL UNIQUE,
    password_hash TEXT    NOT NULL,          -- argon2 hash of the PORTAL password
    is_admin      INTEGER NOT NULL DEFAULT 0,
    totp_secret   TEXT,                      -- base32; NULL = 2FA disabled
    win_username  TEXT    NOT NULL,          -- mapped Windows / RDS account
    win_password  TEXT    NOT NULL,          -- Windows account password, encrypted at rest
    failed_count  INTEGER NOT NULL DEFAULT 0,
    locked_until  INTEGER,                   -- unix seconds; NULL = not locked
    created_at    INTEGER NOT NULL
);

-- A published app (or full desktop) that the gateway can open over RDP.
CREATE TABLE apps (
    id         INTEGER PRIMARY KEY AUTOINCREMENT,
    key        TEXT    NOT NULL UNIQUE,      -- short id used in URLs
    name       TEXT    NOT NULL,             -- display name (also the Guacamole connection name)
    protocol   TEXT    NOT NULL DEFAULT 'rdp',
    hostname   TEXT    NOT NULL,             -- RDS host reachable from the gateway
    port       TEXT    NOT NULL DEFAULT '3389',
    remote_app TEXT,                         -- e.g. '||excel'; NULL = full desktop
    created_at INTEGER NOT NULL
);

-- Which apps each user may open.
CREATE TABLE user_apps (
    user_id INTEGER NOT NULL REFERENCES users(id) ON DELETE CASCADE,
    app_id  INTEGER NOT NULL REFERENCES apps(id)  ON DELETE CASCADE,
    PRIMARY KEY (user_id, app_id)
);

-- Active portal sessions (cookie token -> user).
CREATE TABLE sessions (
    token      TEXT PRIMARY KEY,
    user_id    INTEGER NOT NULL REFERENCES users(id) ON DELETE CASCADE,
    expires_at INTEGER NOT NULL,
    created_at INTEGER NOT NULL
);

-- Between password step and TOTP step.
CREATE TABLE pending_logins (
    token      TEXT PRIMARY KEY,
    user_id    INTEGER NOT NULL REFERENCES users(id) ON DELETE CASCADE,
    expires_at INTEGER NOT NULL
);
