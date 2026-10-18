use std::collections::HashMap;

use crossfire::oneshot::TxOneshot;
use enumflags2::BitFlags;

use crate::{
    imap::{RemoteMail, RemoteMailMetadata},
    maildir::{FlagChange, NewLocalMailMetadata},
    repository::{Flag, ModSeq, SequenceSet, Uid},
};

pub enum Task {
    NewMail(RemoteMail),
    Delete(SequenceSet),
    UpdateFlags(RemoteMailMetadata),
    SetHighestModseq(ModSeq),
    CompleteInitialSync(ModSeq, TxOneshot<()>),
    ApplyRemoteUpdates(Vec<RemoteMailMetadata>, TxOneshot<Vec<Uid>>),
    AddSynced(Vec<(NewLocalMailMetadata, Uid)>),
    ApplyFlagChanges(Vec<Uid>, FlagChange),
    Flags(Vec<Uid>, TxOneshot<HashMap<Uid, BitFlags<Flag>>>),
    HighestModseq(TxOneshot<ModSeq>),
}
