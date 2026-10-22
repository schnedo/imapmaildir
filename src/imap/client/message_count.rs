#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MessageCount {
    exists: u32,
    known: u32,
    /// Expunges since [`Self::begin_command`]
    expunged_in_command: u32,
}

impl MessageCount {
    pub fn new(exists: u32) -> Self {
        Self {
            exists,
            known: exists,
            expunged_in_command: 0,
        }
    }

    pub fn on_exists(&mut self, exists: u32) {
        self.exists = exists;
    }

    pub fn on_fetch(&mut self, sequence: u32) {
        self.known = self.known.max(sequence);
    }

    /// Starts counting expunges during a command whose FETCH responses get applied afterwards via
    /// [`Self::on_command_fetch`]. Returns the current message count.
    pub fn begin_command(&mut self) -> u32 {
        self.expunged_in_command = 0;
        self.exists
    }

    /// Marks messages up to `highest_sequence` known after a command, and all `covered` messages
    /// if it covered every message existing at its start. Both may be stale by expunges received
    /// afterwards, so they get lowered by all expunges during the command. This may underestimate
    /// but never makes new messages known.
    pub fn on_command_fetch(&mut self, highest_sequence: u32, covered: Option<u32>) {
        let known = highest_sequence
            .max(covered.unwrap_or_default())
            .saturating_sub(self.expunged_in_command);
        self.known = self.known.max(known).min(self.exists);
    }

    pub fn on_expunge(&mut self, count: u32) {
        self.expunged_in_command = self.expunged_in_command.saturating_add(count);
        self.exists = self.exists.saturating_sub(count);
        self.known = self.known.saturating_sub(count).min(self.exists);
    }

    pub fn on_append(&mut self, count: u32) {
        self.known = self.known.saturating_add(count);
    }

    pub fn exists(self) -> u32 {
        self.exists
    }

    pub fn has_unknown(self) -> bool {
        self.exists > self.known
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_new_count_has_no_unknown() {
        assert!(!MessageCount::new(3).has_unknown());
    }

    #[test]
    fn test_exists_increase_is_unknown() {
        let mut count = MessageCount::new(3);
        count.on_exists(4);

        assert!(count.has_unknown());
    }

    #[test]
    fn test_fetch_of_new_message_makes_it_known() {
        let mut count = MessageCount::new(3);
        count.on_exists(5);
        count.on_fetch(4);
        assert!(count.has_unknown());

        count.on_fetch(5);
        assert!(!count.has_unknown());
    }

    #[test]
    fn test_fetch_of_old_message_does_not_make_new_known() {
        let mut count = MessageCount::new(3);
        count.on_exists(4);
        count.on_fetch(2);

        assert!(count.has_unknown());
    }

    #[test]
    fn test_command_fetch_before_expunge_does_not_make_new_known() {
        let mut count = MessageCount::new(10);
        count.on_exists(11);
        count.begin_command();
        // FETCH of sequence 11 was collected, then sequence 1 got expunged and another mail arrived
        count.on_expunge(1);
        count.on_exists(11);
        count.on_command_fetch(11, None);

        assert!(count.has_unknown());
    }

    #[test]
    fn test_command_fetch_never_exceeds_exists() {
        let mut count = MessageCount::new(10);
        count.on_exists(11);
        count.begin_command();
        count.on_expunge(2);
        count.on_command_fetch(11, None);
        count.on_exists(10);

        assert!(count.has_unknown());
    }

    #[test]
    fn test_command_covering_all_makes_them_known() {
        let mut count = MessageCount::new(3);
        count.on_exists(5);
        let covered = count.begin_command();
        count.on_command_fetch(0, Some(covered));

        assert!(!count.has_unknown());
    }

    #[test]
    fn test_command_covering_all_with_expunge_converges() {
        let mut count = MessageCount::new(3);
        count.on_exists(5);
        let covered = count.begin_command();
        count.on_expunge(1);
        count.on_command_fetch(0, Some(covered));
        assert!(!count.has_unknown());

        let covered = count.begin_command();
        count.on_command_fetch(0, Some(covered));
        assert!(!count.has_unknown());
    }

    #[test]
    fn test_own_append_is_known() {
        let mut count = MessageCount::new(3);
        count.on_exists(5);
        count.on_append(2);

        assert!(!count.has_unknown());
    }

    #[test]
    fn test_remote_mail_during_own_append_is_unknown() {
        let mut count = MessageCount::new(3);
        count.on_exists(6);
        count.on_append(2);

        assert!(count.has_unknown());
    }

    #[test]
    fn test_expunge_of_known_message_keeps_count_known() {
        let mut count = MessageCount::new(3);
        count.on_expunge(1);

        assert!(!count.has_unknown());
    }

    #[test]
    fn test_expunge_keeps_unknown_mail_unknown() {
        let mut count = MessageCount::new(3);
        count.on_exists(4);
        count.on_expunge(1);

        assert!(count.has_unknown());
    }
}
