//! The vault's member list (`.vault/members`) and the signatures that make
//! changes to it count.
//!
//! Every commit this tool makes carries `Vault-Signature: <node id> <sig>`, an
//! ed25519 signature by the machine's node key over the commit's tree and
//! parents. A commit arriving from a peer that changes the member list is
//! accepted only if a member at its parent signed it; a merge may only combine
//! its parents' lists.

use std::collections::{BTreeMap, BTreeSet, HashMap};

use anyhow::{Context, Result, bail};
use iroh::{EndpointId, SecretKey, Signature};

use crate::git::Git;

pub const MEMBERS_PATH: &str = ".vault/members";
const TRAILER: &str = "Vault-Signature:";

/// Node id → machine name.
pub type Members = BTreeMap<EndpointId, String>;

pub fn parse(text: &str) -> Result<Members> {
    text.lines()
        .filter(|l| !l.trim().is_empty())
        .map(|line| {
            let (id, name) = line.split_once(' ').unwrap_or((line, ""));
            let id = id
                .parse()
                .with_context(|| format!("bad member line: {line}"))?;
            Ok((id, name.trim().to_owned()))
        })
        .collect()
}

pub fn format(members: &Members) -> String {
    members
        .iter()
        .map(|(id, name)| format!("{id} {name}\n"))
        .collect()
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
    let commits: Vec<Vec<&str>> = graph.lines().map(|l| l.split(' ').collect()).collect();
    // Blob id of each commit's member list, in one batch.
    let wanted: BTreeSet<&str> = commits.iter().flatten().copied().collect();
    let query: String = wanted
        .iter()
        .map(|c| format!("{c}:{MEMBERS_PATH}\n"))
        .collect();
    let answers = git.run_stdin(["cat-file", "--batch-check=%(objectname)"], &query)?;
    let blob: HashMap<&str, Option<&str>> = wanted
        .iter()
        .zip(answers.lines())
        .map(|(c, a)| (*c, (!a.ends_with(" missing")).then_some(a)))
        .collect();

    for line in &commits {
        let (commit, parents) = (line[0], &line[1..]);
        if parents.iter().any(|p| blob[p] == blob[commit])
            || (parents.is_empty() && blob[commit].is_none())
        {
            continue;
        }
        let ours = at(git, commit)?.unwrap_or_default();
        if parents.len() > 1 {
            let mut union = Members::new();
            for p in parents {
                union.extend(at(git, p)?.unwrap_or_default());
            }
            if ours.keys().all(|id| union.contains_key(id)) {
                continue;
            }
            bail!("merge {commit} adds members neither parent had");
        }
        let allowed = match parents.first() {
            Some(p) => at(git, p)?.unwrap_or_else(|| trusted.clone()),
            None => trusted.clone(),
        };
        match signer(git, commit)? {
            Some(id) if allowed.contains_key(&id) => {}
            _ => bail!("commit {commit} changes the member list without a member's signature"),
        }
    }
    Ok(())
}
