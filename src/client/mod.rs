mod codec;
mod connection;
mod session;
mod tag_generator;

use connection::Connection;
use futures::stream::StreamExt;
use session::Session;
use tag_generator::TagGenerator;
use thiserror::Error;

pub struct Client {
    connection: Connection,
}

impl Client {
    pub async fn connect(host: &str, port: u16) -> Self {
        let (connection, _) = Connection::connect_to(host, port).await;

        Client { connection }
    }

    pub async fn login(mut self, username: &str, password: &str) -> Result<Session, LoginError> {
        let command = format!("LOGIN {username} {password}");
        let mut responses = self.connection.send(&command);
        let response = responses
            .next()
            .await
            .expect("login should receive response");
        if let imap_proto::Response::Done {
            tag: _,
            status,
            code: _,
            information: _,
        } = response.parsed()
        {
            match status {
                imap_proto::Status::Ok => {
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
