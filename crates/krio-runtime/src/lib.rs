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
