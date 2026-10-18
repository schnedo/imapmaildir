pub mod fixtures;

use std::{
    collections::HashSet,
    fs::File,
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
