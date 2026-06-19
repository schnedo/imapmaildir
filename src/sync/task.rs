use crate::{
    imap::{RemoteMail, RemoteMailMetadata},
    repository::{ModSeq, SequenceSet, Uid},
};

pub enum Task {
    NewMail(RemoteMail),
    Delete(SequenceSet),
    UpdateFlags(RemoteMailMetadata),
    UpdateModseq(Uid, ModSeq),
    Shutdown,
}
