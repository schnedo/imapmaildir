use std::num::NonZeroU64;

use enumflags2::{BitFlags, bitflags};

use crate::imap::client::encoding::LiteralMode;

#[bitflags]
#[repr(u16)]
#[derive(Copy, Clone, Debug)]
// todo: check https://datatracker.ietf.org/doc/html/rfc5550#section-12 and update readme regarding
// compatibility to lemonade mail stores
pub enum Capability {
    Condstore, // rfc7162
    Enable,    // rfc5161
    Idle,      // rfc2177
    Imap4rev1,
    LiteralMinus, // rfc7888
    LiteralPlus,  // rfc7888
    Multiappend,  // rfc3502
    QResync,      // rfc7162
    UidPlus,      // rfc4315
}
#[derive(Debug, Default)]
pub struct Capabilities {
    capabilities: BitFlags<Capability>,
    /// Maximum size of appended mails in octets, if the server announces one for all mailboxes
    /// (RFC 7889)
    append_limit: Option<NonZeroU64>,
}

impl Capabilities {
    pub fn insert(&mut self, capability: &imap_proto::Capability) {
        match capability {
            imap_proto::Capability::Imap4rev1 => {
                self.capabilities.insert(Capability::Imap4rev1);
            }
            imap_proto::Capability::Auth(_) => {
                unreachable!("tried inserting auth capability in capabilities");
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
                "LITERAL-" => {
                    self.capabilities.insert(Capability::LiteralMinus);
                }
                "LITERAL+" => {
                    self.capabilities.insert(Capability::LiteralPlus);
                }
                "MULTIAPPEND" => {
                    self.capabilities.insert(Capability::Multiappend);
                }
                "QRESYNC" => {
                    self.capabilities.insert(Capability::QResync);
                }
                "UIDPLUS" => {
                    self.capabilities.insert(Capability::UidPlus);
                }
                "APPENDLIMIT" => {
                    // limits per mailbox require STATUS (APPENDLIMIT), which imap-proto cannot
                    // parse. The server rejects too large mails then.
                    log::debug!("server announced append limits per mailbox, ignoring them");
                }
                atom if let Some(limit) = atom.strip_prefix("APPENDLIMIT=") => {
                    match limit.parse() {
                        Ok(limit) => self.append_limit = Some(limit),
                        Err(e) => log::warn!("ignoring invalid capability {atom}: {e}"),
                    }
                }
                _ => {
                    log::trace!("unknown capability {cow}");
                }
            },
        }
    }

    pub fn contains(&self, other: Capability) -> bool {
        self.capabilities.contains(other)
    }

    pub fn append_limit(&self) -> Option<NonZeroU64> {
        self.append_limit
    }

    pub fn literal_mode(&self) -> LiteralMode {
        if self.contains(Capability::LiteralPlus) {
            LiteralMode::Plus
        } else if self.contains(Capability::LiteralMinus) {
            LiteralMode::Minus
        } else {
            LiteralMode::Sync
        }
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
                    log::trace!("unknown auth capability {cow}");
                }
            },
            _ => {
                unreachable!("tried inserting non-auth capability in auth_capabilities");
            }
        }
    }

    pub fn contains(&self, other: AuthCapability) -> bool {
        self.capabilities.contains(other)
    }
}

#[cfg(test)]
mod tests {
    use std::borrow::Cow;

    use rstest::*;

    use super::*;

    #[rstest]
    #[case("APPENDLIMIT=35651584", Some(35_651_584))]
    #[case("APPENDLIMIT", None)]
    #[case("APPENDLIMIT=", None)]
    #[case("APPENDLIMIT=0", None)]
    #[case("APPENDLIMIT=-1", None)]
    fn test_insert_append_limit(#[case] atom: &'static str, #[case] expected: Option<u64>) {
        let mut capabilities = Capabilities::default();

        capabilities.insert(&imap_proto::Capability::Atom(Cow::Borrowed(atom)));

        assert_eq!(capabilities.append_limit().map(NonZeroU64::get), expected);
    }
}
