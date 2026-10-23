pub mod config;
mod imap;
pub mod logging;
mod maildir;
mod repository;
mod sync;

pub use crate::imap::Client;
pub use crate::imap::ClientError;
pub use crate::imap::Tls;
pub use crate::imap::TransportError;
pub use crate::sync::Error as SyncError;
pub use crate::sync::Syncer;
pub use crate::sync::Transient;
pub use crate::sync::reconnecting;
pub use crate::sync::on_local_change;
