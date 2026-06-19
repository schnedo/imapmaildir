use crate::{
    imap::{RemoteMail, RemoteMailMetadata},
    repository::{ModSeq, SequenceSet, Uid},
};

pub enum Task {
    NewMail(RemoteMail),
    Delete(SequenceSet),
    HighestModSeq(ModSeq),
    UpdateFlags(RemoteMailMetadata),
    UpdateModseq(Uid, ModSeq),
    Shutdown,
}
