//! Phase 2 v1 tests: contract surface + the real direct-yield
//! transform against a minimal toy CFG.

use krio_async::{
    AsyncHooks, BlockKind, FrameState, LivenessMap, SuspendingFns, SuspensionSite, TransformError,
    transform_to_state_machine,
};
use krio_stackless::CoroCfg;
use std::collections::HashSet;

// ── Host types ────────────────────────────────────────────────────

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
struct ToyBlockId(u32);
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
struct ToyValueId(u32);
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
struct ToyFnId(u32);

#[derive(Debug, Clone)]
enum ToyStmt {
    /// A regular statement krio-async ignores.
    User(&'static str),
    /// A direct yield call. The hooks classify this as DirectYield.
    Yield,
}

#[derive(Debug, Default)]
struct ToyCfg {
    blocks: Vec<Vec<ToyStmt>>,
}

impl ToyCfg {
    fn new() -> Self {
        ToyCfg::default()
    }

    fn push_block(&mut self, stmts: Vec<ToyStmt>) -> ToyBlockId {
        let id = ToyBlockId(self.blocks.len() as u32);
        self.blocks.push(stmts);
        id
    }
}

impl CoroCfg for ToyCfg {
    type BlockId = ToyBlockId;
    type LocalId = ToyValueId;

    fn block_count(&self) -> usize {
        self.blocks.len()
    }
    fn statement_count(&self, bb: ToyBlockId) -> usize {
        self.blocks[bb.0 as usize].len()
    }
    fn block_ids(&self) -> Vec<ToyBlockId> {
        (0..self.blocks.len() as u32).map(ToyBlockId).collect()
    }

    fn new_block(&mut self) -> ToyBlockId {
        let id = ToyBlockId(self.blocks.len() as u32);
        self.blocks.push(Vec::new());
        id
    }

    fn split_after(&mut self, src: ToyBlockId, idx: usize) -> ToyBlockId {
        let src_idx = src.0 as usize;
        let tail: Vec<_> = self.blocks[src_idx].drain((idx + 1)..).collect();
        let id = ToyBlockId(self.blocks.len() as u32);
        self.blocks.push(tail);
        id
    }

    // Phase 2 v1 doesn't call any of these — krio-async returns a
    // layout, the host emits the actual code. Stubs would only be
    // hit if a future v1 cap leaks; keep them honest.

    fn new_state_local(&mut self) -> ToyValueId {
        unimplemented!("toy cfg: new_state_local not used by Phase 2 v1")
    }
    fn new_bool_local(&mut self) -> ToyValueId {
        unimplemented!()
    }
    fn new_mut_bool_local(&mut self) -> ToyValueId {
        unimplemented!()
    }
    fn emit_assign_i64(&mut self, _: ToyBlockId, _: ToyValueId, _: i64) {
        unimplemented!()
    }
    fn emit_assign_bool(&mut self, _: ToyBlockId, _: ToyValueId, _: bool) {
        unimplemented!()
    }
    fn emit_eq_check_i64(&mut self, _: ToyBlockId, _: ToyValueId, _: ToyValueId, _: i64) {
        unimplemented!()
    }
    fn replace_with_nop(&mut self, _: ToyBlockId, _: usize) {
        unimplemented!()
    }
    fn prepend_assign_i64(&mut self, _: ToyBlockId, _: ToyValueId, _: i64) {
        unimplemented!()
    }
    fn set_goto(&mut self, _: ToyBlockId, _: ToyBlockId) {
        unimplemented!()
    }
    fn set_branch(&mut self, _: ToyBlockId, _: ToyValueId, _: ToyBlockId, _: ToyBlockId) {
        unimplemented!()
    }
    fn set_switch(
        &mut self,
        _: ToyBlockId,
        _: ToyValueId,
        _: Vec<(i64, ToyBlockId)>,
        _: ToyBlockId,
    ) {
        unimplemented!()
    }
    fn redirect_targets(&mut self, _: ToyBlockId, _: ToyBlockId, _: ToyBlockId) {
        unimplemented!()
    }
}

struct ToySuspending {
    suspending: HashSet<ToyFnId>,
    yields: HashSet<ToyFnId>,
}
impl SuspendingFns for ToySuspending {
    type FnId = ToyFnId;
    fn is_suspending(&self, id: ToyFnId) -> bool {
        self.suspending.contains(&id)
    }
    fn is_yield_primitive(&self, id: ToyFnId) -> bool {
        self.yields.contains(&id)
    }
}

struct ToyHooks;
impl AsyncHooks for ToyHooks {
    type Cfg = ToyCfg;
    type FnId = ToyFnId;
    fn classify(
        &self,
        cfg: &ToyCfg,
        bb: ToyBlockId,
        idx: usize,
    ) -> Option<SuspensionSite<ToyFnId, ToyValueId>> {
        match cfg.blocks[bb.0 as usize][idx] {
            ToyStmt::Yield => Some(SuspensionSite::DirectYield { value: None }),
            _ => None,
        }
    }
}

// ── Tests ─────────────────────────────────────────────────────────

#[test]
fn non_suspending_fn_returns_trivial_layout() {
    let mut cfg = ToyCfg::new();
    cfg.push_block(vec![ToyStmt::User("noop")]);

    let suspending = ToySuspending {
        suspending: HashSet::new(),
        yields: HashSet::new(),
    };
    let hooks = ToyHooks;

    let layout = transform_to_state_machine(&mut cfg, ToyFnId(42), &suspending, &hooks, &LivenessMap::new())
        .expect("non-suspending fn should produce a trivial Ok layout");
    assert!(layout.resume_entries.is_empty());
    assert!(layout.yield_blocks.is_empty());
    assert!(layout.block_kinds.is_empty());
}

#[test]
fn no_yields_in_suspending_fn_just_records_entry() {
    // Suspending fn body with no actual yields — layout has just
    // state 0 (entry) and no yield blocks. Useful for fns that are
    // tainted (call something that may yield) but happen to take a
    // path that doesn't.
    let mut cfg = ToyCfg::new();
    cfg.push_block(vec![ToyStmt::User("a"), ToyStmt::User("b")]);

    let suspending = ToySuspending {
        suspending: HashSet::from([ToyFnId(1)]),
        yields: HashSet::new(),
    };
    let layout =
        transform_to_state_machine(&mut cfg, ToyFnId(1), &suspending, &ToyHooks, &LivenessMap::new()).unwrap();
    assert_eq!(layout.resume_entries.len(), 1, "state 0 = entry");
    assert!(layout.yield_blocks.is_empty());
    assert!(layout.block_kinds.is_empty());
}

#[test]
fn single_yield_at_block_tail_splits_correctly() {
    let mut cfg = ToyCfg::new();
    let bb0 = cfg.push_block(vec![ToyStmt::User("before"), ToyStmt::Yield]);
    let _ = bb0;

    let suspending = ToySuspending {
        suspending: HashSet::from([ToyFnId(1)]),
        yields: HashSet::new(),
    };
    let layout =
        transform_to_state_machine(&mut cfg, ToyFnId(1), &suspending, &ToyHooks, &LivenessMap::new()).unwrap();

    // resume_entries[0] = original entry, resume_entries[1] = block
    // created by splitting after the yield (initially empty).
    assert_eq!(layout.resume_entries.len(), 2);
    assert_eq!(layout.yield_blocks.len(), 1);
    assert_eq!(layout.yield_blocks[0].1, 1, "next state after first yield = 1");

    // Yield block kept the User + Yield; resume entry got the (empty) tail.
    assert_eq!(cfg.statement_count(layout.yield_blocks[0].0), 2);
    assert_eq!(cfg.statement_count(layout.resume_entries[1]), 0);

    // Block kind classification.
    assert_eq!(layout.block_kinds.len(), 1);
    assert!(matches!(layout.block_kinds[0].1, BlockKind::DirectYield));
}

#[test]
fn yield_in_middle_of_block_is_v1_capped() {
    // Yield not at tail → v1 refuses with SuspensionInBranchedBlock
    // (the term is wren_lift's; "branched" here means "block has
    // statements after the yield", which for a real IR implies the
    // block continues past the yield via fall-through into a Branch
    // or similar).
    let mut cfg = ToyCfg::new();
    cfg.push_block(vec![
        ToyStmt::User("before"),
        ToyStmt::Yield,
        ToyStmt::User("after"),
    ]);

    let suspending = ToySuspending {
        suspending: HashSet::from([ToyFnId(1)]),
        yields: HashSet::new(),
    };
    let result = transform_to_state_machine(&mut cfg, ToyFnId(1), &suspending, &ToyHooks, &LivenessMap::new());
    assert!(matches!(
        result,
        Err(TransformError::SuspensionInBranchedBlock { .. })
    ));
}

// ── Phase 3: cross-fn call dispatch ───────────────────────────────

/// Hook that classifies a chosen statement index as a cross-fn call.
struct CrossFnAt {
    bb: ToyBlockId,
    idx: usize,
    callee: ToyFnId,
    args: Vec<ToyValueId>,
    result: ToyValueId,
}
impl AsyncHooks for CrossFnAt {
    type Cfg = ToyCfg;
    type FnId = ToyFnId;
    fn classify(
        &self,
        _cfg: &ToyCfg,
        bb: ToyBlockId,
        idx: usize,
    ) -> Option<SuspensionSite<ToyFnId, ToyValueId>> {
        if bb == self.bb && idx == self.idx {
            Some(SuspensionSite::CrossFnCall {
                callee: self.callee,
                receiver: None,
                args: self.args.clone(),
                result: self.result,
            })
        } else {
            None
        }
    }
}

#[test]
fn cross_fn_call_creates_init_resume_pair() {
    // Block: [setup, call, post]. Cross-fn at idx=1 — split should
    // yield: bb0 = [setup, call], post_call = [post]; resume_check
    // is a fresh synthetic block. block_kinds gets two entries:
    // CrossFnCallInit at bb0 + CrossFnCallResume at resume_check.
    let mut cfg = ToyCfg::new();
    let bb0 = cfg.push_block(vec![
        ToyStmt::User("setup"),
        ToyStmt::User("call"),
        ToyStmt::User("post"),
    ]);

    let suspending = ToySuspending {
        suspending: HashSet::from([ToyFnId(1)]),
        yields: HashSet::new(),
    };
    let hooks = CrossFnAt {
        bb: bb0,
        idx: 1,
        callee: ToyFnId(99),
        args: vec![ToyValueId(7)],
        result: ToyValueId(42),
    };

    let layout = transform_to_state_machine(
        &mut cfg,
        ToyFnId(1),
        &suspending,
        &hooks,
        &LivenessMap::new(),
    )
    .unwrap();

    // resume_entries[0] = bb0 (entry); resume_entries[1] = resume_check
    assert_eq!(layout.resume_entries.len(), 2);
    let resume_check = layout.resume_entries[1];

    // Yield-blocks records bb0 advancing to state 1 (resume_check).
    assert_eq!(layout.yield_blocks.len(), 1);
    assert_eq!(layout.yield_blocks[0], (bb0, 1));

    // Two block_kinds entries — Init at bb0, Resume at resume_check.
    assert_eq!(layout.block_kinds.len(), 2);
    let init = &layout.block_kinds[0];
    let resume = &layout.block_kinds[1];

    assert_eq!(init.0, bb0);
    match &init.1 {
        BlockKind::CrossFnCallInit {
            resume_check_block,
            args,
            result,
            callee,
            ..
        } => {
            assert_eq!(*resume_check_block, resume_check);
            assert_eq!(*args, vec![ToyValueId(7)]);
            assert_eq!(*result, ToyValueId(42));
            assert_eq!(*callee, ToyFnId(99));
        }
        other => panic!("expected CrossFnCallInit, got {other:?}"),
    }

    assert_eq!(resume.0, resume_check);
    match &resume.1 {
        BlockKind::CrossFnCallResume {
            done_block,
            args,
            result,
            callee,
            ..
        } => {
            // done_block is the post-call block — created by the
            // split; it has the User("post") statement.
            assert_eq!(*args, vec![ToyValueId(7)]);
            assert_eq!(*result, ToyValueId(42));
            assert_eq!(*callee, ToyFnId(99));
            // Sanity: the done block carries the post-call statement.
            assert_eq!(cfg.statement_count(*done_block), 1);
        }
        other => panic!("expected CrossFnCallResume, got {other:?}"),
    }

    // The yield block (bb0) keeps statements through the call.
    assert_eq!(cfg.statement_count(bb0), 2);
    // The resume_check block starts empty — host emits the helper.
    assert_eq!(cfg.statement_count(resume_check), 0);
}

#[test]
fn cross_fn_with_liveness_loads_at_resume_check() {
    // Captures-lift integration: live values across a cross-fn call
    // are saved before the Init's Return and loaded at the
    // resume_check block (NOT the post-call block).
    let mut cfg = ToyCfg::new();
    let bb0 = cfg.push_block(vec![
        ToyStmt::User("setup"),
        ToyStmt::User("call"),
        ToyStmt::User("post"),
    ]);

    let suspending = ToySuspending {
        suspending: HashSet::from([ToyFnId(1)]),
        yields: HashSet::new(),
    };
    let hooks = CrossFnAt {
        bb: bb0,
        idx: 1,
        callee: ToyFnId(99),
        args: vec![],
        result: ToyValueId(0),
    };

    let mut liveness = LivenessMap::new();
    liveness.record(bb0, 1, vec![ToyValueId(5)]);

    let layout = transform_to_state_machine(
        &mut cfg,
        ToyFnId(1),
        &suspending,
        &hooks,
        &liveness,
    )
    .unwrap();

    let resume_check = layout.resume_entries[1];
    assert_eq!(layout.yield_saves.len(), 1);
    assert_eq!(layout.yield_saves[0], (bb0, vec![(0, ToyValueId(5))]));
    assert_eq!(layout.resume_loads.len(), 1);
    assert_eq!(
        layout.resume_loads[0],
        (resume_check, vec![(0, ToyValueId(5))])
    );
}

#[test]
fn cross_fn_call_at_block_tail_works_too() {
    // Cross-fn calls don't require a post-call statement —
    // post_call can be empty, host's lowering still wires it up.
    let mut cfg = ToyCfg::new();
    let bb0 = cfg.push_block(vec![ToyStmt::User("call_at_tail")]);

    let suspending = ToySuspending {
        suspending: HashSet::from([ToyFnId(1)]),
        yields: HashSet::new(),
    };
    let hooks = CrossFnAt {
        bb: bb0,
        idx: 0,
        callee: ToyFnId(99),
        args: vec![],
        result: ToyValueId(0),
    };

    let layout = transform_to_state_machine(
        &mut cfg,
        ToyFnId(1),
        &suspending,
        &hooks,
        &LivenessMap::new(),
    )
    .unwrap();

    assert_eq!(layout.resume_entries.len(), 2);
    assert_eq!(layout.block_kinds.len(), 2);
}

#[test]
fn multiple_yields_each_get_own_state() {
    // Two blocks each ending in Yield → states 1 and 2 (state 0 =
    // entry).
    let mut cfg = ToyCfg::new();
    cfg.push_block(vec![ToyStmt::User("a"), ToyStmt::Yield]);
    cfg.push_block(vec![ToyStmt::User("b"), ToyStmt::Yield]);

    let suspending = ToySuspending {
        suspending: HashSet::from([ToyFnId(1)]),
        yields: HashSet::new(),
    };
    let layout =
        transform_to_state_machine(&mut cfg, ToyFnId(1), &suspending, &ToyHooks, &LivenessMap::new()).unwrap();

    assert_eq!(layout.resume_entries.len(), 3);
    assert_eq!(layout.yield_blocks.len(), 2);
    assert_eq!(layout.yield_blocks[0].1, 1);
    assert_eq!(layout.yield_blocks[1].1, 2);
}

#[test]
fn frame_state_default_is_state_zero() {
    let frame: FrameState<u64> = FrameState::default();
    assert_eq!(frame.state_id, 0);
    assert!(frame.saved_values.is_empty());
}

// ── Phase 2 v2: captures lift ─────────────────────────────────────

#[test]
fn liveness_drives_save_load_tables() {
    // One yield with two values live across it. The transform
    // should allocate slots [0, 1] and record both sides.
    let mut cfg = ToyCfg::new();
    cfg.push_block(vec![ToyStmt::User("compute"), ToyStmt::Yield]);

    let suspending = ToySuspending {
        suspending: HashSet::from([ToyFnId(1)]),
        yields: HashSet::new(),
    };

    let mut liveness = LivenessMap::new();
    liveness.record(
        ToyBlockId(0),
        1, // index of the Yield stmt
        vec![ToyValueId(7), ToyValueId(11)],
    );

    let layout = transform_to_state_machine(
        &mut cfg,
        ToyFnId(1),
        &suspending,
        &ToyHooks,
        &liveness,
    )
    .unwrap();

    assert_eq!(layout.yield_saves.len(), 1);
    let (save_block, saves) = &layout.yield_saves[0];
    assert_eq!(*save_block, ToyBlockId(0));
    assert_eq!(saves.len(), 2);
    assert_eq!(saves[0], (0, ToyValueId(7)));
    assert_eq!(saves[1], (1, ToyValueId(11)));

    // resume_loads carries the same (slot, original_value) pairs,
    // keyed on the resume entry block.
    assert_eq!(layout.resume_loads.len(), 1);
    let (load_block, loads) = &layout.resume_loads[0];
    assert_eq!(*load_block, layout.resume_entries[1]);
    assert_eq!(loads.len(), 2);
    assert_eq!(loads[0], (0, ToyValueId(7)));
    assert_eq!(loads[1], (1, ToyValueId(11)));
}

#[test]
fn empty_liveness_skips_save_load_tables() {
    // No live values → no entries in yield_saves / resume_loads.
    // Splits + state numbering still happen.
    let mut cfg = ToyCfg::new();
    cfg.push_block(vec![ToyStmt::User("a"), ToyStmt::Yield]);

    let suspending = ToySuspending {
        suspending: HashSet::from([ToyFnId(1)]),
        yields: HashSet::new(),
    };

    let layout = transform_to_state_machine(
        &mut cfg,
        ToyFnId(1),
        &suspending,
        &ToyHooks,
        &LivenessMap::new(),
    )
    .unwrap();

    assert_eq!(layout.resume_entries.len(), 2);
    assert!(layout.yield_saves.is_empty());
    assert!(layout.resume_loads.is_empty());
}

#[test]
fn shared_value_across_yields_reuses_slot() {
    // Same value live at two suspensions should hit the same slot.
    // Demonstrates that the slot allocator is per-function, not
    // per-suspension.
    let mut cfg = ToyCfg::new();
    cfg.push_block(vec![ToyStmt::User("def_v"), ToyStmt::Yield]);
    cfg.push_block(vec![ToyStmt::User("use_v"), ToyStmt::Yield]);

    let suspending = ToySuspending {
        suspending: HashSet::from([ToyFnId(1)]),
        yields: HashSet::new(),
    };

    let mut liveness = LivenessMap::new();
    liveness.record(ToyBlockId(0), 1, vec![ToyValueId(42)]);
    liveness.record(ToyBlockId(1), 1, vec![ToyValueId(42)]);

    let layout = transform_to_state_machine(
        &mut cfg,
        ToyFnId(1),
        &suspending,
        &ToyHooks,
        &liveness,
    )
    .unwrap();

    assert_eq!(layout.yield_saves.len(), 2);
    // Both saves use slot 0 because the same value is live at both.
    assert_eq!(layout.yield_saves[0].1[0], (0, ToyValueId(42)));
    assert_eq!(layout.yield_saves[1].1[0], (0, ToyValueId(42)));
}

#[test]
fn distinct_values_get_distinct_slots() {
    // Different values at different suspensions get separate slots.
    let mut cfg = ToyCfg::new();
    cfg.push_block(vec![ToyStmt::User("def_a"), ToyStmt::Yield]);
    cfg.push_block(vec![ToyStmt::User("def_b"), ToyStmt::Yield]);

    let suspending = ToySuspending {
        suspending: HashSet::from([ToyFnId(1)]),
        yields: HashSet::new(),
    };

    let mut liveness = LivenessMap::new();
    liveness.record(ToyBlockId(0), 1, vec![ToyValueId(1)]);
    liveness.record(ToyBlockId(1), 1, vec![ToyValueId(2)]);

    let layout = transform_to_state_machine(
        &mut cfg,
        ToyFnId(1),
        &suspending,
        &ToyHooks,
        &liveness,
    )
    .unwrap();

    assert_eq!(layout.yield_saves[0].1[0], (0, ToyValueId(1)));
    assert_eq!(layout.yield_saves[1].1[0], (1, ToyValueId(2)));
}
