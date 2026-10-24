use std::{cell::Cell, time::Duration};

const MIN_TIMEOUT: Duration = Duration::from_mins(1);

/// Timeout for reissuing IDLE, which shrinks when the server ends IDLE earlier, as servers do
/// not announce their limit. Never grows back, as reissuing IDLE more often is cheap, while
/// being logged out requires reconnecting and resyncing.
#[derive(Debug)]
pub struct IdleTimeout {
    current: Cell<Duration>,
}

impl IdleTimeout {
    #[must_use]
    pub fn new(configured: Duration) -> Self {
        Self {
            current: Cell::new(configured),
        }
    }

    #[must_use]
    pub fn get(&self) -> Duration {
        self.current.get()
    }

    /// Shrinks the timeout to well below `idled`, after which the server ended IDLE
    pub fn on_terminated(&self, idled: Duration) {
        let shrunk = (idled * 4 / 5).max(MIN_TIMEOUT);
        if shrunk < self.current.get() {
            log::warn!(
                "server ended IDLE after {} seconds. Reissuing IDLE every {} seconds from now on. \
                 Consider setting idle_timeout in the account config",
                idled.as_secs(),
                shrunk.as_secs()
            );
            self.current.set(shrunk);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_shrinks_below_terminated_idle() {
        let timeout = IdleTimeout::new(Duration::from_mins(29));

        timeout.on_terminated(Duration::from_mins(10));

        assert_eq!(timeout.get(), Duration::from_mins(8));
    }

    #[test]
    fn test_does_not_shrink_below_minimum() {
        let timeout = IdleTimeout::new(Duration::from_mins(29));

        timeout.on_terminated(Duration::from_secs(5));

        assert_eq!(timeout.get(), MIN_TIMEOUT);
    }

    #[test]
    fn test_never_grows() {
        let timeout = IdleTimeout::new(Duration::from_mins(5));

        timeout.on_terminated(Duration::from_mins(29));

        assert_eq!(timeout.get(), Duration::from_mins(5));
    }
}
