//! Lock access that cannot panic.
//!
//! A std lock is poisoned only when a thread panicked while holding it. The
//! guarded data here are plain maps that are never left half-updated across a
//! panic point, so recovering the guard is safe, and it keeps one failed task
//! from turning every later tool call into an error (or, with `unwrap`, a
//! crash of the whole server).

use std::sync::{PoisonError, RwLock, RwLockReadGuard, RwLockWriteGuard};

/// Read guard, recovered if the lock was poisoned.
pub fn read<T>(lock: &RwLock<T>) -> RwLockReadGuard<'_, T> {
    lock.read().unwrap_or_else(PoisonError::into_inner)
}

/// Write guard, recovered if the lock was poisoned.
pub fn write<T>(lock: &RwLock<T>) -> RwLockWriteGuard<'_, T> {
    lock.write().unwrap_or_else(PoisonError::into_inner)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_poisoned_lock_is_recovered() {
        // Given a lock poisoned by a panicking writer
        let lock = std::sync::Arc::new(RwLock::new(1));
        let poisoner = lock.clone();
        let _ = std::thread::spawn(move || {
            let _guard = poisoner.write().unwrap();
            panic!("poison");
        })
        .join();
        assert!(lock.is_poisoned());
        // Then both guards still give access to the data
        *write(&lock) += 1;
        assert_eq!(*read(&lock), 2);
    }
}
