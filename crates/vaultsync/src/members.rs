//! The vault's member list (`.vault/members`) and the signatures that make
//! commits count.
//!
//! Every commit this tool makes carries `Vault-Signature: <node id> <sig>`, an
//! ed25519 signature by the machine's node key over the commit's tree and
//! parents. Once a vault has a member list, a commit arriving from a peer is
//! accepted only if a member at its parent signed it, or it is exactly the
//! deterministic merge of its parents. A removed machine stays removed: its tombstone
//! (`removed <id> <name>`) outranks any line listing it, and never goes away.

use std::collections::{BTreeMap, HashMap};

use anyhow::{Context, Result, bail};
use iroh::{EndpointId, SecretKey, Signature};

use crate::git::Git;
use crate::merge::merge;

pub const MEMBERS_PATH: &str = ".vault/members";
const TRAILER: &str = "Vault-Signature:";
const REMOVED: &str = "removed ";

/// Node id → machine name, for listed and for removed machines.
#[derive(Clone, Default)]
pub struct Members {
    pub listed: BTreeMap<EndpointId, String>,
    pub removed: BTreeMap<EndpointId, String>,
}

impl Members {
    /// Listed and not removed: removal wins when a merge keeps both lines.
    pub fn contains(&self, id: &EndpointId) -> bool {
        self.listed.contains_key(id) && !self.removed.contains_key(id)
    }

    pub fn active(&self) -> impl Iterator<Item = (&EndpointId, &String)> {
        self.listed
            .iter()
            .filter(|(id, _)| !self.removed.contains_key(*id))
    }
}

pub fn parse(text: &str) -> Result<Members> {
    let mut members = Members::default();
    for line in text.lines().filter(|l| !l.trim().is_empty()) {
        let (map, entry) = match line.strip_prefix(REMOVED) {
            Some(rest) => (&mut members.removed, rest),
            None => (&mut members.listed, line),
        };
        let (id, name) = entry.split_once(' ').unwrap_or((entry, ""));
        let id = id
            .parse()
            .with_context(|| format!("bad member line: {line}"))?;
        map.insert(id, name.trim().to_owned());
    }
    Ok(members)
}

pub fn format(members: &Members) -> String {
    let listed = members
        .listed
        .iter()
        .map(|(id, name)| format!("{id} {name}\n"));
    let removed = members
        .removed
        .iter()
        .map(|(id, name)| format!("{REMOVED}{id} {name}\n"));
    listed.chain(removed).collect()
}

/// The member list in `rev`'s tree, if it has one.
pub fn at(git: &Git, rev: &str) -> Result<Option<Members>> {
    match git.run(["cat-file", "blob", &format!("{rev}:{MEMBERS_PATH}")]) {
        Ok(text) => parse(&text).map(Some),
        Err(_) => Ok(None),
    }
}

fn payload(tree: &str, parents: &[&str]) -> Vec<u8> {
    format!("vaultsync-commit-v1\n{tree}\n{}\n", parents.join(" ")).into_bytes()
}

/// The trailer line signing a commit of `tree` on `parents`.
pub fn sign(key: &SecretKey, tree: &str, parents: &[&str]) -> String {
    let sig = key.sign(&payload(tree, parents));
    format!(
        "{TRAILER} {} {}",
        key.public(),
        data_encoding::HEXLOWER.encode(&sig.to_bytes())
    )
}

/// The node that validly signed `commit`, if any.
fn signer(git: &Git, commit: &str) -> Result<Option<EndpointId>> {
    let raw = git.run(["cat-file", "commit", commit])?;
    let (header, message) = raw.split_once("\n\n").unwrap_or((&raw, ""));
    let field = |name: &'static str| {
        header
            .lines()
            .filter_map(move |l| l.strip_prefix(name)?.strip_prefix(' '))
    };
    let tree = field("tree").next().context("commit without tree")?;
    let parents: Vec<&str> = field("parent").collect();
    let Some(value) = message.lines().rev().find_map(|l| l.strip_prefix(TRAILER)) else {
        return Ok(None);
    };
    let mut words = value.split_whitespace();
    let (Some(id), Some(sig)) = (words.next(), words.next()) else {
        return Ok(None);
    };
    let Ok(id) = id.parse::<EndpointId>() else {
        return Ok(None);
    };
    let Some(sig) = data_encoding::HEXLOWER
        .decode(sig.as_bytes())
        .ok()
        .and_then(|b| <[u8; 64]>::try_from(b).ok())
    else {
        return Ok(None);
    };
    Ok(id
        .verify(&payload(tree, &parents), &Signature::from_bytes(&sig))
        .is_ok()
        .then_some(id))
}

/// Checks every commit reachable from `tip` but not from `head`. `trusted` is
/// the local member list, which vouches for a list appearing where a parent
/// had none.
pub fn validate(git: &Git, head: &str, tip: &str, trusted: &Members) -> Result<()> {
    let graph = git.run(["rev-list", "--parents", tip, "--not", head])?;
    let mut lists: HashMap<String, Option<Members>> = HashMap::new();
    let mut at_cached = |rev: &str| -> Result<Option<Members>> {
        if !lists.contains_key(rev) {
            lists.insert(rev.to_owned(), at(git, rev)?);
        }
        Ok(lists[rev].clone())
    };
    for line in graph.lines() {
        let line: Vec<&str> = line.split(' ').collect();
        let (commit, parents) = (line[0], &line[1..]);
        let ours = at_cached(commit)?;
        if parents.len() > 1 {
            let mut member_era = false;
            for p in parents {
                member_era |= at_cached(p)?.is_some();
            }
            // Merges are deterministic: whoever merges these parents makes this
            // exact commit, so one that differs was forged.
            if member_era && (parents.len() != 2 || merge(git, parents[0], parents[1])? != commit) {
                bail!("merge {commit} is not the merge of its parents");
            }
            continue;
        }
        let signed_by = signer(git, commit)?;
        match parents.first().map(|p| at_cached(p)).transpose()?.flatten() {
            // History from before the vault had a member list.
            None => {
                if ours.is_some() && !signed_by.is_some_and(|id| trusted.contains(&id)) {
                    bail!("commit {commit} starts a member list without a trusted signature");
                }
            }
            Some(before) => {
                if !signed_by.is_some_and(|id| before.contains(&id)) {
                    bail!("commit {commit} is not signed by a member");
                }
                let Some(after) = ours else {
                    bail!("commit {commit} deletes the member list");
                };
                if !before
                    .removed
                    .keys()
                    .all(|id| after.removed.contains_key(id))
                {
                    bail!("commit {commit} brings back a removed member");
                }
            }
        }
    }
    Ok(())
}
