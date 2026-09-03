//! Cluster behaviour: what runs where, who stops when, and what the
//! host is told about it.

use std::sync::Arc;
use std::sync::Mutex;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::thread;

use krio_core::{Clock, Suspension, Task, TaskId};
use krio_parallel::{Cluster, SpinPark, TaskObserver};
use krio_runtime::{AgentId, AgentRole, Drive, ParallelScheduler};

// ── Clocks ────────────────────────────────────────────────────────

/// Wall-clock, for tests that only need time to move forwards.
struct StdClock(std::time::Instant);
impl StdClock {
    fn new() -> Self {
        Self(std::time::Instant::now())
    }
}
impl Clock for StdClock {
    fn now_ms(&self) -> f64 {
        self.0.elapsed().as_secs_f64() * 1000.0
    }
}

/// Advances one millisecond per reading, so a budget is exactly
/// countable instead of dependent on how fast the machine is.
struct TickClock(AtomicU64);
impl TickClock {
    fn new() -> Self {
        Self(AtomicU64::new(0))
    }
}
impl Clock for TickClock {
    fn now_ms(&self) -> f64 {
        self.0.fetch_add(1, Ordering::Relaxed) as f64
    }
}

// ── Tasks ─────────────────────────────────────────────────────────

/// Completes after `steps` resumptions, then bumps a shared counter.
struct Countdown {
    steps: u32,
    done: Arc<AtomicUsize>,
}

impl Task for Countdown {
    fn step(&mut self) -> Suspension {
        if self.steps == 0 {
            self.done.fetch_add(1, Ordering::Relaxed);
            Suspension::Completed
        } else {
            self.steps -= 1;
            Suspension::Yielded
        }
    }
}

fn countdown(steps: u32, done: &Arc<AtomicUsize>) -> Box<dyn Task + Send> {
    Box::new(Countdown {
        steps,
        done: Arc::clone(done),
    })
}

// ── Observer ──────────────────────────────────────────────────────

#[derive(Default)]
struct Recorder {
    begins: Mutex<Vec<(TaskId, AgentId)>>,
    migrations: Mutex<Vec<(TaskId, AgentId, AgentId)>>,
    ends: AtomicUsize,
}

impl TaskObserver for Recorder {
    fn on_step_begin(&self, task: TaskId, agent: AgentId) {
        self.begins.lock().unwrap().push((task, agent));
    }
    fn on_step_end(&self, _task: TaskId, _agent: AgentId) {
        self.ends.fetch_add(1, Ordering::Relaxed);
    }
    fn on_migrate(&self, task: TaskId, from: AgentId, to: AgentId) {
        self.migrations.lock().unwrap().push((task, from, to));
    }
}

// ── Tests ─────────────────────────────────────────────────────────

#[test]
fn single_agent_drains_every_task() {
    let done = Arc::new(AtomicUsize::new(0));
    let cluster = Cluster::new(1, SpinPark::default(), StdClock::new());

    for _ in 0..64 {
        cluster.spawn(countdown(3, &done));
    }

    // Unbounded worker pass: runs until there is nothing left.
    let mut passes = 0;
    while !matches!(
        cluster.drive_once(AgentId(0), AgentRole::Worker),
        Drive::Idle
    ) {
        passes += 1;
        assert!(passes < 1000, "worker pass made no progress");
    }

    assert_eq!(done.load(Ordering::Relaxed), 64);
}

#[test]
fn worker_pass_runs_until_dry() {
    let done = Arc::new(AtomicUsize::new(0));
    let cluster = Cluster::new(1, SpinPark::default(), TickClock::new());

    for _ in 0..100 {
        cluster.spawn(countdown(0, &done));
    }

    // Unbounded: one pass takes all hundred, however long the clock says
    // that took.
    assert_eq!(
        cluster.drive_once(AgentId(0), AgentRole::Worker),
        Drive::Ran(100)
    );
    assert_eq!(done.load(Ordering::Relaxed), 100);
}

#[test]
fn main_agent_pass_is_bounded_by_the_frame_budget() {
    let done = Arc::new(AtomicUsize::new(0));
    let mut cluster = Cluster::new(1, SpinPark::default(), TickClock::new());
    cluster.set_frame_ms(8.0);

    for _ in 0..100 {
        cluster.spawn(countdown(0, &done));
    }

    // The same hundred tasks, but the main agent must hand control back.
    let first = cluster.drive_once(AgentId(0), AgentRole::Main);
    let Drive::Ran(n) = first else {
        panic!("expected work, got {first:?}");
    };
    assert!(
        n < 100,
        "main agent ran the whole queue ({n}) instead of yielding at its budget"
    );
    assert!(n > 0, "main agent made no progress at all");

    // And it picks up where it left off rather than losing the rest.
    let mut total = n;
    while total < 100 {
        match cluster.drive_once(AgentId(0), AgentRole::Main) {
            Drive::Ran(k) => total += k,
            other => panic!("stalled with {total} done: {other:?}"),
        }
    }
    assert_eq!(done.load(Ordering::Relaxed), 100);
}

#[test]
fn idle_cluster_reports_idle() {
    let cluster = Cluster::new(2, SpinPark::default(), StdClock::new());
    assert_eq!(cluster.drive_once(AgentId(0), AgentRole::Main), Drive::Idle);
    assert!(!cluster.has_work());
}

#[test]
fn spawn_on_places_work_without_migrating_it() {
    let done = Arc::new(AtomicUsize::new(0));
    let recorder = Arc::new(Recorder::default());
    let mut cluster = Cluster::new(2, SpinPark::default(), StdClock::new());
    cluster.set_observer(Arc::clone(&recorder) as Arc<dyn TaskObserver>);

    let id = cluster.spawn_on(AgentId(0), countdown(0, &done));
    assert!(id.is_task(), "a real task must not get the NONE sentinel");

    // Agent 0 finds it in its own deque: no steal, so no migration.
    assert_eq!(
        cluster.drive_once(AgentId(0), AgentRole::Worker),
        Drive::Ran(1)
    );
    assert_eq!(done.load(Ordering::Relaxed), 1);
    assert!(recorder.migrations.lock().unwrap().is_empty());

    let begins = recorder.begins.lock().unwrap();
    assert_eq!(begins.len(), 1);
    assert_eq!(begins[0], (id, AgentId(0)));
    assert_eq!(recorder.ends.load(Ordering::Relaxed), 1);
}

#[test]
fn stealing_reports_the_migration_to_the_host() {
    let done = Arc::new(AtomicUsize::new(0));
    let recorder = Arc::new(Recorder::default());
    let mut cluster = Cluster::new(2, SpinPark::default(), StdClock::new());
    cluster.set_observer(Arc::clone(&recorder) as Arc<dyn TaskObserver>);

    // Placed on agent 0 …
    let id = cluster.spawn_on(AgentId(0), countdown(0, &done));

    // … but driven by agent 1, whose only route to it is a steal.
    assert_eq!(
        cluster.drive_once(AgentId(1), AgentRole::Worker),
        Drive::Ran(1)
    );

    let migrations = recorder.migrations.lock().unwrap();
    assert_eq!(
        migrations.as_slice(),
        &[(id, AgentId(0), AgentId(1))],
        "a stolen task must tell the host it changed agent"
    );

    // The host is told it moved *before* it is told it ran there.
    let begins = recorder.begins.lock().unwrap();
    assert_eq!(begins[0], (id, AgentId(1)));
}

#[test]
fn overflow_spills_to_the_injector_instead_of_being_lost() {
    let done = Arc::new(AtomicUsize::new(0));
    // Deque capacity 4 per agent, but 200 tasks: most must spill.
    let cluster = Cluster::with_capacity(1, 4, SpinPark::default(), StdClock::new());

    for _ in 0..200 {
        cluster.spawn(countdown(2, &done));
    }

    let mut guard = 0;
    while !matches!(
        cluster.drive_once(AgentId(0), AgentRole::Worker),
        Drive::Idle
    ) {
        guard += 1;
        assert!(guard < 10_000, "no progress");
    }
    assert_eq!(done.load(Ordering::Relaxed), 200);
}

#[test]
fn four_agents_share_one_queue_and_all_shut_down() {
    const TASKS: usize = 2_000;
    let done = Arc::new(AtomicUsize::new(0));
    let cluster = Arc::new(Cluster::new(4, SpinPark::default(), StdClock::new()));

    for _ in 0..TASKS {
        cluster.spawn(countdown(4, &done));
    }

    // Agents 1..4 are workers: they park when dry and must wake again.
    let workers: Vec<_> = (1..4)
        .map(|i| {
            let cluster = Arc::clone(&cluster);
            thread::spawn(move || cluster.run(AgentId(i)))
        })
        .collect();

    // Agent 0 stands in for a main thread: bounded passes, no parking.
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(30);
    while done.load(Ordering::Relaxed) < TASKS {
        cluster.drive_once(AgentId(0), AgentRole::Main);
        assert!(
            std::time::Instant::now() < deadline,
            "cluster stalled at {}/{TASKS}",
            done.load(Ordering::Relaxed)
        );
    }

    assert_eq!(done.load(Ordering::Relaxed), TASKS);

    // Every parked worker must come back out of `run`.
    cluster.shutdown();
    for w in workers {
        w.join().expect("a worker failed to shut down");
    }
}

#[test]
fn shutdown_wakes_agents_that_never_had_work() {
    let cluster = Arc::new(Cluster::new(3, SpinPark::default(), StdClock::new()));

    let workers: Vec<_> = (0..3)
        .map(|i| {
            let cluster = Arc::clone(&cluster);
            thread::spawn(move || cluster.run(AgentId(i)))
        })
        .collect();

    // They go straight to sleep having never seen a task; shutdown is
    // the only thing that can bring them back.
    thread::sleep(std::time::Duration::from_millis(20));
    cluster.shutdown();

    for w in workers {
        w.join().expect("an idle worker was never woken");
    }
}

#[test]
fn a_task_spawned_after_agents_park_still_gets_picked_up() {
    let done = Arc::new(AtomicUsize::new(0));
    let cluster = Arc::new(Cluster::new(2, SpinPark::default(), StdClock::new()));

    let worker = {
        let cluster = Arc::clone(&cluster);
        thread::spawn(move || cluster.run(AgentId(1)))
    };

    // Let it reach the park path with nothing to do.
    thread::sleep(std::time::Duration::from_millis(20));

    // The lost-wakeup case: work arrives while the agent is asleep.
    cluster.spawn(countdown(0, &done));

    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
    while done.load(Ordering::Relaxed) == 0 {
        assert!(
            std::time::Instant::now() < deadline,
            "a parked agent missed a wakeup"
        );
        thread::yield_now();
    }

    cluster.shutdown();
    worker.join().unwrap();
}
