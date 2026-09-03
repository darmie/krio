//! Evidence that agents actually run at the same time.
//!
//! Every test in this crate proves the cluster is *correct* — nothing is
//! lost, nothing runs twice. None of them prove it is *parallel*: a
//! four-agent cluster that quietly did all the work on one agent would
//! pass all of them. This measures the thing tests do not.
//!
//! Three independent signals, because any one of them alone can lie:
//!
//! * **max concurrent** — agents inside `Task::step` at the same instant,
//!   sampled by the observer. If this reaches N, execution genuinely
//!   overlapped. A speedup number alone could come from cache effects.
//! * **per-agent share** — how the steps actually landed. A high max
//!   with a lopsided share means one agent did the work while others
//!   briefly touched it.
//! * **steals** — tasks that changed agent. Work stealing is what makes
//!   the share even, so zero steals with an even share would mean the
//!   spawn happened to balance, not that the scheduler did.
//!
//! ```text
//! cargo run --release --example parallel
//!
//! # and on shared-memory wasm:
//! CARGO_TARGET_WASM32_WASIP1_THREADS_RUNNER='wasmtime -W threads=y -W shared-memory=y -S threads=y' \
//!   cargo run --release --example parallel --target wasm32-wasip1-threads
//! ```

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::thread;
use std::time::Instant;

use krio_core::{Clock, Suspension, Task, TaskId};
use krio_parallel::{Cluster, SpinPark, TaskObserver};
use krio_runtime::{AgentId, ParallelScheduler};

/// Deliberately CPU-bound and un-vectorisable, so the wall clock
/// measures scheduling rather than memory bandwidth.
struct Grind {
    rounds: u32,
    per_round: u32,
    state: u64,
    done: Arc<AtomicUsize>,
}

impl Task for Grind {
    fn step(&mut self) -> Suspension {
        if self.rounds == 0 {
            self.done.fetch_add(1, Ordering::Relaxed);
            return Suspension::Completed;
        }
        self.rounds -= 1;
        // A dependent chain: each step needs the previous result, so the
        // CPU cannot pipeline its way out of the work.
        for _ in 0..self.per_round {
            self.state = self
                .state
                .wrapping_mul(6364136223846793005)
                .wrapping_add(1442695040888963407);
            self.state ^= self.state >> 33;
        }
        // Yield between rounds: a task that never yields can never be
        // stolen, and this is meant to show stealing.
        Suspension::Yielded
    }
}

struct Stats {
    steps_per_agent: Vec<AtomicUsize>,
    migrations: AtomicUsize,
    /// Agents currently inside `step`.
    in_step: AtomicUsize,
    /// High-water mark of the above — the overlap evidence.
    max_in_step: AtomicUsize,
}

impl Stats {
    fn new(agents: usize) -> Self {
        Self {
            steps_per_agent: (0..agents).map(|_| AtomicUsize::new(0)).collect(),
            migrations: AtomicUsize::new(0),
            in_step: AtomicUsize::new(0),
            max_in_step: AtomicUsize::new(0),
        }
    }
}

impl TaskObserver for Stats {
    fn on_step_begin(&self, _task: TaskId, agent: AgentId) {
        self.steps_per_agent[agent.0 as usize].fetch_add(1, Ordering::Relaxed);
        let now = self.in_step.fetch_add(1, Ordering::AcqRel) + 1;
        self.max_in_step.fetch_max(now, Ordering::AcqRel);
    }
    fn on_step_end(&self, _task: TaskId, _agent: AgentId) {
        self.in_step.fetch_sub(1, Ordering::AcqRel);
    }
    fn on_migrate(&self, _task: TaskId, _from: AgentId, _to: AgentId) {
        self.migrations.fetch_add(1, Ordering::Relaxed);
    }
}

struct StdClock(Instant);
impl Clock for StdClock {
    fn now_ms(&self) -> f64 {
        self.0.elapsed().as_secs_f64() * 1000.0
    }
}

const TASKS: usize = 96;
const ROUNDS: u32 = 24;
const PER_ROUND: u32 = 24_000;

/// How the work is handed to the cluster. The distinction matters more
/// than it looks — see the two tables in `main`.
#[derive(Clone, Copy, PartialEq)]
enum Submit {
    /// Into the shared injector: every agent pulls its own work, so the
    /// load evens out without anyone stealing anything.
    Injector,
    /// All of it onto agent 0's private deque. The only route to the
    /// other agents is a steal, so this is what actually exercises the
    /// deque.
    AllOnAgentZero,
}

fn run(agents: u32, submit: Submit) -> (f64, usize, usize, Vec<usize>) {
    let done = Arc::new(AtomicUsize::new(0));
    let stats = Arc::new(Stats::new(agents as usize));

    let mut cluster = Cluster::new(agents, SpinPark::default(), StdClock(Instant::now()));
    cluster.set_observer(Arc::clone(&stats) as Arc<dyn TaskObserver>);
    let cluster = Arc::new(cluster);

    for i in 0..TASKS {
        let task = Box::new(Grind {
            rounds: ROUNDS,
            per_round: PER_ROUND,
            state: 0x9E3779B97F4A7C15 ^ i as u64,
            done: Arc::clone(&done),
        });
        match submit {
            Submit::Injector => cluster.spawn(task),
            Submit::AllOnAgentZero => {
                cluster.spawn_on(AgentId(0), task);
            }
        }
    }

    let start = Instant::now();

    // Every agent is a worker here. The point is throughput, not UI
    // responsiveness — see `AgentRole::Main` for the other case.
    let workers: Vec<_> = (0..agents)
        .map(|i| {
            let cluster = Arc::clone(&cluster);
            thread::spawn(move || cluster.run(AgentId(i)))
        })
        .collect();

    while done.load(Ordering::Relaxed) < TASKS {
        thread::yield_now();
    }
    let elapsed = start.elapsed().as_secs_f64() * 1000.0;

    cluster.shutdown();
    for w in workers {
        w.join().unwrap();
    }

    (
        elapsed,
        stats.max_in_step.load(Ordering::Relaxed),
        stats.migrations.load(Ordering::Relaxed),
        stats
            .steps_per_agent
            .iter()
            .map(|c| c.load(Ordering::Relaxed))
            .collect(),
    )
}

fn main() {
    let cpus = thread::available_parallelism()
        .map(|n| n.get())
        .unwrap_or(1);
    println!("krio-parallel — {TASKS} tasks x {ROUNDS} rounds, {cpus} hardware threads available");

    for submit in [Submit::Injector, Submit::AllOnAgentZero] {
        println!(
            "\n{}",
            match submit {
                Submit::Injector =>
                    "spawn() — work goes to the shared injector, every agent pulls its own",
                Submit::AllOnAgentZero =>
                    "spawn_on(agent 0) — one agent owns everything; the rest must steal it",
            }
        );
        println!(
            "{:>7}  {:>10}  {:>8}  {:>14}  {:>7}   steps per agent",
            "agents", "wall (ms)", "speedup", "max concurrent", "steals"
        );
        println!("{}", "-".repeat(90));

        let mut baseline = 0.0;
        for agents in [1u32, 2, 4, 8] {
            if agents as usize > cpus * 2 {
                continue;
            }
            let (ms, max_concurrent, steals, per_agent) = run(agents, submit);
            if agents == 1 {
                baseline = ms;
            }
            let share: Vec<String> = per_agent.iter().map(|c| format!("{c}")).collect();
            println!(
                "{:>7}  {:>10.0}  {:>7.2}x  {:>14}  {:>7}   {}",
                agents,
                ms,
                baseline / ms,
                max_concurrent,
                steals,
                share.join(" ")
            );
        }
    }

    println!(
        "\nmax concurrent is the overlap proof: agents inside Task::step at the same\n\
         instant. Reaching N means execution genuinely overlapped rather than\n\
         interleaved on one agent.\n\n\
         The two tables separate two things that look alike. The first balances\n\
         through the injector and needs no steals at all; only the second shows\n\
         the deque doing its job, and its steps-per-agent column is the scheduler\n\
         pulling work off an agent that was handed all of it."
    );
}
