//! krio-fiber — stackful Wren/Lua-style fibers (planned).
//!
//! First-class coroutine values backed by a per-fiber stack and a
//! target-specific context-switch primitive. Yield works from any
//! call depth because the suspension preserves the entire stack —
//! no compile-time transform is involved.
//!
//! ## Status: NOT YET IMPLEMENTED
//!
//! Targeted surface (Wren-shaped):
//!
//! - `Fiber::new(closure) -> Fiber` — allocate a stack, install the
//!   closure as the entry point.
//! - `fiber.call(value) -> CallOutcome` — switch into the fiber,
//!   returning whatever it next yields or its final return value.
//! - `Fiber::yield_(value) -> ResumeValue` — from inside the fiber,
//!   suspend and hand `value` back to the caller's `.call(...)`.
//! - `fiber.is_done() -> bool`.
//!
//! Targeted internals:
//!
//! - Fixed-size stack pages (configurable, ~4-32 KB default).
//! - Context switch in target-specific asm: x86_64, aarch64, riscv64
//!   to start. Saves callee-saved registers + swaps the stack
//!   pointer; ~30-50 lines per target.
//! - `Fiber` is `!Send` by construction (single-thread cooperative).
//!   A separate variant could lift this for work-stealing later.
//!
//! Until landed, this crate is empty.

#![no_std]
