//! The one assertion no engine harness can make: that a browser's main
//! thread can drive the cluster without throwing.
//!
//! `memory.atomic.wait32` throws unconditionally on the main thread —
//! the web platform will not let the UI thread block — and nothing in
//! the type system stops an agent from reaching it, because the same
//! [`WasmPark`] is legitimately shared by every agent in the cluster.
//! The rule is enforced structurally instead: `run()` parks and is
//! worker-only, `drive_once()` never parks. That is a claim about a
//! code path, and the only place it can be *checked* is a real main
//! thread.
//!
//! wasmtime cannot show this. Every agent there may block, so a
//! `drive_once` that wrongly parked would simply sleep and the suite
//! would still pass. Here, the same mistake is an uncaught
//! `RuntimeError` and the test fails.
//!
//! Run with a matching ChromeDriver:
//!
//! ```text
//! CHROMEDRIVER=$(which chromedriver) \
//! RUSTFLAGS='-Ctarget-feature=+atomics,+bulk-memory,+mutable-globals' \
//!   cargo test -p krio-wasm --target wasm32-unknown-unknown \
//!   -Zbuild-std=std,panic_abort --test browser
//! ```
//!
//! ## Without a driver
//!
//! ChromeDriver has to match Chrome's major version exactly, which is a
//! reliable way to be blocked on a developer machine — Homebrew disabled
//! its matching build over a Gatekeeper failure while this was written.
//! No driver is needed. `NO_HEADLESS=1` makes the runner serve the tests
//! and wait instead of driving a browser, and any browser can then load
//! them:
//!
//! ```text
//! NO_HEADLESS=1 RUSTFLAGS='-Ctarget-feature=+atomics,+bulk-memory,+mutable-globals' \
//!   cargo test -p krio-wasm --target wasm32-unknown-unknown \
//!   -Zbuild-std=std,panic_abort --test browser
//! # → Interactive browsers tests are now available at http://127.0.0.1:8000
//!
//! # open that URL, or scrape it without touching a driver:
//! "/Applications/Google Chrome.app/Contents/MacOS/Google Chrome" \
//!   --headless=new --virtual-time-budget=20000 --dump-dom \
//!   http://127.0.0.1:8000/
//! ```
//!
//! The one thing that would otherwise bite is cross-origin isolation:
//! without it there is no `SharedArrayBuffer` and an atomics module will
//! not instantiate at all. The runner's own server already sends
//! `Cross-Origin-Opener-Policy: same-origin` and
//! `Cross-Origin-Embedder-Policy: require-corp`, so this works as-is.
//!
//! The trade for skipping the driver is that a failure shows up in the
//! page rather than in an exit code, which is why CI still uses the
//! driver — its runner image ships a matched pair.

// `target_feature = "atomics"` as well as the target itself: a plain
// wasm32-unknown-unknown build has no atomics, so `WasmPark` implements
// no `Park` there and this file would not compile. That build is a CI
// row in its own right — it checks the crate still works where the
// instructions are unavailable — so this has to disappear on it rather
// than break it.
#![cfg(all(
    target_arch = "wasm32",
    target_os = "unknown",
    target_feature = "atomics"
))]

use core::sync::atomic::{AtomicUsize, Ordering};

use krio_core::{Suspension, Task};
use krio_parallel::Cluster;
use krio_runtime::{AgentId, AgentRole, Drive, ParallelScheduler};
use krio_wasm::{EpochClock, WasmPark};

use wasm_bindgen_test::{wasm_bindgen_test, wasm_bindgen_test_configure};

wasm_bindgen_test_configure!(run_in_browser);

static COMPLETED: AtomicUsize = AtomicUsize::new(0);

struct Countdown {
    steps: u32,
}

impl Task for Countdown {
    fn step(&mut self) -> Suspension {
        if self.steps == 0 {
            COMPLETED.fetch_add(1, Ordering::Relaxed);
            Suspension::Completed
        } else {
            self.steps -= 1;
            Suspension::Yielded
        }
    }
}

#[wasm_bindgen_test]
fn the_build_reports_cluster_support() {
    let support = krio_wasm::cluster_support()
        .expect("browser test must be built with -Ctarget-feature=+atomics");
    assert!(support.wasm);
    assert!(support.atomics);
}

/// The point of the file. If `drive_once` ever reached `Park::park`,
/// this would throw rather than fail an assertion.
#[wasm_bindgen_test]
fn the_main_agent_drives_a_cluster_without_blocking() {
    const TASKS: usize = 64;
    let before = COMPLETED.load(Ordering::Relaxed);

    // A real WasmPark, not a spin fallback: the whole question is
    // whether the main-agent path can hold one without touching it.
    let cluster = Cluster::new(4, WasmPark::new(), EpochClock::new());

    for _ in 0..TASKS {
        cluster.spawn(Box::new(Countdown { steps: 3 }));
    }

    // No workers exist — nobody else will ever run these — so the main
    // agent has to drain the whole cluster through bounded passes.
    let mut passes = 0;
    while COMPLETED.load(Ordering::Relaxed) - before < TASKS {
        assert_ne!(
            cluster.drive_once(AgentId(0), AgentRole::Main),
            Drive::ShuttingDown,
            "cluster shut down unexpectedly"
        );
        passes += 1;
        assert!(
            passes < 10_000,
            "main agent made no progress: {}/{TASKS}",
            COMPLETED.load(Ordering::Relaxed) - before
        );
    }

    assert_eq!(COMPLETED.load(Ordering::Relaxed) - before, TASKS);
}

/// An idle main agent must report `Idle` and hand control back, not
/// wait for work to appear.
#[wasm_bindgen_test]
fn an_idle_main_agent_returns_instead_of_waiting() {
    let cluster = Cluster::new(2, WasmPark::new(), EpochClock::new());

    // With nothing queued, a worker would park here. The main agent
    // must come straight back so the event loop keeps turning.
    assert_eq!(cluster.drive_once(AgentId(0), AgentRole::Main), Drive::Idle);
    assert!(!cluster.has_work());
}

/// A task that parks is set aside without the main agent blocking on
/// it, and a wake from the same agent brings it back.
#[wasm_bindgen_test]
fn waiting_tasks_do_not_block_the_main_agent() {
    struct Waiter;
    impl Task for Waiter {
        fn step(&mut self) -> Suspension {
            Suspension::Pending
        }
    }

    let cluster = Cluster::new(1, WasmPark::new(), EpochClock::new());
    let id = cluster.spawn_on(AgentId(0), Box::new(Waiter));

    assert_eq!(
        cluster.drive_once(AgentId(0), AgentRole::Main),
        Drive::Ran(1)
    );
    assert_eq!(cluster.parked_count(), 1);

    // Idle, not blocked — the UI thread is free to go back to painting.
    assert_eq!(cluster.drive_once(AgentId(0), AgentRole::Main), Drive::Idle);

    assert!(cluster.wake(id));
    assert_eq!(cluster.parked_count(), 0);
}
