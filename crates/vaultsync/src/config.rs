//! The vaults this user's background service runs: one file per vault in
//! `~/.config/vaultsync/vaults/`, holding the vault's folder. Adding a vault
//! writes only its own file, so registrations never race over a shared list.

use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};

fn base() -> Result<PathBuf> {
    // Tests point this elsewhere; the service and the CLI always agree on it.
    Ok(match std::env::var_os("VAULTSYNC_CONFIG_DIR") {
        Some(dir) => PathBuf::from(dir),
        None => PathBuf::from(std::env::var_os("HOME").context("HOME is not set")?)
            .join(".config/vaultsync"),
    })
}

pub fn vaults() -> Result<Vec<PathBuf>> {
    let dir = base()?.join("vaults");
    let entries = match fs::read_dir(&dir) {
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(vec![]),
        read => read.with_context(|| format!("reading {}", dir.display()))?,
    };
    let mut vaults = vec![];
    for entry in entries {
        let text = fs::read_to_string(entry?.path())?;
        let folder = text.trim();
        if !folder.is_empty() {
            vaults.push(PathBuf::from(folder));
        }
    }
    vaults.sort();
    Ok(vaults)
}

/// Adds `folder` to the service's vaults.
pub fn track(folder: &Path) -> Result<()> {
    refuse_overlap(folder)?;
    let dir = base()?.join("vaults");
    fs::create_dir_all(&dir)?;
    let name = blake3::hash(folder.as_os_str().as_encoded_bytes()).to_hex();
    fs::write(dir.join(&name[..16]), format!("{}\n", folder.display()))?;
    Ok(())
}

/// A vault may not be, or hold, this configuration: a vault's files come
/// from other machines, and they must never land among these.
pub fn refuse_overlap(folder: &Path) -> Result<()> {
    let folder = std::path::absolute(folder)?;
    let base = base()?;
    if base.starts_with(&folder) || folder.starts_with(&base) {
        bail!(
            "{} overlaps vaultsync's own settings in {}; choose another folder",
            folder.display(),
            base.display()
        );
    }
    Ok(())
}
