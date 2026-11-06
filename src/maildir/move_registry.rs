//! Correlates the halves of renames between maildirs of different mailboxes. Moving a mail from
//! one maildir to another makes inotify report `MOVED_FROM` in the first and `MOVED_TO` in the
//! second directory, both carrying the same cookie. Cookies are unique system-wide, so they match
//! across the watchers of different mailboxes.

use std::{
    collections::HashMap,
    sync::{Arc, Mutex, MutexGuard},
    time::{Duration, Instant},
};

use futures::channel::oneshot;

/// Minimum time between scans for expired entries
const PRUNE_INTERVAL: Duration = Duration::from_secs(1);

/// How long halves of renames stay known, which only needs to cover the delay between the
/// watchers of both mailboxes
const MAX_AGE: Duration = Duration::from_secs(30);

/// Which side of a move a half is
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Direction {
    /// The mail moved out of the mailbox
    Departure,
    /// The mail moved into the mailbox
    Arrival,
}

/// Mail seen moving out of or into a mailbox
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Half {
    pub mailbox: String,
    /// File name prefix of the mail, which identifies it independently of UIDs and flags
    pub prefix: String,
}

/// Both halves of a move between mailboxes
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Pair {
    pub from: Half,
    pub to: Half,
}

/// Cookies by mailbox and file name prefix, nested to look them up without allocating keys
type Index = HashMap<String, HashMap<String, u32>>;

/// Halves of one rename
struct Entry {
    from: Option<Half>,
    to: Option<Half>,
    /// Tasks waiting for the second half
    waiters: Vec<oneshot::Sender<Pair>>,
    created: Instant,
}

impl Entry {
    fn pair(&self) -> Option<Pair> {
        Some(Pair {
            from: self.from.clone()?,
            to: self.to.clone()?,
        })
    }
}

#[derive(Default)]
struct State {
    entries: HashMap<u32, Entry>,
    /// Cookies of mails moved out of a mailbox
    departures: Index,
    /// Cookies of mails moved into a mailbox
    arrivals: Index,
    last_prune: Option<Instant>,
}

impl State {
    fn index(&mut self, direction: Direction) -> &mut Index {
        match direction {
            Direction::Departure => &mut self.departures,
            Direction::Arrival => &mut self.arrivals,
        }
    }

    fn lookup(&self, direction: Direction, mailbox: &str, prefix: &str) -> Option<u32> {
        let index = match direction {
            Direction::Departure => &self.departures,
            Direction::Arrival => &self.arrivals,
        };

        index.get(mailbox)?.get(prefix).copied()
    }

    /// Forgets the index key of `half`, unless a newer rename of the same mail replaced it
    fn forget(&mut self, direction: Direction, half: &Half, cookie: u32) {
        let index = self.index(direction);
        if let Some(prefixes) = index.get_mut(&half.mailbox) {
            if prefixes.get(&half.prefix) == Some(&cookie) {
                prefixes.remove(&half.prefix);
            }
            if prefixes.is_empty() {
                index.remove(&half.mailbox);
            }
        }
    }

    /// Forgets expired entries and their index keys. Scans at most once per [`PRUNE_INTERVAL`],
    /// as bulk moves publish many halves.
    fn prune(&mut self, now: Instant) {
        if self
            .last_prune
            .is_some_and(|last| now.duration_since(last) < PRUNE_INTERVAL)
        {
            return;
        }
        self.last_prune = Some(now);
        let expired: Vec<u32> = self
            .entries
            .iter()
            .filter(|(_, entry)| now.duration_since(entry.created) > MAX_AGE)
            .map(|(cookie, _)| *cookie)
            .collect();
        for cookie in expired {
            if let Some(entry) = self.entries.remove(&cookie) {
                if let Some(from) = entry.from {
                    self.forget(Direction::Departure, &from, cookie);
                }
                if let Some(to) = entry.to {
                    self.forget(Direction::Arrival, &to, cookie);
                }
            }
        }
        for entry in self.entries.values_mut() {
            entry.waiters.retain(|waiter| !waiter.is_canceled());
        }
    }
}

/// Halves of renames between maildirs, shared by the watchers of all mailboxes
#[derive(Default)]
pub struct MoveRegistry {
    state: Mutex<State>,
}

impl MoveRegistry {
    /// Locks the state, which stays consistent if a holder panicked
    fn lock(&self) -> MutexGuard<'_, State> {
        self.state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    fn publish(&self, cookie: u32, direction: Direction, half: Half) {
        let now = Instant::now();
        let mut state = self.lock();
        state.prune(now);
        state
            .index(direction)
            .entry(half.mailbox.clone())
            .or_default()
            .insert(half.prefix.clone(), cookie);
        let entry = state.entries.entry(cookie).or_insert_with(|| Entry {
            from: None,
            to: None,
            waiters: Vec::new(),
            created: now,
        });
        match direction {
            Direction::Departure => entry.from = Some(half),
            Direction::Arrival => entry.to = Some(half),
        }
        if let Some(pair) = entry.pair() {
            for waiter in entry.waiters.drain(..) {
                // waiters that gave up are gone
                let _ = waiter.send(pair.clone());
            }
        }
    }

    async fn pair(&self, cookie: u32, timeout: Duration) -> Option<Pair> {
        let waiting = {
            let mut state = self.lock();
            let entry = state.entries.get_mut(&cookie)?;
            if let Some(pair) = entry.pair() {
                return Some(pair);
            }
            let (tx, rx) = oneshot::channel();
            entry.waiters.push(tx);
            rx
        };

        compio::time::timeout(timeout, waiting)
            .await
            .ok()
            .and_then(Result::ok)
    }
}

/// Reports renames of one mailbox's maildir to the [`MoveRegistry`]
#[derive(Clone)]
pub struct MoveTap {
    mailbox: String,
    registry: Arc<MoveRegistry>,
}

impl std::fmt::Debug for MoveTap {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("MoveTap")
            .field("mailbox", &self.mailbox)
            .finish_non_exhaustive()
    }
}

/// File name prefix before the UID and flags, as stored in the mail's metadata. Whether the name
/// carries a UID, i.e. belongs to a mail synced from some mailbox, is the second value.
fn prefix_of(name: &str) -> (&str, bool) {
    let head = name.rsplit_once(":2,").map_or(name, |(head, _)| head);
    match head.rsplit_once(",U=") {
        Some((prefix, uid)) if uid.parse::<u32>().is_ok() => (prefix, true),
        _ => (head, false),
    }
}

impl MoveTap {
    #[must_use]
    pub fn new(mailbox: String, registry: Arc<MoveRegistry>) -> Self {
        Self { mailbox, registry }
    }

    /// Records that the mail `name` moved out of the mailbox, whose rename has no second half in
    /// the same maildir
    pub fn moved_from(&self, cookie: u32, name: &str) {
        self.publish(cookie, Direction::Departure, name);
    }

    /// Records that the mail `name` moved into the mailbox, whose rename has no first half in the
    /// same maildir
    pub fn moved_to(&self, cookie: u32, name: &str) {
        self.publish(cookie, Direction::Arrival, name);
    }

    fn publish(&self, cookie: u32, direction: Direction, name: &str) {
        let (prefix, tracked) = prefix_of(name);
        // mails without UID, e.g. deliveries, were never synced, so no mailbox can move them
        if !tracked {
            return;
        }
        let half = Half {
            mailbox: self.mailbox.clone(),
            prefix: prefix.to_string(),
        };
        self.registry.publish(cookie, direction, half);
    }

    /// Cookie of the rename moving the mail with file name `prefix` out of the mailbox
    #[must_use]
    pub(crate) fn departure(&self, prefix: &str) -> Option<u32> {
        self.registry
            .lock()
            .lookup(Direction::Departure, &self.mailbox, prefix)
    }

    /// Cookie of the rename moving the mail with file name `prefix` into the mailbox
    #[must_use]
    pub(crate) fn arrival(&self, prefix: &str) -> Option<u32> {
        self.registry
            .lock()
            .lookup(Direction::Arrival, &self.mailbox, prefix)
    }

    /// Waits up to `timeout` for the other half of the rename with `cookie`. `None` if the mail
    /// moved from or to somewhere unwatched.
    pub(crate) async fn pair(&self, cookie: u32, timeout: Duration) -> Option<Pair> {
        self.registry.pair(cookie, timeout).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn block_on<F: Future>(future: F) -> F::Output {
        compio::runtime::Runtime::new().unwrap().block_on(future)
    }

    fn taps() -> (MoveTap, MoveTap) {
        let registry = Arc::new(MoveRegistry::default());
        (
            MoveTap::new("a".to_string(), registry.clone()),
            MoveTap::new("b".to_string(), registry),
        )
    }

    fn pair(from_prefix: &str) -> Pair {
        Pair {
            from: Half {
                mailbox: "a".to_string(),
                prefix: from_prefix.to_string(),
            },
            to: Half {
                mailbox: "b".to_string(),
                prefix: from_prefix.to_string(),
            },
        }
    }

    #[test]
    fn test_prefix_of_strips_uid_and_flags() {
        assert_eq!(prefix_of("1.P1.host,U=3:2,S"), ("1.P1.host", true));
        assert_eq!(prefix_of("1.P1.host:2,"), ("1.P1.host", false));
        assert_eq!(prefix_of("1.P1.host"), ("1.P1.host", false));
    }

    #[test]
    fn test_prefix_of_keeps_commas_of_prefix() {
        assert_eq!(
            prefix_of("1.M1P2.host,S=100,W=104,U=3:2,S"),
            ("1.M1P2.host,S=100,W=104", true)
        );
        assert_eq!(
            prefix_of("1.M1P2.host,S=100,W=104:2,"),
            ("1.M1P2.host,S=100,W=104", false)
        );
    }

    #[test]
    fn test_untracked_names_are_not_published() {
        let (a, b) = taps();
        a.moved_from(1, "x.P1.host:2,S");
        b.moved_to(1, "x.P1.host:2,S");

        assert_eq!(a.departure("x.P1.host"), None);
        assert_eq!(b.arrival("x.P1.host"), None);
    }

    #[test]
    fn test_pair_resolves_in_either_order() {
        let (a, b) = taps();

        block_on(async {
            a.moved_from(1, "x.P1.host,U=3:2,S");
            b.moved_to(1, "x.P1.host,U=3:2,S");
            assert_eq!(
                b.pair(1, Duration::from_millis(10)).await,
                Some(pair("x.P1.host"))
            );

            b.moved_to(2, "y.P1.host,U=4:2,");
            let waiting = b.pair(2, Duration::from_secs(5));
            a.moved_from(2, "y.P1.host,U=4:2,");
            assert_eq!(waiting.await, Some(pair("y.P1.host")));
        });
    }

    #[test]
    fn test_pair_gives_up_without_second_half() {
        let (_, b) = taps();
        b.moved_to(3, "z.P1.host,U=5:2,");

        let result = block_on(b.pair(3, Duration::from_millis(10)));

        assert_eq!(result, None);
    }

    #[test]
    fn test_pair_of_unknown_cookie_is_none() {
        let (a, _) = taps();

        assert_eq!(block_on(a.pair(9, Duration::from_millis(10))), None);
    }

    #[test]
    fn test_departures_and_arrivals_are_found_by_prefix() {
        let (a, b) = taps();
        a.moved_from(1, "x.P1.host,U=3:2,S");
        b.moved_to(1, "x.P1.host,U=3:2,S");

        assert_eq!(a.departure("x.P1.host"), Some(1));
        assert_eq!(a.arrival("x.P1.host"), None);
        assert_eq!(b.arrival("x.P1.host"), Some(1));
        assert_eq!(b.departure("x.P1.host"), None);
        assert_eq!(b.arrival("other"), None);
    }

    #[test]
    fn test_expired_entries_get_pruned() {
        let registry = MoveRegistry::default();
        registry.publish(
            1,
            Direction::Departure,
            Half {
                mailbox: "a".to_string(),
                prefix: "old".to_string(),
            },
        );
        let later = Instant::now() + MAX_AGE + Duration::from_secs(1);

        let mut state = registry.lock();
        state.prune(later);

        assert!(state.entries.is_empty());
        assert!(state.departures.is_empty());
    }

    #[test]
    fn test_pruning_keeps_index_keys_of_newer_renames() {
        let registry = MoveRegistry::default();
        let half = |prefix: &str| Half {
            mailbox: "a".to_string(),
            prefix: prefix.to_string(),
        };
        registry.publish(1, Direction::Departure, half("same"));
        registry.publish(2, Direction::Departure, half("same"));
        let mut state = registry.lock();
        state.entries.get_mut(&1).unwrap().created -= MAX_AGE + Duration::from_secs(1);

        state.prune(Instant::now() + PRUNE_INTERVAL * 2);

        assert_eq!(state.lookup(Direction::Departure, "a", "same"), Some(2));
        assert!(state.entries.contains_key(&2));
        assert!(!state.entries.contains_key(&1));
    }
}
