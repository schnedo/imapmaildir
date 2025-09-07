use futures::StreamExt;

use super::Repository;

pub struct Syncer<T, U>
where
    T: Repository,
    U: Repository,
{
    remote: T,
    local: U,
}

impl<T, U> Syncer<T, U>
where
    T: Repository,
    U: Repository,
{
    pub fn new(remote: T, local: U) -> Self {
        assert!(
            remote.validity() == local.validity(),
            "repositories need same validity to sync"
        );
        Self { remote, local }
    }

    pub async fn init_remote_to_local(&mut self) {
        let all_mail = self.remote.get_all();
        all_mail
            .for_each(async |mail| {
                self.local.store(&mail);
            })
            .await;
    }
}
