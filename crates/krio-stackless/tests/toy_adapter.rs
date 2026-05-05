//! Toy CFG IR + adapter — validates the krio trait surface against a
//! non-Zura IR. This is the "second consumer" Phase 2's API design
//! was supposed to pressure-test against. If a piece of behaviour is
//! awkward here, it's almost certainly leaking from the Zura adapter
//! into the trait surface.
//!
//! The toy IR is intentionally minimal: just enough to:
//!   - Build a hand-rolled CFG with marker statements.
//!   - Run krio against it via `run_with`.
//!   - Inspect the output structurally.
//!
//! There is no execution engine — these tests assert *shape*.

use std::collections::BTreeMap;

use krio_stackless::cfg::{CoroCfg, CoroHooks, Marker};
use krio_stackless::executor::{CooperativeExecutor, WakerExecutor};

// ── Toy IR ────────────────────────────────────────────────────────

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
struct ToyBlockId(u32);

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
struct ToyLocalId(u32);

#[derive(Debug, Clone, Copy)]
enum LocalTy {
    I64Mut,
    Bool,
    BoolMut,
}

#[derive(Debug, Clone)]
enum ToyStmt {
    /// Marker statement — these drive the krio transform.
    Marker(Marker),
    /// Generic "user statement" — krio doesn't touch these.
    User(&'static str),
    AssignI64(ToyLocalId, i64),
    AssignBool(ToyLocalId, bool),
    EqCheckI64 {
        dest: ToyLocalId,
        lhs: ToyLocalId,
        rhs: i64,
    },
    Nop,
}

#[derive(Debug, Clone)]
enum ToyTerm {
    Unreachable,
    Goto(ToyBlockId),
    Branch {
        cond: ToyLocalId,
        t: ToyBlockId,
        f: ToyBlockId,
    },
    Switch {
        discr: ToyLocalId,
        targets: Vec<(i64, ToyBlockId)>,
        otherwise: ToyBlockId,
    },
    Return,
}

#[derive(Debug, Default)]
struct ToyBody {
    blocks: Vec<(Vec<ToyStmt>, ToyTerm)>,
    locals: Vec<LocalTy>,
}

impl ToyBody {
    fn new() -> Self {
        ToyBody::default()
    }

    fn push_block(&mut self, stmts: Vec<ToyStmt>, term: ToyTerm) -> ToyBlockId {
        let id = ToyBlockId(self.blocks.len() as u32);
        self.blocks.push((stmts, term));
        id
    }

    fn block(&self, id: ToyBlockId) -> &(Vec<ToyStmt>, ToyTerm) {
        &self.blocks[id.0 as usize]
    }
}

// ── CoroCfg impl over ToyBody ──────────────────────────────────────

impl CoroCfg for ToyBody {
    type BlockId = ToyBlockId;
    type LocalId = ToyLocalId;

    fn block_count(&self) -> usize {
        self.blocks.len()
    }

    fn statement_count(&self, bb: ToyBlockId) -> usize {
        self.blocks[bb.0 as usize].0.len()
    }

    fn block_ids(&self) -> Vec<ToyBlockId> {
        (0..self.blocks.len() as u32).map(ToyBlockId).collect()
    }

    fn new_block(&mut self) -> ToyBlockId {
        let id = ToyBlockId(self.blocks.len() as u32);
        self.blocks.push((Vec::new(), ToyTerm::Unreachable));
        id
    }

    fn new_state_local(&mut self) -> ToyLocalId {
        let id = ToyLocalId(self.locals.len() as u32);
        self.locals.push(LocalTy::I64Mut);
        id
    }

    fn new_bool_local(&mut self) -> ToyLocalId {
        let id = ToyLocalId(self.locals.len() as u32);
        self.locals.push(LocalTy::Bool);
        id
    }

    fn new_mut_bool_local(&mut self) -> ToyLocalId {
        let id = ToyLocalId(self.locals.len() as u32);
        self.locals.push(LocalTy::BoolMut);
        id
    }

    fn emit_assign_i64(&mut self, bb: ToyBlockId, local: ToyLocalId, value: i64) {
        self.blocks[bb.0 as usize]
            .0
            .push(ToyStmt::AssignI64(local, value));
    }

    fn emit_assign_bool(&mut self, bb: ToyBlockId, local: ToyLocalId, value: bool) {
        self.blocks[bb.0 as usize]
            .0
            .push(ToyStmt::AssignBool(local, value));
    }

    fn emit_eq_check_i64(
        &mut self,
        bb: ToyBlockId,
        dest: ToyLocalId,
        lhs: ToyLocalId,
        rhs: i64,
    ) {
        self.blocks[bb.0 as usize]
            .0
            .push(ToyStmt::EqCheckI64 { dest, lhs, rhs });
    }

    fn replace_with_nop(&mut self, bb: ToyBlockId, idx: usize) {
        self.blocks[bb.0 as usize].0[idx] = ToyStmt::Nop;
    }

    fn split_after(&mut self, src: ToyBlockId, idx: usize) -> ToyBlockId {
        let src_idx = src.0 as usize;
        let tail: Vec<ToyStmt> = self.blocks[src_idx].0.drain((idx + 1)..).collect();
        let term = std::mem::replace(&mut self.blocks[src_idx].1, ToyTerm::Unreachable);
        let id = ToyBlockId(self.blocks.len() as u32);
        self.blocks.push((tail, term));
        id
    }

    fn prepend_assign_i64(&mut self, bb: ToyBlockId, local: ToyLocalId, value: i64) {
        self.blocks[bb.0 as usize]
            .0
            .insert(0, ToyStmt::AssignI64(local, value));
    }

    fn set_goto(&mut self, bb: ToyBlockId, target: ToyBlockId) {
        self.blocks[bb.0 as usize].1 = ToyTerm::Goto(target);
    }

    fn set_branch(
        &mut self,
        bb: ToyBlockId,
        cond: ToyLocalId,
        true_bb: ToyBlockId,
        false_bb: ToyBlockId,
    ) {
        self.blocks[bb.0 as usize].1 = ToyTerm::Branch {
            cond,
            t: true_bb,
            f: false_bb,
        };
    }

    fn set_switch(
        &mut self,
        bb: ToyBlockId,
        discr: ToyLocalId,
        targets: Vec<(i64, ToyBlockId)>,
        otherwise: ToyBlockId,
    ) {
        self.blocks[bb.0 as usize].1 = ToyTerm::Switch {
            discr,
            targets,
            otherwise,
        };
    }

    fn redirect_targets(
        &mut self,
        bb: ToyBlockId,
        from: ToyBlockId,
        to: ToyBlockId,
    ) {
        let term = &mut self.blocks[bb.0 as usize].1;
        match term {
            ToyTerm::Goto(t) if *t == from => *t = to,
            ToyTerm::Branch { t, f, .. } => {
                if *t == from {
                    *t = to;
                }
                if *f == from {
                    *f = to;
                }
            }
            ToyTerm::Switch {
                targets, otherwise, ..
            } => {
                for (_, b) in targets {
                    if *b == from {
                        *b = to;
                    }
                }
                if *otherwise == from {
                    *otherwise = to;
                }
            }
            _ => {}
        }
    }
}

// ── Hooks ──────────────────────────────────────────────────────────

struct ToyHooks;

impl CoroHooks for ToyHooks {
    type Cfg = ToyBody;

    fn classify_marker(
        &self,
        cfg: &ToyBody,
        bb: ToyBlockId,
        idx: usize,
    ) -> Option<Marker> {
        match &cfg.blocks[bb.0 as usize].0[idx] {
            ToyStmt::Marker(m) => Some(*m),
            _ => None,
        }
    }

    fn emit_guarded_recv_peek(
        &mut self,
        _cfg: &mut ToyBody,
        _bb: ToyBlockId,
        _idx: usize,
        _resume_bb: ToyBlockId,
    ) -> ToyLocalId {
        // Toy IR has no channel concept — these tests don't exercise
        // GuardedRecv. If they did, the hook would extract the
        // channel operand from the marker statement, prepend a peek-
        // is-ready check, and move the recv into resume_bb.
        unreachable!("toy adapter doesn't emit GuardedRecv markers");
    }
}

// ── Helpers ────────────────────────────────────────────────────────

fn build_simple_region() -> ToyBody {
    // Real IR builders emit each coroutine boundary into its own
    // block, AND assign block IDs in source/control-flow order —
    // krio's region-discovery scan iterates `block_ids()` in that
    // order. The toy test mirrors that layout, building blocks
    // top-down then patching forward references via redirect.
    //
    //   bb0: RegionBegin; CoroutineBegin    -> goto bb1
    //   bb1: User("c1.s1"); Yield           -> goto bb2
    //   bb2: User("c1.s2"); CoroutineEnd    -> goto bb3
    //   bb3: CoroutineBegin                 -> goto bb4
    //   bb4: User("c2.s1"); CoroutineEnd    -> goto bb5
    //   bb5: RegionEnd                      -> goto bb6
    //   bb6: User("post-region"); return
    //
    // Build by allocating empty blocks first, then filling. That's
    // exactly how the Zura MIR builder handles forward edges.
    let mut body = ToyBody::new();
    let bb_region_begin = body.push_block(vec![], ToyTerm::Unreachable); // 0
    let bb_c1_yield = body.push_block(vec![], ToyTerm::Unreachable); // 1
    let bb_c1_end = body.push_block(vec![], ToyTerm::Unreachable); // 2
    let bb_c2_begin = body.push_block(vec![], ToyTerm::Unreachable); // 3
    let bb_c2_end = body.push_block(vec![], ToyTerm::Unreachable); // 4
    let bb_region_end = body.push_block(vec![], ToyTerm::Unreachable); // 5
    let bb_post = body.push_block(vec![], ToyTerm::Unreachable); // 6

    body.blocks[bb_region_begin.0 as usize] = (
        vec![
            ToyStmt::Marker(Marker::RegionBegin),
            ToyStmt::Marker(Marker::CoroutineBegin),
        ],
        ToyTerm::Goto(bb_c1_yield),
    );
    body.blocks[bb_c1_yield.0 as usize] = (
        vec![
            ToyStmt::User("c1.s1"),
            ToyStmt::Marker(Marker::Yield),
        ],
        ToyTerm::Goto(bb_c1_end),
    );
    body.blocks[bb_c1_end.0 as usize] = (
        vec![
            ToyStmt::User("c1.s2"),
            ToyStmt::Marker(Marker::CoroutineEnd),
        ],
        ToyTerm::Goto(bb_c2_begin),
    );
    body.blocks[bb_c2_begin.0 as usize] = (
        vec![ToyStmt::Marker(Marker::CoroutineBegin)],
        ToyTerm::Goto(bb_c2_end),
    );
    body.blocks[bb_c2_end.0 as usize] = (
        vec![
            ToyStmt::User("c2.s1"),
            ToyStmt::Marker(Marker::CoroutineEnd),
        ],
        ToyTerm::Goto(bb_region_end),
    );
    body.blocks[bb_region_end.0 as usize] = (
        vec![ToyStmt::Marker(Marker::RegionEnd)],
        ToyTerm::Goto(bb_post),
    );
    body.blocks[bb_post.0 as usize] = (
        vec![ToyStmt::User("post-region")],
        ToyTerm::Return,
    );
    body
}

fn count_markers(body: &ToyBody) -> BTreeMap<&'static str, usize> {
    let mut counts: BTreeMap<&'static str, usize> = BTreeMap::new();
    for (stmts, _) in &body.blocks {
        for s in stmts {
            if let ToyStmt::Marker(m) = s {
                let key = match m {
                    Marker::RegionBegin => "region_begin",
                    Marker::RegionEnd => "region_end",
                    Marker::CoroutineBegin => "coroutine_begin",
                    Marker::CoroutineEnd => "coroutine_end",
                    Marker::Yield => "yield",
                    Marker::GuardedRecv => "guarded_recv",
                    Marker::ProducingSend => "producing_send",
                };
                *counts.entry(key).or_default() += 1;
            }
        }
    }
    counts
}

fn has_switch_terminator(body: &ToyBody) -> bool {
    body.blocks
        .iter()
        .any(|(_, t)| matches!(t, ToyTerm::Switch { .. }))
}

// ── Tests ──────────────────────────────────────────────────────────

#[test]
fn toy_adapter_round_trip() {
    // A region with two coroutines and one yield in the first
    // coroutine. After the transform, every marker is gone, a
    // switch dispatch exists, and the region's "after" target
    // is reachable via the executor exit.
    let mut body = build_simple_region();
    let before_blocks = body.blocks.len();

    let mut hooks = ToyHooks;
    let mut exec = CooperativeExecutor;
    krio_stackless::run_with(&mut body, &mut hooks, &mut exec);

    assert!(
        body.blocks.len() > before_blocks,
        "expected new blocks after transform, had {} now {}",
        before_blocks,
        body.blocks.len()
    );

    let markers = count_markers(&body);
    assert!(
        markers.is_empty(),
        "expected all markers erased, found {markers:?}"
    );

    assert!(
        has_switch_terminator(&body),
        "expected a Switch terminator from the dispatch block"
    );
}

#[test]
fn toy_adapter_idempotent() {
    let mut body = build_simple_region();
    let mut hooks = ToyHooks;
    let mut exec = CooperativeExecutor;
    krio_stackless::run_with(&mut body, &mut hooks, &mut exec);
    let after_first = body.blocks.len();

    krio_stackless::run_with(&mut body, &mut hooks, &mut exec);
    assert_eq!(
        body.blocks.len(),
        after_first,
        "second run on already-transformed body should be a no-op"
    );
}

#[test]
fn toy_adapter_no_region_passthrough() {
    // No region markers → transform is a no-op.
    let mut body = ToyBody::new();
    body.push_block(
        vec![ToyStmt::User("a"), ToyStmt::User("b")],
        ToyTerm::Return,
    );
    let before = body.blocks.clone().len();
    let mut hooks = ToyHooks;
    let mut exec = CooperativeExecutor;
    krio_stackless::run_with(&mut body, &mut hooks, &mut exec);
    assert_eq!(body.blocks.len(), before);
}

// ── WakerExecutor tests ───────────────────────────────────────────

#[test]
fn waker_executor_records_one_region_pair() {
    // After the transform runs with WakerExecutor, the executor
    // should have one (done, pending) BlockId pair recorded —
    // that's how the host wires its waker plumbing.
    let mut body = build_simple_region();
    let mut hooks = ToyHooks;
    let mut exec = WakerExecutor::<ToyBlockId>::new();
    krio_stackless::run_with(&mut body, &mut hooks, &mut exec);

    assert_eq!(
        exec.regions.len(),
        1,
        "expected one RegionExits recorded, got {}",
        exec.regions.len()
    );
    let exits = &exec.regions[0];
    assert_ne!(
        exits.done, exits.pending,
        "done and pending must be distinct blocks"
    );
}

#[test]
fn waker_executor_erases_markers_too() {
    // WakerExecutor still erases all markers, same as Cooperative.
    let mut body = build_simple_region();
    let mut hooks = ToyHooks;
    let mut exec = WakerExecutor::<ToyBlockId>::new();
    krio_stackless::run_with(&mut body, &mut hooks, &mut exec);

    let markers = count_markers(&body);
    assert!(
        markers.is_empty(),
        "expected all markers erased, found {markers:?}"
    );
}

#[test]
fn waker_executor_no_loop_back() {
    // The cooperative executor's loop body branches from the final
    // all_done check back to its `loop_top`. WakerExecutor branches
    // to `region_pending` instead — and `region_pending`'s default
    // terminator is whatever the toy adapter installed for a fresh
    // block (Unreachable). Verify region_pending is reachable AND
    // unreachable-terminated (consumer's job to wire it up).
    let mut body = build_simple_region();
    let mut hooks = ToyHooks;
    let mut exec = WakerExecutor::<ToyBlockId>::new();
    krio_stackless::run_with(&mut body, &mut hooks, &mut exec);

    let pending_bb = exec.regions[0].pending;
    let pending_term = &body.blocks[pending_bb.0 as usize].1;
    assert!(
        matches!(pending_term, ToyTerm::Unreachable),
        "region_pending should default to Unreachable until host wires it up; got {pending_term:?}"
    );
}

#[test]
fn waker_executor_done_path_reaches_post_region() {
    // The `done` exit should chain into the original post-region
    // path (the toy fixture has "post-region" as the after-region
    // user code). Following gotos from done should eventually land
    // on a block whose first statement is User("post-region").
    let mut body = build_simple_region();
    let mut hooks = ToyHooks;
    let mut exec = WakerExecutor::<ToyBlockId>::new();
    krio_stackless::run_with(&mut body, &mut hooks, &mut exec);

    let mut current = exec.regions[0].done;
    let mut hops = 0;
    let landed = loop {
        hops += 1;
        assert!(hops < 10, "too many hops chasing done's goto");
        let (stmts, term) = &body.blocks[current.0 as usize];
        let has_post = stmts.iter().any(|s| matches!(s, ToyStmt::User(name) if *name == "post-region"));
        if has_post {
            break true;
        }
        match term {
            ToyTerm::Goto(next) => current = *next,
            _ => break false,
        }
    };
    assert!(landed, "done exit should chain to the post-region path");
}
