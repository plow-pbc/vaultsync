//! The vault's member list (`.vault/members`) and the signatures that make
//! commits count.
//!
//! Every commit this tool makes carries `Vault-Signature: <node id> <sig>`, an
//! ed25519 signature by the machine's node key over the commit's tree and
//! parents. Once a vault has a member list, a commit arriving from a peer is
//! accepted only if a member at its parent signed it, or it is exactly the
//! deterministic merge of its parents. A merge the earlier engine made can't
//! be recomputed; it counts only beneath a member's signed commit. A removed machine stays removed: its tombstone
//! (`removed <id> <name>`) outranks any line listing it, and never goes away.

use std::collections::{BTreeMap, HashMap, HashSet};

use anyhow::{Context, Result, bail};
use iroh::{EndpointId, SecretKey, Signature};

use git2::{Oid, Repository};

use crate::merge::{is_current, merge};

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
pub fn at(repo: &Repository, rev: &str) -> Result<Option<Members>> {
    let Ok(commit) = repo.revparse_single(rev).and_then(|o| o.peel_to_commit()) else {
        return Ok(None);
    };
    let Ok(entry) = commit.tree()?.get_path(std::path::Path::new(MEMBERS_PATH)) else {
        return Ok(None);
    };
    let Ok(blob) = repo.find_blob(entry.id()) else {
        return Ok(None);
    };
    parse(&String::from_utf8_lossy(blob.content())).map(Some)
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
fn signer(repo: &Repository, commit: Oid) -> Result<Option<EndpointId>> {
    let commit = repo.find_commit(commit)?;
    let tree = commit.tree_id().to_string();
    let parents: Vec<String> = commit.parent_ids().map(|p| p.to_string()).collect();
    let parents: Vec<&str> = parents.iter().map(String::as_str).collect();
    let message = String::from_utf8_lossy(commit.message_bytes());
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
        .verify(&payload(&tree, &parents), &Signature::from_bytes(&sig))
        .is_ok()
        .then_some(id))
}

/// Commits reachable from any of `from` but not from `head`.
fn reachable(repo: &Repository, from: &[Oid], head: Oid) -> Result<HashSet<Oid>> {
    if from.is_empty() {
        return Ok(HashSet::new());
    }
    let mut walk = repo.revwalk()?;
    for id in from {
        walk.push(*id)?;
    }
    walk.hide(head)?;
    Ok(walk.collect::<Result<_, _>>()?)
}

/// Checks every commit reachable from `tip` but not from `head`. `trusted` is
/// the local member list, which vouches for a list appearing where a parent
/// had none.
pub fn validate(repo: &Repository, head: Oid, tip: Oid, trusted: &Members) -> Result<()> {
    let mut walk = repo.revwalk()?;
    walk.push(tip)?;
    walk.hide(head)?;
    let mut commits: Vec<(Oid, Vec<Oid>)> = vec![];
    for id in walk {
        let id = id?;
        commits.push((id, repo.find_commit(id)?.parent_ids().collect()));
    }
    let mut lists: HashMap<Oid, Option<Members>> = HashMap::new();
    let mut at_cached = |id: Oid| -> Result<Option<Members>> {
        if let Some(list) = lists.get(&id) {
            return Ok(list.clone());
        }
        let list = at(repo, &id.to_string())?;
        lists.insert(id, list.clone());
        Ok(list)
    };
    let mut signers = HashMap::new();
    for (commit, _) in &commits {
        signers.insert(*commit, signer(repo, *commit)?);
    }
    // A trusted member's signed commit that starts the member list vouches
    // for the history beneath it: an imported vault's unsigned past.
    let mut introductions = Vec::new();
    for (commit, parents) in &commits {
        let starts_list = parents.len() <= 1
            && at_cached(*commit)?.is_some()
            && parents
                .first()
                .map(|p| at_cached(*p))
                .transpose()?
                .flatten()
                .is_none();
        if starts_list && signers[commit].is_some_and(|id| trusted.contains(&id)) {
            introductions.push(*commit);
        }
    }
    let vouched = reachable(repo, &introductions, head)?;
    // Every signed commit must pass as a member's below, or nothing is
    // accepted; so what lies beneath one was taken in by a member.
    let signed: Vec<Oid> = signers
        .iter()
        .filter_map(|(id, s)| s.map(|_| *id))
        .collect();
    let beneath_signed = reachable(repo, &signed, head)?;
    for (commit, parents) in &commits {
        let commit = *commit;
        let ours = at_cached(commit)?;
        if parents.len() > 1 {
            let mut member_era = ours.is_some();
            for p in parents {
                member_era |= at_cached(*p)?.is_some();
            }
            if !member_era {
                continue;
            }
            // Merges are deterministic: whoever merges these parents makes this
            // exact commit, so one that differs (or invents a member list) was
            // forged.
            if is_current(repo.find_commit(commit)?.message_bytes()) {
                if parents.len() != 2 || merge(repo, parents[0], parents[1])? != commit {
                    bail!("merge {commit} is not the merge of its parents");
                }
            } else if !beneath_signed.contains(&commit) {
                bail!(
                    "merge {commit} is from an earlier vaultsync and no member signed on top of it"
                );
            }
            continue;
        }
        let signed_by = signers[&commit];
        match parents
            .first()
            .map(|p| at_cached(*p))
            .transpose()?
            .flatten()
        {
            // No member list at its parent: a root, or history from before the
            // vault had one. Only a current member may bring that in, by signing
            // it or the commit that starts the list on top of it; a removed
            // machine could otherwise start a fresh, unsigned line of history.
            None => {
                if !signed_by.is_some_and(|id| trusted.contains(&id)) && !vouched.contains(&commit)
                {
                    bail!(
                        "commit {commit} has no member list behind it and no current member's signature"
                    );
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
