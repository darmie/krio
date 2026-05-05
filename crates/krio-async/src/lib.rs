//! krio-async — cross-function stackless coroutines.
//!
//! Extends [`krio-stackless`](../krio_stackless) with **function
//! colour propagation**: a yield can suspend the whole call stack
//! up to the nearest async boundary, not just the current
//! function. Locals that live across a suspension are lifted from
//! stack slots into a per-frame slot table on a runtime fiber-style
//! frame stack.
//!
//! This crate's design is a generalised port of the AOT state-
//! machine lowering in the `wren_lift` Wren JIT/AOT runtime
//! (`src/codegen/aot_state_machine.rs`). That codebase already
//! validates the shape against a real production language; krio-
//! async's job is to lift the algorithm out of Wren-specific
//! types so any host compiler can drive it.
//!
//! ## Status — Phase 1
//!
//! Phase 1 is the public **type contract**: hosts can wire their
//! IR types to the trait surfaces defined here, and the
//! [`transform_to_state_machine`] entry point compiles and returns
//! [`TransformError::Unimplemented`]. This is enough for a host to
//! start integrating without waiting on the body lowering.
//!
//! Phase 2 implements the direct-yield split (functions that call
//! a yield primitive but don't recurse through other suspending
//! callees) by reusing `krio-stackless`'s state-machine emission.
//!
//! Phase 3 adds the cross-function dispatch
//! ([`BlockKind::CrossFnCallInit`] / [`BlockKind::CrossFnCallResume`])
//! — the genuinely new mechanic over `krio-stackless`.
//!
//! ## How it relates to the family
//!
//! ```text
//! krio-stackless     — single-function state-machine transform
//!     ↑
//!     | (Phase 2 reuses the per-fn split)
//! krio-async         — cross-function state-machine + frame stack
//!     ↑
//!     | (host compilers consume both)
//! krio-fiber         — orthogonal: stackful runtime, different model
//! ```
//!
//! ## Function-colour propagation
//!
//! krio-async asks the host for the **transitive yield-reachable**
//! set: every function that may, directly or via a callee, end up
//! at a yield primitive. The host computes this once via taint
//! analysis over its call graph and exposes it through
//! [`SuspendingFns`].
//!
//! Functions outside that set lower to ordinary native code with
//! no transform. Functions inside it are split at every suspending
//! call site (direct yield or cross-fn call to another tainted
//! function) into a state machine.
//!
//! ## Runtime side
//!
//! Lowered async functions don't live on a real C stack — that
//! stack would have to unwind across a suspension and resume later,
//! which native code can't do. Instead the host keeps a per-fiber
//! stack of [`FrameState`]s: each frame records its current state
//! ID + a slot table for live-across-suspension values. A
//! suspending call pushes a fresh frame; Done pops it; Yield
//! leaves the stack alone so the next poll resumes the deepest
//! suspended frame.

#![no_std]

extern crate alloc;

use alloc::vec::Vec;
use core::fmt::Debug;
use core::hash::Hash;

pub use krio_core::{CfgId, Marker, Suspension};

/// Process-unique IDs the host uses to refer to functions / methods
/// in its program. Treated as opaque handles by krio-async.
pub trait FnId: Copy + Eq + Hash + Debug {}
impl<T: Copy + Eq + Hash + Debug> FnId for T {}

/// Host-implemented "may this function suspend?" oracle.
///
/// The host is expected to compute the transitive set in advance:
/// any function that calls a yield primitive directly *or via a
/// callee that itself calls one* should report
/// [`SuspendingFns::is_suspending`] = `true`. In wren_lift this is
/// `tainted_names: HashSet<String>`; in any host it's a fixpoint
/// over the call graph.
///
/// `is_yield_primitive` distinguishes the language's actual yield
/// builtin (e.g. `Fiber.yield(_)`, `await`, `suspend`) from "merely"
/// suspending callees. Direct-yield call sites and cross-fn call
/// sites lower differently.
pub trait SuspendingFns {
    type FnId: FnId;

    /// True if this function may suspend (directly or transitively).
    fn is_suspending(&self, fn_id: Self::FnId) -> bool;

    /// True if this function *is* a yield primitive — its call site
    /// gets the [`BlockKind::DirectYield`] lowering instead of the
    /// cross-fn dispatch.
    fn is_yield_primitive(&self, fn_id: Self::FnId) -> bool;
}

/// What kind of suspension a block represents. The block's final
/// terminator + any pre-Return helpers are emitted differently for
/// each kind.
///
/// - [`BlockKind::DirectYield`]: simple — set kind=Yield in the
///   frame state, return the yielded value.
/// - [`BlockKind::CrossFnCallInit`]: first half of a cross-fn call.
///   Pre-call setup (advance own state, push child frame, save
///   args), then jump to the matching `CrossFnCallResume`.
/// - [`BlockKind::CrossFnCallResume`]: synthetic block the
///   dispatcher's switch lands in on resume from a yielded child.
///   Invoke the child's poll fn, peek its kind: on Yield, propagate
///   up; on Done, pop the child frame and continue at `done_block`.
pub enum BlockKind<B: CfgId, V: CfgId, F: FnId> {
    DirectYield,
    CrossFnCallInit {
        /// MIR block-id of the matching `CrossFnCallResume`.
        resume_check_block: B,
        /// Receiver slot, if the call has one (method).
        receiver: Option<V>,
        /// Argument slots in source order.
        args: Vec<V>,
        /// Result slot (where the call's return value lands).
        result: V,
        /// Callee identifier.
        callee: F,
    },
    CrossFnCallResume {
        /// MIR block-id of the post-call block (where the original
        /// post-Call instructions live).
        done_block: B,
        receiver: Option<V>,
        args: Vec<V>,
        result: V,
        callee: F,
    },
}

/// Layout produced by [`transform_to_state_machine`]. Mirrors
/// wren_lift's `StateMachineLayout` so future ports of host-specific
/// lowering code are mechanical.
pub struct StateMachineLayout<B: CfgId, V: CfgId, F: FnId> {
    /// Per-state entry block. `resume_entries[0]` is the original
    /// MIR entry block. `resume_entries[i]` for `i > 0` is the
    /// block created by splitting at the `i`-th yield call.
    pub resume_entries: Vec<B>,
    /// Blocks whose final `Return(v)` should be lowered as a
    /// suspension: store `next_state` to the state struct, stamp
    /// kind=Yield, return v. Other returns stamp kind=Done.
    pub yield_blocks: Vec<(B, u32)>,
    /// Per yield block, the live-across values to save before the
    /// Return — `(slot_index, value_to_save)`. The host emits a
    /// `save_value(frame, slot, v)` call sequence in the body.
    pub yield_saves: Vec<(B, Vec<(u32, V)>)>,
    /// Per resume block, the loads to emit at the block's entry —
    /// `(slot_index, fresh_value_id_to_define)`. The transform has
    /// already rewritten downstream uses to point at the fresh id;
    /// the host just needs to call `load_value(frame, slot)` and
    /// store the result.
    pub resume_loads: Vec<(B, Vec<(u32, V)>)>,
    /// Per yielding block, the *kind* of suspension. Direct yield
    /// vs the two halves of a cross-fn call.
    pub block_kinds: Vec<(B, BlockKind<B, V, F>)>,
}

/// Runtime-side per-frame state. Equivalent to wren_lift's
/// `AotFrameState`. The host stores one of these per active call on
/// the fiber's frame stack.
///
/// `V` is whatever the host's runtime value representation is
/// (`wren_lift::Value`, `zura_runtime::Value`, etc.) — krio-async
/// stays agnostic.
pub struct FrameState<V> {
    /// Resume state ID. `0` = run from the entry block.
    pub state_id: u32,
    /// Live-across-suspension values + the initial-call args
    /// passed by the caller. The transform allocates slot indices;
    /// the runtime resizes on demand at first save.
    pub saved_values: Vec<V>,
}

impl<V> FrameState<V> {
    pub fn new() -> Self {
        Self {
            state_id: 0,
            saved_values: Vec::new(),
        }
    }
}

impl<V> Default for FrameState<V> {
    fn default() -> Self {
        Self::new()
    }
}

/// Errors the transform can refuse with. Matches wren_lift's "v1
/// caps" — features the simple split can't yet handle. Surface
/// these as hard errors so a host can't silently mis-compile.
#[derive(Debug)]
pub enum TransformError<B: CfgId, V: CfgId> {
    /// A value defined before a suspension is used after it.
    /// Implementing this requires lowering "save value to frame
    /// slot" + "load on resume"; planned for Phase 2.
    LiveValueAcrossSuspension { block: B, value: V },
    /// A suspension call landed inside a block that already has a
    /// non-Return terminator (e.g. inside a Branch). v1 only
    /// supports linear bodies; v2 will handle this.
    SuspensionInBranchedBlock { block: B },
    /// Phase 1: the transform body isn't implemented yet. Hosts
    /// integrating against the API will see this until Phase 2
    /// lands the lowering.
    Unimplemented,
}

/// Transform a function's CFG into a state-machine layout.
///
/// **Phase 1**: this is currently a stub. It returns
/// [`TransformError::Unimplemented`] for any non-trivial input. The
/// API contract is stable so a host can wire it in now and start
/// using direct-yield lowering when Phase 2 lands.
///
/// `fn_id` is the function being transformed; the transform asks
/// `suspending.is_suspending(fn_id)` to decide whether to emit a
/// state machine at all (a function not in the suspending set
/// lowers as ordinary code with no transform).
///
/// `inspect_call` is a host callback for inspecting Call statements
/// at given (block, statement) positions — Phase 3 needs it to
/// classify cross-fn calls. Phase 1 doesn't actually invoke it.
pub fn transform_to_state_machine<C, V, F, S>(
    _cfg: &mut C,
    _fn_id: F,
    suspending: &S,
) -> Result<StateMachineLayout<C, V, F>, TransformError<C, V>>
where
    C: CfgId,
    V: CfgId,
    F: FnId,
    S: SuspendingFns<FnId = F>,
{
    // Honour the contract for non-suspending fns — they don't need
    // a transform and Phase 1 can answer correctly: the trivial
    // (empty) layout is fine because the host won't emit any
    // suspension scaffolding for them.
    if !suspending.is_suspending(_fn_id) {
        return Ok(StateMachineLayout {
            resume_entries: Vec::new(),
            yield_blocks: Vec::new(),
            yield_saves: Vec::new(),
            resume_loads: Vec::new(),
            block_kinds: Vec::new(),
        });
    }
    Err(TransformError::Unimplemented)
}
