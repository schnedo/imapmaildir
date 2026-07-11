pub mod fixtures;

use std::collections::HashSet;

use assertables::*;
use rstest::*;

use crate::fixtures::*;

#[rstest]
#[tokio::test]
#[awt]
async fn test_syncing_new_mail_works(#[future] mail_setup: MailSetup) {
    let client_mail = mail_setup.client_mail();
    let client_mailbox = client_mail.mailbox("INBOX");
    let initial_client_mails: HashSet<_> = client_mailbox.mails().await.into_iter().collect();
    let server_mailbox = mail_setup.server_mail().mailbox("INBOX");
    let (handle, mut call_rx) = mail_setup.sync_continuous("INBOX").await;
    let content = b"as";
    server_mailbox.add_mail(content);
    assert_some!(call_rx.recv().await);

    let content = b"flksajflkajf";
    server_mailbox.add_mail(content);
    assert_some!(call_rx.recv().await);
    handle.abort();
    assert_none!(call_rx.recv().await);

    let client_mails: HashSet<_> = client_mailbox.mails().await.into_iter().collect();
    let new_mails: HashSet<_> = client_mails
        .difference(&initial_client_mails)
        .map(MailFile::content)
        .collect();
    assert_contains!(new_mails, content.as_slice());
    assert_len_eq_x!(new_mails, 2);
}
