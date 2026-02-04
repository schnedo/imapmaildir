use crate::repository::{ModSeq, UidValidity};

#[derive(Debug)]
pub struct MailboxMetadata {
    uid_validity: UidValidity,
    highest_modseq: ModSeq,
}

impl MailboxMetadata {
    pub fn uid_validity(&self) -> UidValidity {
        self.uid_validity
    }

    pub fn highest_modseq(&self) -> ModSeq {
        self.highest_modseq
    }
}

#[derive(Default, Debug)]
pub struct MailboxMetadataBuilder {
    uid_validity: Option<UidValidity>,
    highest_modseq: Option<ModSeq>,
}

impl MailboxMetadataBuilder {
    pub fn build(self) -> Result<MailboxMetadata, &'static str> {
        match (self.uid_validity, self.highest_modseq) {
            (Some(uid_validity), Some(highest_modseq)) => Ok(MailboxMetadata {
                uid_validity,
                highest_modseq,
            }),
            _ => Err("not all required fields present"),
        }
    }
    pub fn uid_validity(&mut self, uid_validity: UidValidity) {
        self.uid_validity = Some(uid_validity);
    }

    pub fn highest_modseq(&mut self, highest_modseq: ModSeq) {
        self.highest_modseq = Some(highest_modseq);
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
        assert_err!(builder.build());
        let mut builder = MailboxMetadataBuilder::default();
        builder.highest_modseq(highest_modseq);
        assert_err!(builder.build());
        let mut builder = MailboxMetadataBuilder::default();
        builder.uid_validity(uid_validity);
        assert_err!(builder.build());
    }
}
