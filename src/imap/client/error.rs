use thiserror::Error;

use crate::{
    config::AuthError,
    imap::{
        client::capability::{AuthCapability, Capability},
        transport,
    },
    repository::{MailboxMetadataBuilderError, Uid, UidConversionError, UidValidity},
};

#[derive(Debug, Error)]
pub enum Error {
    #[error("{0}")]
    Transport(transport::Error),
    #[error("{0}")]
    Auth(AuthError),
    #[error("Server does not support {0:?} capability")]
    MissingCapability(Capability),
    #[error("Server does not support {0:?} authentication")]
    MissingAuthCapability(AuthCapability),
    #[error("Server rejected connection: {}", .0.as_deref().unwrap_or("BAD"))]
    ConnectionRejected(Option<String>),
    #[error("Pre-authenticated connections are not supported")]
    PreAuthUnsupported,
    #[error("Received invalid greeting from server")]
    InvalidGreeting,
    #[error("Received invalid uid: {0}")]
    InvalidUid(UidConversionError),
    #[error("Received invalid modseq: {0}")]
    InvalidModSeq(&'static str),
    #[error("Received invalid uid validity: {0}")]
    InvalidUidValidity(&'static str),
    #[error("Received invalid uid set {0}")]
    InvalidUidSet(String),
    #[error("Remote uid validity {remote} differs from local {local}")]
    UidValidityMismatch {
        local: UidValidity,
        remote: UidValidity,
    },
    #[error("Received incomplete mailbox data on SELECT: {0}")]
    IncompleteMailboxData(MailboxMetadataBuilderError),
    #[error("Server did not send EXISTS on SELECT")]
    MissingExists,
    #[error("Received incomplete mail metadata")]
    IncompleteMetadata,
    #[error("Received mail {0} without content")]
    MissingContent(Uid),
    #[error("Server did not respond to APPEND with APPENDUID")]
    MissingAppendUid,
    #[error(
        "Server returned no uids for mailbox with {0} messages. Refusing to delete all local mails"
    )]
    ImplausibleUidList(u32),
    #[error("Server rejected IDLE")]
    IdleRejected,
    #[error("Idle stop channel closed")]
    IdleStopClosed,
    #[error("Task processing stopped")]
    TaskProcessingStopped,
}

impl From<transport::Error> for Error {
    fn from(value: transport::Error) -> Self {
        Self::Transport(value)
    }
}

impl From<AuthError> for Error {
    fn from(value: AuthError) -> Self {
        Self::Auth(value)
    }
}

impl From<UidConversionError> for Error {
    fn from(value: UidConversionError) -> Self {
        Self::InvalidUid(value)
    }
}
