use std::{
    cell::{Cell, RefCell},
    future::poll_fn,
    rc::Rc,
    task::{Poll, Waker},
};

/// Limits the bytes of downloaded mails waiting for being stored, as storing may be slower than
/// downloading, e.g. on slow disks. Bounding the task queue by count alone would let large mails
/// pile up.
///
/// Not thread-safe, as downloading and storing run on the same single-threaded runtime of a
/// mailbox.
#[derive(Debug, Clone)]
pub struct ByteBudget {
    inner: Rc<Inner>,
}

#[derive(Debug)]
struct Inner {
    capacity: usize,
    available: Cell<usize>,
    closed: Cell<bool>,
    /// Only a single task waits, which queues the downloaded mails
    waker: RefCell<Option<Waker>>,
}

#[derive(Debug, PartialEq, Eq)]
pub struct Closed;

impl ByteBudget {
    pub fn new(capacity: usize) -> Self {
        Self {
            inner: Rc::new(Inner {
                capacity,
                available: Cell::new(capacity),
                closed: Cell::new(false),
                waker: RefCell::new(None),
            }),
        }
    }

    /// Budget taken by a mail of `len` bytes. Mails larger than the capacity take all of it, so
    /// that they wait for all others to be stored instead of forever.
    pub fn cost(&self, len: usize) -> usize {
        len.min(self.inner.capacity)
    }

    /// Waits until the budget of a mail of `len` bytes is available and takes it. Must only be
    /// awaited by one task at a time.
    ///
    /// # Errors
    /// If the budget got closed, as nobody would release it anymore
    pub async fn acquire(&self, len: usize) -> Result<(), Closed> {
        let cost = self.cost(len);
        poll_fn(|cx| {
            let available = self.inner.available.get();
            if let Some(remaining) = available.checked_sub(cost) {
                self.inner.available.set(remaining);
                Poll::Ready(Ok(()))
            } else if self.inner.closed.get() {
                Poll::Ready(Err(Closed))
            } else {
                *self.inner.waker.borrow_mut() = Some(cx.waker().clone());
                Poll::Pending
            }
        })
        .await
    }

    /// Returns `cost` previously taken via [`Self::acquire`]
    pub fn release(&self, cost: usize) {
        self.inner.available.set(self.inner.available.get() + cost);
        self.wake();
    }

    /// Makes waiting and following acquisitions fail, e.g. when storing stopped
    pub fn close(&self) {
        self.inner.closed.set(true);
        self.wake();
    }

    fn wake(&self) {
        // takes the waker first, so that waking does not hold the borrow
        let waker = self.inner.waker.borrow_mut().take();
        if let Some(waker) = waker {
            waker.wake();
        }
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use assertables::*;
    use futures::FutureExt as _;

    use super::*;

    #[compio::test]
    async fn test_acquire_takes_available_budget() {
        let budget = ByteBudget::new(10);

        assert_ok!(budget.acquire(4).await);
        assert_ok!(budget.acquire(6).await);

        assert_none!(budget.acquire(1).now_or_never());
    }

    #[compio::test]
    async fn test_release_wakes_waiting_acquire() {
        let budget = ByteBudget::new(10);
        assert_ok!(budget.acquire(10).await);
        let releaser = budget.clone();
        compio::runtime::spawn(async move {
            compio::time::sleep(Duration::from_millis(10)).await;
            releaser.release(releaser.cost(10));
        })
        .detach();

        assert_ok!(budget.acquire(3).await);
    }

    #[compio::test]
    async fn test_oversized_mail_takes_whole_budget() {
        let budget = ByteBudget::new(10);
        assert_ok!(budget.acquire(1).await);

        assert_none!(budget.acquire(100).now_or_never());
        budget.release(1);
        assert_ok!(budget.acquire(100).await);
        assert_eq!(budget.cost(100), 10);
    }

    #[compio::test]
    async fn test_close_fails_waiting_acquire() {
        let budget = ByteBudget::new(10);
        assert_ok!(budget.acquire(10).await);
        let closer = budget.clone();
        compio::runtime::spawn(async move {
            compio::time::sleep(Duration::from_millis(10)).await;
            closer.close();
        })
        .detach();

        assert_eq!(assert_err!(budget.acquire(1).await), Closed);
    }
}
