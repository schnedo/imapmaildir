use futures::StreamExt;

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

    pub async fn init_a_to_b(&mut self) {
        assert!(
            self.a.validity() == self.b.validity(),
            "repositories need same validity to sync"
        );
        let all_mail = self.a.get_all();
        all_mail
            .for_each(async |mail| {
                self.b.store(&mail);
            })
            .await;
    }
}
