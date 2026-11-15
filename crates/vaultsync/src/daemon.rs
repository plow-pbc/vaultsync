//! `vaultsync run`: commit edits as they happen and keep in step with every
//! member that can be reached.

use std::path::PathBuf;
use std::time::Duration;

use anyhow::Result;
use iroh::EndpointId;
use notify_debouncer_mini::{DebounceEventResult, new_debouncer, notify::RecursiveMode};
use tokio::sync::mpsc;
use tokio::task::spawn_blocking;

use crate::net::{Event, Net};
use crate::vault::Vault;

const DEBOUNCE: Duration = Duration::from_millis(500);

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
    edited(&vault, &net).await;
    eprintln!(
        "watching {} as node {}",
        vault.folder().display(),
        vault.node_id()?
    );
    while let Some(event) = rx.recv().await {
        match event {
            Event::Edited => edited(&vault, &net).await,
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

async fn edited(vault: &Vault, net: &Net) {
    match blocking(vault, Vault::commit_pending).await {
        Ok(Some(id)) => {
            eprintln!("committed {id}");
            poke_all(vault, net).await;
        }
        Ok(None) => {}
        Err(e) => eprintln!("commit failed, will retry on the next edit: {e:#}"),
    }
}

async fn sync_with(vault: &Vault, net: &Net, peer: EndpointId) {
    if let Err(e) = net.fetch(vault, peer).await {
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
