use crate::{
    imap::{RemoteMail, RemoteMailMetadata},
    repository::{ModSeq, SequenceSet},
};

pub enum Task {
    NewMail(RemoteMail),
    Delete(SequenceSet),
    UpdateFlags(RemoteMailMetadata),
    SetHighestModseq(ModSeq),
}
