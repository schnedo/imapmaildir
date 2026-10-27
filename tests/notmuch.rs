//! Syncing keywords with notmuch tags. Needs the `notmuch` command and libnotmuch, without
//! which the tests pass after logging a warning.

pub mod fixtures;

use std::{
    collections::{BTreeSet, HashMap},
    fs,
    path::{Path, PathBuf},
    process::Command,
    time::Duration,
};

use assertables::*;
use imapmaildir::{Notmuch, NotmuchLocation, TagConfig, TagSync};
use rstest::*;

use crate::fixtures::*;

/// Notmuch database indexing the client mails
struct Tags {
    config: PathBuf,
    sync: TagSync,
}

impl Tags {
    /// `None` if notmuch is not available
    fn new(mail_setup: &MailSetup) -> Option<Self> {
        let root = mail_setup.config().maildir_base_path();
        let config = assert_some!(root.parent()).join("notmuch-config");
        assert_ok!(fs::write(
            &config,
            format!(
                "[database]\npath={}\n[new]\ntags=inbox;unread\n[maildir]\nsynchronize_flags=true\n",
                root.display()
            )
        ));
        let created = Command::new("notmuch")
            .arg(format!("--config={}", config.display()))
            .arg("new")
            .status();
        if !created.is_ok_and(|status| status.success()) {
            eprintln!("skipping test needing notmuch");
            return None;
        }
        let notmuch = match Notmuch::start(NotmuchLocation {
            config: Some(config.clone()),
            ..NotmuchLocation::default()
        }) {
            Ok(notmuch) => notmuch,
            Err(e) => {
                eprintln!("skipping test needing libnotmuch: {e}");
                return None;
            }
        };

        Some(Self {
            config,
            sync: TagSync {
                notmuch,
                config: TagConfig::default(),
            },
        })
    }

    fn notmuch(&self, args: &[&str]) -> String {
        let output = assert_ok!(
            Command::new("notmuch")
                .arg(format!("--config={}", self.config.display()))
                .args(args)
                .output()
        );
        assert!(
            output.status.success(),
            "notmuch {args:?} failed: {output:?}"
        );
        assert_ok!(String::from_utf8(output.stdout))
    }

    /// All tags of mails in `folder`
    fn tags_of(&self, folder: &str) -> BTreeSet<String> {
        self.notmuch(&["search", "--output=tags", &format!("folder:{folder}")])
            .lines()
            .map(str::to_string)
            .collect()
    }
}

/// Keywords of dovecot's maildir by their file name letter
fn dovecot_keywords(mailbox: &Path) -> HashMap<char, String> {
    let Ok(keywords) = fs::read_to_string(mailbox.join("dovecot-keywords")) else {
        return HashMap::new();
    };
    keywords
        .lines()
        .filter_map(|line| {
            let (index, keyword) = line.split_once(' ')?;
            let index: u8 = index.parse().ok()?;
            Some((char::from(b'a' + index), keyword.to_string()))
        })
        .collect()
}

/// Keywords of all server mails in INBOX
fn server_keywords(mail_setup: &MailSetup) -> BTreeSet<String> {
    let inbox = mail_setup.server_mail().dir().to_path_buf();
    let letters = dovecot_keywords(&inbox);
    let mut keywords = BTreeSet::new();
    for entry in assert_ok!(fs::read_dir(inbox.join("cur"))) {
        let name = assert_ok!(entry).file_name().to_string_lossy().into_owned();
        if let Some((_, info)) = name.rsplit_once(":2,") {
            keywords.extend(
                info.chars()
                    .filter_map(|letter| letters.get(&letter).cloned()),
            );
        }
    }

    keywords
}

#[rstest]
#[compio::test]
#[awt]
async fn test_remote_keyword_becomes_tag(#[future] mail_setup: MailSetup) {
    let Some(tags) = Tags::new(&mail_setup) else {
        return;
    };
    mail_setup.sync_with_tags("INBOX", &tags.sync).await;
    assert!(!tags.tags_of("INBOX").contains("work"));
    let server = mail_setup.server_mail().dir().to_path_buf();
    assert_ok!(fs::write(server.join("dovecot-keywords"), "0 work\n"));
    let server_mailbox = mail_setup.server_mail().mailbox("INBOX");
    let mut server_mails = server_mailbox.mails().await;
    let mail = assert_some!(server_mails.first_mut());
    assert!(mail.add_flag('a'));

    mail_setup.sync_with_tags("INBOX", &tags.sync).await;

    assert!(tags.tags_of("INBOX").contains("work"));
}

#[rstest]
#[compio::test]
#[awt]
async fn test_tag_becomes_remote_keyword(#[future] mail_setup: MailSetup) {
    let Some(tags) = Tags::new(&mail_setup) else {
        return;
    };
    mail_setup.sync_with_tags("INBOX", &tags.sync).await;
    assert_is_empty!(server_keywords(&mail_setup));

    tags.notmuch(&["tag", "+todo", "--", "folder:INBOX"]);
    mail_setup.sync_with_tags("INBOX", &tags.sync).await;

    assert_eq!(
        server_keywords(&mail_setup),
        BTreeSet::from(["todo".to_string()])
    );
    // tags of notmuch and flags stay local
    tags.notmuch(&["tag", "-todo", "+attachment", "--", "folder:INBOX"]);
    mail_setup.sync_with_tags("INBOX", &tags.sync).await;

    assert_is_empty!(server_keywords(&mail_setup));
}

#[rstest]
#[compio::test]
#[awt]
async fn test_tag_refused_by_server_does_not_stop_syncing(#[future] mail_setup: MailSetup) {
    let Some(tags) = Tags::new(&mail_setup) else {
        return;
    };
    mail_setup.sync_with_tags("INBOX", &tags.sync).await;
    // dovecot refuses keywords longer than 50 characters
    let long = "k".repeat(60);
    let other = "j".repeat(60);
    tags.notmuch(&[
        "tag",
        &format!("+{long}"),
        &format!("+{other}"),
        "+todo",
        "--",
        "folder:INBOX",
    ]);
    mail_setup.sync_with_tags("INBOX", &tags.sync).await;
    // only refused ones
    tags.notmuch(&["tag", &format!("-{long}"), "--", "folder:INBOX"]);
    tags.notmuch(&["tag", &format!("+{long}2"), "--", "folder:INBOX"]);

    mail_setup.sync_with_tags("INBOX", &tags.sync).await;
    mail_setup.sync_with_tags("INBOX", &tags.sync).await;

    assert_eq!(
        server_keywords(&mail_setup),
        BTreeSet::from(["todo".to_string()])
    );
}

/// Keywords of server mails in INBOX by the body of their mail
fn server_keywords_by_body(mail_setup: &MailSetup) -> HashMap<String, BTreeSet<String>> {
    let inbox = mail_setup.server_mail().dir().to_path_buf();
    let letters = dovecot_keywords(&inbox);
    let mut keywords = HashMap::new();
    for entry in assert_ok!(fs::read_dir(inbox.join("cur"))) {
        let path = assert_ok!(entry).path();
        let name = path.file_name().unwrap_or_default().to_string_lossy();
        let Some((_, info)) = name.rsplit_once(":2,") else {
            continue;
        };
        let content = assert_ok!(fs::read_to_string(&path));
        let body = content.split("\n\n").nth(1).unwrap_or_default().trim();
        keywords.insert(
            body.to_string(),
            info.chars()
                .filter_map(|letter| letters.get(&letter).cloned())
                .collect(),
        );
    }

    keywords
}

#[rstest]
#[compio::test]
#[awt]
async fn test_different_tags_of_several_mails_become_their_remote_keywords(
    #[future] mail_setup: MailSetup,
) {
    let Some(tags) = Tags::new(&mail_setup) else {
        return;
    };
    let client_mail = mail_setup.client_mail();
    for i in 0..3 {
        let path = client_mail.dir().join(format!("INBOX/cur/multi{i}:2,S"));
        assert_ok!(fs::write(
            path,
            format!("Message-ID: <multi{i}@test>\nSubject: multi\n\nmulti {i}\n"),
        ));
    }
    mail_setup.sync_with_tags("INBOX", &tags.sync).await;
    // dovecot refuses keywords longer than 50 characters
    let long = "k".repeat(60);
    tags.notmuch(&["tag", "+a", "--", "id:multi0@test"]);
    tags.notmuch(&["tag", "+a", "+b", "--", "id:multi1@test"]);
    tags.notmuch(&["tag", "+c", &format!("+{long}"), "--", "id:multi2@test"]);

    mail_setup.sync_with_tags("INBOX", &tags.sync).await;

    let keywords = server_keywords_by_body(&mail_setup);
    let expected =
        |values: &[&str]| -> BTreeSet<String> { values.iter().map(ToString::to_string).collect() };
    assert_eq!(keywords["multi 0"], expected(&["a"]));
    assert_eq!(keywords["multi 1"], expected(&["a", "b"]));
    assert_eq!(keywords["multi 2"], expected(&["c"]));
}

#[rstest]
#[compio::test]
#[awt]
async fn test_tag_changed_while_idling_becomes_remote_keyword(#[future] mail_setup: MailSetup) {
    let Some(tags) = Tags::new(&mail_setup) else {
        return;
    };
    let (_handle, _changes) = mail_setup
        .sync_continuous_with_tags("INBOX", Some(tags.sync.clone()))
        .await;
    // the initial sync of tags completes before idling
    compio::time::sleep(Duration::from_secs(1)).await;

    tags.notmuch(&["tag", "+todo", "--", "folder:INBOX"]);

    let mut waited = Duration::ZERO;
    while server_keywords(&mail_setup).is_empty() {
        assert!(
            waited < Duration::from_secs(10),
            "keyword did not get synced"
        );
        compio::time::sleep(Duration::from_millis(100)).await;
        waited += Duration::from_millis(100);
    }
    assert_eq!(
        server_keywords(&mail_setup),
        BTreeSet::from(["todo".to_string()])
    );
}

#[rstest]
#[compio::test]
#[awt]
async fn test_downloaded_mail_gets_indexed_with_tags(#[future] mail_setup: MailSetup) {
    let Some(tags) = Tags::new(&mail_setup) else {
        return;
    };
    mail_setup.sync_with_tags("INBOX", &tags.sync).await;
    let server_mailbox = mail_setup.server_mail().mailbox("INBOX");
    server_mailbox.add_mail(b"Message-ID: <new@example.com>\r\nSubject: new\r\n\r\nnew mail");
    mail_setup.sync_with_tags("INBOX", &tags.sync).await;
    let tagged = tags.notmuch(&["search", "--output=tags", "id:new@example.com"]);

    assert_eq!(
        tagged.lines().collect::<BTreeSet<_>>(),
        BTreeSet::from(["inbox", "unread"])
    );
}
