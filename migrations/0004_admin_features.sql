-- Settings changed from the app: admin sign-in, session timeouts, screen viewing.
CREATE TABLE settings (
    key   TEXT PRIMARY KEY,
    value TEXT NOT NULL
);

-- What happened and when, for the app's Activity screen. No foreign key on
-- user_id: the history of a deleted user stays.
CREATE TABLE activity (
    id       INTEGER PRIMARY KEY AUTOINCREMENT,
    at       INTEGER NOT NULL,             -- unix seconds
    user_id  INTEGER,
    username TEXT    NOT NULL DEFAULT '',
    kind     TEXT    NOT NULL,             -- signin, session_start, print, admin, ...
    detail   TEXT    NOT NULL DEFAULT '',
    address  TEXT    NOT NULL DEFAULT ''
);
CREATE INDEX activity_at ON activity (at);
CREATE INDEX activity_user ON activity (user_id, at);

-- When and from where a user may sign in, as JSON (see src/access.rs).
-- NULL: any time, from anywhere.
ALTER TABLE users ADD COLUMN access_rules TEXT;
