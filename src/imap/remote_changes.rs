use crate::{
    imap::{RemoteMailMetadata, SelectedClient},
    repository::{MailboxMetadata, SequenceSet},
};

/// Changes on the server since the last sync
pub struct RemoteChanges {
    pub updates: Vec<RemoteMailMetadata>,
    pub deletions: Option<SequenceSet>,
}

/// Selected mailbox together with its state and the remote changes
pub struct Selection {
    //todo: remove pub and use getters instead
    pub client: SelectedClient,
    pub mailbox_data: MailboxMetadata,
    pub remote_changes: RemoteChanges,
}
