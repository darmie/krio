//! krio — MIR→MIR state-machine transform for stackless coroutines.
//!
//! Lowers a CFG annotated with cooperative-concurrency markers into a
//! plain CFG with no suspension semantics left: each coroutine becomes
//! a switch-on-state machine, and the surrounding region becomes an
//! executor loop that polls each coroutine until they all report
//! Ready. Everything stays in one stack frame — no heap allocation,
//! no ABI changes, no closure environment.
//!
//! ## What the input looks like
//!
//! The transform expects two kinds of marker statements in the CFG:
//!
//! ```text
//! region_begin                    // open a concurrency region
//!   coroutine_begin               // open coroutine #1
//!     ... user statements ...
//!     suspend                     // a yield point
//!     ... more user statements ...
//!   coroutine_end
//!   coroutine_begin               // open coroutine #2
//!     ...
//!   coroutine_end
//! region_end
//! ```
//!
//! Suspension points come in three flavours:
//!
//! - unconditional yield — always suspends.
//! - guarded recv (queue-style) — suspends if the channel is empty,
//!   otherwise consumes the value.
//! - producing send — runs the send, then yields once so any
//!   consumer gets a turn.
//!
//! ## What the output looks like
//!
//! Per coroutine:
//!
//! ```text
//! state_N : i64 mut               // 0 = entry, 1..K resume points, DONE = done
//! poll_N  : i64 mut               // 0 = Ready, 1 = Pending
//!
//! dispatch_N:                     // entry — switch on state_N
//!   switch state_N {
//!     0    -> entry_block
//!     1    -> resume_block_1
//!     ...
//!     K    -> resume_block_K
//!     DONE -> exit_N
//!   }
//! ```
//!
//! At each old suspend site, statements after the suspend move into a
//! fresh `resume_block_i`, and the suspend itself becomes:
//!
//! ```text
//! state_N = i
//! poll_N  = Pending
//! goto exit_N
//! ```
//!
//! At `coroutine_end`:
//!
//! ```text
//! poll_N  = Ready
//! state_N = DONE
//! goto exit_N
//! ```
//!
//! Per region (the cooperative executor loop):
//!
//! ```text
//! loop_top:
//!   all_done = true
//!   for each coroutine N {
//!     if state_N == DONE { skip }
//!     else {
//!       goto dispatch_N           // run a turn
//!       // exit_N falls back here
//!       if poll_N == Ready { state_N = DONE }
//!       else               { all_done = false }
//!     }
//!   }
//!   if all_done { goto after_region }
//!   else        { goto loop_top }
//! ```
//!
//! The original markers are replaced with `Nop` and the spawn-body
//! statements move into their own block so the executor can dispatch
//! each coroutine independently.
//!
//! ## Why guarded recv is special
//!
//! Unlike a plain `yield`, the CFG can't know at compile time whether
//! a recv will block. The transform splits the recv into two parts:
//! a peek (`is_ready` predicate) followed by either a real recv (if
//! ready) or a yield Pending (if not). On the next turn the dispatch
//! re-enters the peek and tries again.
//!
//! ## Where this fits in the krio family
//!
//! `krio-stackless` is the per-function state-machine engine — the
//! lowest-level primitive in the krio family. Other variants reuse or
//! extend it:
//!
//! - `krio-async`: cross-function suspension via function-colour
//!   propagation, captures-as-fields, caller↔callee state composition.
//!   Builds on top of this crate.
//! - `krio-fiber`: stackful Wren-style fibers. Different model — a
//!   runtime, not a transform — so it doesn't share the algorithm,
//!   only the shared vocabulary in `krio-core`.
//! - `krio-preempt`: preemptive scheduler over `krio-fiber`.
//!
//! All four crates speak the same `Marker` / `Suspension` /
//! `CfgId` vocabulary defined in `krio-core`.

pub mod cfg;
pub mod executor;

pub use cfg::{CfgId, CoroCfg, CoroHooks, Marker};
pub use executor::{CooperativeExecutor, Executor, RegionExits, WakerExecutor};

const DONE_STATE: i64 = 9999;

/// Coordinates the algorithm has discovered for a region or coroutine.
/// Used internally; surfaced for advanced consumers who want to drive
/// their own executor.
#[derive(Debug)]
pub struct Region<B: CfgId> {
    pub region_begin: (B, usize),
    pub region_end: (B, usize),
    pub coroutines: Vec<Coroutine<B>>,
}

#[derive(Debug)]
pub struct Coroutine<B: CfgId> {
    pub begin: (B, usize),
    pub end: (B, usize),
}

/// Per-coroutine state machine — the locals + dispatch/exit blocks
/// the cooperative executor (or any future executor) will wire up.
#[derive(Debug)]
pub struct Machine<B: CfgId, L: CfgId> {
    pub state_local: L,
    pub poll_result_local: L,
    pub dispatch_bb: B,
    pub exit_bb: B,
}

/// Internal suspension-kind tag carried alongside each detected
/// suspension site. Exposed `pub` so `find_suspension_points` can be
/// called by advanced consumers, but most users will only see it via
/// `Marker` at the `CoroHooks` layer.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SuspKind {
    Yield,
    GuardedRecv,
    ProducingSend,
}

// ── Public entry points ────────────────────────────────────────────

/// Run the cooperative coroutine transform against an arbitrary
/// CFG-shaped IR. Repeatedly finds and rewrites one region at a time
/// until none remain (so nested regions inside coroutines work).
pub fn run_with<C, H, E>(cfg: &mut C, hooks: &mut H, executor: &mut E)
where
    C: CoroCfg,
    H: CoroHooks<Cfg = C>,
    E: Executor<C>,
{
    loop {
        let regions = find_regions(cfg, hooks);
        let next = regions.into_iter().find(|r| !r.coroutines.is_empty());
        match next {
            Some(region) => transform_region(cfg, hooks, executor, &region),
            None => break,
        }
    }
}

// ── Region discovery ───────────────────────────────────────────────
//
// Region = the boundary marked by RegionBegin..RegionEnd. Inside it
// lives one or more coroutines. The scan returns regions in CFG
// order; the order within a region matches source.

pub fn find_regions<C, H>(cfg: &C, hooks: &H) -> Vec<Region<C::BlockId>>
where
    C: CoroCfg,
    H: CoroHooks<Cfg = C>,
{
    let mut regions = Vec::new();
    let mut current_region: Option<(C::BlockId, usize)> = None;
    let mut current_coroutines: Vec<Coroutine<C::BlockId>> = Vec::new();
    let mut current_coro_begin: Option<(C::BlockId, usize)> = None;

    for bb in cfg.block_ids() {
        for stmt_idx in 0..cfg.statement_count(bb) {
            match hooks.classify_marker(cfg, bb, stmt_idx) {
                Some(Marker::RegionBegin) => {
                    current_region = Some((bb, stmt_idx));
                    current_coroutines = Vec::new();
                }
                Some(Marker::RegionEnd) => {
                    if let Some(begin) = current_region.take() {
                        regions.push(Region {
                            region_begin: begin,
                            region_end: (bb, stmt_idx),
                            coroutines: std::mem::take(&mut current_coroutines),
                        });
                    }
                }
                Some(Marker::CoroutineBegin) => {
                    current_coro_begin = Some((bb, stmt_idx));
                }
                Some(Marker::CoroutineEnd) => {
                    if let Some(begin) = current_coro_begin.take() {
                        current_coroutines.push(Coroutine {
                            begin,
                            end: (bb, stmt_idx),
                        });
                    }
                }
                _ => {}
            }
        }
    }

    regions
}

// ── Suspension point detection ─────────────────────────────────────
//
// Suspension points are statements that may give up control: yield,
// guarded recv, producing send. The list is in CFG order so state
// IDs can be assigned 1..=K.

pub fn find_suspension_points<C, H>(
    cfg: &C,
    hooks: &H,
    coroutine: &Coroutine<C::BlockId>,
) -> Vec<(C::BlockId, usize, SuspKind)>
where
    C: CoroCfg,
    H: CoroHooks<Cfg = C>,
{
    let mut points = Vec::new();
    let begin = coroutine.begin.0;
    let end = coroutine.end.0;
    for bb in cfg.block_ids() {
        // Skip blocks before the coroutine's begin block.
        if bb < begin {
            continue;
        }
        // Stop once we're past the coroutine's end block.
        if bb > end {
            break;
        }
        for idx in 0..cfg.statement_count(bb) {
            match hooks.classify_marker(cfg, bb, idx) {
                Some(Marker::Yield) => points.push((bb, idx, SuspKind::Yield)),
                Some(Marker::GuardedRecv) => points.push((bb, idx, SuspKind::GuardedRecv)),
                Some(Marker::ProducingSend) => points.push((bb, idx, SuspKind::ProducingSend)),
                _ => {}
            }
        }
    }
    points
}

// ── Top-level transform ────────────────────────────────────────────

fn transform_region<C, H, E>(
    cfg: &mut C,
    hooks: &mut H,
    executor: &mut E,
    region: &Region<C::BlockId>,
) where
    C: CoroCfg,
    H: CoroHooks<Cfg = C>,
    E: Executor<C>,
{
    // Snapshot each coroutine's suspension points before mutating —
    // adding blocks invalidates any later scan.
    let coro_suspensions: Vec<Vec<(C::BlockId, usize, SuspKind)>> = region
        .coroutines
        .iter()
        .map(|c| find_suspension_points(cfg, hooks, c))
        .collect();

    let mut machines: Vec<Machine<C::BlockId, C::LocalId>> = Vec::new();

    for (idx, coroutine) in region.coroutines.iter().enumerate() {
        let suspensions = &coro_suspensions[idx];
        let machine = build_state_machine(cfg, hooks, coroutine, suspensions);
        machines.push(machine);
    }

    // Hand off to the executor strategy.
    executor.finalize_region(cfg, region, &machines);
}

fn build_state_machine<C, H>(
    cfg: &mut C,
    hooks: &mut H,
    coroutine: &Coroutine<C::BlockId>,
    suspensions: &[(C::BlockId, usize, SuspKind)],
) -> Machine<C::BlockId, C::LocalId>
where
    C: CoroCfg,
    H: CoroHooks<Cfg = C>,
{
    // Allocate the two coroutine locals: state + poll_result.
    let state_local = cfg.new_state_local();
    let poll_result_local = cfg.new_state_local();

    let dispatch_bb = cfg.new_block();
    let exit_bb = cfg.new_block();

    if suspensions.is_empty() {
        emit_no_suspension_machine(
            cfg,
            coroutine,
            state_local,
            poll_result_local,
            dispatch_bb,
            exit_bb,
        );
    } else {
        emit_multi_state_machine(
            cfg,
            hooks,
            coroutine,
            suspensions,
            state_local,
            poll_result_local,
            dispatch_bb,
            exit_bb,
        );
    }

    Machine {
        state_local,
        poll_result_local,
        dispatch_bb,
        exit_bb,
    }
}

/// No suspension points — the coroutine runs to completion on its
/// first poll. Dispatch:
///   if state == DONE -> exit
///   else             -> coroutine entry
fn emit_no_suspension_machine<C: CoroCfg>(
    cfg: &mut C,
    coroutine: &Coroutine<C::BlockId>,
    state_local: C::LocalId,
    poll_result_local: C::LocalId,
    dispatch_bb: C::BlockId,
    exit_bb: C::BlockId,
) {
    let coro_entry_bb = coroutine.begin.0;

    // Done block: poll = Ready, state = DONE, goto exit.
    let done_bb = cfg.new_block();
    cfg.emit_assign_i64(done_bb, poll_result_local, 0);
    cfg.emit_assign_i64(done_bb, state_local, DONE_STATE);
    cfg.set_goto(done_bb, exit_bb);

    // Dispatch: is_done = (state == DONE); branch.
    let check_done = cfg.new_bool_local();
    cfg.emit_eq_check_i64(dispatch_bb, check_done, state_local, DONE_STATE);
    cfg.set_branch(dispatch_bb, check_done, exit_bb, coro_entry_bb);

    // The block holding `coroutine_end` falls through to wherever
    // the next coroutine (or region_end) lives — but we want it to
    // feed the executor's post-poll path. Retarget it.
    cfg.set_goto(coroutine.end.0, done_bb);
}

/// K suspension points -> multi-state machine.
#[allow(clippy::too_many_arguments)]
fn emit_multi_state_machine<C, H>(
    cfg: &mut C,
    hooks: &mut H,
    coroutine: &Coroutine<C::BlockId>,
    suspensions: &[(C::BlockId, usize, SuspKind)],
    state_local: C::LocalId,
    poll_result_local: C::LocalId,
    dispatch_bb: C::BlockId,
    exit_bb: C::BlockId,
) where
    C: CoroCfg,
    H: CoroHooks<Cfg = C>,
{
    let coro_entry_bb = coroutine.begin.0;

    // Resume-block table. Index 0 = the original entry; index i > 0
    // is the resume target for suspension i.
    let mut state_entries: Vec<C::BlockId> = vec![coro_entry_bb];

    for (susp_idx, &(susp_bb, susp_stmt, susp_kind)) in suspensions.iter().enumerate() {
        let state_id = (susp_idx + 1) as i64;

        // Yield block: state = next_id; poll = Pending; goto exit.
        let yield_bb = cfg.new_block();
        cfg.emit_assign_i64(yield_bb, state_local, state_id);
        cfg.emit_assign_i64(yield_bb, poll_result_local, 1);
        cfg.set_goto(yield_bb, exit_bb);

        // Resume block: split out the tail of the suspend's block.
        let resume_bb = cfg.split_after(susp_bb, susp_stmt);

        match susp_kind {
            SuspKind::GuardedRecv => {
                // Hook does the IR-specific surgery: emit the peek,
                // move the original recv into resume_bb, return the
                // bool LocalId of the peek result.
                let is_ready = hooks.emit_guarded_recv_peek(cfg, susp_bb, susp_stmt, resume_bb);
                cfg.set_branch(susp_bb, is_ready, resume_bb, yield_bb);
            }
            SuspKind::ProducingSend => {
                // Producing send: keep the send statement, redirect
                // terminator to yield_bb so consumers get a turn
                // before the next send.
                cfg.set_goto(susp_bb, yield_bb);
            }
            SuspKind::Yield => {
                // Unconditional yield: erase the marker, jump to yield_bb.
                cfg.replace_with_nop(susp_bb, susp_stmt);
                cfg.set_goto(susp_bb, yield_bb);
            }
        }

        state_entries.push(resume_bb);
    }

    // Done block: poll = Ready, state = DONE, goto exit.
    let done_bb = cfg.new_block();
    cfg.emit_assign_i64(done_bb, poll_result_local, 0);
    cfg.emit_assign_i64(done_bb, state_local, DONE_STATE);
    cfg.set_goto(done_bb, exit_bb);

    // Wire `coroutine_end` into the done block.
    cfg.set_goto(coroutine.end.0, done_bb);

    // Dispatch switch:
    //   state 0    -> entry block
    //   state i    -> resume_block_i (1..=K)
    //   otherwise  -> exit (covers DONE)
    let targets: Vec<(i64, C::BlockId)> = state_entries
        .iter()
        .enumerate()
        .map(|(i, &bb)| (i as i64, bb))
        .collect();
    cfg.set_switch(dispatch_bb, state_local, targets, exit_bb);
}
