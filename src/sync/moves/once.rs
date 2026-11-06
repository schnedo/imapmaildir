//! Hands mails moved between maildirs of mailboxes over to the server as `UID MOVE` when syncing
//! once, instead of deleting them in the source mailbox and uploading them to the destination.
//!
//! Without watching, moves are recognized by file name prefix: a mail deleted from one mailbox
//! (its state knows the prefix) that appears as new mail in another one moved. Each mailbox session
//! detects its local changes while connecting, using [`MoveReporter::detect`], and reports the
//! prefixes of its deletions and new mails to the [`Planner`]. The planner pairs them once all
//! sessions reported, and hands each session its part of the moves, [`PlannedMoves`]. Detecting
//! only needs the local files, so reporting never waits for the server.
//!
//! The sessions agree like those of watching do: destinations accept moves as soon as they got
//! their remote changes, sources then move the mails and tell the destinations their new UIDs.
//! Dropping a session's moves, e.g. as it failed, makes the other side delete or upload the mail
//! instead.

use std::{
    collections::{BTreeMap, HashMap, HashSet},
    path::Path,
    sync::mpsc,
};

use crossfire::oneshot::{RxOneshot, TxOneshot, oneshot};
use enumflags2::BitFlags;
use futures::future::join_all;

use crate::{
    imap::RemoteMailMetadata,
    maildir::{ChangeRecorder, LoadError, LocalChanges, MaildirRepository},
    repository::{Flag, Uid},
    sync::{
        Error,
        moves::{Accepted, Departure, Incoming, Moved, Transfer},
    },
};

/// Mail of this mailbox that another mailbox may take
struct Outgoing {
    uid: Uid,
    prefix: String,
    destination: String,
    /// Whether to copy the mail, as another destination takes it by moving
    copy: bool,
    accepted: RxOneshot<TxOneshot<Moved>>,
}

/// The part of a mailbox in moves between mailboxes
#[derive(Default)]
pub(crate) struct PlannedMoves {
    outgoing: Vec<Outgoing>,
    /// Mails moving here, by file name prefix
    incoming: HashMap<String, TxOneshot<TxOneshot<Moved>>>,
}

impl PlannedMoves {
    /// Drops the updates of mails that move out from `updates`, instead of applying them in this
    /// mailbox, as their files are gone. The destination mailbox applies them with its next
    /// sync, as the moved mails keep their server flags. Until then they are not stored as local
    /// changes, as the destination compares local flags with the ones of the last sync.
    pub(crate) fn drop_remote_updates(&self, updates: &mut Vec<RemoteMailMetadata>) {
        if self.outgoing.is_empty() {
            return;
        }
        let outgoing: HashSet<Uid> = self.outgoing.iter().map(|mail| mail.uid).collect();
        updates.retain(|update| !outgoing.contains(&update.uid()));
    }

    /// Accepts the moves into this mailbox. Returns the ones that are due.
    pub(crate) fn accept_incoming(&mut self) -> Incoming {
        let moves = std::mem::take(&mut self.incoming)
            .into_iter()
            .filter_map(|(prefix, accept)| {
                // moves, whose source gave up, would never arrive
                if accept.is_disconnected() {
                    return None;
                }
                let (reply, moved) = oneshot();
                accept.send(reply);
                Some((prefix, moved))
            })
            .collect();
        Incoming::planned(moves)
    }

    /// UIDs of the mails of this mailbox, which move out
    pub(crate) fn outgoing_uids(&self) -> Vec<Uid> {
        let mut uids: Vec<Uid> = self.outgoing.iter().map(|mail| mail.uid).collect();
        uids.sort_unstable();
        uids.dedup();
        uids
    }

    /// Waits for the destinations to accept the moves. Returns the accepted ones, with `stored`
    /// the flags of the mails at the last sync.
    pub(crate) async fn accepted(
        &mut self,
        stored: &HashMap<Uid, BitFlags<Flag>>,
    ) -> Vec<Accepted> {
        let outgoing = std::mem::take(&mut self.outgoing);
        let replies = join_all(outgoing.into_iter().map(|mail| async move {
            let Outgoing {
                uid,
                prefix,
                destination,
                copy,
                accepted,
            } = mail;
            let Ok(reply) = accepted.await else {
                log::debug!("{destination} did not accept {prefix}");
                return None;
            };
            let flags = stored.get(&uid).copied().unwrap_or_default();
            let departure = Departure { uid, prefix, flags };
            let transfer = if copy { Transfer::Copy } else { Transfer::Move };
            Some(Accepted::new(departure, destination, transfer, reply))
        }))
        .await;

        replies.into_iter().flatten().collect()
    }

    /// Agrees with the other mailboxes on the planned moves. `stored` are the flags of the
    /// outgoing mails at the last sync.
    pub(crate) async fn agree(
        mut self,
        stored: &HashMap<Uid, BitFlags<Flag>>,
    ) -> (Vec<Accepted>, Incoming) {
        // before waiting, as the other mailboxes may wait for this one
        let incoming = self.accept_incoming();
        let accepted = self.accepted(stored).await;

        (accepted, incoming)
    }
}

/// What pairing needs to know about the local changes of a mailbox
#[derive(Debug, Default)]
struct Summary {
    /// File name prefixes of deleted mails by UID
    deleted: HashMap<Uid, String>,
    /// File name prefixes of new mails
    news: HashSet<String>,
}

impl Summary {
    fn of(changes: &LocalChanges) -> Self {
        Self {
            deleted: changes
                .deletions
                .iter()
                .filter_map(|uid| Some((*uid, changes.deleted_prefixes.get(uid)?.clone())))
                .collect(),
            news: changes
                .news
                .iter()
                .map(|mail| mail.fileprefix().to_string())
                .collect(),
        }
    }
}

/// Pairs the deletions and new mails of the mailboxes in `summaries`. Mails that moved to more
/// than one mailbox move to the first one by name and get copied to the others.
fn plan(summaries: &BTreeMap<String, Summary>) -> HashMap<String, PlannedMoves> {
    let mut planned: HashMap<String, PlannedMoves> = summaries
        .keys()
        .map(|mailbox| (mailbox.clone(), PlannedMoves::default()))
        .collect();
    // prefix to source mailbox and UID
    let mut departures: HashMap<&str, (&str, Uid)> = HashMap::new();
    for (mailbox, summary) in summaries {
        for (uid, prefix) in &summary.deleted {
            departures.entry(prefix).or_insert((mailbox, *uid));
        }
    }
    // prefix to destination mailboxes in ascending order
    let mut arrivals: BTreeMap<&str, Vec<&str>> = BTreeMap::new();
    for (mailbox, summary) in summaries {
        for prefix in &summary.news {
            if departures
                .get(prefix.as_str())
                .is_some_and(|(source, _)| source != mailbox)
            {
                arrivals.entry(prefix).or_default().push(mailbox);
            }
        }
    }
    for (prefix, mut destinations) in arrivals {
        destinations.sort_unstable();
        let (source, uid) = departures[prefix];
        for (index, destination) in destinations.into_iter().enumerate() {
            let (accept, accepted) = oneshot();
            if let Some(moves) = planned.get_mut(destination) {
                moves.incoming.insert(prefix.to_string(), accept);
            }
            if let Some(moves) = planned.get_mut(source) {
                moves.outgoing.push(Outgoing {
                    uid,
                    prefix: prefix.to_string(),
                    destination: destination.to_string(),
                    // the first destination takes the mail by moving
                    copy: index > 0,
                    accepted,
                });
            }
        }
    }

    planned
}

/// A session's summary and the way to answer it with its part of the moves
struct Report {
    mailbox: String,
    summary: Summary,
    reply: TxOneshot<PlannedMoves>,
}

/// Collects the reports of the sessions and pairs their moves
pub struct Planner {
    reports: mpsc::Receiver<Report>,
}

/// Lets a session report its local changes to the [`Planner`]
#[derive(Clone)]
pub struct MoveReporter {
    reports: mpsc::Sender<Report>,
}

/// Creates the planner and the reporter its sessions get clones of. Drop the reporter after
/// handing out the clones, as the planner finishes once all reporters are gone.
#[must_use]
pub fn planner() -> (MoveReporter, Planner) {
    let (reports, receiver) = mpsc::channel();
    (MoveReporter { reports }, Planner { reports: receiver })
}

impl Planner {
    /// Waits until all sessions reported or gave up, then hands them their moves. Blocks.
    pub fn plan(self) {
        let mut summaries = BTreeMap::new();
        let mut replies = HashMap::new();
        while let Ok(Report {
            mailbox,
            summary,
            reply,
        }) = self.reports.recv()
        {
            replies.insert(mailbox.clone(), reply);
            summaries.insert(mailbox, summary);
        }
        let mut planned = plan(&summaries);
        for (mailbox, reply) in replies {
            if let Some(moves) = planned.remove(&mailbox) {
                reply.send(moves);
            }
        }
    }
}

/// Local changes of a mailbox with the moves the other sessions agreed on
pub(crate) struct Detected {
    pub changes: LocalChanges,
    /// Resolves once all sessions reported, or errors if the planner gave up or the mailbox had no
    /// changes that could be part of a move
    pub moves: RxOneshot<PlannedMoves>,
}

/// Loaded maildir with the local changes a session detected before connecting. Pass it to
/// [`crate::Syncer::sync_once`].
pub struct Detection {
    pub(crate) repository: MaildirRepository,
    pub(crate) detected: Detected,
}

impl MoveReporter {
    /// Detects the local changes of the maildir of `mailbox` and reports them to the planner.
    /// Gives up the report, if the mailbox has no completed initial sync or nothing that could be
    /// part of a move, so the planner does not wait for it. Recovered mails get reported to
    /// `recorder`.
    ///
    /// # Errors
    /// If loading the maildir or detecting its changes fails
    pub async fn detect(
        self,
        mailbox: &str,
        mail_dir: &Path,
        state_dir: &Path,
        recorder: Option<ChangeRecorder>,
    ) -> Result<Option<Detection>, Error> {
        let repository = match MaildirRepository::load(
            &mail_dir.join(mailbox),
            &state_dir.join(mailbox),
        )
        .await
        {
            Ok(repository) => repository,
            Err(LoadError::Uninitialized) => return Ok(None),
            Err(e) => return Err(e.into()),
        };
        if !repository.is_initial_sync_complete().await? {
            return Ok(None);
        }
        // detecting local changes may already recover interrupted downloads
        repository.record_changes(recorder);
        let changes = repository.detect_changes().await?;
        let (reply, moves) = oneshot();
        // without deletions and new mails, nothing can pair, so nobody has to wait for this one
        if !changes.deletions.is_empty() || !changes.news.is_empty() {
            let report = Report {
                mailbox: mailbox.to_string(),
                summary: Summary::of(&changes),
                reply,
            };
            // a planner that is gone leaves the receiver erroring, which gives up the moves
            let _ = self.reports.send(report);
        }

        Ok(Some(Detection {
            repository,
            detected: Detected { changes, moves },
        }))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::maildir::{LocalFlagChangesBuilder, NewLocalMailMetadata};

    fn block_on<F: Future>(future: F) -> F::Output {
        compio::runtime::Runtime::new().unwrap().block_on(future)
    }

    fn uid(number: u32) -> Uid {
        Uid::try_from(number).unwrap()
    }

    fn changes(deleted: &[(u32, &str)], news: &[&str]) -> LocalChanges {
        LocalChanges::new(
            deleted.iter().map(|(number, _)| uid(*number)).collect(),
            news.iter()
                .map(|prefix| NewLocalMailMetadata::new(BitFlags::empty(), (*prefix).to_string()))
                .collect(),
            LocalFlagChangesBuilder::default(),
        )
        .with_deleted_prefixes(
            deleted
                .iter()
                .map(|(number, prefix)| (uid(*number), (*prefix).to_string()))
                .collect(),
        )
    }

    fn mailboxes(
        changes: impl IntoIterator<Item = (&'static str, LocalChanges)>,
    ) -> BTreeMap<String, Summary> {
        changes
            .into_iter()
            .map(|(mailbox, changes)| (mailbox.to_string(), Summary::of(&changes)))
            .collect()
    }

    #[test]
    fn test_deletion_with_new_mail_elsewhere_is_move() {
        let planned = plan(&mailboxes([
            ("a", changes(&[(1, "x.P1.host")], &[])),
            ("b", changes(&[], &["x.P1.host"])),
        ]));

        let outgoing = &planned["a"].outgoing;
        assert_eq!(outgoing.len(), 1);
        assert_eq!(outgoing[0].uid, uid(1));
        assert_eq!(outgoing[0].destination, "b");
        assert!(!outgoing[0].copy);
        assert!(planned["b"].incoming.contains_key("x.P1.host"));
    }

    #[test]
    fn test_deletion_without_new_mail_is_no_move() {
        let planned = plan(&mailboxes([
            ("a", changes(&[(1, "x.P1.host")], &[])),
            ("b", changes(&[], &["y.P1.host"])),
        ]));

        assert!(planned["a"].outgoing.is_empty());
        assert!(planned["b"].incoming.is_empty());
    }

    #[test]
    fn test_new_mail_without_deletion_is_no_move() {
        let planned = plan(&mailboxes([
            ("a", changes(&[], &[])),
            ("b", changes(&[], &["x.P1.host"])),
        ]));

        assert!(planned["b"].incoming.is_empty());
    }

    #[test]
    fn test_same_prefix_in_same_mailbox_is_no_move() {
        let planned = plan(&mailboxes([(
            "a",
            changes(&[(1, "x.P1.host")], &["x.P1.host"]),
        )]));

        assert!(planned["a"].outgoing.is_empty());
        assert!(planned["a"].incoming.is_empty());
    }

    #[test]
    fn test_mail_in_two_mailboxes_moves_to_first_and_copies_to_others() {
        let planned = plan(&mailboxes([
            ("a", changes(&[(1, "x.P1.host")], &[])),
            ("c", changes(&[], &["x.P1.host"])),
            ("b", changes(&[], &["x.P1.host"])),
        ]));

        let mut outgoing: Vec<_> = planned["a"]
            .outgoing
            .iter()
            .map(|mail| (mail.destination.as_str(), mail.copy))
            .collect();
        outgoing.sort_unstable();
        assert_eq!(outgoing, [("b", false), ("c", true)]);
        assert!(planned["b"].incoming.contains_key("x.P1.host"));
        assert!(planned["c"].incoming.contains_key("x.P1.host"));
    }

    #[test]
    fn test_mutual_moves_pair_both_ways() {
        let planned = plan(&mailboxes([
            ("a", changes(&[(1, "x.P1.host")], &["y.P1.host"])),
            ("b", changes(&[(2, "y.P1.host")], &["x.P1.host"])),
        ]));

        assert_eq!(planned["a"].outgoing.len(), 1);
        assert_eq!(planned["b"].outgoing.len(), 1);
        assert!(planned["a"].incoming.contains_key("y.P1.host"));
        assert!(planned["b"].incoming.contains_key("x.P1.host"));
    }

    #[test]
    fn test_dropped_destination_is_not_accepting() {
        let mut planned = plan(&mailboxes([
            ("a", changes(&[(1, "x.P1.host")], &[])),
            ("b", changes(&[], &["x.P1.host"])),
        ]));
        let mut source = planned.remove("a").unwrap();
        drop(planned);

        let accepted = block_on(source.accepted(&HashMap::new()));

        assert!(accepted.is_empty());
    }

    #[test]
    fn test_accepted_destination_gets_flags_of_last_sync() {
        let mut planned = plan(&mailboxes([
            ("a", changes(&[(1, "x.P1.host")], &[])),
            ("b", changes(&[], &["x.P1.host"])),
        ]));
        let mut source = planned.remove("a").unwrap();
        let mut destination = planned.remove("b").unwrap();
        let mut incoming = destination.accept_incoming();
        let stored = HashMap::from([(uid(1), Flag::Seen.into())]);

        let mut accepted = block_on(source.accepted(&stored));
        assert_eq!(accepted.len(), 1);
        assert!(matches!(accepted[0].transfer, Transfer::Move));
        accepted.remove(0).moved(5, uid(3));
        let moved = block_on(incoming.arrival("x.P1.host")).unwrap();

        assert_eq!(moved.uid, uid(3));
        assert_eq!(moved.flags, BitFlags::from(Flag::Seen));
    }

    #[test]
    fn test_planner_answers_reporters_once_all_are_gone() {
        let (reporter, planner) = planner();
        let summaries = mailboxes([
            ("a", changes(&[(1, "x.P1.host")], &[])),
            ("b", changes(&[], &["x.P1.host"])),
        ]);
        let mut replies = HashMap::new();
        for (mailbox, summary) in summaries {
            let (reply, moves) = oneshot();
            replies.insert(mailbox.clone(), moves);
            reporter
                .reports
                .send(Report {
                    mailbox,
                    summary,
                    reply,
                })
                .unwrap();
        }
        drop(reporter);

        planner.plan();

        let source = block_on(replies.remove("a").unwrap()).unwrap();
        let destination = block_on(replies.remove("b").unwrap()).unwrap();
        assert_eq!(source.outgoing.len(), 1);
        assert!(destination.incoming.contains_key("x.P1.host"));
    }

    #[test]
    fn test_dropped_planner_leaves_reporters_without_moves() {
        let (reply, moves) = oneshot::<PlannedMoves>();
        let (reporter, planner) = planner();
        drop(planner);
        let report = Report {
            mailbox: "a".to_string(),
            summary: Summary::default(),
            reply,
        };

        // reporting must not fail, as the session continues without moves
        let _ = reporter.reports.send(report);

        assert!(block_on(moves).is_err());
    }
}
