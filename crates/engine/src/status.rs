//! `vaultsync status --json`: what a front end shows for a vault.

use std::collections::BTreeMap;
use std::fs;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use anyhow::Result;
use serde_json::{Value, json};

use crate::vault::Vault;

/// A peer heard from within this long is online; so is a daemon whose state
/// file is this fresh.
const ONLINE: Duration = Duration::from_secs(90);

pub fn now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("after 1970")
        .as_secs()
}

/// What the daemon records: when it last synced, and when it last heard from
/// each peer (unix seconds, by node id).
#[derive(Default)]
pub struct Seen {
    pub last_sync: Option<u64>,
    pub peers: BTreeMap<String, u64>,
}

impl Seen {
    pub fn load(vault: &Vault) -> Result<(Seen, Option<u64>)> {
        let Ok(text) = fs::read_to_string(vault.daemon_state()?) else {
            return Ok((Seen::default(), None));
        };
        let v: Value = serde_json::from_str(&text)?;
        let seen = Seen {
            last_sync: v["last_sync"].as_u64(),
            peers: serde_json::from_value(v["peers"].clone())?,
        };
        Ok((seen, v["updated"].as_u64()))
    }

    /// Written whole and renamed into place, so a reader never sees half.
    pub fn save(&self, vault: &Vault) -> Result<()> {
        let path = vault.daemon_state()?;
        let tmp = path.with_extension("json.tmp");
        let body = json!({"updated": now(), "last_sync": self.last_sync, "peers": self.peers});
        fs::write(&tmp, serde_json::to_vec(&body)?)?;
        fs::rename(tmp, path)?;
        Ok(())
    }
}

pub fn json(vault: &Vault) -> Result<Value> {
    let (seen, updated) = Seen::load(vault)?;
    let fresh = |t: u64| now().saturating_sub(t) <= ONLINE.as_secs();
    let me = vault.node_id()?;
    let peers: Vec<Value> = vault
        .members()?
        .active()
        .filter(|(id, _)| **id != me)
        .map(|(id, name)| {
            let last_seen = seen.peers.get(&id.to_string()).copied();
            json!({
                "id": id.to_string(),
                "name": name,
                "online": last_seen.is_some_and(fresh),
                "last_seen": last_seen,
            })
        })
        .collect();
    Ok(json!({
        "folder": vault.folder(),
        "machine": vault.machine_name()?,
        "node": me.to_string(),
        "head": vault.head()?,
        "pending": vault.pending()?,
        "running": updated.is_some_and(fresh),
        // False until the engine has first run this vault (just added).
        "started": updated.is_some(),
        "last_sync": seen.last_sync,
        "peers": peers,
        "conflicts": vault.conflicts()?,
    }))
}
