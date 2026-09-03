//! Stopping every agent at a point where none of them is mutating.
//!
//! krio does not collect garbage and should not learn how. What a host
//! that does collect cannot build for itself is the *rendezvous*: with
//! work spread over agents, a collection needs every one of them stopped
//! somewhere safe before it can scan. Only the scheduler knows where
//! those points are and who is currently between them.
//!
//! ## What counts as safe
//!
//! An agent is safe when it is not inside `Task::step`. Three ways to
//! be there, and all three have to be handled or the barrier hangs:
//!
//! * **between steps** — the scheduler checks on its own, so a task made
//!   of many short steps needs nothing from its author;
//! * **idle** — an agent asleep on its parking slot is trivially safe,
//!   but it must be woken to *say so*, and it must not pick up work
//!   again until the world resumes;
//! * **inside a long step** — the hard one. See below.
//!
//! ## Hot loops
//!
//! A game loop, a physics tick, a decode loop: one `step` that runs for
//! milliseconds and never returns to the scheduler. Nothing the
//! scheduler does can interrupt it — wasm has no signals and no way to
//! suspend another agent's stack — so the loop has to ask.
//!
//! That is the same answer HotSpot and Go reach: a poll at loop
//! back-edges. [`Safepoint::requested`] is one relaxed load of a shared
//! flag, which is a load and a predictable branch — cheap enough for the
//! inner loop of a renderer, and the only known way to bound
//! time-to-safepoint on a target with no preemption.
//!
//! A host that never polls does not corrupt anything; it just cannot be
//! collected while that loop runs, and a `stop_the_world` waits until it
//! finishes. That failure mode is a pause, which is diagnosable, rather
//! than a torn heap, which is not.

use core::sync::atomic::{AtomicBool, AtomicU32, Ordering};

use crate::park::Park;

/// Cluster-wide stop-the-world state.
pub(crate) struct Safepoint {
    /// Someone wants the world stopped. Read on the hot path, so
    /// relaxed: a late observation costs one more loop iteration, and
    /// the barrier is what enforces correctness.
    requested: AtomicBool,
    /// Agents currently waiting at the barrier.
    stopped: AtomicU32,
    /// Bumped once per release. Agents park on *this* rather than on the
    /// flag, so a stop/resume pair that lands while an agent is on its
    /// way in cannot leave it asleep against a flag that already went
    /// back to false.
    generation: AtomicU32,
}

impl Safepoint {
    pub(crate) const fn new() -> Self {
        Self {
            requested: AtomicBool::new(false),
            stopped: AtomicU32::new(0),
            generation: AtomicU32::new(0),
        }
    }

    /// Is a stop pending? One relaxed load — this is the hot-loop poll.
    #[inline]
    pub(crate) fn requested(&self) -> bool {
        self.requested.load(Ordering::Relaxed)
    }

    /// How many agents are waiting at the barrier.
    pub(crate) fn stopped_count(&self) -> u32 {
        self.stopped.load(Ordering::Acquire)
    }

    /// Ask for a stop. Returns `false` if one was already pending, so
    /// two hosts cannot both think they own the world.
    pub(crate) fn request(&self) -> bool {
        self.requested
            .compare_exchange(false, true, Ordering::AcqRel, Ordering::Relaxed)
            .is_ok()
    }

    /// Wait here until the world resumes, counting this agent as safe
    /// for as long as it does.
    ///
    /// Reads the generation *before* announcing, so a release that lands
    /// between the two is seen as a generation change rather than
    /// slept through.
    pub(crate) fn enter<P: Park>(&self, parker: &P) {
        let generation = self.generation.load(Ordering::Acquire);
        self.stopped.fetch_add(1, Ordering::AcqRel);
        // Wake anyone spinning on the count — the last arrival is what
        // completes the barrier.
        parker.unpark_all(&self.stopped);

        while self.generation.load(Ordering::Acquire) == generation {
            parker.park(&self.generation, generation);
        }

        self.stopped.fetch_sub(1, Ordering::AcqRel);
        // Tell whoever is draining that one more has left.
        parker.unpark_all(&self.stopped);
    }

    /// Let everyone go, and wait for the barrier to empty.
    ///
    /// The generation moves before the flag clears: an agent still on
    /// its way to the barrier must find a *changed generation* rather
    /// than a cleared flag, or it would wait for a release that already
    /// happened.
    ///
    /// Draining before returning is not tidiness, it is correctness.
    /// Agents decrement on their way out, which happens *after* they
    /// observe the release — so a second `stop_the_world` that started
    /// immediately would read a count left over from this one, conclude
    /// the world was already stopped, and hand a collector a heap that
    /// several agents are still writing to. Found by a test that ran
    /// twenty stops back to back.
    ///
    /// Gives up if `stopping` is set: agents on their way out of a
    /// shutdown will not all come back to be counted.
    pub(crate) fn release<P: Park>(&self, parker: &P, stopping: &AtomicBool) {
        self.generation.fetch_add(1, Ordering::AcqRel);
        self.requested.store(false, Ordering::Release);
        parker.unpark_all(&self.generation);

        loop {
            let waiting = self.stopped.load(Ordering::Acquire);
            if waiting == 0 || stopping.load(Ordering::Acquire) {
                return;
            }
            parker.unpark_all(&self.generation);
            // Park rather than spin. A hard spin here is not merely
            // wasteful: on a machine with more runnable threads than
            // cores it holds a core against the very agents it is
            // waiting for, and the wait becomes self-defeating. Found by
            // running five clusters at once on ten cores.
            parker.park(&self.stopped, waiting);
        }
    }

    /// Wait until `expected` agents have arrived, or `stopping` is set.
    ///
    /// Waits through the backend's [`Park`] rather than spinning: the
    /// requester and the agents it waits for are competing for the same
    /// cores, and a hard spin makes that competition worse the more
    /// contended the machine already is.
    ///
    /// Returns `false` if it gave up because the cluster is shutting
    /// down.
    pub(crate) fn await_all<P: Park>(
        &self,
        expected: u32,
        stopping: &AtomicBool,
        parker: &P,
    ) -> bool {
        loop {
            let arrived = self.stopped.load(Ordering::Acquire);
            if arrived >= expected {
                return true;
            }
            if stopping.load(Ordering::Acquire) {
                return false;
            }
            // Park on the count, which arriving agents notify. Spinning
            // here starves the agents being waited for whenever the
            // machine is oversubscribed.
            parker.park(&self.stopped, arrived);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::park::SpinPark;

    #[test]
    fn a_second_request_is_refused() {
        let sp = Safepoint::new();
        assert!(sp.request(), "first request owns the world");
        assert!(!sp.request(), "a second must not think it does too");
        assert!(sp.requested());
        sp.release(&SpinPark::default(), &AtomicBool::new(false));
        assert!(!sp.requested());
        assert!(sp.request(), "and it can be taken again afterwards");
    }

    #[test]
    fn releasing_moves_the_generation() {
        let sp = Safepoint::new();
        let before = sp.generation.load(Ordering::Acquire);
        sp.request();
        sp.release(&SpinPark::default(), &AtomicBool::new(false));
        assert_ne!(
            sp.generation.load(Ordering::Acquire),
            before,
            "an agent parked on the old generation must see it move"
        );
    }
}
