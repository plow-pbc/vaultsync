//! The wire between members: one iroh connection per request, one stream.
//!
//! Client → server: `VS0`, a kind byte, then for a fetch the commit ids the
//! client has (one per line). Server → client: a status byte, then for a
//! fetch a git bundle of what the client lacks. Peers only ever fetch; no peer
//! writes into another's repository. A connection from a node that is not on
//! the member list is closed before it can ask anything.

use std::net::SocketAddr;
use std::path::Path;
use std::process::Stdio;
use std::time::Duration;

use anyhow::{Context, Result, bail};
use iroh::endpoint::{Connection, Incoming, presets};
use iroh::{Endpoint, EndpointAddr, EndpointId, RelayMode, SecretKey};
use tokio::io::AsyncWriteExt;
use tokio::sync::mpsc::UnboundedSender;
use tokio::task::spawn_blocking;
use tokio::time::timeout;

use crate::vault::Vault;

const ALPN: &[u8] = b"vaultsync/0";
const MAGIC: &[u8] = b"VS0";
const FETCH: u8 = 1;
const POKE: u8 = 2;
const BUNDLE_FOLLOWS: u8 = 0;
const NOTHING_NEW: u8 = 1;
const DIAL_TIMEOUT: Duration = Duration::from_secs(15);
const REQUEST_TIMEOUT: Duration = Duration::from_secs(10);
/// Longest a fetch's transfer may take.
const FETCH_TIMEOUT: Duration = Duration::from_secs(300);

/// What the daemon reacts to.
pub enum Event {
    Edited,
    /// A peer has new commits.
    Poke(EndpointId),
    CatchUp,
    /// A fetch from this peer finished.
    Fetched(EndpointId, Result<()>),
}

/// How peers find each other: n0's relays and address lookup, or (for tests
/// and offline use) address files in a shared directory on this machine.
#[derive(Clone)]
pub struct Net {
    pub endpoint: Endpoint,
    addr_dir: Option<std::path::PathBuf>,
}

impl Net {
    pub async fn bind(key: SecretKey, addr_dir: Option<&Path>) -> Result<Net> {
        let endpoint = match addr_dir {
            None => {
                let endpoint = Endpoint::builder(presets::N0)
                    .secret_key(key)
                    .alpns(vec![ALPN.to_vec()])
                    .bind()
                    .await?;
                if timeout(Duration::from_secs(5), endpoint.online())
                    .await
                    .is_err()
                {
                    eprintln!("no relay reachable yet; only direct connections will work");
                }
                endpoint
            }
            Some(dir) => {
                let endpoint = Endpoint::builder(presets::Minimal)
                    .secret_key(key)
                    .alpns(vec![ALPN.to_vec()])
                    .relay_mode(RelayMode::Disabled)
                    .bind_addr("127.0.0.1:0")?
                    .bind()
                    .await?;
                let addrs: String = endpoint
                    .bound_sockets()
                    .iter()
                    .map(|a| format!("{a}\n"))
                    .collect();
                std::fs::write(dir.join(endpoint.id().to_string()), addrs)?;
                endpoint
            }
        };
        Ok(Net {
            endpoint,
            addr_dir: addr_dir.map(Path::to_owned),
        })
    }

    async fn dial(&self, peer: EndpointId) -> Result<Connection> {
        let mut addr = EndpointAddr::new(peer);
        if let Some(dir) = &self.addr_dir {
            let text = std::fs::read_to_string(dir.join(peer.to_string())).unwrap_or_default();
            for line in text.lines() {
                addr = addr.with_ip_addr(line.parse::<SocketAddr>()?);
            }
        }
        timeout(DIAL_TIMEOUT, self.endpoint.connect(addr, ALPN))
            .await
            .context("timed out")?
            .map_err(Into::into)
    }

    /// Sends one request and returns the stream to read the reply from.
    async fn request(
        &self,
        peer: EndpointId,
        kind: u8,
        body: &[u8],
    ) -> Result<(Connection, noq::RecvStream, u8)> {
        let conn = self.dial(peer).await?;
        let (mut send, mut recv) = conn.open_bi().await?;
        send.write_all(MAGIC).await?;
        send.write_all(&[kind]).await?;
        send.write_all(body).await?;
        send.finish()?;
        let mut status = [0u8];
        recv.read_exact(&mut status)
            .await
            .map_err(|e| match conn.close_reason() {
                Some(reason) => anyhow::anyhow!("{reason}"),
                None => e.into(),
            })?;
        Ok((conn, recv, status[0]))
    }

    /// Tells `peer` this machine has new commits for it to fetch.
    pub async fn poke(&self, peer: EndpointId) -> Result<()> {
        let (conn, _, _) = self.request(peer, POKE, b"").await?;
        conn.close(0u32.into(), b"done");
        Ok(())
    }

    /// Fetches `peer`'s head into this vault's copy of it. True if it moved.
    pub async fn fetch(&self, vault: &Vault, peer: EndpointId) -> Result<bool> {
        let v = vault.clone();
        let haves = spawn_blocking(move || v.haves()).await??;
        let path = vault.incoming(&peer)?;
        // Only the transfer can time out: the import below always finishes,
        // since cancelling it would leave it running against the next fetch.
        let arrived = timeout(FETCH_TIMEOUT, async {
            let (conn, mut recv, status) = self
                .request(peer, FETCH, haves.join("\n").as_bytes())
                .await?;
            if status == BUNDLE_FOLLOWS {
                let mut file = tokio::fs::File::create(&path).await?;
                tokio::io::copy(&mut recv, &mut file).await?;
                file.flush().await?;
            }
            conn.close(0u32.into(), b"done");
            anyhow::Ok(status == BUNDLE_FOLLOWS)
        })
        .await
        .context("fetch timed out")
        .and_then(|r| r)
        .inspect_err(|_| {
            let _ = std::fs::remove_file(&path);
        })?;
        if !arrived {
            return Ok(false);
        }
        let v = vault.clone();
        spawn_blocking(move || v.receive(&peer, &path)).await?
    }

    /// Answers peers until the endpoint closes.
    pub async fn serve(self, vault: Vault, events: UnboundedSender<Event>) {
        while let Some(incoming) = self.endpoint.accept().await {
            let (vault, events) = (vault.clone(), events.clone());
            tokio::spawn(async move {
                if let Err(e) = answer(incoming, vault, events).await {
                    eprintln!("request failed: {e:#}");
                }
            });
        }
    }
}

async fn answer(incoming: Incoming, vault: Vault, events: UnboundedSender<Event>) -> Result<()> {
    let conn = incoming.await?;
    let peer = conn.remote_id();
    let v = vault.clone();
    if !spawn_blocking(move || v.members())
        .await??
        .contains_key(&peer)
    {
        eprintln!("refused connection from {}: not a member", peer.fmt_short());
        conn.close(1u32.into(), b"not a member");
        return Ok(());
    }
    if crate::vault::hooked("stall-serving") {
        std::future::pending::<()>().await;
    }
    let (mut send, mut recv) = conn.accept_bi().await?;
    let request = timeout(REQUEST_TIMEOUT, recv.read_to_end(1 << 20)).await??;
    let Some(rest) = request.strip_prefix(MAGIC) else {
        bail!("{} sent a malformed request", peer.fmt_short());
    };
    match rest.split_first() {
        Some((&POKE, _)) => {
            let _ = events.send(Event::Poke(peer));
            send.write_all(&[NOTHING_NEW]).await?;
        }
        Some((&FETCH, body)) => {
            let haves: Vec<String> = String::from_utf8_lossy(body)
                .lines()
                .map(str::to_owned)
                .collect();
            let v = vault.clone();
            let (present, news) = spawn_blocking(move || {
                let present = v.present(&haves)?;
                let news = v.has_news(&present)?;
                anyhow::Ok((present, news))
            })
            .await??;
            if news {
                send.write_all(&[BUNDLE_FOLLOWS]).await?;
                let mut child = tokio::process::Command::from(vault.bundle_command(&present))
                    .stdout(Stdio::piped())
                    .kill_on_drop(true)
                    .spawn()?;
                tokio::io::copy(child.stdout.as_mut().expect("piped"), &mut send).await?;
                if !child.wait().await?.success() {
                    bail!("git bundle failed");
                }
            } else {
                send.write_all(&[NOTHING_NEW]).await?;
            }
        }
        _ => bail!("{} sent an unknown request", peer.fmt_short()),
    }
    send.finish()?;
    // Let the client close once it has everything.
    let _ = timeout(Duration::from_secs(30), conn.closed()).await;
    Ok(())
}
