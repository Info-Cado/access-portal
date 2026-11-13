//! Backups of everything the portal keeps: the database (a consistent copy,
//! made with `VACUUM INTO` while the portal runs), the settings file and the
//! public link's details. One folder per backup under `<data>\backups`, made
//! daily, the oldest removed after `keep`; optionally copied to a second
//! folder too (another drive, a synced folder).
//!
//! Restoring is done at the next start, before the database is opened: the app
//! asks for a restore (a marker file) and restarts itself.

use std::path::{Path, PathBuf};

use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};

/// Files copied besides the database.
const EXTRA_FILES: [&str; 3] = ["portal.env", "tunnel.json", "install-id"];
const RESTORE_MARKER: &str = "restore-pending";

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Settings {
    /// Also copy each backup here ("" = only in the data folder).
    #[serde(default)]
    pub copy_dir: String,
    /// How many backups to keep.
    #[serde(default = "default_keep")]
    pub keep: u32,
}

fn default_keep() -> u32 {
    14
}

impl Default for Settings {
    fn default() -> Self {
        Self { copy_dir: String::new(), keep: default_keep() }
    }
}

#[derive(Serialize)]
pub struct Backup {
    pub name: String,
    /// Unix seconds.
    pub at: i64,
    pub size: u64,
}

pub fn dir(data_dir: &Path) -> PathBuf {
    data_dir.join("backups")
}

fn valid_name(name: &str) -> bool {
    name.len() == 17 && name.bytes().all(|b| b.is_ascii_digit() || b == b'-')
}

/// Make a backup now; returns its name ("2026-11-14-031500").
pub async fn make(pool: &sqlx::SqlitePool, data_dir: &Path, settings: &Settings) -> Result<String> {
    make_with(pool, data_dir, settings, true).await
}

/// A backup that removes no older ones (taken just before a restore, which
/// must not remove the backup being restored).
pub async fn make_keeping_all(pool: &sqlx::SqlitePool, data_dir: &Path, settings: &Settings) -> Result<String> {
    make_with(pool, data_dir, settings, false).await
}

async fn make_with(pool: &sqlx::SqlitePool, data_dir: &Path, settings: &Settings, prune_old: bool) -> Result<String> {
    // Named after the time to the second; two in one second wait for the next.
    let (name, target) = loop {
        let t = crate::clock::now();
        let name = format!("{:04}-{:02}-{:02}-{:02}{:02}{:02}", t.year, t.month, t.day, t.hour, t.minute, t.second);
        let target = dir(data_dir).join(&name);
        if !target.exists() {
            break (name, target);
        }
        tokio::time::sleep(std::time::Duration::from_millis(300)).await;
    };
    tokio::fs::create_dir_all(&target).await?;
    let db = target.join("portal.db");
    // A consistent copy of the live database.
    sqlx::query("VACUUM INTO ?")
        .bind(db.display().to_string())
        .execute(pool)
        .await
        .context("copying the database")?;
    for file in EXTRA_FILES {
        let from = data_dir.join(file);
        if from.exists() {
            tokio::fs::copy(&from, target.join(file)).await.with_context(|| format!("copying {file}"))?;
        }
    }
    if prune_old {
        prune(&dir(data_dir), settings.keep)?;
    }
    if prune_old && !settings.copy_dir.trim().is_empty() {
        let copy = PathBuf::from(settings.copy_dir.trim()).join("Access Portal backups");
        let result = copy_folder(&target, &copy.join(&name)).and_then(|_| prune(&copy, settings.keep));
        if let Err(e) = result {
            tracing::warn!("copying the backup to {}: {e:#}", copy.display());
            bail!("Backed up, but copying to {} failed: {e}", copy.display());
        }
    }
    tracing::info!("backup {name} made");
    Ok(name)
}

fn copy_folder(from: &Path, to: &Path) -> Result<()> {
    std::fs::create_dir_all(to)?;
    for entry in std::fs::read_dir(from)? {
        let entry = entry?;
        std::fs::copy(entry.path(), to.join(entry.file_name()))?;
    }
    Ok(())
}

/// Keep the newest `keep` backups in `folder`.
fn prune(folder: &Path, keep: u32) -> Result<()> {
    let mut names: Vec<String> = match std::fs::read_dir(folder) {
        Ok(entries) => entries
            .flatten()
            .filter_map(|e| e.file_name().into_string().ok())
            .filter(|n| valid_name(n))
            .collect(),
        Err(_) => return Ok(()),
    };
    names.sort();
    let extra = names.len().saturating_sub(keep.max(1) as usize);
    for name in &names[..extra] {
        // A file written a moment ago can be held briefly (by antivirus, say).
        for attempt in 0..5 {
            match std::fs::remove_dir_all(folder.join(name)) {
                Ok(()) => break,
                Err(_) if attempt < 4 => std::thread::sleep(std::time::Duration::from_millis(200)),
                Err(e) => tracing::warn!("removing old backup {name}: {e}"),
            }
        }
    }
    Ok(())
}

/// Backups in the data folder, newest first.
pub fn list(data_dir: &Path) -> Vec<Backup> {
    let mut out: Vec<Backup> = std::fs::read_dir(dir(data_dir))
        .into_iter()
        .flatten()
        .flatten()
        .filter_map(|e| {
            let name = e.file_name().into_string().ok().filter(|n| valid_name(n))?;
            let db = e.path().join("portal.db");
            let meta = std::fs::metadata(&db).ok()?;
            let at = meta
                .modified()
                .ok()
                .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
                .map(|d| d.as_secs() as i64)
                .unwrap_or(0);
            Some(Backup { name, at, size: meta.len() })
        })
        .collect();
    out.sort_by(|a, b| b.name.cmp(&a.name));
    out
}

/// Ask for `name` to be restored at the next start.
pub fn request_restore(data_dir: &Path, name: &str) -> Result<()> {
    if !valid_name(name) || !dir(data_dir).join(name).join("portal.db").exists() {
        bail!("That backup doesn't exist.");
    }
    std::fs::write(data_dir.join(RESTORE_MARKER), name)?;
    Ok(())
}

/// At start, before the database is opened: put a requested backup in place.
/// The files it replaces are kept next to it with a `.before-restore` suffix.
pub fn restore_if_requested(data_dir: &Path) -> Result<()> {
    let marker = data_dir.join(RESTORE_MARKER);
    let Ok(name) = std::fs::read_to_string(&marker) else { return Ok(()) };
    let _ = std::fs::remove_file(&marker);
    let name = name.trim();
    let from = dir(data_dir).join(name);
    if !valid_name(name) || !from.join("portal.db").exists() {
        bail!("the backup to restore ({name}) is missing");
    }
    for file in std::iter::once("portal.db").chain(EXTRA_FILES) {
        let source = from.join(file);
        if !source.exists() {
            continue;
        }
        let live = data_dir.join(file);
        if live.exists() {
            std::fs::copy(&live, data_dir.join(format!("{file}.before-restore")))?;
        }
        std::fs::copy(&source, &live).with_context(|| format!("restoring {file}"))?;
    }
    // The old database's write-ahead files belong to the replaced copy.
    for suffix in ["-wal", "-shm"] {
        let _ = std::fs::remove_file(data_dir.join(format!("portal.db{suffix}")));
    }
    tracing::warn!("restored backup {name}");
    Ok(())
}
