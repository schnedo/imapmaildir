use std::{
    collections::{BTreeSet, HashSet},
    path::{Path, PathBuf},
};

use crate::notmuch::{
    TagMapping,
    database::{Database, Error, Message},
};

/// How new messages get indexed, like `notmuch new` does
#[derive(Debug, Clone, Default)]
pub struct IndexOptions {
    /// Tags of new messages, except synced ones, which only come from the server
    new_tags: Vec<String>,
    /// Synced tags of new messages, which `notmuch new` may have added
    synced_new_tags: BTreeSet<String>,
    /// Whether to update tags from maildir flags
    synchronize_flags: bool,
}

impl IndexOptions {
    /// # Errors
    /// If the configuration cannot be read
    pub fn of(database: &Database, mapping: &TagMapping) -> Result<Self, Error> {
        let (synced_new_tags, new_tags): (Vec<String>, Vec<String>) = database
            .new_tags()
            .into_iter()
            .partition(|tag| mapping.is_synced(tag));
        Ok(Self {
            new_tags,
            synced_new_tags: synced_new_tags.into_iter().collect(),
            synchronize_flags: database.synchronizes_flags()?,
        })
    }

    /// Indexes the mail at `path`. Returns its message and whether it is new to notmuch.
    /// `None` if it is no mail or vanished.
    fn index<'db>(
        &self,
        database: &'db Database,
        path: &Path,
    ) -> Result<Option<(Message<'db>, bool)>, Error> {
        let Some((message, new)) = database.index_file(path)? else {
            return Ok(None);
        };
        message.frozen(|message| {
            if new {
                for tag in &self.new_tags {
                    message.add_tag(tag)?;
                }
            }
            if self.synchronize_flags {
                message.maildir_flags_to_tags()?;
            }

            Ok(())
        })?;

        Ok(Some((message, new)))
    }
}

/// Synced tags of the mail at `path`
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TagUpdate {
    pub path: PathBuf,
    /// Synced tags notmuch got last. If unknown, `tags` only get added.
    pub synced: Option<BTreeSet<String>>,
    pub tags: BTreeSet<String>,
}

/// Applies `update` to the message of its file, indexing the file if needed. Only changes tags,
/// that differ from the last synced ones, so that unsynced changes in notmuch stay. Without
/// known synced tags, tags only get added. Returns `None` if the file vanished, otherwise the
/// synced tags of `new.tags`, that the message has without the server having them. As they may
/// come from `notmuch new`, they should stay local.
///
/// # Errors
/// If notmuch fails
pub fn update(
    database: &Database,
    mapping: &TagMapping,
    options: &IndexOptions,
    update: &TagUpdate,
) -> Result<Option<BTreeSet<String>>, Error> {
    let (message, synced) = match database.find_by_filename(&update.path)? {
        Some(message) => (message, update.synced.as_ref()),
        None if !update.path.exists() => return Ok(None),
        // e.g. renamed by other programs, so that notmuch only knows the message under another
        // file name, or new to notmuch, e.g. as its database got recreated
        None => match options.index(database, &update.path)? {
            Some((message, false)) => {
                forget_vanished(database, &message, &update.path)?;
                // changing tags of the message read before would restore the vanished files
                drop(message);
                let Some(message) = database.find_by_filename(&update.path)? else {
                    return Ok(None);
                };
                (message, update.synced.as_ref())
            }
            Some((message, true)) => (message, None),
            None => return Ok(None),
        },
    };
    let current: BTreeSet<String> = message.tags()?.into_iter().collect();
    let (added, removed): (BTreeSet<&String>, BTreeSet<&String>) = match synced {
        Some(synced) => (
            update.tags.difference(synced).collect(),
            synced.difference(&update.tags).collect(),
        ),
        // other synced tags are changes in notmuch, which get synced to the server
        None => (update.tags.difference(&current).collect(), BTreeSet::new()),
    };
    let local = if synced.is_none() {
        current
            .iter()
            .filter(|tag| {
                options.synced_new_tags.contains(*tag)
                    && mapping.is_synced(tag)
                    && !update.tags.contains(*tag)
            })
            .cloned()
            .collect()
    } else {
        BTreeSet::new()
    };
    message.frozen(|message| {
        for tag in added.into_iter().filter(|tag| !current.contains(*tag)) {
            message.add_tag(tag)?;
        }
        for tag in removed.into_iter().filter(|tag| current.contains(*tag)) {
            message.remove_tag(tag)?;
        }

        Ok(())
    })?;

    Ok(Some(local))
}

/// Moves the indexed file `from` to `to`, indexing it if notmuch does not know it yet
///
/// # Errors
/// If notmuch fails
pub fn rename(
    database: &Database,
    options: &IndexOptions,
    from: &Path,
    to: &Path,
) -> Result<(), Error> {
    // removing the last file would remove the message with its tags
    let Some((message, _)) = options.index(database, to)? else {
        // e.g. renamed again by other programs. Other files keep the message.
        if !from.exists()
            && let Some(message) = database.find_by_filename(from)?
            && message.filenames()?.len() > 1
        {
            database.remove_filename(from)?;
        }
        return Ok(());
    };
    forget_vanished(database, &message, to)?;
    drop(message);
    database.remove_filename(from)?;
    // tags of maildir flags got updated with the flags of both files
    if options.synchronize_flags
        && let Some(message) = database.find_by_filename(to)?
    {
        message.maildir_flags_to_tags()?;
    }

    Ok(())
}

/// Removes files of `message` in the maildir of its file at `path`, that vanished. These were
/// renamed without notmuch knowing, e.g. when moving new mails from `new` to `cur`. Files in other
/// maildirs stay, as they may get renamed by syncing them.
fn forget_vanished(database: &Database, message: &Message<'_>, path: &Path) -> Result<(), Error> {
    let Some(maildir) = path.parent().and_then(Path::parent) else {
        return Ok(());
    };
    for filename in message.filenames()? {
        if filename != path
            && filename.parent().and_then(Path::parent) == Some(maildir)
            && !filename.exists()
        {
            log::debug!("removing vanished {} from notmuch", filename.display());
            database.remove_filename(&filename)?;
        }
    }

    Ok(())
}

/// Mails of `folder` with tags changed in the revisions `from` to `to` and their synced tags
///
/// # Errors
/// If querying notmuch fails
pub fn changed(
    database: &Database,
    mapping: &TagMapping,
    folder: &Path,
    from: u64,
    to: u64,
) -> Result<Vec<(PathBuf, BTreeSet<String>)>, Error> {
    let Some(query) = folder_query(database, folder, from, to) else {
        return Ok(Vec::new());
    };
    log::debug!("querying notmuch for {query}");
    let mut changed = Vec::new();
    database.search(&query, |message| {
        let tags: BTreeSet<String> = message
            .tags()?
            .into_iter()
            .filter(|tag| mapping.is_synced(tag))
            .collect();
        for filename in message.filenames()? {
            if filename.parent() == Some(folder) {
                changed.push((filename, tags.clone()));
            }
        }

        Ok(())
    })?;

    Ok(changed)
}

/// Folders with files of mails changed in the revisions `from` to `to`, e.g. their tags
///
/// # Errors
/// If querying notmuch fails
pub fn changed_folders(database: &Database, from: u64, to: u64) -> Result<HashSet<PathBuf>, Error> {
    let mut folders = HashSet::new();
    database.search(&format!("lastmod:{from}..{to}"), |message| {
        folders.extend(
            message
                .filenames()?
                .iter()
                .filter_map(|filename| filename.parent().map(Path::to_path_buf)),
        );

        Ok(())
    })?;

    Ok(folders)
}

/// Query of messages of `folder` changed in the revisions `from` to `to`. `None` if the folder
/// is outside the mail root.
fn folder_query(database: &Database, folder: &Path, from: u64, to: u64) -> Option<String> {
    let root = database.mail_root()?;
    let relative = folder.strip_prefix(&root).ok()?;

    Some(format!(
        "lastmod:{from}..{to} and (path:{})",
        quote(&relative.to_string_lossy())
    ))
}

/// Quotes `value` for notmuch queries
fn quote(value: &str) -> String {
    format!("\"{}\"", value.replace('"', "\"\""))
}

#[cfg(test)]
pub mod tests {
    use std::fs;

    use assertables::*;
    use tempfile::TempDir;

    use super::*;
    use crate::{
        notmuch::{Location, TagConfig, ffi},
        repository::KeywordSource,
    };

    /// Notmuch database in a temporary mail root without configuration file
    pub struct TestDatabase {
        pub root: TempDir,
    }

    impl TestDatabase {
        /// `None` if libnotmuch cannot be loaded. Tests needing it pass without it, logging a
        /// warning.
        pub fn if_available() -> Option<Self> {
            if let Err(e) = ffi::library() {
                eprintln!("skipping test needing libnotmuch: {e}");
                return None;
            }
            let root = assert_ok!(TempDir::new());
            assert_ok!(assert_ok!(Database::create(root.path())).close());
            Some(Self { root })
        }

        pub fn location(&self) -> Location {
            Location {
                config: Some(PathBuf::new()),
                profile: None,
                database: Some(self.root.path().to_path_buf()),
            }
        }

        pub fn open(&self, writable: bool) -> Database {
            assert_ok!(Database::open(&self.location(), writable))
        }

        /// Writes a mail with `id` to `folder` below the mail root
        pub fn mail(&self, folder: &str, filename: &str, id: &str) -> PathBuf {
            let dir = self.root.path().join(folder);
            assert_ok!(fs::create_dir_all(&dir));
            let path = dir.join(filename);
            assert_ok!(fs::write(
                &path,
                format!("Message-ID: <{id}>\nFrom: a@example.com\nSubject: test\n\nbody\n"),
            ));
            path
        }

        pub fn tags(&self, path: &Path) -> BTreeSet<String> {
            let database = self.open(false);
            let message = assert_some!(assert_ok!(database.find_by_filename(path)));
            assert_ok!(message.tags()).into_iter().collect()
        }

        /// File names of the message of `path`
        pub fn filenames(&self, path: &Path) -> Vec<PathBuf> {
            let database = self.open(false);
            let message = assert_some!(assert_ok!(database.find_by_filename(path)));
            assert_ok!(message.filenames())
        }

        pub fn tag(&self, path: &Path, tag: &str) {
            self.with_message(path, |message| message.add_tag(tag));
        }

        pub fn untag(&self, path: &Path, tag: &str) {
            self.with_message(path, |message| message.remove_tag(tag));
        }

        fn with_message(&self, path: &Path, f: impl FnOnce(&Message<'_>) -> Result<(), Error>) {
            let database = self.open(true);
            let message = assert_some!(assert_ok!(database.find_by_filename(path)));
            assert_ok!(f(&message));
            drop(message);
            assert_ok!(database.close());
        }

        /// Indexes `path` without tagging it, like `notmuch new` before adding its tags
        pub fn index_file(&self, path: &Path) {
            let database = self.open(true);
            assert_some!(assert_ok!(database.index_file(path)));
            assert_ok!(database.close());
        }

        pub fn is_indexed(&self, path: &Path) -> bool {
            let database = self.open(false);
            assert_ok!(database.find_by_filename(path)).is_some()
        }
    }

    pub fn set(tags: &[&str]) -> BTreeSet<String> {
        tags.iter().map(|tag| (*tag).to_string()).collect()
    }

    fn mapping() -> TagMapping {
        TagMapping::new(&TagConfig::default(), KeywordSource::Flags)
    }

    fn gmail() -> TagMapping {
        TagMapping::new(&TagConfig::default(), KeywordSource::GmailLabels)
    }

    /// Updates the tags of `path`. Returns whether it did not vanish.
    fn index(database: &TestDatabase, path: &Path, synced: Option<&[&str]>, tags: &[&str]) -> bool {
        update_with(database, &mapping(), path, synced, tags).is_some()
    }

    fn update_with(
        database: &TestDatabase,
        mapping: &TagMapping,
        path: &Path,
        synced: Option<&[&str]>,
        tags: &[&str],
    ) -> Option<BTreeSet<String>> {
        let opened = database.open(true);
        let options = assert_ok!(IndexOptions::of(&opened, mapping));
        let updated = assert_ok!(super::update(
            &opened,
            mapping,
            &options,
            &TagUpdate {
                path: path.to_path_buf(),
                synced: synced.map(set),
                tags: set(tags),
            },
        ));
        assert_ok!(opened.close());
        updated
    }

    fn rename_indexed(database: &TestDatabase, from: &Path, to: &Path) {
        let opened = database.open(true);
        let options = assert_ok!(IndexOptions::of(&opened, &mapping()));
        assert_ok!(rename(&opened, &options, from, to));
        assert_ok!(opened.close());
    }

    #[test]
    fn test_update_indexes_new_mail_with_new_tags_and_flags() {
        let Some(database) = TestDatabase::if_available() else {
            return;
        };
        let path = database.mail("INBOX/cur", "1,U=1:2,", "one@example.com");

        let updated = index(&database, &path, None, &["work"]);

        assert!(updated);
        // without configuration, new mails get `inbox` and `unread`, which `synchronize_flags`
        // keeps for mails without `S` flag
        assert_eq!(database.tags(&path), set(&["inbox", "unread", "work"]));
    }

    #[test]
    fn test_update_merges_synced_tags_without_known_ones() {
        let Some(database) = TestDatabase::if_available() else {
            return;
        };
        let path = database.mail("INBOX/cur", "1,U=1:2,S", "one@example.com");
        index(&database, &path, None, &["old"]);

        index(&database, &path, None, &["work"]);

        assert_eq!(database.tags(&path), set(&["inbox", "old", "work"]));
    }

    #[test]
    fn test_update_without_known_tags_keeps_synced_new_tags_local() {
        let Some(database) = TestDatabase::if_available() else {
            return;
        };
        // like indexed by `notmuch new`, which added `inbox`
        let path = database.mail("INBOX/cur", "1,U=1:2,", "one@example.com");
        database.index_file(&path);
        database.tag(&path, "inbox");

        let kept = assert_some!(update_with(&database, &gmail(), &path, None, &["work"]));

        // `inbox` may come from `notmuch new`, so it neither gets removed nor synced
        assert_eq!(kept, set(&["inbox"]));
        assert_eq!(database.tags(&path), set(&["inbox", "work"]));
    }

    #[test]
    fn test_update_keeps_unsynced_changes_with_known_tags() {
        let Some(database) = TestDatabase::if_available() else {
            return;
        };
        let path = database.mail("INBOX/cur", "1,U=1:2,S", "one@example.com");
        index(&database, &path, None, &["a", "b"]);
        database.tag(&path, "local");

        index(&database, &path, Some(&["a", "b"]), &["b", "c"]);

        assert_eq!(database.tags(&path), set(&["b", "c", "inbox", "local"]));
    }

    #[test]
    fn test_update_skips_vanished_mail() {
        let Some(database) = TestDatabase::if_available() else {
            return;
        };

        let updated = index(
            &database,
            &database.root.path().join("INBOX/cur/1,U=1:2,S"),
            None,
            &["a"],
        );

        assert!(!updated);
    }

    #[test]
    fn test_rename_moves_file_of_message() {
        let Some(database) = TestDatabase::if_available() else {
            return;
        };
        let from = database.mail("INBOX/cur", "1,U=1:2,", "one@example.com");
        index(&database, &from, None, &["a"]);
        let to = from.with_file_name("1,U=1:2,S");
        assert_ok!(fs::rename(&from, &to));

        rename_indexed(&database, &from, &to);

        assert!(!database.is_indexed(&from));
        assert_eq!(database.filenames(&to), vec![to.clone()]);
        // the seen flag removes `unread` with `synchronize_flags`
        assert_eq!(database.tags(&to), set(&["a", "inbox"]));
    }

    #[test]
    fn test_update_without_known_tags_keeps_tags_of_other_copies() {
        let Some(database) = TestDatabase::if_available() else {
            return;
        };
        let inbox = database.mail("INBOX/cur", "1,U=1:2,S", "one@example.com");
        let sent = database.mail("Sent/cur", "1,U=1:2,S", "one@example.com");
        index(&database, &inbox, None, &["work"]);

        index(&database, &sent, None, &["sent-only"]);

        assert_eq!(database.tags(&inbox), set(&["inbox", "sent-only", "work"]));
    }

    #[test]
    fn test_update_without_known_tags_keeps_new_tags_of_other_copies() {
        let Some(database) = TestDatabase::if_available() else {
            return;
        };
        let other = database.mail("Other/cur", "1,U=1:2,S", "one@example.com");
        database.index_file(&other);
        database.tag(&other, "inbox");
        let copy = database.mail("INBOX/cur", "1,U=1:2,S", "one@example.com");

        assert_some!(update_with(&database, &gmail(), &copy, None, &["sent"]));

        assert_eq!(database.tags(&copy), set(&["inbox", "sent"]));
    }

    #[test]
    fn test_rename_to_vanished_file_removes_vanished_copy() {
        let Some(database) = TestDatabase::if_available() else {
            return;
        };
        let other = database.mail("Other/cur", "1,U=1:2,S", "one@example.com");
        let from = database.mail("INBOX/cur", "1,U=1:2,S", "one@example.com");
        for path in [&other, &from] {
            index(&database, path, None, &["a"]);
        }
        // renamed twice by other programs, before notmuch learned the first rename
        assert_ok!(fs::remove_file(&from));

        rename_indexed(&database, &from, &from.with_file_name("1,U=1:2,FS"));

        assert_eq!(database.filenames(&other), vec![other.clone()]);
    }

    #[test]
    fn test_rename_forgets_file_moved_from_new_to_cur_before() {
        let Some(database) = TestDatabase::if_available() else {
            return;
        };
        let delivered = database.mail("INBOX/new", "1.delivered", "one@example.com");
        index(&database, &delivered, None, &[]);
        database.tag(&delivered, "todo");
        // moved to cur without telling notmuch, then uploaded and renamed with its uid
        let moved = database.root.path().join("INBOX/cur/1.delivered:2,");
        assert_ok!(fs::create_dir_all(assert_some!(moved.parent())));
        assert_ok!(fs::rename(&delivered, &moved));
        let uploaded = moved.with_file_name("1.delivered,U=1:2,");
        assert_ok!(fs::rename(&moved, &uploaded));

        rename_indexed(&database, &moved, &uploaded);

        assert_eq!(database.filenames(&uploaded), vec![uploaded.clone()]);
        assert!(database.tags(&uploaded).contains("todo"));
    }

    #[test]
    fn test_update_forgets_file_moved_from_new_to_cur_while_changing_tags() {
        let Some(database) = TestDatabase::if_available() else {
            return;
        };
        let delivered = database.mail("INBOX/new", "1.delivered", "one@example.com");
        index(&database, &delivered, None, &[]);
        let moved = database.root.path().join("INBOX/cur/1.delivered,U=1:2,");
        assert_ok!(fs::create_dir_all(assert_some!(moved.parent())));
        assert_ok!(fs::rename(&delivered, &moved));

        index(&database, &moved, Some(&[]), &["work"]);

        assert_eq!(database.filenames(&moved), vec![moved.clone()]);
        assert!(database.tags(&moved).contains("work"));
    }

    #[test]
    fn test_rename_keeps_vanished_files_of_other_maildirs() {
        let Some(database) = TestDatabase::if_available() else {
            return;
        };
        let other = database.mail("Other/cur", "1,U=1:2,S", "one@example.com");
        let from = database.mail("INBOX/cur", "1,U=1:2,", "one@example.com");
        for path in [&other, &from] {
            index(&database, path, None, &[]);
        }
        // e.g. renamed in the other mailbox, which syncs that itself
        assert_ok!(fs::remove_file(&other));
        let to = from.with_file_name("1,U=1:2,S");
        assert_ok!(fs::rename(&from, &to));

        rename_indexed(&database, &from, &to);

        let mut filenames = database.filenames(&to);
        filenames.sort();
        assert_eq!(filenames, vec![to.clone(), other.clone()]);
    }

    #[test]
    fn test_rename_removes_tags_of_flags_removed_from_file() {
        let Some(database) = TestDatabase::if_available() else {
            return;
        };
        let from = database.mail("INBOX/cur", "1,U=1:2,FS", "one@example.com");
        index(&database, &from, None, &[]);
        assert_eq!(database.tags(&from), set(&["flagged", "inbox"]));
        let to = from.with_file_name("1,U=1:2,");
        assert_ok!(fs::rename(&from, &to));

        rename_indexed(&database, &from, &to);

        assert_eq!(database.tags(&to), set(&["inbox", "unread"]));
    }

    #[test]
    fn test_changed_reports_synced_tags_of_folder_since_revision() {
        let Some(database) = TestDatabase::if_available() else {
            return;
        };
        let inbox = database.mail("INBOX/cur", "1,U=1:2,S", "one@example.com");
        let other = database.mail("Other/cur", "1,U=1:2,S", "two@example.com");
        for path in [&inbox, &other] {
            index(&database, path, None, &[]);
        }
        let before = database.open(false).revision();
        database.tag(&inbox, "work");
        database.tag(&other, "work");
        let after = database.open(false).revision();
        let folder = database.root.path().join("INBOX/cur");

        let opened = database.open(false);
        let changed_between =
            |from, to| assert_ok!(changed(&opened, &mapping(), &folder, from, to));

        assert_eq!(
            changed_between(before.number + 1, after.number),
            vec![(inbox, set(&["work"]))]
        );
        assert_is_empty!(changed_between(after.number + 1, after.number + 101));
    }

    #[test]
    fn test_changed_folders_have_files_of_changed_mails() {
        let Some(database) = TestDatabase::if_available() else {
            return;
        };
        let inbox = database.mail("INBOX/cur", "1,U=1:2,S", "one@example.com");
        let other = database.mail("Other/cur", "1,U=1:2,S", "two@example.com");
        for path in [&inbox, &other] {
            index(&database, path, None, &[]);
        }
        let before = database.open(false).revision();
        database.tag(&other, "work");
        let after = database.open(false).revision();

        let folders = assert_ok!(changed_folders(
            &database.open(false),
            before.number + 1,
            after.number
        ));

        assert_eq!(
            folders,
            HashSet::from([database.root.path().join("Other/cur")])
        );
    }
}
