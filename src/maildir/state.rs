use std::{
    convert::Into,
    fs::{self, create_dir_all},
    io,
    path::{Path, PathBuf},
    sync::LazyLock,
};

use enumflags2::BitFlag;
use include_dir::{Dir, include_dir};
use log::{debug, trace, warn};
use rusqlite::{Connection, OpenFlags, OptionalExtension, Result, Row};
use rusqlite_migration::Migrations;
use thiserror::Error;

use crate::{
    maildir::{LocalMailMetadata, MaildirFile as _},
    repository::{Flag, MailboxMetadata, ModSeq, Uid, UidValidity},
};

const STATE_FILE_NAME: &str = "imapmaildir.db";
const MIGRATIONS_DIR: Dir = include_dir!("$CARGO_MANIFEST_DIR/migrations");
static MIGRATIONS: LazyLock<Migrations<'static>> = LazyLock::new(|| {
    Migrations::from_directory(&MIGRATIONS_DIR).expect("generating migrations should succeed")
});

fn apply_migrations(db: &mut Connection) -> Result<(), InitError> {
    MIGRATIONS.to_latest(db)?;
    db.pragma_update(None, "journal_mode", "wal")?;
    db.pragma_update(None, "synchronous", "normal")?;

    Ok(())
}

#[derive(Debug)]
pub struct State {
    db: Connection,
}

impl State {
    fn new(db: Connection) -> Self {
        Self { db }
    }

    pub fn load(state_dir: &Path) -> Result<Self, InitError> {
        let state_file = Self::prepare_state_file(state_dir)?;
        debug!(
            "try loading existing state file {}",
            state_file.to_string_lossy()
        );
        if state_file.try_exists()? {
            let res = Connection::open_with_flags(
                state_file,
                OpenFlags::SQLITE_OPEN_READ_WRITE
                    | OpenFlags::SQLITE_OPEN_NO_MUTEX
                    | OpenFlags::SQLITE_OPEN_URI,
            );

            let mut db = res?;
            apply_migrations(&mut db)?;

            Ok(Self::new(db))
        } else {
            Err(InitError::Missing(state_file))
        }
    }

    pub fn init(state_dir: &Path, mailbox_metadata: &MailboxMetadata) -> Result<Self, InitError> {
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

    pub fn remove_from(state_dir: &Path) -> io::Result<()> {
        if state_dir.try_exists()? {
            let state_file = state_dir.join(STATE_FILE_NAME);
            if state_file.try_exists()? {
                fs::remove_file(&state_file)?;
            }
            if let Err(e) = fs::remove_dir(state_dir)
                && e.kind() != io::ErrorKind::DirectoryNotEmpty
            {
                warn!(
                    "Could not remove empty state directory {}: {e}",
                    state_dir.display()
                );
            }
        }

        Ok(())
    }

    fn prepare_state_file(state_dir: &Path) -> io::Result<PathBuf> {
        create_dir_all(state_dir)?;

        Ok(state_dir.join(STATE_FILE_NAME))
    }

    pub fn uid_validity(&self) -> Result<UidValidity, Error> {
        trace!("getting cached uid_validity");
        self.db
            .query_one("select uid_validity from mailbox_metadata", (), |row| {
                let validity: u32 = row.get(0)?;
                let validity = validity.try_into().map_err(Error::from);
                Ok(validity)
            })?
    }

    pub fn update_highest_modseq(&mut self, value: ModSeq) -> Result<(), Error> {
        trace!("check for updating highest_modseq with {value:?}");
        let transaction = self.db.transaction()?;
        let highest_modseq = get_highest_modseq(&transaction)?;
        if value > highest_modseq {
            set_highest_modseq(&transaction, value)?;
        }
        transaction.commit()?;

        Ok(())
    }

    pub fn set_highest_modseq(&self, value: ModSeq) -> Result<(), Error> {
        trace!("setting highest_modseq {value}");
        set_highest_modseq(&self.db, value).map_err(std::convert::Into::into)
    }

    pub fn highest_modseq(&self) -> Result<ModSeq, Error> {
        trace!("getting cached highest_modseq");
        get_highest_modseq(&self.db)
    }

    pub fn update(&self, data: &LocalMailMetadata) -> Result<(), Error> {
        trace!("updating mail cache {data:?}");
        let mut stmt = self
            .db
            .prepare_cached("update mail_metadata set flags=?1 where uid=?2")?;
        stmt.execute((data.flags().bits(), u32::from(data.uid())))?;

        Ok(())
    }

    pub fn store(&self, data: &LocalMailMetadata) -> Result<(), Error> {
        trace!("storing mail cache {data:?}");
        let uid = data.uid();
        let mut stmt = self
            .db
            .prepare_cached("insert into mail_metadata (uid,flags,fileprefix) values (?1,?2,?3)")?;
        stmt.execute((u32::from(uid), data.flags().bits(), &data.fileprefix()))?;

        Ok(())
    }

    pub fn get_by_id(&self, uid: Uid) -> Result<Option<LocalMailMetadata>, Error> {
        trace!("get existing metadata with {uid:?}");
        let mut stmt = self
            .db
            .prepare_cached("select * from mail_metadata where uid = ?1")?;

        stmt.query_one([u32::from(uid)], |row| row.try_into())
            .optional()
            .map_err(std::convert::Into::into)
    }

    pub fn delete_by_id(&self, uid: Uid) -> Result<(), Error> {
        trace!("deleting {uid:?}");
        let mut stmt = self
            .db
            .prepare_cached("delete from mail_metadata where uid = ?1")?;
        stmt.execute([u32::from(uid)])?;

        Ok(())
    }

    pub fn fore_each(&mut self, mut cb: impl FnMut(LocalMailMetadata)) -> Result<ModSeq, Error> {
        trace!("getting all stored mail metadata");
        let db = self.db.transaction()?;
        let mut stmt = db.prepare_cached("select uid,flags,fileprefix from mail_metadata;")?;
        for row in stmt.query_map([], |row| LocalMailMetadata::try_from(row))? {
            cb(row?);
        }
        let current_highest_modseq = get_highest_modseq(&db)?;

        Ok(current_highest_modseq)
    }
}

impl Drop for State {
    fn drop(&mut self) {
        self.db
            .execute("pragma optimize;", [])
            .expect("sqlite should be optimizable");
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
    let result = db.query_one("select highest_modseq from mailbox_metadata", [], |row| {
        let modseq: i64 = row.get(0)?;
        let modseq: Result<ModSeq, Error> = modseq.try_into().map_err(Error::from);
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

#[cfg(test)]
mod tests {
    use std::{collections::HashSet, fs, os::unix::fs::PermissionsExt};

    use assertables::*;
    use rstest::*;
    use tempfile::{TempDir, tempdir};

    use crate::repository::MailboxMetadataBuilder;

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
            assert_ok!(Uid::try_from(3)),
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
    fn test_state_inits_correctly(
        state: TestState,
        highest_modseq: ModSeq,
        uid_validity: UidValidity,
    ) {
        let cached_modseq = assert_ok!(state.state.highest_modseq());
        assert_eq!(cached_modseq, highest_modseq);
        assert_eq!(assert_ok!(state.state.uid_validity()), uid_validity);
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
        assert_matches!(result, InitError::Io(_));
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
            InitError::DbError(Error::Db(rusqlite::Error::SqliteFailure(_, _)))
        );
    }

    #[rstest]
    fn test_load_loads_correct(
        loadable_state_dir: TempDir,
        highest_modseq: ModSeq,
        uid_validity: UidValidity,
    ) {
        let result = assert_ok!(State::load(loadable_state_dir.path()));
        assert_eq!(assert_ok!(result.highest_modseq()), highest_modseq);
        assert_eq!(assert_ok!(result.uid_validity()), uid_validity);
    }

    #[rstest]
    fn test_load_errors_on_unreadable_state_file(loadable_state_dir: TempDir) {
        let mut permissions = assert_ok!(fs::metadata(loadable_state_dir.path())).permissions();
        permissions.set_mode(0o000);
        assert_ok!(fs::set_permissions(loadable_state_dir.path(), permissions));
        let result = assert_err!(State::load(loadable_state_dir.path()));
        assert_matches!(result, InitError::Io(_));
    }

    #[rstest]
    fn test_load_errors_on_missing_state_file(temp_dir: TempDir) {
        let result = assert_err!(State::load(temp_dir.path()));
        assert_matches!(result, InitError::Missing(_));
    }

    #[rstest]
    fn test_update_highest_modseq_updates_highest_modseq_if_value_is_higher(mut state: TestState) {
        let initial_modseq = assert_ok!(state.state.highest_modseq());
        let new_modseq = assert_ok!(ModSeq::try_from(i64::MAX));
        assert_gt!(new_modseq, initial_modseq);
        assert_ok!(state.state.update_highest_modseq(new_modseq));

        assert_eq!(assert_ok!(state.state.highest_modseq()), new_modseq);
    }

    #[rstest]
    fn test_update_highest_modseq_does_not_update_if_value_is_lower(mut state: TestState) {
        let initial_modseq = assert_ok!(state.state.highest_modseq());
        let new_modseq = assert_ok!(ModSeq::try_from(1));
        assert_le!(new_modseq, initial_modseq);
        assert_ok!(state.state.update_highest_modseq(new_modseq));

        assert_eq!(assert_ok!(state.state.highest_modseq()), initial_modseq);
    }

    #[rstest]
    fn test_set_highest_modseq_always_updates(state: TestState) {
        let initial_modseq = assert_ok!(state.state.highest_modseq());
        let new_modseq = assert_ok!(ModSeq::try_from(1));
        assert_lt!(new_modseq, initial_modseq);
        assert_ok!(state.state.set_highest_modseq(new_modseq));

        assert_eq!(assert_ok!(state.state.highest_modseq()), new_modseq);
    }

    #[rstest]
    fn test_storing_metadata_succeeds(state: TestState, metadata: LocalMailMetadata) {
        assert_none!(assert_ok!(state.state.get_by_id(metadata.uid())));
        assert_ok!(state.state.store(&metadata));
        let stored = assert_some!(assert_ok!(state.state.get_by_id(metadata.uid())));
        assert_eq!(metadata, stored);
    }

    #[rstest]
    fn test_updating_metadata_succeeds(state: TestState, mut metadata: LocalMailMetadata) {
        assert_ok!(state.state.store(&metadata));
        let flags = Flag::Seen | Flag::Deleted;
        assert_ne!(flags, metadata.flags());
        metadata.set_flags(flags);
        assert_ok!(state.state.update(&metadata));
        let stored = assert_some!(assert_ok!(state.state.get_by_id(metadata.uid())));
        assert_eq!(metadata, stored);
    }

    #[rstest]
    fn test_delete_by_uid_succeeds(state: TestState, metadata: LocalMailMetadata) {
        assert_ok!(state.state.store(&metadata));
        assert_some!(assert_ok!(state.state.get_by_id(metadata.uid())));
        assert_ok!(state.state.delete_by_id(metadata.uid()));
        assert_none!(assert_ok!(state.state.get_by_id(metadata.uid())));
    }

    #[rstest]
    fn test_foreach_fails_with_invalid_uid_in_state(mut state: TestState) {
        assert_ok!(state.state.db.execute_batch(
            "insert into mail_metadata (uid,flags,fileprefix) values (0,0,\"foo\")"
        ));
        assert_err!(state.state.fore_each(|_| {}));
    }

    #[rstest]
    fn test_foreach_gets_all_stored_data(mut state: TestState, metadata: LocalMailMetadata) {
        assert_ok!(state.state.store(&metadata));
        let stored_first = assert_some!(assert_ok!(state.state.get_by_id(metadata.uid())));
        let metadata = metadata.set_uid(assert_ok!(Uid::try_from(9)));
        assert_ok!(state.state.store(&metadata));
        let stored_second = assert_some!(assert_ok!(state.state.get_by_id(metadata.uid())));

        let mut stored = HashSet::new();
        assert_ok!(state.state.fore_each(|data| {
            stored.insert(data);
        }));
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
    fn test_delete_from_deletes_state_file(loadable_state_dir: TempDir) {
        assert_ok!(State::remove_from(loadable_state_dir.path()));
        let state_file = loadable_state_dir.path().join(STATE_FILE_NAME);
        assert!(!assert_ok!(state_file.try_exists()));
    }

    #[rstest]
    fn test_delete_from_deletes_state_dir_if_empty(loadable_state_dir: TempDir) {
        assert_ok!(State::remove_from(loadable_state_dir.path()));
        assert!(!assert_ok!(loadable_state_dir.path().try_exists()));
    }

    #[rstest]
    fn test_delete_from_keeps_state_dir_if_not_empty(loadable_state_dir: TempDir) {
        assert_ok!(fs::write(loadable_state_dir.path().join("afasdfas"), ""));
        assert_ok!(State::remove_from(loadable_state_dir.path()));
        assert!(assert_ok!(loadable_state_dir.path().try_exists()));
    }

    #[rstest]
    fn test_delete_from_errors_if_state_file_cannot_be_accessed(loadable_state_dir: TempDir) {
        let mut permissions = assert_ok!(loadable_state_dir.path().metadata()).permissions();
        permissions.set_mode(0o000);
        assert_ok!(fs::set_permissions(&loadable_state_dir, permissions));

        let result = assert_err!(State::remove_from(loadable_state_dir.path()));

        assert_matches!(result, io::Error { .. });
    }

    #[rstest]
    fn test_delete_from_errors_if_state_dir_cannot_be_accessed(temp_dir: TempDir) {
        let state_dir = temp_dir.path().join("kljlkfsajd");
        assert_ok!(fs::create_dir(&state_dir));
        let mut permissions = assert_ok!(temp_dir.path().metadata()).permissions();
        permissions.set_mode(0o000);
        assert_ok!(fs::set_permissions(&temp_dir, permissions));

        let result = assert_err!(State::remove_from(&state_dir));

        assert_matches!(result, io::Error { .. });
    }

    #[rstest]
    fn test_drop_does_not_panic_if_database_file_already_deleted(state: TestState) {
        let TestState { state, dir } = state;
        drop(dir);
        drop(state);
    }
}
