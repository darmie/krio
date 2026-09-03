//! Tasks that are waiting on something, and the wakes that bring them
//! back.
//!
//! [`krio_core::Suspension`] draws a distinction the first version of
//! this scheduler threw away: `Yielded` means *give someone else a turn*
//! and `Pending` means *I am waiting on a channel, a future, an event*.
//! Re-queueing both is correct in the sense that nothing is lost, and
//! wrong in the sense that an agent holding one `Pending` task spins a
//! core polling something that cannot possibly have changed.
//!
//! So a `Pending` task is moved out of the run queues entirely and held
//! here until someone calls [`crate::Cluster::wake`]. An agent whose
//! only remaining work is parked reports `Idle` and goes to sleep, which
//! is the whole point.
//!
//! ## Who wakes it
//!
//! Not krio. The family deliberately does not own channels — the
//! stackless transform emits the peek and lets the host own the recv —
//! so the host, which does own the channel, is what calls `wake` when
//! data arrives. It learns the [`TaskId`] from
//! [`crate::TaskObserver::on_step_begin`], which already carries it.
//!
//! Threading a waker through `Task::step` would be the other design, and
//! it would mean changing a trait that `RoundRobin` and `Fiber` also
//! implement, to add a parameter neither of them can use.
//!
//! ## The race, and why a wake is never lost
//!
//! A task returns `Pending` and the scheduler starts to park it. Before
//! the job reaches this map, the event fires on another agent and `wake`
//! runs — finds nothing — and the task would sleep forever.
//!
//! The fix is to remember the wake. `wake` on an unknown id records it;
//! [`Parked::park`] checks for a recorded wake before storing anything
//! and, if it finds one, hands the job straight back to run again. The
//! two operations take the same lock, so one of them always sees the
//! other.

use alloc::collections::{BTreeMap, BTreeSet};
use core::cell::UnsafeCell;
use core::hint;
use core::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

use krio_core::TaskId;

/// Waiting tasks, plus wakes that arrived before their task did.
pub(crate) struct Parked<T> {
    locked: AtomicBool,
    tasks: UnsafeCell<BTreeMap<u64, T>>,
    /// Ids woken while no task was parked under them. Consumed by the
    /// next [`Parked::park`] for that id.
    early_wakes: UnsafeCell<BTreeSet<u64>>,
    /// Mirror of `tasks.len()`, readable without taking the lock — the
    /// idle path checks it on every pass and must not serialise agents
    /// against each other to do so.
    count: AtomicUsize,
}

// SAFETY: both maps are only touched under `locked`.
unsafe impl<T: Send> Send for Parked<T> {}
unsafe impl<T: Send> Sync for Parked<T> {}

impl<T> Parked<T> {
    pub(crate) fn new() -> Self {
        Self {
            locked: AtomicBool::new(false),
            tasks: UnsafeCell::new(BTreeMap::new()),
            early_wakes: UnsafeCell::new(BTreeSet::new()),
            count: AtomicUsize::new(0),
        }
    }

    fn lock(&self) {
        while self
            .locked
            .compare_exchange_weak(false, true, Ordering::Acquire, Ordering::Relaxed)
            .is_err()
        {
            hint::spin_loop();
        }
    }

    fn unlock(&self) {
        self.locked.store(false, Ordering::Release);
    }

    /// Hold `task` until it is woken.
    ///
    /// Returns it straight back when a wake for `id` already arrived, in
    /// which case the caller must re-queue rather than park — that is
    /// the lost-wakeup race being closed.
    pub(crate) fn park(&self, id: TaskId, task: T) -> Option<T> {
        self.lock();
        let raced = unsafe { (*self.early_wakes.get()).remove(&id.0) };
        if raced {
            self.unlock();
            return Some(task);
        }
        unsafe { (*self.tasks.get()).insert(id.0, task) };
        self.count
            .store(unsafe { (*self.tasks.get()).len() }, Ordering::Release);
        self.unlock();
        None
    }

    /// Take the task waiting under `id`, if any.
    ///
    /// When nothing is parked the wake is recorded instead, so a task
    /// that is on its way here does not miss it.
    pub(crate) fn wake(&self, id: TaskId) -> Option<T> {
        self.lock();
        let task = unsafe { (*self.tasks.get()).remove(&id.0) };
        if task.is_none() {
            // Either the task is mid-park on another agent — this is the
            // race — or it has already completed, in which case the id
            // is never claimed and this entry stays. A host that only
            // wakes live tasks leaves nothing behind; one that wakes
            // completed tasks leaks a `u64` per distinct id.
            unsafe { (*self.early_wakes.get()).insert(id.0) };
        } else {
            self.count
                .store(unsafe { (*self.tasks.get()).len() }, Ordering::Release);
        }
        self.unlock();
        task
    }

    /// How many tasks are waiting. Lock-free, so the idle path can ask
    /// on every pass.
    pub(crate) fn len(&self) -> usize {
        self.count.load(Ordering::Acquire)
    }

    /// Number of recorded wakes still waiting to be claimed. Only used
    /// by tests, to assert the race bookkeeping does not accumulate in
    /// ordinary operation.
    #[cfg(test)]
    pub(crate) fn early_wake_count(&self) -> usize {
        self.lock();
        let n = unsafe { (*self.early_wakes.get()).len() };
        self.unlock();
        n
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn park_then_wake_returns_the_task() {
        let parked: Parked<u32> = Parked::new();
        assert!(parked.park(TaskId(7), 99).is_none());
        assert_eq!(parked.len(), 1);

        assert_eq!(parked.wake(TaskId(7)), Some(99));
        assert_eq!(parked.len(), 0);
        assert_eq!(parked.early_wake_count(), 0);
    }

    #[test]
    fn a_wake_that_arrives_first_is_not_lost() {
        let parked: Parked<u32> = Parked::new();

        // The event fires while the task is still on its way here.
        assert_eq!(parked.wake(TaskId(7)), None);
        assert_eq!(parked.early_wake_count(), 1);

        // Parking must then refuse to sleep and hand the task back.
        assert_eq!(
            parked.park(TaskId(7), 99),
            Some(99),
            "a task parked after its wake must run again, not sleep"
        );
        assert_eq!(parked.len(), 0);
        assert_eq!(parked.early_wake_count(), 0, "the wake should be consumed");
    }

    #[test]
    fn waking_the_wrong_id_leaves_others_alone() {
        let parked: Parked<u32> = Parked::new();
        parked.park(TaskId(1), 10);
        parked.park(TaskId(2), 20);

        assert_eq!(parked.wake(TaskId(2)), Some(20));
        assert_eq!(parked.len(), 1);
        assert_eq!(parked.wake(TaskId(1)), Some(10));
        assert_eq!(parked.len(), 0);
    }
}
