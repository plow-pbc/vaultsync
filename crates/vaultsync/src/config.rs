//! The vaults this user's background service runs: one folder per line in
//! `~/.config/vaultsync/vaults`.

use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};

fn path() -> Result<PathBuf> {
    // Tests point this elsewhere; the service and the CLI always agree on it.
    let dir = match std::env::var_os("VAULTSYNC_CONFIG_DIR") {
        Some(dir) => PathBuf::from(dir),
        None => PathBuf::from(std::env::var_os("HOME").context("HOME is not set")?)
            .join(".config/vaultsync"),
    };
    Ok(dir.join("vaults"))
}

pub fn vaults() -> Result<Vec<PathBuf>> {
    let path = path()?;
    let text = match fs::read_to_string(&path) {
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => String::new(),
        read => read.with_context(|| format!("reading {}", path.display()))?,
    };
    Ok(text
        .lines()
        .map(str::trim)
        .filter(|l| !l.is_empty())
        .map(PathBuf::from)
        .collect())
}

/// Adds `folder` to the list, once.
pub fn track(folder: &Path) -> Result<()> {
    let mut vaults = vaults()?;
    if vaults.iter().any(|v| v == folder) {
        return Ok(());
    }
    vaults.push(folder.to_owned());
    let path = path()?;
    fs::create_dir_all(path.parent().expect("has a parent"))?;
    let text: String = vaults
        .iter()
        .map(|v| format!("{}\n", v.display()))
        .collect();
    // Renamed into place: never half written, and a symlink at the path is
    // replaced rather than followed.
    let tmp = path.with_extension(format!("{}.tmp", std::process::id()));
    fs::write(&tmp, text)?;
    fs::rename(tmp, path)?;
    Ok(())
}
