//! The vaultsync engine: everything the command-line tool and the Mac app
//! do, behind one command interface (`cli`), so both editions stay the same.

pub mod cli;
mod config;
mod daemon;
mod join;
pub mod members;
pub mod merge;
mod net;
mod status;
mod vault;
