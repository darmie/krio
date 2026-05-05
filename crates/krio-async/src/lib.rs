//! krio-async — cross-function stackless coroutines (planned).
//!
//! Extends `krio-stackless` with function-colour propagation so
//! suspension can cross call boundaries: a caller that invokes a
//! suspending callee saves its own state, embeds the callee's state
//! machine as a field, and resumes both on the next poll. Locals
//! that live across a suspension are lifted from stack slots into
//! struct fields — the "borrow checker on yield" pass.
//!
//! ## Status: NOT YET IMPLEMENTED
//!
//! Targeted shape:
//!
//! - `SuspendingFns` trait the host implements: tells krio-async
//!   which callees may suspend.
//! - Captures lift: the host marks "alive across suspension" locals;
//!   krio-async generates a state struct holding them.
//! - Composition: at every suspending call site, the state struct
//!   gains a field for the callee's machine; the dispatch threads
//!   `Pending` returns through.
//!
//! Targeted programming model: comparable to Rust's `async fn` /
//! Kotlin's `suspend fun`. Stackless, zero per-coroutine heap alloc
//! for fixed-size state, function colour viral up the call graph.
//!
//! Until landed, this crate is empty.

#![no_std]
