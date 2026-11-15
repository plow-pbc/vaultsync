//! `vaultsync run`: commit edits as they happen and keep in step with every
//! member that can be reached.

use std::any::Any;
use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::Result;
use iroh::EndpointId;
use notify_debouncer_mini::notify::{self, PollWatcher, RecursiveMode};
use notify_debouncer_mini::{
    Config, DebounceEventHandler, DebounceEventResult, new_debouncer, new_debouncer_opt,
};
use tokio::sync::mpsc;
use tokio::task::spawn_blocking;
use tokio::time::Instant;

use crate::net::{Event, Net};
use crate::vault::Vault;

const DEBOUNCE: Duration = Duration::from_millis(500);
/// How often to look for changes when polling.
const POLL: Duration = Duration::from_secs(2);

pub struct Opts {
    pub catch_up: Duration,
    pub addr_dir: Option<PathBuf>,
    pub poll: bool,
}

pub fn run(vault: Vault, opts: Opts) -> Result<()> {
    tokio::runtime::Runtime::new()?.block_on(daemon(vault, opts))
}

async fn daemon(vault: Vault, opts: Opts) -> Result<()> {
    let net = Net::bind(vault.secret_key()?, opts.addr_dir.as_deref()).await?;
    let (events, mut rx) = mpsc::unbounded_channel();

    let edits = events.clone();
    let _watch = watch(
        vault.folder(),
        opts.poll,
        move |res: DebounceEventResult| match res {
            Ok(_) => {
                let _ = edits.send(Event::Edited);
            }
            Err(e) => eprintln!("watch error: {e}"),
        },
    )?;
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
    let mut fetches = Fetches::default();
    let mut announced = None;
    edited(&vault, &net, &mut retry, &mut announced).await;
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
            Event::Edited => edited(&vault, &net, &mut retry, &mut announced).await,
            Event::Poke(peer) => fetches.start(&vault, &net, &events, peer),
            Event::CatchUp => {
                for peer in blocking(&vault, Vault::peers).await.unwrap_or_default() {
                    fetches.start(&vault, &net, &events, peer);
                }
            }
            Event::Fetched(peer, result) => {
                match result {
                    Ok(()) => integrate(&vault, &net, peer, &mut announced).await,
                    Err(e) => eprintln!("could not fetch from {}: {e:#}", peer.fmt_short()),
                }
                fetches.finish(&vault, &net, &events, peer);
            }
        }
    }
    Ok(())
}

/// Watches `folder` (until the returned value is dropped) with the OS's notifications, or by polling when asked to
/// or when those are unavailable: a user can't raise an exhausted inotify
/// limit without root.
fn watch(
    folder: &Path,
    poll: bool,
    handler: impl DebounceEventHandler + Clone,
) -> Result<Box<dyn Any + Send>> {
    if !poll {
        let native = new_debouncer(DEBOUNCE, handler.clone()).and_then(|mut d| {
            d.watcher().watch(folder, RecursiveMode::Recursive)?;
            Ok(d)
        });
        match native {
            Ok(d) => return Ok(Box::new(d)),
            Err(e) => eprintln!(
                "file notifications unavailable ({e}); checking for changes every {} s instead",
                POLL.as_secs()
            ),
        }
    }
    let config = Config::default()
        .with_timeout(DEBOUNCE)
        .with_notify_config(notify::Config::default().with_poll_interval(POLL));
    let mut d = new_debouncer_opt::<_, PollWatcher>(config, handler)?;
    d.watcher().watch(folder, RecursiveMode::Recursive)?;
    Ok(Box::new(d))
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
async fn edited(vault: &Vault, net: &Net, retry: &mut Retry, announced: &mut Option<String>) {
    match blocking(vault, Vault::commit_pending).await {
        Ok(id) => {
            *retry = Retry::default();
            if let Some(id) = id {
                eprintln!("committed {id}");
            }
            // Also covers commits made outside the watcher: member changes,
            // rollbacks, a newcomer admitted.
            announce(vault, net, announced).await;
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

/// Fetches run side by side, so a slow peer holds up neither edits nor other
/// peers; each result comes back to the event loop, which integrates them one
/// at a time. One fetch per peer: a request meanwhile runs after it. A running
/// fetch is never cancelled, since its bundle import can't be.
#[derive(Default)]
struct Fetches {
    running: HashSet<EndpointId>,
    again: HashSet<EndpointId>,
}

impl Fetches {
    fn start(
        &mut self,
        vault: &Vault,
        net: &Net,
        events: &mpsc::UnboundedSender<Event>,
        peer: EndpointId,
    ) {
        if !self.running.insert(peer) {
            self.again.insert(peer);
            return;
        }
        let (vault, net, events) = (vault.clone(), net.clone(), events.clone());
        tokio::spawn(async move {
            let result = net.fetch(&vault, peer).await.map(|_| ());
            let _ = events.send(Event::Fetched(peer, result));
        });
    }

    fn finish(
        &mut self,
        vault: &Vault,
        net: &Net,
        events: &mpsc::UnboundedSender<Event>,
        peer: EndpointId,
    ) {
        self.running.remove(&peer);
        if self.again.remove(&peer) {
            self.start(vault, net, events, peer);
        }
    }
}

async fn integrate(vault: &Vault, net: &Net, peer: EndpointId, announced: &mut Option<String>) {
    match blocking(vault, move |v| v.integrate(&peer)).await {
        Ok(Some(head)) => {
            eprintln!("synced {} from {}", &head[..7], peer.fmt_short());
            announce(vault, net, announced).await;
        }
        Ok(None) => {}
        Err(e) => eprintln!("{e:#}"),
    }
}

/// Pokes every other member if HEAD moved since the last poke.
async fn announce(vault: &Vault, net: &Net, announced: &mut Option<String>) {
    let (head, peers) = match blocking(vault, |v| Ok((v.head()?, v.peers()?))).await {
        Ok(found) => found,
        // Left unannounced, so the next event tries again.
        Err(e) => return eprintln!("could not tell peers about new commits: {e:#}"),
    };
    if Some(&head) == announced.as_ref() {
        return;
    }
    *announced = Some(head);
    for peer in peers {
        let net = net.clone();
        tokio::spawn(async move { net.poke(peer).await });
    }
}
