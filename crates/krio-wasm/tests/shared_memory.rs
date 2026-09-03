//! The real thing: a krio cluster driven by several agents over shared
//! linear memory, parking on `memory.atomic.wait32`.
//!
//! Everything else in the family can be checked on native hardware.
//! These cannot — they are the assertions that only mean something when
//! the parking is a wasm instruction against a shared memory, so they
//! compile away everywhere else.
//!
//! Run with:
//!
//! ```text
//! CARGO_TARGET_WASM32_WASIP1_THREADS_RUNNER='wasmtime -W threads=y -W shared-memory=y -S threads=y' \
//!   cargo test -p krio-wasm --target wasm32-wasip1-threads
//! ```
//!
//! `wasm32-wasip1-threads` is a stand-in for a browser, not the
//! production target — it has `atomics` on by default and one instance
//! per agent sharing one memory, which is the same shape a Worker
//! cluster has. WASI itself has moved on (threads were removed from
//! Preview 2), which is exactly why agent creation is a host hook rather
//! than a WASI import: this harness swaps for a browser one without
//! touching the crate.

#![cfg(all(target_arch = "wasm32", target_feature = "atomics"))]

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::thread;
use std::time::{Duration, Instant};

use krio_core::{Suspension, Task};
use krio_parallel::Cluster;
use krio_runtime::{AgentId, AgentRole, ParallelScheduler};
use krio_wasm::{EpochClock, WasmPark, publish_epoch_ms};

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

#[test]
fn this_build_can_host_a_cluster() {
    let support = krio_wasm::cluster_support().expect("atomics build must report support");
    assert!(support.wasm);
    assert!(support.atomics);
}

#[test]
fn agents_share_one_queue_over_shared_memory() {
    const TASKS: usize = 1_000;
    let done = Arc::new(AtomicUsize::new(0));
    let cluster = Arc::new(Cluster::new(4, WasmPark::new(), EpochClock::new()));

    for _ in 0..TASKS {
        cluster.spawn(countdown(3, &done));
    }

    // Three worker agents, each its own wasm instance over the same
    // linear memory, parking on the atomic-wait instruction when dry.
    let workers: Vec<_> = (1..4)
        .map(|i| {
            let cluster = Arc::clone(&cluster);
            thread::spawn(move || cluster.run(AgentId(i)))
        })
        .collect();

    // Agent 0 stands in for a main thread: bounded passes, never parks.
    let deadline = Instant::now() + Duration::from_secs(60);
    while done.load(Ordering::Relaxed) < TASKS {
        cluster.drive_once(AgentId(0), AgentRole::Main);
        assert!(
            Instant::now() < deadline,
            "cluster stalled at {}/{TASKS}",
            done.load(Ordering::Relaxed)
        );
    }

    assert_eq!(done.load(Ordering::Relaxed), TASKS);

    cluster.shutdown();
    for w in workers {
        w.join().expect("an agent failed to come out of run()");
    }
}

/// The assertion this whole file exists for: an agent genuinely asleep
/// inside `memory.atomic.wait32` is woken by another agent's
/// `memory.atomic.notify`, with no host involvement on either side.
#[test]
fn a_parked_agent_is_woken_by_the_atomic_notify() {
    let done = Arc::new(AtomicUsize::new(0));
    let cluster = Arc::new(Cluster::new(2, WasmPark::new(), EpochClock::new()));

    let worker = {
        let cluster = Arc::clone(&cluster);
        thread::spawn(move || cluster.run(AgentId(1)))
    };

    // Give it time to run dry, publish PARKED and actually block. With
    // an indefinite timeout, nothing but a notify can bring it back.
    thread::sleep(Duration::from_millis(50));
    assert_eq!(done.load(Ordering::Relaxed), 0);

    cluster.spawn(countdown(0, &done));

    let deadline = Instant::now() + Duration::from_secs(30);
    while done.load(Ordering::Relaxed) == 0 {
        assert!(
            Instant::now() < deadline,
            "a parked agent was never woken — the notify did not land"
        );
        thread::yield_now();
    }

    cluster.shutdown();
    worker.join().expect("agent did not shut down");
}

#[test]
fn shutdown_wakes_agents_blocked_indefinitely() {
    let cluster = Arc::new(Cluster::new(3, WasmPark::new(), EpochClock::new()));

    let workers: Vec<_> = (0..3)
        .map(|i| {
            let cluster = Arc::clone(&cluster);
            thread::spawn(move || cluster.run(AgentId(i)))
        })
        .collect();

    // They block having never seen a task. With no timeout there is no
    // escape hatch, so this is a real test of the shutdown notify rather
    // than of a spin loop noticing a flag.
    thread::sleep(Duration::from_millis(50));
    cluster.shutdown();

    for w in workers {
        w.join().expect("an idle agent was never woken by shutdown");
    }
}

/// The epoch clock is a plain static, so every agent reads the same
/// address — which is only true because workers share linear memory and
/// do not re-run data-segment initialisation.
#[test]
fn every_agent_reads_the_same_epoch() {
    publish_epoch_ms(42_000);

    let readings: Vec<u64> = (0..4)
        .map(|_| thread::spawn(krio_wasm::epoch_ms))
        .map(|h| h.join().unwrap())
        .collect();

    assert!(
        readings.iter().all(|&r| r >= 42_000),
        "an agent saw a stale or zeroed epoch: {readings:?} — workers may be \
         re-initialising the data segment"
    );
}
