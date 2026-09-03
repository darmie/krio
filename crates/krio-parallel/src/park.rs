//! How an idle agent waits, and how it gets woken.
//!
//! This is the one place the design is genuinely target-specific, so it
//! is the one place that is a trait. A native host parks on a futex or a
//! condvar; a Web Worker parks on `memory.atomic.wait32`; a browser's
//! main thread cannot park at all and never calls this — it returns to
//! the event loop and re-enters through `drive_once` instead.
//!
//! The waker never branches on which of those it is waking. It stores
//! [`NOTIFIED`] and calls [`Park::unpark`]; whether that resolves a
//! futex wait or an `Atomics.waitAsync` promise is the backend's
//! business.

use core::hint;
use core::sync::atomic::{AtomicU32, Ordering};

/// Agent is running, or about to look for work.
pub const RUNNING: u32 = 0;
/// Agent has published its intent to sleep and may be waiting.
pub const PARKED: u32 = 1;
/// Someone handed this agent work while it was parked.
pub const NOTIFIED: u32 = 2;

/// Blocking strategy for an idle agent.
pub trait Park: Sync {
    /// Wait until `slot` holds something other than `expected`.
    ///
    /// **May return spuriously.** Callers re-check their queues and the
    /// shutdown flag after every return, so a backend is free to wake
    /// early, time out, or not sleep at all.
    fn park(&self, slot: &AtomicU32, expected: u32);

    /// Wake an agent waiting on `slot`.
    ///
    /// The caller has already moved the slot out of [`PARKED`]; this is
    /// only the platform notification.
    fn unpark(&self, slot: &AtomicU32);
}

/// Bounded spin. Correct anywhere, ideal nowhere.
///
/// The fallback when a target has no blocking primitive, and what the
/// test suite runs on. It gives up after a bounded number of iterations
/// and reports a spurious wake rather than spinning forever, so a lost
/// notification costs a wasted queue scan instead of a hung agent.
pub struct SpinPark {
    /// Iterations before reporting a spurious wake.
    pub spins: u32,
}

impl Default for SpinPark {
    fn default() -> Self {
        Self { spins: 4096 }
    }
}

impl Park for SpinPark {
    fn park(&self, slot: &AtomicU32, expected: u32) {
        for _ in 0..self.spins {
            if slot.load(Ordering::Acquire) != expected {
                return;
            }
            hint::spin_loop();
        }
    }

    fn unpark(&self, _slot: &AtomicU32) {
        // The state store the caller already made is the whole signal.
    }
}
