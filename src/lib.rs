pub mod config;
mod imap;
mod maildir;
mod repository;
mod sync;

pub use crate::imap::Client;
pub use crate::sync::Syncer;
