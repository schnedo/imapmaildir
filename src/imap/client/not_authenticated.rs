use log::{debug, trace};
use tokio::sync::mpsc;

use crate::{
    config::{self, Auth},
    imap::{
        client::{
            AuthenticatedClient,
            capability::{AuthCapabilities, AuthCapability, Capabilities},
        },
        transport::{Connection, ResponseData},
    },
};

pub struct Client {
    connection: Connection,
    untagged_response_receiver: mpsc::Receiver<ResponseData>,
    capabilities: Capabilities,
    auth_capabilities: AuthCapabilities,
}

impl Client {
    pub async fn login(
        connection_config: &config::Connection,
        auth_config: &Auth,
    ) -> AuthenticatedClient {
        let connected = Self::connect(connection_config).await;
        connected.authenticate(auth_config).await
    }

    async fn connect(connection_config: &config::Connection) -> Self {
        let (untagged_response_sender, mut untagged_response_receiver) = mpsc::channel(32);
        // todo: pass configured server cert file
        let mut connection = Connection::start(connection_config, untagged_response_sender)
            .await
            .expect("creating a connection should succeed");

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
                if let Some(information) = information {
                    trace!("greeting information: {information}");
                }
                if let Some(imap_proto::ResponseCode::Capabilities(caps)) = code {
                    update_capabilities(&mut capabilities, &mut auth_capabilities, caps);
                } else {
                    let command = "CAPABILITY";
                    debug!("{command}");
                    connection
                        .send(command.into())
                        .await
                        .expect("capability request should succeed");
                    let response = untagged_response_receiver.recv().await.expect(
                        "CAPABILITY command should return single untagged CAPABILITY response",
                    );
                    match response.parsed() {
                        imap_proto::Response::Capabilities(caps) => {
                            update_capabilities(&mut capabilities, &mut auth_capabilities, caps);
                        }
                        _ => todo!("handle misbehaved response to CAPABILITY"),
                    }
                }
            }
            imap_proto::Response::Data {
                status: imap_proto::Status::Bad,
                code: _,
                information: _,
            } => {
                todo!("handle server rejecting connection");
            }
            imap_proto::Response::Data {
                status: imap_proto::Status::PreAuth,
                code: _,
                information: _,
            } => {
                todo!("handle pre-authenticated state");
            }
            _ => {
                panic!("greeting should only ever be ok, bad or preauth, as per specification")
            }
        }

        Self {
            connection,
            untagged_response_receiver,
            capabilities,
            auth_capabilities,
        }
    }

    async fn authenticate(mut self, auth_config: &Auth) -> AuthenticatedClient {
        match auth_config {
            Auth::Plain(plain_auth_config) => {
                assert!(
                    self.auth_capabilities.contains(AuthCapability::Plain),
                    "server should support PLAIN auth capability"
                );
                debug!("LOGIN <user> <password>");
                let response = self
                    .connection
                    // todo: this will block if server sends untagged response
                    .send(
                        format!(
                            "LOGIN {} {}",
                            plain_auth_config.user(),
                            plain_auth_config.password()
                        )
                        .into(),
                    )
                    .await
                    .expect("login should succeed");
                if let Some(imap_proto::ResponseCode::Capabilities(caps)) =
                    response.unsafe_get_tagged_response_code()
                {
                    update_capabilities(&mut self.capabilities, &mut self.auth_capabilities, caps);
                } else {
                    self.connection
                        .send("CAPABILITY".into())
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
    }
}

fn update_capabilities(
    capabilities: &mut Capabilities,
    auth_capabilities: &mut AuthCapabilities,
    caps: &[imap_proto::Capability],
) {
    for cap in caps {
        match cap {
            imap_proto::Capability::Auth(_) => auth_capabilities.insert(cap),
            imap_proto::Capability::Imap4rev1 | imap_proto::Capability::Atom(_) => {
                capabilities.insert(cap);
            }
        }
    }
    trace!("updated capabilities to {capabilities:?}");
}
