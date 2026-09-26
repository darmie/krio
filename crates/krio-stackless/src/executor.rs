//! Executor strategies. The state-machine transform is neutral about
//! how coroutines get polled — the `Executor` trait is the variation
//! point that picks a scheduling model.
//!
//! Two built-ins:
//!
//! - [`CooperativeExecutor`] — emits a round-robin polling loop.
//!   Drives every coroutine to completion in one stack frame.
//!   Best when the host can afford to "park" inside the polling loop.
//!
//! - [`WakerExecutor`] — emits a one-shot poll structure with two
//!   exits (`region_done` / `region_pending`). The host wraps the
//!   enclosing function in its own waker plumbing and re-enters on
//!   wake events. **Caveat**: the host must arrange for the state
//!   locals to persist across calls (typically via function-colour
//!   propagation in `krio-async` or by allocating state on the heap).
//!   Without that, a second call re-initialises state.
//!
//! Preemptive scheduling lives in a separate sibling crate — preemption
//! doesn't share the state-machine transform, so it doesn't share this
//! trait.

use crate::cfg::CoroCfg;
use crate::{DONE_STATE, Machine, POLL_BLOCKED, Region};
use krio_core::CfgId;

/// Build the executor wrapper around already-emitted coroutine state
/// machines. Called once per region after every `Machine` for the
/// region has been built.
pub trait Executor<C: CoroCfg> {
    fn finalize_region(
        &mut self,
        cfg: &mut C,
        region: &Region<C::BlockId>,
        machines: &[Machine<C::BlockId, C::LocalId>],
    );
}

/// Round-robin polling loop. Drives every coroutine to completion in
/// a single thread, single stack frame. The default for cooperative
/// concurrency primitives — Lua's `coroutine.create/resume/yield`,
/// Go's `go funcName()` (without the work-stealing scheduler), or
/// any structured-concurrency `scope`/`nursery`/`task_group`
/// construct.
pub struct CooperativeExecutor;

impl<C: CoroCfg> Executor<C> for CooperativeExecutor {
    fn finalize_region(
        &mut self,
        cfg: &mut C,
        region: &Region<C::BlockId>,
        machines: &[Machine<C::BlockId, C::LocalId>],
    ) {
        if region.coroutines.iter().any(|c| c.priority.is_some()) {
            build_priority_loop(cfg, region, machines);
        } else {
            build_cooperative_loop(cfg, region, machines);
        }
    }
}

/// Lay out the cooperative round-robin loop:
///
/// ```text
/// loop_top:
///   all_done = true
///   for each coroutine N:
///     if state_N == DONE { skip }
///     else {
///       goto dispatch_N
///       // exit_N falls back here
///       if poll_N == Ready { state_N = DONE }
///       else               { all_done = false }
///     }
///   if all_done { goto region_exit } else { goto loop_top }
/// ```
fn build_cooperative_loop<C: CoroCfg>(
    cfg: &mut C,
    region: &Region<C::BlockId>,
    machines: &[Machine<C::BlockId, C::LocalId>],
) {
    let loop_bb = cfg.new_block();
    let region_exit_bb = region_exit(cfg, region);

    let all_done_local = cfg.new_mut_bool_local();

    // Top of the loop: clear all_done.
    cfg.emit_assign_bool(loop_bb, all_done_local, true);

    // Chain per coroutine:
    //   loop_bb -> check_0 -> [done? skip : dispatch_0 -> exit_0]
    //                       -> after_poll_0 -> next_0 -> check_1 -> ...
    let mut current_bb = loop_bb;

    for (i, machine) in machines.iter().enumerate() {
        let check_bb = if i == 0 { current_bb } else { cfg.new_block() };
        if i > 0 {
            cfg.set_goto(current_bb, check_bb);
        }

        // is_done = (state == DONE)
        let is_done = cfg.new_bool_local();
        cfg.emit_eq_check_i64(check_bb, is_done, machine.state_local, DONE_STATE);

        // Done -> skip the dispatch and fall through; not done ->
        // run the dispatch, which lands back at after_poll via exit.
        let after_poll_bb = cfg.new_block();
        cfg.set_branch(check_bb, is_done, after_poll_bb, machine.dispatch_bb);

        // The coroutine's exit block (yield path + done path both
        // converge here) flows into the post-poll continuation.
        cfg.set_goto(machine.exit_bb, after_poll_bb);

        // After-poll: did this turn complete the coroutine?
        //   poll == Ready (0) -> latch state = DONE
        //   else              -> clear all_done so the loop runs again
        let is_ready = cfg.new_bool_local();
        cfg.emit_eq_check_i64(after_poll_bb, is_ready, machine.poll_result_local, 0);

        let mark_done_bb = cfg.new_block();
        let mark_pending_bb = cfg.new_block();
        let next_bb = cfg.new_block();

        cfg.set_branch(after_poll_bb, is_ready, mark_done_bb, mark_pending_bb);

        cfg.emit_assign_i64(mark_done_bb, machine.state_local, DONE_STATE);
        cfg.set_goto(mark_done_bb, next_bb);

        cfg.emit_assign_bool(mark_pending_bb, all_done_local, false);
        cfg.set_goto(mark_pending_bb, next_bb);

        current_bb = next_bb;
    }

    // After all coroutines have been polled: exit if all_done is
    // still true, otherwise round-robin again.
    cfg.set_branch(current_bb, all_done_local, region_exit_bb, loop_bb);

    finish_region(cfg, region, machines, loop_bb);
}

/// The block control reaches once every coroutine of `region` is done:
/// it continues with whatever followed the region.
fn region_exit<C: CoroCfg>(cfg: &mut C, region: &Region<C::BlockId>) -> C::BlockId {
    let region_exit_bb = cfg.new_block();
    // Split region_end after its last statement so the post-region
    // terminator lands in a block of its own, then continue there.
    let region_end_bb = region.region_end.0;
    let last_idx = cfg.statement_count(region_end_bb).saturating_sub(1);
    let post_region_bb = cfg.split_after(region_end_bb, last_idx);
    cfg.set_goto(region_exit_bb, post_region_bb);
    region_exit_bb
}

/// Erase the region's markers, initialise every machine's state, and
/// enter the executor at `entry_bb`.
fn finish_region<C: CoroCfg>(
    cfg: &mut C,
    region: &Region<C::BlockId>,
    machines: &[Machine<C::BlockId, C::LocalId>],
    entry_bb: C::BlockId,
) {
    // Erase the per-coroutine markers — the dispatch + executor
    // logic owns the control flow now.
    for coroutine in &region.coroutines {
        cfg.replace_with_nop(coroutine.begin.0, coroutine.begin.1);
        cfg.replace_with_nop(coroutine.end.0, coroutine.end.1);
    }
    // Erase region markers too.
    cfg.replace_with_nop(region.region_begin.0, region.region_begin.1);
    cfg.replace_with_nop(region.region_end.0, region.region_end.1);

    // The block holding `region_begin` typically also holds the
    // first coroutine's begin marker AND the first few statements
    // of its body. To put the body where the dispatch can switch
    // to it:
    //   1. Find the offset where the markers end.
    //   2. Move everything after into a fresh block that becomes
    //      coroutine 0's entry.
    //   3. Retarget the dispatch's "state 0" arm to the new block.
    //   4. Emit state-init writes + goto loop_bb in the original.
    let region_begin_bb = region.region_begin.0;

    // Markers were just NOP'd; locate the index after the last NOP /
    // marker in the region_begin block. Without a "is_nop" trait
    // method we conservatively start splitting at index 0 — every
    // statement that was a marker is now a Nop, so the moved tail
    // includes them. The Nops are harmless.
    //
    // We *could* expose `is_nop` later; for now, splitting at the
    // immediate position after region_begin's marker index is good
    // enough because all the markers live contiguously at the head
    // of the block.
    let split_idx = region.region_begin.1; // marker we just NOP'd lives here
    // Statements [0..=split_idx] stay (split_idx is the last in the
    // head); [split_idx+1..] move into the new entry block.
    let new_entry = cfg.split_after(region_begin_bb, split_idx);

    // If coroutine 0's body started in the same block, retarget the
    // first machine's dispatch state-0 arm.
    if let (Some(first_machine), Some(first_coro)) = (machines.first(), region.coroutines.first()) {
        if first_coro.begin.0 == region_begin_bb {
            cfg.redirect_targets(first_machine.dispatch_bb, first_coro.begin.0, new_entry);
        }
    }

    // The original block is now stripped of body — only Nop'd
    // markers remain. Append state-init writes for every coroutine,
    // then jump into the executor loop.
    for machine in machines {
        cfg.emit_assign_i64(region_begin_bb, machine.state_local, 0);
    }
    cfg.set_goto(region_begin_bb, entry_bb);
}

/// Lay out a strict-priority loop, for regions whose coroutines have
/// priorities. Lower priorities run first; coroutines without one come
/// after all that have one. The lowest ready band always runs next:
/// after any coroutine runs (or completes) the scan restarts from the
/// top, and one that is blocked passes the turn along. Within a band
/// the scan resumes after the member that ran last, so equal
/// priorities take turns.
///
/// ```text
/// top:
///   for each band B (lowest first), members m_0..m_n-1, last_B:
///     for k in last_B+1..n, then 0..=last_B:
///       if state_k != DONE { poll m_k
///         blocked      -> continue the scan
///         ran / done   -> last_B = k; goto top }
///   all done ? exit : goto top
/// ```
fn build_priority_loop<C: CoroCfg>(
    cfg: &mut C,
    region: &Region<C::BlockId>,
    machines: &[Machine<C::BlockId, C::LocalId>],
) {
    let region_exit_bb = region_exit(cfg, region);

    let mut order: Vec<usize> = (0..machines.len()).collect();
    order.sort_by_key(|&i| (region.coroutines[i].priority.unwrap_or(u32::MAX), i));
    let mut bands: Vec<Vec<usize>> = Vec::new();
    for i in order {
        let p = region.coroutines[i].priority;
        match bands.last_mut() {
            Some(band) if region.coroutines[band[0]].priority == p => band.push(i),
            _ => bands.push(vec![i]),
        }
    }

    let init_bb = cfg.new_block();
    let top_bb = cfg.new_block();
    let check_done_bb = cfg.new_block();
    // Per band: the member that ran last (-1: none yet) and which half
    // of the scan is under way.
    let lasts: Vec<C::LocalId> = bands.iter().map(|_| cfg.new_state_local()).collect();
    let sweeps: Vec<C::LocalId> = bands.iter().map(|_| cfg.new_state_local()).collect();
    for &last in &lasts {
        cfg.emit_assign_i64(init_bb, last, -1);
    }
    cfg.set_goto(init_bb, top_bb);

    // Scan sites: first half (members after last), second half (the
    // rest), for each band in order.
    let firsts: Vec<Vec<C::BlockId>> = bands
        .iter()
        .map(|b| b.iter().map(|_| cfg.new_block()).collect())
        .collect();
    let seconds: Vec<Vec<C::BlockId>> = bands
        .iter()
        .map(|b| b.iter().map(|_| cfg.new_block()).collect())
        .collect();
    cfg.set_goto(top_bb, firsts[0][0]);

    for (b, band) in bands.iter().enumerate() {
        let n = band.len();
        let next_band = firsts.get(b + 1).map_or(check_done_bb, |f| f[0]);
        for (k, &m) in band.iter().enumerate() {
            let machine = &machines[m];
            let next_first = if k + 1 < n {
                firsts[b][k + 1]
            } else {
                seconds[b][0]
            };
            let next_second = if k + 1 < n {
                seconds[b][k + 1]
            } else {
                next_band
            };
            let later: Vec<i64> = (k as i64..n as i64).collect();

            // First half polls k when last < k; second half when last >= k.
            let poll_first = cfg.new_block();
            let poll_second = cfg.new_block();
            cfg.set_switch(
                firsts[b][k],
                lasts[b],
                later.iter().map(|&v| (v, next_first)).collect(),
                poll_first,
            );
            cfg.set_switch(
                seconds[b][k],
                lasts[b],
                later.iter().map(|&v| (v, poll_second)).collect(),
                next_second,
            );
            for (poll, sweep, next) in [(poll_first, 0, next_first), (poll_second, 1, next_second)]
            {
                cfg.emit_assign_i64(poll, sweeps[b], sweep);
                let done = cfg.new_bool_local();
                cfg.emit_eq_check_i64(poll, done, machine.state_local, DONE_STATE);
                cfg.set_branch(poll, done, next, machine.dispatch_bb);
            }

            // After the poll: blocked passes the turn on; anything else
            // restarts from the top band.
            let after = cfg.new_block();
            cfg.set_goto(machine.exit_bb, after);
            let blocked = cfg.new_bool_local();
            cfg.emit_eq_check_i64(after, blocked, machine.poll_result_local, POLL_BLOCKED);
            let ran = cfg.new_block();
            let pass_on = cfg.new_block();
            cfg.set_branch(after, blocked, pass_on, ran);
            cfg.emit_assign_i64(ran, lasts[b], k as i64);
            cfg.set_goto(ran, top_bb);
            cfg.set_switch(pass_on, sweeps[b], vec![(0, next_first)], next_second);
        }
    }

    // Nothing ran this scan: leave once all are done, else scan again.
    let mut check_bb = check_done_bb;
    for machine in machines {
        let done = cfg.new_bool_local();
        cfg.emit_eq_check_i64(check_bb, done, machine.state_local, DONE_STATE);
        let next = cfg.new_block();
        cfg.set_branch(check_bb, done, next, top_bb);
        check_bb = next;
    }
    cfg.set_goto(check_bb, region_exit_bb);

    finish_region(cfg, region, machines, init_bb);
}

// ── WakerExecutor ─────────────────────────────────────────────────

/// Exit blocks recorded for one region after a [`WakerExecutor`] run.
///
/// The host wires these into its waker plumbing:
/// - `done` is reached when every coroutine has completed; krio has
///   already pointed it at the original after-region path.
/// - `pending` is reached when at least one coroutine is still
///   suspended; krio leaves its terminator unset (the consumer's
///   "unreachable" or equivalent) so the host can install whatever
///   "yield Pending to the caller" idiom its IR uses.
#[derive(Debug, Clone)]
pub struct RegionExits<B: CfgId> {
    pub done: B,
    pub pending: B,
}

/// One-shot polling executor: every coroutine in the region is polled
/// once, then the structure exits via `region_done` (all complete) or
/// `region_pending` (at least one still suspended).
///
/// Unlike [`CooperativeExecutor`], `WakerExecutor` does **not** loop —
/// after Pending, control leaves through `region_pending` and the
/// host is responsible for re-entering the function on a wake event.
///
/// **Caveat — state persistence.** State and poll-result locals are
/// emitted as plain CFG locals. For the post-Pending re-entry to
/// observe the saved state, the host compiler must arrange for these
/// locals to persist across calls. Two ways this typically happens:
///
/// 1. The host runs a captures-to-fields lift (see `krio-async`) so
///    locals live across suspensions become struct fields.
/// 2. The host allocates state on the heap and threads it through
///    every call manually.
///
/// Without one of these, the second poll re-initialises state and
/// nothing useful happens. `WakerExecutor` deliberately stops short
/// of doing the lift itself — that's a deeper transform that needs
/// type-level information krio-stackless doesn't carry.
///
/// Use the field [`Self::regions`] to read out the
/// `done` / `pending` BlockIds after running. Each completed region
/// pushes one `RegionExits`; the order matches `find_regions`'s
/// discovery order (top-down through the body).
#[derive(Debug, Default)]
pub struct WakerExecutor<B: CfgId> {
    pub regions: Vec<RegionExits<B>>,
}

impl<B: CfgId> WakerExecutor<B> {
    pub fn new() -> Self {
        Self {
            regions: Vec::new(),
        }
    }
}

impl<C: CoroCfg> Executor<C> for WakerExecutor<C::BlockId> {
    fn finalize_region(
        &mut self,
        cfg: &mut C,
        region: &Region<C::BlockId>,
        machines: &[Machine<C::BlockId, C::LocalId>],
    ) {
        let exits = build_oneshot_poll(cfg, region, machines);
        self.regions.push(exits);
    }
}

/// One-shot poll structure. Pseudocode:
///
/// ```text
/// poll_top:
///   all_done = true
///   for each coroutine N:
///     if state_N == DONE { skip }
///     else {
///       goto dispatch_N
///       // exit_N falls back here
///       if poll_N == Ready { state_N = DONE }
///       else               { all_done = false }
///     }
///   if all_done { goto region_done } else { goto region_pending }
/// ```
fn build_oneshot_poll<C: CoroCfg>(
    cfg: &mut C,
    region: &Region<C::BlockId>,
    machines: &[Machine<C::BlockId, C::LocalId>],
) -> RegionExits<C::BlockId> {
    let poll_top_bb = cfg.new_block();
    let region_done_bb = cfg.new_block();
    let region_pending_bb = cfg.new_block();

    // `region_done` inherits the original "after the region" path,
    // exactly like CooperativeExecutor's exit.
    let region_end_bb = region.region_end.0;
    let last_idx = cfg.statement_count(region_end_bb).saturating_sub(1);
    let post_region_bb = cfg.split_after(region_end_bb, last_idx);
    cfg.set_goto(region_done_bb, post_region_bb);

    // `region_pending` is left with whatever default terminator the
    // host's `new_block()` gives it (typically "unreachable"). The
    // consumer redirects it after `run_with` returns.

    let all_done_local = cfg.new_mut_bool_local();

    cfg.emit_assign_bool(poll_top_bb, all_done_local, true);

    let mut current_bb = poll_top_bb;

    for (i, machine) in machines.iter().enumerate() {
        let check_bb = if i == 0 { current_bb } else { cfg.new_block() };
        if i > 0 {
            cfg.set_goto(current_bb, check_bb);
        }

        let is_done = cfg.new_bool_local();
        cfg.emit_eq_check_i64(check_bb, is_done, machine.state_local, DONE_STATE);

        let after_poll_bb = cfg.new_block();
        cfg.set_branch(check_bb, is_done, after_poll_bb, machine.dispatch_bb);

        cfg.set_goto(machine.exit_bb, after_poll_bb);

        let is_ready = cfg.new_bool_local();
        cfg.emit_eq_check_i64(after_poll_bb, is_ready, machine.poll_result_local, 0);

        let mark_done_bb = cfg.new_block();
        let mark_pending_bb = cfg.new_block();
        let next_bb = cfg.new_block();

        cfg.set_branch(after_poll_bb, is_ready, mark_done_bb, mark_pending_bb);

        cfg.emit_assign_i64(mark_done_bb, machine.state_local, DONE_STATE);
        cfg.set_goto(mark_done_bb, next_bb);

        cfg.emit_assign_bool(mark_pending_bb, all_done_local, false);
        cfg.set_goto(mark_pending_bb, next_bb);

        current_bb = next_bb;
    }

    // Final fork: all_done -> region_done; otherwise -> region_pending.
    // No loop-back, unlike CooperativeExecutor.
    cfg.set_branch(
        current_bb,
        all_done_local,
        region_done_bb,
        region_pending_bb,
    );

    // Erase markers + lift the first coroutine's body — same dance
    // as the cooperative path.
    for coroutine in &region.coroutines {
        cfg.replace_with_nop(coroutine.begin.0, coroutine.begin.1);
        cfg.replace_with_nop(coroutine.end.0, coroutine.end.1);
    }
    cfg.replace_with_nop(region.region_begin.0, region.region_begin.1);
    cfg.replace_with_nop(region.region_end.0, region.region_end.1);

    let region_begin_bb = region.region_begin.0;
    let split_idx = region.region_begin.1;
    let new_entry = cfg.split_after(region_begin_bb, split_idx);

    if let (Some(first_machine), Some(first_coro)) = (machines.first(), region.coroutines.first())
        && first_coro.begin.0 == region_begin_bb
    {
        cfg.redirect_targets(first_machine.dispatch_bb, first_coro.begin.0, new_entry);
    }

    // State init runs once, on the very first call to the function.
    // For the second call to observe saved state, the host has to
    // arrange persistence (see the Caveat in WakerExecutor's docs).
    for machine in machines {
        cfg.emit_assign_i64(region_begin_bb, machine.state_local, 0);
    }
    cfg.set_goto(region_begin_bb, poll_top_bb);

    RegionExits {
        done: region_done_bb,
        pending: region_pending_bb,
    }
}
