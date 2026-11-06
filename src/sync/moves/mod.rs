//! Hands mails moved between maildirs of mailboxes synced by the same process over to the server
//! as `UID MOVE`, instead of deleting them in the source mailbox and uploading them to the
//! destination mailbox.
//!
//! Mailboxes recognize moves in one of two ways, see [`Moves`]: [`watch`] pairs the halves of
//! renames reported by the watchers, [`once`] pairs deletions and new mails by file name prefix
//! when syncing once. Either way the mailboxes then agree the same way: the destination accepts,
//! the source moves (or copies) the mail and tells the destination its new UID.

mod once;
mod watch;

use std::{collections::HashMap, time::Duration};

use crossfire::oneshot::{RxOneshot, TxOneshot};
use enumflags2::BitFlags;

use crate::{
    maildir::NewLocalMailMetadata,
    repository::{Flag, Uid},
};

pub(crate) use once::{Detected, PlannedMoves};
pub use once::{Detection, MoveReporter, planner};
pub use watch::MoveLink;

/// How long the destination waits for the source to finish moving, which may take several
/// commands for many mails
const MOVE_TIMEOUT: Duration = Duration::from_secs(60);

/// How mails get handed over to another mailbox
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub(crate) enum Transfer {
    /// Removes the mails from the source mailbox
    Move,
    /// Leaves the mails in the source mailbox
    Copy,
}

/// How mails moved between mailboxes get recognized
pub(crate) enum Moves<'a> {
    /// By watching, with the links to the other mailboxes
    Watched(&'a MoveLink),
    /// By comparing the changes of all mailboxes before syncing
    Planned(PlannedMoves),
}

impl Moves<'_> {
    /// UIDs of the mails that may have moved to other mailboxes, whose last synced flags
    /// [`Self::agree`] needs
    pub(crate) fn departing(
        &self,
        deletions: &[Uid],
        deleted_prefixes: &HashMap<Uid, String>,
    ) -> Vec<Uid> {
        match self {
            Moves::Watched(link) => deletions
                .iter()
                .filter(|uid| {
                    deleted_prefixes
                        .get(uid)
                        .is_some_and(|prefix| link.has_departure(prefix))
                })
                .copied()
                .collect(),
            Moves::Planned(planned) => planned.outgoing_uids(),
        }
    }

    /// Agrees with the other mailboxes on the mails that moved out of and into this mailbox.
    /// `flags` are the last synced flags of the [`Self::departing`] mails. Returns the moves the
    /// destinations accepted and the ones still due to arrive here.
    pub(crate) async fn agree(
        self,
        deleted_prefixes: &HashMap<Uid, String>,
        news: &[NewLocalMailMetadata],
        flags: &HashMap<Uid, BitFlags<Flag>>,
    ) -> (Vec<Accepted>, Incoming) {
        match self {
            Moves::Watched(link) => {
                let departures = flags
                    .iter()
                    .filter_map(|(uid, flags)| {
                        Some(Departure {
                            uid: *uid,
                            prefix: deleted_prefixes.get(uid)?.clone(),
                            flags: *flags,
                        })
                    })
                    .collect();
                let arrivals = news
                    .iter()
                    .filter(|mail| link.has_arrival(mail.fileprefix()))
                    .map(|mail| mail.fileprefix().to_string())
                    .collect();
                // offering while accepting lets mailboxes moving mails to each other agree
                // without waiting for timeouts
                let (offered, incoming) = futures::join!(
                    link.offer_departures(departures),
                    link.accept_arrivals(arrivals)
                );
                let accepted = link.await_acceptance(offered).await;

                (accepted, incoming)
            }
            Moves::Planned(planned) => planned.agree(flags).await,
        }
    }
}

/// Where a mail moved to
#[derive(Debug)]
pub(crate) struct Moved {
    /// UIDVALIDITY of the destination mailbox
    pub uid_validity: u32,
    /// UID in the destination mailbox
    pub uid: Uid,
    /// Flags the source mailbox stored at its last sync. The destination compares the file's
    /// flags to them to find local changes. Flags changed on the server since arrive with the
    /// destination's next sync.
    pub flags: BitFlags<Flag>,
}

/// A mail deleted locally that another mailbox of the same account may have taken
#[derive(Debug)]
pub(crate) struct Departure {
    pub uid: Uid,
    /// File name prefix of the mail
    pub prefix: String,
    /// Flags the mail had at the last sync
    pub flags: BitFlags<Flag>,
}

/// Departure the destination accepted
pub(crate) struct Accepted {
    pub departure: Departure,
    pub destination: String,
    pub transfer: Transfer,
    reply: TxOneshot<Moved>,
}

impl Accepted {
    /// Creates a departure the `destination` accepted with `reply`
    pub(crate) fn new(
        departure: Departure,
        destination: String,
        transfer: Transfer,
        reply: TxOneshot<Moved>,
    ) -> Self {
        Self {
            departure,
            destination,
            transfer,
            reply,
        }
    }

    /// Tells the destination where the mail moved to
    pub fn moved(self, uid_validity: u32, uid: Uid) {
        let flags = self.departure.flags;
        self.reply.send(Moved {
            uid_validity,
            uid,
            flags,
        });
    }
}

/// Moves a mailbox accepted from other mailboxes, which are still due
#[derive(Default)]
pub(crate) struct Incoming {
    pub(super) moves: HashMap<String, RxOneshot<Moved>>,
    /// Whether to wait for sources as long as they exist, instead of up to [`MOVE_TIMEOUT`]
    patient: bool,
}

impl Incoming {
    /// Creates the moves a mailbox accepted when syncing once, by file name prefix. Waiting for
    /// them has no timeout, as sources may be busy for long, but a source that gives up ends the
    /// wait.
    pub(crate) fn planned(moves: HashMap<String, RxOneshot<Moved>>) -> Self {
        Self {
            moves,
            patient: true,
        }
    }

    /// Whether the mail with this file name prefix is moving here
    #[must_use]
    pub fn contains(&self, prefix: &str) -> bool {
        self.moves.contains_key(prefix)
    }

    /// Waits for where the mail with `prefix` moved to. `None` if the move failed.
    pub async fn arrival(&mut self, prefix: &str) -> Option<Moved> {
        let moved = self.moves.remove(prefix)?;
        if self.patient {
            return moved.await.ok();
        }
        compio::time::timeout(MOVE_TIMEOUT, moved).await.ok()?.ok()
    }
}
