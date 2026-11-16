//! Deterministic merges: two peers merging the same pair of commits produce
//! the identical commit, so they converge instead of merging each other's
//! merges forever.

use std::collections::BTreeSet;
use std::path::Path;

use anyhow::{Context, Result};
use git2::{
    AttrCheckFlags, AttrValue, Index, IndexEntry, IndexTime, MergeFileInput, MergeFileOptions,
    MergeOptions, Oid, Repository, RepositoryOpenFlags, Signature, Time,
};

use crate::vault::is_binary;

/// The line that marks a merge made by this engine, which every member can
/// recompute exactly. Merges without it came from the earlier engine.
pub const MERGE_TRAILER: &str = "Vault-Merge: 2";

/// Merges commits `a` and `b` and returns the merge commit. Same-line edits
/// keep conflict markers labelled with both sides' short ids; conflicting
/// binary files keep both versions, the second beside the first.
pub fn merge(repo: &Repository, a: Oid, b: Oid) -> Result<Oid> {
    let (ours, theirs) = if a < b { (a, b) } else { (b, a) };
    // Attributes (a `binary` pattern, say) come from the commit merged into,
    // not from this machine's files, so every machine merges alike.
    let repo = Repository::open_ext(
        repo.path(),
        RepositoryOpenFlags::NO_SEARCH | RepositoryOpenFlags::BARE,
        std::iter::empty::<&std::ffi::OsStr>(),
    )?;
    let (ours, theirs) = (repo.find_commit(ours)?, repo.find_commit(theirs)?);
    let mut attributes = Index::new()?;
    attributes.read_tree(&ours.tree()?)?;
    repo.set_index(&mut attributes)?;
    let mut merged = repo.merge_commits(&ours, &theirs, Some(&MergeOptions::new()))?;
    let labels = (
        ours.id().to_string()[..12].to_owned(),
        theirs.id().to_string()[..12].to_owned(),
    );

    let conflicts: Vec<_> = merged.conflicts()?.collect::<Result<_, _>>()?;
    // What a person should look at, for the commit message.
    let mut paths = BTreeSet::new();
    let mut resolved = vec![];
    for c in &conflicts {
        let lossy = |e: &IndexEntry| String::from_utf8_lossy(&e.path).into_owned();
        match (&c.our, &c.their) {
            (Some(o), Some(t)) if o.path == t.path => {
                let (our_text, their_text) = (blob(&repo, o.id)?, blob(&repo, t.id)?);
                let path = String::from_utf8_lossy(&o.path).into_owned();
                let no_merge = matches!(
                    AttrValue::from_string(repo.get_attr(
                        Path::new(&path),
                        "merge",
                        AttrCheckFlags::default()
                    )?),
                    AttrValue::False
                );
                let symlink = |mode: u32| mode & 0o170000 == 0o120000;
                if no_merge
                    || symlink(o.mode)
                    || symlink(t.mode)
                    || is_binary(&our_text)
                    || is_binary(&their_text)
                {
                    paths.insert(path.clone());
                    resolved.push(index_entry(&o.path, o.mode as i32, o.id));
                    let beside = side_name(&path, &t.id.to_string());
                    resolved.push(index_entry(beside.as_bytes(), t.mode as i32, t.id));
                    continue;
                }
                let base_text = match &c.ancestor {
                    Some(a) => blob(&repo, a.id)?,
                    None => vec![],
                };
                let input = |text| {
                    let mut input = MergeFileInput::new();
                    input.content(text);
                    input
                };
                let mut opts = MergeFileOptions::new();
                opts.our_label(&labels.0).their_label(&labels.1);
                let result = git2::merge_file(
                    &input(&base_text),
                    &input(&our_text),
                    &input(&their_text),
                    Some(&mut opts),
                )?;
                if !result.is_automergeable() {
                    paths.insert(path);
                }
                let id = repo.blob(result.content())?;
                resolved.push(index_entry(&o.path, o.mode as i32, id));
            }
            // One side deleted or renamed it: what the other side left as
            // it was goes with it, and what it changed is kept, each version
            // at its own path.
            (our, their) => {
                let unchanged = |e: &IndexEntry| {
                    c.ancestor
                        .as_ref()
                        .is_some_and(|a| (&a.path, a.id, a.mode) == (&e.path, e.id, e.mode))
                };
                for e in [our, their].into_iter().flatten() {
                    if !unchanged(e) {
                        paths.insert(lossy(e));
                        resolved.push(index_entry(&e.path, e.mode as i32, e.id));
                    }
                }
            }
        }
    }
    for c in &conflicts {
        for e in [&c.ancestor, &c.our, &c.their].into_iter().flatten() {
            merged.remove_path(Path::new(&*String::from_utf8_lossy(&e.path)))?;
        }
    }
    for entry in &resolved {
        merged.add(entry)?;
    }
    let tree = repo.find_tree(merged.write_tree_to(&repo)?)?;

    let mut message = "Merge\n\n".to_owned();
    if !paths.is_empty() {
        message.push_str("Conflicts:\n");
        for path in &paths {
            message.push_str(&format!("\t{path}\n"));
        }
        message.push('\n');
    }
    message.push_str(MERGE_TRAILER);
    message.push('\n');
    let time = ours.time().seconds().max(theirs.time().seconds());
    let who = Signature::new("vaultsync", "vaultsync", &Time::new(time, 0))?;
    Ok(repo.commit(None, &who, &who, &message, &tree, &[&ours, &theirs])?)
}

/// Whether `message` is that of a merge this engine made.
pub fn is_current(message: &[u8]) -> bool {
    message
        .split(|b| *b == b'\n')
        .any(|line| line == MERGE_TRAILER.as_bytes())
}

fn blob(repo: &Repository, id: Oid) -> Result<Vec<u8>> {
    Ok(repo
        .find_blob(id)
        .context("reading a merged file")?
        .content()
        .to_vec())
}

/// An index entry for a file already stored in the repository.
pub fn index_entry(path: &[u8], mode: i32, id: Oid) -> IndexEntry {
    IndexEntry {
        ctime: IndexTime::new(0, 0),
        mtime: IndexTime::new(0, 0),
        dev: 0,
        ino: 0,
        mode: mode as u32,
        uid: 0,
        gid: 0,
        file_size: 0,
        id,
        flags: path.len().min(0xfff) as u16,
        flags_extended: 0,
        path: path.to_vec(),
    }
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
