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
#[derive(Debug)]
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

/// Live-across-suspension data the host hands to the transform.
///
/// For each suspension site `(block, statement_idx)`, the host
/// records the values that are defined before the suspension and
/// used after it. The transform turns these into save/load slot
/// tables in the returned [`StateMachineLayout`].
///
/// The host computes liveness with whatever dataflow framework its
/// IR uses; krio-async deliberately doesn't model it because:
///
/// - liveness depends on how the host's IR represents def/use
///   chains (SSA vs explicit locals, single-assignment vs reassign,
///   ownership flowing through Drop checks, etc.);
/// - serious compilers already have liveness; re-implementing
///   inside the library would either duplicate or be too coarse;
/// - the host knows value types, which a library can't.
///
/// An empty `at_site` is fine — krio-async treats it as "no
/// captures." If the host's liveness is wrong (under-reports),
/// downstream code reads stale state; that's a host-side bug, not
/// a library check.
pub struct LivenessMap<B: CfgId, V: CfgId> {
    pub at_site: Vec<((B, usize), Vec<V>)>,
}

impl<B: CfgId, V: CfgId> LivenessMap<B, V> {
    pub fn new() -> Self {
        Self {
            at_site: Vec::new(),
        }
    }

    /// Record that `values` are live across the suspension at
    /// `(block, idx)`. Used by hosts building a `LivenessMap`
    /// procedurally.
    pub fn record(&mut self, block: B, idx: usize, values: Vec<V>) {
        self.at_site.push(((block, idx), values));
    }

    fn lookup(&self, block: B, idx: usize) -> &[V] {
        self.at_site
            .iter()
            .find(|((b, i), _)| *b == block && *i == idx)
            .map(|(_, v)| v.as_slice())
            .unwrap_or(&[])
    }
}

impl<B: CfgId, V: CfgId> Default for LivenessMap<B, V> {
    fn default() -> Self {
        Self::new()
    }
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

/// Errors the transform can refuse with. Surface these as hard
/// errors so a host can't silently mis-compile.
#[derive(Debug)]
pub enum TransformError<B: CfgId> {
    /// A suspension call landed inside a block that has statements
    /// after it. Phase 2 v1/v2 only handle yield-at-tail; v3 will
    /// split mid-block by replicating the post-yield tail across
    /// successors. Hosts can also work around this by normalising
    /// their CFG before calling krio-async.
    SuspensionInBranchedBlock { block: B },
    /// A `SuspensionSite::CrossFnCall` was classified — Phase 3
    /// hasn't landed.
    Unimplemented,
}

/// Transform a function's CFG into a state-machine layout.
///
/// **Phase 2 v2** — direct yields with captures lift:
///
/// - Splits each yield's block at the yield, building the
///   `resume_entries` table.
/// - Reads `liveness` to find values that cross each suspension,
///   allocates a slot per unique value across the function, and
///   populates `yield_saves` (what to save before each yield's
///   Return) + `resume_loads` (what to load at each resume entry).
/// - Refuses cross-function calls to suspending callees with
///   [`TransformError::Unimplemented`] (Phase 3 covers them).
/// - Refuses suspensions that aren't the last statement of their
///   block with [`TransformError::SuspensionInBranchedBlock`].
///
/// On success, the host's lowering reads the layout to:
/// 1. Emit a dispatcher prologue (load state_id, switch to
///    `resume_entries[state_id]`).
/// 2. For each yield block, emit `runtime_save(frame, slot, v)`
///    for every `(slot, v)` in `yield_saves[block]` before the
///    Return; advance state; stamp kind=Yield.
/// 3. For each resume block in `resume_loads`, emit
///    `let v_fresh = runtime_load(frame, slot)` and rewrite
///    downstream uses of the original `v` to `v_fresh` using the
///    host's IR's normal use-rewriting machinery.
///
/// Slot allocation is **one slot per unique LocalId** across all
/// suspensions in this function. A value live at multiple yields
/// uses the same slot (saved at each yield, loaded at each resume).
/// Hosts wanting tighter packing (e.g. share slots between
/// non-overlapping live ranges) can pass a smaller `liveness` and
/// trust krio-async's allocator.
pub fn transform_to_state_machine<S, H>(
    cfg: &mut H::Cfg,
    fn_id: H::FnId,
    suspending: &S,
    hooks: &H,
    liveness: &LivenessMap<
        <H::Cfg as krio_stackless::CoroCfg>::BlockId,
        <H::Cfg as krio_stackless::CoroCfg>::LocalId,
    >,
) -> Result<
    StateMachineLayout<
        <H::Cfg as krio_stackless::CoroCfg>::BlockId,
        <H::Cfg as krio_stackless::CoroCfg>::LocalId,
        H::FnId,
    >,
    TransformError<<H::Cfg as krio_stackless::CoroCfg>::BlockId>,
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

    // Snapshot block IDs before splitting.
    let original_blocks: Vec<_> = cfg.block_ids();
    let entry_block = *original_blocks
        .first()
        .expect("krio-async: cfg has no blocks");

    // Pass 1 — scan + classify + validate.
    //
    // Caps:
    // - DirectYield must be the last statement in its block. Hosts
    //   normalise mid-block direct yields before calling.
    // - CrossFnCall can be anywhere; the split + synthetic resume
    //   block cover the post-call code path.
    // - Phase 3 v1 cap: at most one suspension per original block.
    //   Splitting once per block keeps the position bookkeeping
    //   trivial; lifting this is straightforward (process splits
    //   high-to-low) but not in v1.
    let mut sites = Vec::new();
    let mut seen_blocks = alloc::collections::BTreeSet::new();
    for &bb in &original_blocks {
        let count = cfg.statement_count(bb);
        for idx in 0..count {
            if let Some(site) = hooks.classify(cfg, bb, idx) {
                if !seen_blocks.insert(bb) {
                    return Err(TransformError::SuspensionInBranchedBlock { block: bb });
                }
                match &site {
                    SuspensionSite::DirectYield { .. } => {
                        if idx + 1 != count {
                            return Err(TransformError::SuspensionInBranchedBlock {
                                block: bb,
                            });
                        }
                    }
                    SuspensionSite::CrossFnCall { .. } => {
                        // No tail requirement — the split + resume
                        // pair handles post-call code in any position.
                    }
                }
                sites.push((bb, idx, site));
            }
        }
    }

    // Pass 2 — split each suspension's block + record state IDs +
    // populate block_kinds.
    let mut resume_entries = vec![entry_block];
    let mut yield_blocks = Vec::new();
    let mut block_kinds = Vec::new();
    // For Pass 3 (captures lift), we need to know which resume entry
    // corresponds to which (bb, idx) site. DirectYield's resume is
    // resume_entries[N+1]; cross-fn's resume is the synthetic
    // resume_check block. Record the index alongside the site.
    let mut site_resumes: Vec<<H::Cfg as krio_stackless::CoroCfg>::BlockId> =
        Vec::with_capacity(sites.len());

    for (bb, idx, site) in &sites {
        match site {
            SuspensionSite::DirectYield { .. } => {
                let resume = cfg.split_after(*bb, *idx);
                let next_state = resume_entries.len() as u32;
                resume_entries.push(resume);
                yield_blocks.push((*bb, next_state));
                block_kinds.push((*bb, BlockKind::DirectYield));
                site_resumes.push(resume);
            }
            SuspensionSite::CrossFnCall {
                callee,
                receiver,
                args,
                result,
            } => {
                // Split: bb keeps [0..=idx], post_call gets [idx+1..]
                // and inherits bb's terminator.
                let post_call = cfg.split_after(*bb, *idx);
                // Synthetic resume_check block — the dispatcher's
                // br_table lands here on resume from a yielded child.
                let resume_check = cfg.new_block();

                let next_state = resume_entries.len() as u32;
                resume_entries.push(resume_check);
                // bb is a "yields" block: when the host lowers it as
                // "set state, push child frame, return Pending", the
                // next_state advance lands the dispatcher in
                // resume_check next time.
                yield_blocks.push((*bb, next_state));

                block_kinds.push((
                    *bb,
                    BlockKind::CrossFnCallInit {
                        resume_check_block: resume_check,
                        receiver: *receiver,
                        args: args.clone(),
                        result: *result,
                        callee: *callee,
                    },
                ));
                block_kinds.push((
                    resume_check,
                    BlockKind::CrossFnCallResume {
                        done_block: post_call,
                        receiver: *receiver,
                        args: args.clone(),
                        result: *result,
                        callee: *callee,
                    },
                ));
                // Captures lift treats the resume_check block as the
                // "load" side — values defined before the call must
                // be reloaded there to be visible after.
                site_resumes.push(resume_check);
            }
        }
    }

    // Pass 3 — captures lift. Walk the suspension sites again,
    // consult `liveness`, allocate slots, and record save/load
    // pairs. Slot allocation is global per function: a value live
    // at multiple suspensions reuses its slot.
    use alloc::collections::BTreeMap;
    let mut value_to_slot: BTreeMap<
        <H::Cfg as krio_stackless::CoroCfg>::LocalId,
        u32,
    > = BTreeMap::new();
    let mut next_slot: u32 = 0;
    let mut yield_saves = Vec::new();
    let mut resume_loads = Vec::new();

    for (i, (bb, idx, _site)) in sites.iter().enumerate() {
        let live = liveness.lookup(*bb, *idx);
        if live.is_empty() {
            continue;
        }
        let mut entries: Vec<(u32, _)> = Vec::with_capacity(live.len());
        for &v in live {
            let slot = *value_to_slot.entry(v).or_insert_with(|| {
                let s = next_slot;
                next_slot += 1;
                s
            });
            entries.push((slot, v));
        }
        yield_saves.push((*bb, entries.clone()));
        resume_loads.push((site_resumes[i], entries));
    }

    Ok(StateMachineLayout {
        resume_entries,
        yield_blocks,
        yield_saves,
        resume_loads,
        block_kinds,
    })
}
