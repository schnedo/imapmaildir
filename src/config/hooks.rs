use std::time::Duration;

use derive_getters::Getters;

pub use crate::sync::{DEFAULT_MAX_PENDING, HookFailure};

/// Commands informed about changes of the remote state, that got applied locally
#[derive(Getters, Debug, Clone)]
pub struct Hooks {
    /// Called once per batch of changes of a mailbox
    on_local_change: Vec<String>,
    #[getter(copy)]
    on_local_change_failure: HookFailure,
    /// Time after which a running hook gets killed and fails. Unlimited if unset.
    #[getter(copy)]
    on_local_change_timeout: Option<Duration>,
    /// Changes a mailbox collects, before it queues them for the hook
    #[getter(copy)]
    max_pending: usize,
}

impl Hooks {
    /// Creates hook settings
    #[must_use]
    pub fn new(
        on_local_change: Vec<String>,
        on_local_change_failure: HookFailure,
        on_local_change_timeout: Option<Duration>,
        max_pending: usize,
    ) -> Self {
        Self {
            on_local_change,
            on_local_change_failure,
            on_local_change_timeout,
            max_pending,
        }
    }
}

impl Default for Hooks {
    fn default() -> Self {
        Self::new(
            Vec::new(),
            HookFailure::default(),
            None,
            DEFAULT_MAX_PENDING,
        )
    }
}
