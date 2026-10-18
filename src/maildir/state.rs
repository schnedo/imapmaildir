use std::{
    collections::HashSet,
    convert::Into,
    fmt::Debug,
    io,
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
use rusqlite::{Connection, OpenFlags, OptionalExtension, Result, Row};
use rusqlite_migration::Migrations;
use thiserror::Error;

use crate::{
    maildir::{LocalMailMetadata, MaildirFile as _},
    repository::{Flag, ModSeq, Uid, UidValidity},
};

const STATE_FILE_NAME: &str = "imapmaildir.db";
const MIGRATIONS_DIR: Dir = include_dir!("$CARGO_MANIFEST_DIR/migrations");
static MIGRATIONS: LazyLock<Migrations<'static>> = LazyLock::new(|| {
    Migrations::from_directory(&MIGRATIONS_DIR).expect("generating migrations should succeed")
});
const JOB_QUEUE_SIZE: usize = 64;

fn apply_migrations(db: &mut Connection) -> Result<(), InitError> {
    MIGRATIONS.to_latest(db)?;
    db.pragma_update(None, "journal_mode", "wal")?;
    db.pragma_update(None, "synchronous", "normal")?;

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
        ready_rx
            .await
            .expect("state thread should report whether the database could be opened")?;

        Ok(Self { jobs })
    }

    async fn call<R: Send + 'static>(
        &self,
        f: impl FnOnce(&mut Connection) -> R + Send + 'static,
    ) -> R {
        let (result_tx, result_rx) = oneshot();
        self.jobs
            .send(Command::Run(Box::new(move |db: &mut Connection| {
                result_tx.send(f(db));
            })))
            .await
            .expect("state thread should still be running");
        result_rx
            .await
            .expect("state thread should return a result")
    }

    pub async fn close(self) {
        let (closed_tx, closed_rx) = oneshot();
        self.jobs
            .send(Command::Close(closed_tx))
            .await
            .expect("state thread should still be running");
        closed_rx
            .await
            .expect("state thread should signal closing the database");
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

    pub async fn complete_initial_sync(&self, highest_modseq: ModSeq) -> Result<(), Error> {
        log::trace!("completing initial sync with highest_modseq {highest_modseq}");
        self.call(move |db| {
            db.execute(
                "insert into highest_modseq (highest_modseq) values (?1)",
                [i64::from(highest_modseq)],
            )?;

            Ok(())
        })
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

    pub async fn update(&self, data: &LocalMailMetadata) -> Result<(), Error> {
        log::trace!("updating mail cache {data:?}");
        let (flags, uid) = (data.flags().bits(), u32::from(data.uid()));
        self.call(move |db| {
            let mut stmt = db.prepare_cached("update mail_metadata set flags=?1 where uid=?2")?;
            stmt.execute((flags, uid))?;

            Ok(())
        })
        .await
    }

    pub async fn store_all(
        &self,
        data: Vec<LocalMailMetadata>,
    ) -> Result<Vec<LocalMailMetadata>, Error> {
        log::trace!("storing {} mail caches", data.len());
        self.call(move |db| {
            let transaction = db.transaction()?;
            let mut conflicts = Vec::new();
            {
                let mut stmt = transaction.prepare_cached(
                    "insert into mail_metadata (uid,flags,fileprefix) values (?1,?2,?3)",
                )?;
                for entry in data {
                    match stmt.execute((
                        u32::from(entry.uid()),
                        entry.flags().bits(),
                        &entry.fileprefix(),
                    )) {
                        Ok(_) => {}
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

            Ok(conflicts)
        })
        .await
    }

    pub async fn get_by_id(&self, uid: Uid) -> Result<Option<LocalMailMetadata>, Error> {
        log::trace!("get existing metadata with {uid:?}");
        self.call(move |db| {
            let mut stmt = db.prepare_cached("select * from mail_metadata where uid = ?1")?;

            stmt.query_one([u32::from(uid)], |row| row.try_into())
                .optional()
                .map_err(std::convert::Into::into)
        })
        .await
    }

    pub async fn get_all_by_id(&self, uids: Vec<Uid>) -> Result<Vec<LocalMailMetadata>, Error> {
        log::trace!("get existing metadata of {} mails", uids.len());
        self.call(move |db| {
            let mut stmt = db.prepare_cached("select * from mail_metadata where uid = ?1")?;
            let mut entries = Vec::with_capacity(uids.len());
            for uid in uids {
                if let Some(entry) = stmt
                    .query_one([u32::from(uid)], |row| row.try_into())
                    .optional()?
                {
                    entries.push(entry);
                }
            }

            Ok(entries)
        })
        .await
    }

    pub async fn update_all(&self, data: Vec<LocalMailMetadata>) -> Result<(), Error> {
        log::trace!("updating {} mail caches", data.len());
        self.call(move |db| {
            let transaction = db.transaction()?;
            {
                let mut stmt =
                    transaction.prepare_cached("update mail_metadata set flags=?1 where uid=?2")?;
                for entry in data {
                    stmt.execute((entry.flags().bits(), u32::from(entry.uid())))?;
                }
            }
            transaction.commit()?;

            Ok(())
        })
        .await
    }

    pub async fn insert_all(&self, data: Vec<LocalMailMetadata>) -> Result<(), Error> {
        log::trace!("inserting {} mail caches", data.len());
        self.call(move |db| {
            let transaction = db.transaction()?;
            {
                let mut stmt = transaction.prepare_cached(
                    "insert into mail_metadata (uid,flags,fileprefix) values (?1,?2,?3)",
                )?;
                for entry in data {
                    stmt.execute((
                        u32::from(entry.uid()),
                        entry.flags().bits(),
                        &entry.fileprefix(),
                    ))?;
                }
            }
            transaction.commit()?;

            Ok(())
        })
        .await
    }

    pub async fn delete_all_by_id(&self, uids: Vec<Uid>) -> Result<(), Error> {
        log::trace!("deleting {} mail caches", uids.len());
        self.call(move |db| {
            let transaction = db.transaction()?;
            {
                let mut stmt =
                    transaction.prepare_cached("delete from mail_metadata where uid = ?1")?;
                for uid in uids {
                    stmt.execute([u32::from(uid)])?;
                }
            }
            transaction.commit()?;

            Ok(())
        })
        .await
    }

    // todo: test deleting non existent mail should not fail
    pub async fn delete_by_id(&self, uid: Uid) -> Result<(), Error> {
        log::trace!("deleting {uid:?}");
        self.call(move |db| {
            let mut stmt = db.prepare_cached("delete from mail_metadata where uid = ?1")?;
            stmt.execute([u32::from(uid)])?;

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

    pub async fn fore_each(&self, mut cb: impl FnMut(LocalMailMetadata)) -> Result<ModSeq, Error> {
        log::trace!("getting all stored mail metadata");
        let (entries, current_highest_modseq) = self
            .call(|db| {
                let db = db.transaction()?;
                let entries = {
                    let mut stmt =
                        db.prepare_cached("select uid,flags,fileprefix from mail_metadata;")?;
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
        Ok(Self::new(uid, flags, value.get(2)?))
    }
}

#[derive(Debug, Error)]
pub enum Error {
    #[error("Could not parse cached data")]
    Conversion,
    #[error("Error with db call: {0}")]
    Db(rusqlite::Error),
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
    Migrations(rusqlite_migration::Error),
}

impl From<rusqlite_migration::Error> for InitError {
    fn from(value: rusqlite_migration::Error) -> Self {
        Self::Migrations(value)
    }
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
                .complete_initial_sync(highest_modseq)
                .await
        );
        incomplete_state
    }

    #[fixture]
    async fn loadable_state_dir(#[future] state: TestState) -> TempDir {
        let TestState { state, dir } = state.await;
        state.close().await;
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
                .complete_initial_sync(highest_modseq)
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
        assert_ok!(state.state.update(&metadata).await);
        let stored = assert_some!(assert_ok!(state.state.get_by_id(metadata.uid()).await));
        assert_eq!(metadata, stored);
    }

    #[rstest]
    #[compio::test]
    #[awt]
    async fn test_delete_by_uid_succeeds(#[future] state: TestState, metadata: LocalMailMetadata) {
        assert_ok!(state.state.insert_all(vec![metadata.clone()]).await);
        assert_some!(assert_ok!(state.state.get_by_id(metadata.uid()).await));
        assert_ok!(state.state.delete_by_id(metadata.uid()).await);
        assert_none!(assert_ok!(state.state.get_by_id(metadata.uid()).await));
    }

    #[rstest]
    #[compio::test]
    #[awt]
    async fn test_foreach_fails_with_invalid_uid_in_state(#[future] state: TestState) {
        assert_ok!(
            state
                .state
                .call(|db| db.execute_batch(
                    "insert into mail_metadata (uid,flags,fileprefix) values (0,0,\"foo\")"
                ))
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
        let error = rusqlite_migration::Error::InvalidUserVersion;
        let dbinit_error: InitError = error.into();
        assert_matches!(
            dbinit_error,
            InitError::Migrations(rusqlite_migration::Error::InvalidUserVersion)
        );
        let error = Error::Conversion;
        let dbinit_error: InitError = error.into();
        assert_matches!(dbinit_error, InitError::DbError(Error::Conversion));
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
}
