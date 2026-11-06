pub mod fixtures;

use std::{collections::HashSet, ffi::OsString, fs, path::PathBuf, sync::Arc};

use assertables::*;
use imapmaildir::{ChangeRecorder, DEFAULT_MAX_PENDING, HookThread, MailChange, batched_hook};
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

#[rstest]
#[compio::test]
#[awt]
async fn test_initial_sync_records_single_initial(#[future] mail_setup: MailSetup) {
    mail_setup.client_mail().wipe();

    let changes = mail_setup.sync_recording("INBOX").await;

    assert_eq!(changes, [MailChange::Initial]);
}

#[rstest]
#[compio::test]
#[awt]
async fn test_unchanged_sync_records_nothing(#[future] mail_setup: MailSetup) {
    let changes = mail_setup.sync_recording("INBOX").await;

    assert_is_empty!(changes);
}

#[rstest]
#[compio::test]
#[awt]
async fn test_remote_changes_get_recorded(#[future] mail_setup: MailSetup) {
    mail_setup.sync("INBOX").await;
    let server_mailbox = mail_setup.server_mail().mailbox("INBOX");
    let content = b"recorded";
    server_mailbox.add_mail(content);
    let deleted = assert_some!(server_mailbox.mails().await.into_iter().next());
    let deleted_uid = deleted.uid();
    deleted.delete();

    let changes = mail_setup.sync_recording("INBOX").await;

    let client_mail = mail_setup.client_mail();
    let client_mailbox = client_mail.mailbox("INBOX");
    let downloaded = assert_some!(
        client_mailbox
            .mails()
            .await
            .into_iter()
            .find(|mail| mail.content() == content)
    );
    assert_len_eq_x!(changes.as_slice(), 2);
    assert_any!(changes.iter(), |change: &MailChange| matches!(
        change,
        MailChange::Downloaded {
            uid, path, flags, ..
        }
            if u64::from(u32::from(*uid)) == downloaded.uid() && path.exists() && flags.is_empty()
    ));
    assert_any!(changes.iter(), |change: &MailChange| matches!(
        change,
        MailChange::Deleted { uid, path }
            if u64::from(u32::from(*uid)) == deleted_uid && !path.exists()
    ));
}

#[rstest]
#[compio::test]
#[awt]
async fn test_batched_hook_gets_lines(#[future] mail_setup: MailSetup) {
    mail_setup.sync("INBOX").await;
    let server_mailbox = mail_setup.server_mail().mailbox("INBOX");
    server_mailbox.add_mail(b"hooked");
    let output = mail_setup.config().state_dir().join("hook.tsv");
    let cmd: Vec<OsString> = ["sh", "-c", &format!("cat >> '{}'", output.display())]
        .into_iter()
        .map(OsString::from)
        .collect();
    let hooks = assert_ok!(HookThread::start());
    let (sink, hook_task) = batched_hook(
        &cmd,
        "INBOX",
        DEFAULT_MAX_PENDING,
        None,
        Arc::new(|e| panic!("hook failed: {e}")),
        hooks.runner(),
    );

    mail_setup
        .sync_with_changes("INBOX", Some(ChangeRecorder::new(vec![sink])))
        .await;
    hook_task
        .await
        .unwrap_or_else(|_| panic!("hook task panicked"));

    let lines = assert_ok!(fs::read_to_string(&output));
    let fields: Vec<_> = assert_some!(lines.lines().next()).split('\t').collect();
    assert_len_eq_x!(lines.lines().collect::<Vec<_>>(), 1);
    assert_eq!(fields[0], "INBOX");
    assert_eq!(fields[1], "downloaded");
    assert_eq!(assert_ok!(fs::read(fields[2])), b"hooked");
    assert_starts_with!(fields[3], "uid=");
    assert_ends_with!(fields[3], " flags=");
    assert_eq!(fields[4], "");
}

#[rstest]
#[compio::test]
#[awt]
async fn test_moving_mail_between_mailboxes_syncing_them_separately_uploads_it(
    #[future] mail_setup: MailSetup,
) {
    mail_setup.sync("DRAFT").await;
    let client_mail = mail_setup.client_mail();
    let client_inbox = client_mail.mailbox("INBOX");
    let initial_inbox = client_inbox.mails().await.len();
    let client_draft = client_mail.mailbox("DRAFT");
    let initial_draft = client_draft.mails().await.len();
    let mail = assert_some!(client_inbox.mails().await.into_iter().next());
    mail_setup.relocate_client_mail(mail.path(), "DRAFT", Relocation::Move);

    mail_setup.sync("INBOX").await;
    mail_setup.sync("DRAFT").await;

    let server_inbox = mail_setup.server_mail().mailbox("INBOX");
    let client_inbox_mails: HashSet<_> = client_inbox.mails().await.into_iter().collect();
    let client_draft_mails: HashSet<_> = client_draft.mails().await.into_iter().collect();
    let server_inbox_mails: HashSet<_> = server_inbox.mails().await.into_iter().collect();
    assert_eq!(server_inbox_mails.len(), initial_inbox - 1);
    assert_eq!(server_inbox_mails, client_inbox_mails);
    assert_eq!(client_draft_mails.len(), initial_draft + 1);
}

/// A mail of INBOX, which is about to move to DRAFT, with the mailbox sizes before
struct MoveSetup {
    path: PathBuf,
    uid: u64,
    content: Vec<u8>,
    initial_inbox: usize,
    initial_draft: usize,
}

/// Syncs DRAFT, gives the mails on the server an old INTERNALDATE and picks a mail to move
async fn prepare_move(mail_setup: &MailSetup) -> MoveSetup {
    mail_setup.sync("DRAFT").await;
    age_server_mails(mail_setup);
    let client_mail = mail_setup.client_mail();
    let (client_inbox, client_draft) = (client_mail.mailbox("INBOX"), client_mail.mailbox("DRAFT"));
    let inbox = client_inbox.mails().await;
    let initial_draft = client_draft.mails().await.len();
    let mail = assert_some!(inbox.first());

    MoveSetup {
        path: mail.path().to_path_buf(),
        uid: mail.uid(),
        content: mail.content().to_vec(),
        initial_inbox: inbox.len(),
        initial_draft,
    }
}

/// Asserts that the mail moved from INBOX to DRAFT on the server
fn assert_moved_on_server(mail_setup: &MailSetup, setup: &MoveSetup) {
    assert_eq!(
        server_files(mail_setup, "INBOX").len(),
        setup.initial_inbox - 1
    );
    assert_eq!(
        server_files(mail_setup, "DRAFT").len(),
        setup.initial_draft + 1
    );
}

#[rstest]
#[compio::test]
#[awt]
async fn test_moving_mail_between_mailboxes_moves_it_on_server(#[future] mail_setup: MailSetup) {
    let setup = prepare_move(&mail_setup).await;
    mail_setup.relocate_client_mail(&setup.path, "DRAFT", Relocation::Move);

    mail_setup.sync_together(&MAILBOXES).await;

    assert_moved_on_server(&mail_setup, &setup);
    // uploading the mail would have given it a new INTERNALDATE
    assert_eq!(
        aged_server_files(&mail_setup, "DRAFT"),
        setup.initial_draft + 1
    );
    let client_mail = mail_setup.client_mail();
    let (client_inbox, client_draft) = (client_mail.mailbox("INBOX"), client_mail.mailbox("DRAFT"));
    let client_draft_mails = client_draft.mails().await;
    assert!(
        client_draft_mails
            .iter()
            .any(|mail| mail.content() == setup.content)
    );
    assert!(server_file_with(&mail_setup, "DRAFT", &setup.content).is_some());
    let client_inbox_mails: HashSet<_> = client_inbox.mails().await.into_iter().collect();
    let server_inbox = mail_setup.server_mail().mailbox("INBOX");
    let server_inbox_mails: HashSet<_> = server_inbox.mails().await.into_iter().collect();
    assert_eq!(client_inbox_mails, server_inbox_mails);

    // the next sync has nothing left to do
    mail_setup.sync_together(&MAILBOXES).await;
    assert_moved_on_server(&mail_setup, &setup);
}

#[rstest]
#[compio::test]
#[awt]
async fn test_moving_mail_with_remote_flag_change_keeps_it_in_destination(
    #[future] mail_setup: MailSetup,
) {
    let setup = prepare_move(&mail_setup).await;
    let server_inbox = mail_setup.server_mail().mailbox("INBOX");
    let mut server_mails = server_inbox.mails().await;
    let server_mail = assert_some!(server_mails.iter_mut().find(|mail| mail.uid() == setup.uid));
    assert!(server_mail.add_flag('F'));
    mail_setup.relocate_client_mail(&setup.path, "DRAFT", Relocation::Move);

    mail_setup.sync_together(&MAILBOXES).await;

    assert_moved_on_server(&mail_setup, &setup);
    assert_eq!(
        aged_server_files(&mail_setup, "DRAFT"),
        setup.initial_draft + 1
    );
    // the server's flag change reaches the moved mail with the next sync
    mail_setup.sync_together(&MAILBOXES).await;
    let client_mail = mail_setup.client_mail();
    let client_draft = client_mail.mailbox("DRAFT");
    let client_draft_mails = client_draft.mails().await;
    let moved = assert_some!(
        client_draft_mails
            .iter()
            .find(|mail| mail.content() == setup.content)
    );
    assert!(moved.has_flag('F'));
    let server_file = assert_some!(server_file_with(&mail_setup, "DRAFT", &setup.content));
    let name = server_file.to_string_lossy();
    let (_, flags) = assert_some!(name.rsplit_once(":2,"));
    assert_contains!(flags, "F");
}

#[rstest]
#[compio::test]
#[awt]
async fn test_moving_mail_with_dropped_planner_uploads_it(#[future] mail_setup: MailSetup) {
    let setup = prepare_move(&mail_setup).await;
    mail_setup.relocate_client_mail(&setup.path, "DRAFT", Relocation::Move);

    mail_setup.sync_with_dropped_planner("INBOX").await;
    mail_setup.sync_with_dropped_planner("DRAFT").await;

    assert_moved_on_server(&mail_setup, &setup);
    // uploading gives the mail a new INTERNALDATE
    assert_eq!(aged_server_files(&mail_setup, "DRAFT"), setup.initial_draft);
}

#[rstest]
#[compio::test]
#[awt]
async fn test_syncing_unchanged_mailboxes_together_does_nothing(#[future] mail_setup: MailSetup) {
    mail_setup.sync("DRAFT").await;
    let inbox_files = server_files(&mail_setup, "INBOX").len();
    let draft_files = server_files(&mail_setup, "DRAFT").len();

    mail_setup.sync_together(&MAILBOXES).await;

    assert_eq!(server_files(&mail_setup, "INBOX").len(), inbox_files);
    assert_eq!(server_files(&mail_setup, "DRAFT").len(), draft_files);
}
