mod authenticated;
mod capability;
mod not_authenticated;
mod selected;

pub use authenticated::AuthenticatedClient;
pub use authenticated::RemoteChanges;
pub use authenticated::Selection;
pub use not_authenticated::Client;
pub use selected::SelectedClient;
