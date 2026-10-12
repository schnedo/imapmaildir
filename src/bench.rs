//! Facade over crate internals for `benches/imapmaildir.rs`.
//!
//! Rules to keep benchmark baselines comparable across async runtime changes:
//! - no runtime types in signatures
//! - inputs are owned buffers (`Bytes`), as required by completion based io
//! - runtime is owned by the facade types and created outside of measured code

use std::path::Path;

use bytes::Bytes;
use enumflags2::BitFlags;
use tokio::{
    runtime::{Builder, Runtime},
    sync::mpsc,
};

use crate::{
    imap::{RemoteContent, RemoteMail, RemoteMailMetadata},
    maildir::MaildirRepository,
    repository::{Flag, MailboxMetadata, MailboxMetadataBuilder, ModSeq, Uid},
    sync::{Syncer, Task},
};

#[must_use]
pub fn runtime_info() -> &'static str {
    "tokio:current_thread"
}

fn runtime() -> Runtime {
    Builder::new_current_thread()
        .enable_io()
        .enable_time()
        .build()
        .expect("runtime should be buildable")
}

fn uid(uid: u32) -> Uid {
    Uid::try_from(uid).expect("uid should be non zero")
}

fn flags(flags: &str) -> BitFlags<Flag> {
    flags
        .chars()
        .map(|flag| Flag::try_from(flag).expect("flag should be a maildir flag char"))
        .collect()
}

fn mailbox_metadata() -> MailboxMetadata {
    let mut builder = MailboxMetadataBuilder::default();
    builder.uid_validity(1.try_into().expect("1 should be a valid uid validity"));
    builder.highest_modseq(ModSeq::try_from(1).expect("1 should be a valid modseq"));
    builder
        .build()
        .expect("mailbox metadata should be complete")
}

pub struct Mail(RemoteMail);

impl Mail {
    #[must_use]
    pub fn new(uid_: u32, flags_: &str, modseq: i64, content: Bytes) -> Self {
        Self(RemoteMail::new(
            RemoteMailMetadata::new(
                uid(uid_),
                flags(flags_),
                ModSeq::try_from(modseq).expect("modseq should be positive"),
            ),
            RemoteContent::from_bytes(content),
        ))
    }
}

pub struct Repo {
    rt: Runtime,
    repo: MaildirRepository,
}

impl Repo {
    #[must_use]
    pub fn init(mail_dir: &Path, state_dir: &Path) -> Self {
        Self {
            rt: runtime(),
            repo: MaildirRepository::try_init(&mailbox_metadata(), mail_dir, state_dir)
                .expect("repository should be creatable"),
        }
    }

    pub fn store_all(&self, mails: Vec<Mail>) {
        self.rt.block_on(async {
            for mail in mails {
                self.repo
                    .store(&mail.0)
                    .await
                    .expect("storing mail should succeed");
            }
        });
    }

    pub fn update_flags_all(&self, uids: &[u32], flags_: &str, modseq: i64) {
        let flags_ = flags(flags_);
        let modseq = ModSeq::try_from(modseq).expect("modseq should be positive");
        self.rt.block_on(async {
            for &id in uids {
                self.repo
                    .update_flags(&RemoteMailMetadata::new(uid(id), flags_, modseq))
                    .await
                    .expect("updating flags should succeed");
            }
        });
    }

    pub fn delete_all(&self, uids: &[u32]) {
        self.rt.block_on(async {
            for &id in uids {
                self.repo
                    .delete(uid(id))
                    .await
                    .expect("deleting mail should succeed");
            }
        });
    }

    #[must_use]
    pub fn detect_changes(&self) -> usize {
        self.rt.block_on(async {
            let changes = self
                .repo
                .detect_changes()
                .await
                .expect("detecting changes should succeed");

            changes.news.len() + changes.deletions.len()
        })
    }

    pub fn process_new_mails(&self, mails: Vec<Mail>) {
        self.rt.block_on(async {
            let (task_tx, task_rx) = mpsc::channel(32);
            let processing = Syncer::setup_task_processing(self.repo.clone(), task_rx, || {});
            for mail in mails {
                task_tx
                    .send(Task::NewMail(mail.0))
                    .await
                    .expect("task processing should be running");
            }
            task_tx
                .send(Task::Shutdown)
                .await
                .expect("task processing should be running");
            processing
                .await
                .expect("task processing should finish successfully");
        });
    }
}
