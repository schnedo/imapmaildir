use std::collections::HashMap;

use crate::repository::{Flag, LocalMail, ModSeq, SequenceSet, SequenceSetBuilder, Uid};

#[derive(Debug)]
pub struct LocalFlagChanges {
    additional_flags: HashMap<Flag, SequenceSet>,
    removed_flags: HashMap<Flag, SequenceSet>,
}

impl LocalFlagChanges {
    pub fn additional_flags(&self) -> impl Iterator<Item = (Flag, &SequenceSet)> {
        self.additional_flags.iter().map(|(flag, set)| (*flag, set))
    }

    pub fn removed_flags(&self) -> impl Iterator<Item = (Flag, &SequenceSet)> {
        self.removed_flags.iter().map(|(flag, set)| (*flag, set))
    }
}

#[derive(Debug, Default)]
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
                .map(|(flag, builder)| {
                    (
                        flag,
                        builder.build().expect("sequence set should be buildable"),
                    )
                })
                .collect(),
            removed_flags: self
                .removed_flags
                .drain()
                .map(|(flag, builder)| {
                    (
                        flag,
                        builder.build().expect("sequence set should be buildable"),
                    )
                })
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
            set.remove(uid);
            todo!("more removal")
        }
    }
}

#[derive(Debug)]
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
