//! What krio tells a host about tasks it is driving.
//!
//! krio does not collect garbage, walk roots, or decide what a value
//! means. What it owes a host that does is enough to *find* its own
//! state for a task krio owns and may move — the same bargain
//! `krio-fiber` already strikes by publishing `saved_sp` and the
//! callee-saved frame offsets and then getting out of the way.
//!
//! For a stackful fiber a host can key its shadow state to a fiber id
//! and know the fiber never moves. A migratable task needs the same key
//! plus one event that has no stackful equivalent: **it changed agent**.
//! That is the entire difference, and [`TaskObserver::on_migrate`] is
//! the entire API for it.
//!
//! Every method has a default no-op body, so a host implements only the
//! events it cares about, and a host that cares about none pays nothing
//! — the scheduler skips the calls when no observer is installed.

use krio_core::TaskId;
use krio_runtime::AgentId;

/// Host hooks into the lifecycle of a scheduled task.
///
/// Called on the agent doing the work, synchronously, with no scheduler
/// lock held. An implementation that blocks stalls that agent, so keep
/// them to bookkeeping.
pub trait TaskObserver: Send + Sync {
    /// About to call `Task::step` on `agent`.
    fn on_step_begin(&self, task: TaskId, agent: AgentId) {
        let _ = (task, agent);
    }

    /// `Task::step` returned. Fires for a completing step too — pair it
    /// with the scheduler's own drop if you need "task is gone".
    fn on_step_end(&self, task: TaskId, agent: AgentId) {
        let _ = (task, agent);
    }

    /// `task` moved from `from` to `to` and is about to run there.
    ///
    /// Emitted after a successful steal and before the thief steps it,
    /// so a host relocating shadow state has a window in which no agent
    /// is executing the task.
    fn on_migrate(&self, task: TaskId, from: AgentId, to: AgentId) {
        let _ = (task, from, to);
    }
}
