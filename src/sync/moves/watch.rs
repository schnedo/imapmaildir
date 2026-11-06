//! Recognizes mails moved between maildirs by watching.
//!
//! The watchers of both mailboxes report the halves of the rename to a shared
//! [`MoveRegistry`](crate::maildir::MoveRegistry), which pairs them by their inotify cookie. The
//! source mailbox then offers the move to the destination mailbox. Once the destination accepted,
//! the source moves the mail and tells the destination its new UID. While watching, either side
//! falls back to deleting or uploading the mail, if the other one does not respond in time.

use std::{
    cell::RefCell,
    collections::HashMap,
    sync::Arc,
    time::{Duration, Instant},
};

use crossfire::{
    AsyncRx, MTx, mpsc,
    oneshot::{RxOneshot, TxOneshot, oneshot},
};
use futures::future::join_all;

use super::{Accepted, Departure, Incoming, Moved, Transfer};
use crate::maildir::{MoveRegistry, MoveTap};

/// How long to wait for the watcher of the other mailbox to report its half of a rename
const PAIR_TIMEOUT: Duration = Duration::from_millis(200);
/// How long mailboxes wait for each other to agree on a move, e.g. while the other one is
/// reconnecting. Late agreements are safe: the mailbox that gave up deleted or uploaded the mail,
/// so the other one finds nobody accepting.
const AGREE_TIMEOUT: Duration = Duration::from_secs(2);
/// How long offers wait for the arrival of their mail, as the watchers of both mailboxes split
/// bulk moves into batches differently
const OFFER_MAX_AGE: Duration = Duration::from_secs(30);

/// Offers to move the mail of the rename with `cookie` to the receiving mailbox
pub(crate) struct Offer {
    cookie: u32,
    /// Accepts the offer by handing over the sender for the move's result
    accept: TxOneshot<TxOneshot<Moved>>,
}

/// Offered departure waiting for the acceptance of its destination
pub(crate) struct Offered {
    departure: Departure,
    destination: String,
    accepted: RxOneshot<TxOneshot<Moved>>,
}

/// Connection of a mailbox to the other mailboxes synced by the process
pub struct MoveLink {
    mailbox: String,
    tap: MoveTap,
    peers: Arc<HashMap<String, MTx<mpsc::List<Offer>>>>,
    inbox: AsyncRx<mpsc::List<Offer>>,
    /// Offers received before the arrival of their mail got handled, by cookie
    pending: RefCell<HashMap<u32, (Offer, Instant)>>,
}

impl std::fmt::Debug for MoveLink {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("MoveLink")
            .field("mailbox", &self.mailbox)
            .finish_non_exhaustive()
    }
}

impl MoveLink {
    /// Connects `mailboxes`. Each link belongs to the mailbox of the same name.
    #[must_use]
    pub fn connect(mailboxes: &[String]) -> HashMap<String, MoveLink> {
        // unbounded, as dropped offers would upload mails instead of moving them. The offers of
        // moved mails are all there is to queue.
        let (txs, rxs): (Vec<_>, Vec<_>) = mailboxes
            .iter()
            .map(|_| mpsc::unbounded_async::<Offer>())
            .unzip();
        let peers: Arc<HashMap<_, _>> = Arc::new(mailboxes.iter().cloned().zip(txs).collect());
        let registry = Arc::new(MoveRegistry::default());

        mailboxes
            .iter()
            .cloned()
            .zip(rxs)
            .map(|(mailbox, inbox)| {
                let link = MoveLink {
                    tap: MoveTap::new(mailbox.clone(), registry.clone()),
                    mailbox: mailbox.clone(),
                    peers: peers.clone(),
                    inbox,
                    pending: RefCell::new(HashMap::new()),
                };
                (mailbox, link)
            })
            .collect()
    }

    /// Reports the renames in the mailbox's maildir, which the watcher has to pass on
    #[must_use]
    pub(crate) fn tap(&self) -> MoveTap {
        self.tap.clone()
    }

    /// Whether the mail with file name `prefix` is known to have moved out of the mailbox
    #[must_use]
    pub(crate) fn has_departure(&self, prefix: &str) -> bool {
        self.tap.departure(prefix).is_some()
    }

    /// Whether the mail with file name `prefix` is known to have moved into the mailbox
    #[must_use]
    pub(crate) fn has_arrival(&self, prefix: &str) -> bool {
        self.tap.arrival(prefix).is_some()
    }

    /// Waits for the watcher of the other mailbox to report its half of each rename
    async fn resolve<T>(
        &self,
        renames: impl Iterator<Item = (T, u32)>,
    ) -> Vec<(T, u32, Option<crate::maildir::Pair>)> {
        join_all(renames.map(|(item, cookie)| async move {
            (item, cookie, self.tap.pair(cookie, PAIR_TIMEOUT).await)
        }))
        .await
    }

    /// Offers `departures` that moved to other mailboxes. Returns the offered ones.
    pub(crate) async fn offer_departures(&self, departures: Vec<Departure>) -> Vec<Offered> {
        let renames = departures.into_iter().filter_map(|departure| {
            let cookie = self.tap.departure(&departure.prefix)?;
            Some((departure, cookie))
        });
        let mut offered = Vec::new();
        for (departure, cookie, pair) in self.resolve(renames).await {
            let Some(pair) = pair else {
                continue;
            };
            let destination = pair.to.mailbox;
            if pair.from.mailbox != self.mailbox || destination == self.mailbox {
                continue;
            }
            let Some(peer) = self.peers.get(&destination) else {
                continue;
            };
            let (accept, accepted) = oneshot();
            if peer.send(Offer { cookie, accept }).is_ok() {
                log::debug!("offering {} to {destination}", departure.prefix);
                offered.push(Offered {
                    departure,
                    destination,
                    accepted,
                });
            }
        }

        offered
    }

    /// Waits for the destinations of `offered` to accept. Returns the accepted ones.
    pub(crate) async fn await_acceptance(&self, offered: Vec<Offered>) -> Vec<Accepted> {
        let deadline = Instant::now() + AGREE_TIMEOUT;
        let mut accepted = Vec::new();
        for Offered {
            departure,
            destination,
            accepted: reply,
        } in offered
        {
            let remaining = deadline.saturating_duration_since(Instant::now());
            if let Ok(Ok(reply)) = compio::time::timeout(remaining, reply).await {
                accepted.push(Accepted::new(departure, destination, Transfer::Move, reply));
            } else {
                log::debug!("{destination} did not accept {}", departure.prefix);
            }
        }

        accepted
    }

    /// Accepts the offers for the mails with the file name `prefixes`, which moved here from
    /// other mailboxes, waiting for them to arrive. Keeps others for later arrivals.
    pub(crate) async fn accept_arrivals(&self, prefixes: Vec<String>) -> Incoming {
        let renames = prefixes.into_iter().filter_map(|prefix| {
            let cookie = self.tap.arrival(&prefix)?;
            Some((prefix, cookie))
        });
        let mut wanted: HashMap<u32, String> = self
            .resolve(renames)
            .await
            .into_iter()
            .filter_map(|(prefix, cookie, pair)| {
                let from = pair?.from.mailbox;
                (from != self.mailbox && self.peers.contains_key(&from)).then_some((cookie, prefix))
            })
            .collect();

        let deadline = Instant::now() + AGREE_TIMEOUT;
        let mut incoming = Incoming::default();
        loop {
            self.drain_inbox();
            self.accept_pending(&mut wanted, &mut incoming);
            let remaining = deadline.saturating_duration_since(Instant::now());
            if wanted.is_empty() || remaining.is_zero() {
                break;
            }
            let Some(offer) = compio::time::timeout(remaining, self.inbox.recv())
                .await
                .ok()
                .and_then(Result::ok)
            else {
                break;
            };
            self.keep(offer);
        }
        self.pending
            .borrow_mut()
            .retain(|_, (_, received)| received.elapsed() < OFFER_MAX_AGE);

        incoming
    }

    fn keep(&self, offer: Offer) {
        self.pending
            .borrow_mut()
            .insert(offer.cookie, (offer, Instant::now()));
    }

    fn drain_inbox(&self) {
        while let Ok(offer) = self.inbox.try_recv() {
            self.keep(offer);
        }
    }

    /// Accepts the received offers for the `wanted` cookies. Others stay for later arrivals.
    fn accept_pending(&self, wanted: &mut HashMap<u32, String>, incoming: &mut Incoming) {
        let mut pending = self.pending.borrow_mut();
        wanted.retain(|cookie, prefix| {
            let Some((Offer { accept, .. }, _)) = pending.remove(cookie) else {
                return true;
            };
            // offers that got abandoned meanwhile are stale
            if !accept.is_disconnected() {
                let (reply, moved) = oneshot();
                accept.send(reply);
                incoming.moves.insert(std::mem::take(prefix), moved);
            }
            false
        });
    }
}

#[cfg(test)]
mod tests {
    use enumflags2::BitFlags;

    use super::*;
    use crate::repository::Uid;

    fn block_on<F: Future>(future: F) -> F::Output {
        compio::runtime::Runtime::new().unwrap().block_on(future)
    }

    fn uid(number: u32) -> Uid {
        Uid::try_from(number).unwrap()
    }

    fn departure(number: u32, prefix: &str) -> Departure {
        Departure {
            uid: uid(number),
            prefix: prefix.to_string(),
            flags: BitFlags::empty(),
        }
    }

    fn links() -> (MoveLink, MoveLink) {
        let mut links = MoveLink::connect(&["a".to_string(), "b".to_string()]);
        (links.remove("a").unwrap(), links.remove("b").unwrap())
    }

    #[test]
    fn test_agreed_move_reaches_destination() {
        let (a, b) = links();
        a.tap().moved_from(1, "x.P1.host,U=7:2,S");
        b.tap().moved_to(1, "x.P1.host,U=7:2,S");

        block_on(async {
            let offered = a.offer_departures(vec![departure(7, "x.P1.host")]).await;
            assert_eq!(offered.len(), 1);
            let mut incoming = b.accept_arrivals(vec!["x.P1.host".to_string()]).await;
            assert!(incoming.contains("x.P1.host"));
            let mut accepted = a.await_acceptance(offered).await;
            assert_eq!(accepted.len(), 1);
            let accepted = accepted.remove(0);
            assert_eq!(accepted.destination, "b");
            accepted.moved(42, uid(9));

            let moved = incoming.arrival("x.P1.host").await.unwrap();
            assert_eq!(moved.uid_validity, 42);
            assert_eq!(moved.uid, uid(9));
        });
    }

    #[test]
    fn test_deletion_without_rename_is_not_offered() {
        let (a, _b) = links();

        let offered = block_on(a.offer_departures(vec![departure(7, "x.P1.host")]));

        assert!(offered.is_empty());
    }

    #[test]
    fn test_move_out_of_synced_mailboxes_is_not_offered() {
        let (a, _b) = links();
        a.tap().moved_from(1, "x.P1.host,U=7:2,S");

        let offered = block_on(a.offer_departures(vec![departure(7, "x.P1.host")]));

        assert!(offered.is_empty());
    }

    #[test]
    fn test_arrival_without_rename_does_not_wait() {
        let (_a, b) = links();
        let start = Instant::now();

        let incoming = block_on(b.accept_arrivals(vec!["x.P1.host".to_string()]));

        assert!(!incoming.contains("x.P1.host"));
        assert!(start.elapsed() < PAIR_TIMEOUT);
    }

    #[test]
    fn test_offers_wait_for_arrivals_of_later_batches() {
        let (a, b) = links();
        for (cookie, prefix) in [(1, "x.P1.host"), (2, "y.P1.host")] {
            a.tap().moved_from(cookie, &format!("{prefix},U=7:2,S"));
            b.tap().moved_to(cookie, &format!("{prefix},U=7:2,S"));
        }

        block_on(async {
            let offered = a
                .offer_departures(vec![departure(7, "x.P1.host"), departure(8, "y.P1.host")])
                .await;
            assert_eq!(offered.len(), 2);
            let first = b.accept_arrivals(vec!["x.P1.host".to_string()]).await;
            assert!(first.contains("x.P1.host"));
            assert!(!first.contains("y.P1.host"));

            let start = Instant::now();
            let second = b.accept_arrivals(vec!["y.P1.host".to_string()]).await;

            assert!(second.contains("y.P1.host"));
            assert!(start.elapsed() < PAIR_TIMEOUT);
            assert_eq!(a.await_acceptance(offered).await.len(), 2);
        });
    }

    #[test]
    fn test_offers_without_arrival_stay_unaccepted() {
        let (a, b) = links();
        a.tap().moved_from(1, "x.P1.host,U=7:2,S");
        b.tap().moved_to(1, "x.P1.host,U=7:2,S");

        block_on(async {
            let offered = a.offer_departures(vec![departure(7, "x.P1.host")]).await;
            let incoming = b.accept_arrivals(Vec::new()).await;
            assert!(!incoming.contains("x.P1.host"));
            drop(b);
            assert!(a.await_acceptance(offered).await.is_empty());
        });
    }

    #[test]
    fn test_offers_for_thousands_of_mails_are_not_dropped() {
        const COUNT: u32 = 3000;
        let (a, b) = links();
        let prefixes: Vec<String> = (0..COUNT).map(|n| format!("{n}.P1.host")).collect();
        for (cookie, prefix) in (0..COUNT).zip(&prefixes) {
            a.tap().moved_from(cookie, &format!("{prefix},U=7:2,S"));
            b.tap().moved_to(cookie, &format!("{prefix},U=7:2,S"));
        }
        let departures = (0..COUNT)
            .zip(&prefixes)
            .map(|(n, prefix)| departure(n + 1, prefix))
            .collect();

        block_on(async {
            let offered = a.offer_departures(departures).await;
            assert_eq!(offered.len(), COUNT as usize);
            let incoming = b.accept_arrivals(prefixes.clone()).await;
            assert!(prefixes.iter().all(|prefix| incoming.contains(prefix)));
            assert_eq!(a.await_acceptance(offered).await.len(), COUNT as usize);
        });
    }
}
