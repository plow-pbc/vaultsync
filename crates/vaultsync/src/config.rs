//! The vaults this user's background service runs: one folder per line in
//! `~/.config/vaultsync/vaults` (`$XDG_CONFIG_HOME` honored).

use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};

fn path() -> Result<PathBuf> {
    let base = match std::env::var_os("XDG_CONFIG_HOME") {
        Some(dir) if !dir.is_empty() => PathBuf::from(dir),
        _ => PathBuf::from(std::env::var_os("HOME").context("HOME is not set")?).join(".config"),
    };
    Ok(base.join("vaultsync").join("vaults"))
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
    fs::write(path, text)?;
    Ok(())
}
