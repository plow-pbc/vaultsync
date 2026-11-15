//! Inviting a machine and joining with the code.
//!
//! The code is a long one-time secret plus where to find the inviter: its
//! node id and its vault folder. It rides after the `#` of a link, so a web
//! server never sees it. The joining machine dials the inviter directly; the
//! inviter consumes the invite, adds the newcomer to the member list and
//! sends it the whole history. No key ever leaves either machine.

use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{Context, Result, bail};
use data_encoding::BASE32_NOPAD;
use iroh::EndpointId;

use crate::net::Net;
use crate::vault::Vault;

const PREFIX: &str = "vs1";
const LINK: &str = "https://plow.co/sync#";

pub struct Invite {
    pub secret: [u8; 32],
    pub inviter: EndpointId,
    /// The inviter's folder, `~/`-relative when under its home.
    pub folder: String,
}

impl Invite {
    pub fn encode(&self) -> String {
        let mut bytes = self.secret.to_vec();
        bytes.extend_from_slice(self.inviter.as_bytes());
        bytes.extend_from_slice(self.folder.as_bytes());
        format!("{PREFIX}{}", BASE32_NOPAD.encode(&bytes).to_lowercase())
    }

    pub fn decode(code: &str) -> Result<Invite> {
        let code = code.trim().rsplit('#').next().unwrap_or_default();
        let body = code
            .strip_prefix(PREFIX)
            .context("not a vaultsync invite code")?;
        let bytes = BASE32_NOPAD
            .decode(body.to_uppercase().as_bytes())
            .context("invite code is damaged")?;
        if bytes.len() < 64 {
            bail!("invite code is too short");
        }
        Ok(Invite {
            secret: bytes[..32].try_into().expect("32 bytes"),
            inviter: EndpointId::from_bytes(bytes[32..64].try_into().expect("32 bytes"))?,
            folder: String::from_utf8(bytes[64..].to_vec())?,
        })
    }
}

fn home() -> Option<PathBuf> {
    std::env::var_os("HOME").map(PathBuf::from)
}

/// Prints a fresh invite: a link and the bare code.
pub fn invite(vault: &Vault, ttl: Duration) -> Result<String> {
    let folder = match home().and_then(|h| vault.folder().strip_prefix(h).ok().map(Path::to_owned))
    {
        Some(rel) => format!("~/{}", rel.display()),
        None => vault.folder().display().to_string(),
    };
    let code = Invite {
        secret: vault.create_invite(ttl)?,
        inviter: vault.node_id()?,
        folder,
    }
    .encode();
    Ok(format!(
        "Valid once, for {} minutes, while `vaultsync run` is running here.\n\
         Link: {LINK}{code}\n\
         Or on the new machine: vaultsync join {code}\n",
        ttl.as_secs().div_ceil(60)
    ))
}

/// Joins the vault behind `code` into `folder` (default: the inviter's).
pub fn join(
    code: &str,
    folder: Option<PathBuf>,
    machine: Option<String>,
    addr_dir: Option<PathBuf>,
) -> Result<Vault> {
    let invite = Invite::decode(code)?;
    let folder = folder.unwrap_or_else(|| match (invite.folder.strip_prefix("~/"), home()) {
        (Some(rel), Some(home)) => home.join(rel),
        _ => PathBuf::from(&invite.folder),
    });
    let vault = Vault::for_join(&folder, machine)?;
    let result = tokio::runtime::Runtime::new()?.block_on(async {
        let net = Net::bind(vault.secret_key()?, addr_dir.as_deref()).await?;
        net.join(&vault, invite.inviter, &invite.secret).await
    });
    match result.and_then(|bundle| vault.adopt(&bundle)) {
        Ok(()) => Ok(vault),
        Err(e) => {
            vault.discard()?;
            Err(e)
        }
    }
}
