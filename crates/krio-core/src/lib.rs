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
//!
//! ## What does *not* live here
//!
//! - The state-machine transform → `krio-stackless`.
//! - The fiber runtime → `krio-fiber`.
//! - The cross-function async transform → `krio-async`.
//! - Schedulers / executors → each variant ships its own.

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

/// IDs the consumer uses to refer to blocks and locals in CFG-shaped
/// IRs. Treated as opaque handles by the framework, but `Ord` is
/// required because the stackless transform's suspension scan needs
/// to bound its search to a coroutine's block range. Every CFG IR
/// known to ship with stable, source-order block IDs (LLVM IR,
/// Cranelift IR, Rust MIR) satisfies this naturally.
pub trait CfgId: Copy + Eq + Ord + Hash + Debug {}
impl<T: Copy + Eq + Ord + Hash + Debug> CfgId for T {}
