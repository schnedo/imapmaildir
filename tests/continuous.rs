pub mod fixtures;

use std::collections::HashSet;

use assertables::*;
use crossfire::{AsyncRx, mpsc::Array};
use rstest::*;

use crate::fixtures::*;

async fn wait_for_mail(
    client_mailbox: &impl Maildir,
    call_rx: &AsyncRx<Array<()>>,
    content: &[u8],
) {
    while !client_mailbox
        .mails()
        .await
        .iter()
        .any(|mail| mail.content() == content)
    {
        assert_ok!(call_rx.recv().await);
    }
}

#[rstest]
#[compio::test]
#[awt]
async fn test_syncing_new_mail_works(#[future] mail_setup: MailSetup) {
    let client_mail = mail_setup.client_mail();
    let client_mailbox = client_mail.mailbox("INBOX");
    let initial_client_mails: HashSet<_> = client_mailbox.mails().await.into_iter().collect();
    let server_mailbox = mail_setup.server_mail().mailbox("INBOX");
    let (handle, call_rx) = mail_setup.sync_continuous("INBOX").await;
    let first_content = b"as";
    server_mailbox.add_mail(first_content);
    wait_for_mail(&client_mailbox, &call_rx, first_content).await;

    let content = b"flksajflkajf";
    server_mailbox.add_mail(content);
    wait_for_mail(&client_mailbox, &call_rx, content).await;
    drop(handle);
    while call_rx.recv().await.is_ok() {}

    let client_mails: HashSet<_> = client_mailbox.mails().await.into_iter().collect();
    let new_mails: HashSet<_> = client_mails
        .difference(&initial_client_mails)
        .map(MailFile::content)
        .collect();
    assert_contains!(new_mails, first_content.as_slice());
    assert_contains!(new_mails, content.as_slice());
    assert_len_eq_x!(new_mails, 2);
}
