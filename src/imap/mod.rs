mod client;
mod remote_changes;
mod remote_mail;
mod transport;

pub use client::AuthenticatedClient;
pub use client::Client;
pub use client::SelectedClient;
pub use remote_changes::RemoteChanges;
pub use remote_changes::Selection;
pub use remote_mail::RemoteContent;
pub use remote_mail::RemoteMail;
pub use remote_mail::RemoteMailMetadata;
pub use remote_mail::RemoteMailMetadataBuilder;
