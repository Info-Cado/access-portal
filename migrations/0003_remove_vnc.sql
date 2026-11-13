-- The portal now opens every app with its built-in RDP client. VNC apps and the
-- Guacamole gateway are gone, along with the columns only they used.
DELETE FROM user_apps WHERE app_id IN (SELECT id FROM apps WHERE protocol = 'vnc');
DELETE FROM apps WHERE protocol = 'vnc';
ALTER TABLE apps DROP COLUMN conn_password;
ALTER TABLE apps DROP COLUMN remote_app;
ALTER TABLE apps DROP COLUMN protocol;
