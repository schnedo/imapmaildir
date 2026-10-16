#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MessageCount {
    exists: u32,
    known: u32,
}

impl MessageCount {
    pub fn new(exists: u32) -> Self {
        Self {
            exists,
            known: exists,
        }
    }

    pub fn on_exists(&mut self, exists: u32) {
        self.exists = exists;
    }

    pub fn on_fetch(&mut self, sequence: u32) {
        self.known = self.known.max(sequence);
    }

    pub fn on_expunge(&mut self, count: u32) {
        self.exists = self.exists.saturating_sub(count);
        self.known = self.known.saturating_sub(count).min(self.exists);
    }

    pub fn on_append(&mut self, count: u32) {
        self.known = self.known.saturating_add(count);
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
