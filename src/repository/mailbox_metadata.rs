use std::fmt::Display;

use thiserror::Error;

use crate::repository::{ModSeq, UidValidity};

/// Mailbox state that must match the server to continue syncing
#[derive(Debug)]
pub struct MailboxMetadata {
    uid_validity: UidValidity,
    highest_modseq: ModSeq,
}

impl MailboxMetadata {
    /// `UIDVALIDITY` of the mailbox the local mails belong to
    pub fn uid_validity(&self) -> UidValidity {
        self.uid_validity
    }

    /// Highest `MODSEQ` synced from the server
    pub fn highest_modseq(&self) -> ModSeq {
        self.highest_modseq
    }
}

/// Collects the fields of [`MailboxMetadata`]
#[derive(Default, Debug)]
pub struct MailboxMetadataBuilder {
    uid_validity: Option<UidValidity>,
    highest_modseq: Option<ModSeq>,
}

impl MailboxMetadataBuilder {
    /// Fails if any field is missing
    pub fn build(self) -> Result<MailboxMetadata, MailboxMetadataBuilderError> {
        match (self.uid_validity, self.highest_modseq) {
            (Some(uid_validity), Some(highest_modseq)) => Ok(MailboxMetadata {
                uid_validity,
                highest_modseq,
            }),
            (uid_validity_opt, highest_modseq_opt) => {
                let mut missing_fields = Vec::new();
                if uid_validity_opt.is_none() {
                    missing_fields.push("uid_validity".into());
                }
                if highest_modseq_opt.is_none() {
                    missing_fields.push("highest_modseq".into());
                }

                Err(MailboxMetadataBuilderError { missing_fields })
            }
        }
    }
    /// Sets `UIDVALIDITY`
    pub fn uid_validity(&mut self, uid_validity: UidValidity) {
        self.uid_validity = Some(uid_validity);
    }

    /// Sets the highest `MODSEQ`
    pub fn highest_modseq(&mut self, highest_modseq: ModSeq) {
        self.highest_modseq = Some(highest_modseq);
    }
}

/// Error for a [`MailboxMetadata`] that lacks fields
#[derive(Debug, Error)]
pub struct MailboxMetadataBuilderError {
    missing_fields: Vec<String>,
}

impl Display for MailboxMetadataBuilderError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        "Missing fields: ".fmt(f)?;
        for field in &self.missing_fields {
            field.fmt(f)?;
        }

        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use assertables::*;
    use rstest::*;

    use super::*;

    #[fixture]
    fn uid_validity() -> UidValidity {
        assert_ok!(UidValidity::try_from(4))
    }

    #[fixture]
    fn highest_modseq() -> ModSeq {
        assert_ok!(ModSeq::try_from(9))
    }

    #[rstest]
    fn test_mailbox_builder_is_correct(uid_validity: UidValidity, highest_modseq: ModSeq) {
        let mut builder = MailboxMetadataBuilder::default();
        builder.uid_validity(uid_validity);
        builder.highest_modseq(highest_modseq);

        let result = assert_ok!(builder.build());
        assert_eq!(result.uid_validity(), uid_validity);
        assert_eq!(result.highest_modseq(), highest_modseq);
    }

    #[rstest]
    fn test_mailbox_builder_errors_on_missing_field(
        uid_validity: UidValidity,
        highest_modseq: ModSeq,
    ) {
        let builder = MailboxMetadataBuilder::default();
        let err = assert_err!(builder.build());
        assert_in!("uid_validity".into(), err.missing_fields);
        assert_in!("highest_modseq".into(), err.missing_fields);
        let mut builder = MailboxMetadataBuilder::default();
        builder.highest_modseq(highest_modseq);
        let err = assert_err!(builder.build());
        assert_in!("uid_validity".into(), err.missing_fields);
        let mut builder = MailboxMetadataBuilder::default();
        builder.uid_validity(uid_validity);
        let err = assert_err!(builder.build());
        assert_in!("highest_modseq".into(), err.missing_fields);
    }

    #[rstest]
    fn test_builder_error_shows_missing_fields() {
        let builder = MailboxMetadataBuilder::default();
        let error = assert_err!(builder.build());
        let stringified = error.to_string();
        assert_contains!(stringified, "uid_validity");
        assert_contains!(stringified, "highest_modseq");
    }
}
