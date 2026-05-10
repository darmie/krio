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
use crate::{DONE_STATE, Machine, Region};
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
        build_cooperative_loop(cfg, region, machines);
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
    let region_exit_bb = cfg.new_block();

    // The exit block inherits whatever the original `region_end`
    // block was pointing to — that's the "after the region" path
    // in source order. We grab it by setting the new block's
    // terminator to `goto region_end` first, then redirecting.
    // Simpler: split the region_end block at its end, take the new
    // block as the exit.
    //
    // Easiest path that matches the original implementation: set the
    // exit block to fall through to whatever region_end's successor
    // was. We don't have a "clone terminator" trait method, so we
    // use redirect_targets to redirect from a sentinel — but we
    // don't have a clean way to express that either.
    //
    // Workaround: use split_after on region_end at its last index to
    // force the post-region terminator into a new block we'll keep.
    let region_end_bb = region.region_end.0;
    let last_idx = cfg.statement_count(region_end_bb).saturating_sub(1);
    let post_region_bb = cfg.split_after(region_end_bb, last_idx);
    cfg.set_goto(region_exit_bb, post_region_bb);

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
    cfg.set_goto(region_begin_bb, loop_bb);
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
