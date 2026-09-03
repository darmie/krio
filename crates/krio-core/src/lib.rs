//! krio-core — shared vocabulary for the krio coroutine framework family.
//!
//! Every execution model in the krio family (stackless transform,
//! cross-function async, stackful fibers, preemptive scheduling) needs
//! to talk about the same things at some level: when does a coroutine
//! suspend, what makes a region, which schedule does it run under.
//!
//! `krio-core` is dependency-free and intentionally tiny. Its job is to
//! keep the family's vocabulary consistent so a downstream consumer
//! can mix execution models in one program and the types still line up.
//!
//! ## What lives here
//!
//! - [`Marker`] — the seven categories any IR's marker statements fall
//!   into. Used by the stackless transform; the cross-function variant
//!   extends it; the fiber runtime maps these to its own primitives.
//! - [`CfgId`] — blanket trait bound for opaque block / local IDs that
//!   plug into the trait surfaces.
//! - [`Suspension`] — a normalised report of "why did the coroutine
//!   stop running" that schedulers can act on uniformly.
//! - [`Task`] — "advance one step" interface that runtime variants
//!   (`krio-fiber`, future `krio-async`, host wrappers around
//!   `krio-stackless`) implement so a single scheduler can drive a
//!   heterogeneous mix.
//!
//! ## What does *not* live here
//!
//! - The state-machine transform → `krio-stackless`.
//! - The fiber runtime → `krio-fiber`.
//! - The cross-function async transform → `krio-async`.
//! - Scheduler implementations → `krio-runtime`.

#![no_std]

use core::fmt::Debug;
use core::hash::Hash;

/// Marker categories the algorithm cares about. The consumer's marker
/// classifier returns one of these for each statement that drives the
/// transform; everything else is a regular statement.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Marker {
    /// Open a concurrency region.
    RegionBegin,
    /// Close the most recent region.
    RegionEnd,
    /// Open a coroutine inside the current region.
    CoroutineBegin,
    /// Close the most recent coroutine.
    CoroutineEnd,
    /// Unconditional yield — always suspends.
    Yield,
    /// Guarded recv — suspends only when the channel is empty,
    /// otherwise consumes a value. The consumer emits the peek and
    /// owns the recv statement; the transform orchestrates control flow.
    GuardedRecv,
    /// Producing send — runs the send first, then yields once so any
    /// consumer gets a turn.
    ProducingSend,
}

/// Why a coroutine stopped running on this turn. Each variant exists
/// in every execution model in the family, even when the model
/// represents it differently at runtime (e.g. stackful fibers don't
/// need the discriminator for `Yield` vs `GuardedRecv` because the
/// suspension preserves the entire stack).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Suspension {
    /// Coroutine ran to completion and will not resume.
    Completed,
    /// Coroutine suspended at an unconditional yield.
    Yielded,
    /// Coroutine suspended waiting on a channel / future / event.
    Pending,
}

impl Suspension {
    /// Has the underlying coroutine finished — i.e. is it safe to
    /// drop without leaking work?
    pub fn is_done(self) -> bool {
        matches!(self, Suspension::Completed)
    }
}

/// A unit of work a krio scheduler can drive. Implemented by every
/// runtime in the family that produces something callable:
/// `krio-fiber::Fiber`, `krio-async`'s future-equivalent (when that
/// lands), and any host wrapper around `krio-stackless` output that
/// the host compiler synthesises.
///
/// `krio-stackless` itself does *not* implement `Task` — it's a
/// compile-time transform. The host wraps its output into a `Task`
/// via whatever IR-to-runtime bridge it has.
pub trait Task {
    /// Run one step. Returns:
    /// - [`Suspension::Yielded`] / [`Suspension::Pending`] if the
    ///   task is still alive.
    /// - [`Suspension::Completed`] when the task is finished.
    ///
    /// After `Completed` is returned, calling `step` again is a
    /// programming error (panic / abort / undefined behaviour at
    /// the implementer's discretion).
    fn step(&mut self) -> Suspension;
}

/// IDs the consumer uses to refer to blocks and locals in CFG-shaped
/// IRs. Treated as opaque handles by the framework, but `Ord` is
/// required because the stackless transform's suspension scan needs
/// to bound its search to a coroutine's block range. Every CFG IR
/// known to ship with stable, source-order block IDs (LLVM IR,
/// Cranelift IR, Rust MIR) satisfies this naturally.
pub trait CfgId: Copy + Eq + Ord + Hash + Debug {}
impl<T: Copy + Eq + Ord + Hash + Debug> CfgId for T {}

/// Monotonic millisecond source.
///
/// Deadline polling sits on the hot path — a cooperative coroutine is
/// expected to call `should_yield_early()` at every checkpoint — so an
/// implementation should be a *load*, not a syscall and certainly not a
/// foreign-function call. On a target with shared memory the intended
/// shape is a counter one agent bumps and every other agent reads
/// relaxed; on a native target `SystemTime` is fine.
///
/// The origin is unspecified. Only differences between two readings on
/// the same `Clock` are meaningful.
///
/// # Why this is vocabulary rather than a scheduler detail
/// Both `krio-fiber` (per-fiber deadlines) and `krio-preempt` (time
/// slices) need a clock, and a host mixing the two must be able to give
/// them the *same* one — otherwise a fiber's deadline and the slice that
/// set it are measured against different origins. That makes it shared
/// vocabulary, which is what this crate is for.
pub trait Clock {
    /// Milliseconds since this clock's own origin.
    fn now_ms(&self) -> f64;
}

/// Process-unique identity for a [`Task`], stable across a migration
/// between execution agents.
///
/// krio does not interpret the value. It exists so a host can key its
/// own state — a shadow call stack, a GC root table, a profiler span —
/// to a task it does not own and cannot look inside, and still follow
/// that task when a scheduler moves it to another agent.
///
/// Id `0` is reserved for "no task" / the scheduler's own context, so a
/// host can use it as a sentinel the way `krio-fiber` uses fiber id 0
/// for the scheduler context.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct TaskId(pub u64);

impl TaskId {
    /// The scheduler / host context — never a real task.
    pub const NONE: TaskId = TaskId(0);

    /// Is this a real task rather than the [`TaskId::NONE`] sentinel?
    pub fn is_task(self) -> bool {
        self.0 != 0
    }
}
