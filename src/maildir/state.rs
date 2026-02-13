use std::{
    convert::Into,
    fs::create_dir_all,
    io,
    path::{Path, PathBuf},
    sync::Arc,
};

use enumflags2::BitFlag;
use log::{debug, trace};
use rusqlite::{Connection, Error, OpenFlags, OptionalExtension, Result, Row};
use thiserror::Error;
use tokio::sync::{
    Mutex,
    mpsc::{self, error::SendError},
};

use crate::{
    maildir::LocalMailMetadata,
    repository::{Flag, ModSeq, Uid, UidValidity},
};

#[derive(Debug, Error)]
pub enum DbError {
    #[error("Could not parse cached data")]
    Conversion,
    #[error("Communication channel between database and imap already closed")]
    ChannelClosed,
    #[error("Error with db call: {0}")]
    Db(rusqlite::Error),
}

impl From<<ModSeq as TryFrom<u64>>::Error> for DbError {
    fn from(_: <ModSeq as TryFrom<u64>>::Error) -> Self {
        Self::Conversion
    }
}

impl From<SendError<LocalMailMetadata>> for DbError {
    fn from(_: SendError<LocalMailMetadata>) -> Self {
        Self::ChannelClosed
    }
}

impl From<rusqlite::Error> for DbError {
    fn from(value: rusqlite::Error) -> Self {
        Self::Db(value)
    }
}

#[derive(Debug, Error)]
pub enum DbInitError {
    #[error("{0}")]
    DbError(DbError),
    #[error("IO Issue when constructing DB {0}")]
    Io(io::Error),
}

impl From<DbError> for DbInitError {
    fn from(value: DbError) -> Self {
        Self::DbError(value)
    }
}

impl From<rusqlite::Error> for DbInitError {
    fn from(value: rusqlite::Error) -> Self {
        Self::DbError(value.into())
    }
}

impl From<io::Error> for DbInitError {
    fn from(value: io::Error) -> Self {
        Self::Io(value)
    }
}

fn get_highest_modseq(db: &Connection) -> Result<ModSeq, DbError> {
    let result = db.query_one("select * from pragma_user_version", [], |row| {
        let modseq: u64 = row.get(0)?;
        let modseq: Result<ModSeq, DbError> = modseq.try_into().map_err(DbError::from);
        Ok(modseq)
    });

    result?
}

fn set_highest_modseq(db: &Connection, value: ModSeq) -> Result<(), rusqlite::Error> {
    trace!("setting highest_modseq {value}");
    db.pragma_update(None, "user_version", u64::from(value))
}

fn get_state_version(db: &Connection) -> Result<u32, rusqlite::Error> {
    db.query_one("select state_version from maildir_info", [], |row| {
        row.get(0)
    })
}

const CURRENT_VERSION: u32 = 1;
const STATE_FILE_NAME: &str = "imapmaildir.db";

#[derive(Clone, Debug)]
pub struct State {
    db: Arc<Mutex<Connection>>,
    cached_highest_modseq: Arc<Mutex<ModSeq>>,
}

impl State {
    fn try_new(db: Connection, highest_modseq: ModSeq) -> Self {
        let cached_highest_modseq = highest_modseq;

        Self {
            db: Arc::new(Mutex::new(db)),
            cached_highest_modseq: Arc::new(Mutex::new(cached_highest_modseq)),
        }
    }

    pub fn load(state_dir: &Path) -> Result<Self, DbInitError> {
        let state_file = Self::prepare_state_file(state_dir)?;
        debug!(
            "try loading existing state file {}",
            state_file.to_string_lossy()
        );
        let db = Connection::open_with_flags(
            state_file,
            OpenFlags::SQLITE_OPEN_READ_WRITE
                | OpenFlags::SQLITE_OPEN_NO_MUTEX
                | OpenFlags::SQLITE_OPEN_URI,
        )?;

        if get_state_version(&db)? != CURRENT_VERSION {
            todo!("handle state version mismatch")
        }

        let highest_modseq = get_highest_modseq(&db)?;
        Ok(Self::try_new(db, highest_modseq))
    }

    pub fn init(
        state_dir: &Path,
        uid_validity: UidValidity,
        highest_modseq: ModSeq,
    ) -> Result<Self, DbInitError> {
        let state_file = Self::prepare_state_file(state_dir)?;
        debug!("creating new state file {}", state_file.to_string_lossy());
        let db = Connection::open(state_file)?;
        db.execute_batch(
            "pragma journal_mode=wal;
            pragma user_version=1;
            pragma synchronous=1;
            create table mail_metadata (
                uid integer primary key,
                flags integer not null,
                fileprefix text not null
            ) strict;
            create table maildir_info (
                uid_validity integer primary key,
                state_version integer not null
            ) strict;
            pragma optimize;",
        )?;
        trace!("setting cached uid_validity {uid_validity}");
        db.execute(
            "insert or ignore into maildir_info (state_version, uid_validity) values (?1, ?2)",
            [CURRENT_VERSION, u32::from(uid_validity)],
        )?;
        set_highest_modseq(&db, highest_modseq)?;

        Ok(Self::try_new(db, highest_modseq))
    }

    fn prepare_state_file(state_dir: &Path) -> io::Result<PathBuf> {
        create_dir_all(state_dir)?;

        Ok(state_dir.join(STATE_FILE_NAME))
    }

    pub async fn uid_validity(&self) -> Result<UidValidity, DbError> {
        trace!("getting cached uid_validity");
        self.db
            .lock()
            .await
            .query_one("select uid_validity from maildir_info", (), |row| {
                let validity: u32 = row.get(0)?;
                let validity = validity.try_into().map_err(DbError::from);
                Ok(validity)
            })?
    }

    // todo: remove cached value and use transaction instead
    pub async fn update_highest_modseq(&self, value: ModSeq) -> Result<(), DbError> {
        trace!(
            "check for updating highest_modseq {:?} with {value:?}",
            self.cached_highest_modseq
        );
        let mut cached_highest_modseq = self.cached_highest_modseq.lock().await;
        if value > *cached_highest_modseq {
            self.set_highest_modseq_uncached(value).await?;
            *cached_highest_modseq = value;
        }

        Ok(())
    }

    async fn set_highest_modseq_uncached(&self, value: ModSeq) -> Result<(), DbError> {
        trace!("setting highest_modseq {value}");
        let db = self.db.lock().await;
        set_highest_modseq(&db, value).map_err(std::convert::Into::into)
    }

    pub async fn set_highest_modseq(&self, value: ModSeq) -> Result<(), DbError> {
        trace!(
            "check for setting highest_modseq {:?} to {value:?}",
            self.cached_highest_modseq
        );
        let mut cached_highest_modseq = self.cached_highest_modseq.lock().await;
        if *cached_highest_modseq != value {
            self.set_highest_modseq_uncached(value).await?;
            *cached_highest_modseq = value;
        }

        Ok(())
    }

    pub async fn highest_modseq(&self) -> Result<ModSeq, DbError> {
        trace!("getting cached highest_modseq");
        let db = self.db.lock().await;
        get_highest_modseq(&db)
    }

    pub async fn update(&self, data: &LocalMailMetadata) -> Result<(), DbError> {
        trace!("updating mail cache {data:?}");
        let db = self.db.lock().await;
        let mut stmt = db.prepare_cached("update mail_metadata set flags=?1 where uid=?2")?;
        stmt.execute((
            data.flags().bits(),
            u32::from(data.uid().expect("invalid input data")),
        ))?;

        Ok(())
    }

    pub async fn store(&self, data: &LocalMailMetadata) -> Result<(), DbError> {
        trace!("storing mail cache {data:?}");
        let uid = data.uid().ok_or(DbError::Conversion)?;
        let db = self.db.lock().await;
        let mut stmt = db
            .prepare_cached("insert into mail_metadata (uid,flags,fileprefix) values (?1,?2,?3)")?;
        stmt.execute((u32::from(uid), data.flags().bits(), &data.fileprefix()))?;

        Ok(())
    }

    pub async fn get_by_id(&self, uid: Uid) -> Result<Option<LocalMailMetadata>, DbError> {
        trace!("get existing metadata with {uid:?}");
        let db = self.db.lock().await;
        let mut stmt = db.prepare_cached("select * from mail_metadata where uid = ?1")?;

        stmt.query_one([u32::from(uid)], |row| row.try_into())
            .optional()
            .map_err(std::convert::Into::into)
    }

    pub async fn delete_by_id(&self, uid: Uid) -> Result<(), DbError> {
        trace!("deleting {uid:?}");
        let db = self.db.lock().await;
        let mut stmt = db.prepare_cached("delete from mail_metadata where uid = ?1")?;
        stmt.execute([u32::from(uid)])?;

        Ok(())
    }

    pub async fn get_all(
        &self,
        all_entries_tx: mpsc::Sender<LocalMailMetadata>,
    ) -> Result<ModSeq, DbError> {
        trace!("getting all stored mail metadata");
        let db = self.db.lock().await;
        let mut stmt = db.prepare_cached("select uid,flags,fileprefix from mail_metadata;")?;

        let current_highest_modseq = *self.cached_highest_modseq.lock().await;
        for entry in stmt
            .query_map([], |row| LocalMailMetadata::try_from(row))?
            .map(|maybe_row| maybe_row.map_err(DbError::from))
        {
            let entry = entry?;
            all_entries_tx.send(entry).await?;
        }

        Ok(current_highest_modseq)
    }
}

impl Drop for State {
    fn drop(&mut self) {
        let db = self
            .db
            .try_lock()
            .expect("db should not be locked when dropping State");
        db.execute("pragma optimize;", [])
            .expect("sqlite should be optimizable");
    }
}

impl TryFrom<&Row<'_>> for LocalMailMetadata {
    type Error = Error;

    fn try_from(value: &Row) -> Result<Self, Self::Error> {
        let uid: u32 = value.get(0)?;
        let uid = Uid::try_from(uid).ok();
        let flags = Flag::from_bits_truncate(value.get(1)?);
        Ok(Self::new(uid, flags, value.get(2)?))
    }
}

#[cfg(test)]
mod tests {
    use std::{fs, os::unix::fs::PermissionsExt};

    use assertables::*;
    use rstest::*;
    use tempfile::{TempDir, tempdir};

    use super::*;

    struct TestState {
        dir: TempDir,
        state: State,
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
    fn state(temp_dir: TempDir, uid_validity: UidValidity, highest_modseq: ModSeq) -> TestState {
        TestState {
            state: assert_ok!(State::init(temp_dir.path(), uid_validity, highest_modseq)),
            dir: temp_dir,
        }
    }

    #[fixture]
    fn loadable_state_dir(state: TestState) -> TempDir {
        state.dir
    }

    #[fixture]
    fn metadata() -> LocalMailMetadata {
        LocalMailMetadata::new(
            Some(assert_ok!(Uid::try_from(3))),
            Flag::all(),
            Some("prefix".to_string()),
        )
    }

    #[rstest]
    fn test_state_init_initializes_db(state: TestState) {
        assert!(assert_ok!(fs::exists(
            state.dir.path().join(STATE_FILE_NAME)
        )));
    }

    #[rstest]
    fn test_state_uses_write_ahead_log(state: TestState) {
        assert!(assert_ok!(fs::exists(
            state.dir.path().join(STATE_FILE_NAME.to_string() + "-wal")
        )));
    }

    #[rstest]
    #[tokio::test]
    async fn test_state_inits_with_correct_highest_modseq(
        state: TestState,
        highest_modseq: ModSeq,
    ) {
        let cached_modseq = assert_ok!(state.state.highest_modseq().await);
        assert_eq!(cached_modseq, highest_modseq);
        assert_eq!(
            cached_modseq,
            *state.state.cached_highest_modseq.lock().await
        );
    }

    #[rstest]
    #[tokio::test]
    async fn test_state_inits_with_correct_uid_validity(
        state: TestState,
        uid_validity: UidValidity,
    ) {
        assert_eq!(assert_ok!(state.state.uid_validity().await), uid_validity);
    }

    #[rstest]
    fn test_state_init_fails_on_not_creatable_state_dir(
        temp_dir: TempDir,
        uid_validity: UidValidity,
        highest_modseq: ModSeq,
    ) {
        let mut permissions = assert_ok!(temp_dir.path().metadata()).permissions();
        permissions.set_mode(0o000);
        assert_ok!(fs::set_permissions(temp_dir.path(), permissions));
        let state_dir = temp_dir.path().join("foo");

        let result = assert_err!(State::init(&state_dir, uid_validity, highest_modseq));
        assert_matches!(result, DbInitError::Io(_));
    }

    #[rstest]
    fn test_state_init_fails_on_not_creatable_state_file(
        temp_dir: TempDir,
        uid_validity: UidValidity,
        highest_modseq: ModSeq,
    ) {
        let mut permissions = assert_ok!(temp_dir.path().metadata()).permissions();
        permissions.set_mode(0o000);
        assert_ok!(fs::set_permissions(temp_dir.path(), permissions));

        let result = assert_err!(State::init(temp_dir.path(), uid_validity, highest_modseq));
        assert_matches!(
            result,
            DbInitError::DbError(DbError::Db(rusqlite::Error::SqliteFailure(_, _)))
        );
    }

    #[rstest]
    #[tokio::test]
    async fn test_load_loads_correct(loadable_state_dir: TempDir, highest_modseq: ModSeq) {
        let result = assert_ok!(State::load(loadable_state_dir.path()));
        assert_eq!(assert_ok!(result.highest_modseq().await), highest_modseq);
        assert_eq!(*result.cached_highest_modseq.lock().await, highest_modseq);
    }

    #[rstest]
    #[tokio::test]
    async fn test_load_errors_on_unreadable_state_file(loadable_state_dir: TempDir) {
        let mut permissions = assert_ok!(fs::metadata(loadable_state_dir.path())).permissions();
        permissions.set_mode(0o000);
        assert_ok!(fs::set_permissions(loadable_state_dir.path(), permissions));
        let result = assert_err!(State::load(loadable_state_dir.path()));
        assert_matches!(
            result,
            DbInitError::DbError(DbError::Db(rusqlite::Error::SqliteFailure(_, _)))
        );
    }

    // #[rstest]
    // #[tokio::test]
    // async fn test_update_highest_modseq_updates_highest_modseq_if_value_is_higher(
    //     state: TestState,
    // ) {
    //     let initial_modseq = assert_ok!(state.state.highest_modseq().await);
    //     todo: do not use user_version for highest_modseq, as modseqs are u63, while user_version
    //     is u32
    //     let new_modseq = assert_ok!(ModSeq::try_from(u64::MAX));
    //     assert_ne!(new_modseq, initial_modseq);
    //     assert_ok!(state.state.update_highest_modseq(new_modseq).await);
    //
    //     assert_eq!(assert_ok!(state.state.highest_modseq().await), new_modseq);
    // }

    #[rstest]
    #[tokio::test]
    async fn test_storing_metadata_succeeds(state: TestState, metadata: LocalMailMetadata) {
        assert_none!(assert_ok!(
            state.state.get_by_id(assert_some!(metadata.uid())).await
        ));
        assert_ok!(state.state.store(&metadata).await);
        let stored = assert_some!(assert_ok!(
            state.state.get_by_id(assert_some!(metadata.uid())).await
        ));
        assert_eq!(metadata, stored);
    }

    #[rstest]
    #[tokio::test]
    async fn test_updating_metadata_succeeds(state: TestState, mut metadata: LocalMailMetadata) {
        assert_ok!(state.state.store(&metadata).await);
        let flags = Flag::Seen | Flag::Deleted;
        assert_ne!(flags, metadata.flags());
        metadata.set_flags(flags);
        assert_ok!(state.state.update(&metadata).await);
        let stored = assert_some!(assert_ok!(
            state.state.get_by_id(assert_some!(metadata.uid())).await
        ));
        assert_eq!(metadata, stored);
    }
}
