use std::path::PathBuf;
use std::time::Duration;

use anyhow::Result;
use clap::{Parser, Subcommand};
use iroh::EndpointId;

use vaultsync::vault::Vault;
use vaultsync::{config, daemon, join, members, status};

/// Keeps a folder of notes in sync across machines, with full history.
#[derive(Parser)]
#[command(version)]
struct Cli {
    /// A path inside the vault (default: the current directory).
    #[arg(long, global = true, default_value = ".")]
    vault: PathBuf,
    #[command(subcommand)]
    command: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    /// Make a folder a vault, keeping any git history it already has.
    Init {
        folder: PathBuf,
        /// Name recorded on this machine's commits (default: the hostname).
        #[arg(long)]
        machine: Option<String>,
    },
    /// Show the vault, this machine, and uncommitted changes.
    Status {
        /// As JSON, with peers, last sync and conflicts, for a front end.
        #[arg(long)]
        json: bool,
        /// Every vault the background service runs (with --json).
        #[arg(long, requires = "json")]
        all: bool,
    },
    /// Have the background service (`run --all`) keep this vault in sync.
    /// `init` and `join` do this themselves.
    Track,
    /// Print this machine's node id, which other members add.
    Id,
    /// Change the member list.
    #[command(subcommand)]
    Member(MemberCmd),
    /// Print a one-time code (and link) another machine can join with.
    Invite {
        /// Seconds the code stays valid.
        #[arg(long, default_value_t = 600, value_parser = clap::value_parser!(u64).range(1..))]
        expires_in: u64,
    },
    /// Join a vault with a code from `vaultsync invite` on a member.
    Join {
        /// The code, or the whole link.
        code: String,
        /// Where to put the vault (default: the inviting machine's folder).
        #[arg(long)]
        folder: Option<PathBuf>,
        /// Name recorded on this machine's commits (default: the hostname).
        #[arg(long)]
        machine: Option<String>,
        #[arg(long, hide = true)]
        addr_dir: Option<PathBuf>,
    },
    /// Remove a machine, by name or node id. Every member refuses it as the
    /// removal reaches them.
    Remove { device: String },
    /// List commits, newest first.
    Log {
        #[arg(short = 'n', default_value_t = 20)]
        limit: usize,
        /// Only commits touching this path (relative to the vault).
        path: Option<PathBuf>,
    },
    /// Restore the vault, or some of its paths, to a prior commit as a new commit.
    Rollback {
        commit: String,
        /// Paths relative to the vault, all in one commit; none for the whole vault.
        paths: Vec<PathBuf>,
    },
    /// Commit edits as they happen and sync with the other members.
    Run {
        /// Seconds between catch-ups with every member.
        #[arg(long, default_value_t = 30, value_parser = clap::value_parser!(u64).range(1..))]
        catch_up: u64,
        /// Find peers through address files in this directory instead of the
        /// network (tests, offline use).
        #[arg(long, hide = true)]
        addr_dir: Option<PathBuf>,
        /// Look for changes every 2 s instead of using the OS's file
        /// notifications (used anyway when those are unavailable).
        #[arg(long)]
        poll: bool,
        /// Run every vault in ~/.config/vaultsync/vaults, picking up ones
        /// added later (what the background service runs).
        #[arg(long)]
        all: bool,
    },
}

#[derive(Subcommand)]
enum MemberCmd {
    /// Trust another machine, by the id `vaultsync id` prints there.
    Add {
        id: EndpointId,
        #[arg(long)]
        name: String,
    },
    /// List the members.
    List,
}

fn main() -> Result<()> {
    let cli = Cli::parse();
    let open = || Vault::open(&cli.vault);
    match cli.command {
        Cmd::Init { folder, machine } => {
            let vault = Vault::init(&folder, machine)?;
            config::track(vault.folder())?;
            print!("{}", vault.status()?);
        }
        Cmd::Status { json: false, .. } => print!("{}", open()?.status()?),
        Cmd::Status { json: true, all } => {
            let value = if all {
                // A vault moved or deleted since it was listed reports its
                // error; the others still show.
                let vaults: Vec<_> = config::vaults()?
                    .iter()
                    .map(|folder| {
                        Vault::open(folder)
                            .and_then(|vault| status::json(&vault))
                            .unwrap_or_else(|e| {
                                serde_json::json!({"folder": folder, "error": format!("{e:#}")})
                            })
                    })
                    .collect();
                serde_json::Value::from(vaults)
            } else {
                status::json(&open()?)?
            };
            println!("{value}");
        }
        Cmd::Track => config::track(open()?.folder())?,
        Cmd::Id => println!("{}", open()?.node_id()?),
        Cmd::Member(MemberCmd::Add { id, name }) => {
            println!("committed {}", open()?.add_member(id, &name)?)
        }
        Cmd::Member(MemberCmd::List) => print!("{}", members::format(&open()?.members()?)),
        Cmd::Invite { expires_in } => print!(
            "{}",
            join::invite(&open()?, Duration::from_secs(expires_in))?
        ),
        Cmd::Join {
            code,
            folder,
            machine,
            addr_dir,
        } => {
            let vault = join::join(&code, folder, machine, addr_dir)?;
            config::track(vault.folder())?;
            print!("{}", vault.status()?);
            println!(
                "Joined. Start syncing with: vaultsync --vault {} run",
                vault.folder().display()
            );
        }
        Cmd::Remove { device } => println!("committed {}", open()?.remove_member(&device)?),
        Cmd::Log { limit, path } => print!("{}", open()?.log(limit, path.as_deref())?),
        Cmd::Rollback { commit, paths } => {
            println!("committed {}", open()?.rollback(&commit, &paths)?)
        }
        Cmd::Run {
            catch_up,
            addr_dir,
            poll,
            all,
        } => {
            let opts = daemon::Opts {
                catch_up: Duration::from_secs(catch_up),
                addr_dir,
                poll,
            };
            if all {
                daemon::run_all(opts)?
            } else {
                daemon::run(open()?, opts)?
            }
        }
    }
    Ok(())
}
