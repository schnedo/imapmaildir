pub mod fixtures;

use std::collections::HashSet;

use assertables::*;
use rstest::*;

use crate::fixtures::*;

#[rstest]
#[tokio::test]
#[awt]
async fn test_recieve_new_remote_mail(#[future] mail_setup: MailSetup) {
    let client_mail = mail_setup.client_mail();
    let client_mailbox = client_mail.mailbox("INBOX");
    let initial_client_mails: HashSet<_> = client_mailbox.mails().await.into_iter().collect();
    let server_mailbox = mail_setup.server_mail().mailbox("INBOX");

    let (handle, mut call_rx) = mail_setup.sync_continuous("INBOX").await;
    tokio::task::yield_now().await;

    let content = b"flksajflkajf";
    server_mailbox.add_mail(content);
    assert_some!(call_rx.recv().await);
    server_mailbox.add_mail(content);
    assert_some!(call_rx.recv().await);
    handle.abort();
    assert_none!(call_rx.recv().await);
    let client_mails: HashSet<_> = client_mailbox.mails().await.into_iter().collect();
    let new_mails: Vec<_> = client_mails.difference(&initial_client_mails).collect();
    assert_len_eq_x!(&new_mails, 2);
    for mail in new_mails {
        assert_eq!(content, mail.content());
    }
}
