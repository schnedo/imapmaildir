use crate::{
    imap::RemoteMail,
    repository::{ModSeq, SequenceSet, Uid},
};

pub enum Task {
    NewMail(RemoteMail),
    Delete(SequenceSet),
    HighestModSeq(ModSeq),
    UpdateModseq(Uid, ModSeq),
    Shutdown(),
}
