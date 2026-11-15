//! Deterministic merges: two peers merging the same pair of commits produce
//! the identical commit, so they converge instead of merging each other's
//! merges forever.

use std::collections::BTreeMap;
use std::path::Path;

use anyhow::{Context, Result, bail};

use crate::git::{Git, exec};

/// Merges commits `a` and `b` and returns the merge commit. Same-line edits
/// keep git's conflict markers; conflicting binary files keep both versions.
pub fn merge(git: &Git, a: &str, b: &str) -> Result<String> {
    let (ours, theirs) = if a < b { (a, b) } else { (b, a) };
    // Short ids label the conflict markers, the same on every machine.
    let out = git
        .command([
            "merge-tree",
            "--write-tree",
            "--allow-unrelated-histories",
            "-z",
            &ours[..12],
            &theirs[..12],
        ])
        .output()?;
    if !matches!(out.status.code(), Some(0 | 1)) {
        bail!(
            "merge-tree failed: {}",
            String::from_utf8_lossy(&out.stderr)
        );
    }
    let stdout = String::from_utf8_lossy(&out.stdout);
    let mut fields = stdout.split('\0');
    let mut tree = fields
        .next()
        .context("merge-tree printed nothing")?
        .to_owned();

    // Conflicted paths: "<mode> <oid> <stage>\t<path>", until an empty field.
    let mut conflicts: BTreeMap<&str, BTreeMap<&str, (&str, &str)>> = BTreeMap::new();
    for entry in fields.take_while(|f| !f.is_empty()) {
        let (meta, path) = entry.split_once('\t').context("bad merge-tree entry")?;
        let [mode, oid, stage] = meta.split(' ').collect::<Vec<_>>()[..] else {
            bail!("bad merge-tree entry: {entry}");
        };
        conflicts
            .entry(path)
            .or_default()
            .insert(stage, (mode, oid));
    }
    // Both sides changed it, yet git left one side's version as is (binary
    // content, or a `binary`/`-merge` attribute): keep the other beside it.
    let mut unmarked: Vec<(String, &str, &str)> = Vec::new();
    for (path, stages) in &conflicts {
        let (Some(&(m2, o2)), Some(&(m3, o3))) = (stages.get("2"), stages.get("3")) else {
            continue;
        };
        let kept = git
            .value(["rev-parse", &format!("{tree}:{path}")])
            .unwrap_or_default();
        if kept == o2 {
            unmarked.push((side_name(path, o3), m3, o3));
        } else if kept == o3 {
            unmarked.push((side_name(path, o2), m2, o2));
        }
    }
    if !unmarked.is_empty() {
        tree = add_to_tree(git, &tree, &unmarked)?;
    }

    let mut message = "Merge".to_owned();
    if !conflicts.is_empty() {
        message.push_str("\n\nConflicts:\n");
        for path in conflicts.keys() {
            message.push_str(&format!("\t{path}\n"));
        }
    }
    let time = git
        .value(["log", "--no-walk", "--format=%ct", ours, theirs])?
        .lines()
        .filter_map(|t| t.parse::<u64>().ok())
        .max()
        .context("parents without commit times")?;
    let date = format!("{time} +0000");
    let mut cmd = git.command([
        "commit-tree",
        &tree,
        "-p",
        ours,
        "-p",
        theirs,
        "-m",
        &message,
    ]);
    for who in ["AUTHOR", "COMMITTER"] {
        cmd.env(format!("GIT_{who}_NAME"), "vault-sync")
            .env(format!("GIT_{who}_EMAIL"), "vault-sync")
            .env(format!("GIT_{who}_DATE"), &date);
    }
    Ok(exec(cmd)?.trim_end().to_owned())
}

/// `photo.png` → `photo.conflict-1a2b3c4d.png`, beside the original.
fn side_name(path: &str, oid: &str) -> String {
    let p = Path::new(path);
    let stem = p.file_stem().unwrap_or_default().to_string_lossy();
    let name = match p.extension() {
        Some(ext) => format!("{stem}.conflict-{}.{}", &oid[..8], ext.to_string_lossy()),
        None => format!("{stem}.conflict-{}", &oid[..8]),
    };
    match p.parent().filter(|d| !d.as_os_str().is_empty()) {
        Some(dir) => dir.join(name).to_string_lossy().into_owned(),
        None => name,
    }
}

/// `tree` plus extra files, built in a scratch index so the real one is untouched.
fn add_to_tree(git: &Git, tree: &str, files: &[(String, &str, &str)]) -> Result<String> {
    let index = git.git_dir.join("vaultsync").join("merge-index");
    let _ = std::fs::remove_file(&index);
    let with_index = |args: &[&str]| {
        let mut cmd = git.command(args);
        cmd.env("GIT_INDEX_FILE", &index);
        exec(cmd)
    };
    with_index(&["read-tree", tree])?;
    for (path, mode, oid) in files {
        with_index(&[
            "update-index",
            "--add",
            "--cacheinfo",
            &format!("{mode},{oid},{path}"),
        ])?;
    }
    let tree = with_index(&["write-tree"])?;
    std::fs::remove_file(&index)?;
    Ok(tree.trim_end().to_owned())
}
