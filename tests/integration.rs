mod fixtures;

use std::collections::HashSet;

use assertables::*;
use imapmaildir::Client;
use imapmaildir::Syncer;
use rstest::*;

use crate::fixtures::*;

#[rstest]
#[tokio::test]
#[awt]
async fn test_no_updates_does_nothing(#[future] mail_setup: MailSetup) {
    let config = mail_setup.config();
    let client_mail = mail_setup.client_mail();
    let client_mailbox = client_mail.mailbox("INBOX");
    let initial_client_mails: HashSet<_> = client_mailbox.mails().collect();
    let server_mailbox = mail_setup.server_mail().mailbox("INBOX");
    let initial_server_mails: HashSet<_> = server_mailbox.mails().collect();

    let client = Client::login(config.connection(), config.auth()).await;
    Syncer::sync(
        "INBOX",
        config.maildir_base_path(),
        config.state_dir(),
        client,
    )
    .await;
    let container = mail_setup.container();
    assert_ok!(container.stop().await);

    let client_mails = client_mailbox.mails().collect();
    assert_eq!(initial_client_mails, client_mails);
    let server_mails = server_mailbox.mails().collect();
    assert_eq!(initial_server_mails, server_mails);
}

#[rstest]
#[tokio::test]
#[awt]
async fn test_initial_sync_works(#[future] mail_setup: MailSetup) {
    let config = mail_setup.config();
    let client_mail = mail_setup.client_mail();
    client_mail.wipe();
    let server_mailbox = mail_setup.server_mail().mailbox("INBOX");
    let initial_server_mails: HashSet<_> = server_mailbox.mails().collect();

    let client = Client::login(config.connection(), config.auth()).await;
    Syncer::sync(
        "INBOX",
        config.maildir_base_path(),
        config.state_dir(),
        client,
    )
    .await;
    let container = mail_setup.container();
    assert_ok!(container.stop().await);

    let client_mailbox = client_mail.mailbox("INBOX");
    let client_mails = client_mailbox.mails().collect();
    let server_mails = server_mailbox.mails().collect();
    assert_eq!(server_mails, client_mails);
    assert_eq!(initial_server_mails, server_mails);
}

#[rstest]
#[tokio::test]
#[awt]
async fn test_adding_flag_works(#[future] mail_setup: MailSetup) {
    let config = mail_setup.config();
    let client_mail = mail_setup.client_mail();
    let client_mailbox = client_mail.mailbox("INBOX");
    let mut mail = assert_some!(client_mailbox.mails().next());
    assert!(mail.add_flag('D'));
    assert_any!(client_mailbox.mails(), |mail: MailFile| mail.has_flag('D'));

    let client = Client::login(config.connection(), config.auth()).await;
    Syncer::sync(
        "INBOX",
        config.maildir_base_path(),
        config.state_dir(),
        client,
    )
    .await;
    let container = mail_setup.container();
    assert_ok!(container.stop().await);

    let server_mailbox = mail_setup.server_mail().mailbox("INBOX");
    let client_mails: HashSet<_> = client_mailbox.mails().collect();
    let server_mails: HashSet<_> = server_mailbox.mails().collect();
    assert_eq!(server_mails, client_mails);
    assert_any!(client_mailbox.mails(), |mail: MailFile| mail.has_flag('D'));
    assert_any!(server_mailbox.mails(), |mail: MailFile| mail.has_flag('D'));
}

#[rstest]
#[tokio::test]
#[awt]
async fn test_syncing_added_flag_works(#[future] mail_setup: MailSetup) {
    let config = mail_setup.config();
    let server_mail = mail_setup.server_mail();
    let server_mailbox = server_mail.mailbox("INBOX");
    let mut mail = assert_some!(server_mailbox.mails().next());
    assert!(mail.add_flag('D'));
    assert_any!(server_mailbox.mails(), |mail: MailFile| mail.has_flag('D'));

    let client = Client::login(config.connection(), config.auth()).await;
    Syncer::sync(
        "INBOX",
        config.maildir_base_path(),
        config.state_dir(),
        client,
    )
    .await;
    let container = mail_setup.container();
    assert_ok!(container.stop().await);

    let client_mail = mail_setup.client_mail();
    let client_mailbox = client_mail.mailbox("INBOX");
    let client_mails: HashSet<_> = client_mailbox.mails().collect();
    let server_mails: HashSet<_> = server_mailbox.mails().collect();
    assert_eq!(server_mails, client_mails);
    assert_any!(client_mailbox.mails(), |mail: MailFile| mail.has_flag('D'));
    assert_any!(server_mailbox.mails(), |mail: MailFile| mail.has_flag('D'));
}
