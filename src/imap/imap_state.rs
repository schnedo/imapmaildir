use std::sync::Mutex;

use log::{trace, warn};

#[derive(Default)]
pub struct ImapState {
    capabilities: Mutex<BitFlags<Capability>>,
}

impl ImapState {
    pub fn handle_untagged_response(&self, response: &imap_proto::Response<'_>) {
        trace!("handling untagged response {response:?}");
        match response {
            imap_proto::Response::Capabilities(items)
            | imap_proto::Response::Data {
                status: imap_proto::Status::Ok,
                code: Some(imap_proto::ResponseCode::Capabilities(items)),
                information: _,
            } => {
                self.update_capabilities(items);
            }
            imap_proto::Response::Data {
                status,
                code,
                information,
            } => todo!(),
            imap_proto::Response::Expunge(_) => todo!(),
            imap_proto::Response::Vanished { earlier, uids } => todo!(),
            imap_proto::Response::Fetch(_, attribute_values) => todo!(),
            imap_proto::Response::MailboxData(mailbox_datum) => todo!(),
            imap_proto::Response::Quota(quota) => todo!(),
            imap_proto::Response::QuotaRoot(quota_root) => todo!(),
            imap_proto::Response::Id(hash_map) => todo!(),
            imap_proto::Response::Acl(acl) => todo!(),
            imap_proto::Response::ListRights(list_rights) => todo!(),
            imap_proto::Response::MyRights(my_rights) => todo!(),
            _ => warn!("ignoring unknown untagged response: {response:?}"),
        }
    }

    pub fn update_capabilities(&self, capabilities: &[imap_proto::Capability]) {
        let mut caps = self
            .capabilities
            .lock()
            .expect("capabilities should be lockable");
        for capability in capabilities {
            match capability {
                imap_proto::Capability::Imap4rev1 => {
                    caps.insert(Capability::Imap4rev1);
                }
                imap_proto::Capability::Auth(cow) => {
                    trace!("unhandled auth capabilty {cow}");
                }
                imap_proto::Capability::Atom(cow) => match cow.as_ref() {
                    "CONDSTORE" => {
                        caps.insert(Capability::Condstore);
                    }
                    "ENABLE" => {
                        caps.insert(Capability::Enable);
                    }
                    "IDLE" => {
                        caps.insert(Capability::Idle);
                    }
                    "QRESYNC" => {
                        caps.insert(Capability::QResync);
                    }
                    _ => {
                        trace!("unhandled capability {cow}");
                    }
                },
            }
        }
        trace!("updated capabilities to {caps:?}");
    }
}
