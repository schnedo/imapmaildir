use crate::{
    imap::SelectedClient,
    repository::{MailboxMetadata, RemoteMailMetadata, SequenceSet},
};

pub struct RemoteChanges {
    pub updates: Vec<RemoteMailMetadata>,
    pub deletions: Option<SequenceSet>,
}

pub struct Selection {
    //todo: remove pub and use getters instead
    pub client: SelectedClient,
    pub mailbox_data: MailboxMetadata,
    pub remote_changes: RemoteChanges,
}
