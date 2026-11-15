mod daemon;
mod git;
mod members;
mod merge;
mod net;
mod vault;

use std::path::PathBuf;
use std::time::Duration;

use anyhow::Result;
use clap::{Parser, Subcommand};
use iroh::EndpointId;

use vault::Vault;

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
    Status,
    /// Print this machine's node id, which other members add.
    Id,
    /// Change the member list.
    #[command(subcommand)]
    Member(MemberCmd),
    /// List commits, newest first.
    Log {
        #[arg(short = 'n', default_value_t = 20)]
        limit: usize,
        /// Only commits touching this path (relative to the vault).
        path: Option<PathBuf>,
    },
    /// Restore the vault, or one path, to a prior commit as a new commit.
    Rollback {
        commit: String,
        /// Path relative to the vault.
        path: Option<PathBuf>,
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
            print!("{}", vault.status()?);
        }
        Cmd::Status => print!("{}", open()?.status()?),
        Cmd::Id => println!("{}", open()?.node_id()?),
        Cmd::Member(MemberCmd::Add { id, name }) => {
            println!("committed {}", open()?.add_member(id, &name)?)
        }
        Cmd::Member(MemberCmd::List) => print!("{}", members::format(&open()?.members()?)),
        Cmd::Log { limit, path } => print!("{}", open()?.log(limit, path.as_deref())?),
        Cmd::Rollback { commit, path } => {
            println!("committed {}", open()?.rollback(&commit, path.as_deref())?)
        }
        Cmd::Run { catch_up, addr_dir } => daemon::run(
            open()?,
            daemon::Opts {
                catch_up: Duration::from_secs(catch_up),
                addr_dir,
            },
        )?,
    }
    Ok(())
}
