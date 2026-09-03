//! krio-parallel across real Web Workers, in a real browser.
//!
//! Same demonstration as `crates/krio-parallel/examples/parallel.rs`,
//! with Web Workers where that one uses OS threads. The cluster code is
//! identical — only the bootstrap differs, which is the claim the whole
//! design rests on.

use std::sync::OnceLock;
use std::sync::atomic::{AtomicUsize, Ordering};

use krio_core::{Suspension, Task, TaskId};
use krio_parallel::{Cluster, TaskObserver};
use krio_wasm::WasmPark;
use krio_runtime::{AgentId, ParallelScheduler};
use krio_wasm::EpochClock;
use wasm_bindgen::prelude::*;

type Cl = Cluster<WasmPark, EpochClock>;

/// Lives in linear memory, so every worker instance sees the same one.
/// That is only true because workers share the memory and do not re-run
/// data-segment initialisation.
static CLUSTER: OnceLock<Cl> = OnceLock::new();
static STATS: OnceLock<Stats> = OnceLock::new();
static DONE: AtomicUsize = AtomicUsize::new(0);

struct Stats {
    steps_per_agent: Vec<AtomicUsize>,
    migrations: AtomicUsize,
    in_step: AtomicUsize,
    max_in_step: AtomicUsize,
}

impl TaskObserver for Stats {
    fn on_step_begin(&self, _t: TaskId, agent: AgentId) {
        self.steps_per_agent[agent.0 as usize].fetch_add(1, Ordering::Relaxed);
        let now = self.in_step.fetch_add(1, Ordering::AcqRel) + 1;
        self.max_in_step.fetch_max(now, Ordering::AcqRel);
    }
    fn on_step_end(&self, _t: TaskId, _a: AgentId) {
        self.in_step.fetch_sub(1, Ordering::AcqRel);
    }
    fn on_migrate(&self, _t: TaskId, _f: AgentId, _to: AgentId) {
        self.migrations.fetch_add(1, Ordering::Relaxed);
    }
}

struct Grind {
    rounds: u32,
    per_round: u32,
    state: u64,
}

impl Task for Grind {
    fn step(&mut self) -> Suspension {
        if self.rounds == 0 {
            DONE.fetch_add(1, Ordering::Relaxed);
            return Suspension::Completed;
        }
        self.rounds -= 1;
        for _ in 0..self.per_round {
            self.state = self
                .state
                .wrapping_mul(6364136223846793005)
                .wrapping_add(1442695040888963407);
            self.state ^= self.state >> 33;
        }
        Suspension::Yielded
    }
}

/// Build the cluster. Called once, on the main thread, before any
/// worker is started.
#[wasm_bindgen]
pub fn init_cluster(agents: u32) -> Result<(), JsValue> {
    krio_wasm::cluster_support()
        .ok_or_else(|| JsValue::from_str("build lacks +atomics — no cluster possible"))?;

    let stats = Stats {
        steps_per_agent: (0..agents).map(|_| AtomicUsize::new(0)).collect(),
        migrations: AtomicUsize::new(0),
        in_step: AtomicUsize::new(0),
        max_in_step: AtomicUsize::new(0),
    };
    STATS
        .set(stats)
        .map_err(|_| JsValue::from_str("already initialised"))?;

    let mut cluster = Cluster::new(agents, WasmPark::new(), EpochClock::new());
    cluster.set_observer(std::sync::Arc::new(StatsRef) as std::sync::Arc<dyn TaskObserver>);
    CLUSTER
        .set(cluster)
        .map_err(|_| JsValue::from_str("already initialised"))?;
    Ok(())
}

/// Indirection so the observer can be an `Arc` while the data lives in
/// a shared static every worker can reach.
struct StatsRef;
impl TaskObserver for StatsRef {
    fn on_step_begin(&self, t: TaskId, a: AgentId) {
        if let Some(s) = STATS.get() {
            s.on_step_begin(t, a);
        }
    }
    fn on_step_end(&self, t: TaskId, a: AgentId) {
        if let Some(s) = STATS.get() {
            s.on_step_end(t, a);
        }
    }
    fn on_migrate(&self, t: TaskId, f: AgentId, to: AgentId) {
        if let Some(s) = STATS.get() {
            s.on_migrate(t, f, to);
        }
    }
}

/// Queue the workload. `all_on_agent_zero` forces every task onto one
/// agent's private deque, so the others can only get work by stealing.
#[wasm_bindgen]
pub fn submit(tasks: u32, rounds: u32, per_round: u32, all_on_agent_zero: bool) {
    let cluster = CLUSTER.get().expect("init_cluster first");
    DONE.store(0, Ordering::Relaxed);
    for i in 0..tasks {
        let task = Box::new(Grind {
            rounds,
            per_round,
            state: 0x9E3779B97F4A7C15 ^ i as u64,
        });
        if all_on_agent_zero {
            cluster.spawn_on(AgentId(0), task);
        } else {
            cluster.spawn(task);
        }
    }
}

/// A worker's whole life: block here until the cluster shuts down.
#[wasm_bindgen]
pub fn run_agent(agent: u32) {
    CLUSTER.get().expect("init_cluster first").run(AgentId(agent));
}

/// One non-blocking pass for the main thread. `memory.atomic.wait32`
/// throws here, so this is the only entry point it may use.
#[wasm_bindgen]
pub fn drive_main() -> u32 {
    use krio_runtime::{AgentRole, Drive};
    let cluster = CLUSTER.get().expect("init_cluster first");
    match cluster.drive_once(AgentId(0), AgentRole::Main) {
        Drive::Idle => 0,
        Drive::Ran(n) => n as u32,
        Drive::ShuttingDown => u32::MAX,
    }
}

#[wasm_bindgen]
pub fn completed() -> u32 {
    DONE.load(Ordering::Relaxed) as u32
}

#[wasm_bindgen]
pub fn shutdown() {
    CLUSTER.get().expect("init_cluster first").shutdown();
}

/// The address JS hands to `Atomics.waitAsync`.
#[wasm_bindgen]
pub fn park_slot_addr(agent: u32) -> u32 {
    CLUSTER
        .get()
        .expect("init_cluster first")
        .park_slot_addr(AgentId(agent)) as u32
}

#[wasm_bindgen]
pub fn publish_epoch_ms(ms: f64) {
    krio_wasm::publish_epoch_ms(ms as u64);
}

/// `maxConcurrent,migrations,steps0,steps1,...`
#[wasm_bindgen]
pub fn stats_csv() -> String {
    let s = STATS.get().expect("init_cluster first");
    let mut out = format!(
        "{},{}",
        s.max_in_step.load(Ordering::Relaxed),
        s.migrations.load(Ordering::Relaxed)
    );
    for c in &s.steps_per_agent {
        out.push(',');
        out.push_str(&c.load(Ordering::Relaxed).to_string());
    }
    out
}

/// Zero the counters between workloads, so two measurements in one page
/// load do not accumulate into each other.
#[wasm_bindgen]
pub fn reset_stats() {
    let s = STATS.get().expect("init_cluster first");
    for c in &s.steps_per_agent {
        c.store(0, Ordering::Relaxed);
    }
    s.migrations.store(0, Ordering::Relaxed);
    s.max_in_step.store(0, Ordering::Relaxed);
}
