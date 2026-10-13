pub mod config;
mod imap;
pub mod logging;
mod maildir;
mod repository;
mod sync;

pub use crate::imap::Client;
pub use crate::sync::Syncer;
pub use crate::sync::on_local_change;
