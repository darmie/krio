//! krio-runtime — scheduler primitives for the krio family.
//!
//! Drives [`krio_core::Task`]s of any execution model through a
//! uniform interface. A single scheduler can run a heterogeneous
//! mix: stackful fibers from `krio-fiber`, future async coroutines
//! from `krio-async`, and host-wrapped stackless coroutines from
//! `krio-stackless` all just need to implement `Task`.
//!
//! ## What's here
//!
//! - [`Scheduler`] trait — the interface schedulers expose.
//! - [`RoundRobin`] — the simplest viable scheduler. Polls every
//!   spawned task in a loop, dropping completed ones. Single-thread,
//!   no priorities, no fairness guarantees beyond "round-robin until
//!   done." Good as a baseline, good as a default for cooperative
//!   workloads.
//!
//! ## What's not here yet
//!
//! - Work-stealing / multi-thread scheduling — needs `Send` task
//!   types, which most krio task models deliberately don't have.
//! - Priority / fair scheduling — straightforward to add as separate
//!   `Scheduler` impls.
//! - Timer / sleep / channel primitives — each variant in the family
//!   typically ships its own; a unified set might land here later.

#![no_std]

extern crate alloc;

use alloc::boxed::Box;
use alloc::vec::Vec;

use krio_core::{Suspension, Task};

/// The scheduler interface. A `Scheduler` owns a collection of
/// [`Task`]s, dispatches steps, and reports when everything is done.
pub trait Scheduler {
    /// Add a task to the scheduler. Ownership transfers; the
    /// scheduler drops the task when it completes.
    fn spawn(&mut self, task: Box<dyn Task>);

    /// Run one round of the scheduling policy. The exact meaning is
    /// up to the implementer (e.g. "step every task once" for
    /// round-robin, "step the highest-priority ready task" for
    /// priority).
    ///
    /// Returns `true` if at least one task is still alive after the
    /// round, `false` if everything has completed.
    fn tick(&mut self) -> bool;

    /// Drive every task to completion. Returns when no live tasks
    /// remain.
    fn run_to_completion(&mut self) {
        while self.tick() {}
    }

    /// Number of tasks currently held by the scheduler (live ones).
    fn task_count(&self) -> usize;
}

/// Round-robin scheduler. Each `tick` steps every task once;
/// completed tasks are removed in the same pass.
pub struct RoundRobin {
    tasks: Vec<Box<dyn Task>>,
}

impl RoundRobin {
    pub fn new() -> Self {
        Self { tasks: Vec::new() }
    }

    pub fn with_capacity(cap: usize) -> Self {
        Self {
            tasks: Vec::with_capacity(cap),
        }
    }
}

impl Default for RoundRobin {
    fn default() -> Self {
        Self::new()
    }
}

impl Scheduler for RoundRobin {
    fn spawn(&mut self, task: Box<dyn Task>) {
        self.tasks.push(task);
    }

    fn tick(&mut self) -> bool {
        if self.tasks.is_empty() {
            return false;
        }
        // Step in place; collect completed indices in reverse so we
        // can swap_remove without disturbing earlier indices.
        let mut to_remove: Vec<usize> = Vec::new();
        for (i, task) in self.tasks.iter_mut().enumerate() {
            if matches!(task.step(), Suspension::Completed) {
                to_remove.push(i);
            }
        }
        for &i in to_remove.iter().rev() {
            self.tasks.swap_remove(i);
        }
        !self.tasks.is_empty()
    }

    fn task_count(&self) -> usize {
        self.tasks.len()
    }
}

// ── Parallel scheduling ───────────────────────────────────────────

/// Index of one execution agent inside a cluster that shares an
/// address space — an OS thread natively, a Web Worker in a browser.
///
/// Agent `0` is the agent that bootstrapped the cluster. In a browser
/// that is conventionally the main thread, which is why [`AgentRole`]
/// exists as a separate value: the *identity* of an agent and what it
/// is *allowed to do* are different questions.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct AgentId(pub u32);

/// What an agent is permitted to do when it runs out of work.
///
/// This is not a preference — it is a property of where the code runs.
/// A browser's main thread throws on `memory.atomic.wait32`, so an
/// agent driving it must return to the event loop instead of blocking.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AgentRole {
    /// Must not block: returns to a host event loop between passes.
    Main,
    /// May block: parks until another agent hands it work.
    Worker,
}

/// How long a single pass may run before yielding control back.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Budget {
    /// Run until the agent has no work left.
    Unbounded,
    /// Stop once the clock passes this many milliseconds from the
    /// start of the pass.
    Deadline(f64),
}

impl AgentRole {
    /// The budget implied by this role.
    ///
    /// Deliberately derived rather than configured. The role already
    /// decides how an agent *waits*; letting it also decide how long
    /// an agent *runs* means a host cannot starve a UI thread by
    /// forgetting to pass a number.
    pub fn budget(self, frame_ms: f64) -> Budget {
        match self {
            AgentRole::Main => Budget::Deadline(frame_ms),
            AgentRole::Worker => Budget::Unbounded,
        }
    }
}

/// Outcome of one scheduling pass.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Drive {
    /// No work was available anywhere in the cluster.
    Idle,
    /// This many tasks were stepped.
    Ran(usize),
    /// The cluster is shutting down; stop calling in.
    ShuttingDown,
}

/// A scheduler whose tasks may move between agents sharing one address
/// space.
///
/// The currency is `Box<dyn Task + Send>` rather than `Box<dyn Task>`,
/// and that bound is the whole safety argument: an execution agent is a
/// thread as far as the type system is concerned — a Web Worker under
/// `+atomics` included — so `Send` already means exactly "may be moved
/// to another agent". A `krio_fiber::Fiber` is `!Send` and therefore
/// cannot enter one of these schedulers at all, which is the correct
/// outcome: a suspended stack is not relocatable.
///
/// This is a second trait rather than a widening of [`Scheduler`]. A
/// host that drives one agent should not have to think about `Send`,
/// and a host that drives many should not lose the guarantee.
pub trait ParallelScheduler: Sync {
    /// Submit a task to the cluster. Any agent may pick it up.
    fn spawn(&self, task: Box<dyn Task + Send>);

    /// Run one pass on behalf of `agent`, bounded by `role`'s budget.
    ///
    /// Never parks, so this is the only entry point an [`AgentRole::Main`]
    /// agent may use.
    fn drive_once(&self, agent: AgentId, role: AgentRole) -> Drive;

    /// Drive until shutdown, parking whenever there is no work.
    ///
    /// [`AgentRole::Worker`] agents only — on a browser main thread the
    /// park will throw.
    fn run(&self, agent: AgentId);

    /// Ask every agent to stop at its next pass boundary.
    fn shutdown(&self);
}
