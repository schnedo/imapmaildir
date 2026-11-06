use std::collections::HashMap;

use enumflags2::BitFlags;

use crate::{
    maildir::{NewLocalMailMetadata, OtherFlags},
    notmuch::Revision,
    repository::{Flag, KeywordChange, SequenceSet, SequenceSetBuilder, Uid},
};

/// Flag and keyword changes applied to a mail
#[derive(Debug, Default, PartialEq, Eq, Hash, Clone)]
pub struct FlagChange {
    pub added: BitFlags<Flag>,
    pub removed: BitFlags<Flag>,
    pub keywords: KeywordChange,
}

impl FlagChange {
    /// Creates a change of system flags without keyword changes
    pub fn new(added: BitFlags<Flag>, removed: BitFlags<Flag>) -> Self {
        Self {
            added,
            removed,
            keywords: KeywordChange::default(),
        }
    }

    /// Creates a change consisting only of keyword changes
    pub fn of_keywords(keywords: KeywordChange) -> Self {
        Self {
            keywords,
            ..Self::default()
        }
    }

    /// Combines this change with `next`, which happened afterwards
    pub fn then(&self, next: &Self) -> Self {
        Self {
            added: (self.added & !next.removed) | next.added,
            removed: (self.removed & !next.added) | next.removed,
            keywords: self.keywords.then(&next.keywords),
        }
    }

    /// Whether the change neither adds nor removes anything
    pub fn is_empty(&self) -> bool {
        self.added.is_empty() && self.removed.is_empty() && self.keywords.is_empty()
    }
}

/// Flag changes grouped by identical change, with the uids they apply to
#[derive(Debug)]
pub struct LocalFlagChanges {
    changes: HashMap<FlagChange, SequenceSet>,
}

impl LocalFlagChanges {
    /// Iterates over each distinct change with the uids it applies to
    pub fn iter(&self) -> impl Iterator<Item = (&FlagChange, &SequenceSet)> {
        self.changes.iter()
    }
}

/// Collects pending flag changes per uid before grouping them
#[derive(Debug, Default, PartialEq, Clone)]
pub struct LocalFlagChangesBuilder {
    changes: HashMap<Uid, FlagChange>,
}

impl LocalFlagChangesBuilder {
    /// Groups uids by change, dropping empty changes
    pub fn build(self) -> LocalFlagChanges {
        let mut grouped: HashMap<FlagChange, SequenceSetBuilder> = HashMap::new();
        for (uid, change) in self.changes {
            if !change.is_empty() {
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

    /// Records `change` for `uid`, merging it with an already recorded one
    pub fn insert(&mut self, uid: Uid, change: FlagChange) {
        self.changes
            .entry(uid)
            .and_modify(|existing| *existing = existing.then(&change))
            .or_insert(change);
    }

    /// Forgets changes recorded for `uid`
    pub fn remove(&mut self, uid: Uid) {
        if self.changes.remove(&uid).is_some() {
            log::debug!("removed {uid} from local changes");
        }
    }
}

/// All changes of the local maildir, which still need to be synced to the remote
#[derive(Debug, Default, PartialEq)]
pub struct LocalChanges {
    pub updates: LocalFlagChangesBuilder,
    pub deletions: Vec<Uid>,
    /// File name prefixes of deleted mails, which may have moved to another mailbox
    pub deleted_prefixes: HashMap<Uid, String>,
    pub news: Vec<NewLocalMailMetadata>,
    /// Flags without IMAP equivalent changed by other clients, which only need storing
    pub other_flags: Vec<(Uid, OtherFlags)>,
    /// Revision of notmuch, up to which tag changes are included
    pub tags_revision: Option<Revision>,
}

impl LocalChanges {
    /// Sets the file name prefixes of the deleted mails
    #[must_use]
    pub fn with_deleted_prefixes(mut self, deleted_prefixes: HashMap<Uid, String>) -> Self {
        self.deleted_prefixes = deleted_prefixes;
        self
    }

    /// Creates changes from deletions, new mails and flag updates, leaving the rest empty
    pub fn new(
        deletions: Vec<Uid>,
        news: Vec<NewLocalMailMetadata>,
        updates: LocalFlagChangesBuilder,
    ) -> Self {
        Self {
            updates,
            deletions,
            news,
            ..Self::default()
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
        FlagChange::new(added, removed)
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
        builder.insert(uid, expected.clone());
        let changes = builder.build();
        assert_eq!(vec![(&expected, &set)], changes.iter().collect::<Vec<_>>());
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
                &change(Flag::Flagged | Flag::Answered, Flag::Seen.into()),
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
        builder.insert(first, expected.clone());
        builder.insert(second, expected.clone());
        let changes = builder.build();
        let mut set = SequenceSetBuilder::default();
        set.add(first);
        set.add(second);
        assert_eq!(
            vec![(&expected, &assert_ok!(set.build()))],
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
        let deletions = vec![Uid::MAX];
        let flags = BitFlags::all();
        let metadata = NewLocalMailMetadata::new(flags, "prefix".to_string());
        let news = vec![metadata];

        let changes = LocalChanges::new(deletions.clone(), news.clone(), builder.clone());
        assert_eq!(changes.deletions, deletions);
        assert_eq!(changes.news, news);
        assert_eq!(changes.updates, builder);
    }
}
