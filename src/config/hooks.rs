use derive_getters::Getters;

pub use crate::sync::{Batching, HookFailure};

/// Commands informed about changes of the remote state, that got applied locally
#[derive(Getters, Debug, Clone)]
pub struct Hooks {
    /// Called once per batch of changes of a mailbox
    on_local_change: Vec<String>,
    #[getter(copy)]
    on_local_change_failure: HookFailure,
    #[getter(copy)]
    batching: Batching,
}

impl Hooks {
    #[must_use]
    pub fn new(
        on_local_change: Vec<String>,
        on_local_change_failure: HookFailure,
        batching: Batching,
    ) -> Self {
        Self {
            on_local_change,
            on_local_change_failure,
            batching,
        }
    }
}

impl Default for Hooks {
    fn default() -> Self {
        Self::new(Vec::new(), HookFailure::default(), Batching::default())
    }
}
