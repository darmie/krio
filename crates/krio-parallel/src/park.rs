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

    /// Wake *every* agent waiting on `slot`.
    ///
    /// Needed by the safepoint barrier, where all agents wait on one
    /// address and all of them must go at once — waking them one at a
    /// time would serialise the resume and, worse, requires knowing how
    /// many there are.
    ///
    /// The default forwards to [`Park::unpark`], which is correct for
    /// any backend whose `park` re-checks the value it was given (a
    /// bounded spin, say). A backend that genuinely sleeps must override
    /// it or the barrier releases one agent and hangs the rest.
    fn unpark_all(&self, slot: &AtomicU32) {
        self.unpark(slot);
    }
}

/// Bounded spin. Correct anywhere, ideal nowhere.
///
/// The fallback when a target has no blocking primitive. It gives up
/// after a bounded number of iterations and reports a spurious wake
/// rather than spinning forever, so a lost notification costs a wasted
/// queue scan instead of a hung agent.
///
/// # It never yields the core
///
/// Nothing here hands time back to the OS — there is nothing in `core`
/// that can. So an idle agent on `SpinPark` burns a core rather than
/// releasing it, and a machine running more agents than it has cores
/// makes *worse* progress on the agents being waited for. A
/// stop-the-world under those conditions can appear to hang.
///
/// Use it when there is genuinely no alternative. A std host should
/// give the cluster a park that blocks or at least yields; a wasm host
/// should use `krio_wasm::WasmPark`, which sleeps properly on
/// `memory.atomic.wait32`.
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
