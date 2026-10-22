use crate::repository::Uid;

/// Maps message sequence numbers to UIDs of the selected mailbox.
///
/// Used without QRESYNC to translate `EXPUNGE` and UID-less `FETCH` responses to UIDs. Sequence
/// order equals UID order, so an expunge shifts all following messages down by one. Slots of
/// messages with unknown UID are `None`. Any inconsistency invalidates the map instead of guessing,
/// as a wrong mapping would delete the wrong local mail.
///
/// Slots are a gap buffer with the gap at the last expunge, so runs of expunges in ascending or
/// descending order cost O(n) in total instead of O(n) each.
#[derive(Debug)]
pub struct UidMap {
    slots: Vec<Option<Uid>>,
    gap_start: usize,
    gap_end: usize,
    valid: bool,
    /// Expunges since [`Self::begin_command`]
    expunged: usize,
    /// Slots with unknown UID. Only accurate while valid
    unknown: usize,
}

impl UidMap {
    pub fn new(exists: u32) -> Self {
        Self {
            slots: vec![None; exists as usize],
            gap_start: 0,
            gap_end: 0,
            valid: true,
            expunged: 0,
            unknown: exists as usize,
        }
    }

    /// Whether learning UIDs of fetched mails could resolve later responses. False if all UIDs are
    /// known or the map is invalid, as only seeding validates it again.
    pub fn needs_uids(&self) -> bool {
        self.valid && self.unknown > 0
    }

    #[cfg(test)]
    pub fn is_valid(&self) -> bool {
        self.valid
    }

    fn invalidate(&mut self, reason: &str) {
        if self.valid {
            log::debug!("invalidating sequence to uid map: {reason}");
        }
        self.valid = false;
    }

    fn len(&self) -> usize {
        self.slots.len() - (self.gap_end - self.gap_start)
    }

    fn physical(&self, index: usize) -> usize {
        if index < self.gap_start {
            index
        } else {
            index + (self.gap_end - self.gap_start)
        }
    }

    fn get(&self, index: usize) -> Option<Uid> {
        if index < self.len() {
            self.slots[self.physical(index)]
        } else {
            None
        }
    }

    fn move_gap(&mut self, index: usize) {
        if self.gap_start == self.gap_end {
            self.gap_start = index;
            self.gap_end = index;
        } else if index < self.gap_start {
            let count = self.gap_start - index;
            self.slots
                .copy_within(index..self.gap_start, self.gap_end - count);
            self.gap_start = index;
            self.gap_end -= count;
        } else if index > self.gap_start {
            let count = index - self.gap_start;
            self.slots
                .copy_within(self.gap_end..self.gap_end + count, self.gap_start);
            self.gap_start = index;
            self.gap_end += count;
        }
    }

    /// Removes the gap so that `slots` holds exactly the messages in sequence order
    fn compact(&mut self) {
        let len = self.len();
        self.move_gap(len);
        self.slots.truncate(len);
        self.gap_end = len;
    }

    /// Replaces the map with the complete, ascending and duplicate-free UID list of the mailbox
    pub fn seed(&mut self, uids: &[Uid], exists: u32) {
        debug_assert!(uids.is_sorted());
        self.gap_start = 0;
        self.gap_end = 0;
        self.valid = uids.len() == exists as usize;
        self.unknown = if self.valid { 0 } else { exists as usize };
        if self.valid {
            self.slots.clear();
            self.slots.extend(uids.iter().copied().map(Some));
        } else {
            log::debug!(
                "not seeding sequence to uid map with {} uids for {exists} messages",
                uids.len()
            );
            self.slots.clear();
            self.slots.resize(exists as usize, None);
        }
    }

    /// Starts counting expunges during a single command
    pub fn begin_command(&mut self) {
        self.expunged = 0;
    }

    /// Expunges since [`Self::begin_command`]. Sequence numbers of responses received before an
    /// expunge are stale afterwards.
    pub fn expunged_in_command(&self) -> usize {
        self.expunged
    }

    pub fn on_exists(&mut self, exists: u32) {
        let exists = exists as usize;
        let len = self.len();
        if exists < len {
            self.invalidate("EXISTS shrank without EXPUNGE");
            self.compact();
            self.slots.truncate(exists);
            self.gap_start = exists;
            self.gap_end = exists;
        } else {
            // slots after the gap are the last messages, so pushing to slots appends messages
            self.unknown += exists - len;
            self.slots.resize(self.slots.len() + exists - len, None);
        }
    }

    /// Removes the message at `sequence` and returns its UID if known
    pub fn on_expunge(&mut self, sequence: u32) -> Option<Uid> {
        self.expunged += 1;
        let Some(index) = Self::index(sequence).filter(|index| *index < self.len()) else {
            self.invalidate("EXPUNGE out of range");
            return None;
        };
        self.move_gap(index);
        let uid = self.slots[self.gap_end];
        self.gap_end += 1;
        if uid.is_none() {
            self.unknown = self.unknown.saturating_sub(1);
        }

        if self.valid { uid } else { None }
    }

    /// Records the UID of the message at `sequence` and returns its UID if known
    pub fn on_fetch(&mut self, sequence: u32, uid: Option<Uid>) -> Option<Uid> {
        let Some(index) = Self::index(sequence).filter(|index| *index < self.len()) else {
            self.invalidate("FETCH out of range");
            return None;
        };
        let slot = self.physical(index);
        match (self.slots[slot], uid) {
            (Some(known), Some(uid)) if known != uid => {
                self.invalidate("FETCH UID differs from known UID");
                return None;
            }
            (None, Some(uid)) => {
                let before = index.checked_sub(1).and_then(|before| self.get(before));
                let after = self.get(index + 1);
                if before.is_some_and(|before| before >= uid)
                    || after.is_some_and(|after| after <= uid)
                {
                    self.invalidate("FETCH UID out of order");
                    return None;
                }
                self.slots[slot] = Some(uid);
                self.unknown = self.unknown.saturating_sub(1);
            }
            _ => {}
        }

        if self.valid { self.slots[slot] } else { None }
    }

    fn index(sequence: u32) -> Option<usize> {
        (sequence as usize).checked_sub(1)
    }
}

#[cfg(test)]
mod tests {
    use assertables::*;

    use super::*;

    fn uid(uid: u32) -> Uid {
        assert_ok!(Uid::try_from(uid))
    }

    fn seeded(uids: &[u32]) -> UidMap {
        let exists = assert_ok!(u32::try_from(uids.len()));
        let mut map = UidMap::new(exists);
        let uids: Vec<Uid> = uids.iter().map(|u| uid(*u)).collect();
        map.seed(&uids, exists);
        assert!(map.is_valid());
        map
    }

    #[test]
    fn test_expunge_resolves_uid_and_shifts_following() {
        let mut map = seeded(&[3, 5, 8]);

        assert_eq!(map.on_expunge(2), Some(uid(5)));
        assert_eq!(map.on_expunge(2), Some(uid(8)));
        assert_eq!(map.on_expunge(1), Some(uid(3)));
        assert!(map.is_valid());
    }

    #[test]
    fn test_expunge_of_unknown_slot_is_none_but_valid() {
        let mut map = seeded(&[3]);
        map.on_exists(2);

        assert_none!(map.on_expunge(2));
        assert!(map.is_valid());
    }

    #[test]
    fn test_expunge_out_of_range_invalidates() {
        let mut map = seeded(&[3]);

        assert_none!(map.on_expunge(2));
        assert!(!map.is_valid());
        assert_none!(map.on_expunge(1));
    }

    #[test]
    fn test_seed_with_wrong_count_invalidates() {
        let mut map = UidMap::new(2);
        map.seed(&[uid(1)], 2);

        assert!(!map.is_valid());
    }

    #[test]
    fn test_exists_shrink_invalidates() {
        let mut map = seeded(&[1, 2]);
        map.on_exists(1);

        assert!(!map.is_valid());
    }

    #[test]
    fn test_fetch_fills_unknown_slot() {
        let mut map = UidMap::new(2);

        assert_eq!(map.on_fetch(1, Some(uid(4))), Some(uid(4)));
        assert_eq!(map.on_fetch(2, Some(uid(7))), Some(uid(7)));
        assert_eq!(map.on_fetch(2, None), Some(uid(7)));
        assert_eq!(map.on_expunge(1), Some(uid(4)));
    }

    #[test]
    fn test_fetch_without_uid_of_unknown_slot_is_none() {
        let mut map = UidMap::new(1);

        assert_none!(map.on_fetch(1, None));
        assert!(map.is_valid());
    }

    #[test]
    fn test_fetch_with_differing_uid_invalidates() {
        let mut map = seeded(&[3]);

        assert_none!(map.on_fetch(1, Some(uid(4))));
        assert!(!map.is_valid());
    }

    #[test]
    fn test_fetch_out_of_order_invalidates() {
        let mut map = seeded(&[3]);
        map.on_exists(2);

        assert_none!(map.on_fetch(2, Some(uid(2))));
        assert!(!map.is_valid());
    }

    #[test]
    fn test_fetch_out_of_range_invalidates() {
        let mut map = seeded(&[3]);

        assert_none!(map.on_fetch(2, Some(uid(4))));
        assert!(!map.is_valid());
    }

    #[test]
    fn test_expunges_in_any_order_match_naive_removal() {
        let uids: Vec<u32> = (1..=50).map(|u| u * 2).collect();
        let mut map = seeded(&uids);
        let mut expected: Vec<Uid> = uids.iter().map(|u| uid(*u)).collect();
        let mut state = 7usize;
        while !expected.is_empty() {
            state = state.wrapping_mul(1_103_515_245).wrapping_add(12345);
            let index = state % expected.len();
            let sequence = assert_ok!(u32::try_from(index + 1));

            assert_eq!(map.on_expunge(sequence), Some(expected.remove(index)));
            for (index, expected) in expected.iter().enumerate() {
                let sequence = assert_ok!(u32::try_from(index + 1));
                assert_eq!(map.on_fetch(sequence, None), Some(*expected));
            }
        }
        assert!(map.is_valid());
    }

    #[test]
    fn test_exists_after_expunge_appends_unknown_slots() {
        let mut map = seeded(&[3, 5, 8]);
        assert_eq!(map.on_expunge(2), Some(uid(5)));
        map.on_exists(4);

        assert_eq!(map.on_fetch(2, None), Some(uid(8)));
        assert_none!(map.on_fetch(3, None));
        assert_eq!(map.on_fetch(4, Some(uid(10))), Some(uid(10)));
        assert_eq!(map.on_expunge(1), Some(uid(3)));
        assert_eq!(map.on_fetch(3, None), Some(uid(10)));
        assert!(map.is_valid());
    }

    #[test]
    fn test_fetch_order_check_skips_gap() {
        let mut map = UidMap::new(4);
        assert_eq!(map.on_fetch(1, Some(uid(2))), Some(uid(2)));
        assert_eq!(map.on_fetch(4, Some(uid(9))), Some(uid(9)));
        assert_none!(map.on_expunge(2));

        assert_none!(map.on_fetch(2, Some(uid(10))));
        assert!(!map.is_valid());
    }

    #[test]
    fn test_exists_shrink_after_expunge_keeps_remaining() {
        let mut map = seeded(&[1, 2, 3, 4]);
        assert_some!(map.on_expunge(2));
        map.on_exists(2);

        assert!(!map.is_valid());
        assert_eq!(map.len(), 2);
    }

    #[test]
    fn test_expunge_of_new_slot_is_none_but_valid() {
        let mut map = seeded(&[3]);
        map.on_exists(3);

        assert_none!(map.on_expunge(3));
        assert_none!(map.on_expunge(2));
        assert!(map.is_valid());
        assert_eq!(map.on_expunge(1), Some(uid(3)));
    }

    #[test]
    fn test_needs_uids_until_all_known() {
        let mut map = seeded(&[3]);
        assert!(!map.needs_uids());

        map.on_exists(3);
        assert!(map.needs_uids());
        assert_none!(map.on_expunge(3));
        assert!(map.needs_uids());
        assert_some!(map.on_fetch(2, Some(uid(5))));
        assert!(!map.needs_uids());
    }

    #[test]
    fn test_invalid_map_needs_no_uids() {
        let mut map = UidMap::new(2);
        assert!(map.needs_uids());

        map.on_exists(1);
        assert!(!map.needs_uids());
    }

    #[test]
    fn test_begin_command_resets_expunge_count() {
        let mut map = seeded(&[3, 4]);
        assert_some!(map.on_expunge(1));
        assert_eq!(map.expunged_in_command(), 1);

        map.begin_command();
        assert_eq!(map.expunged_in_command(), 0);
        assert_some!(map.on_expunge(1));
        assert_eq!(map.expunged_in_command(), 1);
    }
}
