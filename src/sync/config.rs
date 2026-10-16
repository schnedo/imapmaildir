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
}

impl Config {
    #[must_use]
    pub fn new(store_batch_size: usize, store_batch_max_bytes: usize) -> Self {
        Self {
            store_batch_size,
            store_batch_max_bytes,
        }
    }
}

impl Default for Config {
    fn default() -> Self {
        Self {
            // concurrent fsyncs get merged by the filesystem. Gains flatten above 64 on btrfs
            store_batch_size: 64,
            store_batch_max_bytes: 32 * 1024 * 1024,
        }
    }
}
