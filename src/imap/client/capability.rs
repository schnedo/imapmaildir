use enumflags2::{BitFlags, bitflags};
use log::trace;

#[bitflags]
#[repr(u8)]
#[derive(Copy, Clone, Debug)]
pub enum Capability {
    Condstore,
    Enable,
    Idle,
    Imap4rev1,
    QResync,
}
#[derive(Debug, Default)]
pub struct Capabilities {
    capabilities: BitFlags<Capability>,
}

impl Capabilities {
    pub fn insert(&mut self, capability: &imap_proto::Capability) {
        match capability {
            imap_proto::Capability::Imap4rev1 => {
                self.capabilities.insert(Capability::Imap4rev1);
            }
            imap_proto::Capability::Auth(_) => {
                panic!("tried inserting auth capability in capabilities");
            }
            imap_proto::Capability::Atom(cow) => match cow.as_ref() {
                "CONDSTORE" => {
                    self.capabilities.insert(Capability::Condstore);
                }
                "ENABLE" => {
                    self.capabilities.insert(Capability::Enable);
                }
                "IDLE" => {
                    self.capabilities.insert(Capability::Idle);
                }
                "QRESYNC" => {
                    self.capabilities.insert(Capability::QResync);
                }
                _ => {
                    trace!("unknown capability {cow}");
                }
            },
        }
    }

    pub fn contains(&self, other: Capability) -> bool {
        self.capabilities.contains(other)
    }
}

#[bitflags]
#[repr(u8)]
#[derive(Copy, Clone, Debug)]
pub enum AuthCapability {
    Plain,
}
#[derive(Debug, Default)]
pub struct AuthCapabilities {
    capabilities: BitFlags<AuthCapability>,
}

impl AuthCapabilities {
    pub fn insert(&mut self, capability: &imap_proto::Capability) {
        match capability {
            imap_proto::Capability::Auth(cow) => match cow.as_ref() {
                "PLAIN" => {
                    self.capabilities.insert(AuthCapability::Plain);
                }
                _ => {
                    trace!("unknown auth capability {cow}");
                }
            },
            _ => {
                panic!("tried inserting non-auth capability in auth_capabilities");
            }
        }
    }

    pub fn contains(&self, other: AuthCapability) -> bool {
        self.capabilities.contains(other)
    }
}
