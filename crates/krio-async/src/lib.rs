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
//! ## Status — Phase 3 v2
//!
//! All three suspension shapes work end-to-end and any block can
//! hold any number of them:
//!
//! - **Direct yields** at any position in a block (split moves the
//!   post-yield tail into the resume entry).
//! - **Cross-fn calls** to suspending callees, lowered as an
//!   [`BlockKind::CrossFnCallInit`] / [`BlockKind::CrossFnCallResume`]
//!   pair around a synthetic resume_check block.
//! - **Multiple suspensions per original block** in any combination.
//!   Sites are processed in source order; each split's tail block
//!   becomes the host of any subsequent suspensions in the same
//!   original block.
//!
//! Liveness is keyed by *original* `(block, idx)` coordinates the
//! host wrote against the pre-transform CFG; krio-async maps each
//! site to its post-split yield-block and resume entry internally.
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

pub mod validate;
pub use validate::{LayoutError, validate_layout};

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

/// `BlockId` of the host CFG behind an [`AsyncHooks`] impl.
pub type HookBlockId<H> = <<H as AsyncHooks>::Cfg as krio_stackless::CoroCfg>::BlockId;
/// `LocalId` of the host CFG behind an [`AsyncHooks`] impl.
pub type HookLocalId<H> = <<H as AsyncHooks>::Cfg as krio_stackless::CoroCfg>::LocalId;
/// `Result` type returned by the transform entry points. Folds the
/// long associated-type projections through [`AsyncHooks`] into a
/// single alias so the signatures stay readable.
pub type TransformResult<H> = Result<
    StateMachineLayout<HookBlockId<H>, HookLocalId<H>, <H as AsyncHooks>::FnId>,
    TransformError<HookBlockId<H>>,
>;

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
///
/// The Phase 3 v2 transform has no current error variants — every
/// pattern Phase 2 v1/v2 capped (mid-block yields, multiple yields
/// in one block, cross-fn calls) is now handled. The enum is
/// `#[non_exhaustive]` so future variants can be added without an
/// API break.
#[derive(Debug)]
#[non_exhaustive]
pub enum TransformError<B: CfgId> {
    #[doc(hidden)]
    _Phantom(core::marker::PhantomData<B>),
}

/// Transform a function's CFG into a state-machine layout.
///
/// **Phase 3 v2** — direct yields, cross-fn calls, and multiple
/// suspensions per original block in any combination:
///
/// - Splits each suspension's block at the suspension point,
///   building the `resume_entries` table. Multiple suspensions in
///   the same original block are processed in source order; each
///   split's tail block becomes the host of any subsequent
///   suspensions.
/// - Reads `liveness` (keyed by *original* `(block, idx)` —
///   coordinates the host wrote against the pre-transform CFG) to
///   find values that cross each suspension, allocates a slot per
///   unique value across the function, and populates `yield_saves`
///   + `resume_loads`.
/// - Cross-fn calls produce an `Init` / `Resume` pair around a
///   synthetic `resume_check` block; captures load at the
///   `resume_check`, not at the post-call block.
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
///
/// This function calls [`transform_to_state_machine_with_options`]
/// with default options. Use the `_with_options` variant if your
/// host needs to reserve slots for state-id, params, or other
/// runtime-ABI bookkeeping (see [`TransformOptions`]).
pub fn transform_to_state_machine<S, H>(
    cfg: &mut H::Cfg,
    fn_id: H::FnId,
    suspending: &S,
    hooks: &H,
    liveness: &LivenessMap<HookBlockId<H>, HookLocalId<H>>,
) -> TransformResult<H>
where
    S: SuspendingFns<FnId = H::FnId>,
    H: AsyncHooks,
{
    transform_to_state_machine_with_options(
        cfg,
        fn_id,
        suspending,
        hooks,
        liveness,
        TransformOptions::default(),
    )
}

/// Tunables for [`transform_to_state_machine_with_options`].
///
/// Hosts that need to reserve slots for runtime-ABI bookkeeping
/// (state-id, function parameters, scratch space) can use this to
/// shift krio's slot allocator past the reserved range. Without it,
/// captures-lift slots are allocated from 0 upward and the host has
/// to either:
/// (a) put its bookkeeping at `next_slot..` post-transform, or
/// (b) avoid the natural `slot=0 means state-id` convention.
///
/// Either workaround is brittle. With reservation, the contract is
/// explicit: krio guarantees no captures-lift slot will fall in
/// `0..reserved_slots`.
///
/// ```text
///  slot 0 ─┐
///         ├─ host's reservation (state-id, params, scratch)
///  N-1   ─┘
///  N     ─┐
///         ├─ krio's captures-lift allocation (one per unique LocalId)
///  ...   ─┘
/// ```
#[derive(Debug, Clone, Default)]
pub struct TransformOptions {
    /// First slot index krio's captures-lift allocator may use.
    /// Slots `0..reserved_slots` are off-limits — the host owns them.
    /// Default: 0 (no reservation, krio uses the full slot space).
    pub reserved_slots: u32,
}

/// Like [`transform_to_state_machine`] but with explicit options.
///
/// Currently the only option is [`TransformOptions::reserved_slots`].
/// See [`TransformOptions`] for the slot-reservation rationale.
pub fn transform_to_state_machine_with_options<S, H>(
    cfg: &mut H::Cfg,
    fn_id: H::FnId,
    suspending: &S,
    hooks: &H,
    liveness: &LivenessMap<HookBlockId<H>, HookLocalId<H>>,
    options: TransformOptions,
) -> TransformResult<H>
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

    // Pass 1 — scan original blocks once for sites. Coordinates
    // captured here are pre-transform; downstream liveness lookups
    // use them directly. Element type:
    // `(HookBlockId<H>, usize, SuspensionSite<H::FnId, HookLocalId<H>>)`.
    let mut sites = Vec::new();
    for &bb in &original_blocks {
        let count = cfg.statement_count(bb);
        for idx in 0..count {
            if let Some(site) = hooks.classify(cfg, bb, idx) {
                sites.push((bb, idx, site));
            }
        }
    }

    // Process sites in (original_block, original_idx) ascending
    // order. Sites in the same original block share a "running"
    // current_block: the first split moves the post-suspension tail
    // into a new block, that new block becomes the home of the
    // next suspension in the same original block, and so on. The
    // tail block created by a DirectYield split serves both as
    // (a) the resume entry for the yield and (b) the home of any
    // later suspensions in the same original block. For CrossFnCall
    // the resume entry is the synthetic resume_check block, while
    // the home of later suspensions is the post_call block (the
    // tail of the split).
    sites.sort_by_key(|(bb, idx, _)| (*bb, *idx));

    let mut resume_entries = vec![entry_block];
    let mut yield_blocks = Vec::new();
    let mut block_kinds = Vec::new();
    let mut site_yield_blocks: Vec<<H::Cfg as krio_stackless::CoroCfg>::BlockId> =
        Vec::with_capacity(sites.len());
    let mut site_resumes: Vec<<H::Cfg as krio_stackless::CoroCfg>::BlockId> =
        Vec::with_capacity(sites.len());

    let mut current_orig_block = entry_block;
    let mut current_block = entry_block;
    let mut prev_orig_idx: Option<usize> = None;

    for (orig_bb, orig_idx, site) in &sites {
        if *orig_bb != current_orig_block || prev_orig_idx.is_none() {
            current_orig_block = *orig_bb;
            current_block = *orig_bb;
            prev_orig_idx = None;
        }

        // Translate the host's pre-transform `orig_idx` into the
        // current_block's idx: the previous split removed the
        // statements through `prev_orig_idx`, so subtract that off.
        let current_idx = match prev_orig_idx {
            None => *orig_idx,
            Some(prev) => *orig_idx - (prev + 1),
        };

        match site {
            SuspensionSite::DirectYield { .. } => {
                let resume = cfg.split_after(current_block, current_idx);
                let next_state = resume_entries.len() as u32;
                resume_entries.push(resume);
                yield_blocks.push((current_block, next_state));
                block_kinds.push((current_block, BlockKind::DirectYield));
                site_yield_blocks.push(current_block);
                site_resumes.push(resume);

                // Subsequent suspensions in the same original block
                // live in `resume`.
                current_block = resume;
                prev_orig_idx = Some(*orig_idx);
            }
            SuspensionSite::CrossFnCall {
                callee,
                receiver,
                args,
                result,
            } => {
                // Split: current_block keeps [0..=current_idx],
                // post_call gets the tail and inherits the original
                // terminator.
                let post_call = cfg.split_after(current_block, current_idx);
                // Synthetic resume_check block — the dispatcher's
                // br_table lands here on resume from a yielded child.
                let resume_check = cfg.new_block();

                let next_state = resume_entries.len() as u32;
                resume_entries.push(resume_check);
                // current_block is a "yields" block: when the host
                // lowers it as "set state, push child frame, save
                // args, return Pending", the next_state advance lands
                // the dispatcher in resume_check next time.
                yield_blocks.push((current_block, next_state));

                block_kinds.push((
                    current_block,
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
                site_yield_blocks.push(current_block);
                site_resumes.push(resume_check);

                // Subsequent suspensions in the same original block
                // live in `post_call` — that's where the post-call
                // statements went.
                current_block = post_call;
                prev_orig_idx = Some(*orig_idx);
            }
        }
    }

    // Pass 3 — captures lift. Walk the suspension sites again,
    // consult `liveness`, allocate slots, and record save/load
    // pairs. Slot allocation is global per function: a value live
    // at multiple suspensions reuses its slot.
    use alloc::collections::BTreeMap;
    let mut value_to_slot: BTreeMap<<H::Cfg as krio_stackless::CoroCfg>::LocalId, u32> =
        BTreeMap::new();
    // Captures-lift slot allocator starts at `reserved_slots` so the
    // host can pre-claim `0..reserved_slots` for runtime-ABI uses
    // (state-id, params, etc.) without colliding with krio's slots.
    let mut next_slot: u32 = options.reserved_slots;
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
        yield_saves.push((site_yield_blocks[i], entries.clone()));
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
