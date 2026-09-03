//! Cluster-wide overflow queue.
//!
//! Two jobs, both of them low-traffic by design:
//!
//! - where an agent without a deque of its own submits work (the main
//!   agent in a browser hands the cluster a task and returns to the
//!   event loop);
//! - where a full [`crate::deque::Deque`] spills, which is what lets the
//!   deques stay fixed-size.
//!
//! A spin lock is the right shape here rather than a lock-free MPMC
//! queue: the critical section is a `VecDeque` push or pop, contention
//! is rare because the fast path never touches this, and — the deciding
//! constraint — a browser's main thread may not block on
//! `memory.atomic.wait32`. A brief spin is not blocking; a futex wait
//! would throw.

use alloc::collections::VecDeque;
use core::cell::UnsafeCell;
use core::hint;
use core::sync::atomic::{AtomicBool, Ordering};

pub(crate) struct Injector<T> {
    locked: AtomicBool,
    queue: UnsafeCell<VecDeque<T>>,
}

// SAFETY: `queue` is only ever touched while `locked` is held.
unsafe impl<T: Send> Send for Injector<T> {}
unsafe impl<T: Send> Sync for Injector<T> {}

impl<T> Injector<T> {
    pub(crate) fn new() -> Self {
        Self {
            locked: AtomicBool::new(false),
            queue: UnsafeCell::new(VecDeque::new()),
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

    pub(crate) fn push(&self, value: T) {
        self.lock();
        unsafe { (*self.queue.get()).push_back(value) };
        self.unlock();
    }

    pub(crate) fn pop(&self) -> Option<T> {
        self.lock();
        let value = unsafe { (*self.queue.get()).pop_front() };
        self.unlock();
        value
    }

    pub(crate) fn is_empty(&self) -> bool {
        self.lock();
        let empty = unsafe { (*self.queue.get()).is_empty() };
        self.unlock();
        empty
    }
}
