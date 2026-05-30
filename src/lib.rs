mod config;
mod imap;
mod maildir;
#[cfg(test)]
mod mock_server;
mod repository;
mod sync;

pub use crate::config::Config;
pub use crate::imap::Client;
pub use crate::sync::Syncer;
