use crate::{
    imap::RemoteMail,
    repository::{ModSeq, SequenceSet},
};

pub enum Task {
    NewMail(RemoteMail),
    Delete(SequenceSet),
    HighestModSeq(ModSeq),
    UpdateModseq(ModSeq),
    Shutdown(),
}
