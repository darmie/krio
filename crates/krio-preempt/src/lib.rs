//! krio-preempt — time-slicing scheduler over krio-fiber.
//!
//! Drives a set of fibers with a per-resume time slice. Each round,
//! every live fiber gets a fresh deadline and is resumed; the fiber
//! polls [`krio_fiber::is_deadline_passed`] (or
//! [`krio_fiber::should_yield_early`]) at cooperative checkpoints
//! and yields when its slice elapses. Done fibers are dropped at
//! the end of the round they finish in.
//!
//! ## Cooperative today, signal-based tomorrow
//!
//! v1 is **cooperative slicing** — fibers must call
//! `should_yield_early()` at reasonable intervals. A fiber that
//! never polls won't yield mid-slice (a hot loop without checks
//! starves the scheduler). v2 will add real preemption via
//! `setitimer` + `SIGVTALRM` on Unix; that's a target-specific
//! follow-up that's intentionally not in this crate yet.
//!
//! Despite the name, krio-preempt v1 is "best-effort time-slicing"
//! rather than true preemption. The crate stays named `krio-preempt`
//! because the *external* contract is the same: a scheduler that
//! gives every fiber bounded CPU per round. v2 will tighten the
//! guarantee.
//!
//! ## Why not just use [`krio_runtime::RoundRobin`]?
//!
//! `RoundRobin` resumes each task once per tick. A fiber that runs
//! to completion in one resume hogs the round. `TimeSliceScheduler`
//! caps each resume at a wall-clock slice, so multiple fibers
//! progress fairly even when individual yields are far apart.

use std::time::{Duration, SystemTime, UNIX_EPOCH};

use krio_core::{Suspension, Task};
use krio_fiber::Fiber;

/// One scheduled fiber + its accumulated state.
struct Slot {
    fiber: Fiber,
}

/// Cooperative time-slicing scheduler. Each `tick` gives every live
/// fiber a fresh `slice` budget; the fiber's own
/// `should_yield_early()` check at user-chosen safe points causes
/// it to yield when the slice expires. Yields earlier than the
/// slice are fine — the fiber just gets a new slice on the next
/// round.
pub struct TimeSliceScheduler {
    fibers: Vec<Slot>,
    slice: Duration,
}

impl TimeSliceScheduler {
    /// Build a scheduler that gives each fiber `slice` per round.
    pub fn new(slice: Duration) -> Self {
        Self {
            fibers: Vec::new(),
            slice,
        }
    }

    /// Add a fiber to the schedule. Ownership transfers; the fiber
    /// is dropped when it completes.
    pub fn spawn(&mut self, fiber: Fiber) {
        self.fibers.push(Slot { fiber });
    }

    /// Number of live fibers currently scheduled.
    pub fn fiber_count(&self) -> usize {
        self.fibers.len()
    }

    /// Run one scheduling round. Every live fiber gets a fresh
    /// slice budget and is resumed once; finished fibers are
    /// dropped before returning. Returns `true` if at least one
    /// fiber is still alive afterwards.
    pub fn tick(&mut self) -> bool {
        if self.fibers.is_empty() {
            return false;
        }

        let mut to_remove = Vec::new();
        for (i, slot) in self.fibers.iter_mut().enumerate() {
            if slot.fiber.is_done() {
                to_remove.push(i);
                continue;
            }
            // Fresh deadline for this round.
            let deadline = current_time_ms() + self.slice.as_secs_f64() * 1000.0;
            slot.fiber.set_deadline_ms(deadline);
            match slot.fiber.step() {
                Suspension::Completed => to_remove.push(i),
                _ => {
                    // Yielded — keep around for the next round.
                    // Clear the deadline so the fiber's pollers
                    // don't carry it forward (next tick sets a new
                    // one).
                    slot.fiber.clear_deadline();
                }
            }
        }
        for &i in to_remove.iter().rev() {
            self.fibers.swap_remove(i);
        }
        !self.fibers.is_empty()
    }

    /// Drive every fiber to completion. Returns the number of
    /// rounds it took.
    pub fn run_to_completion(&mut self) -> usize {
        let mut rounds = 0;
        while self.tick() {
            rounds += 1;
        }
        rounds
    }
}

fn current_time_ms() -> f64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs_f64() * 1000.0)
        .unwrap_or(0.0)
}
