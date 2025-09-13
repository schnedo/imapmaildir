use std::sync::{Arc, Mutex};

use log::{debug, trace};
use tokio::sync::mpsc;

use crate::imap::{
    client::{
        AuthenticatedClient,
        capability::{AuthCapabilities, AuthCapability, Capabilities, Capability},
    },
    codec::ResponseData,
    connection::Connection,
};

pub struct NotAuthenticatedClient {
    connection: Connection,
    untagged_response_receiver: mpsc::Receiver<ResponseData>,
    capabilities: Capabilities,
    auth_capabilities: AuthCapabilities,
}

impl NotAuthenticatedClient {
    pub async fn connect(host: &str, port: u16) -> Self {
        let (untagged_response_sender, mut untagged_response_receiver) = mpsc::channel(32);
        let mut connection = Connection::start(host, port, untagged_response_sender).await;

        let mut capabilities = Capabilities::default();
        let mut auth_capabilities = AuthCapabilities::default();

        let greeting = untagged_response_receiver
            .recv()
            .await
            .expect("there should be a greeting from the server");
        match greeting.parsed() {
            imap_proto::Response::Data {
                status: imap_proto::Status::Ok,
                code,
                information,
            } => {
                if let Some(imap_proto::ResponseCode::Capabilities(caps)) = code {
                    update_capabilities(&mut capabilities, &mut auth_capabilities, caps);
                } else {
                    connection
                        .send("CAPABILITY")
                        .await
                        .expect("capability request should succeed");
                    if let Some(imap_proto::ResponseCode::Capabilities(caps)) = code {
                        update_capabilities(&mut capabilities, &mut auth_capabilities, caps);
                    } else {
                        panic!(
                            "capability request should be answered with capabilities response as specified"
                        );
                    }
                }
            }
            imap_proto::Response::Data {
                status: imap_proto::Status::Bad,
                code,
                information,
            } => {
                todo!("handle server rejecting connection");
            }
            imap_proto::Response::Data {
                status: imap_proto::Status::PreAuth,
                code,
                information,
            } => {
                todo!("handle pre-authenticated state");
            }
            _ => {
                panic!("greeting should only ever be ok, bad or preauth, as per specification")
            }
        }

        // tokio::spawn(async move {
        //     while let Some(response) = untagged_response_receiver.recv().await {
        //         state.handle_untagged_response(response.parsed());
        //     }
        // });

        Self {
            connection,
            untagged_response_receiver,
            capabilities,
            auth_capabilities,
        }
    }

    pub async fn login(mut self, username: &str, password: &str) -> AuthenticatedClient {
        assert!(self.auth_capabilities.contains(AuthCapability::Plain));
        debug!("LOGIN <user> <password>");
        let response = self
            .connection
            .send(&format!("LOGIN {username} {password}"))
            .await
            .expect("login should succeed");
        if let Some(imap_proto::ResponseCode::Capabilities(caps)) =
            response.unsafe_get_tagged_response_code()
        {
            update_capabilities(&mut self.capabilities, &mut self.auth_capabilities, caps);
        } else {
            self.connection
                .send("CAPABILITY")
                .await
                .expect("capabilities should succeed");
        }

        AuthenticatedClient::new(
            self.connection,
            self.capabilities,
            self.untagged_response_receiver,
        )
    }
}

fn update_capabilities(
    capabilities: &mut Capabilities,
    auth_capabilities: &mut AuthCapabilities,
    caps: &[imap_proto::Capability],
) {
    for cap in caps {
        match cap {
            imap_proto::Capability::Imap4rev1 => capabilities.insert(cap),
            imap_proto::Capability::Auth(cow) => auth_capabilities.insert(cap),
            imap_proto::Capability::Atom(cow) => capabilities.insert(cap),
        }
    }
    trace!("updated capabilities to {capabilities:?}");
}
