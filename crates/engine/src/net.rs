//! The wire between members: one iroh connection per request, one stream.
//!
//! Client → server: `VS1`, a kind byte, then for a fetch the commit ids the
//! client has (one per line). Server → client: a status byte, then for a
//! fetch the head's id on a line and a git pack of what the client lacks. Peers only ever fetch; no peer
//! writes into another's repository. A connection from a node that is not on
//! the member list is closed before it can ask anything.

use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use iroh::endpoint::{Connection, ConnectionError, IncomingAddr, QuicTransportConfig, presets};
use iroh::{Endpoint, EndpointAddr, EndpointId, RelayMode, SecretKey};
use tokio::io::AsyncWriteExt;
use tokio::sync::mpsc::UnboundedSender;
use tokio::task::spawn_blocking;
use tokio::time::timeout;

use crate::vault::Vault;

const ALPN: &[u8] = b"vaultsync/0";
const MAGIC: &[u8] = b"VS1";
/// What the first release, which sent git bundles, opens a request with.
const OLD_MAGIC: &[u8] = b"VS0";
const FETCH: u8 = 1;
const POKE: u8 = 2;
/// The one request a non-member may make: a joining machine with an invite.
const JOIN: u8 = 3;
const PACK_FOLLOWS: u8 = 0;
const NOTHING_NEW: u8 = 1;
const REFUSED: u8 = 2;
const DIAL_TIMEOUT: Duration = Duration::from_secs(15);
const REQUEST_TIMEOUT: Duration = Duration::from_secs(10);
/// Longest a fetch's transfer may take.
const FETCH_TIMEOUT: Duration = Duration::from_secs(300);
/// How long a silent connection is believed: two of iroh's 5 s keep-alives.
/// A restarted peer is unreachable while a connection to its dead instance
/// lingers, since iroh sends new dials down that connection's path.
const IDLE_TIMEOUT: Duration = Duration::from_secs(10);
const UNIDENTIFIED_REPORT_EVERY: Duration = Duration::from_secs(600);

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
        let transport = QuicTransportConfig::builder()
            .max_idle_timeout(Some(IDLE_TIMEOUT.try_into()?))
            .build();
        let endpoint = match addr_dir {
            None => {
                let endpoint = Endpoint::builder(presets::N0)
                    .transport_config(transport)
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
                    .transport_config(transport)
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
                // The first release drops a request it can't read, unanswered.
                Some(ConnectionError::ApplicationClosed(close)) if close.reason.is_empty() => {
                    anyhow::anyhow!(
                        "{} closed the connection without answering; it may run an older vaultsync: upgrade it",
                        peer.fmt_short()
                    )
                }
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

    /// Fetches `peer`'s head into this vault's copy of it.
    pub async fn fetch(&self, vault: &Vault, peer: EndpointId) -> Result<()> {
        let v = vault.clone();
        let haves = spawn_blocking(move || v.haves()).await??;
        let path = vault.incoming(&peer)?;
        // Only the transfer can time out: the import below always finishes,
        // since cancelling it would leave it running against the next fetch.
        let arrived = timeout(FETCH_TIMEOUT, async {
            let (conn, mut recv, status) = self
                .request(peer, FETCH, haves.join("\n").as_bytes())
                .await?;
            if status == PACK_FOLLOWS {
                let mut file = tokio::fs::File::create(&path).await?;
                tokio::io::copy(&mut recv, &mut file).await?;
                file.flush().await?;
            }
            conn.close(0u32.into(), b"done");
            anyhow::Ok(status == PACK_FOLLOWS)
        })
        .await
        .context("fetch timed out")
        .and_then(|r| r)
        .inspect_err(|_| {
            let _ = std::fs::remove_file(&path);
        })?;
        if !arrived {
            return Ok(());
        }
        let v = vault.clone();
        spawn_blocking(move || v.receive(&peer, &path)).await?
    }

    /// Presents an invite secret to `inviter` and returns the pack of the
    /// vault's history it sends back.
    pub async fn join(
        &self,
        vault: &Vault,
        inviter: EndpointId,
        secret: &[u8; 32],
    ) -> Result<PathBuf> {
        let mut body = secret.to_vec();
        body.extend_from_slice(vault.machine_name()?.as_bytes());
        let (conn, mut recv, status) = self
            .request(inviter, JOIN, &body)
            .await
            .context("could not reach the inviting machine (is `vaultsync run` running there?)")?;
        if status != PACK_FOLLOWS {
            let reason = recv.read_to_end(1024).await.unwrap_or_default();
            conn.close(0u32.into(), b"done");
            bail!("{}", String::from_utf8_lossy(&reason));
        }
        let path = vault.incoming(&inviter)?;
        let mut file = tokio::fs::File::create(&path).await?;
        tokio::io::copy(&mut recv, &mut file).await?;
        file.flush().await?;
        conn.close(0u32.into(), b"done");
        Ok(path)
    }

    /// Answers peers until the endpoint closes.
    pub async fn serve(self, vault: Vault, events: UnboundedSender<Event>) {
        while let Some(incoming) = self.endpoint.accept().await {
            let (vault, events) = (vault.clone(), events.clone());
            tokio::spawn(async move {
                let from = incoming.remote_addr();
                let conn = match incoming.await {
                    Ok(conn) => conn,
                    Err(e) => return unidentified(&from, &e),
                };
                if let Err(e) = answer(conn, vault, events).await {
                    eprintln!("request failed: {e:#}");
                }
            });
        }
    }
}

/// Notes a connection attempt that failed before the sender was identified:
/// any address on the internet can send a damaged or non-vaultsync first
/// packet, so these are summed and reported at most every
/// `UNIDENTIFIED_REPORT_EVERY`, naming the latest sender.
fn unidentified(from: &IncomingAddr, e: &dyn std::fmt::Display) {
    static SINCE_REPORT: Mutex<(Option<Instant>, u32)> = Mutex::new((None, 0));
    let mut since = SINCE_REPORT.lock().unwrap();
    since.1 += 1;
    if since
        .0
        .is_some_and(|t| t.elapsed() < UNIDENTIFIED_REPORT_EVERY)
    {
        return;
    }
    let from = match from {
        IncomingAddr::Ip(addr) => addr.to_string(),
        IncomingAddr::Relay { url, endpoint_id } => {
            format!("{} via relay {url}", endpoint_id.fmt_short())
        }
        other => format!("{other:?}"),
    };
    eprintln!(
        "dropped {} connection attempt(s) that failed before identifying themselves; latest from {from}: {e:#}",
        since.1
    );
    *since = (Some(Instant::now()), 0);
}

async fn answer(conn: Connection, vault: Vault, events: UnboundedSender<Event>) -> Result<()> {
    let peer = conn.remote_id();
    if crate::vault::hooked("stall-serving") {
        eprintln!("test hook: stall-serving");
        std::future::pending::<()>().await;
    }
    let (mut send, mut recv) = conn.accept_bi().await?;
    // Only the header before checking who is asking; a joiner's body is small.
    let mut header = [0u8; 4];
    timeout(REQUEST_TIMEOUT, recv.read_exact(&mut header)).await??;
    let kind = header[3];
    if &header[..3] == OLD_MAGIC {
        eprintln!(
            "refused {}: it runs an older vaultsync; upgrade it",
            peer.fmt_short()
        );
        conn.close(
            1u32.into(),
            b"this machine runs a newer vaultsync: upgrade vaultsync here to keep syncing",
        );
        return Ok(());
    }
    if &header[..3] != MAGIC {
        bail!("{} sent a malformed request", peer.fmt_short());
    }
    let v = vault.clone();
    if kind != JOIN && !spawn_blocking(move || v.members()).await??.contains(&peer) {
        eprintln!("refused connection from {}: not a member", peer.fmt_short());
        conn.close(1u32.into(), b"not a member");
        return Ok(());
    }
    let limit = if kind == JOIN { 1024 } else { 1 << 20 };
    let body = timeout(REQUEST_TIMEOUT, recv.read_to_end(limit)).await??;
    let body = body.as_slice();
    match kind {
        POKE => {
            let _ = events.send(Event::Poke(peer));
            send.write_all(&[NOTHING_NEW]).await?;
        }
        FETCH => {
            let haves: Vec<String> = String::from_utf8_lossy(body)
                .lines()
                .map(str::to_owned)
                .collect();
            pause_if_hooked("pause-before-snapshot").await;
            let v = vault.clone();
            // One snapshot: membership is judged, and the pack cut, at the
            // same commit, so a removal can't land between the two.
            let (head, present, news) = spawn_blocking(move || {
                let head = v.head()?;
                if !v.members_at(&head)?.contains(&peer) {
                    bail!("{} is no longer a member", peer.fmt_short());
                }
                let present = v.present(&haves)?;
                let news = v.has_news(&head, &present)?;
                anyhow::Ok((head, present, news))
            })
            .await??;
            pause_if_hooked("pause-after-snapshot").await;
            if news {
                send_pack(&vault, &head, &present, &mut send).await?;
            } else {
                send.write_all(&[NOTHING_NEW]).await?;
            }
        }
        JOIN if body.len() > 32 => {
            let secret: [u8; 32] = body[..32].try_into().expect("32 bytes");
            // One short line: the name goes into the member list as is.
            let name: String = String::from_utf8_lossy(&body[32..])
                .lines()
                .next()
                .unwrap_or_default()
                .trim()
                .chars()
                .take(64)
                .collect();
            let name = if name.is_empty() {
                "unnamed".to_owned()
            } else {
                name
            };
            let (v, n) = (vault.clone(), name.clone());
            let admitted = spawn_blocking(move || {
                v.consume_invite(&secret)?;
                v.resolve(&v.add_member(peer, &n)?)
            })
            .await?;
            match admitted {
                // The newcomer gets the vault as of its admission, nothing later.
                Ok(admission) => {
                    eprintln!("{name} ({}) joined", peer.fmt_short());
                    // Tell the other members about the newcomer.
                    let _ = events.send(Event::Edited);
                    send_pack(&vault, &admission, &[], &mut send).await?;
                }
                Err(e) => {
                    eprintln!("refused join from {}: {e:#}", peer.fmt_short());
                    send.write_all(&[REFUSED]).await?;
                    send.write_all(format!("{e:#}").as_bytes()).await?;
                }
            }
        }
        _ => bail!("{} sent an unknown request", peer.fmt_short()),
    }
    send.finish()?;
    // Let the client close once it has everything.
    let _ = timeout(Duration::from_secs(30), conn.closed()).await;
    Ok(())
}

/// Sends `head` and a pack of what it holds beyond the peer's `haves`.
async fn send_pack(
    vault: &Vault,
    head: &str,
    haves: &[String],
    send: &mut noq::SendStream,
) -> Result<()> {
    let (v, head, haves) = (vault.clone(), head.to_owned(), haves.to_vec());
    let pack = spawn_blocking(move || v.pack(&head, &haves)).await??;
    send.write_all(&[PACK_FOLLOWS]).await?;
    send.write_all(&pack).await?;
    Ok(())
}

/// Test hook: hold a request at a named point for 5 s.
async fn pause_if_hooked(point: &str) {
    if crate::vault::hooked(point) {
        eprintln!("test hook: {point}");
        tokio::time::sleep(Duration::from_secs(5)).await;
    }
}
