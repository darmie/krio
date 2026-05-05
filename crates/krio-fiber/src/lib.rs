//! krio-fiber — Wren/Lua-style stackful fibers.
//!
//! First-class coroutine values backed by a per-fiber stack and a
//! target-specific context-switch primitive. Yield works from any
//! call depth because the suspension preserves the entire physical
//! stack — no compile-time transform involved.
//!
//! ## Usage
//!
//! ```no_run
//! use krio_fiber::{Fiber, FiberStep, yield_now};
//!
//! let mut fiber = Fiber::new(|| {
//!     println!("step 1");
//!     yield_now();
//!     println!("step 2");
//!     yield_now();
//!     println!("step 3");
//! });
//!
//! while let FiberStep::Yielded = fiber.resume() {
//!     // Each loop body runs between two yields.
//! }
//! ```
//!
//! ## Status
//!
//! - x86_64 + aarch64 context-switch implemented (System V / AAPCS64).
//! - Single-threaded; `Fiber` is `!Send`.
//! - Stack allocated as a heap `Box<[u8]>`. An `mmap`-backed variant
//!   with a guard page is a reasonable follow-up; the API stays the
//!   same.
//! - Bidirectional value passing (`call(input) -> Output` per Wren)
//!   not yet implemented — the current API is unit-typed yields. Add
//!   an extra typed channel if you need it; the underlying switch is
//!   already in place.
//!
//! ## Where this fits in the krio family
//!
//! `krio-fiber` is a runtime, not a transform — it shares the
//! `Marker` / `Suspension` vocabulary in `krio-core` but does not
//! depend on `krio-stackless`'s state-machine algorithm. A program
//! can mix stackless coroutines and stackful fibers freely; they
//! cost what their model says they cost.

mod arch;
mod fiber;

pub use fiber::{DEFAULT_STACK_SIZE, Fiber, FiberStep, yield_now};
