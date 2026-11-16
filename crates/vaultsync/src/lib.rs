//! The vaultsync engine: a folder of notes, its history, and the sync between
//! machines. The terminal edition (`vaultsync`) and the Mac app are shells
//! over this one library.

pub mod config;
pub mod daemon;
pub mod join;
pub mod members;
pub mod merge;
pub mod net;
pub mod status;
pub mod vault;
