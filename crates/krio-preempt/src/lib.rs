//! krio-preempt — preemptive scheduler over krio-fiber (planned).
//!
//! Extends `krio-fiber` with timer-driven context switching: instead
//! of waiting for the running fiber to yield, the scheduler is
//! invoked from a periodic interrupt and forcibly switches to the
//! next runnable fiber.
//!
//! ## Status: NOT YET IMPLEMENTED
//!
//! Targeted shape:
//!
//! - `Scheduler::new()` — set up the run-queue.
//! - `scheduler.spawn(fiber)` — enqueue a fiber for execution.
//! - `scheduler.run_forever()` — installs the timer interrupt
//!   handler and dispatches.
//! - Pluggable scheduling policy (round-robin / priority / fair).
//!
//! Targeted internals:
//!
//! - Timer interrupt handler: target-specific. Calls into the fiber's
//!   context-switch primitive with an arbitrary "current" pointer.
//! - Per-fiber state: runnable / blocked / done.
//! - Run queue: lock-free single-thread first; multi-core variant
//!   later.
//!
//! Until landed, this crate is empty.

#![no_std]
