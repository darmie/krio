//! krio-wasm — the WebAssembly backend for [`krio_parallel`], and the
//! only crate in the family that knows browsers exist.
//!
//! Everything target-specific about running krio across Web Workers is
//! quarantined here: how an idle agent sleeps, where the clock comes
//! from, and how an agent is created. `krio-parallel` stays portable and
//! testable on native hardware because this crate exists.
//!
//! ## What a host has to provide
//!
//! Less than the first draft of this design assumed. Parking and waking
//! are core wasm instructions, so the host is not involved in them at
//! all — the whole contract is:
//!
//! 1. **A way to start an agent** — [`set_spawn`]. See [`agent`].
//! 2. **A clock tick** — call [`publish_epoch_ms`] on an interval. See
//!    [`clock`].
//! 3. **The build and the headers** — below.
//!
//! ## The build is not a cargo feature
//!
//! Shared memory is a whole-program mode: every crate in the graph,
//! `std` included, has to be compiled with the same target features,
//! which is why this is a documented contract rather than something a
//! dependency can switch on quietly.
//!
//! ```toml
//! # .cargo/config.toml
//! [unstable]
//! build-std = ["std", "panic_abort"]
//!
//! [build]
//! target = "wasm32-unknown-unknown"
//! rustflags = ["-Ctarget-feature=+atomics,+bulk-memory,+mutable-globals"]
//! ```
//!
//! Plus, on the serving side, cross-origin isolation —
//! `Cross-Origin-Opener-Policy: same-origin` and
//! `Cross-Origin-Embedder-Policy: require-corp`. Without both headers
//! `SharedArrayBuffer` is unavailable and there is no cluster to build.
//!
//! ## Degrade honestly
//!
//! [`cluster_support`] answers what the *build* can do; a host still has
//! to check cross-origin isolation itself, since that is a property of
//! the page rather than the module. Refuse to start a cluster when
//! either is missing rather than quietly running one agent — a Tier 1
//! program silently running at Tier 0 reads as a performance bug months
//! later, and the family already prefers an honest failure to a
//! plausible wrong answer.
//!
//! ## Driving the cluster from the main thread
//!
//! A worker calls `run()` and blocks; the main thread cannot. It drives
//! one bounded pass and returns to the event loop, then needs waking
//! when work appears. `Atomics.waitAsync` does that against the *same*
//! parking slot a worker would block on, which is why a waker only ever
//! issues one `notify` and never branches on who it is waking.
//!
//! ```js
//! const slots = new Int32Array(memory.buffer);
//! const PARKED = 1;
//!
//! function pump() {
//!   // One bounded pass. Returns Idle (0) when there was nothing to do.
//!   const drove = exports.krio_drive_main();
//!   if (drove !== 0) {
//!     // Still work: yield to the event loop, then come straight back
//!     // so rendering and input get a turn between passes.
//!     queueMicrotask(pump);
//!     return;
//!   }
//!   // Genuinely idle. Wait on the slot instead of polling — this is
//!   // the non-blocking twin of memory.atomic.wait32, and the only
//!   // form of waiting the main thread is allowed.
//!   const idx = exports.krio_park_slot_addr(0) >>> 2;
//!   const w = Atomics.waitAsync(slots, idx, PARKED);
//!   (w.async ? w.value : Promise.resolve()).then(pump);
//! }
//! ```
//!
//! The Rust side is three exports over
//! [`krio_parallel::Cluster`]: `drive_once(AgentId(0), AgentRole::Main)`,
//! [`krio_parallel::Cluster::park_slot_addr`], and whatever the host
//! spawns work through. Note `Atomics.waitAsync` returns
//! `{async: false}` when the value already differs — work landed while
//! the pass was ending — so the `w.async` branch is what stops a wake
//! being missed at exactly the moment it matters.
//!
//! ## Putting it together
//!
//! Not compiled as a doctest: `WasmPark` only implements `Park` on a
//! build with the `atomics` feature, so this example cannot type-check
//! on the machine running `cargo test`. The equivalent is exercised for
//! real in `tests/shared_memory.rs` under a wasm engine.
//!
//! ```ignore
//! use krio_parallel::Cluster;
//! use krio_runtime::{AgentId, AgentRole, ParallelScheduler};
//! use krio_wasm::{EpochClock, WasmPark, cluster_support};
//!
//! # fn main() -> Result<(), Box<dyn std::error::Error>> {
//! cluster_support().ok_or("no shared-memory support in this build")?;
//!
//! krio_wasm::set_spawn(|_agent| Ok(())); // host starts a Worker here
//! let cluster = Cluster::new(4, WasmPark::new(), EpochClock::new());
//!
//! // Worker agents block; the main agent must not.
//! # let on_main_thread = true;
//! if on_main_thread {
//!     cluster.drive_once(AgentId(0), AgentRole::Main);
//! } else {
//!     cluster.run(AgentId(1));
//! }
//! # Ok(())
//! # }
//! ```

// `memory.atomic.wait32` / `notify` are still unstable in core::arch, so
// the atomics path needs nightly. Gating the attribute itself — rather
// than the crate — keeps krio-wasm buildable on stable everywhere the
// instructions are not used. This costs a wasm host nothing: a
// shared-memory build already requires nightly for `-Zbuild-std`.
#![cfg_attr(
    all(target_arch = "wasm32", target_feature = "atomics"),
    feature(stdarch_wasm_atomic_wait)
)]
#![no_std]

pub mod agent;
pub mod clock;
pub mod park;

pub use agent::{SpawnError, SpawnFn, can_spawn, set_spawn, spawn_agent};
#[cfg(feature = "fiber-clock")]
pub use clock::install_fiber_clock;
pub use clock::{EpochClock, advance_epoch_ms, epoch_ms, publish_epoch_ms};
pub use park::WasmPark;

/// What this build can actually do, as opposed to what it was aimed at.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ClusterSupport {
    /// Built for wasm at all.
    pub wasm: bool,
    /// The `atomics` target feature is on, so shared-memory parking
    /// works and `Send` across agents means what it should.
    pub atomics: bool,
}

/// Report whether this build can host a multi-agent cluster.
///
/// Returns `None` when it cannot, so the natural spelling at a call site
/// is a `?` that refuses to start rather than a boolean someone forgets
/// to check.
///
/// This reflects **compile-time** capability only. Cross-origin
/// isolation is a property of the page and has to be checked host-side —
/// `crossOriginIsolated` in JS — before trusting that the memory a
/// worker receives is genuinely shared.
pub fn cluster_support() -> Option<ClusterSupport> {
    let support = ClusterSupport {
        wasm: cfg!(target_arch = "wasm32"),
        atomics: cfg!(target_feature = "atomics"),
    };
    if support.atomics { Some(support) } else { None }
}
