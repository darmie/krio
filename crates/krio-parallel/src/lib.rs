//! krio-parallel — work stealing for krio `Task`s across agents that
//! share one address space.
//!
//! An **agent** is whatever executes code with its own stack and its own
//! thread-locals while sharing linear memory with its peers: an OS
//! thread natively, a Web Worker in a browser built with `+atomics`. A
//! set of them sharing memory is a **cluster**, which is the term the
//! wasm and ECMAScript specs already use, and it is worth keeping
//! because the spec's distinction is exactly the one that matters here —
//! *who may block*.
//!
//! ## What may move, and why the compiler already knows
//!
//! The currency is `Box<dyn Task + Send>`. Under `+atomics` a Web Worker
//! is a thread as far as the type system is concerned — thread-locals
//! are per-instance, statics are shared — so `Send` already means
//! precisely "may be handed to another agent". Nothing here needs an
//! `unsafe` marker trait to express it.
//!
//! The consequence lands where it should: a `krio_fiber::Fiber` holds a
//! raw pointer and is `!Send`, so it cannot be spawned onto a cluster at
//! all. That is correct rather than unfortunate. A suspended stack is
//! not relocatable, and the right place to balance fibers is *before*
//! creation — pick the least-loaded agent and build the fiber there.
//!
//! ## Two entry points, because agents are not symmetric
//!
//! [`ParallelScheduler::run`] parks when idle and belongs to a worker.
//! [`ParallelScheduler::drive_once`] never parks and is the only entry
//! point a browser's main thread may use, because
//! `memory.atomic.wait32` throws there. The budget for a pass comes from
//! [`AgentRole`] rather than from a setting: the value that already
//! decides how an agent waits also decides how long it runs, so a host
//! cannot starve a UI thread by forgetting to configure something.
//!
//! ```no_run
//! # use krio_parallel::{Cluster, SpinPark};
//! # use krio_runtime::{AgentId, AgentRole, ParallelScheduler};
//! # fn f<C: krio_core::Clock + Sync>(clock: C, task: Box<dyn krio_core::Task + Send>) {
//! let cluster = Cluster::new(4, SpinPark::default(), clock);
//! cluster.spawn(task);
//!
//! // On a worker: block until the cluster is done with you.
//! cluster.run(AgentId(1));
//!
//! // On a browser main thread: one bounded pass, then back to the loop.
//! let _ = cluster.drive_once(AgentId(0), AgentRole::Main);
//! # }
//! ```

#![no_std]

extern crate alloc;

#[cfg(test)]
extern crate std;

mod deque;
mod injector;
mod observer;
mod park;
mod parked;

use alloc::boxed::Box;
use alloc::sync::Arc;
use alloc::vec::Vec;
use core::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};

use krio_core::{Clock, Suspension, Task, TaskId};
use krio_runtime::{AgentId, AgentRole, Budget, Drive, ParallelScheduler};

use deque::Deque;
use injector::Injector;
use parked::Parked;

pub use observer::TaskObserver;
pub use park::{NOTIFIED, PARKED, Park, RUNNING, SpinPark};

/// Default per-deque capacity. Overflow spills to the injector rather
/// than growing, so this trades memory against how often that happens.
pub const DEFAULT_DEQUE_CAPACITY: usize = 256;

/// Default budget for a [`AgentRole::Main`] pass, in milliseconds.
///
/// Deliberately well under a 16.7 ms frame: the agent still has to hand
/// control back and let the host paint. This is a starting point, not a
/// measured optimum — it wants tuning against a real workload.
pub const DEFAULT_FRAME_MS: f64 = 8.0;

/// A task plus the identity a host tracks it by.
struct Job {
    id: TaskId,
    task: Box<dyn Task + Send>,
}

/// One agent's private queue and its parking slot.
///
/// Aligned so two agents' parking slots do not share a cache line. wasm
/// defines no cache line, but the hardware underneath it has one, and
/// false sharing between two spinning agents is measurable.
#[repr(align(64))]
struct Agent {
    deque: Deque<Job>,
    park_state: AtomicU32,
}

/// A set of agents sharing one address space and one pool of work.
pub struct Cluster<P: Park, C: Clock + Sync> {
    agents: Box<[Agent]>,
    injector: Injector<Job>,
    /// Tasks that returned `Pending` and are waiting on something. Held
    /// out of the run queues so an agent can genuinely sleep instead of
    /// polling a channel that cannot have changed.
    parked: Parked<Job>,
    parker: P,
    clock: C,
    stopping: AtomicBool,
    /// Rotates the first steal victim so equal loads do not permanently
    /// favour agent zero.
    rotor: AtomicU32,
    next_id: AtomicU64,
    observer: Option<Arc<dyn TaskObserver>>,
    frame_ms: f64,
}

impl<P: Park, C: Clock + Sync> Cluster<P, C> {
    /// Build a cluster for `agent_count` agents.
    ///
    /// # Panics
    /// If `agent_count` is zero.
    pub fn new(agent_count: u32, parker: P, clock: C) -> Self {
        Self::with_capacity(agent_count, DEFAULT_DEQUE_CAPACITY, parker, clock)
    }

    /// As [`Cluster::new`], with an explicit per-agent deque capacity.
    ///
    /// # Panics
    /// If `agent_count` is zero.
    pub fn with_capacity(agent_count: u32, deque_capacity: usize, parker: P, clock: C) -> Self {
        assert!(agent_count > 0, "a cluster needs at least one agent");
        let mut agents = Vec::with_capacity(agent_count as usize);
        for _ in 0..agent_count {
            agents.push(Agent {
                deque: Deque::with_capacity(deque_capacity),
                park_state: AtomicU32::new(RUNNING),
            });
        }
        Self {
            agents: agents.into_boxed_slice(),
            injector: Injector::new(),
            parked: Parked::new(),
            parker,
            clock,
            stopping: AtomicBool::new(false),
            rotor: AtomicU32::new(0),
            // Start at 1: TaskId(0) is the "no task" sentinel.
            next_id: AtomicU64::new(1),
            observer: None,
            frame_ms: DEFAULT_FRAME_MS,
        }
    }

    /// Install host hooks. Set before the cluster is shared with its
    /// agents; there is deliberately no way to swap one at runtime.
    pub fn set_observer(&mut self, observer: Arc<dyn TaskObserver>) {
        self.observer = Some(observer);
    }

    /// Override the [`AgentRole::Main`] pass budget.
    pub fn set_frame_ms(&mut self, ms: f64) {
        self.frame_ms = ms;
    }

    pub fn agent_count(&self) -> usize {
        self.agents.len()
    }

    /// Submit a task directly onto `agent`'s own deque.
    ///
    /// Placement, as opposed to [`ParallelScheduler::spawn`]'s "anyone
    /// may take this". Worth reaching for when a host knows a task is
    /// affine to state that already lives on one agent — and the only
    /// balancing available for work that cannot migrate at all.
    ///
    /// # Panics
    /// If `agent` is out of range.
    pub fn spawn_on(&self, agent: AgentId, task: Box<dyn Task + Send>) -> TaskId {
        let job = Job {
            id: self.mint_id(),
            task,
        };
        let id = job.id;
        self.deposit(agent, job);
        self.notify_one();
        id
    }

    /// Is there work anywhere in the cluster?
    ///
    /// Racy, and only ever used to decide whether parking is worth it —
    /// the re-check after publishing `PARKED` is what makes a wrong
    /// answer harmless.
    pub fn has_work(&self) -> bool {
        if !self.injector.is_empty() {
            return true;
        }
        self.agents.iter().any(|a| !a.deque.is_empty())
    }

    fn mint_id(&self) -> TaskId {
        TaskId(self.next_id.fetch_add(1, Ordering::Relaxed))
    }

    /// Put a job on `agent`'s deque, spilling to the injector if full.
    fn deposit(&self, agent: AgentId, job: Job) {
        let i = agent.0 as usize;
        assert!(i < self.agents.len(), "agent {} out of range", agent.0);
        // SAFETY: `push` is owner-only, and `agent` names the calling
        // agent's own deque. `spawn_on` from elsewhere is the one
        // exception and is why this is documented as owner-or-placement:
        // both go through here, and a concurrent push from two agents
        // to the same deque is the caller's error.
        match unsafe { self.agents[i].deque.push(job) } {
            Ok(()) => {}
            Err(job) => self.injector.push(job),
        }
    }

    /// Find the next job for `agent`: own deque, then the injector, then
    /// steal. Returns the victim when the job migrated.
    fn next_job(&self, agent: AgentId) -> Option<(Job, Option<AgentId>)> {
        let i = agent.0 as usize;

        // SAFETY: owner-only, and this is the owner.
        if let Some(job) = unsafe { self.agents[i].deque.pop() } {
            return Some((job, None));
        }

        if let Some(job) = self.injector.pop() {
            return Some((job, None));
        }

        let n = self.agents.len();
        if n > 1 {
            let start = self.rotor.fetch_add(1, Ordering::Relaxed) as usize;
            for k in 0..n {
                let v = (start + k) % n;
                if v == i {
                    continue;
                }
                if let Some(job) = self.agents[v].deque.steal() {
                    return Some((job, Some(AgentId(v as u32))));
                }
            }
        }

        None
    }

    /// Return a task that returned [`Suspension::Pending`] to the run
    /// queues.
    ///
    /// Call this when whatever the task was waiting on has happened —
    /// a channel received, a timer fired, a response arrived. krio does
    /// not own channels, so it cannot know when that is; the host that
    /// does own them calls this. The [`TaskId`] comes from
    /// [`TaskObserver::on_step_begin`], which already carries it.
    ///
    /// Safe to call from any agent, and safe to call *early* — a wake
    /// that arrives before the task has finished parking is remembered
    /// and applied when it does, so a wake is never lost to that race.
    ///
    /// Returns `true` if a parked task was actually resumed. `false`
    /// means either the task was not parked (it may be running, or
    /// already finished) or the wake arrived early and has been
    /// recorded — the two are indistinguishable from outside, and both
    /// are harmless.
    ///
    /// One wart worth knowing: waking a task that has already completed
    /// records a wake nothing will ever claim, leaving a `u64` behind. A
    /// host that only wakes live tasks leaks nothing.
    pub fn wake(&self, task: TaskId) -> bool {
        match self.parked.wake(task) {
            Some(job) => {
                // Via the injector rather than a specific agent's deque:
                // the waker is usually not the agent that parked it, and
                // deques are single-owner.
                self.injector.push(job);
                self.notify_one();
                true
            }
            None => false,
        }
    }

    /// How many tasks are waiting on a [`Cluster::wake`].
    ///
    /// Parked tasks are deliberately not counted by
    /// [`Cluster::has_work`] — they are not runnable, and treating them
    /// as work is exactly what would stop an agent from ever sleeping.
    pub fn parked_count(&self) -> usize {
        self.parked.len()
    }

    /// Move one agent out of `PARKED` and wake it. Does not disturb
    /// agents that are already running.
    fn notify_one(&self) {
        for a in self.agents.iter() {
            if a.park_state
                .compare_exchange(PARKED, NOTIFIED, Ordering::AcqRel, Ordering::Relaxed)
                .is_ok()
            {
                self.parker.unpark(&a.park_state);
                return;
            }
        }
    }
}

impl<P: Park, C: Clock + Sync> ParallelScheduler for Cluster<P, C> {
    fn spawn(&self, task: Box<dyn Task + Send>) {
        let job = Job {
            id: self.mint_id(),
            task,
        };
        self.injector.push(job);
        self.notify_one();
    }

    fn drive_once(&self, agent: AgentId, role: AgentRole) -> Drive {
        if self.stopping.load(Ordering::Acquire) {
            return Drive::ShuttingDown;
        }

        let deadline = match role.budget(self.frame_ms) {
            Budget::Deadline(ms) => Some(self.clock.now_ms() + ms),
            Budget::Unbounded => None,
        };

        let mut ran = 0usize;

        loop {
            let Some((mut job, from)) = self.next_job(agent) else {
                break;
            };

            if let Some(observer) = &self.observer {
                if let Some(from) = from {
                    observer.on_migrate(job.id, from, agent);
                }
                observer.on_step_begin(job.id, agent);
            }

            let suspension = job.task.step();

            if let Some(observer) = &self.observer {
                observer.on_step_end(job.id, agent);
            }

            ran += 1;

            match suspension {
                Suspension::Completed => drop(job),

                // Waiting on something. Hold it out of the run queues
                // entirely, or an agent with one blocked task spins a
                // core polling a channel that cannot have changed.
                //
                // `park` hands the job straight back when a wake beat it
                // here, which is the lost-wakeup race: the event fired
                // on another agent between `step` returning and this
                // line. Re-queue in that case rather than sleeping.
                Suspension::Pending => {
                    let id = job.id;
                    if let Some(job) = self.parked.park(id, job) {
                        self.deposit(agent, job);
                    }
                }

                // Yielded: wants to run again, so back onto this agent's
                // deque, newest-first.
                Suspension::Yielded => {
                    self.deposit(agent, job);
                    // Only worth waking a thief if we left surplus.
                    if self.agents[agent.0 as usize].deque.len() > 1 {
                        self.notify_one();
                    }
                }
            }

            if self.stopping.load(Ordering::Acquire) {
                return Drive::ShuttingDown;
            }

            if let Some(deadline) = deadline {
                if self.clock.now_ms() >= deadline {
                    break;
                }
            }
        }

        if ran == 0 {
            Drive::Idle
        } else {
            Drive::Ran(ran)
        }
    }

    fn run(&self, agent: AgentId) {
        let i = agent.0 as usize;
        assert!(i < self.agents.len(), "agent {} out of range", agent.0);
        let slot = &self.agents[i].park_state;

        loop {
            match self.drive_once(agent, AgentRole::Worker) {
                Drive::ShuttingDown => return,
                Drive::Ran(_) => continue,
                Drive::Idle => {}
            }

            // Publish the intent to sleep.
            if slot
                .compare_exchange(RUNNING, PARKED, Ordering::AcqRel, Ordering::Acquire)
                .is_err()
            {
                // A notification arrived while we were running. Consume
                // it and go round again.
                slot.store(RUNNING, Ordering::Release);
                continue;
            }

            // Re-check *after* publishing PARKED. This is what closes
            // the lost-wakeup race: work pushed between our last scan
            // and the CAS is still seen, because a notifier that missed
            // our PARKED must have pushed before it.
            if self.stopping.load(Ordering::Acquire) || self.has_work() {
                slot.store(RUNNING, Ordering::Release);
                if self.stopping.load(Ordering::Acquire) {
                    return;
                }
                continue;
            }

            self.parker.park(slot, PARKED);
            slot.store(RUNNING, Ordering::Release);

            if self.stopping.load(Ordering::Acquire) {
                return;
            }
        }
    }

    fn shutdown(&self) {
        self.stopping.store(true, Ordering::Release);
        // Wake everyone, parked or not: an agent that is mid-pass will
        // see `stopping` on its next check, and one that is asleep needs
        // the notification to get there at all.
        for a in self.agents.iter() {
            a.park_state.store(NOTIFIED, Ordering::Release);
            self.parker.unpark(&a.park_state);
        }
    }
}
