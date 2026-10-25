use std::{
    fmt::Display,
    time::{Duration, Instant},
};

use crate::{
    imap::ClientError,
    sync::{Error, Shutdown},
};

const MIN_DELAY: Duration = Duration::from_secs(1);
const MAX_DELAY: Duration = Duration::from_mins(5);

/// Errors, that may vanish by reconnecting
pub trait Transient: Display {
    fn is_transient(&self) -> bool;
}

impl Transient for Error {
    fn is_transient(&self) -> bool {
        Error::is_transient(self)
    }
}

impl Transient for ClientError {
    fn is_transient(&self) -> bool {
        ClientError::is_transient(self)
    }
}

/// Runs `session` again after transient errors with exponential backoff, until it stops after
/// `shutdown` got triggered. Returns the first permanent error.
///
/// `session` should set up its own runtime, so that tasks of failed sessions do not linger.
///
/// # Errors
/// The first permanent error of `session`
pub fn reconnecting<E: Transient>(
    name: &str,
    shutdown: &Shutdown,
    mut session: impl FnMut() -> Result<(), E>,
) -> Result<(), E> {
    let mut delay = None;
    while !shutdown.is_triggered() {
        let started = Instant::now();
        let e = match session() {
            Ok(()) => break,
            Err(e) if !e.is_transient() => return Err(e),
            Err(e) => e,
        };
        if shutdown.is_triggered() {
            log::warn!("{name}: {e}. Not reconnecting while shutting down");
            break;
        }
        let next = next_delay(delay, started.elapsed());
        log::warn!("{name}: {e}. Reconnecting in {next:?}");
        if shutdown.sleep(next) {
            break;
        }
        delay = Some(next);
    }

    Ok(())
}

/// Doubles the previous delay up to [`MAX_DELAY`]. Sessions that lasted longer than
/// [`MAX_DELAY`] were healthy, so their failure starts over with [`MIN_DELAY`].
fn next_delay(previous: Option<Duration>, session_duration: Duration) -> Duration {
    match previous {
        Some(previous) if session_duration < MAX_DELAY => (previous * 2).min(MAX_DELAY),
        _ => MIN_DELAY,
    }
}

#[cfg(test)]
mod tests {
    use std::cell::Cell;

    use super::*;

    #[test]
    fn test_next_delay_starts_with_min_delay() {
        assert_eq!(next_delay(None, Duration::ZERO), MIN_DELAY);
    }

    #[test]
    fn test_next_delay_doubles_up_to_max_delay() {
        assert_eq!(
            next_delay(Some(Duration::from_secs(4)), Duration::ZERO),
            Duration::from_secs(8)
        );
        assert_eq!(
            next_delay(Some(Duration::from_mins(4)), Duration::ZERO),
            MAX_DELAY
        );
    }

    #[test]
    fn test_next_delay_resets_after_healthy_session() {
        assert_eq!(next_delay(Some(MAX_DELAY), MAX_DELAY), MIN_DELAY);
    }

    #[derive(Debug)]
    struct TestError(bool);

    impl Display for TestError {
        fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            write!(f, "transient: {}", self.0)
        }
    }

    impl Transient for TestError {
        fn is_transient(&self) -> bool {
            self.0
        }
    }

    #[test]
    fn test_reconnecting_returns_permanent_error() {
        let calls = Cell::new(0);

        let error = reconnecting("test", &Shutdown::new(), || {
            calls.set(calls.get() + 1);
            Err(TestError(false))
        });

        assert!(matches!(error, Err(TestError(false))));
        assert_eq!(calls.get(), 1);
    }

    #[test]
    fn test_reconnecting_retries_transient_error() {
        let calls = Cell::new(0);

        let error = reconnecting("test", &Shutdown::new(), || {
            calls.set(calls.get() + 1);
            Err(TestError(calls.get() < 2))
        });

        assert!(matches!(error, Err(TestError(false))));
        assert_eq!(calls.get(), 2);
    }

    #[test]
    fn test_reconnecting_stops_after_session_stopped() {
        let calls = Cell::new(0);

        let result = reconnecting("test", &Shutdown::new(), || {
            calls.set(calls.get() + 1);
            Ok::<_, TestError>(())
        });

        assert!(result.is_ok());
        assert_eq!(calls.get(), 1);
    }

    #[test]
    fn test_reconnecting_does_not_reconnect_after_shutdown() {
        let shutdown = Shutdown::new();
        let calls = Cell::new(0);

        let result = reconnecting("test", &shutdown, || {
            calls.set(calls.get() + 1);
            shutdown.trigger();
            Err(TestError(true))
        });

        assert!(result.is_ok());
        assert_eq!(calls.get(), 1);
    }
}
