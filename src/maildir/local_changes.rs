use std::collections::HashMap;

use log::debug;

use crate::{
    maildir::LocalMail,
    repository::{Flag, ModSeq, SequenceSet, SequenceSetBuilder, Uid},
};

#[derive(Debug)]
pub struct LocalFlagChanges {
    additional_flags: HashMap<Flag, SequenceSet>,
    removed_flags: HashMap<Flag, SequenceSet>,
}

impl LocalFlagChanges {
    // todo: use single unpack function and return owned SequenceSets?
    pub fn additional_flags(&self) -> impl Iterator<Item = (Flag, &SequenceSet)> {
        self.additional_flags.iter().map(|(flag, set)| (*flag, set))
    }

    pub fn removed_flags(&self) -> impl Iterator<Item = (Flag, &SequenceSet)> {
        self.removed_flags.iter().map(|(flag, set)| (*flag, set))
    }
}

#[derive(Debug, Default, PartialEq, Clone)]
pub struct LocalFlagChangesBuilder {
    additional_flags: HashMap<Flag, SequenceSetBuilder>,
    removed_flags: HashMap<Flag, SequenceSetBuilder>,
}

impl LocalFlagChangesBuilder {
    pub fn build(mut self) -> LocalFlagChanges {
        LocalFlagChanges {
            additional_flags: self
                .additional_flags
                .drain()
                .filter_map(|(flag, builder)| builder.build().ok().map(|set| (flag, set)))
                .collect(),
            removed_flags: self
                .removed_flags
                .drain()
                .filter_map(|(flag, builder)| builder.build().ok().map(|set| (flag, set)))
                .collect(),
        }
    }

    fn insert_into(map: &mut HashMap<Flag, SequenceSetBuilder>, flag: Flag, uid: Uid) {
        if let Some(set) = map.get_mut(&flag) {
            set.add(uid);
        } else {
            let mut set = SequenceSetBuilder::default();
            set.add(uid);
            map.insert(flag, set);
        }
    }

    pub fn insert_additional(&mut self, flag: Flag, uid: Uid) {
        Self::insert_into(&mut self.additional_flags, flag, uid);
    }

    pub fn insert_removed(&mut self, flag: Flag, uid: Uid) {
        Self::insert_into(&mut self.removed_flags, flag, uid);
    }

    pub fn remove(&mut self, uid: Uid) {
        Self::remove_from(&mut self.additional_flags, uid);
        Self::remove_from(&mut self.removed_flags, uid);
    }

    fn remove_from(map: &mut HashMap<Flag, SequenceSetBuilder>, uid: Uid) {
        for set in map.values_mut() {
            if set.remove(uid) {
                debug!("removed {uid} from local changes");
            }
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
    use enumflags2::BitFlags;
    use rstest::*;

    use crate::maildir::LocalMailMetadata;

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

    #[fixture]
    fn flag() -> Flag {
        Flag::Seen
    }

    #[rstest]
    fn test_builder_builds_empty_changes(builder: LocalFlagChangesBuilder) {
        let changes = builder.build();
        assert_is_empty!(changes.additional_flags().collect::<HashMap<_, _>>());
        assert_is_empty!(changes.removed_flags().collect::<HashMap<_, _>>());
    }

    #[rstest]
    fn test_insert_additional_inserts_additional(
        mut builder: LocalFlagChangesBuilder,
        flag: Flag,
        uid: Uid,
        set: SequenceSet,
    ) {
        builder.insert_additional(flag, uid);
        let changes = builder.build();
        assert_eq!(
            HashMap::from([(flag, &set)]),
            changes.additional_flags().collect()
        );
        assert_is_empty!(changes.removed_flags().collect::<HashMap<_, _>>());
    }

    #[rstest]
    fn test_insert_removed_inserts_removed(
        mut builder: LocalFlagChangesBuilder,
        flag: Flag,
        uid: Uid,
        set: SequenceSet,
    ) {
        builder.insert_removed(flag, uid);
        let changes = builder.build();
        assert_is_empty!(changes.additional_flags().collect::<HashMap<_, _>>());
        assert_eq!(
            HashMap::from([(flag, &set)]),
            changes.removed_flags().collect()
        );
    }

    #[rstest]
    fn test_remove_removes_from_both_collections(
        mut builder: LocalFlagChangesBuilder,
        flag: Flag,
        uid: Uid,
    ) {
        builder.insert_additional(flag, uid);
        builder.insert_additional(flag, uid);
        builder.remove(uid);
        let changes = builder.build();
        assert_is_empty!(changes.additional_flags().collect::<HashMap<_, _>>());
        assert_is_empty!(changes.removed_flags().collect::<HashMap<_, _>>());
    }

    #[rstest]
    fn test_local_changes_constructs_correctly(builder: LocalFlagChangesBuilder) {
        let highest_modseq = assert_ok!(ModSeq::try_from(9));
        let deletions = vec![Uid::MAX];
        let flags = BitFlags::all();
        let metadata =
            LocalMailMetadata::new(Uid::try_from(&3).ok(), flags, Some("prefix".to_string()));
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
