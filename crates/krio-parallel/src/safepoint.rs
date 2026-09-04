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
    /// Round counter and pending flag in one word: **odd means a stop is
    /// pending**, and every release bumps it to the next even value.
    ///
    /// These were two fields once — a `requested` flag and a
    /// `generation` — and that could not be made correct. A release has
    /// to move the generation *before* clearing the flag, or an agent
    /// still on its way in waits on a release that already happened; but
    /// then an agent leaving the barrier can re-read the not-yet-cleared
    /// flag, walk straight back in against the *new* generation, and
    /// wait for a release that will never come. Clearing the flag first
    /// just swaps which of the two hangs. One word has no such window:
    /// an agent snapshots it once, and every question — is a stop
    /// pending, is it still *this* stop — is answered by that snapshot.
    ///
    /// Read on the hot path, so [`Safepoint::requested`] reads it
    /// relaxed: a late observation costs one more loop iteration, and
    /// the barrier is what enforces correctness.
    state: AtomicU32,
    /// Agents currently waiting at the barrier.
    stopped: AtomicU32,
}

/// Low bit of [`Safepoint::state`]: a stop is pending.
const PENDING: u32 = 1;

impl Safepoint {
    pub(crate) const fn new() -> Self {
        Self {
            state: AtomicU32::new(0),
            stopped: AtomicU32::new(0),
        }
    }

    /// Is a stop pending? One relaxed load — this is the hot-loop poll.
    #[inline]
    pub(crate) fn requested(&self) -> bool {
        self.state.load(Ordering::Relaxed) & PENDING == PENDING
    }

    /// How many agents are waiting at the barrier.
    pub(crate) fn stopped_count(&self) -> u32 {
        self.stopped.load(Ordering::Acquire)
    }

    /// Ask for a stop. Returns `false` if one was already pending, so
    /// two hosts cannot both think they own the world.
    pub(crate) fn request(&self) -> bool {
        let mut state = self.state.load(Ordering::Acquire);
        loop {
            if state & PENDING == PENDING {
                return false;
            }
            match self.state.compare_exchange_weak(
                state,
                state.wrapping_add(1),
                Ordering::AcqRel,
                Ordering::Acquire,
            ) {
                Ok(_) => return true,
                Err(seen) => state = seen,
            }
        }
    }

    /// Wait here until the world resumes, counting this agent as safe
    /// for as long as it does.
    ///
    /// Snapshots the state *before* announcing. Everything after keys
    /// off that snapshot, so a release landing at any point is seen as
    /// the state moving on rather than slept through.
    pub(crate) fn enter<P: Park>(&self, parker: &P) {
        let state = self.state.load(Ordering::Acquire);
        if state & PENDING != PENDING {
            // Released before we arrived. Joining now would count this
            // agent into a round it is not part of.
            return;
        }

        self.stopped.fetch_add(1, Ordering::AcqRel);
        // Wake anyone spinning on the count — the last arrival is what
        // completes the barrier.
        parker.unpark_all(&self.stopped);

        if self.state.load(Ordering::Acquire) == state {
            while self.state.load(Ordering::Acquire) == state {
                parker.park(&self.state, state);
            }
        }

        self.stopped.fetch_sub(1, Ordering::AcqRel);
        // Tell whoever is draining that one more has left.
        parker.unpark_all(&self.stopped);
    }

    /// Let everyone go, and wait for the barrier to empty.
    ///
    /// Draining before returning is not tidiness, it is correctness.
    /// Agents decrement on their way out, which happens *after* they
    /// observe the release — so a second `stop_the_world` that started
    /// immediately would read a count left over from this one, conclude
    /// the world was already stopped, and hand a collector a heap that
    /// several agents are still writing to. Found by a test that ran
    /// twenty stops back to back.
    ///
    /// A no-op when no stop is pending, so `shutdown` can call it
    /// unconditionally without leaving one behind.
    ///
    /// Gives up if `stopping` is set: agents on their way out of a
    /// shutdown will not all come back to be counted.
    pub(crate) fn release<P: Park>(&self, parker: &P, stopping: &AtomicBool) {
        let mut state = self.state.load(Ordering::Acquire);
        loop {
            if state & PENDING != PENDING {
                return;
            }
            match self.state.compare_exchange_weak(
                state,
                state.wrapping_add(1),
                Ordering::AcqRel,
                Ordering::Acquire,
            ) {
                Ok(_) => break,
                Err(seen) => state = seen,
            }
        }
        parker.unpark_all(&self.state);

        loop {
            let waiting = self.stopped.load(Ordering::Acquire);
            if waiting == 0 || stopping.load(Ordering::Acquire) {
                return;
            }
            parker.unpark_all(&self.state);
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
    fn releasing_moves_the_state() {
        let sp = Safepoint::new();
        let before = sp.state.load(Ordering::Acquire);
        sp.request();
        sp.release(&SpinPark::default(), &AtomicBool::new(false));
        assert_ne!(
            sp.state.load(Ordering::Acquire),
            before,
            "an agent parked on the old state must see it move"
        );
    }

    #[test]
    fn releasing_without_a_stop_leaves_no_stop_behind() {
        // `shutdown` releases unconditionally. If that bumped the word
        // anyway it would land on an odd value, and every agent would
        // read a stop nobody asked for.
        let sp = Safepoint::new();
        sp.release(&SpinPark::default(), &AtomicBool::new(false));
        assert!(!sp.requested());
        sp.request();
        sp.release(&SpinPark::default(), &AtomicBool::new(false));
        sp.release(&SpinPark::default(), &AtomicBool::new(false));
        assert!(!sp.requested(), "a second release must not re-arm one");
    }

    #[test]
    fn entering_after_the_release_does_not_join_the_round() {
        // The hang this word exists to prevent: an agent that reads the
        // stop, is descheduled, and arrives once it is already over must
        // walk straight back out rather than wait on a release that has
        // been and gone.
        let sp = Safepoint::new();
        sp.request();
        sp.release(&SpinPark::default(), &AtomicBool::new(false));
        sp.enter(&SpinPark::default()); // returns, or the test hangs
        assert_eq!(sp.stopped_count(), 0, "and is not counted into the next");
    }
}
