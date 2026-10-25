//! Keeps the threads of one envoy from getting in each other's way.
//!
//! The advisory locks keep envoys apart. Bundles are copied several at a time,
//! so two things need settling between the threads of one envoy as well: who
//! works on an entry, and who may count on the free space.

use std::collections::HashSet;
use std::sync::{Arc, Condvar, Mutex, MutexGuard, PoisonError};
use std::time::Duration;

use crate::key::EntryKey;

/// How often a fill waiting for room looks at the volume again.
const ROOM_RECHECK: Duration = Duration::from_millis(250);

#[derive(Debug, Default)]
pub(crate) struct Coordinator {
    /// Entries a thread is working on now.
    turns: Mutex<HashSet<String>>,
    turn_ended: Condvar,
    /// Bytes promised to fills that are still copying.
    promised: Mutex<u64>,
    promise_ended: Condvar,
}

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

impl Coordinator {
    /// Wait until no other thread is working on `key`, then work on it.
    ///
    /// Two threads that ask for one entry take turns, so the second finds it
    /// cached instead of competing with the first for its lock.
    pub(crate) fn take_turn(self: &Arc<Self>, key: &EntryKey) -> Turn {
        let name = key.to_string();
        let mut turns = lock(&self.turns);
        while !turns.insert(name.clone()) {
            turns = self
                .turn_ended
                .wait(turns)
                .unwrap_or_else(PoisonError::into_inner);
        }
        Turn {
            coordinator: Arc::clone(self),
            name,
        }
    }

    /// Promise `bytes` of the volume to a fill that needs `needed` free.
    ///
    /// `measure` reports the free space on the volume now. That is what is on
    /// disk, and a fill that has only just started has written nothing yet, so
    /// fills planned together would each see all of it. A fill that fits on its
    /// own but not beside the others waits for them and looks again, which is
    /// what it would have found had they gone one after another. When it does
    /// not fit even then, the error carries the free space.
    pub(crate) fn promise(
        self: &Arc<Self>,
        bytes: u64,
        needed: u64,
        measure: impl Fn() -> Option<u64>,
    ) -> Result<Promise, u64> {
        let mut promised = lock(&self.promised);
        // If the volume cannot be measured, the loop ends and the fill tries anyway.
        while let Some(free) = measure() {
            // The fills ahead only use more, so waiting for them cannot help.
            if free < needed {
                return Err(free);
            }
            if free.saturating_sub(*promised) >= needed {
                break;
            }
            promised = self
                .promise_ended
                .wait_timeout(promised, ROOM_RECHECK)
                .unwrap_or_else(PoisonError::into_inner)
                .0;
        }
        *promised += bytes;
        Ok(Promise {
            coordinator: Arc::clone(self),
            bytes,
        })
    }
}

/// The right to work on one entry, held until dropped.
pub(crate) struct Turn {
    coordinator: Arc<Coordinator>,
    name: String,
}

impl Drop for Turn {
    fn drop(&mut self) {
        lock(&self.coordinator.turns).remove(&self.name);
        self.coordinator.turn_ended.notify_all();
    }
}

/// Space promised to a fill, held until it ends.
pub(crate) struct Promise {
    coordinator: Arc<Coordinator>,
    bytes: u64,
}

impl Drop for Promise {
    fn drop(&mut self) {
        let mut promised = lock(&self.coordinator.promised);
        *promised = promised.saturating_sub(self.bytes);
        drop(promised);
        self.coordinator.promise_ended.notify_all();
    }
}

#[cfg(test)]
mod tests {
    use std::sync::mpsc;
    use std::thread;

    use super::*;

    const SHORT: Duration = Duration::from_millis(200);
    const LONG: Duration = Duration::from_secs(10);

    fn key(name: &str) -> EntryKey {
        EntryKey::new("gt", name, "1.0.0").expect("key is valid")
    }

    #[test]
    fn a_thread_asking_for_an_entry_in_use_waits_its_turn() {
        let coordinator = Arc::new(Coordinator::default());
        let first = coordinator.take_turn(&key("tool"));
        let (entered, hear) = mpsc::channel();

        let second = Arc::clone(&coordinator);
        let waiter = thread::spawn(move || {
            let _turn = second.take_turn(&key("tool"));
            entered.send(()).expect("test is listening");
        });
        assert!(
            hear.recv_timeout(SHORT).is_err(),
            "the second thread got in while the first was still working"
        );

        drop(first);
        hear.recv_timeout(LONG)
            .expect("the second thread gets in once the first is done");
        waiter.join().expect("waiter finishes");
    }

    #[test]
    fn different_entries_do_not_wait_for_each_other() {
        let coordinator = Arc::new(Coordinator::default());
        let _first = coordinator.take_turn(&key("tool"));

        let second = Arc::clone(&coordinator);
        thread::spawn(move || drop(second.take_turn(&key("other"))))
            .join()
            .expect("another entry is free at once");
    }

    #[test]
    fn a_fill_that_fits_alone_but_not_beside_another_waits_for_it() {
        let coordinator = Arc::new(Coordinator::default());
        // 1000 free: the first needs 600, the second 500; both fit alone.
        let first = coordinator
            .promise(600, 600, || Some(1000))
            .expect("the first fits");
        let (settled, hear) = mpsc::channel();

        let second = Arc::clone(&coordinator);
        let waiter = thread::spawn(move || {
            let promise = second.promise(500, 500, || Some(1000));
            settled.send(promise.is_ok()).expect("test is listening");
        });
        assert!(
            hear.recv_timeout(SHORT).is_err(),
            "the second fill was told yes or no while the first held the space"
        );

        drop(first);
        assert!(
            hear.recv_timeout(LONG).expect("the second fill is settled"),
            "it fits once the first is done"
        );
        waiter.join().expect("waiter finishes");
    }

    #[test]
    fn a_fill_that_cannot_fit_even_alone_is_refused_at_once() {
        let coordinator = Arc::new(Coordinator::default());
        let _ahead = coordinator
            .promise(10, 10, || Some(100))
            .expect("the first fits");

        let refused = coordinator.promise(500, 500, || Some(100));

        assert_eq!(refused.err(), Some(100));
    }

    #[test]
    fn a_volume_that_cannot_be_measured_is_tried_anyway() {
        let coordinator = Arc::new(Coordinator::default());

        assert!(coordinator.promise(500, 500, || None).is_ok());
    }

    #[test]
    fn a_promise_is_given_back_when_it_is_dropped() {
        let coordinator = Arc::new(Coordinator::default());

        drop(coordinator.promise(600, 600, || Some(1000)).expect("fits"));

        assert!(
            coordinator.promise(900, 900, || Some(1000)).is_ok(),
            "the space the first promised is free again"
        );
    }
}
