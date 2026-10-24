pub mod fixtures;

use std::collections::HashSet;

use assertables::*;
use rstest::*;

use crate::fixtures::*;

#[rstest]
#[compio::test]
#[awt]
async fn test_no_updates_does_nothing(#[future] mail_setup: MailSetup) {
    let client_mail = mail_setup.client_mail();
    let client_mailbox = client_mail.mailbox("INBOX");
    let initial_client_mails: HashSet<_> = client_mailbox.mails().await.into_iter().collect();
    let server_mailbox = mail_setup.server_mail().mailbox("INBOX");
    let initial_server_mails: HashSet<_> = server_mailbox.mails().await.into_iter().collect();

    mail_setup.sync("INBOX").await;

    let client_mails = client_mailbox.mails().await.into_iter().collect();
    assert_eq!(initial_client_mails, client_mails);
    let server_mails = server_mailbox.mails().await.into_iter().collect();
    assert_eq!(initial_server_mails, server_mails);
    assert_eq!(server_mails, client_mails);
}

#[rstest]
#[compio::test]
#[awt]
async fn test_initial_sync_works(#[future] mail_setup: MailSetup) {
    let client_mail = mail_setup.client_mail();
    client_mail.wipe();
    let server_mailbox = mail_setup.server_mail().mailbox("INBOX");
    let initial_server_mails: HashSet<_> = server_mailbox.mails().await.into_iter().collect();

    mail_setup.sync("INBOX").await;

    let client_mailbox = client_mail.mailbox("INBOX");
    let client_mails = client_mailbox.mails().await.into_iter().collect();
    let server_mails = server_mailbox.mails().await.into_iter().collect();
    assert_eq!(server_mails, client_mails);
    assert_eq!(initial_server_mails, server_mails);
}

#[rstest]
#[compio::test]
#[awt]
async fn test_adding_flag_works(#[future] mail_setup: MailSetup) {
    let client_mail = mail_setup.client_mail();
    let client_mailbox = client_mail.mailbox("INBOX");
    let mut client_mails = client_mailbox.mails().await;
    let mail = assert_some!(client_mails.first_mut());
    assert!(!mail.has_flag('D'));
    assert!(mail.add_flag('D'));
    assert!(mail.has_flag('D'));

    mail_setup.sync("INBOX").await;

    let client_mails = client_mailbox.mails().await;
    let mail = assert_some!(client_mails.first());
    assert!(mail.has_flag('D'));
    let server_mailbox = mail_setup.server_mail().mailbox("INBOX");
    let client_mails: HashSet<_> = client_mailbox.mails().await.into_iter().collect();
    let server_mails: HashSet<_> = server_mailbox.mails().await.into_iter().collect();
    assert_eq!(server_mails, client_mails);
}

#[rstest]
#[compio::test]
#[awt]
async fn test_adding_and_removing_flags_of_same_mail_works(#[future] mail_setup: MailSetup) {
    let client_mail = mail_setup.client_mail();
    let client_mailbox = client_mail.mailbox("INBOX");
    let mut client_mails = client_mailbox.mails().await;
    let mail = assert_some!(client_mails.iter_mut().find(|mail| mail.has_flag('S')));
    assert!(!mail.has_flag('F'));
    assert!(mail.remove_flag('S'));
    assert!(mail.add_flag('F'));

    mail_setup.sync("INBOX").await;

    let server_mailbox = mail_setup.server_mail().mailbox("INBOX");
    let client_mails: HashSet<_> = client_mailbox.mails().await.into_iter().collect();
    let server_mails: HashSet<_> = server_mailbox.mails().await.into_iter().collect();
    assert_eq!(server_mails, client_mails);
    assert!(
        server_mails
            .iter()
            .any(|mail| mail.has_flag('F') && !mail.has_flag('S'))
    );
}

#[rstest]
#[compio::test]
#[awt]
async fn test_syncing_added_flag_works(#[future] mail_setup: MailSetup) {
    let server_mail = mail_setup.server_mail();
    let server_mailbox = server_mail.mailbox("INBOX");
    let mut server_mails = server_mailbox.mails().await;
    let mail = assert_some!(server_mails.first_mut());
    assert!(!mail.has_flag('D'));
    assert!(mail.add_flag('D'));
    assert!(mail.has_flag('D'));

    mail_setup.sync("INBOX").await;

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
#[compio::test]
#[awt]
async fn test_removing_flag_works(#[future] mail_setup: MailSetup) {
    let client_mail = mail_setup.client_mail();
    let client_mailbox = client_mail.mailbox("INBOX");
    let mut mail = assert_some!(client_mailbox.mail_with_flag().await);
    assert!(mail.has_flag('S'));
    assert!(mail.remove_flag('S'));
    assert!(!mail.has_flag('S'));

    mail_setup.sync("INBOX").await;

    assert_none!(client_mailbox.mail_with_flag().await);
    let server_mailbox = mail_setup.server_mail().mailbox("INBOX");
    let client_mails: HashSet<_> = client_mailbox.mails().await.into_iter().collect();
    let server_mails: HashSet<_> = server_mailbox.mails().await.into_iter().collect();
    assert_eq!(server_mails, client_mails);
}

#[rstest]
#[compio::test]
#[awt]
async fn test_syncing_removed_flag_works(#[future] mail_setup: MailSetup) {
    let server_mail = mail_setup.server_mail();
    let server_mailbox = server_mail.mailbox("INBOX");
    let mut mail = assert_some!(server_mailbox.mail_with_flag().await);
    assert!(mail.has_flag('S'));
    assert!(mail.remove_flag('S'));
    assert!(!mail.has_flag('S'));

    mail_setup.sync("INBOX").await;

    let client_mail = mail_setup.client_mail();
    let client_mailbox = client_mail.mailbox("INBOX");
    assert_none!(client_mailbox.mail_with_flag().await);
    let client_mails: HashSet<_> = client_mailbox.mails().await.into_iter().collect();
    let server_mails: HashSet<_> = server_mailbox.mails().await.into_iter().collect();
    assert_eq!(server_mails, client_mails);
}

#[rstest]
#[compio::test]
#[awt]
async fn test_adding_new_mail_works(#[future] mail_setup: MailSetup) {
    let client_mail = mail_setup.client_mail();
    let client_mailbox = client_mail.mailbox("INBOX");
    let client_mails: HashSet<_> = client_mailbox.mails().await.into_iter().collect();
    let server_mailbox = mail_setup.server_mail().mailbox("INBOX");
    let initial_server_mails: HashSet<_> = server_mailbox.mails().await.into_iter().collect();
    assert_eq!(initial_server_mails, client_mails);
    let content = String::from("new");
    client_mailbox.add_mail(content.as_bytes());

    mail_setup.sync("INBOX").await;

    let client_mails: HashSet<_> = client_mailbox.mails().await.into_iter().collect();
    let server_mails: HashSet<_> = server_mailbox.mails().await.into_iter().collect();
    assert_eq!(server_mails, client_mails);
    assert_ne!(initial_server_mails, server_mails);
}

#[rstest]
#[compio::test]
#[awt]
async fn test_adding_new_mail_larger_than_non_synchronizing_literal_limit_works(
    #[future] mail_setup: MailSetup,
) {
    let client_mail = mail_setup.client_mail();
    let client_mailbox = client_mail.mailbox("INBOX");
    let server_mailbox = mail_setup.server_mail().mailbox("INBOX");
    let initial_server_mails = server_mailbox.mails().await;
    // LITERAL- only allows non-synchronizing literals up to 4096 octets
    let cur = client_mail.dir().join("INBOX/cur");
    assert_ok!(std::fs::write(cur.join("large:2,S"), "large".repeat(1000)));
    assert_ok!(std::fs::write(cur.join("small:2,S"), "small"));

    mail_setup.sync("INBOX").await;

    let server_mails: HashSet<_> = server_mailbox.mails().await.into_iter().collect();
    assert_eq!(server_mails.len(), initial_server_mails.len() + 2);
    let client_mails: HashSet<_> = client_mailbox.mails().await.into_iter().collect();
    assert_eq!(server_mails, client_mails);
}

#[rstest]
#[compio::test]
#[awt]
async fn test_adding_more_new_mails_than_fit_in_a_batch_works(#[future] mail_setup: MailSetup) {
    let client_mail = mail_setup.client_mail();
    let client_mailbox = client_mail.mailbox("INBOX");
    let server_mailbox = mail_setup.server_mail().mailbox("INBOX");
    let initial_server_mails = server_mailbox.mails().await;
    let new_mails = 70;
    for i in 0..new_mails {
        let path = client_mail.dir().join(format!("INBOX/cur/new{i}:2,S"));
        assert_ok!(std::fs::write(path, format!("new {i}")));
    }

    mail_setup.sync("INBOX").await;

    let server_mails: HashSet<_> = server_mailbox.mails().await.into_iter().collect();
    assert_eq!(server_mails.len(), initial_server_mails.len() + new_mails);
    let client_mails: HashSet<_> = client_mailbox.mails().await.into_iter().collect();
    assert_eq!(server_mails, client_mails);
}

#[rstest]
#[compio::test]
#[awt]
async fn test_mail_rejected_by_server_is_skipped(#[future] mail_setup: MailSetup) {
    let client_mail = mail_setup.client_mail();
    let client_mailbox = client_mail.mailbox("INBOX");
    let server_mailbox = mail_setup.server_mail().mailbox("INBOX");
    let initial_server_mails: HashSet<_> = server_mailbox.mails().await.into_iter().collect();
    let rejected = client_mail.dir().join("INBOX/cur/empty:2,S");
    assert_ok!(std::fs::write(&rejected, b""));
    client_mailbox.add_mail(b"new");

    mail_setup.sync("INBOX").await;

    assert!(assert_ok!(rejected.try_exists()));
    let server_mails: HashSet<_> = server_mailbox.mails().await.into_iter().collect();
    assert_eq!(server_mails.len(), initial_server_mails.len() + 1);
    assert_ok!(std::fs::remove_file(&rejected));
    let client_mails: HashSet<_> = client_mailbox.mails().await.into_iter().collect();
    assert_eq!(server_mails, client_mails);
}

#[rstest]
#[compio::test]
#[awt]
async fn test_mail_exceeding_append_limit_is_skipped(#[future] mail_setup: MailSetup) {
    let client_mail = mail_setup.client_mail();
    let client_mailbox = client_mail.mailbox("INBOX");
    let server_mailbox = mail_setup.server_mail().mailbox("INBOX");
    let initial_server_mails: HashSet<_> = server_mailbox.mails().await.into_iter().collect();
    // the server announces APPENDLIMIT=65536
    let too_large = client_mail.dir().join("INBOX/cur/too_large:2,S");
    assert_ok!(std::fs::write(&too_large, "x".repeat(65_537)));
    client_mailbox.add_mail(b"new");

    mail_setup.sync("INBOX").await;

    assert!(assert_ok!(too_large.try_exists()));
    let server_mails: HashSet<_> = server_mailbox.mails().await.into_iter().collect();
    assert_eq!(server_mails.len(), initial_server_mails.len() + 1);
    assert_ok!(std::fs::remove_file(&too_large));
    let client_mails: HashSet<_> = client_mailbox.mails().await.into_iter().collect();
    assert_eq!(server_mails, client_mails);
}

#[rstest]
#[compio::test]
#[awt]
async fn test_syncing_new_mail_works(#[future] mail_setup: MailSetup) {
    let client_mail = mail_setup.client_mail();
    let client_mailbox = client_mail.mailbox("INBOX");
    let initial_client_mails: HashSet<_> = client_mailbox.mails().await.into_iter().collect();
    let server_mailbox = mail_setup.server_mail().mailbox("INBOX");
    let server_mails: HashSet<_> = server_mailbox.mails().await.into_iter().collect();
    assert_eq!(server_mails, initial_client_mails);
    let content = String::from("new");
    server_mailbox.add_mail(content.as_bytes());

    mail_setup.sync("INBOX").await;

    let client_mails: HashSet<_> = client_mailbox.mails().await.into_iter().collect();
    let server_mails: HashSet<_> = server_mailbox.mails().await.into_iter().collect();
    assert_eq!(server_mails, client_mails);
    assert_ne!(initial_client_mails, client_mails);
}
#[rstest]
#[compio::test]
#[awt]
async fn test_removing_mail_works(#[future] mail_setup: MailSetup) {
    let client_mail = mail_setup.client_mail();
    let client_mailbox = client_mail.mailbox("INBOX");
    let client_mails: HashSet<_> = client_mailbox.mails().await.into_iter().collect();
    let server_mailbox = mail_setup.server_mail().mailbox("INBOX");
    let initial_server_mails: HashSet<_> = server_mailbox.mails().await.into_iter().collect();
    assert_eq!(initial_server_mails, client_mails);
    let mut client_mails = client_mails.into_iter();
    assert_some!(client_mails.next()).delete();

    mail_setup.sync("INBOX").await;

    let client_mails: HashSet<_> = client_mailbox.mails().await.into_iter().collect();
    let server_mails: HashSet<_> = server_mailbox.mails().await.into_iter().collect();
    assert_eq!(server_mails, client_mails);
    assert_ne!(initial_server_mails, server_mails);
}

#[rstest]
#[compio::test]
#[awt]
async fn test_syncing_removed_mail_works(#[future] mail_setup: MailSetup) {
    let client_mail = mail_setup.client_mail();
    let client_mailbox = client_mail.mailbox("INBOX");
    let initial_client_mails: HashSet<_> = client_mailbox.mails().await.into_iter().collect();
    let server_mailbox = mail_setup.server_mail().mailbox("INBOX");
    let server_mails: HashSet<_> = server_mailbox.mails().await.into_iter().collect();
    assert_eq!(server_mails, initial_client_mails);
    let mut server_mails = server_mails.into_iter();
    assert_some!(server_mails.next()).delete();

    mail_setup.sync("INBOX").await;

    let client_mails: HashSet<_> = client_mailbox.mails().await.into_iter().collect();
    let server_mails: HashSet<_> = server_mailbox.mails().await.into_iter().collect();
    assert_eq!(server_mails, client_mails);
    assert_ne!(initial_client_mails, client_mails);
}

#[rstest]
#[compio::test]
#[awt]
async fn test_interrupted_initial_sync_gets_resumed(#[future] mail_setup: MailSetup) {
    let client_mail = mail_setup.client_mail();
    client_mail.wipe();
    mail_setup.sync("INBOX").await;
    let client_mailbox = client_mail.mailbox("INBOX");
    let server_mailbox = mail_setup.server_mail().mailbox("INBOX");

    let state_file = mail_setup
        .config()
        .state_dir()
        .join("INBOX")
        .join("imapmaildir.db");
    let db = assert_ok!(rusqlite::Connection::open(state_file));
    assert_ok!(db.execute("delete from highest_modseq", []));
    let mut client_mails = client_mailbox.mails().await;
    let missing_mail = assert_some!(client_mails.pop());
    assert_ok!(db.execute(
        "delete from mail_metadata where uid = ?1",
        [assert_ok!(i64::try_from(missing_mail.uid()))]
    ));
    drop(db);
    missing_mail.delete();
    let mut server_mails = server_mailbox.mails().await;
    let deleted_mail = assert_some!(server_mails.pop());
    let deleted_uid = deleted_mail.uid();
    deleted_mail.delete();

    mail_setup.sync("INBOX").await;

    let client_mails: HashSet<_> = client_mailbox.mails().await.into_iter().collect();
    let server_mails: HashSet<_> = server_mailbox.mails().await.into_iter().collect();
    assert_eq!(server_mails, client_mails);
    assert!(client_mails.iter().all(|mail| mail.uid() != deleted_uid));
}

#[rstest]
#[compio::test]
#[awt]
async fn test_oauth2_login_syncs(#[future] mail_setup: MailSetup) {
    let client_mail = mail_setup.client_mail();
    client_mail.wipe();
    let server_mailbox = mail_setup.server_mail().mailbox("INBOX");
    let auth = mail_setup.oauth2_auth(&oauth2_token(mail_setup.user()));

    assert_ok!(mail_setup.sync_with_auth("INBOX", &auth).await);

    let client_mailbox = client_mail.mailbox("INBOX");
    let client_mails: HashSet<_> = client_mailbox.mails().await.into_iter().collect();
    let server_mails: HashSet<_> = server_mailbox.mails().await.into_iter().collect();
    assert_eq!(server_mails, client_mails);
}

#[rstest]
#[compio::test]
#[awt]
async fn test_oauth2_login_with_invalid_token_fails_permanently(#[future] mail_setup: MailSetup) {
    let auth = mail_setup.oauth2_auth(&oauth2_token("someone-else"));

    let result = assert_ok!(
        compio::time::timeout(
            std::time::Duration::from_secs(10),
            mail_setup.sync_with_auth("INBOX", &auth)
        )
        .await
    );

    let error = assert_err!(result);
    assert!(
        matches!(error, imapmaildir::ClientError::LoginRejected(_)),
        "unexpected error {error}"
    );
    assert!(!error.is_transient());
}

#[rstest]
#[compio::test]
#[awt]
async fn test_list_remote_mailboxes(#[future] mail_setup: MailSetup) {
    let mailboxes: HashSet<_> = mail_setup.list_remote().await.into_iter().collect();

    assert_eq!(
        mailboxes,
        HashSet::from(["INBOX".to_string(), "DRAFT".to_string()])
    );
}
