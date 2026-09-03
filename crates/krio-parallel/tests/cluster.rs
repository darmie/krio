//! Cluster behaviour: what runs where, who stops when, and what the
//! host is told about it.

use std::sync::Arc;
use std::sync::Mutex;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::thread;

use krio_core::{Clock, Suspension, Task, TaskId};
use krio_parallel::{Cluster, Park, SpinPark, TaskObserver};
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

// ── The main agent must never block ───────────────────────────────

/// A [`Park`] that refuses to be used.
///
/// `memory.atomic.wait32` throws on a browser's main thread, and nothing
/// in the type system prevents an agent from reaching it — the same
/// `WasmPark` is legitimately shared by every agent in the cluster. The
/// rule is structural: `run()` parks and is worker-only, `drive_once()`
/// never parks.
///
/// A browser is one place to check that, and a poor one to rely on: it
/// needs a driver, a headless engine and cross-origin isolation, and it
/// only fails once someone has already shipped the regression. Handing
/// the cluster a parker that panics tests the same claim on every
/// target, in microseconds, right next to the code.
struct PoisonPark;

impl Park for PoisonPark {
    fn park(&self, _slot: &std::sync::atomic::AtomicU32, _expected: u32) {
        panic!("drive_once() parked — this throws on a browser main thread");
    }
    fn unpark(&self, _slot: &std::sync::atomic::AtomicU32) {
        // Notifying is fine anywhere; only waiting is forbidden.
    }
}

#[test]
fn drive_once_never_parks_however_it_is_called() {
    let done = Arc::new(AtomicUsize::new(0));
    let cluster = Cluster::new(3, PoisonPark, StdClock::new());

    // Idle: the tempting place to "just wait for work".
    assert_eq!(cluster.drive_once(AgentId(0), AgentRole::Main), Drive::Idle);
    assert_eq!(
        cluster.drive_once(AgentId(0), AgentRole::Worker),
        Drive::Idle
    );

    // Busy, and over budget, so the pass ends early with work left.
    for _ in 0..64 {
        cluster.spawn(countdown(2, &done));
    }
    while done.load(Ordering::Relaxed) < 64 {
        assert_ne!(
            cluster.drive_once(AgentId(0), AgentRole::Main),
            Drive::ShuttingDown
        );
    }

    // Holding nothing but a waiting task — the other place an agent
    // might decide there is nothing better to do than sleep.
    struct Waiter;
    impl Task for Waiter {
        fn step(&mut self) -> Suspension {
            Suspension::Pending
        }
    }
    cluster.spawn_on(AgentId(0), Box::new(Waiter));
    cluster.drive_once(AgentId(0), AgentRole::Main);
    assert_eq!(cluster.parked_count(), 1);
    assert_eq!(cluster.drive_once(AgentId(0), AgentRole::Main), Drive::Idle);

    // Shutdown notifies every agent; unparking is allowed, waiting is
    // not, so this must not trip the poison either.
    cluster.shutdown();
    assert_eq!(
        cluster.drive_once(AgentId(0), AgentRole::Main),
        Drive::ShuttingDown
    );
}

// ── Waiting tasks ─────────────────────────────────────────────────

/// Returns `Pending` until `ready` is set, counting every poll so a
/// test can prove the scheduler is not spinning on it.
struct Waiter {
    ready: Arc<std::sync::atomic::AtomicBool>,
    polls: Arc<AtomicUsize>,
    done: Arc<AtomicUsize>,
}

impl Task for Waiter {
    fn step(&mut self) -> Suspension {
        self.polls.fetch_add(1, Ordering::Relaxed);
        if self.ready.load(Ordering::Acquire) {
            self.done.fetch_add(1, Ordering::Relaxed);
            Suspension::Completed
        } else {
            Suspension::Pending
        }
    }
}

#[test]
fn a_pending_task_is_parked_rather_than_polled_in_a_loop() {
    let ready = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let polls = Arc::new(AtomicUsize::new(0));
    let done = Arc::new(AtomicUsize::new(0));

    let cluster = Cluster::new(1, SpinPark::default(), StdClock::new());
    cluster.spawn_on(
        AgentId(0),
        Box::new(Waiter {
            ready: Arc::clone(&ready),
            polls: Arc::clone(&polls),
            done: Arc::clone(&done),
        }),
    );

    // First pass polls it once, learns it is waiting, and sets it aside.
    assert_eq!(
        cluster.drive_once(AgentId(0), AgentRole::Worker),
        Drive::Ran(1)
    );
    assert_eq!(polls.load(Ordering::Relaxed), 1);
    assert_eq!(cluster.parked_count(), 1);

    // This is the whole point: further passes find nothing to do. A
    // waiting task must not be work, or an agent can never sleep.
    for _ in 0..50 {
        assert_eq!(
            cluster.drive_once(AgentId(0), AgentRole::Worker),
            Drive::Idle
        );
    }
    assert_eq!(
        polls.load(Ordering::Relaxed),
        1,
        "a parked task was polled again — the agent is spinning on it"
    );
    assert!(!cluster.has_work(), "a waiting task must not count as work");
}

#[test]
fn waking_a_pending_task_puts_it_back_to_work() {
    let ready = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let polls = Arc::new(AtomicUsize::new(0));
    let done = Arc::new(AtomicUsize::new(0));

    let cluster = Cluster::new(1, SpinPark::default(), StdClock::new());
    let id = cluster.spawn_on(
        AgentId(0),
        Box::new(Waiter {
            ready: Arc::clone(&ready),
            polls: Arc::clone(&polls),
            done: Arc::clone(&done),
        }),
    );

    cluster.drive_once(AgentId(0), AgentRole::Worker);
    assert_eq!(cluster.parked_count(), 1);

    // The host's channel received something.
    ready.store(true, Ordering::Release);
    assert!(cluster.wake(id), "waking a parked task must resume it");
    assert_eq!(cluster.parked_count(), 0);
    assert!(cluster.has_work());

    assert_eq!(
        cluster.drive_once(AgentId(0), AgentRole::Worker),
        Drive::Ran(1)
    );
    assert_eq!(done.load(Ordering::Relaxed), 1);
    assert_eq!(polls.load(Ordering::Relaxed), 2);

    // Waking something that is gone is a no-op, not a panic.
    assert!(!cluster.wake(id));
}

#[test]
fn a_wake_that_arrives_before_the_task_parks_is_not_lost() {
    let ready = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let polls = Arc::new(AtomicUsize::new(0));
    let done = Arc::new(AtomicUsize::new(0));

    let cluster = Cluster::new(1, SpinPark::default(), StdClock::new());
    let id = cluster.spawn_on(
        AgentId(0),
        Box::new(Waiter {
            ready: Arc::clone(&ready),
            polls: Arc::clone(&polls),
            done: Arc::clone(&done),
        }),
    );

    // The event fires before the task has ever run, let alone parked.
    // Recording it is what stops the task sleeping through its wake.
    assert!(!cluster.wake(id), "nothing is parked yet");

    // The pass polls it, sees Pending, and finds the recorded wake — so
    // it re-queues and polls again instead of going to sleep.
    let drive = cluster.drive_once(AgentId(0), AgentRole::Worker);
    assert_eq!(drive, Drive::Ran(2), "the recorded wake must be consumed");
    assert_eq!(polls.load(Ordering::Relaxed), 2);
    assert_eq!(cluster.parked_count(), 1, "and then it parks for real");

    // Still reachable afterwards.
    ready.store(true, Ordering::Release);
    assert!(cluster.wake(id));
    cluster.drive_once(AgentId(0), AgentRole::Worker);
    assert_eq!(done.load(Ordering::Relaxed), 1);
}

#[test]
fn a_wake_reaches_an_agent_that_has_gone_to_sleep() {
    let ready = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let polls = Arc::new(AtomicUsize::new(0));
    let done = Arc::new(AtomicUsize::new(0));

    let cluster = Arc::new(Cluster::new(2, SpinPark::default(), StdClock::new()));
    let id = cluster.spawn_on(
        AgentId(1),
        Box::new(Waiter {
            ready: Arc::clone(&ready),
            polls: Arc::clone(&polls),
            done: Arc::clone(&done),
        }),
    );

    let worker = {
        let cluster = Arc::clone(&cluster);
        thread::spawn(move || cluster.run(AgentId(1)))
    };

    // It runs the task once, parks the task, then parks itself — there
    // is genuinely nothing left to do.
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
    while cluster.parked_count() == 0 {
        assert!(std::time::Instant::now() < deadline, "task never parked");
        thread::yield_now();
    }

    ready.store(true, Ordering::Release);
    cluster.wake(id);

    while done.load(Ordering::Relaxed) == 0 {
        assert!(
            std::time::Instant::now() < deadline,
            "a sleeping agent missed a task wake"
        );
        thread::yield_now();
    }

    cluster.shutdown();
    worker.join().unwrap();
}

#[test]
fn park_slots_are_addressable_aligned_and_distinct() {
    let cluster = Cluster::new(4, SpinPark::default(), StdClock::new());

    let addrs: Vec<usize> = (0..4).map(|i| cluster.park_slot_addr(AgentId(i))).collect();

    for (i, &a) in addrs.iter().enumerate() {
        let id = AgentId(i as u32);
        // Atomics.waitAsync and memory.atomic.wait32 both throw on a
        // misaligned address rather than tolerating one.
        assert_eq!(a % 4, 0, "agent {i} slot is not four-byte aligned");
        // Stable: a host hands this to JS once at start-up.
        assert_eq!(a, cluster.park_slot_addr(id));
    }

    // Distinct, and far enough apart not to share a cache line — two
    // agents spinning on one line is measurable false sharing.
    for i in 0..addrs.len() {
        for j in (i + 1)..addrs.len() {
            let gap = addrs[i].abs_diff(addrs[j]);
            assert!(
                gap >= 64,
                "agents {i} and {j} share a cache line (gap {gap})"
            );
        }
    }
}
