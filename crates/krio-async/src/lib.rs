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

use alloc::vec;
use alloc::vec::Vec;
use core::fmt::Debug;
use core::hash::Hash;

pub use krio_core::{CfgId, Marker, Suspension};
pub use krio_stackless::CoroCfg;

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

/// One suspension site identified by [`AsyncHooks::classify`].
/// Either a direct yield-primitive call or a cross-function call
/// to another tainted callee.
pub enum SuspensionSite<F: FnId, V: CfgId> {
    /// Direct call to a yield primitive (`Fiber.yield(_)`,
    /// `await`, `suspend`). The yielded value (if any) is the
    /// host-tracked slot — krio-async records it but doesn't emit
    /// the actual Return; the host's lowering does.
    DirectYield {
        /// Slot holding the yielded value, if the language passes
        /// one. `None` for yield-without-value primitives.
        value: Option<V>,
    },
    /// Call to a function the host has flagged as suspending. The
    /// transform splits at this point so the dispatcher can resume
    /// after the callee finishes.
    CrossFnCall {
        callee: F,
        receiver: Option<V>,
        args: Vec<V>,
        result: V,
    },
}

/// Host-implemented IR hooks the transform needs to inspect the CFG.
/// `AsyncHooks` complements [`SuspendingFns`]: the latter answers
/// "may this *function* suspend"; this trait answers "is this
/// *statement* a suspension point, and what kind".
pub trait AsyncHooks {
    type Cfg: krio_stackless::CoroCfg;
    type FnId: FnId;

    /// Classify the statement at `(bb, idx)` as a suspension site
    /// or `None` if it isn't one.
    ///
    /// The transform invokes this once per statement during the
    /// scan phase. Implementations should be cheap — typically
    /// just match the statement's discriminant against "is this a
    /// Call, and is the callee in the suspending set".
    fn classify(
        &self,
        cfg: &Self::Cfg,
        bb: <Self::Cfg as krio_stackless::CoroCfg>::BlockId,
        idx: usize,
    ) -> Option<SuspensionSite<Self::FnId, <Self::Cfg as krio_stackless::CoroCfg>::LocalId>>;
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
/// **Phase 2 v1** — direct yields only:
///
/// - Refuses cross-function calls to suspending callees with
///   [`TransformError::Unimplemented`] (Phase 3 covers them).
/// - Refuses values that live across a suspension with
///   [`TransformError::LiveValueAcrossSuspension`] — Phase 2 v2
///   adds the captures-to-fields lift. v1 expects every value
///   used after a yield to be (re)defined after the yield too.
/// - Refuses suspensions that aren't the last statement of their
///   block with [`TransformError::SuspensionInBranchedBlock`] —
///   v1 only handles the linear case where the block ends with
///   the yield. v2 will split mid-block.
///
/// The transform mutates `cfg` by splitting each yield's block
/// after the yield's index — the new (initially empty) block
/// becomes the resume entry for that state. The original block
/// keeps the yield as its tail, ready for the host's lowering to
/// rewrite the terminator as "save state, kind=Yield, return".
///
/// On success, the returned [`StateMachineLayout`] tells the host:
/// - `resume_entries[0]` = the original entry block (state 0)
/// - `resume_entries[i]` = the block to enter at state `i`
/// - `yield_blocks[i]` = `(block, next_state)` — block whose
///   Return should advance to `next_state` and stamp kind=Yield
/// - `block_kinds[i]` = `(block, BlockKind)` — the lowering shape
///
/// The host emits the dispatcher prologue itself (it knows how to
/// load `state_id` from its runtime — typically a fiber-frame helper
/// call krio-async deliberately doesn't model).
pub fn transform_to_state_machine<S, H>(
    cfg: &mut H::Cfg,
    fn_id: H::FnId,
    suspending: &S,
    hooks: &H,
) -> Result<
    StateMachineLayout<
        <H::Cfg as krio_stackless::CoroCfg>::BlockId,
        <H::Cfg as krio_stackless::CoroCfg>::LocalId,
        H::FnId,
    >,
    TransformError<
        <H::Cfg as krio_stackless::CoroCfg>::BlockId,
        <H::Cfg as krio_stackless::CoroCfg>::LocalId,
    >,
>
where
    S: SuspendingFns<FnId = H::FnId>,
    H: AsyncHooks,
{
    if !suspending.is_suspending(fn_id) {
        return Ok(StateMachineLayout {
            resume_entries: Vec::new(),
            yield_blocks: Vec::new(),
            yield_saves: Vec::new(),
            resume_loads: Vec::new(),
            block_kinds: Vec::new(),
        });
    }

    // Snapshot block IDs before splitting — `cfg.block_ids()` would
    // grow as we split, but we only want to scan the original
    // function body, not the new resume blocks.
    let original_blocks: Vec<_> = cfg.block_ids();
    let entry_block = *original_blocks
        .first()
        .expect("krio-async: cfg has no blocks");

    // Pass 1 — scan + classify + validate.
    // Vec of (block, idx_at_scan_time, site, count_at_scan_time).
    let mut sites = Vec::new();
    for &bb in &original_blocks {
        let count = cfg.statement_count(bb);
        for idx in 0..count {
            if let Some(site) = hooks.classify(cfg, bb, idx) {
                // v1 cap: must be the last statement in the block.
                // Lifts in v2 (split mid-block, replicate post-yield
                // tail across control-flow successors).
                if idx + 1 != count {
                    return Err(TransformError::SuspensionInBranchedBlock { block: bb });
                }
                // v1 cap: cross-fn calls are Phase 3.
                if matches!(site, SuspensionSite::CrossFnCall { .. }) {
                    return Err(TransformError::Unimplemented);
                }
                sites.push((bb, idx, site));
            }
        }
    }

    // Pass 2 — split each yield's block. Since v1 caps require one
    // suspension per block at the tail, every split is independent
    // — order doesn't matter and indices stay valid.
    let mut resume_entries = vec![entry_block];
    let mut yield_blocks = Vec::new();
    let mut block_kinds = Vec::new();

    for (bb, idx, _site) in sites {
        // split_after at the last index produces an empty new block
        // that inherits bb's terminator. The new block is the resume
        // entry; bb keeps the yield as its tail.
        let resume = cfg.split_after(bb, idx);
        let next_state = resume_entries.len() as u32;
        resume_entries.push(resume);
        yield_blocks.push((bb, next_state));
        block_kinds.push((bb, BlockKind::DirectYield));
    }

    Ok(StateMachineLayout {
        resume_entries,
        yield_blocks,
        // v2 will populate these.
        yield_saves: Vec::new(),
        resume_loads: Vec::new(),
        block_kinds,
    })
}
