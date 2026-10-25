pub mod fixtures;

use std::{
    collections::HashSet,
    fs::{self, File},
    io::Write as _,
    time::Duration,
};

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

#[rstest]
#[compio::test]
#[awt]
async fn test_local_flag_change_after_remote_flag_change_is_synced(
    #[future] mail_setup: MailSetup,
) {
    let client_mail = mail_setup.client_mail();
    let client_mailbox = client_mail.mailbox("INBOX");
    let server_mailbox = mail_setup.server_mail().mailbox("INBOX");
    let (handle, call_rx) = mail_setup.sync_continuous("INBOX").await;
    compio::time::sleep(Duration::from_secs(1)).await;

    let mut server_mails = server_mailbox.mails().await;
    let server_mail = assert_some!(
        server_mails
            .iter_mut()
            .find(|mail| !mail.has_flag('F') && !mail.has_flag('D'))
    );
    let uid = server_mail.uid();
    assert!(server_mail.add_flag('F'));
    while !client_mailbox
        .mails()
        .await
        .iter()
        .any(|mail| mail.uid() == uid && mail.has_flag('F'))
    {
        assert_ok!(call_rx.recv().await);
    }

    let mut client_mails = client_mailbox.mails().await;
    let client_mail = assert_some!(client_mails.iter_mut().find(|mail| mail.uid() == uid));
    assert!(client_mail.add_flag('D'));
    assert_ok!(
        compio::time::timeout(Duration::from_secs(10), async {
            loop {
                let server_mails = server_mailbox.mails().await;
                let server_mail = assert_some!(server_mails.iter().find(|mail| mail.uid() == uid));
                if server_mail.has_flag('D') {
                    assert!(server_mail.has_flag('F'));
                    break;
                }
                compio::time::sleep(Duration::from_millis(100)).await;
            }
        })
        .await
    );
    drop(handle);
}

#[rstest]
#[compio::test]
#[awt]
async fn test_slowly_written_local_mail_is_synced_completely(#[future] mail_setup: MailSetup) {
    let server_mailbox = mail_setup.server_mail().mailbox("INBOX");
    let (handle, _call_rx) = mail_setup.sync_continuous("INBOX").await;
    compio::time::sleep(Duration::from_secs(1)).await;

    let path = mail_setup
        .config()
        .maildir_base_path()
        .join("INBOX/cur/slow:2,S");
    let mut file = assert_ok!(File::create_new(&path));
    assert_ok!(file.write_all(b"first part"));
    assert_ok!(file.flush());
    compio::time::sleep(Duration::from_millis(500)).await;
    assert_ok!(file.write_all(b" second part"));
    drop(file);

    let content = b"first part second part".as_slice();
    assert_ok!(
        compio::time::timeout(Duration::from_secs(10), async {
            while !server_mailbox
                .mails()
                .await
                .iter()
                .any(|mail| mail.content() == content)
            {
                compio::time::sleep(Duration::from_millis(100)).await;
            }
        })
        .await
    );
    assert!(
        !server_mailbox
            .mails()
            .await
            .iter()
            .any(|mail| mail.content() == b"first part")
    );
    drop(handle);
}

#[rstest]
#[compio::test]
#[awt]
async fn test_directory_in_cur_does_not_stop_syncing(#[future] mail_setup: MailSetup) {
    let server_mailbox = mail_setup.server_mail().mailbox("INBOX");
    let (handle, _call_rx) = mail_setup.sync_continuous("INBOX").await;
    compio::time::sleep(Duration::from_secs(1)).await;

    let maildir = mail_setup.config().maildir_base_path().join("INBOX");
    assert_ok!(fs::create_dir(maildir.join("cur/subdir")));
    compio::time::sleep(Duration::from_millis(500)).await;
    let content = b"after subdir".as_slice();
    assert_ok!(fs::write(maildir.join("tmp/after"), content));
    assert_ok!(fs::rename(
        maildir.join("tmp/after"),
        maildir.join("cur/after:2,S")
    ));

    assert_ok!(
        compio::time::timeout(Duration::from_secs(10), async {
            while !server_mailbox
                .mails()
                .await
                .iter()
                .any(|mail| mail.content() == content)
            {
                compio::time::sleep(Duration::from_millis(100)).await;
            }
        })
        .await
    );
    drop(handle);
}

#[rstest]
#[compio::test]
#[awt]
async fn test_local_mail_delivered_to_new_is_synced(#[future] mail_setup: MailSetup) {
    let server_mailbox = mail_setup.server_mail().mailbox("INBOX");
    let (handle, _call_rx) = mail_setup.sync_continuous("INBOX").await;
    compio::time::sleep(Duration::from_secs(1)).await;

    let maildir = mail_setup.config().maildir_base_path().join("INBOX");
    let content = b"delivered to new".as_slice();
    assert_ok!(fs::write(maildir.join("tmp/delivered"), content));
    assert_ok!(fs::rename(
        maildir.join("tmp/delivered"),
        maildir.join("new/delivered")
    ));

    assert_ok!(
        compio::time::timeout(Duration::from_secs(10), async {
            while !server_mailbox
                .mails()
                .await
                .iter()
                .any(|mail| mail.content() == content)
            {
                compio::time::sleep(Duration::from_millis(100)).await;
            }
        })
        .await
    );
    assert_eq!(assert_ok!(maildir.join("new").read_dir()).count(), 0);
    drop(handle);
}

async fn wait_for_absence(mailbox: &impl Maildir, uid: u64) {
    assert_ok!(
        compio::time::timeout(Duration::from_secs(10), async {
            while mailbox.mails().await.iter().any(|mail| mail.uid() == uid) {
                compio::time::sleep(Duration::from_millis(100)).await;
            }
        })
        .await
    );
}

#[rstest]
#[compio::test]
#[awt]
async fn test_remote_deletion_is_synced(#[future] mail_setup: MailSetup) {
    let client_mail = mail_setup.client_mail();
    let client_mailbox = client_mail.mailbox("INBOX");
    let server_mailbox = mail_setup.server_mail().mailbox("INBOX");
    let (handle, call_rx) = mail_setup.sync_continuous("INBOX").await;
    compio::time::sleep(Duration::from_secs(1)).await;

    let server_mail = assert_some!(server_mailbox.mails().await.into_iter().next());
    let uid = server_mail.uid();
    server_mail.delete();

    wait_for_absence(&client_mailbox, uid).await;
    drop(handle);
    while call_rx.recv().await.is_ok() {}
}

#[rstest]
#[compio::test]
#[awt]
async fn test_concurrent_local_and_remote_deletion_are_synced(#[future] mail_setup: MailSetup) {
    let client_mail = mail_setup.client_mail();
    let client_mailbox = client_mail.mailbox("INBOX");
    let server_mailbox = mail_setup.server_mail().mailbox("INBOX");
    let (handle, call_rx) = mail_setup.sync_continuous("INBOX").await;
    compio::time::sleep(Duration::from_secs(1)).await;

    let mut server_mails = server_mailbox.mails().await.into_iter();
    let remote_deleted = assert_some!(server_mails.next());
    let local_deleted_uid = assert_some!(server_mails.next()).uid();
    let remote_deleted_uid = remote_deleted.uid();
    let local_deleted = assert_some!(
        client_mailbox
            .mails()
            .await
            .into_iter()
            .find(|mail| mail.uid() == local_deleted_uid)
    );
    remote_deleted.delete();
    local_deleted.delete();

    wait_for_absence(&client_mailbox, remote_deleted_uid).await;
    wait_for_absence(&server_mailbox, local_deleted_uid).await;
    compio::time::sleep(Duration::from_secs(1)).await;
    let client_uids: HashSet<_> = client_mailbox
        .mails()
        .await
        .iter()
        .map(MailFile::uid)
        .collect();
    let server_uids: HashSet<_> = server_mailbox
        .mails()
        .await
        .iter()
        .map(MailFile::uid)
        .collect();
    assert_eq!(client_uids, server_uids);
    drop(handle);
    while call_rx.recv().await.is_ok() {}
}

#[rstest]
#[compio::test]
#[awt]
async fn test_local_deletion_of_uploaded_mail_is_synced(#[future] mail_setup: MailSetup) {
    let client_mail = mail_setup.client_mail();
    let client_mailbox = client_mail.mailbox("INBOX");
    let server_mailbox = mail_setup.server_mail().mailbox("INBOX");
    let (handle, call_rx) = mail_setup.sync_continuous("INBOX").await;
    compio::time::sleep(Duration::from_secs(1)).await;

    let maildir = mail_setup.config().maildir_base_path().join("INBOX");
    let content = b"uploaded then deleted".as_slice();
    assert_ok!(fs::write(maildir.join("tmp/uploaded"), content));
    assert_ok!(fs::rename(
        maildir.join("tmp/uploaded"),
        maildir.join("new/uploaded")
    ));
    let uid = assert_ok!(
        compio::time::timeout(Duration::from_secs(10), async {
            loop {
                if let Some(mail) = server_mailbox
                    .mails()
                    .await
                    .into_iter()
                    .find(|mail| mail.content() == content)
                {
                    break mail.uid();
                }
                compio::time::sleep(Duration::from_millis(100)).await;
            }
        })
        .await
    );
    let uploaded = assert_ok!(
        compio::time::timeout(Duration::from_secs(10), async {
            loop {
                if let Some(mail) = client_mailbox
                    .mails()
                    .await
                    .into_iter()
                    .find(|mail| mail.uid() == uid)
                {
                    break mail;
                }
                compio::time::sleep(Duration::from_millis(100)).await;
            }
        })
        .await
    );

    uploaded.delete();

    wait_for_absence(&server_mailbox, uid).await;
    drop(handle);
    while call_rx.recv().await.is_ok() {}
}

#[rstest]
#[compio::test]
#[awt]
async fn test_repeated_local_changes_of_same_mail_are_synced(#[future] mail_setup: MailSetup) {
    let client_mail = mail_setup.client_mail();
    let client_mailbox = client_mail.mailbox("INBOX");
    let server_mailbox = mail_setup.server_mail().mailbox("INBOX");
    let (handle, call_rx) = mail_setup.sync_continuous("INBOX").await;
    compio::time::sleep(Duration::from_secs(1)).await;

    let mut local = assert_some!(
        client_mailbox
            .mails()
            .await
            .into_iter()
            .find(|mail| !mail.has_flag('F'))
    );
    let uid = local.uid();
    assert!(local.add_flag('F'));
    assert_ok!(
        compio::time::timeout(Duration::from_secs(10), async {
            while !server_mailbox
                .mails()
                .await
                .iter()
                .any(|mail| mail.uid() == uid && mail.has_flag('F'))
            {
                compio::time::sleep(Duration::from_millis(100)).await;
            }
        })
        .await
    );

    local.delete();

    wait_for_absence(&server_mailbox, uid).await;
    drop(handle);
    while call_rx.recv().await.is_ok() {}
}

#[rstest]
#[compio::test]
#[awt]
async fn test_sync_continues_after_server_disconnect(#[future] mail_setup: MailSetup) {
    let client_mail = mail_setup.client_mail();
    let client_mailbox = client_mail.mailbox("INBOX");
    let server_mailbox = mail_setup.server_mail().mailbox("INBOX");
    let sync = mail_setup.sync_reconnecting("INBOX");
    compio::time::sleep(Duration::from_secs(1)).await;

    mail_setup.server_mail().kick();
    let remote_content = b"after disconnect remote".as_slice();
    server_mailbox.add_mail(remote_content);
    let maildir = mail_setup.config().maildir_base_path().join("INBOX");
    let local_content = b"after disconnect local".as_slice();
    assert_ok!(fs::write(maildir.join("tmp/reconnect"), local_content));
    assert_ok!(fs::rename(
        maildir.join("tmp/reconnect"),
        maildir.join("cur/reconnect:2,S")
    ));

    assert_ok!(
        compio::time::timeout(Duration::from_secs(20), async {
            // client mails can only be listed once the new local mail got its uid
            while !server_mailbox
                .mails()
                .await
                .iter()
                .any(|mail| mail.content() == local_content)
            {
                compio::time::sleep(Duration::from_millis(100)).await;
            }
            while !client_mailbox
                .mails()
                .await
                .iter()
                .any(|mail| mail.content() == remote_content)
            {
                compio::time::sleep(Duration::from_millis(100)).await;
            }
        })
        .await
    );
    drop(sync);
}

#[rstest]
#[compio::test]
#[awt]
async fn test_shutdown_finishes_syncing_detected_local_changes(#[future] mail_setup: MailSetup) {
    let client_mail = mail_setup.client_mail();
    let client_mailbox = client_mail.mailbox("INBOX");
    let server_mailbox = mail_setup.server_mail().mailbox("INBOX");
    let sync = mail_setup.sync_reconnecting("INBOX");
    compio::time::sleep(Duration::from_secs(1)).await;

    let maildir = mail_setup.config().maildir_base_path().join("INBOX");
    let content = b"before shutdown".as_slice();
    assert_ok!(fs::write(maildir.join("tmp/shutdown"), content));
    assert_ok!(fs::rename(
        maildir.join("tmp/shutdown"),
        maildir.join("cur/shutdown:2,S")
    ));
    // lets the watcher report the new mail
    compio::time::sleep(Duration::from_secs(1)).await;
    sync.shut_down();

    assert!(
        server_mailbox
            .mails()
            .await
            .iter()
            .any(|mail| mail.content() == content)
    );
    // client mails can only be listed once the new local mail got its uid
    assert!(
        client_mailbox
            .mails()
            .await
            .iter()
            .any(|mail| mail.content() == content)
    );
}
