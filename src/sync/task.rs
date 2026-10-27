use std::collections::HashMap;

use crossfire::oneshot::TxOneshot;
use enumflags2::BitFlags;

use crate::{
    imap::{RemoteMail, RemoteMailMetadata},
    maildir::{FlagChange, LocalChanges, LocalMail, NewLocalMailMetadata, OtherFlags},
    notmuch::Revision,
    repository::{Flag, KeywordSource, Keywords, ModSeq, SequenceSet, Uid},
};

pub enum Task {
    NewMail(RemoteMail),
    Delete(SequenceSet),
    UpdateFlags(RemoteMailMetadata),
    SetHighestModseq(ModSeq),
    SetKeywordSource(KeywordSource),
    CompleteInitialSync(ModSeq, KeywordSource, TxOneshot<()>),
    ApplyRemoteUpdates(Vec<RemoteMailMetadata>, TxOneshot<Vec<Uid>>),
    AddSynced(Vec<(NewLocalMailMetadata, Uid)>),
    ApplyFlagChanges(Vec<(Vec<Uid>, FlagChange)>),
    UpdateOtherFlags(Vec<(Uid, OtherFlags)>),
    OpenMail(NewLocalMailMetadata, TxOneshot<Option<LocalMail>>),
    Flags(
        Vec<Uid>,
        TxOneshot<HashMap<Uid, (BitFlags<Flag>, Keywords)>>,
    ),
    HighestModseq(TxOneshot<ModSeq>),
    UidsNotIn(Vec<Uid>, TxOneshot<Vec<Uid>>),
    DetectChanges(TxOneshot<LocalChanges>),
    DetectTagChanges(TxOneshot<LocalChanges>),
    CompleteTagChanges(Revision),
}
