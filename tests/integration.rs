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
    let initial_client_mails: HashSet<_> = client_mailbox.mails().await.into_iter().collect();
    let server_mailbox = mail_setup.server_mail().mailbox("INBOX");
    let initial_server_mails: HashSet<_> = server_mailbox.mails().await.into_iter().collect();

    let client = Client::login(config.connection(), config.auth()).await;
    Syncer::sync(
        "INBOX",
        config.maildir_base_path(),
        config.state_dir(),
        client,
    )
    .await;

    let client_mails = client_mailbox.mails().await.into_iter().collect();
    assert_eq!(initial_client_mails, client_mails);
    let server_mails = server_mailbox.mails().await.into_iter().collect();
    assert_eq!(initial_server_mails, server_mails);
    assert_eq!(server_mails, client_mails);
}

#[rstest]
#[tokio::test]
#[awt]
async fn test_initial_sync_works(#[future] mail_setup: MailSetup) {
    let config = mail_setup.config();
    let client_mail = mail_setup.client_mail();
    client_mail.wipe();
    let server_mailbox = mail_setup.server_mail().mailbox("INBOX");
    let initial_server_mails: HashSet<_> = server_mailbox.mails().await.into_iter().collect();

    let client = Client::login(config.connection(), config.auth()).await;
    Syncer::sync(
        "INBOX",
        config.maildir_base_path(),
        config.state_dir(),
        client,
    )
    .await;

    let client_mailbox = client_mail.mailbox("INBOX");
    let client_mails = client_mailbox.mails().await.into_iter().collect();
    let server_mails = server_mailbox.mails().await.into_iter().collect();
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
    let mut client_mails = client_mailbox.mails().await;
    let mail = assert_some!(client_mails.first_mut());
    assert!(!mail.has_flag('D'));
    assert!(mail.add_flag('D'));
    assert!(mail.has_flag('D'));

    let client = Client::login(config.connection(), config.auth()).await;
    Syncer::sync(
        "INBOX",
        config.maildir_base_path(),
        config.state_dir(),
        client,
    )
    .await;

    let client_mails = client_mailbox.mails().await;
    let mail = assert_some!(client_mails.first());
    assert!(mail.has_flag('D'));
    let server_mailbox = mail_setup.server_mail().mailbox("INBOX");
    let client_mails: HashSet<_> = client_mailbox.mails().await.into_iter().collect();
    let server_mails: HashSet<_> = server_mailbox.mails().await.into_iter().collect();
    assert_eq!(server_mails, client_mails);
}

#[rstest]
#[tokio::test]
#[awt]
async fn test_syncing_added_flag_works(#[future] mail_setup: MailSetup) {
    let config = mail_setup.config();
    let server_mail = mail_setup.server_mail();
    let server_mailbox = server_mail.mailbox("INBOX");
    let mut server_mails = server_mailbox.mails().await;
    let mail = assert_some!(server_mails.first_mut());
    assert!(!mail.has_flag('D'));
    assert!(mail.add_flag('D'));
    assert!(mail.has_flag('D'));

    let client = Client::login(config.connection(), config.auth()).await;
    Syncer::sync(
        "INBOX",
        config.maildir_base_path(),
        config.state_dir(),
        client,
    )
    .await;

    let client_mail = mail_setup.client_mail();
    let client_mailbox = client_mail.mailbox("INBOX");
    let client_mails = client_mailbox.mails().await;
    let mail = assert_some!(client_mails.first());
    assert!(!mail.has_flag('S'));
    let client_mails: HashSet<_> = client_mailbox.mails().await.into_iter().collect();
    let server_mails: HashSet<_> = server_mailbox.mails().await.into_iter().collect();
    assert_eq!(server_mails, client_mails);
}

#[rstest]
#[tokio::test]
#[awt]
async fn test_removing_flag_works(#[future] mail_setup: MailSetup) {
    let config = mail_setup.config();
    let client_mail = mail_setup.client_mail();
    let client_mailbox = client_mail.mailbox("INBOX");
    let mut mail = assert_some!(client_mailbox.mail_with_flag().await);
    assert!(mail.has_flag('S'));
    assert!(mail.remove_flag('S'));
    assert!(!mail.has_flag('S'));

    let client = Client::login(config.connection(), config.auth()).await;
    Syncer::sync(
        "INBOX",
        config.maildir_base_path(),
        config.state_dir(),
        client,
    )
    .await;

    assert_none!(client_mailbox.mail_with_flag().await);
    let server_mailbox = mail_setup.server_mail().mailbox("INBOX");
    let client_mails: HashSet<_> = client_mailbox.mails().await.into_iter().collect();
    let server_mails: HashSet<_> = server_mailbox.mails().await.into_iter().collect();
    assert_eq!(server_mails, client_mails);
}

#[rstest]
#[tokio::test]
#[awt]
async fn test_syncing_removed_flag_works(#[future] mail_setup: MailSetup) {
    let config = mail_setup.config();
    let server_mail = mail_setup.server_mail();
    let server_mailbox = server_mail.mailbox("INBOX");
    let mut mail = assert_some!(server_mailbox.mail_with_flag().await);
    assert!(mail.has_flag('S'));
    assert!(mail.remove_flag('S'));
    assert!(!mail.has_flag('S'));

    let client = Client::login(config.connection(), config.auth()).await;
    Syncer::sync(
        "INBOX",
        config.maildir_base_path(),
        config.state_dir(),
        client,
    )
    .await;

    let client_mail = mail_setup.client_mail();
    let client_mailbox = client_mail.mailbox("INBOX");
    assert_none!(client_mailbox.mail_with_flag().await);
    let client_mails: HashSet<_> = client_mailbox.mails().await.into_iter().collect();
    let server_mails: HashSet<_> = server_mailbox.mails().await.into_iter().collect();
    assert_eq!(server_mails, client_mails);
}

#[rstest]
#[tokio::test]
#[awt]
async fn test_adding_new_mail_works(#[future] mail_setup: MailSetup) {
    let config = mail_setup.config();
    let client_mail = mail_setup.client_mail();
    let client_mailbox = client_mail.mailbox("INBOX");
    let client_mails: HashSet<_> = client_mailbox.mails().await.into_iter().collect();
    let server_mailbox = mail_setup.server_mail().mailbox("INBOX");
    let server_mails: HashSet<_> = server_mailbox.mails().await.into_iter().collect();
    assert_eq!(server_mails, client_mails);
    let content = String::from("new");
    client_mailbox.add_mail(content.as_bytes(), "S");

    let client = Client::login(config.connection(), config.auth()).await;
    Syncer::sync(
        "INBOX",
        config.maildir_base_path(),
        config.state_dir(),
        client,
    )
    .await;

    let client_mails: HashSet<_> = client_mailbox.mails().await.into_iter().collect();
    let server_mails: HashSet<_> = server_mailbox.mails().await.into_iter().collect();
    assert_eq!(server_mails, client_mails);
}
