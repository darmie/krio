//! The cluster's clock, kept in shared memory.
//!
//! `SystemTime::now()` traps on `wasm32-unknown-unknown` — *time not
//! implemented on this platform* — so a wasm host has to supply one. The
//! obvious repair is to import `performance.now()` and call it, and for
//! krio that is the wrong shape: the call site is
//! `krio_fiber::should_yield_early()`, which a well-behaved coroutine
//! polls at every checkpoint. A JS boundary crossing per poll costs more
//! than the work being scheduled.
//!
//! So the clock lives *in* linear memory instead. One ticker writes
//! milliseconds into [`EPOCH_MS`]; every agent reads it relaxed. A
//! deadline check becomes a single load — no syscall, no import, and
//! nothing to trap.
//!
//! ## Resolution is the trade
//!
//! The epoch is only as fine as whatever advances it. A `setInterval` on
//! the main thread lands somewhere around 4 ms; a dedicated ticker agent
//! can do much better. Either way a slice budget below a few
//! milliseconds stops meaning anything, which is the right trade for
//! time slicing — nobody schedules a 200 µs slice through a cooperative
//! poll.
//!
//! ## Why a static rather than a field
//!
//! Two reasons, both structural. A `static` lives in the data segment,
//! which *is* the shared memory, so every agent reads the same address
//! without anyone passing a pointer around. And the fiber clock hook is
//! a bare `fn() -> f64` that cannot capture, so the counter it reads has
//! to be reachable without a receiver.
//!
//! This depends on workers not re-initialising memory when they
//! instantiate the module — the standard shared-memory linking mode
//! makes data segments passive so only the bootstrapping agent runs
//! them. A worker that re-runs data init would reset the clock to zero
//! under everyone.

use core::sync::atomic::{AtomicU64, Ordering};

use krio_core::Clock;

/// Milliseconds since the cluster's own origin. Written by the ticker,
/// read by everyone.
static EPOCH_MS: AtomicU64 = AtomicU64::new(0);

/// Reads the shared epoch counter.
///
/// Zero-sized: the state is the static, not the instance, so handing one
/// of these to a `Cluster` costs nothing and every copy agrees.
#[derive(Debug, Clone, Copy, Default)]
pub struct EpochClock;

impl EpochClock {
    pub const fn new() -> Self {
        Self
    }
}

impl Clock for EpochClock {
    #[inline]
    fn now_ms(&self) -> f64 {
        EPOCH_MS.load(Ordering::Relaxed) as f64
    }
}

/// Publish a new reading, from the host's real time source.
///
/// Uses `fetch_max`, so time cannot run backwards even if two tickers
/// race or a reading arrives late. A stale write is simply ignored,
/// which matters because a deadline that moves backwards turns into a
/// fiber that never yields.
///
/// Returns the epoch after the write.
pub fn publish_epoch_ms(ms: u64) -> u64 {
    let previous = EPOCH_MS.fetch_max(ms, Ordering::Relaxed);
    previous.max(ms)
}

/// Advance the epoch by `delta` milliseconds.
///
/// For a ticker that knows its own interval and has no absolute time
/// source of its own. Returns the epoch after the bump.
pub fn advance_epoch_ms(delta: u64) -> u64 {
    EPOCH_MS.fetch_add(delta, Ordering::Relaxed) + delta
}

/// Current epoch reading.
pub fn epoch_ms() -> u64 {
    EPOCH_MS.load(Ordering::Relaxed)
}

/// Point `krio-fiber`'s deadline clock at the epoch.
///
/// After this, a fiber's `is_deadline_passed()` and a `krio-preempt`
/// slice both read the same counter, which is the whole reason the two
/// crates share one clock rather than each holding their own: a deadline
/// computed against one origin and compared against another is either
/// always passed or never.
///
/// Call once per agent — the hook lives in linear memory and is shared,
/// so calling it again is harmless.
#[cfg(feature = "fiber-clock")]
pub fn install_fiber_clock() {
    krio_fiber::set_clock(|| EPOCH_MS.load(Ordering::Relaxed) as f64);
}
