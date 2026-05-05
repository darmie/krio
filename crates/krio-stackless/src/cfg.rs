//! Phase-2 trait surface. Lets krio drive any CFG-shaped IR provided
//! the consumer can answer a small set of questions about it.
//!
//! Two traits:
//!
//!   `CoroCfg`   — read+mut access to the consumer's body. Block /
//!                 local construction, statement emission for the
//!                 specific patterns the algorithm builds, terminator
//!                 manipulation. About a dozen methods total.
//!
//!   `CoroHooks` — consumer-specific surgery the algorithm can't do
//!                 without knowing the IR shape: classify a statement
//!                 as a marker, and (for guarded recv only) emit the
//!                 `is_ready` peek that splits the recv across two
//!                 blocks.
//!
//! These are designed against the algorithm's actual needs, not in
//! the abstract — every method maps to a real call site in `lib.rs`.
//! The reference implementation for Zura's MIR lives in the
//! `zura_mir_adapter` module; a toy adapter under `tests/` validates
//! the abstraction against a non-Zura IR.

// `Marker` and `CfgId` come from `krio-core` so every variant in the
// family — stackless transform, cross-fn async, stackful fibers,
// preemptive scheduler — speaks the same vocabulary.
pub use krio_core::{CfgId, Marker};

/// The CFG body the transform operates on. Read+mut access for
/// algorithm-driven mutations.
pub trait CoroCfg {
    type BlockId: CfgId;
    type LocalId: CfgId;

    // ── Read access ────────────────────────────────────────────────

    /// How many blocks does this body have?
    fn block_count(&self) -> usize;

    /// How many statements live in `bb`?
    fn statement_count(&self, bb: Self::BlockId) -> usize;

    /// Iterate the (in-order) block IDs. Used during region discovery.
    fn block_ids(&self) -> Vec<Self::BlockId>;

    // ── Construction ───────────────────────────────────────────────

    /// Allocate a fresh empty block. The default terminator is the
    /// consumer's "unreachable" or equivalent — krio overwrites it
    /// before returning.
    fn new_block(&mut self) -> Self::BlockId;

    /// Allocate a fresh i64-typed mutable local. Used for state and
    /// poll-result locals.
    fn new_state_local(&mut self) -> Self::LocalId;

    /// Allocate a fresh bool-typed (immutable) local. Used for the
    /// is_done / is_ready check temporaries.
    fn new_bool_local(&mut self) -> Self::LocalId;

    /// Allocate a fresh bool-typed mutable local. Used for the
    /// `all_done` flag in the executor loop.
    fn new_mut_bool_local(&mut self) -> Self::LocalId;

    // ── Statement emission (append-only) ───────────────────────────

    /// Append `local = const_i64(value)` to `bb`.
    fn emit_assign_i64(&mut self, bb: Self::BlockId, local: Self::LocalId, value: i64);

    /// Append `local = const_bool(value)` to `bb`.
    fn emit_assign_bool(&mut self, bb: Self::BlockId, local: Self::LocalId, value: bool);

    /// Append `dest = (lhs == const_i64(rhs))` to `bb`.
    fn emit_eq_check_i64(
        &mut self,
        bb: Self::BlockId,
        dest: Self::LocalId,
        lhs: Self::LocalId,
        rhs: i64,
    );

    // ── Block manipulation ─────────────────────────────────────────

    /// Replace the statement at `(bb, idx)` with a no-op. Used to
    /// erase markers after their structural role is done.
    fn replace_with_nop(&mut self, bb: Self::BlockId, idx: usize);

    /// Move every statement after `idx` from `src` into a fresh
    /// block, transferring `src`'s terminator to it. After this call
    /// `src` ends at index `idx` (inclusive) with no terminator set —
    /// the caller is expected to set one.
    fn split_after(&mut self, src: Self::BlockId, idx: usize) -> Self::BlockId;

    /// Insert `(stmt_local = const_i64(value))` at the FRONT of
    /// `bb`'s statement list. Used by the cooperative executor when
    /// initialising state locals before entering the loop.
    fn prepend_assign_i64(
        &mut self,
        bb: Self::BlockId,
        local: Self::LocalId,
        value: i64,
    );

    // ── Terminator manipulation ────────────────────────────────────

    /// Set `bb`'s terminator to `goto target`.
    fn set_goto(&mut self, bb: Self::BlockId, target: Self::BlockId);

    /// Set `bb`'s terminator to a two-way branch on `cond`.
    fn set_branch(
        &mut self,
        bb: Self::BlockId,
        cond: Self::LocalId,
        true_bb: Self::BlockId,
        false_bb: Self::BlockId,
    );

    /// Set `bb`'s terminator to a switch on `discr` with the given
    /// `(value, target)` pairs and an `otherwise` fallthrough.
    fn set_switch(
        &mut self,
        bb: Self::BlockId,
        discr: Self::LocalId,
        targets: Vec<(i64, Self::BlockId)>,
        otherwise: Self::BlockId,
    );

    /// Within `bb`'s terminator, rewrite every reference to `from`
    /// so it points to `to`. Touches every shape (goto / branch /
    /// switch / call etc.) the consumer's IR supports.
    fn redirect_targets(
        &mut self,
        bb: Self::BlockId,
        from: Self::BlockId,
        to: Self::BlockId,
    );
}

/// Consumer-side hooks the abstract algorithm can't perform on its
/// own — classification of marker statements + the IR-specific
/// surgery for guarded recv (which has to insert a peek and move
/// the original recv across a block boundary).
pub trait CoroHooks {
    type Cfg: CoroCfg;

    /// Classify a statement at `(bb, idx)` as one of the marker
    /// categories, or `None` if it's a regular statement.
    fn classify_marker(
        &self,
        cfg: &Self::Cfg,
        bb: <Self::Cfg as CoroCfg>::BlockId,
        idx: usize,
    ) -> Option<Marker>;

    /// At a `GuardedRecv` suspension slot, emit the peek and hand
    /// back the bool LocalId. The caller has already split the block
    /// after `idx`; the hook receives the resume block as `resume_bb`
    /// so it can MOVE the original recv statement there before
    /// replacing the slot at `(bb, idx)` with the peek.
    ///
    /// After this call, krio expects:
    ///   - `(bb, idx)` is the peek statement (assigning to the
    ///     returned bool LocalId).
    ///   - The original recv lives at `(resume_bb, 0)` (prepended).
    ///
    /// Krio then sets `bb`'s terminator to `branch(returned_local,
    /// resume_bb, yield_bb)`.
    fn emit_guarded_recv_peek(
        &mut self,
        cfg: &mut Self::Cfg,
        bb: <Self::Cfg as CoroCfg>::BlockId,
        idx: usize,
        resume_bb: <Self::Cfg as CoroCfg>::BlockId,
    ) -> <Self::Cfg as CoroCfg>::LocalId;
}
