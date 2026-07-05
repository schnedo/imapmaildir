pub mod fixtures;

use std::collections::HashSet;

use assertables::*;
use imapmaildir::Client;
use imapmaildir::Syncer;
use rstest::*;
use tokio::sync::mpsc;

use crate::fixtures::*;

#[rstest]
#[tokio::test]
#[awt]
async fn test_recieve_new_remote_mail(#[future] mail_setup: MailSetup) {
    let (call_tx, mut call_rx) = mpsc::channel(1);
    let config = mail_setup.config();
    let maildir_base_path = config.maildir_base_path().clone();
    let state_dir = config.state_dir().clone();
    let idle_timout = config.idle_timout();
    let client = Client::login(config.connection(), config.auth()).await;

    let client_mail = mail_setup.client_mail();
    let client_mailbox = client_mail.mailbox("INBOX");
    let initial_client_mails: HashSet<_> = client_mailbox.mails().await.into_iter().collect();
    let server_mailbox = mail_setup.server_mail().mailbox("INBOX");

    let handle = tokio::spawn(async move {
        Syncer::sync_continuously(
            "INBOX",
            &maildir_base_path,
            &state_dir,
            client,
            idle_timout,
            move || {
                let call = call_tx.clone();
                tokio::spawn(async move {
                    assert_ok!(call.send(()).await);
                });
            },
        )
        .await;
    });

    let content = b"flksajflkajf";
    server_mailbox.add_mail(content);
    assert_some!(call_rx.recv().await);
    handle.abort();
    assert_none!(call_rx.recv().await);
    let client_mails: HashSet<_> = client_mailbox.mails().await.into_iter().collect();
    let new_mails: Vec<_> = client_mails.difference(&initial_client_mails).collect();
    assert_len_eq_x!(&new_mails, 1);
    for mail in new_mails {
        assert_eq!(content, mail.content());
    }
}
