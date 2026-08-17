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
//! - Context switch on `x86_64` (System V + MS x64) and on `aarch64`
//!   for non-Windows targets (AAPCS64). Callee-saved GP registers,
//!   callee-saved FP registers, and the callee-saved FP control word
//!   (MXCSR + x87 CW on x86_64, `FPCR` on aarch64) all ride the saved
//!   frame, so a fiber cannot leak its rounding mode into its host.
//! - `aarch64-pc-windows-*` is **not** supported: it needs the TEB
//!   stack-bounds swap the MS x64 path does (without it SEH cannot
//!   unwind out of a fiber) plus a guard-page allocator, so it takes
//!   the panic-stub path instead of silently mis-switching.
//! - Single-threaded; `Fiber` is `!Send`.
//! - **Unix**: stacks allocated via `mmap` with a `PROT_NONE` guard
//!   page below the usable region — stack overflow traps with
//!   SIGSEGV instead of silently corrupting unrelated heap data.
//! - **Other targets**: heap-allocated `Box<[u8]>` (no guard page).
//! - Bidirectional value passing via [`Fiber::resume_with`] /
//!   [`Fiber::take_yield_value`] / [`yield_value`] / [`take_input`].
//!   Untyped channel — values are `Box<dyn Any>`; downcast to
//!   recover the type.
//! - Lifecycle: [`FiberState::New`] / `Suspended` / `Done` /
//!   `Errored`. Panics inside the fiber are caught at the trampoline
//!   boundary, parked on the fiber, and surfaced via
//!   [`FiberStep::Errored`] + [`Fiber::take_error`].
//! - Cooperative cancellation: [`Fiber::cancel`] sets a flag the
//!   fiber polls via [`is_cancelled`] / [`should_yield_early`].
//! - Optional absolute deadlines: [`Fiber::set_deadline_ms`] +
//!   [`is_deadline_passed`].
//! - Nested fibers work — a fiber can drive another fiber from
//!   inside its own body; yields nest through the caller chain
//!   automatically via [`Fiber::resume`]'s prev_active save/restore.
//!
//! ## Not yet implemented
//!
//! - Symmetric `transfer` semantics (abandon the current fiber's
//!   continuation, switch to a peer with the original caller). The
//!   nested-call form covers most use cases.
//! - Targets beyond x86_64 + aarch64-non-Windows. ARM64 Windows is
//!   the closest: it reaches the TEB through `x18` (which the AAPCS64
//!   switch never touches, since Windows reserves it) rather than the
//!   `gs:`-relative moves x64 uses.
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
mod stack;

pub use fiber::{
    DEFAULT_STACK_SIZE, Fiber, FiberState, FiberStep, current_fiber_id, is_cancelled,
    is_deadline_passed, should_yield_early, take_input, take_input_u64, yield_now, yield_u64,
    yield_value,
};
