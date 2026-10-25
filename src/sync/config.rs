use std::time::Duration;

use derive_getters::Getters;

/// Configuration of the sync process
#[derive(Getters, Debug, Clone, Copy)]
pub struct Config {
    /// Maximum number of mails written to disk concurrently
    #[getter(copy)]
    store_batch_size: usize,
    /// Batches stop collecting further mails, once their accumulated size reaches this
    #[getter(copy)]
    store_batch_max_bytes: usize,
    /// Time after shutting down got triggered, after which unfinished server communication gets
    /// abandoned
    #[getter(copy)]
    shutdown_timeout: Duration,
}

impl Config {
    #[must_use]
    pub fn new(
        store_batch_size: usize,
        store_batch_max_bytes: usize,
        shutdown_timeout: Duration,
    ) -> Self {
        Self {
            store_batch_size,
            store_batch_max_bytes,
            shutdown_timeout,
        }
    }
}

impl Default for Config {
    fn default() -> Self {
        Self {
            // concurrent fsyncs get merged by the filesystem. Gains flatten above 64 on btrfs
            store_batch_size: 64,
            store_batch_max_bytes: 32 * 1024 * 1024,
            shutdown_timeout: Duration::from_secs(5),
        }
    }
}
