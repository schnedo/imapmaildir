use std::collections::HashMap;

use enumflags2::BitFlags;

use crate::{
    maildir::LocalMail,
    repository::{Flag, ModSeq, SequenceSet, SequenceSetBuilder, Uid},
};

#[derive(Debug, Default, PartialEq, Eq, Hash, Clone, Copy)]
pub struct FlagChange {
    pub added: BitFlags<Flag>,
    pub removed: BitFlags<Flag>,
}

impl FlagChange {
    fn then(self, next: Self) -> Self {
        Self {
            added: (self.added & !next.removed) | next.added,
            removed: (self.removed & !next.added) | next.removed,
        }
    }
}

#[derive(Debug)]
pub struct LocalFlagChanges {
    changes: HashMap<FlagChange, SequenceSet>,
}

impl LocalFlagChanges {
    pub fn iter(&self) -> impl Iterator<Item = (FlagChange, &SequenceSet)> {
        self.changes.iter().map(|(change, set)| (*change, set))
    }
}

#[derive(Debug, Default, PartialEq, Clone)]
pub struct LocalFlagChangesBuilder {
    changes: HashMap<Uid, FlagChange>,
}

impl LocalFlagChangesBuilder {
    pub fn build(self) -> LocalFlagChanges {
        let mut grouped: HashMap<FlagChange, SequenceSetBuilder> = HashMap::new();
        for (uid, change) in self.changes {
            if change != FlagChange::default() {
                grouped.entry(change).or_default().add(uid);
            }
        }

        LocalFlagChanges {
            changes: grouped
                .into_iter()
                .filter_map(|(change, builder)| builder.build().ok().map(|set| (change, set)))
                .collect(),
        }
    }

    pub fn insert(&mut self, uid: Uid, change: FlagChange) {
        let entry = self.changes.entry(uid).or_default();
        *entry = entry.then(change);
    }

    pub fn remove(&mut self, uid: Uid) {
        if self.changes.remove(&uid).is_some() {
            log::debug!("removed {uid} from local changes");
        }
    }
}

#[derive(Debug, PartialEq)]
pub struct LocalChanges {
    pub highest_modseq: ModSeq,
    pub updates: LocalFlagChangesBuilder,
    pub deletions: Vec<Uid>,
    pub news: Vec<LocalMail>,
}

impl LocalChanges {
    pub fn new(
        highest_modseq: ModSeq,
        deletions: Vec<Uid>,
        news: Vec<LocalMail>,
        updates: LocalFlagChangesBuilder,
    ) -> Self {
        Self {
            highest_modseq,
            updates,
            deletions,
            news,
        }
    }
}

#[cfg(test)]
mod tests {

    use assertables::*;
    use rstest::*;

    use crate::maildir::NewLocalMailMetadata;

    use super::*;

    #[fixture]
    fn builder() -> LocalFlagChangesBuilder {
        LocalFlagChangesBuilder::default()
    }

    #[fixture]
    fn uid() -> Uid {
        Uid::MAX
    }

    #[fixture]
    fn set(uid: Uid) -> SequenceSet {
        let mut builder = SequenceSetBuilder::default();
        builder.add(uid);
        assert_ok!(builder.build())
    }

    fn change(added: BitFlags<Flag>, removed: BitFlags<Flag>) -> FlagChange {
        FlagChange { added, removed }
    }

    #[rstest]
    fn test_builder_builds_empty_changes(builder: LocalFlagChangesBuilder) {
        let changes = builder.build();
        assert_is_empty!(changes.iter().collect::<Vec<_>>());
    }

    #[rstest]
    fn test_insert_inserts_change(
        mut builder: LocalFlagChangesBuilder,
        uid: Uid,
        set: SequenceSet,
    ) {
        let expected = change(Flag::Seen.into(), Flag::Flagged.into());
        builder.insert(uid, expected);
        let changes = builder.build();
        assert_eq!(vec![(expected, &set)], changes.iter().collect::<Vec<_>>());
    }

    #[rstest]
    fn test_insert_combines_changes_of_same_mail(
        mut builder: LocalFlagChangesBuilder,
        uid: Uid,
        set: SequenceSet,
    ) {
        builder.insert(uid, change(Flag::Seen.into(), Flag::Flagged.into()));
        builder.insert(
            uid,
            change(Flag::Flagged | Flag::Answered, Flag::Seen.into()),
        );
        let changes = builder.build();
        assert_eq!(
            vec![(
                change(Flag::Flagged | Flag::Answered, Flag::Seen.into()),
                &set
            )],
            changes.iter().collect::<Vec<_>>()
        );
    }

    #[rstest]
    fn test_build_groups_mails_with_same_change(mut builder: LocalFlagChangesBuilder) {
        let expected = change(Flag::Seen.into(), BitFlags::empty());
        let first = assert_ok!(Uid::try_from(1));
        let second = assert_ok!(Uid::try_from(2));
        builder.insert(first, expected);
        builder.insert(second, expected);
        let changes = builder.build();
        let mut set = SequenceSetBuilder::default();
        set.add(first);
        set.add(second);
        assert_eq!(
            vec![(expected, &assert_ok!(set.build()))],
            changes.iter().collect::<Vec<_>>()
        );
    }

    #[rstest]
    fn test_remove_removes_change(mut builder: LocalFlagChangesBuilder, uid: Uid) {
        builder.insert(uid, change(Flag::Seen.into(), Flag::Flagged.into()));
        builder.remove(uid);
        let changes = builder.build();
        assert_is_empty!(changes.iter().collect::<Vec<_>>());
    }

    #[rstest]
    fn test_local_changes_constructs_correctly(builder: LocalFlagChangesBuilder) {
        let highest_modseq = assert_ok!(ModSeq::try_from(9));
        let deletions = vec![Uid::MAX];
        let flags = BitFlags::all();
        let metadata = NewLocalMailMetadata::new(flags, "prefix".to_string());
        let mail = LocalMail::new(Vec::new(), metadata);
        let news = vec![mail];

        let changes = LocalChanges::new(
            highest_modseq,
            deletions.clone(),
            news.clone(),
            builder.clone(),
        );
        assert_eq!(changes.highest_modseq, highest_modseq);
        assert_eq!(changes.deletions, deletions);
        assert_eq!(changes.news, news);
        assert_eq!(changes.updates, builder);
    }
}
