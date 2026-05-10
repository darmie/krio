//! Phase 3 v2 tests: contract surface + direct-yield split,
//! captures lift, cross-fn dispatch, and multi-suspension within
//! a single original block (any combination of direct + cross-fn).

use krio_async::{
    AsyncHooks, BlockKind, FrameState, LivenessMap, SuspendingFns, SuspensionSite,
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

/// Classifies a host-supplied set of (bb, idx) → site mappings.
/// Lets tests spell out exactly where suspensions live, including
/// multiple per block in any order.
struct ScriptedHooks {
    sites: Vec<(ToyBlockId, usize, SuspensionSite<ToyFnId, ToyValueId>)>,
}
impl AsyncHooks for ScriptedHooks {
    type Cfg = ToyCfg;
    type FnId = ToyFnId;
    fn classify(
        &self,
        _cfg: &ToyCfg,
        bb: ToyBlockId,
        idx: usize,
    ) -> Option<SuspensionSite<ToyFnId, ToyValueId>> {
        self.sites.iter().find_map(|(b, i, s)| {
            if *b == bb && *i == idx {
                Some(clone_site(s))
            } else {
                None
            }
        })
    }
}

fn clone_site(s: &SuspensionSite<ToyFnId, ToyValueId>) -> SuspensionSite<ToyFnId, ToyValueId> {
    match s {
        SuspensionSite::DirectYield { value } => SuspensionSite::DirectYield { value: *value },
        SuspensionSite::CrossFnCall {
            callee,
            receiver,
            args,
            result,
        } => SuspensionSite::CrossFnCall {
            callee: *callee,
            receiver: *receiver,
            args: args.clone(),
            result: *result,
        },
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
fn yield_mid_block_splits_post_yield_tail_into_resume() {
    // Phase 3 v2: yield in the middle of a block splits the
    // post-yield code into the resume entry. The yielding block
    // ends at the yield; the resume block carries everything after.
    let mut cfg = ToyCfg::new();
    let bb0 = cfg.push_block(vec![
        ToyStmt::User("before"),
        ToyStmt::Yield,
        ToyStmt::User("after"),
    ]);

    let suspending = ToySuspending {
        suspending: HashSet::from([ToyFnId(1)]),
        yields: HashSet::new(),
    };
    let layout =
        transform_to_state_machine(&mut cfg, ToyFnId(1), &suspending, &ToyHooks, &LivenessMap::new())
            .expect("v3 handles mid-block direct yield");

    // resume_entries[0] = bb0 (entry); resume_entries[1] = the
    // tail block created by splitting after the yield.
    assert_eq!(layout.resume_entries.len(), 2);
    let resume = layout.resume_entries[1];
    assert_eq!(layout.yield_blocks, vec![(bb0, 1)]);
    assert!(matches!(layout.block_kinds[0].1, BlockKind::DirectYield));

    // Yielding block keeps [before, Yield]; resume keeps [after].
    assert_eq!(cfg.statement_count(bb0), 2);
    assert_eq!(cfg.statement_count(resume), 1);
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

// ── Phase 3 v2: multiple suspensions per original block ───────────

#[test]
fn two_direct_yields_in_same_block_chain_through_resume() {
    // Block: [a, Yield, b, Yield, c]. Two splits — first at idx=1
    // produces tail_a = [b, Yield, c]; second at tail_a's idx=1
    // produces tail_b = [c]. yield_blocks attribute the second yield
    // to tail_a (NOT the original bb).
    let mut cfg = ToyCfg::new();
    let bb0 = cfg.push_block(vec![
        ToyStmt::User("a"),
        ToyStmt::Yield,
        ToyStmt::User("b"),
        ToyStmt::Yield,
        ToyStmt::User("c"),
    ]);

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

    // resume_entries: [bb0, tail_a, tail_b].
    assert_eq!(layout.resume_entries.len(), 3);
    let tail_a = layout.resume_entries[1];
    let tail_b = layout.resume_entries[2];

    // First yield's yielding block is bb0 advancing to state 1;
    // second yield's yielding block is tail_a advancing to state 2.
    assert_eq!(layout.yield_blocks, vec![(bb0, 1), (tail_a, 2)]);
    assert!(matches!(layout.block_kinds[0].1, BlockKind::DirectYield));
    assert_eq!(layout.block_kinds[0].0, bb0);
    assert!(matches!(layout.block_kinds[1].1, BlockKind::DirectYield));
    assert_eq!(layout.block_kinds[1].0, tail_a);

    // Block contents: bb0=[a, Yield], tail_a=[b, Yield], tail_b=[c].
    assert_eq!(cfg.statement_count(bb0), 2);
    assert_eq!(cfg.statement_count(tail_a), 2);
    assert_eq!(cfg.statement_count(tail_b), 1);
}

#[test]
fn three_direct_yields_in_same_block() {
    // Three suspensions exercise the running-tail bookkeeping over
    // more than two splits.
    let mut cfg = ToyCfg::new();
    let bb0 = cfg.push_block(vec![
        ToyStmt::Yield,
        ToyStmt::User("a"),
        ToyStmt::Yield,
        ToyStmt::User("b"),
        ToyStmt::Yield,
        ToyStmt::User("c"),
    ]);

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

    // 4 resume entries (state 0 + one per yield), 3 yield blocks.
    assert_eq!(layout.resume_entries.len(), 4);
    assert_eq!(layout.yield_blocks.len(), 3);
    assert_eq!(layout.yield_blocks[0].1, 1);
    assert_eq!(layout.yield_blocks[1].1, 2);
    assert_eq!(layout.yield_blocks[2].1, 3);
    // Each yield block ends at its yield: the first split's tail is
    // the second's host, etc. So bb0=[Yield], tail_1=[a, Yield],
    // tail_2=[b, Yield], tail_3=[c].
    assert_eq!(cfg.statement_count(bb0), 1);
    assert_eq!(cfg.statement_count(layout.resume_entries[1]), 2);
    assert_eq!(cfg.statement_count(layout.resume_entries[2]), 2);
    assert_eq!(cfg.statement_count(layout.resume_entries[3]), 1);
}

#[test]
fn cross_fn_then_direct_yield_in_same_block() {
    // bb0 = [setup, cross_call, mid, Yield, post]. The cross-fn
    // split moves [mid, Yield, post] into post_call; the yield
    // split inside post_call moves [post] into the final tail.
    let mut cfg = ToyCfg::new();
    let bb0 = cfg.push_block(vec![
        ToyStmt::User("setup"),
        ToyStmt::User("cross_call"),
        ToyStmt::User("mid"),
        ToyStmt::Yield,
        ToyStmt::User("post"),
    ]);

    let suspending = ToySuspending {
        suspending: HashSet::from([ToyFnId(1)]),
        yields: HashSet::new(),
    };
    let hooks = ScriptedHooks {
        sites: vec![
            (
                bb0,
                1,
                SuspensionSite::CrossFnCall {
                    callee: ToyFnId(99),
                    receiver: None,
                    args: vec![],
                    result: ToyValueId(0),
                },
            ),
            (bb0, 3, SuspensionSite::DirectYield { value: None }),
        ],
    };

    let layout = transform_to_state_machine(
        &mut cfg,
        ToyFnId(1),
        &suspending,
        &hooks,
        &LivenessMap::new(),
    )
    .unwrap();

    // resume_entries: [bb0, resume_check, tail_yield].
    assert_eq!(layout.resume_entries.len(), 3);
    let resume_check = layout.resume_entries[1];
    let tail_yield = layout.resume_entries[2];

    // yield_blocks attributes the cross-fn yield to bb0 (state 1)
    // and the direct yield to post_call (state 2). post_call is the
    // CrossFnCallResume's done_block.
    let init_done_block = match &layout.block_kinds[0].1 {
        BlockKind::CrossFnCallInit { resume_check_block, .. } => *resume_check_block,
        _ => panic!("expected Init at index 0"),
    };
    assert_eq!(init_done_block, resume_check);

    let post_call = match &layout.block_kinds[1].1 {
        BlockKind::CrossFnCallResume { done_block, .. } => *done_block,
        _ => panic!("expected Resume at index 1"),
    };

    assert_eq!(layout.yield_blocks, vec![(bb0, 1), (post_call, 2)]);

    // The DirectYield kind should be attached to post_call.
    let direct_kind = &layout.block_kinds[2];
    assert_eq!(direct_kind.0, post_call);
    assert!(matches!(direct_kind.1, BlockKind::DirectYield));

    // Block contents — bb0=[setup, cross_call], post_call=[mid, Yield],
    // tail_yield=[post]. resume_check is synthetic and empty.
    assert_eq!(cfg.statement_count(bb0), 2);
    assert_eq!(cfg.statement_count(post_call), 2);
    assert_eq!(cfg.statement_count(tail_yield), 1);
    assert_eq!(cfg.statement_count(resume_check), 0);
}

#[test]
fn direct_yield_then_cross_fn_in_same_block() {
    // bb0 = [Yield, mid, cross_call, post]. Direct-yield split first
    // moves [mid, cross_call, post] into tail_y; cross-fn split moves
    // [post] into post_call.
    let mut cfg = ToyCfg::new();
    let bb0 = cfg.push_block(vec![
        ToyStmt::Yield,
        ToyStmt::User("mid"),
        ToyStmt::User("cross_call"),
        ToyStmt::User("post"),
    ]);

    let suspending = ToySuspending {
        suspending: HashSet::from([ToyFnId(1)]),
        yields: HashSet::new(),
    };
    let hooks = ScriptedHooks {
        sites: vec![
            (bb0, 0, SuspensionSite::DirectYield { value: None }),
            (
                bb0,
                2,
                SuspensionSite::CrossFnCall {
                    callee: ToyFnId(99),
                    receiver: None,
                    args: vec![],
                    result: ToyValueId(0),
                },
            ),
        ],
    };

    let layout = transform_to_state_machine(
        &mut cfg,
        ToyFnId(1),
        &suspending,
        &hooks,
        &LivenessMap::new(),
    )
    .unwrap();

    // resume_entries: [bb0, tail_y, resume_check].
    assert_eq!(layout.resume_entries.len(), 3);
    let tail_y = layout.resume_entries[1];
    let resume_check = layout.resume_entries[2];

    // yield_blocks: (bb0 → 1, tail_y → 2). The cross-fn's Init lives
    // on tail_y, not bb0.
    assert_eq!(layout.yield_blocks, vec![(bb0, 1), (tail_y, 2)]);

    // First entry: bb0 → DirectYield.
    assert_eq!(layout.block_kinds[0].0, bb0);
    assert!(matches!(layout.block_kinds[0].1, BlockKind::DirectYield));

    // Second entry: tail_y → CrossFnCallInit. Pull resume_check_block
    // out and confirm it matches resume_entries[2].
    match &layout.block_kinds[1] {
        (b, BlockKind::CrossFnCallInit { resume_check_block, .. }) => {
            assert_eq!(*b, tail_y);
            assert_eq!(*resume_check_block, resume_check);
        }
        other => panic!("expected (tail_y, CrossFnCallInit), got {other:?}"),
    }

    // Third entry: resume_check → CrossFnCallResume{done_block=post_call}.
    let post_call = match &layout.block_kinds[2] {
        (b, BlockKind::CrossFnCallResume { done_block, .. }) => {
            assert_eq!(*b, resume_check);
            *done_block
        }
        other => panic!("expected (resume_check, CrossFnCallResume), got {other:?}"),
    };

    // Contents: bb0=[Yield], tail_y=[mid, cross_call],
    // post_call=[post], resume_check=[].
    assert_eq!(cfg.statement_count(bb0), 1);
    assert_eq!(cfg.statement_count(tail_y), 2);
    assert_eq!(cfg.statement_count(post_call), 1);
    assert_eq!(cfg.statement_count(resume_check), 0);
}

#[test]
fn two_cross_fn_calls_in_same_block_each_get_their_own_resume_check() {
    // bb0 = [a, call_1, b, call_2, c]. Two cross-fn splits:
    // first creates post_call_1 + resume_check_1; second splits
    // post_call_1 into post_call_1=[b, call_2] + post_call_2=[c],
    // adds resume_check_2.
    let mut cfg = ToyCfg::new();
    let bb0 = cfg.push_block(vec![
        ToyStmt::User("a"),
        ToyStmt::User("call_1"),
        ToyStmt::User("b"),
        ToyStmt::User("call_2"),
        ToyStmt::User("c"),
    ]);

    let suspending = ToySuspending {
        suspending: HashSet::from([ToyFnId(1)]),
        yields: HashSet::new(),
    };
    let hooks = ScriptedHooks {
        sites: vec![
            (
                bb0,
                1,
                SuspensionSite::CrossFnCall {
                    callee: ToyFnId(7),
                    receiver: None,
                    args: vec![ToyValueId(70)],
                    result: ToyValueId(71),
                },
            ),
            (
                bb0,
                3,
                SuspensionSite::CrossFnCall {
                    callee: ToyFnId(8),
                    receiver: None,
                    args: vec![ToyValueId(80)],
                    result: ToyValueId(81),
                },
            ),
        ],
    };

    let layout = transform_to_state_machine(
        &mut cfg,
        ToyFnId(1),
        &suspending,
        &hooks,
        &LivenessMap::new(),
    )
    .unwrap();

    // 3 resume entries (entry, resume_check_1, resume_check_2).
    assert_eq!(layout.resume_entries.len(), 3);
    let resume_check_1 = layout.resume_entries[1];
    let resume_check_2 = layout.resume_entries[2];

    // 4 block_kinds entries: Init1, Resume1, Init2, Resume2.
    assert_eq!(layout.block_kinds.len(), 4);

    // Init1 at bb0, points at resume_check_1.
    let post_call_1 = match &layout.block_kinds[0] {
        (b, BlockKind::CrossFnCallInit { resume_check_block, callee, .. }) => {
            assert_eq!(*b, bb0);
            assert_eq!(*resume_check_block, resume_check_1);
            assert_eq!(*callee, ToyFnId(7));
            // Resume1 carries the matching done_block.
            match &layout.block_kinds[1] {
                (rb, BlockKind::CrossFnCallResume { done_block, callee, .. }) => {
                    assert_eq!(*rb, resume_check_1);
                    assert_eq!(*callee, ToyFnId(7));
                    *done_block
                }
                other => panic!("expected Resume1, got {other:?}"),
            }
        }
        other => panic!("expected Init1, got {other:?}"),
    };

    // Init2 sits on post_call_1.
    match &layout.block_kinds[2] {
        (b, BlockKind::CrossFnCallInit { resume_check_block, callee, .. }) => {
            assert_eq!(*b, post_call_1);
            assert_eq!(*resume_check_block, resume_check_2);
            assert_eq!(*callee, ToyFnId(8));
        }
        other => panic!("expected Init2 at post_call_1, got {other:?}"),
    }
    let post_call_2 = match &layout.block_kinds[3] {
        (b, BlockKind::CrossFnCallResume { done_block, callee, .. }) => {
            assert_eq!(*b, resume_check_2);
            assert_eq!(*callee, ToyFnId(8));
            *done_block
        }
        other => panic!("expected Resume2, got {other:?}"),
    };

    // Yield-blocks attribute Init1's yield to bb0, Init2's yield to
    // post_call_1.
    assert_eq!(layout.yield_blocks, vec![(bb0, 1), (post_call_1, 2)]);

    // Block contents — bb0=[a, call_1], post_call_1=[b, call_2],
    // post_call_2=[c]. resume_check blocks empty.
    assert_eq!(cfg.statement_count(bb0), 2);
    assert_eq!(cfg.statement_count(post_call_1), 2);
    assert_eq!(cfg.statement_count(post_call_2), 1);
    assert_eq!(cfg.statement_count(resume_check_1), 0);
    assert_eq!(cfg.statement_count(resume_check_2), 0);
}

#[test]
fn multi_suspension_with_liveness_uses_correct_yield_and_resume_blocks() {
    // Two yields in one block, each with a distinct live value.
    // Verify the save lands at the *current* yielding block (not
    // the original bb0 for the second yield) and the load at the
    // matching resume entry.
    let mut cfg = ToyCfg::new();
    let bb0 = cfg.push_block(vec![
        ToyStmt::User("a"),
        ToyStmt::Yield,
        ToyStmt::User("b"),
        ToyStmt::Yield,
    ]);

    let suspending = ToySuspending {
        suspending: HashSet::from([ToyFnId(1)]),
        yields: HashSet::new(),
    };

    // Liveness keyed on original (block, idx).
    let mut liveness = LivenessMap::new();
    liveness.record(bb0, 1, vec![ToyValueId(10)]);
    liveness.record(bb0, 3, vec![ToyValueId(20)]);

    let layout = transform_to_state_machine(
        &mut cfg,
        ToyFnId(1),
        &suspending,
        &ToyHooks,
        &liveness,
    )
    .unwrap();

    let tail_a = layout.resume_entries[1];
    let tail_b = layout.resume_entries[2];

    // First yield's save sits on bb0; load on tail_a.
    assert_eq!(layout.yield_saves[0], (bb0, vec![(0, ToyValueId(10))]));
    assert_eq!(layout.resume_loads[0], (tail_a, vec![(0, ToyValueId(10))]));
    // Second yield's save sits on tail_a (NOT bb0); load on tail_b.
    assert_eq!(layout.yield_saves[1], (tail_a, vec![(1, ToyValueId(20))]));
    assert_eq!(layout.resume_loads[1], (tail_b, vec![(1, ToyValueId(20))]));
}

#[test]
fn multi_suspension_across_blocks_keeps_per_block_state() {
    // Two original blocks, each with two yields. The current_block
    // tracking must reset when crossing block boundaries; otherwise
    // the second block's first yield would inherit stale idx state.
    let mut cfg = ToyCfg::new();
    let bb_a = cfg.push_block(vec![
        ToyStmt::User("a0"),
        ToyStmt::Yield,
        ToyStmt::User("a1"),
        ToyStmt::Yield,
    ]);
    let bb_b = cfg.push_block(vec![
        ToyStmt::User("b0"),
        ToyStmt::Yield,
        ToyStmt::User("b1"),
        ToyStmt::Yield,
    ]);

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

    // 4 yields → 5 resume entries, 4 yield blocks numbered 1..=4.
    assert_eq!(layout.resume_entries.len(), 5);
    assert_eq!(layout.yield_blocks.len(), 4);
    for (i, (_, state)) in layout.yield_blocks.iter().enumerate() {
        assert_eq!(*state, (i + 1) as u32);
    }

    // First yield in each block lives on the original bb_a / bb_b;
    // second yield lives on the tail block created by the first split.
    assert_eq!(layout.yield_blocks[0].0, bb_a);
    assert_eq!(layout.yield_blocks[1].0, layout.resume_entries[1]);
    assert_eq!(layout.yield_blocks[2].0, bb_b);
    assert_eq!(layout.yield_blocks[3].0, layout.resume_entries[3]);
}
