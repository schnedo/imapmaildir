use super::Repository;

pub struct Syncer<T, U>
where
    T: Repository,
    U: Repository,
{
    a: T,
    b: U,
}

impl<T, U> Syncer<T, U>
where
    T: Repository,
    U: Repository,
{
    pub fn new(a: T, b: U) -> Self {
        Self { a, b }
    }

    pub fn sync_a_to_b(&self) {
        assert!(
            self.a.validity() == self.b.validity(),
            "repositories need same validity to sync"
        );
    }
}
