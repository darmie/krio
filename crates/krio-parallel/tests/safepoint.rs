//! Stopping the world: does every agent actually reach the barrier, and
//! is nothing mutating while it is held?

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::thread;
use std::time::{Duration, Instant};

use krio_core::{Clock, Suspension, Task};
use krio_parallel::{Cluster, Park};
use krio_runtime::{AgentId, AgentRole, Drive, ParallelScheduler};

/// A `Park` that hands the core back.
///
/// Not `SpinPark`: that never yields to the OS, so a machine running
/// more agents than it has cores makes no progress on the ones it is
/// waiting for. This whole file runs five clusters at once, which is
/// exactly that situation — and a real std host would give the
/// scheduler a blocking park anyway, so this is also the more honest
/// backend to test against.
struct YieldPark;

impl Park for YieldPark {
    fn park(&self, slot: &std::sync::atomic::AtomicU32, expected: u32) {
        for _ in 0..64 {
            if slot.load(Ordering::Acquire) != expected {
                return;
            }
            std::hint::spin_loop();
        }
        thread::yield_now();
    }
    fn unpark(&self, _slot: &std::sync::atomic::AtomicU32) {}
}

struct StdClock(Instant);
impl Clock for StdClock {
    fn now_ms(&self) -> f64 {
        self.0.elapsed().as_secs_f64() * 1000.0
    }
}
fn clock() -> StdClock {
    StdClock(Instant::now())
}

/// Bumps a shared counter on every step. Standing in for heap mutation:
/// if this moves while the world is stopped, a collector would be
/// scanning under a running mutator.
struct Mutator {
    rounds: u32,
    ticks: Arc<AtomicUsize>,
}

impl Task for Mutator {
    fn step(&mut self) -> Suspension {
        self.ticks.fetch_add(1, Ordering::Relaxed);
        if self.rounds == 0 {
            Suspension::Completed
        } else {
            self.rounds -= 1;
            Suspension::Yielded
        }
    }
}

#[test]
fn nothing_mutates_while_the_world_is_stopped() {
    const AGENTS: u32 = 4;
    let ticks = Arc::new(AtomicUsize::new(0));
    let cluster = Arc::new(Cluster::new(AGENTS, YieldPark, clock()));

    for _ in 0..256 {
        cluster.spawn(Box::new(Mutator {
            rounds: 200,
            ticks: Arc::clone(&ticks),
        }));
    }

    let workers: Vec<_> = (1..AGENTS)
        .map(|i| {
            let c = Arc::clone(&cluster);
            thread::spawn(move || c.run(AgentId(i)))
        })
        .collect();

    // Let them get properly busy first, so the barrier has to interrupt
    // real work rather than catching an idle cluster.
    let deadline = Instant::now() + Duration::from_secs(10);
    while ticks.load(Ordering::Relaxed) < 500 {
        assert!(Instant::now() < deadline, "cluster never got going");
        thread::yield_now();
    }

    for _ in 0..20 {
        let observed = cluster
            .stop_the_world(AgentId(0), || {
                let before = ticks.load(Ordering::SeqCst);
                // If any agent were still stepping, this would move.
                thread::sleep(Duration::from_millis(2));
                let after = ticks.load(Ordering::SeqCst);
                (before, after)
            })
            .expect("no other stop is in flight");
        assert_eq!(
            observed.0, observed.1,
            "a task ran during stop-the-world — the heap was being mutated"
        );
    }

    // The cluster still works afterwards. Checked with a *fresh* task,
    // not by watching the old ones: twenty stops is long enough for the
    // original work to finish, and "the counter stopped moving" would
    // then mean success rather than a wedge.
    let after_ticks = Arc::new(AtomicUsize::new(0));
    cluster.spawn(Box::new(Mutator {
        rounds: 2,
        ticks: Arc::clone(&after_ticks),
    }));
    let deadline = Instant::now() + Duration::from_secs(10);
    while after_ticks.load(Ordering::SeqCst) < 3 {
        assert!(
            Instant::now() < deadline,
            "cluster wedged after stop-the-world"
        );
        thread::yield_now();
    }

    cluster.shutdown();
    for w in workers {
        w.join()
            .expect("an agent did not come back from the barrier");
    }
}

#[test]
fn idle_agents_still_arrive_at_the_barrier() {
    const AGENTS: u32 = 4;
    let cluster = Arc::new(Cluster::new(AGENTS, YieldPark, clock()));

    // No work at all: every agent goes straight to sleep. They are not
    // mutating, but the barrier counts arrivals, so they must wake and
    // report in or this hangs.
    let workers: Vec<_> = (1..AGENTS)
        .map(|i| {
            let c = Arc::clone(&cluster);
            thread::spawn(move || c.run(AgentId(i)))
        })
        .collect();
    thread::sleep(Duration::from_millis(30));

    let ran = cluster
        .stop_the_world(AgentId(0), || cluster.world_is_stopped())
        .expect("stop should be granted");
    assert!(ran, "all sleeping agents must be counted as stopped");

    cluster.shutdown();
    for w in workers {
        w.join().unwrap();
    }
}

#[test]
fn a_second_stop_is_refused_rather_than_interleaved() {
    let cluster = Cluster::new(1, YieldPark, clock());
    // Single agent: the requester is the only one, so no waiting.
    let out = cluster.stop_the_world(AgentId(0), || {
        // Re-entering must not be allowed — two collectors believing they
        // both own the world is worse than one being told no.
        cluster
            .stop_the_world(AgentId(0), || unreachable!())
            .is_none()
    });
    assert_eq!(out, Some(true));

    // And the world is usable again afterwards.
    assert!(cluster.stop_the_world(AgentId(0), || 7) == Some(7));
}

#[test]
fn the_main_agent_can_stop_the_world_without_blocking() {
    const AGENTS: u32 = 3;
    let ticks = Arc::new(AtomicUsize::new(0));
    let cluster = Arc::new(Cluster::new(AGENTS, YieldPark, clock()));
    for _ in 0..64 {
        cluster.spawn(Box::new(Mutator {
            rounds: 500,
            ticks: Arc::clone(&ticks),
        }));
    }
    let workers: Vec<_> = (1..AGENTS)
        .map(|i| {
            let c = Arc::clone(&cluster);
            thread::spawn(move || c.run(AgentId(i)))
        })
        .collect();

    // The non-blocking path: ask, keep driving, poll.
    assert!(cluster.request_safepoint());
    let deadline = Instant::now() + Duration::from_secs(10);
    while !cluster.world_is_stopped() {
        assert!(
            Instant::now() < deadline,
            "workers never reached the barrier"
        );
        // A main agent keeps its event loop alive meanwhile. `drive_once`
        // must return rather than park while a stop is pending.
        assert_ne!(
            cluster.drive_once(AgentId(0), AgentRole::Main),
            Drive::ShuttingDown
        );
    }

    let before = ticks.load(Ordering::SeqCst);
    thread::sleep(Duration::from_millis(2));
    assert_eq!(
        before,
        ticks.load(Ordering::SeqCst),
        "world not actually stopped"
    );

    cluster.resume_world();
    cluster.shutdown();
    for w in workers {
        w.join().unwrap();
    }
}

/// The case the poll exists for: one `step` that runs long and would
/// otherwise hold the whole cluster hostage.
#[test]
fn a_hot_loop_reaches_the_barrier_by_polling() {
    struct HotLoop {
        cluster: Arc<Cluster<YieldPark, StdClock>>,
        polled: Arc<AtomicUsize>,
        stop: Arc<AtomicBool>,
        entered: Arc<AtomicBool>,
    }
    impl Task for HotLoop {
        fn step(&mut self) -> Suspension {
            self.entered.store(true, Ordering::Release);
            // A game loop: milliseconds inside one step, never returning
            // to the scheduler on its own.
            while !self.stop.load(Ordering::Relaxed) {
                // The back-edge poll. One relaxed load.
                if self.cluster.safepoint_requested() {
                    self.polled.fetch_add(1, Ordering::Relaxed);
                    self.cluster.enter_safepoint();
                }
                std::hint::spin_loop();
            }
            Suspension::Completed
        }
    }

    let cluster = Arc::new(Cluster::new(2, YieldPark, clock()));
    let polled = Arc::new(AtomicUsize::new(0));
    let stop = Arc::new(AtomicBool::new(false));
    let entered = Arc::new(AtomicBool::new(false));

    cluster.spawn_on(
        AgentId(1),
        Box::new(HotLoop {
            cluster: Arc::clone(&cluster),
            polled: Arc::clone(&polled),
            stop: Arc::clone(&stop),
            entered: Arc::clone(&entered),
        }),
    );

    let worker = {
        let c = Arc::clone(&cluster);
        thread::spawn(move || c.run(AgentId(1)))
    };

    // Wait for the task to actually be inside its loop rather than
    // sleeping and hoping. If the stop request lands first the agent is
    // still idle, reaches the barrier through `run`'s own check, and
    // never polls — so `polled` stays 0 and the assertion below fires.
    // That panic then skips the cleanup underneath it, leaving the
    // worker spinning on `!stop` forever, and the process hangs instead
    // of failing. Cheap to wait; expensive to guess.
    let deadline = Instant::now() + Duration::from_secs(10);
    while !entered.load(Ordering::Acquire) {
        assert!(Instant::now() < deadline, "hot loop never started");
        thread::yield_now();
    }

    // Without the poll this would hang: the agent is inside one step and
    // nothing can interrupt it.
    let got = cluster.stop_the_world(AgentId(0), || "collected");
    assert_eq!(got, Some("collected"));
    assert!(
        polled.load(Ordering::Relaxed) >= 1,
        "the hot loop should have hit its back-edge poll"
    );

    stop.store(true, Ordering::Relaxed);
    cluster.shutdown();
    worker.join().unwrap();
}
