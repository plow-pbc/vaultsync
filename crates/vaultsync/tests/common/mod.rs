#![allow(dead_code)]

use std::path::Path;
use std::process::{Command, Output};

pub fn vaultsync(vault: &Path, args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_vaultsync"))
        .arg("--vault")
        .arg(vault)
        .args(args)
        .env_remove("VAULTSYNC_AGENT")
        .output()
        .unwrap()
}

pub fn ok(vault: &Path, args: &[&str]) -> String {
    let out = vaultsync(vault, args);
    assert!(
        out.status.success(),
        "vaultsync {args:?} failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8(out.stdout).unwrap()
}

/// Plain git, independent of the code under test.
pub fn git(dir: &Path, args: &[&str]) -> String {
    let out = Command::new("git")
        .args([
            "-c",
            "user.name=someone",
            "-c",
            "user.email=someone@example.com",
        ])
        .args([
            "-c",
            "commit.gpgsign=false",
            "-c",
            "init.defaultBranch=main",
        ])
        .args(args)
        .current_dir(dir)
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "git {args:?}: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8(out.stdout).unwrap()
}

/// Polls `cond` until it holds or `secs` pass; true if it held.
pub fn wait_until(secs: u64, mut cond: impl FnMut() -> bool) -> bool {
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(secs);
    while std::time::Instant::now() < deadline {
        if cond() {
            return true;
        }
        std::thread::sleep(std::time::Duration::from_millis(200));
    }
    cond()
}
