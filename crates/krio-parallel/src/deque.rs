//! Bounded Chase–Lev work-stealing deque.
//!
//! One owner pushes and pops the *bottom*; any number of thieves steal
//! from the *top*. The owner's fast path touches no CAS at all, which is
//! the entire point — an agent running its own work should not pay for
//! the possibility of being stolen from.
//!
//! ## Why bounded
//!
//! The classic deque grows its buffer, and growth is the hard part: a
//! thief may still be reading the old buffer, so the old buffer cannot
//! be freed without epoch reclamation or a GC. A fixed capacity removes
//! that problem entirely, and the scheduler already has somewhere to put
//! the overflow — [`crate::injector::Injector`]. A full deque is a
//! scheduling event, not an error.
//!
//! ## Slot ownership
//!
//! Slots are `MaybeUninit`, and a slot's contents are owned by whichever
//! side wins the index, never by the buffer. A thief reads the value
//! *before* the CAS that claims it, so on a lost race it holds a bitwise
//! duplicate of a value someone else owns and must forget it rather than
//! drop it. `MaybeUninit` is what keeps the buffer's own drop glue from
//! touching those stale bits.

use alloc::boxed::Box;
use alloc::vec::Vec;
use core::cell::UnsafeCell;
use core::mem;
use core::mem::MaybeUninit;
use core::sync::atomic::{AtomicIsize, Ordering, fence};

pub(crate) struct Deque<T> {
    /// Next index a thief will take. Only ever increases.
    top: AtomicIsize,
    /// One past the last index the owner pushed.
    bottom: AtomicIsize,
    /// `capacity - 1`; capacity is always a power of two.
    mask: isize,
    buf: Box<[UnsafeCell<MaybeUninit<T>>]>,
}

// SAFETY: every access is arbitrated by `top`/`bottom`. A value is only
// read by the side that wins the index, so `T: Send` is sufficient — the
// deque never hands out a reference, only ownership.
unsafe impl<T: Send> Send for Deque<T> {}
unsafe impl<T: Send> Sync for Deque<T> {}

impl<T> Deque<T> {
    pub(crate) fn with_capacity(cap: usize) -> Self {
        let cap = cap.next_power_of_two().max(2);
        let mut buf = Vec::with_capacity(cap);
        for _ in 0..cap {
            buf.push(UnsafeCell::new(MaybeUninit::uninit()));
        }
        Self {
            top: AtomicIsize::new(0),
            bottom: AtomicIsize::new(0),
            mask: (cap - 1) as isize,
            buf: buf.into_boxed_slice(),
        }
    }

    pub(crate) fn capacity(&self) -> usize {
        self.buf.len()
    }

    /// Approximate number of queued items. Racy by nature; used for
    /// "is there surplus worth waking a thief for", never for control
    /// flow that must be exact.
    pub(crate) fn len(&self) -> usize {
        let b = self.bottom.load(Ordering::Relaxed);
        let t = self.top.load(Ordering::Relaxed);
        (b - t).max(0) as usize
    }

    pub(crate) fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Push onto the bottom.
    ///
    /// Returns the value back as `Err` when the deque is full, so the
    /// caller can spill it to the injector.
    ///
    /// # Safety
    /// Owner only — exactly one agent may call `push`/`pop` on a given
    /// deque.
    pub(crate) unsafe fn push(&self, value: T) -> Result<(), T> {
        let b = self.bottom.load(Ordering::Relaxed);
        let t = self.top.load(Ordering::Acquire);

        if b - t >= self.capacity() as isize {
            return Err(value);
        }

        // `MaybeUninit::write` does not drop what was there, which is
        // required: the slot may hold the bits of a value a thief owns.
        unsafe { (*self.buf[(b & self.mask) as usize].get()).write(value) };

        // Publish the value before publishing the index that exposes it.
        fence(Ordering::Release);
        self.bottom.store(b + 1, Ordering::Relaxed);
        Ok(())
    }

    /// Pop from the bottom (LIFO — the owner's most recent work, which
    /// is the warmest in cache).
    ///
    /// # Safety
    /// Owner only.
    pub(crate) unsafe fn pop(&self) -> Option<T> {
        let b = self.bottom.load(Ordering::Relaxed) - 1;
        self.bottom.store(b, Ordering::Relaxed);

        // Claim the slot before reading `top`, and stop the two from
        // being reordered: this is the fence the algorithm turns on.
        fence(Ordering::SeqCst);

        let t = self.top.load(Ordering::Relaxed);

        if t > b {
            // Empty. Restore bottom to where it was.
            self.bottom.store(b + 1, Ordering::Relaxed);
            return None;
        }

        if t == b {
            // Exactly one item, and a thief may be reaching for it.
            let won = self
                .top
                .compare_exchange(t, t + 1, Ordering::SeqCst, Ordering::Relaxed)
                .is_ok();
            self.bottom.store(b + 1, Ordering::Relaxed);
            return if won {
                Some(unsafe { (*self.buf[(b & self.mask) as usize].get()).assume_init_read() })
            } else {
                None
            };
        }

        // More than one item: no thief can be at `b`, so no CAS needed.
        Some(unsafe { (*self.buf[(b & self.mask) as usize].get()).assume_init_read() })
    }

    /// Steal from the top. Any agent except the owner.
    pub(crate) fn steal(&self) -> Option<T> {
        let t = self.top.load(Ordering::Acquire);
        fence(Ordering::SeqCst);
        let b = self.bottom.load(Ordering::Acquire);

        if t >= b {
            return None;
        }

        // Read before claiming. Safe against a concurrent `push`
        // because push writes at index `b`, and `b - t < capacity`
        // guarantees `b` and `t` are different slots.
        let value = unsafe { (*self.buf[(t & self.mask) as usize].get()).assume_init_read() };

        if self
            .top
            .compare_exchange(t, t + 1, Ordering::SeqCst, Ordering::Relaxed)
            .is_ok()
        {
            Some(value)
        } else {
            // Lost the race: `value` is a bitwise duplicate of something
            // another agent now owns. Dropping it would be a double free.
            mem::forget(value);
            None
        }
    }
}

impl<T> Drop for Deque<T> {
    fn drop(&mut self) {
        // Only `[top, bottom)` is live; everything else is uninitialised
        // or a stale duplicate. `MaybeUninit` means the buffer itself
        // has no drop glue to get this wrong.
        let b = *self.bottom.get_mut();
        let t = *self.top.get_mut();
        let mut i = t;
        while i < b {
            unsafe { (*self.buf[(i & self.mask) as usize].get()).assume_init_drop() };
            i += 1;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashSet;
    use std::sync::Arc;
    use std::vec::Vec as StdVec;
    use std::{sync::atomic::AtomicBool as StdAtomicBool, thread};

    #[test]
    fn push_pop_is_lifo() {
        let d: Deque<u64> = Deque::with_capacity(8);
        unsafe {
            d.push(1).unwrap();
            d.push(2).unwrap();
            d.push(3).unwrap();
            assert_eq!(d.pop(), Some(3));
            assert_eq!(d.pop(), Some(2));
            assert_eq!(d.pop(), Some(1));
            assert_eq!(d.pop(), None);
        }
    }

    #[test]
    fn steal_is_fifo() {
        let d: Deque<u64> = Deque::with_capacity(8);
        unsafe {
            d.push(1).unwrap();
            d.push(2).unwrap();
            d.push(3).unwrap();
        }
        assert_eq!(d.steal(), Some(1));
        assert_eq!(d.steal(), Some(2));
        assert_eq!(d.steal(), Some(3));
        assert_eq!(d.steal(), None);
    }

    #[test]
    fn full_deque_hands_the_value_back() {
        let d: Deque<u64> = Deque::with_capacity(2);
        unsafe {
            assert!(d.push(1).is_ok());
            assert!(d.push(2).is_ok());
            // Third push has nowhere to go and must return the value so
            // the scheduler can spill it rather than lose it.
            assert_eq!(d.push(99), Err(99));
        }
        assert_eq!(d.capacity(), 2);
    }

    #[test]
    fn drops_only_live_slots() {
        // A stolen value must not be dropped again when the deque dies.
        let d: Deque<std::boxed::Box<u64>> = Deque::with_capacity(4);
        unsafe {
            d.push(std::boxed::Box::new(1)).unwrap();
            d.push(std::boxed::Box::new(2)).unwrap();
        }
        let stolen = d.steal().unwrap();
        assert_eq!(*stolen, 1);
        drop(stolen);
        // Dropping the deque here must free only the remaining item.
        drop(d);
    }

    /// One owner, three thieves, boxed values so miscounting shows up as
    /// a leak or a double free under the test allocator.
    #[test]
    fn concurrent_steal_loses_nothing_and_duplicates_nothing() {
        const N: u64 = 20_000;
        let d: Arc<Deque<std::boxed::Box<u64>>> = Arc::new(Deque::with_capacity(64));
        let done = Arc::new(StdAtomicBool::new(false));

        let thieves: StdVec<_> = (0..3)
            .map(|_| {
                let d = Arc::clone(&d);
                let done = Arc::clone(&done);
                thread::spawn(move || {
                    let mut got = StdVec::new();
                    while !done.load(Ordering::Acquire) {
                        while let Some(v) = d.steal() {
                            got.push(*v);
                        }
                        std::thread::yield_now();
                    }
                    // Drain whatever is left after the owner stopped.
                    while let Some(v) = d.steal() {
                        got.push(*v);
                    }
                    got
                })
            })
            .collect();

        let mut owner_got = StdVec::new();
        let mut next = 0u64;
        while next < N {
            // SAFETY: this thread is the sole owner.
            match unsafe { d.push(std::boxed::Box::new(next)) } {
                Ok(()) => next += 1,
                Err(_) => {
                    // Full: take one back ourselves and try again.
                    if let Some(v) = unsafe { d.pop() } {
                        owner_got.push(*v);
                    }
                }
            }
            if next.is_multiple_of(7) {
                if let Some(v) = unsafe { d.pop() } {
                    owner_got.push(*v);
                }
            }
        }
        while let Some(v) = unsafe { d.pop() } {
            owner_got.push(*v);
        }
        done.store(true, Ordering::Release);

        let mut all = owner_got;
        for t in thieves {
            all.extend(t.join().unwrap());
        }

        let unique: HashSet<u64> = all.iter().copied().collect();
        assert_eq!(
            all.len(),
            unique.len(),
            "a value came out twice — the steal/pop race is wrong"
        );
        assert_eq!(unique.len(), N as usize, "values went missing");
        assert_eq!(*unique.iter().max().unwrap(), N - 1);
    }
}
