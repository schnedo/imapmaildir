use std::{
    convert::Into,
    fs::create_dir_all,
    io,
    path::{Path, PathBuf},
    sync::{Arc, LazyLock},
};

use enumflags2::BitFlag;
use include_dir::{Dir, include_dir};
use log::{debug, trace};
use rusqlite::{Connection, Error, OpenFlags, OptionalExtension, Result, Row};
use rusqlite_migration::Migrations;
use thiserror::Error;
use tokio::sync::{
    Mutex,
    mpsc::{self, error::SendError},
};

use crate::{
    maildir::LocalMailMetadata,
    repository::{Flag, MailboxMetadata, ModSeq, Uid, UidValidity},
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

impl From<<ModSeq as TryFrom<i64>>::Error> for DbError {
    fn from(_: <ModSeq as TryFrom<i64>>::Error) -> Self {
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
    #[error("Could not apply migrations {0}")]
    Migrations(rusqlite_migration::Error),
}

impl From<rusqlite_migration::Error> for DbInitError {
    fn from(value: rusqlite_migration::Error) -> Self {
        Self::Migrations(value)
    }
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
    let result = db.query_one("select highest_modseq from mailbox_metadata", [], |row| {
        let modseq: i64 = row.get(0)?;
        let modseq: Result<ModSeq, DbError> = modseq.try_into().map_err(DbError::from);
        Ok(modseq)
    });

    result?
}

fn set_highest_modseq(db: &Connection, value: ModSeq) -> Result<(), rusqlite::Error> {
    trace!("setting highest_modseq {value}");
    let mut stmt = db.prepare_cached("update mailbox_metadata set highest_modseq=?1")?;
    stmt.execute([i64::from(value)])?;

    Ok(())
}

const STATE_FILE_NAME: &str = "imapmaildir.db";
const MIGRATIONS_DIR: Dir = include_dir!("$CARGO_MANIFEST_DIR/migrations");
static MIGRATIONS: LazyLock<Migrations<'static>> = LazyLock::new(|| {
    Migrations::from_directory(&MIGRATIONS_DIR).expect("generating migrations should succeed")
});

fn apply_migrations(db: &mut Connection) -> Result<(), DbInitError> {
    MIGRATIONS.to_latest(db)?;
    db.pragma_update(None, "journal_mode", "wal")?;
    db.pragma_update(None, "synchronous", "normal")?;

    Ok(())
}

#[derive(Clone, Debug)]
pub struct State {
    db: Arc<Mutex<Connection>>,
}

impl State {
    fn new(db: Connection) -> Self {
        Self {
            db: Arc::new(Mutex::new(db)),
        }
    }

    pub fn load(state_dir: &Path) -> Result<Self, DbInitError> {
        let state_file = Self::prepare_state_file(state_dir)?;
        debug!(
            "try loading existing state file {}",
            state_file.to_string_lossy()
        );
        let mut db = Connection::open_with_flags(
            state_file,
            OpenFlags::SQLITE_OPEN_READ_WRITE
                | OpenFlags::SQLITE_OPEN_NO_MUTEX
                | OpenFlags::SQLITE_OPEN_URI,
        )?;

        apply_migrations(&mut db)?;

        Ok(Self::new(db))
    }

    pub fn init(state_dir: &Path, mailbox_metadata: &MailboxMetadata) -> Result<Self, DbInitError> {
        let state_file = Self::prepare_state_file(state_dir)?;
        debug!("creating new state file {}", state_file.to_string_lossy());
        let mut db = Connection::open(state_file)?;
        apply_migrations(&mut db)?;

        trace!(
            "setting cached uid_validity {}",
            mailbox_metadata.uid_validity()
        );
        db.execute(
            "insert or ignore into mailbox_metadata (highest_modseq, uid_validity) values (?1, ?2)",
            (
                i64::from(mailbox_metadata.highest_modseq()),
                u32::from(mailbox_metadata.uid_validity()),
            ),
        )?;

        Ok(Self::new(db))
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
            .query_one("select uid_validity from mailbox_metadata", (), |row| {
                let validity: u32 = row.get(0)?;
                let validity = validity.try_into().map_err(DbError::from);
                Ok(validity)
            })?
    }

    pub async fn update_highest_modseq(&self, value: ModSeq) -> Result<(), DbError> {
        trace!("check for updating highest_modseq with {value:?}");
        let mut db = self.db.lock().await;
        let transaction = db.transaction()?;
        let highest_modseq = get_highest_modseq(&transaction)?;
        if value > highest_modseq {
            set_highest_modseq(&transaction, value)?;
        }
        transaction.commit()?;

        Ok(())
    }

    pub async fn set_highest_modseq(&self, value: ModSeq) -> Result<(), DbError> {
        trace!("setting highest_modseq {value}");
        let db = self.db.lock().await;
        set_highest_modseq(&db, value).map_err(std::convert::Into::into)
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
            u32::from(data.uid().ok_or(DbError::Conversion)?),
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

    /// WARNING: Setup receiving in separate task or this may deadlock
    pub async fn get_all(
        &self,
        all_entries_tx: mpsc::Sender<LocalMailMetadata>,
    ) -> Result<ModSeq, DbError> {
        trace!("getting all stored mail metadata");
        let db = self.db.lock().await;
        let mut stmt = db.prepare_cached("select uid,flags,fileprefix from mail_metadata;")?;

        let current_highest_modseq = get_highest_modseq(&db)?;
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
    use std::{collections::HashSet, fs, os::unix::fs::PermissionsExt};

    use assertables::*;
    use rstest::*;
    use tempfile::{TempDir, tempdir};

    use crate::repository::MailboxMetadataBuilder;

    use super::*;

    struct TestState {
        // drop order is relevant to not delete db before optimize
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
    fn mailbox_metadata(uid_validity: UidValidity, highest_modseq: ModSeq) -> MailboxMetadata {
        let mut metadata = MailboxMetadataBuilder::default();
        metadata.uid_validity(uid_validity);
        metadata.highest_modseq(highest_modseq);

        assert_ok!(metadata.build())
    }

    #[fixture]
    fn state(temp_dir: TempDir, mailbox_metadata: MailboxMetadata) -> TestState {
        TestState {
            state: assert_ok!(State::init(temp_dir.path(), &mailbox_metadata)),
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
    async fn test_state_inits_correctly(
        state: TestState,
        highest_modseq: ModSeq,
        uid_validity: UidValidity,
    ) {
        let cached_modseq = assert_ok!(state.state.highest_modseq().await);
        assert_eq!(cached_modseq, highest_modseq);
        assert_eq!(assert_ok!(state.state.uid_validity().await), uid_validity);
    }

    #[rstest]
    fn test_state_init_fails_on_not_creatable_state_dir(
        temp_dir: TempDir,
        mailbox_metadata: MailboxMetadata,
    ) {
        let mut permissions = assert_ok!(temp_dir.path().metadata()).permissions();
        permissions.set_mode(0o000);
        assert_ok!(fs::set_permissions(temp_dir.path(), permissions));
        let state_dir = temp_dir.path().join("foo");

        let result = assert_err!(State::init(&state_dir, &mailbox_metadata));
        assert_matches!(result, DbInitError::Io(_));
    }

    #[rstest]
    fn test_state_init_fails_on_not_creatable_state_file(
        temp_dir: TempDir,
        mailbox_metadata: MailboxMetadata,
    ) {
        let mut permissions = assert_ok!(temp_dir.path().metadata()).permissions();
        permissions.set_mode(0o000);
        assert_ok!(fs::set_permissions(temp_dir.path(), permissions));

        let result = assert_err!(State::init(temp_dir.path(), &mailbox_metadata));
        assert_matches!(
            result,
            DbInitError::DbError(DbError::Db(rusqlite::Error::SqliteFailure(_, _)))
        );
    }

    #[rstest]
    #[tokio::test]
    async fn test_load_loads_correct(
        loadable_state_dir: TempDir,
        highest_modseq: ModSeq,
        uid_validity: UidValidity,
    ) {
        let result = assert_ok!(State::load(loadable_state_dir.path()));
        assert_eq!(assert_ok!(result.highest_modseq().await), highest_modseq);
        assert_eq!(assert_ok!(result.uid_validity().await), uid_validity);
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

    #[rstest]
    #[tokio::test]
    async fn test_update_highest_modseq_updates_highest_modseq_if_value_is_higher(
        state: TestState,
    ) {
        let initial_modseq = assert_ok!(state.state.highest_modseq().await);
        let new_modseq = assert_ok!(ModSeq::try_from(i64::MAX));
        assert_ne!(new_modseq, initial_modseq);
        assert_ok!(state.state.update_highest_modseq(new_modseq).await);

        assert_eq!(assert_ok!(state.state.highest_modseq().await), new_modseq);
    }

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

    #[rstest]
    #[tokio::test]
    async fn test_delete_by_uid_succeeds(state: TestState, metadata: LocalMailMetadata) {
        assert_ok!(state.state.store(&metadata).await);
        assert_some!(assert_ok!(
            state.state.get_by_id(assert_some!(metadata.uid())).await
        ));
        assert_ok!(state.state.delete_by_id(assert_some!(metadata.uid())).await);
        assert_none!(assert_ok!(
            state.state.get_by_id(assert_some!(metadata.uid())).await
        ));
    }

    #[rstest]
    #[tokio::test]
    async fn test_get_all_gets_all_stored_data(state: TestState, mut metadata: LocalMailMetadata) {
        assert_ok!(state.state.store(&metadata).await);
        let stored_first = assert_some!(assert_ok!(
            state.state.get_by_id(assert_some!(metadata.uid())).await
        ));
        metadata.set_uid(assert_ok!(Uid::try_from(9)));
        assert_ok!(state.state.store(&metadata).await);
        let stored_second = assert_some!(assert_ok!(
            state.state.get_by_id(assert_some!(metadata.uid())).await
        ));

        let (tx, mut rx) = mpsc::channel(32);
        assert_ok!(state.state.get_all(tx).await);
        let mut stored = HashSet::new();
        while let Some(data) = rx.recv().await {
            stored.insert(data);
        }
        assert_contains!(stored, &stored_first);
        assert_contains!(stored, &stored_second);
    }
}
