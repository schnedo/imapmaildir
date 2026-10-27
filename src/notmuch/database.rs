use std::{
    ffi::{CStr, CString, OsStr, c_char},
    fmt::Display,
    marker::PhantomData,
    os::unix::ffi::OsStrExt as _,
    path::{Path, PathBuf},
    ptr,
};

use thiserror::Error;

use crate::notmuch::ffi;

/// Which notmuch configuration and database to open
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Location {
    /// Configuration file. Searched like notmuch does if unset, ignored if empty.
    pub config: Option<PathBuf>,
    /// Profile of the configuration, e.g. `work` for `~/.config/notmuch/work/config`
    pub profile: Option<String>,
    /// Database directory. Taken from the configuration if unset.
    pub database: Option<PathBuf>,
}

/// Revision of the database, which is only comparable to ones of the same `uuid`
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Revision {
    pub uuid: String,
    pub number: u64,
}

#[derive(Debug, Error)]
pub struct Error {
    status: ffi::Status,
    action: &'static str,
    message: Option<String>,
}

impl Error {
    fn new(status: ffi::Status, action: &'static str) -> Self {
        Self {
            status,
            action,
            message: None,
        }
    }

    /// Whether e.g. another process holds the write lock, so that retrying may succeed
    pub fn is_retryable(&self) -> bool {
        self.status == ffi::STATUS_XAPIAN_EXCEPTION
    }
}

impl Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let status = ffi::library().map_or_else(
            |_| format!("status {}", self.status),
            |lib| {
                // SAFETY: notmuch returns a static string for every status
                unsafe { CStr::from_ptr((lib.notmuch_status_to_string)(self.status)) }
                    .to_string_lossy()
                    .into_owned()
            },
        );
        write!(f, "cannot {} notmuch database: {status}", self.action)?;
        if let Some(message) = &self.message {
            write!(f, " ({})", message.trim())?;
        }

        Ok(())
    }
}

fn library() -> Result<&'static ffi::Library, Error> {
    ffi::library().map_err(|message| Error {
        status: ffi::STATUS_FILE_ERROR,
        action: "load library of",
        message: Some(message),
    })
}

fn check(status: ffi::Status, action: &'static str) -> Result<(), Error> {
    if status == ffi::STATUS_SUCCESS {
        Ok(())
    } else {
        Err(Error::new(status, action))
    }
}

fn c_path(path: &Path) -> Result<CString, Error> {
    CString::new(path.as_os_str().as_bytes())
        .map_err(|_| Error::new(ffi::STATUS_FILE_ERROR, "pass path containing NUL to"))
}

fn c_str(value: &str) -> Result<CString, Error> {
    CString::new(value).map_err(|_| Error::new(ffi::STATUS_FILE_ERROR, "pass NUL to"))
}

/// Copies a string owned by notmuch
///
/// # Safety
/// `value` must be NULL or point to a NUL-terminated string
unsafe fn to_string(value: *const c_char) -> Option<String> {
    // SAFETY: guaranteed by caller
    (!value.is_null()).then(|| {
        unsafe { CStr::from_ptr(value) }
            .to_string_lossy()
            .into_owned()
    })
}

fn to_path(value: &CStr) -> PathBuf {
    PathBuf::from(OsStr::from_bytes(value.to_bytes()))
}

/// Converts the strings of a notmuch iterator, e.g. of tags, with `convert`, skipping `None`,
/// and destroys it. `None` if the iterator is NULL, which notmuch returns on errors.
///
/// # Safety
/// `iterator` must be NULL or valid, the functions must belong to its type
unsafe fn collect<I, T>(
    iterator: *mut I,
    valid: unsafe extern "C" fn(*mut I) -> ffi::Bool,
    get: unsafe extern "C" fn(*mut I) -> *const c_char,
    move_to_next: unsafe extern "C" fn(*mut I),
    destroy: unsafe extern "C" fn(*mut I),
    convert: impl Fn(&CStr) -> Option<T>,
) -> Option<Vec<T>> {
    let mut values = Vec::new();
    if iterator.is_null() {
        return None;
    }
    // SAFETY: guaranteed by caller, the iterator gets destroyed after iterating
    unsafe {
        while valid(iterator) != 0 {
            let value = get(iterator);
            if !value.is_null()
                && let Some(value) = convert(CStr::from_ptr(value))
            {
                values.push(value);
            }
            move_to_next(iterator);
        }
        destroy(iterator);
    }

    Some(values)
}

/// Takes an error message allocated by notmuch with `malloc`
///
/// # Safety
/// `message` must be NULL or point to a NUL-terminated string allocated with `malloc`
unsafe fn take_message(message: *mut c_char) -> Option<String> {
    // SAFETY: guaranteed by caller
    let taken = unsafe { to_string(message) };
    if !message.is_null() {
        // SAFETY: notmuch allocates error messages with malloc
        unsafe { libc::free(message.cast()) };
    }

    taken
}

impl std::fmt::Debug for Database {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Database").finish_non_exhaustive()
    }
}

pub struct Database {
    lib: &'static ffi::Library,
    raw: *mut ffi::Database,
}

impl Database {
    /// # Errors
    /// If the configuration or database cannot be opened, e.g. as another process holds the
    /// write lock
    pub fn open(location: &Location, writable: bool) -> Result<Self, Error> {
        let lib = library()?;
        let database = location.database.as_deref().map(c_path).transpose()?;
        let config = location.config.as_deref().map(c_path).transpose()?;
        let profile = location.profile.as_deref().map(c_str).transpose()?;
        let mode = if writable {
            ffi::DATABASE_MODE_READ_WRITE
        } else {
            ffi::DATABASE_MODE_READ_ONLY
        };
        // SAFETY: all strings are NUL-terminated or NULL and outlive the call
        Self::open_with(lib, "open", |raw, message| unsafe {
            (lib.notmuch_database_open_with_config)(
                database.as_ref().map_or(ptr::null(), |path| path.as_ptr()),
                mode,
                config.as_ref().map_or(ptr::null(), |path| path.as_ptr()),
                profile
                    .as_ref()
                    .map_or(ptr::null(), |profile| profile.as_ptr()),
                raw,
                message,
            )
        })
    }

    /// Creates a new database in `path`, which is also its mail root, ignoring any configuration
    ///
    /// # Errors
    /// If the database cannot be created
    pub fn create(path: &Path) -> Result<Self, Error> {
        let lib = library()?;
        let path = c_path(path)?;
        let config = c_str("")?;
        // SAFETY: all strings are NUL-terminated and outlive the call
        Self::open_with(lib, "create", |raw, message| unsafe {
            (lib.notmuch_database_create_with_config)(
                path.as_ptr(),
                config.as_ptr(),
                ptr::null(),
                raw,
                message,
            )
        })
    }

    /// Takes the database `f` opens, which returns the status and sets the database and an
    /// error message allocated by notmuch
    fn open_with(
        lib: &'static ffi::Library,
        action: &'static str,
        f: impl FnOnce(*mut *mut ffi::Database, *mut *mut c_char) -> ffi::Status,
    ) -> Result<Self, Error> {
        let mut raw = ptr::null_mut();
        let mut message = ptr::null_mut();
        let status = f(&raw mut raw, &raw mut message);
        // SAFETY: notmuch sets the message to NULL or a malloc'ed string
        let message = unsafe { take_message(message) };
        if status == ffi::STATUS_SUCCESS && !raw.is_null() {
            Ok(Self { lib, raw })
        } else {
            Err(Error {
                status,
                action,
                message,
            })
        }
    }

    /// Commits changes and closes the database
    ///
    /// # Errors
    /// If committing fails
    pub fn close(self) -> Result<(), Error> {
        // SAFETY: the database is valid until destroyed on drop
        check(
            unsafe { (self.lib.notmuch_database_close)(self.raw) },
            "close",
        )
    }

    pub fn revision(&self) -> Revision {
        let mut uuid = ptr::null();
        // SAFETY: the database is valid, notmuch owns the returned uuid
        let number = unsafe { (self.lib.notmuch_database_get_revision)(self.raw, &raw mut uuid) };
        Revision {
            // SAFETY: notmuch returns a NUL-terminated uuid
            uuid: unsafe { to_string(uuid) }.unwrap_or_default(),
            number,
        }
    }

    /// Directory of the database
    pub fn path(&self) -> Option<PathBuf> {
        // SAFETY: the database is valid, notmuch owns the returned string
        let path = unsafe { (self.lib.notmuch_database_get_path)(self.raw) };
        // SAFETY: notmuch returns a NUL-terminated string
        (!path.is_null()).then(|| to_path(unsafe { CStr::from_ptr(path) }))
    }

    pub fn mail_root(&self) -> Option<PathBuf> {
        // SAFETY: the database is valid, notmuch owns the returned string
        let root = unsafe { (self.lib.notmuch_config_get)(self.raw, ffi::CONFIG_MAIL_ROOT) };
        // SAFETY: notmuch returns a NUL-terminated string
        (!root.is_null()).then(|| to_path(unsafe { CStr::from_ptr(root) }))
    }

    /// Tags of `new.tags`, that `notmuch new` adds to new messages
    pub fn new_tags(&self) -> Vec<String> {
        // SAFETY: the database is valid, the functions belong to config values
        unsafe {
            collect(
                (self.lib.notmuch_config_get_values)(self.raw, ffi::CONFIG_NEW_TAGS),
                self.lib.notmuch_config_values_valid,
                self.lib.notmuch_config_values_get,
                self.lib.notmuch_config_values_move_to_next,
                self.lib.notmuch_config_values_destroy,
                |tag| (!tag.is_empty()).then(|| tag.to_string_lossy().into_owned()),
            )
        }
        .unwrap_or_default()
    }

    /// Whether flags of the maildir file names are synced with tags (`maildir.synchronize_flags`)
    ///
    /// # Errors
    /// If the configuration cannot be read
    pub fn synchronizes_flags(&self) -> Result<bool, Error> {
        let mut value = 0;
        // SAFETY: the database is valid
        check(
            unsafe {
                (self.lib.notmuch_config_get_bool)(
                    self.raw,
                    ffi::CONFIG_SYNC_MAILDIR_FLAGS,
                    &raw mut value,
                )
            },
            "read configuration of",
        )?;

        Ok(value != 0)
    }

    /// Runs `f` as single atomic change. If `f` fails, the database must get dropped without
    /// closing it, which discards the change.
    ///
    /// # Errors
    /// If `f` fails or the change cannot be started or committed
    pub fn atomic<T>(&self, f: impl FnOnce(&Self) -> Result<T, Error>) -> Result<T, Error> {
        // SAFETY: the database is valid
        check(
            unsafe { (self.lib.notmuch_database_begin_atomic)(self.raw) },
            "begin change of",
        )?;
        // without ending the atomic section, closing the database discards the partial change
        let result = f(self)?;
        // SAFETY: the database is valid and in an atomic section
        check(
            unsafe { (self.lib.notmuch_database_end_atomic)(self.raw) },
            "end change of",
        )?;

        Ok(result)
    }

    /// Indexes the mail file at the absolute `path`. Returns its message and whether it is new,
    /// i.e. no other file of the message is indexed. `None` if the file is no mail or vanished.
    ///
    /// # Errors
    /// If indexing fails otherwise
    pub fn index_file(&self, path: &Path) -> Result<Option<(Message<'_>, bool)>, Error> {
        let filename = c_path(path)?;
        let mut message = ptr::null_mut();
        // SAFETY: the database is valid, the filename NUL-terminated
        let status = unsafe {
            (self.lib.notmuch_database_index_file)(
                self.raw,
                filename.as_ptr(),
                ptr::null_mut(),
                &raw mut message,
            )
        };
        match status {
            ffi::STATUS_SUCCESS | ffi::STATUS_DUPLICATE_MESSAGE_ID => Ok(Some((
                Message::new(self.lib, message),
                status == ffi::STATUS_SUCCESS,
            ))),
            ffi::STATUS_FILE_ERROR | ffi::STATUS_FILE_NOT_EMAIL => {
                log::warn!(
                    "notmuch cannot index {}: {}",
                    path.display(),
                    Error::new(status, "index file in")
                );
                Ok(None)
            }
            status => Err(Error::new(status, "index file in")),
        }
    }

    /// Finds the message of the absolute `path`
    ///
    /// # Errors
    /// If searching the message fails
    pub fn find_by_filename(&self, path: &Path) -> Result<Option<Message<'_>>, Error> {
        let filename = c_path(path)?;
        let mut message = ptr::null_mut();
        // SAFETY: the database is valid, the filename NUL-terminated
        check(
            unsafe {
                (self.lib.notmuch_database_find_message_by_filename)(
                    self.raw,
                    filename.as_ptr(),
                    &raw mut message,
                )
            },
            "find message in",
        )?;

        Ok((!message.is_null()).then(|| Message::new(self.lib, message)))
    }

    /// Removes the file at the absolute `path` from its message, which gets removed with its last
    /// file. Does nothing if the file is not indexed.
    ///
    /// # Errors
    /// If removing fails
    pub fn remove_filename(&self, path: &Path) -> Result<(), Error> {
        let filename = c_path(path)?;
        // SAFETY: the database is valid, the filename NUL-terminated
        match unsafe { (self.lib.notmuch_database_remove_message)(self.raw, filename.as_ptr()) } {
            ffi::STATUS_SUCCESS | ffi::STATUS_DUPLICATE_MESSAGE_ID => Ok(()),
            status => Err(Error::new(status, "remove file from")),
        }
    }

    /// Number of messages matching `query`, including excluded ones
    ///
    /// # Errors
    /// If the query is invalid or counting fails
    pub fn count(&self, query: &str) -> Result<u32, Error> {
        let query = c_str(query)?;
        let mut count = 0;
        // SAFETY: the database is valid, the query NUL-terminated and destroyed after counting
        let status = unsafe {
            let raw_query = (self.lib.notmuch_query_create)(self.raw, query.as_ptr());
            if raw_query.is_null() {
                return Err(Error::new(ffi::STATUS_XAPIAN_EXCEPTION, "query"));
            }
            (self.lib.notmuch_query_set_omit_excluded)(raw_query, ffi::EXCLUDE_FALSE);
            let status = (self.lib.notmuch_query_count_messages)(raw_query, &raw mut count);
            (self.lib.notmuch_query_destroy)(raw_query);
            status
        };
        check(status, "count messages in")?;

        Ok(count)
    }

    /// Calls `f` with every message matching `query`, including excluded ones
    ///
    /// # Errors
    /// If the query is invalid, searching fails or `f` fails
    pub fn search(
        &self,
        query: &str,
        mut f: impl FnMut(&Message<'_>) -> Result<(), Error>,
    ) -> Result<(), Error> {
        let query = c_str(query)?;
        // SAFETY: the database is valid, the query NUL-terminated. The query owns the messages
        // and gets destroyed after iterating them
        unsafe {
            let raw_query = (self.lib.notmuch_query_create)(self.raw, query.as_ptr());
            if raw_query.is_null() {
                return Err(Error::new(ffi::STATUS_XAPIAN_EXCEPTION, "query"));
            }
            (self.lib.notmuch_query_set_omit_excluded)(raw_query, ffi::EXCLUDE_FALSE);
            (self.lib.notmuch_query_set_sort)(raw_query, ffi::SORT_UNSORTED);
            let mut messages = ptr::null_mut();
            let status = (self.lib.notmuch_query_search_messages)(raw_query, &raw mut messages);
            if status != ffi::STATUS_SUCCESS {
                (self.lib.notmuch_query_destroy)(raw_query);
                return Err(Error::new(status, "query"));
            }
            let mut result = Ok(());
            while result.is_ok() && (self.lib.notmuch_messages_valid)(messages) != 0 {
                // NULL on errors, e.g. as another process changed the database meanwhile.
                // Skipping the message would lose its changes.
                let raw = (self.lib.notmuch_messages_get)(messages);
                result = if raw.is_null() {
                    Err(Error::new(ffi::STATUS_XAPIAN_EXCEPTION, "read message of"))
                } else {
                    f(&Message::new(self.lib, raw))
                };
                (self.lib.notmuch_messages_move_to_next)(messages);
            }
            (self.lib.notmuch_messages_destroy)(messages);
            (self.lib.notmuch_query_destroy)(raw_query);
            result
        }
    }
}

impl Drop for Database {
    fn drop(&mut self) {
        // SAFETY: the database is valid and not used afterwards
        let status = unsafe { (self.lib.notmuch_database_destroy)(self.raw) };
        if let Err(e) = check(status, "close") {
            log::warn!("{e}");
        }
    }
}

impl std::fmt::Debug for Message<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Message").finish_non_exhaustive()
    }
}

pub struct Message<'db> {
    lib: &'static ffi::Library,
    raw: *mut ffi::Message,
    database: PhantomData<&'db Database>,
}

impl Message<'_> {
    fn new(lib: &'static ffi::Library, raw: *mut ffi::Message) -> Self {
        Self {
            lib,
            raw,
            database: PhantomData,
        }
    }

    /// # Errors
    /// If notmuch fails to read them, e.g. as another process changed the database meanwhile
    pub fn tags(&self) -> Result<Vec<String>, Error> {
        // SAFETY: the message is valid, the functions belong to tags
        unsafe {
            collect(
                (self.lib.notmuch_message_get_tags)(self.raw),
                self.lib.notmuch_tags_valid,
                self.lib.notmuch_tags_get,
                self.lib.notmuch_tags_move_to_next,
                self.lib.notmuch_tags_destroy,
                |tag| Some(tag.to_string_lossy().into_owned()),
            )
        }
        .ok_or_else(|| Error::new(ffi::STATUS_XAPIAN_EXCEPTION, "read tags of"))
    }

    /// # Errors
    /// If notmuch fails to read them, e.g. as another process changed the database meanwhile
    pub fn filenames(&self) -> Result<Vec<PathBuf>, Error> {
        // SAFETY: the message is valid, the functions belong to filenames
        unsafe {
            collect(
                (self.lib.notmuch_message_get_filenames)(self.raw),
                self.lib.notmuch_filenames_valid,
                self.lib.notmuch_filenames_get,
                self.lib.notmuch_filenames_move_to_next,
                self.lib.notmuch_filenames_destroy,
                |filename| Some(to_path(filename)),
            )
        }
        .ok_or_else(|| Error::new(ffi::STATUS_XAPIAN_EXCEPTION, "read file names of"))
    }

    /// # Errors
    /// If the tag is invalid, e.g. too long, or cannot be stored
    pub fn add_tag(&self, tag: &str) -> Result<(), Error> {
        let tag = c_str(tag)?;
        // SAFETY: the message is valid, the tag NUL-terminated
        check(
            unsafe { (self.lib.notmuch_message_add_tag)(self.raw, tag.as_ptr()) },
            "add tag in",
        )
    }

    /// # Errors
    /// If the tag is invalid, e.g. too long, or cannot be stored
    pub fn remove_tag(&self, tag: &str) -> Result<(), Error> {
        let tag = c_str(tag)?;
        // SAFETY: the message is valid, the tag NUL-terminated
        check(
            unsafe { (self.lib.notmuch_message_remove_tag)(self.raw, tag.as_ptr()) },
            "remove tag in",
        )
    }

    /// Updates tags from the flags of the maildir file names, like `notmuch new` does
    ///
    /// # Errors
    /// If the tags cannot be stored
    pub fn maildir_flags_to_tags(&self) -> Result<(), Error> {
        // SAFETY: the message is valid
        check(
            unsafe { (self.lib.notmuch_message_maildir_flags_to_tags)(self.raw) },
            "sync maildir flags in",
        )
    }

    /// Applies the tag changes of `f` at once
    ///
    /// # Errors
    /// If `f` fails or the message cannot be frozen or thawed
    pub fn frozen(&self, f: impl FnOnce(&Self) -> Result<(), Error>) -> Result<(), Error> {
        // SAFETY: the message is valid
        check(
            unsafe { (self.lib.notmuch_message_freeze)(self.raw) },
            "freeze message in",
        )?;
        let result = f(self);
        // SAFETY: the message is valid and frozen
        let thawed = check(
            unsafe { (self.lib.notmuch_message_thaw)(self.raw) },
            "thaw message in",
        );

        result.and(thawed)
    }
}

impl Drop for Message<'_> {
    fn drop(&mut self) {
        // SAFETY: the message is valid and not used afterwards
        unsafe { (self.lib.notmuch_message_destroy)(self.raw) };
    }
}
