use futures::stream::StreamExt;
use log::{debug, trace};
use thiserror::Error;

use crate::imap::{client::mail::Session, connection::SendCommand};

pub struct Client<T: SendCommand> {
    connection: T,
}

impl<T: SendCommand> Client<T> {
    pub fn new(connection: T) -> Self {
        Self { connection }
    }

    pub async fn login(mut self, username: &str, password: &str) -> Result<Session<T>, LoginError> {
        debug!("LOGIN <user> <password>");
        let command = format!("LOGIN {username} {password}");
        let response = {
            let mut responses = self.connection.send(&command);
            responses
                .next()
                .await
                .expect("login should receive response")
        };
        if let imap_proto::Response::Done {
            tag: _,
            status,
            code,
            information: _,
        } = response.parsed()
        {
            match status {
                imap_proto::Status::Ok => {
                    trace!("{:?}", code);
                    Ok(Session::new(self.connection))
                },
                imap_proto::Status::No => Err(LoginError),
                imap_proto::Status::Bad => panic!("Login command unknown or invalid arguments. This is an unrecoverable issue in code."),
                _ => panic!("response to login should only ever be Ok, No or Bad"),
            }
        } else {
            panic!("response to login should only ever be tagged")
        }
    }
}

#[derive(Debug, Error)]
#[error("username or password rejected")]
pub struct LoginError;

#[cfg(test)]
mod tests {
    use imap_proto::*;

    use crate::imap::connection::mock_connection::MockConnection;

    use super::*;

    #[tokio::test]
    async fn should_return_session_when_login_ok() {
        let mock_responses = [[Response::Done {
            tag: RequestId("0000".to_owned()),
            status: Status::Ok,
            code: Some(ResponseCode::Capabilities(vec![Capability::Imap4rev1])),
            information: Some(std::borrow::Cow::Borrowed("Logged in")),
        }]];
        let mock_connection = MockConnection::new(mock_responses);
        let client = Client::new(mock_connection);

        let maybe_session = client.login("name", "password").await;

        assert!(matches!(maybe_session, Ok(Session { .. })));
    }

    #[tokio::test]
    async fn should_return_login_error_when_login_no() {
        let mock_responses = [[Response::Done {
            tag: RequestId("0000".to_owned()),
            status: Status::No,
            code: None,
            information: Some(std::borrow::Cow::Borrowed(
                "[AUTHENTICATIONFAILED] Authentication failed.",
            )),
        }]];
        let mock_connection = MockConnection::new(mock_responses);
        let client = Client::new(mock_connection);

        let maybe_session = client.login("name", "password").await;

        assert!(matches!(maybe_session, Err(LoginError)));
    }
}
