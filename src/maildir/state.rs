use std::{
    collections::{HashMap, HashSet},
    convert::Into,
    ffi::OsString,
    fmt::Debug,
    io,
    os::unix::ffi::OsStringExt as _,
    path::{Path, PathBuf},
    sync::LazyLock,
    thread,
};

use crossfire::{
    MAsyncTx,
    mpsc::{self, Array},
    oneshot::{TxOneshot, oneshot},
};
use enumflags2::BitFlag;
use include_dir::{Dir, include_dir};
use rusqlite::{Connection, OpenFlags, OptionalExtension, Params, Result, Row};
use thiserror::Error;

use crate::{
    maildir::{LocalMailMetadata, MaildirFile as _, OtherFlags},
    notmuch::Revision,
    repository::{Flag, KeywordSource, Keywords, ModSeq, Uid, UidValidity},
};

const STATE_FILE_NAME: &str = "imapmaildir.db";
const MIGRATIONS_DIR: Dir = include_dir!("$CARGO_MANIFEST_DIR/migrations");
static MIGRATIONS: LazyLock<Vec<&'static str>> = LazyLock::new(|| {
    let mut dirs: Vec<_> = MIGRATIONS_DIR.dirs().collect();
    dirs.sort_by_key(|dir| dir.path());
    dirs.into_iter()
        .map(|dir| {
            dir.get_file(dir.path().join("up.sql"))
                .and_then(|file| file.contents_utf8())
                .expect("every migration should contain an utf8 up.sql")
        })
        .collect()
});
const JOB_QUEUE_SIZE: usize = 64;
const GET_ALL_BY_ID_CHUNK_SIZE: usize = 256;
static GET_ALL_BY_ID_PARAMS: LazyLock<String> =
    LazyLock::new(|| vec!["?"; GET_ALL_BY_ID_CHUNK_SIZE].join(","));
static GET_TAGS_PENDING_PAGE_QUERY: LazyLock<String> =
    LazyLock::new(|| format!("{TAGS_PENDING_QUERY} uid > ?1 order by uid limit ?2"));
const TAGS_PENDING_QUERY: &str = "select uid,flags,fileprefix,other_flags,keywords,notmuch_keywords \
    from mail_metadata where (notmuch_keywords is null or notmuch_keywords != keywords) and";

/// Mail, whose keywords differ from the ones last synced to notmuch, with both of them
type TagsPending = (LocalMailMetadata, Keywords, Option<Keywords>);

fn migrate_to_latest(db: &mut Connection) -> Result<(), InitError> {
    let current: u32 = db.pragma_query_value(None, "user_version", |row| row.get(0))?;
    let latest = u32::try_from(MIGRATIONS.len()).expect("migration count should fit into u32");
    if current > latest {
        return Err(InitError::Migrations(MigrationError::TooFarAhead {
            current,
            latest,
        }));
    }
    for (version, migration) in (current + 1..).zip(&MIGRATIONS[current as usize..]) {
        log::debug!("applying migration {version}");
        let tx = db.transaction()?;
        tx.execute_batch(migration)?;
        tx.pragma_update(None, "user_version", version)?;
        tx.commit()?;
    }

    Ok(())
}

fn apply_migrations(db: &mut Connection) -> Result<(), InitError> {
    migrate_to_latest(db)?;
    db.pragma_update(None, "journal_mode", "wal")?;
    // commits must be durable before renaming mails accordingly, otherwise a power loss makes
    // e.g. uploaded mails look untracked, which uploads them again
    db.pragma_update(None, "synchronous", "full")?;

    Ok(())
}

type Job = Box<dyn FnOnce(&mut Connection) + Send>;

enum Command {
    Run(Job),
    Close(TxOneshot<()>),
}

pub struct State {
    jobs: MAsyncTx<Array<Command>>,
}

impl Debug for State {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("State").finish_non_exhaustive()
    }
}

impl State {
    async fn start(
        state_file: PathBuf,
        open: impl FnOnce(&Path) -> Result<Connection, InitError> + Send + 'static,
    ) -> Result<Self, InitError> {
        let (ready_tx, ready_rx) = oneshot();
        let (jobs, job_rx) = mpsc::bounded_async_blocking::<Command>(JOB_QUEUE_SIZE);
        thread::Builder::new()
            .name(format!("state {}", state_file.display()))
            .spawn(move || {
                let mut db = match open(&state_file) {
                    Ok(db) => {
                        ready_tx.send(Ok(()));
                        db
                    }
                    Err(e) => {
                        ready_tx.send(Err(e));
                        return;
                    }
                };
                let mut closed_tx = None;
                while let Ok(command) = job_rx.recv() {
                    match command {
                        Command::Run(job) => job(&mut db),
                        Command::Close(done_tx) => {
                            closed_tx = Some(done_tx);
                            break;
                        }
                    }
                }
                if let Err(e) = db.execute("pragma optimize;", []) {
                    log::warn!("could not optimize state {}: {e}", state_file.display());
                }
                drop(db);
                if let Some(closed_tx) = closed_tx {
                    closed_tx.send(());
                }
            })?;
        ready_rx.await.map_err(|_| Error::ThreadStopped)??;

        Ok(Self { jobs })
    }

    async fn call<T: Send + 'static, E: From<Error> + Send + 'static>(
        &self,
        f: impl FnOnce(&mut Connection) -> Result<T, E> + Send + 'static,
    ) -> Result<T, E> {
        let (result_tx, result_rx) = oneshot();
        self.jobs
            .send(Command::Run(Box::new(move |db: &mut Connection| {
                result_tx.send(f(db));
            })))
            .await
            .map_err(|_| Error::ThreadStopped)?;
        result_rx.await.map_err(|_| Error::ThreadStopped)?
    }

    /// Executes `sql` with each of `params` in one transaction
    async fn execute_all<P: Params>(
        &self,
        sql: &'static str,
        params: impl IntoIterator<Item = P> + Send + 'static,
    ) -> Result<(), Error> {
        self.call(move |db| {
            let transaction = db.transaction()?;
            execute_each(&transaction, sql, params)?;
            transaction.commit()?;

            Ok(())
        })
        .await
    }

    pub async fn close(self) -> Result<(), Error> {
        let (closed_tx, closed_rx) = oneshot();
        self.jobs
            .send(Command::Close(closed_tx))
            .await
            .map_err(|_| Error::ThreadStopped)?;
        closed_rx.await.map_err(|_| Error::ThreadStopped)
    }

    pub async fn load(state_dir: &Path) -> Result<Self, InitError> {
        let state_file = Self::prepare_state_file(state_dir).await?;
        log::debug!(
            "try loading existing state file {}",
            state_file.to_string_lossy()
        );
        if try_exists(&state_file).await? {
            Self::start(state_file, |state_file| {
                let mut db = Connection::open_with_flags(
                    state_file,
                    OpenFlags::SQLITE_OPEN_READ_WRITE
                        | OpenFlags::SQLITE_OPEN_NO_MUTEX
                        | OpenFlags::SQLITE_OPEN_URI,
                )?;
                apply_migrations(&mut db)?;

                Ok(db)
            })
            .await
        } else {
            Err(InitError::Missing(state_file))
        }
    }

    pub async fn init(state_dir: &Path, uid_validity: UidValidity) -> Result<Self, InitError> {
        let state_file = Self::prepare_state_file(state_dir).await?;
        log::debug!("creating new state file {}", state_file.to_string_lossy());
        Self::start(state_file, move |state_file| {
            let mut db = Connection::open(state_file)?;
            apply_migrations(&mut db)?;

            log::trace!("setting cached uid_validity {uid_validity}");
            db.execute(
                "insert or ignore into uid_validity (uid_validity) values (?1)",
                [u32::from(uid_validity)],
            )?;

            Ok(db)
        })
        .await
    }

    /// Completes the initial sync, which fetched keywords from `keyword_source`
    pub async fn complete_initial_sync(
        &self,
        highest_modseq: ModSeq,
        keyword_source: KeywordSource,
    ) -> Result<(), Error> {
        log::trace!("completing initial sync with highest_modseq {highest_modseq}");
        self.call(move |db| {
            let transaction = db.transaction()?;
            transaction.execute(
                "insert into highest_modseq (highest_modseq) values (?1)",
                [i64::from(highest_modseq)],
            )?;
            set_keyword_source(&transaction, keyword_source)?;
            transaction.commit()?;

            Ok(())
        })
        .await
    }

    /// Where the stored keywords came from. `None` for states of versions not storing keywords.
    pub async fn keyword_source(&self) -> Result<Option<KeywordSource>, Error> {
        self.call(|db| {
            let source: Option<String> = db
                .query_one("select source from keyword_source", [], |row| row.get(0))
                .optional()?;

            Ok(source.and_then(|source| KeywordSource::parse(&source)))
        })
        .await
    }

    pub async fn set_keyword_source(&self, keyword_source: KeywordSource) -> Result<(), Error> {
        self.call(move |db| {
            let transaction = db.transaction()?;
            set_keyword_source(&transaction, keyword_source)?;
            transaction.commit()?;

            Ok(())
        })
        .await
    }

    /// Keywords of stored mails among `uids`
    pub async fn keywords(&self, uids: Vec<Uid>) -> Result<HashMap<Uid, Keywords>, Error> {
        log::trace!("get keywords of {} mails", uids.len());
        self.call(move |db| {
            let mut keywords = HashMap::with_capacity(uids.len());
            for_each_by_id(
                db,
                "select uid,keywords from mail_metadata where",
                &uids,
                |row| {
                    keywords.insert(
                        uid_of(row)?,
                        Keywords::from_stored(&row.get::<_, String>(1)?),
                    );
                    Ok(())
                },
            )?;

            Ok(keywords)
        })
        .await
    }

    /// Keywords last synced to notmuch of stored mails among `uids`, `None` if never synced,
    /// and the ones, whose tags notmuch has, but which stay local
    pub async fn notmuch_keywords(
        &self,
        uids: Vec<Uid>,
    ) -> Result<HashMap<Uid, (Option<Keywords>, Keywords)>, Error> {
        self.call(move |db| {
            let mut keywords = HashMap::with_capacity(uids.len());
            for_each_by_id(
                db,
                "select uid,notmuch_keywords,notmuch_local from mail_metadata where",
                &uids,
                |row| {
                    let stored: Option<String> = row.get(1)?;
                    let local: String = row.get(2)?;
                    keywords.insert(
                        uid_of(row)?,
                        (
                            stored.as_deref().map(Keywords::from_stored),
                            Keywords::from_stored(&local),
                        ),
                    );
                    Ok(())
                },
            )?;

            Ok(keywords)
        })
        .await
    }

    /// Mails among `uids` with tags pending
    pub async fn tags_pending_of(&self, uids: Vec<Uid>) -> Result<Vec<TagsPending>, Error> {
        self.call(move |db| {
            let mut pending = Vec::new();
            for_each_by_id(db, TAGS_PENDING_QUERY, &uids, |row| {
                pending.push(read_tags_pending(row)?);
                Ok(())
            })?;

            Ok(pending)
        })
        .await
    }

    /// Up to `limit` mails with tags pending and UIDs after `after`
    pub async fn tags_pending(&self, after: u32, limit: usize) -> Result<Vec<TagsPending>, Error> {
        self.call(move |db| {
            let mut stmt = db.prepare_cached(&GET_TAGS_PENDING_PAGE_QUERY)?;
            stmt.query_map(
                (after, i64::try_from(limit).unwrap_or(i64::MAX)),
                read_tags_pending,
            )?
            .map(|pending| Ok(pending?))
            .collect()
        })
        .await
    }

    /// Records keywords as synced to notmuch
    pub async fn set_notmuch_keywords_all(&self, data: Vec<(Uid, Keywords)>) -> Result<(), Error> {
        log::trace!("updating notmuch keywords of {} mail caches", data.len());
        self.execute_all(
            "update mail_metadata set notmuch_keywords=?1 where uid=?2",
            keywords_params(data),
        )
        .await
    }

    /// Records keywords, whose tags notmuch has, but which stay local
    pub async fn set_notmuch_local_all(&self, data: Vec<(Uid, Keywords)>) -> Result<(), Error> {
        self.execute_all(
            "update mail_metadata set notmuch_local=?1 where uid=?2",
            keywords_params(data),
        )
        .await
    }

    /// Forgets the keywords synced to notmuch, e.g. after its database got recreated, and the
    /// revision tags got synced up to
    pub async fn forget_notmuch_keywords(&self) -> Result<(), Error> {
        self.call(|db| {
            let transaction = db.transaction()?;
            transaction.execute(
                "update mail_metadata set notmuch_keywords=null, notmuch_local=''",
                [],
            )?;
            transaction.execute("delete from tags_revision", [])?;
            transaction.commit()?;

            Ok(())
        })
        .await
    }

    /// Revision of the notmuch database, up to which changed tags got synced, with the
    /// fingerprint of the tag mapping used
    pub async fn tags_revision(&self) -> Result<Option<(Revision, String)>, Error> {
        self.call(|db| {
            Ok(db
                .query_one(
                    "select uuid,revision,mapping from tags_revision",
                    [],
                    |row| {
                        Ok((
                            Revision {
                                uuid: row.get(0)?,
                                number: row.get::<_, i64>(1)?.cast_unsigned(),
                            },
                            row.get(2)?,
                        ))
                    },
                )
                .optional()?)
        })
        .await
    }

    pub async fn set_tags_revision(
        &self,
        revision: Revision,
        mapping: String,
    ) -> Result<(), Error> {
        self.call(move |db| {
            let transaction = db.transaction()?;
            transaction.execute("delete from tags_revision", [])?;
            transaction.execute(
                "insert into tags_revision (uuid,revision,mapping) values (?1,?2,?3)",
                (&revision.uuid, revision.number.cast_signed(), &mapping),
            )?;
            transaction.commit()?;

            Ok(())
        })
        .await
    }

    /// Files to remove from notmuch, that got deleted before `deleted` seconds since the epoch
    pub async fn notmuch_removals(&self, deleted: i64) -> Result<Vec<PathBuf>, Error> {
        self.call(move |db| {
            let mut stmt =
                db.prepare_cached("select path from notmuch_removals where deleted <= ?1")?;
            stmt.query_map([deleted], |row| row.get::<_, Vec<u8>>(0))?
                .map(|path| Ok(PathBuf::from(OsString::from_vec(path?))))
                .collect()
        })
        .await
    }

    pub async fn delete_notmuch_removals(&self, paths: Vec<PathBuf>) -> Result<(), Error> {
        self.execute_all(
            "delete from notmuch_removals where path = ?1",
            paths
                .into_iter()
                .map(|path| [path.into_os_string().into_vec()]),
        )
        .await
    }

    pub async fn update_keywords_all(&self, data: Vec<(Uid, Keywords)>) -> Result<(), Error> {
        log::trace!("updating keywords of {} mail caches", data.len());
        self.execute_all(
            "update mail_metadata set keywords=?1 where uid=?2",
            keywords_params(data),
        )
        .await
    }

    pub async fn is_initial_sync_complete(&self) -> Result<bool, Error> {
        self.call(|db| {
            db.query_one("select exists (select 1 from highest_modseq)", [], |row| {
                row.get(0)
            })
            .map_err(Into::into)
        })
        .await
    }

    pub async fn remove_from(state_dir: &Path) -> io::Result<()> {
        if try_exists(state_dir).await? {
            let state_file = state_dir.join(STATE_FILE_NAME);
            if try_exists(&state_file).await? {
                compio::fs::remove_file(&state_file).await?;
            }
            if let Err(e) = compio::fs::remove_dir(state_dir).await
                && e.kind() != io::ErrorKind::DirectoryNotEmpty
            {
                log::warn!(
                    "Could not remove empty state directory {}: {e}",
                    state_dir.display()
                );
            }
        }

        Ok(())
    }

    async fn prepare_state_file(state_dir: &Path) -> io::Result<PathBuf> {
        compio::fs::create_dir_all(state_dir).await?;

        Ok(state_dir.join(STATE_FILE_NAME))
    }

    pub async fn uid_validity(&self) -> Result<UidValidity, Error> {
        log::trace!("getting cached uid_validity");
        self.call(|db| {
            db.query_one("select uid_validity from uid_validity", (), |row| {
                let validity: u32 = row.get(0)?;
                let validity = validity.try_into().map_err(Error::from);
                Ok(validity)
            })?
        })
        .await
    }

    pub async fn update_highest_modseq(&self, value: ModSeq) -> Result<(), Error> {
        log::trace!("check for updating highest_modseq with {value:?}");
        self.call(move |db| {
            let transaction = db.transaction()?;
            let highest_modseq = get_highest_modseq(&transaction)?;
            if value > highest_modseq {
                set_highest_modseq(&transaction, value)?;
            }
            transaction.commit()?;

            Ok(())
        })
        .await
    }

    pub async fn highest_modseq(&self) -> Result<ModSeq, Error> {
        log::trace!("getting cached highest_modseq");
        self.call(|db| get_highest_modseq(db)).await
    }

    /// Stores new mails with their `keywords`. Returns the stored ones and conflicting ones,
    /// whose UID is already stored.
    pub async fn store_all(
        &self,
        data: Vec<LocalMailMetadata>,
        mut keywords: HashMap<Uid, Keywords>,
    ) -> Result<(Vec<LocalMailMetadata>, Vec<LocalMailMetadata>), Error> {
        log::trace!("storing {} mail caches", data.len());
        self.call(move |db| {
            let transaction = db.transaction()?;
            let mut stored = Vec::with_capacity(data.len());
            let mut conflicts = Vec::new();
            {
                let mut stmt = transaction.prepare_cached(
                    "insert into mail_metadata (uid,flags,fileprefix,other_flags,keywords) values (?1,?2,?3,?4,?5)",
                )?;
                for entry in data {
                    let keywords = keywords
                        .remove(&entry.uid())
                        .map(|keywords| keywords.to_stored())
                        .unwrap_or_default();
                    match stmt.execute((
                        u32::from(entry.uid()),
                        entry.flags().bits(),
                        &entry.fileprefix(),
                        entry.other_flags().as_str(),
                        keywords,
                    )) {
                        Ok(_) => stored.push(entry),
                        Err(rusqlite::Error::SqliteFailure(
                            rusqlite::ffi::Error {
                                code: rusqlite::ErrorCode::ConstraintViolation,
                                ..
                            },
                            _,
                        )) => conflicts.push(entry),
                        Err(e) => return Err(e.into()),
                    }
                }
            }
            transaction.commit()?;

            Ok((stored, conflicts))
        })
        .await
    }

    pub async fn get_by_id(&self, uid: Uid) -> Result<Option<LocalMailMetadata>, Error> {
        log::trace!("get existing metadata with {uid:?}");
        self.call(move |db| {
            let mut stmt = db.prepare_cached(
                "select uid,flags,fileprefix,other_flags from mail_metadata where uid = ?1",
            )?;

            stmt.query_one([u32::from(uid)], |row| row.try_into())
                .optional()
                .map_err(std::convert::Into::into)
        })
        .await
    }

    pub async fn get_all_by_id(&self, uids: Vec<Uid>) -> Result<Vec<LocalMailMetadata>, Error> {
        log::trace!("get existing metadata of {} mails", uids.len());
        self.call(move |db| {
            let mut entries = Vec::with_capacity(uids.len());
            for_each_by_id(
                db,
                "select uid,flags,fileprefix,other_flags from mail_metadata where",
                &uids,
                |row| {
                    entries.push(row.try_into()?);
                    Ok(())
                },
            )?;

            Ok(entries)
        })
        .await
    }

    pub async fn update_all(&self, data: Vec<LocalMailMetadata>) -> Result<(), Error> {
        log::trace!("updating {} mail caches", data.len());
        self.call(move |db| {
            let transaction = db.transaction()?;
            {
                let mut stmt = transaction.prepare_cached(
                    "update mail_metadata set flags=?1, other_flags=?2 where uid=?3",
                )?;
                for entry in data {
                    stmt.execute((
                        entry.flags().bits(),
                        entry.other_flags().as_str(),
                        u32::from(entry.uid()),
                    ))?;
                }
            }
            transaction.commit()?;

            Ok(())
        })
        .await
    }

    /// Updates flags without IMAP equivalent, which other clients changed in the maildir
    pub async fn update_other_flags_all(&self, data: Vec<(Uid, OtherFlags)>) -> Result<(), Error> {
        log::trace!("updating other flags of {} mail caches", data.len());
        self.execute_all(
            "update mail_metadata set other_flags=?1 where uid=?2",
            data.into_iter()
                .map(|(uid, other_flags)| (other_flags.into_string(), u32::from(uid))),
        )
        .await
    }

    /// Inserts mails without keywords
    pub async fn insert_all(&self, data: Vec<LocalMailMetadata>) -> Result<(), Error> {
        log::trace!("inserting {} mail caches", data.len());
        self.call(move |db| {
            let transaction = db.transaction()?;
            {
                let mut stmt = transaction.prepare_cached(
                    "insert into mail_metadata (uid,flags,fileprefix,other_flags) values (?1,?2,?3,?4)",
                )?;
                for entry in data {
                    stmt.execute((
                        u32::from(entry.uid()),
                        entry.flags().bits(),
                        &entry.fileprefix(),
                        entry.other_flags().as_str(),
                    ))?;
                }
            }
            transaction.commit()?;

            Ok(())
        })
        .await
    }

    pub async fn delete_all_by_id(&self, uids: Vec<Uid>) -> Result<(), Error> {
        self.delete_all_by_id_removing(uids, Vec::new(), 0).await
    }

    /// Deletes mails and remembers their files at `paths`, deleted at `deleted` seconds since
    /// the epoch, for removing them from notmuch, also after failures
    pub async fn delete_all_by_id_removing(
        &self,
        uids: Vec<Uid>,
        paths: Vec<PathBuf>,
        deleted: i64,
    ) -> Result<(), Error> {
        log::trace!("deleting {} mail caches", uids.len());
        self.call(move |db| {
            let transaction = db.transaction()?;
            execute_each(
                &transaction,
                "delete from mail_metadata where uid = ?1",
                uids.into_iter().map(|uid| [u32::from(uid)]),
            )?;
            execute_each(
                &transaction,
                "insert or replace into notmuch_removals (path,deleted) values (?1,?2)",
                paths
                    .into_iter()
                    .map(|path| (path.into_os_string().into_vec(), deleted)),
            )?;
            transaction.commit()?;

            Ok(())
        })
        .await
    }

    pub async fn uids(&self) -> Result<HashSet<Uid>, Error> {
        log::trace!("getting all stored uids");
        self.call(|db| {
            let mut stmt = db.prepare_cached("select uid from mail_metadata;")?;
            stmt.query_map([], |row| row.get::<_, u32>(0))?
                .map(|uid| Uid::try_from(uid?).map_err(|_| Error::Conversion))
                .collect()
        })
        .await
    }

    /// Stored UIDs missing from the ascending `uids` in ascending order, e.g. mails deleted
    /// remotely
    pub async fn uids_not_in(&self, uids: Vec<Uid>) -> Result<Vec<Uid>, Error> {
        log::trace!("getting stored uids not in {} uids", uids.len());
        debug_assert!(uids.is_sorted());
        self.call(move |db| {
            // merging in rust is several times faster than a temp table or rarray in sqlite
            let mut remaining = uids.iter().copied().peekable();
            let mut missing = Vec::new();
            let mut stmt = db.prepare_cached("select uid from mail_metadata order by uid")?;
            for stored in stmt.query_map([], |row| row.get::<_, u32>(0))? {
                let stored = Uid::try_from(stored?).map_err(|_| Error::Conversion)?;
                while remaining.next_if(|uid| *uid < stored).is_some() {}
                if remaining.next_if_eq(&stored).is_none() {
                    missing.push(stored);
                }
            }

            Ok(missing)
        })
        .await
    }

    pub async fn fore_each(&self, mut cb: impl FnMut(LocalMailMetadata)) -> Result<ModSeq, Error> {
        log::trace!("getting all stored mail metadata");
        let (entries, current_highest_modseq) = self
            .call(|db| {
                let db = db.transaction()?;
                let entries = {
                    let mut stmt = db.prepare_cached(
                        "select uid,flags,fileprefix,other_flags from mail_metadata;",
                    )?;
                    stmt.query_map([], |row| LocalMailMetadata::try_from(row))?
                        .collect::<Result<Vec<_>, _>>()?
                };
                let current_highest_modseq = get_highest_modseq(&db)?;

                Ok::<_, Error>((entries, current_highest_modseq))
            })
            .await?;
        for entry in entries {
            cb(entry);
        }

        Ok(current_highest_modseq)
    }
}

async fn try_exists(path: &Path) -> io::Result<bool> {
    match compio::fs::metadata(path).await {
        Ok(_) => Ok(true),
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(false),
        Err(e) => Err(e),
    }
}

impl TryFrom<&Row<'_>> for LocalMailMetadata {
    type Error = rusqlite::Error;

    fn try_from(value: &Row) -> Result<Self, Self::Error> {
        let uid_column = 0;
        let uid: u32 = value.get(uid_column)?;
        let uid = Uid::try_from(uid).map_err(|e| {
            rusqlite::Error::FromSqlConversionFailure(
                uid_column,
                rusqlite::types::Type::Integer,
                Box::new(e),
            )
        })?;
        let flags = Flag::from_bits_truncate(value.get(1)?);
        let mut metadata = Self::new(uid, flags, value.get(2)?);
        metadata.set_other_flags(OtherFlags::from(value.get::<_, String>(3)?));

        Ok(metadata)
    }
}

#[derive(Debug, Error)]
pub enum Error {
    #[error("Could not parse cached data")]
    Conversion,
    #[error("Error with db call: {0}")]
    Db(rusqlite::Error),
    #[error("State thread stopped")]
    ThreadStopped,
}

impl From<<ModSeq as TryFrom<i64>>::Error> for Error {
    fn from(_: <ModSeq as TryFrom<i64>>::Error) -> Self {
        Self::Conversion
    }
}

impl From<rusqlite::Error> for Error {
    fn from(value: rusqlite::Error) -> Self {
        Self::Db(value)
    }
}

#[derive(Debug, Error)]
pub enum InitError {
    #[error("{0}")]
    DbError(Error),
    #[error("No state found at {0}")]
    Missing(PathBuf),
    #[error("IO Issue when constructing DB {0}")]
    Io(io::Error),
    #[error("Could not apply migrations {0}")]
    Migrations(MigrationError),
}

#[derive(Debug, Error)]
pub enum MigrationError {
    #[error("database schema version {current} is newer than latest known version {latest}")]
    TooFarAhead { current: u32, latest: u32 },
}

impl From<Error> for InitError {
    fn from(value: Error) -> Self {
        Self::DbError(value)
    }
}

impl From<rusqlite::Error> for InitError {
    fn from(value: rusqlite::Error) -> Self {
        Self::DbError(value.into())
    }
}

impl From<io::Error> for InitError {
    fn from(value: io::Error) -> Self {
        Self::Io(value)
    }
}

fn get_highest_modseq(db: &Connection) -> Result<ModSeq, Error> {
    let result = db.query_one("select highest_modseq from highest_modseq", [], |row| {
        let modseq: i64 = row.get(0)?;
        let modseq: Result<ModSeq, Error> = modseq.try_into().map_err(Error::from);
        Ok(modseq)
    });

    result?
}

fn set_keyword_source(db: &Connection, source: KeywordSource) -> Result<(), rusqlite::Error> {
    log::trace!("setting keyword source {source:?}");
    db.execute("delete from keyword_source", [])?;
    db.execute(
        "insert into keyword_source (source) values (?1)",
        [source.as_str()],
    )?;

    Ok(())
}

fn execute_each<P: Params>(
    db: &Connection,
    sql: &str,
    params: impl IntoIterator<Item = P>,
) -> Result<(), rusqlite::Error> {
    let mut stmt = db.prepare_cached(sql)?;
    for params in params {
        stmt.execute(params)?;
    }

    Ok(())
}

fn keywords_params(data: Vec<(Uid, Keywords)>) -> impl Iterator<Item = (String, u32)> + Send {
    data.into_iter()
        .map(|(uid, keywords)| (keywords.to_stored(), u32::from(uid)))
}

/// Calls `f` with the rows of `select` followed by a condition on the UIDs of `uids`, which
/// gets queried in chunks
fn for_each_by_id(
    db: &Connection,
    select: &str,
    uids: &[Uid],
    mut f: impl FnMut(&Row) -> Result<(), Error>,
) -> Result<(), Error> {
    let mut stmt = db.prepare_cached(&format!("{select} uid in ({})", *GET_ALL_BY_ID_PARAMS))?;
    for chunk in uids.chunks(GET_ALL_BY_ID_CHUNK_SIZE) {
        let params = (0..GET_ALL_BY_ID_CHUNK_SIZE)
            .map(|i| u32::from(chunk.get(i).unwrap_or(&chunk[0]).to_owned()));
        let mut rows = stmt.query(rusqlite::params_from_iter(params))?;
        while let Some(row) = rows.next()? {
            f(row)?;
        }
    }

    Ok(())
}

fn uid_of(row: &Row) -> Result<Uid, Error> {
    Uid::try_from(row.get::<_, u32>(0)?).map_err(|_| Error::Conversion)
}

fn read_tags_pending(row: &Row) -> Result<TagsPending, rusqlite::Error> {
    let keywords: String = row.get(4)?;
    let synced: Option<String> = row.get(5)?;
    Ok((
        LocalMailMetadata::try_from(row)?,
        Keywords::from_stored(&keywords),
        synced.as_deref().map(Keywords::from_stored),
    ))
}

fn set_highest_modseq(db: &Connection, value: ModSeq) -> Result<(), rusqlite::Error> {
    log::trace!("setting highest_modseq {value}");
    let mut stmt = db.prepare_cached("update highest_modseq set highest_modseq=?1")?;
    stmt.execute([i64::from(value)])?;

    Ok(())
}

#[cfg(test)]
mod tests {
    use std::{fs, os::unix::fs::PermissionsExt};

    use assertables::*;
    use rstest::*;
    use tempfile::{TempDir, tempdir};

    use super::*;

    struct TestState {
        state: State,
        dir: TempDir,
    }

    #[fixture]
    fn temp_dir() -> TempDir {
        assert_ok!(tempdir())
    }

    #[fixture]
    fn uid_validity() -> UidValidity {
        assert_ok!(UidValidity::try_from(8))
    }

    #[fixture]
    fn highest_modseq() -> ModSeq {
        assert_ok!(ModSeq::try_from(83))
    }

    #[fixture]
    async fn incomplete_state(temp_dir: TempDir, uid_validity: UidValidity) -> TestState {
        TestState {
            state: assert_ok!(State::init(temp_dir.path(), uid_validity).await),
            dir: temp_dir,
        }
    }

    #[fixture]
    async fn state(#[future] incomplete_state: TestState, highest_modseq: ModSeq) -> TestState {
        let incomplete_state = incomplete_state.await;
        assert_ok!(
            incomplete_state
                .state
                .complete_initial_sync(highest_modseq, KeywordSource::Flags)
                .await
        );
        incomplete_state
    }

    #[fixture]
    async fn loadable_state_dir(#[future] state: TestState) -> TempDir {
        let TestState { state, dir } = state.await;
        assert_ok!(state.close().await);
        dir
    }

    #[fixture]
    fn metadata() -> LocalMailMetadata {
        LocalMailMetadata::new(
            assert_ok!(Uid::try_from(3)),
            Flag::all(),
            Some("prefix".to_string()),
        )
    }

    #[rstest]
    #[compio::test]
    #[awt]
    async fn test_get_all_by_id_returns_existing_entries_across_chunks(#[future] state: TestState) {
        let uid = |uid: u32| assert_ok!(Uid::try_from(uid));
        let stored: Vec<_> = (1..=600)
            .map(|i| LocalMailMetadata::new(uid(i), Flag::Seen.into(), Some(i.to_string())))
            .collect();
        assert_is_empty!(assert_ok!(state.state.store_all(stored.clone(), HashMap::new()).await).1);
        let requested = (300..=700).map(uid).collect();

        let mut entries = assert_ok!(state.state.get_all_by_id(requested).await);

        entries.sort_by_key(LocalMailMetadata::uid);
        assert_eq!(entries, stored[299..]);
    }

    #[rstest]
    #[compio::test]
    #[awt]
    async fn test_state_init_initializes_db(#[future] state: TestState) {
        assert!(assert_ok!(fs::exists(
            state.dir.path().join(STATE_FILE_NAME)
        )));
    }

    #[rstest]
    #[compio::test]
    #[awt]
    async fn test_state_uses_write_ahead_log(#[future] state: TestState) {
        assert!(assert_ok!(fs::exists(
            state.dir.path().join(STATE_FILE_NAME.to_string() + "-wal")
        )));
    }

    #[rstest]
    #[compio::test]
    #[awt]
    async fn test_state_inits_correctly(
        #[future] state: TestState,
        highest_modseq: ModSeq,
        uid_validity: UidValidity,
    ) {
        let cached_modseq = assert_ok!(state.state.highest_modseq().await);
        assert_eq!(cached_modseq, highest_modseq);
        assert_eq!(assert_ok!(state.state.uid_validity().await), uid_validity);
    }

    #[rstest]
    #[compio::test]
    async fn test_state_init_fails_on_not_creatable_state_dir(
        temp_dir: TempDir,
        uid_validity: UidValidity,
    ) {
        let mut permissions = assert_ok!(temp_dir.path().metadata()).permissions();
        permissions.set_mode(0o000);
        assert_ok!(fs::set_permissions(temp_dir.path(), permissions));
        let state_dir = temp_dir.path().join("foo");

        let result = assert_err!(State::init(&state_dir, uid_validity).await);
        assert_matches!(result, InitError::Io(_));
    }

    #[rstest]
    #[compio::test]
    async fn test_state_init_fails_on_not_creatable_state_file(
        temp_dir: TempDir,
        uid_validity: UidValidity,
    ) {
        let mut permissions = assert_ok!(temp_dir.path().metadata()).permissions();
        permissions.set_mode(0o000);
        assert_ok!(fs::set_permissions(temp_dir.path(), permissions));

        let result = assert_err!(State::init(temp_dir.path(), uid_validity).await);
        assert_matches!(
            result,
            InitError::DbError(Error::Db(rusqlite::Error::SqliteFailure(_, _)))
        );
    }

    #[rstest]
    #[compio::test]
    #[awt]
    async fn test_load_loads_correct(
        #[future] loadable_state_dir: TempDir,
        highest_modseq: ModSeq,
        uid_validity: UidValidity,
    ) {
        let result = assert_ok!(State::load(loadable_state_dir.path()).await);
        assert_eq!(assert_ok!(result.highest_modseq().await), highest_modseq);
        assert_eq!(assert_ok!(result.uid_validity().await), uid_validity);
    }

    #[rstest]
    #[compio::test]
    #[awt]
    async fn test_initial_sync_is_incomplete_after_init(#[future] incomplete_state: TestState) {
        assert!(!assert_ok!(
            incomplete_state.state.is_initial_sync_complete().await
        ));
        assert_err!(incomplete_state.state.highest_modseq().await);
    }

    #[rstest]
    #[compio::test]
    #[awt]
    async fn test_complete_initial_sync_completes_initial_sync(
        #[future] incomplete_state: TestState,
        highest_modseq: ModSeq,
        uid_validity: UidValidity,
    ) {
        assert_ok!(
            incomplete_state
                .state
                .complete_initial_sync(highest_modseq, KeywordSource::Flags)
                .await
        );

        assert!(assert_ok!(
            incomplete_state.state.is_initial_sync_complete().await
        ));
        assert_eq!(
            assert_ok!(incomplete_state.state.highest_modseq().await),
            highest_modseq
        );
        assert_eq!(
            assert_ok!(incomplete_state.state.uid_validity().await),
            uid_validity
        );
    }

    #[rstest]
    #[compio::test]
    #[awt]
    async fn test_uids_returns_stored_uids(
        #[future] state: TestState,
        metadata: LocalMailMetadata,
    ) {
        assert_ok!(state.state.insert_all(vec![metadata.clone()]).await);

        assert_eq!(
            assert_ok!(state.state.uids().await),
            HashSet::from([metadata.uid()])
        );
    }

    #[rstest]
    #[compio::test]
    #[awt]
    async fn test_uids_not_in_returns_missing_uids_sorted(#[future] state: TestState) {
        let uid = |uid: u32| assert_ok!(Uid::try_from(uid));
        let entries = [5, 1, 3, 8]
            .into_iter()
            .map(|u| LocalMailMetadata::new(uid(u), Flag::Seen.into(), Some(format!("prefix{u}"))))
            .collect();
        assert_ok!(state.state.insert_all(entries).await);

        assert_eq!(
            assert_ok!(
                state
                    .state
                    .uids_not_in(vec![3, 3, 9].into_iter().map(uid).collect())
                    .await
            ),
            vec![uid(1), uid(5), uid(8)]
        );
        assert_eq!(
            assert_ok!(state.state.uids_not_in(vec![uid(1), uid(5), uid(8)]).await),
            vec![uid(3)]
        );
        assert_eq!(
            assert_ok!(state.state.uids_not_in(Vec::new()).await),
            vec![uid(1), uid(3), uid(5), uid(8)]
        );
        let remote: Vec<_> = vec![1, 2, 3, 4, 5, 8, 10].into_iter().map(uid).collect();
        assert_is_empty!(assert_ok!(state.state.uids_not_in(remote).await));
        assert_eq!(assert_ok!(state.state.uids().await).len(), 4);
    }

    #[rstest]
    #[compio::test]
    #[awt]
    async fn test_keywords_are_stored_and_updated(
        #[future] state: TestState,
        metadata: LocalMailMetadata,
    ) {
        let uid = metadata.uid();
        let keywords: Keywords = ["$Junk", "my label"]
            .into_iter()
            .map(str::to_string)
            .collect();
        assert_ok!(
            state
                .state
                .store_all(vec![metadata], HashMap::from([(uid, keywords.clone())]))
                .await
        );
        assert_eq!(
            assert_ok!(state.state.keywords(vec![uid]).await),
            HashMap::from([(uid, keywords)])
        );

        assert_ok!(
            state
                .state
                .update_keywords_all(vec![(uid, Keywords::default())])
                .await
        );

        assert_eq!(
            assert_ok!(state.state.keywords(vec![uid]).await),
            HashMap::from([(uid, Keywords::default())])
        );
    }

    #[rstest]
    #[compio::test]
    #[awt]
    async fn test_keyword_source_is_set_by_initial_sync_and_updated(
        #[future] incomplete_state: TestState,
        highest_modseq: ModSeq,
    ) {
        assert_none!(assert_ok!(incomplete_state.state.keyword_source().await));

        assert_ok!(
            incomplete_state
                .state
                .complete_initial_sync(highest_modseq, KeywordSource::GmailLabels)
                .await
        );
        assert_eq!(
            assert_ok!(incomplete_state.state.keyword_source().await),
            Some(KeywordSource::GmailLabels)
        );

        assert_ok!(
            incomplete_state
                .state
                .set_keyword_source(KeywordSource::Flags)
                .await
        );
        assert_eq!(
            assert_ok!(incomplete_state.state.keyword_source().await),
            Some(KeywordSource::Flags)
        );
    }

    #[rstest]
    #[compio::test]
    #[awt]
    async fn test_load_errors_on_unreadable_state_file(#[future] loadable_state_dir: TempDir) {
        let mut permissions = assert_ok!(fs::metadata(loadable_state_dir.path())).permissions();
        permissions.set_mode(0o000);
        assert_ok!(fs::set_permissions(loadable_state_dir.path(), permissions));
        let result = assert_err!(State::load(loadable_state_dir.path()).await);
        assert_matches!(result, InitError::Io(_));
    }

    #[rstest]
    #[compio::test]
    async fn test_load_errors_on_missing_state_file(temp_dir: TempDir) {
        let result = assert_err!(State::load(temp_dir.path()).await);
        assert_matches!(result, InitError::Missing(_));
    }

    #[rstest]
    #[compio::test]
    #[awt]
    async fn test_update_highest_modseq_updates_highest_modseq_if_value_is_higher(
        #[future] state: TestState,
    ) {
        let initial_modseq = assert_ok!(state.state.highest_modseq().await);
        let new_modseq = assert_ok!(ModSeq::try_from(i64::MAX));
        assert_gt!(new_modseq, initial_modseq);
        assert_ok!(state.state.update_highest_modseq(new_modseq).await);

        assert_eq!(assert_ok!(state.state.highest_modseq().await), new_modseq);
    }

    #[rstest]
    #[compio::test]
    #[awt]
    async fn test_update_highest_modseq_does_not_update_if_value_is_lower(
        #[future] state: TestState,
    ) {
        let initial_modseq = assert_ok!(state.state.highest_modseq().await);
        let new_modseq = assert_ok!(ModSeq::try_from(1));
        assert_le!(new_modseq, initial_modseq);
        assert_ok!(state.state.update_highest_modseq(new_modseq).await);

        assert_eq!(
            assert_ok!(state.state.highest_modseq().await),
            initial_modseq
        );
    }

    #[rstest]
    #[compio::test]
    #[awt]
    async fn test_storing_metadata_succeeds(
        #[future] state: TestState,
        metadata: LocalMailMetadata,
    ) {
        assert_none!(assert_ok!(state.state.get_by_id(metadata.uid()).await));
        assert_ok!(state.state.insert_all(vec![metadata.clone()]).await);
        let stored = assert_some!(assert_ok!(state.state.get_by_id(metadata.uid()).await));
        assert_eq!(metadata, stored);
    }

    #[rstest]
    #[compio::test]
    #[awt]
    async fn test_updating_metadata_succeeds(
        #[future] state: TestState,
        mut metadata: LocalMailMetadata,
    ) {
        assert_ok!(state.state.insert_all(vec![metadata.clone()]).await);
        let flags = Flag::Seen | Flag::Deleted;
        assert_ne!(flags, metadata.flags());
        metadata.set_flags(flags);
        assert_ok!(state.state.update_all(vec![metadata.clone()]).await);
        let stored = assert_some!(assert_ok!(state.state.get_by_id(metadata.uid()).await));
        assert_eq!(metadata, stored);
    }

    #[rstest]
    #[compio::test]
    #[awt]
    async fn test_other_flags_are_stored_and_updated(
        #[future] state: TestState,
        mut metadata: LocalMailMetadata,
    ) {
        metadata.set_other_flags(OtherFlags::from("P".to_string()));
        assert_ok!(state.state.insert_all(vec![metadata.clone()]).await);
        let stored = assert_some!(assert_ok!(state.state.get_by_id(metadata.uid()).await));
        assert_eq!(metadata, stored);

        let other_flags = OtherFlags::from("Pa".to_string());
        assert_ok!(
            state
                .state
                .update_other_flags_all(vec![(metadata.uid(), other_flags.clone())])
                .await
        );
        let stored = assert_some!(assert_ok!(state.state.get_by_id(metadata.uid()).await));
        assert_eq!(stored.other_flags(), &other_flags);
        assert_eq!(stored.flags(), metadata.flags());
    }

    #[rstest]
    #[compio::test]
    #[awt]
    async fn test_delete_by_uid_succeeds(#[future] state: TestState, metadata: LocalMailMetadata) {
        assert_ok!(state.state.insert_all(vec![metadata.clone()]).await);
        assert_some!(assert_ok!(state.state.get_by_id(metadata.uid()).await));
        assert_ok!(state.state.delete_all_by_id(vec![metadata.uid()]).await);
        assert_none!(assert_ok!(state.state.get_by_id(metadata.uid()).await));
    }

    #[rstest]
    #[compio::test]
    #[awt]
    async fn test_delete_by_uid_of_missing_mail_succeeds(
        #[future] state: TestState,
        metadata: LocalMailMetadata,
    ) {
        assert_none!(assert_ok!(state.state.get_by_id(metadata.uid()).await));
        assert_ok!(state.state.delete_all_by_id(vec![metadata.uid()]).await);
    }

    #[rstest]
    #[compio::test]
    #[awt]
    async fn test_foreach_fails_with_invalid_uid_in_state(#[future] state: TestState) {
        assert_ok!(
            state
                .state
                .call(|db| db
                    .execute_batch(
                        "insert into mail_metadata (uid,flags,fileprefix) values (0,0,\"foo\")"
                    )
                    .map_err(Error::from))
                .await
        );
        assert_err!(state.state.fore_each(|_| {}).await);
    }

    #[rstest]
    #[compio::test]
    #[awt]
    async fn test_foreach_gets_all_stored_data(
        #[future] state: TestState,
        metadata: LocalMailMetadata,
    ) {
        assert_ok!(state.state.insert_all(vec![metadata.clone()]).await);
        let stored_first = assert_some!(assert_ok!(state.state.get_by_id(metadata.uid()).await));
        let metadata = metadata.set_uid(assert_ok!(Uid::try_from(9)));
        assert_ok!(state.state.insert_all(vec![metadata.clone()]).await);
        let stored_second = assert_some!(assert_ok!(state.state.get_by_id(metadata.uid()).await));

        let mut stored = HashSet::new();
        assert_ok!(
            state
                .state
                .fore_each(|data| {
                    stored.insert(data);
                })
                .await
        );
        assert_contains!(stored, &stored_first);
        assert_contains!(stored, &stored_second);
    }

    #[rstest]
    fn test_dbiniterror_conversions() {
        let error = Error::Conversion;
        let dbinit_error: InitError = error.into();
        assert_matches!(dbinit_error, InitError::DbError(Error::Conversion));
    }

    #[rstest]
    fn test_migrations_are_applied_in_order_and_idempotent() {
        let mut db = assert_ok!(Connection::open_in_memory());
        assert_ok!(migrate_to_latest(&mut db));
        let version: usize =
            assert_ok!(db.pragma_query_value(None, "user_version", |row| row.get::<_, u32>(0)))
                as usize;
        assert_eq!(version, MIGRATIONS.len());
        assert_ok!(migrate_to_latest(&mut db));
    }

    #[rstest]
    fn test_migrations_reject_newer_schema() {
        let mut db = assert_ok!(Connection::open_in_memory());
        assert_ok!(db.pragma_update(
            None,
            "user_version",
            assert_ok!(u32::try_from(MIGRATIONS.len())) + 1
        ));
        let error = assert_err!(migrate_to_latest(&mut db));
        assert_matches!(
            error,
            InitError::Migrations(MigrationError::TooFarAhead { .. })
        );
    }

    #[rstest]
    fn test_dberror_conversions() {
        let modseq_error = assert_err!(ModSeq::try_from(0));
        let db_error: Error = modseq_error.into();
        assert_matches!(db_error, Error::Conversion);
    }

    #[rstest]
    #[compio::test]
    #[awt]
    async fn test_delete_from_deletes_state_file(#[future] loadable_state_dir: TempDir) {
        assert_ok!(State::remove_from(loadable_state_dir.path()).await);
        let state_file = loadable_state_dir.path().join(STATE_FILE_NAME);
        assert!(!assert_ok!(state_file.try_exists()));
    }

    #[rstest]
    #[compio::test]
    #[awt]
    async fn test_delete_from_deletes_state_dir_if_empty(#[future] loadable_state_dir: TempDir) {
        assert_ok!(State::remove_from(loadable_state_dir.path()).await);
        assert!(!assert_ok!(loadable_state_dir.path().try_exists()));
    }

    #[rstest]
    #[compio::test]
    #[awt]
    async fn test_delete_from_keeps_state_dir_if_not_empty(#[future] loadable_state_dir: TempDir) {
        assert_ok!(fs::write(loadable_state_dir.path().join("afasdfas"), ""));
        assert_ok!(State::remove_from(loadable_state_dir.path()).await);
        assert!(assert_ok!(loadable_state_dir.path().try_exists()));
    }

    #[rstest]
    #[compio::test]
    #[awt]
    async fn test_delete_from_errors_if_state_file_cannot_be_accessed(
        #[future] loadable_state_dir: TempDir,
    ) {
        let mut permissions = assert_ok!(loadable_state_dir.path().metadata()).permissions();
        permissions.set_mode(0o000);
        assert_ok!(fs::set_permissions(&loadable_state_dir, permissions));

        let result = assert_err!(State::remove_from(loadable_state_dir.path()).await);

        assert_matches!(result, io::Error { .. });
    }

    #[rstest]
    #[compio::test]
    async fn test_delete_from_errors_if_state_dir_cannot_be_accessed(temp_dir: TempDir) {
        let state_dir = temp_dir.path().join("kljlkfsajd");
        assert_ok!(fs::create_dir(&state_dir));
        let mut permissions = assert_ok!(temp_dir.path().metadata()).permissions();
        permissions.set_mode(0o000);
        assert_ok!(fs::set_permissions(&temp_dir, permissions));

        let result = assert_err!(State::remove_from(&state_dir).await);

        assert_matches!(result, io::Error { .. });
    }

    #[rstest]
    #[compio::test]
    #[awt]
    async fn test_drop_does_not_panic_if_database_file_already_deleted(#[future] state: TestState) {
        let TestState { state, dir } = state;
        drop(dir);
        drop(state);
    }

    #[test]
    fn test_pending_tags_get_found_without_scanning_all_mails() {
        let mut db = assert_ok!(Connection::open_in_memory());
        assert_ok!(migrate_to_latest(&mut db));

        let plan: String = assert_ok!(db.query_one(
            &format!("explain query plan {}", *GET_TAGS_PENDING_PAGE_QUERY),
            (0, 1),
            |row| row.get(3),
        ));

        assert_contains!(plan, "USING INDEX mail_metadata_tags_pending");
    }
}
