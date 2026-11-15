mod git;
mod vault;

use std::path::PathBuf;

use anyhow::Result;
use clap::{Parser, Subcommand};

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
    /// Watch the vault and commit edits as they happen.
    Run,
}

fn main() -> Result<()> {
    let cli = Cli::parse();
    match cli.command {
        Cmd::Init { folder, machine } => {
            let vault = Vault::init(&folder, machine)?;
            print!("{}", vault.status()?);
        }
        Cmd::Status => print!("{}", Vault::open(&cli.vault)?.status()?),
        Cmd::Log { limit, path } => {
            print!("{}", Vault::open(&cli.vault)?.log(limit, path.as_deref())?)
        }
        Cmd::Rollback { commit, path } => {
            let id = Vault::open(&cli.vault)?.rollback(&commit, path.as_deref())?;
            println!("committed {id}");
        }
        Cmd::Run => Vault::open(&cli.vault)?.run()?,
    }
    Ok(())
}
