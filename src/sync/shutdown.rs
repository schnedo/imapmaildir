use std::{
    sync::{Arc, Condvar, Mutex, MutexGuard, PoisonError},
    time::{Duration, Instant},
};

use futures::channel::oneshot;

#[derive(Default)]
struct State {
    /// When shutting down got triggered first
    triggered: Option<Instant>,
    failed: bool,
    waiters: Vec<oneshot::Sender<()>>,
}

/// Tells sync sessions on any thread to stop at the next safe point
#[derive(Clone, Default)]
pub struct Shutdown(Arc<(Mutex<State>, Condvar)>);

impl Shutdown {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    fn state(&self) -> MutexGuard<'_, State> {
        self.0.0.lock().unwrap_or_else(PoisonError::into_inner)
    }

    pub fn trigger(&self) {
        self.stop(false);
    }

    /// Shuts down due to an error, that other threads cannot report themselves
    pub fn fail(&self) {
        self.stop(true);
    }

    fn stop(&self, failed: bool) {
        let mut state = self.state();
        state.triggered.get_or_insert_with(Instant::now);
        state.failed |= failed;
        for waiter in state.waiters.drain(..) {
            // a dropped receiver does not wait anymore
            let _ = waiter.send(());
        }
        self.0.1.notify_all();
    }

    #[must_use]
    pub fn is_triggered(&self) -> bool {
        self.state().triggered.is_some()
    }

    /// Whether shutting down got triggered by [`Self::fail`]
    #[must_use]
    pub fn has_failed(&self) -> bool {
        self.state().failed
    }

    /// Completes once shut down got triggered
    pub async fn wait(&self) {
        let rx = {
            let mut state = self.state();
            if state.triggered.is_some() {
                return;
            }
            // waiters that stopped waiting must not pile up across sessions
            state.waiters.retain(|waiter| !waiter.is_canceled());
            let (tx, rx) = oneshot::channel();
            state.waiters.push(tx);
            rx
        };
        // the sender gets dropped only after sending
        let _ = rx.await;
    }

    /// Completes `timeout` after shutting down got triggered
    pub async fn timed_out(&self, timeout: Duration) {
        self.wait().await;
        let triggered = self.state().triggered;
        if let Some(triggered) = triggered {
            compio::time::sleep_until(triggered + timeout).await;
        }
    }

    /// Blocks the thread for `duration` or until shut down got triggered. Returns whether it got.
    #[must_use]
    pub fn sleep(&self, duration: Duration) -> bool {
        let state = self.state();
        let (state, _) = self
            .0
            .1
            .wait_timeout_while(state, duration, |state| state.triggered.is_none())
            .unwrap_or_else(PoisonError::into_inner);

        state.triggered.is_some()
    }
}

#[cfg(test)]
mod tests {
    use std::{thread, time::Instant};

    use super::*;

    #[compio::test]
    async fn test_wait_completes_after_trigger() {
        let shutdown = Shutdown::new();
        let triggering = shutdown.clone();
        let trigger = thread::spawn(move || {
            thread::sleep(Duration::from_millis(10));
            triggering.trigger();
        });

        shutdown.wait().await;

        assert!(shutdown.is_triggered());
        trigger.join().expect("trigger thread should not panic");
    }

    #[compio::test]
    async fn test_fail_shuts_down() {
        let shutdown = Shutdown::new();
        assert!(!shutdown.has_failed());

        shutdown.fail();
        shutdown.trigger();

        shutdown.wait().await;
        assert!(shutdown.has_failed());
    }

    #[compio::test]
    async fn test_wait_completes_if_already_triggered() {
        let shutdown = Shutdown::new();
        shutdown.trigger();

        shutdown.wait().await;
    }

    #[compio::test]
    async fn test_wait_forgets_waiters_that_stopped_waiting() {
        let shutdown = Shutdown::new();
        for _ in 0..10 {
            let wait = std::pin::pin!(shutdown.wait());
            assert!(futures::poll!(wait).is_pending());
        }

        assert_eq!(shutdown.state().waiters.len(), 1);
    }

    #[compio::test]
    async fn test_timed_out_counts_from_trigger() {
        let shutdown = Shutdown::new();
        shutdown.trigger();
        compio::time::sleep(Duration::from_millis(200)).await;
        let started = Instant::now();

        shutdown.timed_out(Duration::from_millis(200)).await;

        assert!(started.elapsed() < Duration::from_millis(200));
    }

    #[test]
    fn test_sleep_times_out_without_trigger() {
        assert!(!Shutdown::new().sleep(Duration::from_millis(1)));
    }

    #[test]
    fn test_sleep_ends_early_on_trigger() {
        let shutdown = Shutdown::new();
        let triggering = shutdown.clone();
        let trigger = thread::spawn(move || {
            thread::sleep(Duration::from_millis(10));
            triggering.trigger();
        });
        let started = Instant::now();

        assert!(shutdown.sleep(Duration::from_mins(1)));
        assert!(started.elapsed() < Duration::from_secs(30));
        trigger.join().expect("trigger thread should not panic");
    }
}
