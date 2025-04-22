use futures::stream::StreamExt;
use log::{debug, trace};
use thiserror::Error;

use crate::imap::connection::SendCommand;

use super::session::Session;

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
        let mut responses = self.connection.send(&command);
        let response = responses
            .next()
            .await
            .expect("login should receive response");
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
