mod client;
mod codec;
mod connection;
mod mailbox;
mod tag_generator;

pub use client::AuthenticatedClient;
pub use client::Client;
pub use mailbox::ModSeq;
pub use mailbox::RemoteMail;
pub use mailbox::RemoteMailMetadata;
pub use mailbox::SequenceSetBuilder;
pub use mailbox::Uid;
pub use mailbox::UidValidity;
