//! Parking an idle agent on shared memory.
//!
//! An earlier sketch of this design assumed parking would need a host
//! import alongside spawning. It does not: `memory.atomic.wait32` and
//! `memory.atomic.notify` are core wasm *instructions*, so an agent can
//! sleep and be woken without the host being involved at all. Only
//! creating an agent genuinely needs the host — see [`crate::agent`].
//!
//! ## The main thread cannot use this
//!
//! `memory.atomic.wait32` throws unconditionally on a browser's main
//! thread; the web platform will not let the UI thread block. That is
//! not a rule this crate can enforce with a type — the same
//! [`WasmPark`] value is legitimately shared by every agent in the
//! cluster — so it is enforced structurally instead: `Park::park` is
//! only ever called from `ParallelScheduler::run`, which is documented
//! worker-only, and the main agent drives the cluster through
//! `drive_once`, which never parks.
//!
//! The main agent still gets woken by the *same* `notify`. It waits on
//! the JS side with `Atomics.waitAsync` against the same address, which
//! is why the waker never has to know which kind of agent it is waking.

#[cfg(all(target_arch = "wasm32", target_feature = "atomics"))]
use core::sync::atomic::AtomicU32;
#[cfg(all(target_arch = "wasm32", target_feature = "atomics"))]
use krio_parallel::Park;

/// Blocks an agent on its parking slot using wasm's atomic wait.
///
/// Only usable where the `atomics` target feature is on. On a target
/// without it the type still exists — so documentation and signatures
/// resolve everywhere — but implements no `Park`, and a host should use
/// [`krio_parallel::SpinPark`] instead.
///
/// # Never on the browser main thread
/// See the module docs. `park` will throw there rather than block.
#[derive(Debug, Clone, Copy)]
pub struct WasmPark {
    /// Nanoseconds to wait before reporting a spurious wake; negative
    /// means wait indefinitely.
    ///
    /// Only read by the atomic-wait instruction, so on a target without
    /// it the field is genuinely unused rather than accidentally so.
    #[cfg_attr(
        not(all(target_arch = "wasm32", target_feature = "atomics")),
        allow(dead_code)
    )]
    timeout_ns: i64,
}

impl Default for WasmPark {
    fn default() -> Self {
        Self::new()
    }
}

impl WasmPark {
    /// Wait indefinitely.
    ///
    /// Safe as the default because shutdown notifies every agent, parked
    /// or not, so nothing relies on a timeout to make progress.
    pub const fn new() -> Self {
        Self { timeout_ns: -1 }
    }

    /// Wake spuriously after `ms` even if nobody notified.
    ///
    /// Costs a wasted queue scan per idle agent per interval, and buys
    /// tolerance of a host that drops a notification. Reach for it while
    /// bringing up a new harness, not in production.
    pub const fn with_timeout_ms(ms: u32) -> Self {
        Self {
            timeout_ns: (ms as i64) * 1_000_000,
        }
    }
}

#[cfg(all(target_arch = "wasm32", target_feature = "atomics"))]
impl Park for WasmPark {
    fn park(&self, slot: &AtomicU32, expected: u32) {
        // SAFETY: `slot` is a live `AtomicU32` in linear memory, which
        // is what the instruction requires. A mismatched value or an
        // expired timeout returns immediately rather than blocking, and
        // the caller re-checks its queues either way.
        unsafe {
            core::arch::wasm32::memory_atomic_wait32(
                slot.as_ptr() as *mut i32,
                expected as i32,
                self.timeout_ns,
            );
        }
    }

    fn unpark(&self, slot: &AtomicU32) {
        // Wake one waiter. The caller has already moved the slot out of
        // PARKED, so a waiter that has not yet slept sees the new value
        // and skips the wait entirely.
        //
        // SAFETY: as above — a live `AtomicU32` in linear memory.
        unsafe {
            core::arch::wasm32::memory_atomic_notify(slot.as_ptr() as *mut i32, 1);
        }
    }
}

#[cfg(not(all(target_arch = "wasm32", target_feature = "atomics")))]
impl WasmPark {
    /// Why this type has no `Park` impl on this target.
    ///
    /// Kept as a real function rather than a comment so the reason
    /// appears in the docs a host reads while wondering why their
    /// `Cluster::new` will not compile.
    pub const fn why_unavailable() -> &'static str {
        "krio-wasm: atomic parking needs the `atomics` target feature — build with \
         -Ctarget-feature=+atomics,+bulk-memory,+mutable-globals and -Zbuild-std, \
         or use krio_parallel::SpinPark"
    }
}
