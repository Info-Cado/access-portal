-- An app can open a single program on a black screen (RDP "alternate shell"),
-- instead of the Windows desktop. `program` is the executable the session runs
-- in place of Explorer; when it closes, the session ends. `work_dir` is that
-- program's working folder, and also the only data folder the user's Windows
-- account is granted on this computer (see host::scope_folder). Both NULL keeps
-- the old behaviour: the app opens the Windows desktop of the target host.
ALTER TABLE apps ADD COLUMN program  TEXT;
ALTER TABLE apps ADD COLUMN work_dir TEXT;
