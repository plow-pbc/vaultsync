//! The vaultsync engine: everything the command-line tool and the Mac app
//! do, behind one command interface (`cli`), so both editions stay the same.

pub mod cli;
mod config;
mod daemon;
mod git;
mod join;
mod members;
mod merge;
mod net;
mod status;
mod vault;
