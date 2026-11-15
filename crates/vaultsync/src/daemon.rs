//! `vaultsync run`: commit edits as they happen and keep in step with every
//! member that can be reached.

use std::path::PathBuf;
use std::time::Duration;

use anyhow::Result;
use iroh::EndpointId;
use notify_debouncer_mini::{DebounceEventResult, new_debouncer, notify::RecursiveMode};
use tokio::sync::mpsc;
use tokio::task::spawn_blocking;
use tokio::time::Instant;

use crate::net::{Event, Net};
use crate::vault::Vault;

const DEBOUNCE: Duration = Duration::from_millis(500);
/// Longest a fetch may take before the daemon moves on.
const FETCH_TIMEOUT: Duration = Duration::from_secs(300);

pub struct Opts {
    pub catch_up: Duration,
    pub addr_dir: Option<PathBuf>,
}

pub fn run(vault: Vault, opts: Opts) -> Result<()> {
    tokio::runtime::Runtime::new()?.block_on(daemon(vault, opts))
}

async fn daemon(vault: Vault, opts: Opts) -> Result<()> {
    let net = Net::bind(vault.secret_key()?, opts.addr_dir.as_deref()).await?;
    let (events, mut rx) = mpsc::unbounded_channel();

    let edits = events.clone();
    let mut debouncer = new_debouncer(DEBOUNCE, move |res: DebounceEventResult| match res {
        Ok(_) => {
            let _ = edits.send(Event::Edited);
        }
        Err(e) => eprintln!("watch error: {e}"),
    })?;
    debouncer
        .watcher()
        .watch(vault.folder(), RecursiveMode::Recursive)?;
    tokio::spawn(net.clone().serve(vault.clone(), events.clone()));
    let ticks = events.clone();
    tokio::spawn(async move {
        let mut interval = tokio::time::interval(opts.catch_up);
        while ticks.send(Event::CatchUp).is_ok() {
            interval.tick().await;
        }
    });

    // Catch edits made while nothing was watching.
    let mut retry = Retry::default();
    edited(&vault, &net, &mut retry).await;
    eprintln!(
        "watching {} as node {}",
        vault.folder().display(),
        vault.node_id()?
    );
    loop {
        // A failed commit is tried again at its deadline, edits or not.
        let deadline = retry
            .at
            .unwrap_or_else(|| Instant::now() + Duration::from_secs(3600));
        let event = tokio::select! {
            event = rx.recv() => match event {
                Some(event) => event,
                None => break,
            },
            _ = tokio::time::sleep_until(deadline), if retry.at.is_some() => Event::Edited,
        };
        match event {
            Event::Edited => edited(&vault, &net, &mut retry).await,
            Event::Poke(peer) => sync_with(&vault, &net, peer).await,
            Event::CatchUp => {
                for peer in blocking(&vault, Vault::peers).await.unwrap_or_default() {
                    sync_with(&vault, &net, peer).await;
                }
            }
        }
    }
    Ok(())
}

async fn blocking<T: Send + 'static>(
    vault: &Vault,
    f: impl FnOnce(&Vault) -> Result<T> + Send + 'static,
) -> Result<T> {
    let vault = vault.clone();
    spawn_blocking(move || f(&vault)).await?
}

/// When to try a failed commit again, backing off 1 s doubling to a minute.
#[derive(Default)]
struct Retry {
    wait: Option<Duration>,
    at: Option<Instant>,
}

/// Commits pending edits; on failure sets the next retry deadline.
async fn edited(vault: &Vault, net: &Net, retry: &mut Retry) {
    match blocking(vault, Vault::commit_pending).await {
        Ok(id) => {
            *retry = Retry::default();
            if let Some(id) = id {
                eprintln!("committed {id}");
                poke_all(vault, net).await;
            }
        }
        Err(e) => {
            let wait = retry.wait.map_or(Duration::from_secs(1), |w| {
                (w * 2).min(Duration::from_secs(60))
            });
            *retry = Retry {
                wait: Some(wait),
                at: Some(Instant::now() + wait),
            };
            eprintln!("commit failed, retrying in {}s: {e:#}", wait.as_secs());
        }
    }
}

async fn sync_with(vault: &Vault, net: &Net, peer: EndpointId) {
    let fetched = tokio::time::timeout(FETCH_TIMEOUT, net.fetch(vault, peer)).await;
    if let Err(e) = fetched.map_err(anyhow::Error::from).and_then(|r| r) {
        eprintln!("could not fetch from {}: {e:#}", peer.fmt_short());
        return;
    }
    match blocking(vault, move |v| v.integrate(&peer)).await {
        Ok(Some(head)) => {
            eprintln!("synced {} from {}", &head[..7], peer.fmt_short());
            poke_all(vault, net).await;
        }
        Ok(None) => {}
        Err(e) => eprintln!("{e:#}"),
    }
}

async fn poke_all(vault: &Vault, net: &Net) {
    for peer in blocking(vault, Vault::peers).await.unwrap_or_default() {
        let net = net.clone();
        tokio::spawn(async move { net.poke(peer).await });
    }
}
