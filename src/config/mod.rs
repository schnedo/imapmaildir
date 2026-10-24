mod account;

pub use crate::imap::Auth;
pub use crate::imap::AuthError;
pub use crate::imap::OAuth2Auth;
pub use crate::imap::PlainAuth;
pub use account::Account;
pub use account::Error as AccountError;

pub use crate::sync::Config as SyncConfig;
