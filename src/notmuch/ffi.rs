//! Bindings of the parts of libnotmuch 5.4 (notmuch 0.32) and later, that are needed for syncing
//! tags. See `notmuch.h` for their documentation. The library gets loaded at runtime, so that
//! imapmaildir runs without it unless syncing tags.

use std::{
    ffi::{CStr, c_char, c_int, c_uint, c_ulong},
    sync::OnceLock,
};

pub type Status = c_uint;
pub type Bool = c_int;

pub const STATUS_SUCCESS: Status = 0;
pub const STATUS_XAPIAN_EXCEPTION: Status = 3;
pub const STATUS_FILE_ERROR: Status = 4;
pub const STATUS_FILE_NOT_EMAIL: Status = 5;
pub const STATUS_DUPLICATE_MESSAGE_ID: Status = 6;

pub type DatabaseMode = c_uint;
pub const DATABASE_MODE_READ_ONLY: DatabaseMode = 0;
pub const DATABASE_MODE_READ_WRITE: DatabaseMode = 1;

pub type ConfigKey = c_uint;
pub const CONFIG_MAIL_ROOT: ConfigKey = 1;
pub const CONFIG_NEW_TAGS: ConfigKey = 5;
pub const CONFIG_SYNC_MAILDIR_FLAGS: ConfigKey = 7;

pub type Exclude = c_uint;
pub const EXCLUDE_FALSE: Exclude = 2;

pub type Sort = c_uint;
pub const SORT_UNSORTED: Sort = 3;

macro_rules! opaque {
    ($($name:ident),*) => {
        $(
            #[repr(C)]
            pub struct $name {
                _private: [u8; 0],
            }
        )*
    };
}

opaque!(
    Database,
    Message,
    Messages,
    Query,
    Tags,
    Filenames,
    ConfigValues,
    IndexOpts
);

/// Soname of libnotmuch 5, whose ABI stays compatible within the major version
const LIBRARY: &CStr = c"libnotmuch.so.5";

macro_rules! library {
    ($($name:ident: fn($($arg:ident: $ty:ty),*) $(-> $ret:ty)?;)*) => {
        pub struct Library {
            $(pub $name: unsafe extern "C" fn($($ty),*) $(-> $ret)?,)*
        }

        impl Library {
            /// # Safety
            /// `handle` must be a handle of libnotmuch returned by `dlopen`
            unsafe fn load_from(handle: *mut libc::c_void) -> Result<Self, String> {
                Ok(Self {
                    $($name: {
                        let symbol = concat!(stringify!($name), "\0");
                        // SAFETY: the handle is valid and the symbol NUL-terminated
                        let address = unsafe { libc::dlsym(handle, symbol.as_ptr().cast()) };
                        if address.is_null() {
                            return Err(format!("libnotmuch lacks {}", stringify!($name)));
                        }
                        // SAFETY: the symbol has the declared signature in libnotmuch 5.4+
                        unsafe {
                            std::mem::transmute::<
                                *mut libc::c_void,
                                unsafe extern "C" fn($($ty),*) $(-> $ret)?,
                            >(address)
                        }
                    },)*
                })
            }
        }
    };
}

library! {
    notmuch_status_to_string: fn(status: Status) -> *const c_char;
    notmuch_database_open_with_config: fn(database_path: *const c_char, mode: DatabaseMode, config_path: *const c_char, profile: *const c_char, database: *mut *mut Database, error_message: *mut *mut c_char) -> Status;
    notmuch_database_create_with_config: fn(database_path: *const c_char, config_path: *const c_char, profile: *const c_char, database: *mut *mut Database, error_message: *mut *mut c_char) -> Status;
    notmuch_database_close: fn(database: *mut Database) -> Status;
    notmuch_database_destroy: fn(database: *mut Database) -> Status;
    notmuch_database_begin_atomic: fn(database: *mut Database) -> Status;
    notmuch_database_end_atomic: fn(database: *mut Database) -> Status;
    notmuch_database_get_path: fn(database: *mut Database) -> *const c_char;
    notmuch_database_get_revision: fn(database: *mut Database, uuid: *mut *const c_char) -> c_ulong;
    notmuch_database_index_file: fn(database: *mut Database, filename: *const c_char, indexopts: *mut IndexOpts, message: *mut *mut Message) -> Status;
    notmuch_database_remove_message: fn(database: *mut Database, filename: *const c_char) -> Status;
    notmuch_database_find_message_by_filename: fn(database: *mut Database, filename: *const c_char, message: *mut *mut Message) -> Status;
    notmuch_config_get: fn(database: *mut Database, key: ConfigKey) -> *const c_char;
    notmuch_config_get_values: fn(database: *mut Database, key: ConfigKey) -> *mut ConfigValues;
    notmuch_config_values_valid: fn(values: *mut ConfigValues) -> Bool;
    notmuch_config_values_get: fn(values: *mut ConfigValues) -> *const c_char;
    notmuch_config_values_move_to_next: fn(values: *mut ConfigValues);
    notmuch_config_values_destroy: fn(values: *mut ConfigValues);
    notmuch_config_get_bool: fn(database: *mut Database, key: ConfigKey, value: *mut Bool) -> Status;
    notmuch_query_create: fn(database: *mut Database, query: *const c_char) -> *mut Query;
    notmuch_query_set_omit_excluded: fn(query: *mut Query, omit_excluded: Exclude);
    notmuch_query_set_sort: fn(query: *mut Query, sort: Sort);
    notmuch_query_search_messages: fn(query: *mut Query, out: *mut *mut Messages) -> Status;
    notmuch_query_count_messages: fn(query: *mut Query, count: *mut c_uint) -> Status;
    notmuch_query_destroy: fn(query: *mut Query);
    notmuch_messages_valid: fn(messages: *mut Messages) -> Bool;
    notmuch_messages_get: fn(messages: *mut Messages) -> *mut Message;
    notmuch_messages_move_to_next: fn(messages: *mut Messages);
    notmuch_messages_destroy: fn(messages: *mut Messages);
    notmuch_message_get_filenames: fn(message: *mut Message) -> *mut Filenames;
    notmuch_message_get_tags: fn(message: *mut Message) -> *mut Tags;
    notmuch_message_add_tag: fn(message: *mut Message, tag: *const c_char) -> Status;
    notmuch_message_remove_tag: fn(message: *mut Message, tag: *const c_char) -> Status;
    notmuch_message_maildir_flags_to_tags: fn(message: *mut Message) -> Status;
    notmuch_message_freeze: fn(message: *mut Message) -> Status;
    notmuch_message_thaw: fn(message: *mut Message) -> Status;
    notmuch_message_destroy: fn(message: *mut Message);
    notmuch_tags_valid: fn(tags: *mut Tags) -> Bool;
    notmuch_tags_get: fn(tags: *mut Tags) -> *const c_char;
    notmuch_tags_move_to_next: fn(tags: *mut Tags);
    notmuch_tags_destroy: fn(tags: *mut Tags);
    notmuch_filenames_valid: fn(filenames: *mut Filenames) -> Bool;
    notmuch_filenames_get: fn(filenames: *mut Filenames) -> *const c_char;
    notmuch_filenames_move_to_next: fn(filenames: *mut Filenames);
    notmuch_filenames_destroy: fn(filenames: *mut Filenames);
}

/// Loads libnotmuch once
///
/// # Errors
/// If the library cannot be found or lacks required functions, e.g. as it is too old
pub fn library() -> Result<&'static Library, String> {
    static LOADED: OnceLock<Result<Library, String>> = OnceLock::new();
    LOADED
        .get_or_init(|| {
            // SAFETY: the name is NUL-terminated. The library stays loaded for the whole process
            let handle =
                unsafe { libc::dlopen(LIBRARY.as_ptr(), libc::RTLD_NOW | libc::RTLD_LOCAL) };
            if handle.is_null() {
                // SAFETY: dlerror returns NULL or a NUL-terminated message
                let error = unsafe { libc::dlerror() };
                let reason = if error.is_null() {
                    "unknown error".to_string()
                } else {
                    // SAFETY: checked for NULL
                    unsafe { CStr::from_ptr(error) }
                        .to_string_lossy()
                        .into_owned()
                };
                return Err(format!(
                    "cannot load {}: {reason}",
                    LIBRARY.to_string_lossy()
                ));
            }
            // SAFETY: the handle belongs to libnotmuch
            unsafe { Library::load_from(handle) }
        })
        .as_ref()
        .map_err(Clone::clone)
}
