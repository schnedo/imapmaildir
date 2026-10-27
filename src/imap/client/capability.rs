use std::num::NonZeroU64;

use enumflags2::{BitFlags, bitflags};

use crate::{imap::client::encoding::LiteralMode, repository::KeywordSource};

#[bitflags]
#[repr(u16)]
#[derive(Copy, Clone, Debug)]
// todo: check https://datatracker.ietf.org/doc/html/rfc5550#section-12 and update readme regarding
// compatibility to lemonade mail stores
pub enum Capability {
    Condstore, // rfc7162
    Enable,    // rfc5161
    GmailExt,  // https://developers.google.com/workspace/gmail/imap/imap-extensions
    Idle,      // rfc2177
    Imap4rev1,
    LiteralMinus, // rfc7888
    LiteralPlus,  // rfc7888
    Multiappend,  // rfc3502
    QResync,      // rfc7162
    SaslIr,       // rfc4959
    UidPlus,      // rfc4315
}
#[derive(Debug, Default)]
pub struct Capabilities {
    supported: BitFlags<Capability>,
    /// Whether Gmail labels are used as keywords, as they are synced
    gmail_labels: bool,
    /// Maximum size of appended mails in octets, if the server announces one for all mailboxes
    /// (RFC 7889)
    append_limit: Option<NonZeroU64>,
}

impl Capabilities {
    pub fn insert(&mut self, capability: &imap_proto::Capability) {
        match capability {
            imap_proto::Capability::Imap4rev1 => {
                self.supported.insert(Capability::Imap4rev1);
            }
            imap_proto::Capability::Auth(_) => {
                unreachable!("tried inserting auth capability in capabilities");
            }
            imap_proto::Capability::Atom(cow) => match cow.as_ref() {
                "CONDSTORE" => {
                    self.supported.insert(Capability::Condstore);
                }
                "ENABLE" => {
                    self.supported.insert(Capability::Enable);
                }
                "X-GM-EXT-1" => {
                    self.supported.insert(Capability::GmailExt);
                }
                "IDLE" => {
                    self.supported.insert(Capability::Idle);
                }
                "LITERAL-" => {
                    self.supported.insert(Capability::LiteralMinus);
                }
                "LITERAL+" => {
                    self.supported.insert(Capability::LiteralPlus);
                }
                "MULTIAPPEND" => {
                    self.supported.insert(Capability::Multiappend);
                }
                "QRESYNC" => {
                    self.supported.insert(Capability::QResync);
                }
                "SASL-IR" => {
                    self.supported.insert(Capability::SaslIr);
                }
                "UIDPLUS" => {
                    self.supported.insert(Capability::UidPlus);
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
        self.supported.contains(other)
    }

    /// Replaces all capabilities with those of a CAPABILITY response after login, as capability
    /// lists are complete (RFC 3501 7.2.1)
    pub fn replace(&mut self, caps: &[imap_proto::Capability]) {
        *self = Self {
            gmail_labels: self.gmail_labels,
            ..Self::default()
        };
        for cap in caps {
            match cap {
                imap_proto::Capability::Imap4rev1 | imap_proto::Capability::Atom(_) => {
                    self.insert(cap);
                }
                imap_proto::Capability::Auth(_) => {
                    log::warn!("unexpected capability respone {cap:?}");
                }
            }
        }
        log::trace!("updated capabilities to {self:?}");
    }

    pub fn append_limit(&self) -> Option<NonZeroU64> {
        self.append_limit
    }

    /// Uses Gmail's labels as keywords, if the server supports them. Fetching them otherwise
    /// costs extra round trips.
    pub fn use_gmail_labels(&mut self, enabled: bool) {
        self.gmail_labels = enabled;
    }

    /// Gmail keeps keywords as labels and reports them more completely via `X-GM-LABELS`
    pub fn keyword_source(&self) -> KeywordSource {
        if self.gmail_labels && self.contains(Capability::GmailExt) {
            KeywordSource::GmailLabels
        } else {
            KeywordSource::Flags
        }
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
    OAuthBearer, // rfc7628
    Plain,
    XOAuth2,
}
#[derive(Debug, Default)]
pub struct AuthCapabilities {
    supported: BitFlags<AuthCapability>,
}

impl AuthCapabilities {
    pub fn insert(&mut self, capability: &imap_proto::Capability) {
        match capability {
            imap_proto::Capability::Auth(cow) => match cow.as_ref() {
                "OAUTHBEARER" => {
                    self.supported.insert(AuthCapability::OAuthBearer);
                }
                "PLAIN" => {
                    self.supported.insert(AuthCapability::Plain);
                }
                "XOAUTH2" => {
                    self.supported.insert(AuthCapability::XOAuth2);
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
        self.supported.contains(other)
    }
}

#[cfg(test)]
mod tests {
    use std::borrow::Cow;

    use rstest::*;

    use super::*;

    #[test]
    fn test_oauth2_mechanisms_and_sasl_ir_are_parsed() {
        let mut auth_capabilities = AuthCapabilities::default();
        auth_capabilities.insert(&imap_proto::Capability::Auth(Cow::Borrowed("XOAUTH2")));
        auth_capabilities.insert(&imap_proto::Capability::Auth(Cow::Borrowed("OAUTHBEARER")));
        let mut capabilities = Capabilities::default();
        capabilities.insert(&imap_proto::Capability::Atom(Cow::Borrowed("SASL-IR")));

        assert!(auth_capabilities.contains(AuthCapability::XOAuth2));
        assert!(auth_capabilities.contains(AuthCapability::OAuthBearer));
        assert!(!auth_capabilities.contains(AuthCapability::Plain));
        assert!(capabilities.contains(Capability::SaslIr));
    }

    #[test]
    fn test_gmail_extension_selects_labels_as_keyword_source() {
        let mut capabilities = Capabilities::default();
        assert_eq!(capabilities.keyword_source(), KeywordSource::Flags);

        capabilities.insert(&imap_proto::Capability::Atom(Cow::Borrowed("X-GM-EXT-1")));
        assert_eq!(capabilities.keyword_source(), KeywordSource::Flags);

        capabilities.use_gmail_labels(true);
        capabilities.replace(&[imap_proto::Capability::Atom(Cow::Borrowed("X-GM-EXT-1"))]);

        assert_eq!(capabilities.keyword_source(), KeywordSource::GmailLabels);
    }

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
