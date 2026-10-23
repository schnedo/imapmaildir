use thiserror::Error;

use crate::{
    imap::{
        client::{
            auth::Error as AuthError,
            capability::{AuthCapability, Capability},
        },
        transport,
    },
    repository::{MailboxMetadataBuilderError, UidConversionError, UidValidity},
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
    #[error("Server supports neither OAUTHBEARER nor XOAUTH2 authentication")]
    MissingOAuth2Mechanism,
    #[error("Server rejected login: {}", .0.as_deref().unwrap_or("NO"))]
    LoginRejected(Option<String>),
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

impl Error {
    /// Whether reconnecting may succeed, e.g. because the connection broke or the server is
    /// temporarily unavailable
    #[must_use]
    pub fn is_transient(&self) -> bool {
        match self {
            Self::Transport(e) => e.is_transient(),
            // e.g. password manager not unlocked yet
            Self::Auth(_) | Self::ConnectionRejected(_) | Self::InvalidGreeting => true,
            // retrying wrong credentials could lock the account
            Self::LoginRejected(information) => !information.as_deref().is_some_and(|info| {
                [
                    "[AUTHENTICATIONFAILED]",
                    "[AUTHORIZATIONFAILED]",
                    "[EXPIRED]",
                    "[PRIVACYREQUIRED]",
                ]
                .iter()
                .any(|code| info.starts_with(code))
            }),
            Self::MissingCapability(_)
            | Self::MissingAuthCapability(_)
            | Self::MissingOAuth2Mechanism
            | Self::PreAuthUnsupported
            | Self::InvalidUid(_)
            | Self::InvalidModSeq(_)
            | Self::InvalidUidValidity(_)
            | Self::InvalidUidSet(_)
            | Self::UidValidityMismatch { .. }
            | Self::IncompleteMailboxData(_)
            | Self::MissingExists
            | Self::IncompleteMetadata
            | Self::MissingAppendUid
            | Self::ImplausibleUidList(_)
            | Self::IdleRejected
            | Self::IdleStopClosed
            | Self::TaskProcessingStopped => false,
        }
    }
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_wrong_credentials_are_permanent() {
        assert!(
            !Error::LoginRejected(Some(
                "[AUTHENTICATIONFAILED] Invalid credentials (Failure)".to_string()
            ))
            .is_transient()
        );
    }

    #[test]
    fn test_other_login_rejections_are_transient() {
        assert!(
            Error::LoginRejected(Some(
                "Too many simultaneous connections. (Failure)".to_string()
            ))
            .is_transient()
        );
        assert!(Error::LoginRejected(None).is_transient());
    }

    #[test]
    fn test_broken_connection_is_transient() {
        assert!(Error::Transport(transport::Error::Closed).is_transient());
    }

    #[test]
    fn test_protocol_errors_are_permanent() {
        assert!(!Error::MissingCapability(Capability::Condstore).is_transient());
        assert!(!Error::ImplausibleUidList(3).is_transient());
    }
}
