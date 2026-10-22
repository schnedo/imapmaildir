mod authenticated;
mod capability;
mod encoding;
mod error;
mod fetch;
mod message_count;
mod not_authenticated;
mod selected;
mod uid_map;

pub use authenticated::AuthenticatedClient;
pub use error::Error;
pub use not_authenticated::Client;
pub use selected::IdleStopReason;
pub use selected::SelectedClient;
