use crossfire::oneshot::TxOneshot;

use crate::{
    imap::{RemoteMail, RemoteMailMetadata},
    maildir::{FlagChange, NewLocalMailMetadata},
    repository::{ModSeq, SequenceSet, Uid},
};

pub enum Task {
    NewMail(RemoteMail),
    Delete(SequenceSet),
    UpdateFlags(RemoteMailMetadata),
    SetHighestModseq(ModSeq),
    CompleteInitialSync(ModSeq, TxOneshot<()>),
    ApplyRemoteUpdates(Vec<RemoteMailMetadata>, TxOneshot<Vec<Uid>>),
    AddSynced(NewLocalMailMetadata, Uid),
    ApplyFlagChange(Uid, FlagChange),
    HighestModseq(TxOneshot<ModSeq>),
}
