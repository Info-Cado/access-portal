-- A connection-level password for protocols where the secret belongs to the
-- host rather than to each user (VNC). Encrypted at rest like win_password.
-- NULL for RDP apps, which log in with the user's own Windows account.
ALTER TABLE apps ADD COLUMN conn_password TEXT;
