use std::time::Duration;

use thiserror::Error;

use crate::{
    imap::ClientError,
    maildir::{
        DeleteError, DetectChangesError, InitError, LoadError, MaildirError,
        MaildirRepositoryError, StateError, StoreError,
    },
    repository::UidValidity,
};

#[derive(Debug, Error)]
pub enum Error {
    #[error("{0}")]
    Imap(ClientError),
    #[error("Cannot load maildir repository: {0}")]
    Load(LoadError),
    #[error("Cannot initialize maildir repository: {0}")]
    Init(InitError),
    #[error("{0}")]
    State(StateError),
    #[error("Cannot store mail: {0}")]
    Store(StoreError),
    #[error("Cannot delete mail: {0}")]
    Delete(DeleteError),
    #[error("Cannot detect local changes: {0}")]
    DetectChanges(DetectChangesError),
    #[error("{0}")]
    Repository(MaildirRepositoryError),
    #[error("{0}")]
    Maildir(MaildirError),
    #[error("Remote uid validity {remote} differs from local {local}")]
    UidValidityMismatch {
        local: UidValidity,
        remote: UidValidity,
    },
    #[error("Task processing stopped")]
    TaskProcessingStopped,
    #[error("Task processing panicked")]
    TaskProcessingPanicked,
    #[error("Shut down")]
    ShutDown,
}

impl Error {
    /// Whether reconnecting may succeed. Local errors are never transient.
    #[must_use]
    pub fn is_transient(&self) -> bool {
        match self {
            Self::Imap(e) => e.is_transient(),
            _ => false,
        }
    }

    /// How long IDLE lasted before the server ended it
    #[must_use]
    pub fn idle_terminated(&self) -> Option<Duration> {
        match self {
            Self::Imap(ClientError::IdleTerminated(idled)) => Some(*idled),
            _ => None,
        }
    }
}

impl From<ClientError> for Error {
    fn from(value: ClientError) -> Self {
        Self::Imap(value)
    }
}

impl From<LoadError> for Error {
    fn from(value: LoadError) -> Self {
        Self::Load(value)
    }
}

impl From<InitError> for Error {
    fn from(value: InitError) -> Self {
        Self::Init(value)
    }
}

impl From<StateError> for Error {
    fn from(value: StateError) -> Self {
        Self::State(value)
    }
}

impl From<StoreError> for Error {
    fn from(value: StoreError) -> Self {
        Self::Store(value)
    }
}

impl From<DeleteError> for Error {
    fn from(value: DeleteError) -> Self {
        Self::Delete(value)
    }
}

impl From<DetectChangesError> for Error {
    fn from(value: DetectChangesError) -> Self {
        Self::DetectChanges(value)
    }
}

impl From<MaildirRepositoryError> for Error {
    fn from(value: MaildirRepositoryError) -> Self {
        Self::Repository(value)
    }
}

impl From<MaildirError> for Error {
    fn from(value: MaildirError) -> Self {
        Self::Maildir(value)
    }
}
