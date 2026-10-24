//! Same identity shares a lock; completed identities do not retain allocations.
use std::{
    collections::HashMap,
    sync::{Arc, Mutex, Weak},
};
use tokio::sync::{Mutex as AsyncMutex, OwnedMutexGuard};

#[derive(Default)]
pub(crate) struct KeyedLocks(Mutex<HashMap<String, Weak<AsyncMutex<()>>>>);

impl KeyedLocks {
    pub async fn lock(&self, key: &str) -> OwnedMutexGuard<()> {
        let lock = {
            let mut locks = self.0.lock().unwrap();
            if locks.len() >= 256 {
                locks.retain(|_, lock| lock.strong_count() > 0);
            }
            match locks.get(key).and_then(Weak::upgrade) {
                Some(lock) => lock,
                None => {
                    let lock = Arc::new(AsyncMutex::new(()));
                    locks.insert(key.into(), Arc::downgrade(&lock));
                    lock
                }
            }
        };
        lock.lock_owned().await
    }
}
