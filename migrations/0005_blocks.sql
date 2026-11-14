-- An administrator can block a user from the portal for a while (or until
-- unblocked) when disconnecting them, separate from the automatic lockout after
-- wrong passwords. Unix seconds; NULL or a past time means not blocked.
-- A far-future value (see api.rs) means "until an administrator unblocks".
ALTER TABLE users ADD COLUMN blocked_until INTEGER;
