//! Bringing an agent into existence — the one thing wasm cannot do for
//! itself.
//!
//! Parking and waking turned out to need no host involvement at all
//! (see [`crate::park`]): they are instructions. Creating an agent is
//! different. A wasm module cannot start a Web Worker, and it should not
//! pretend to know how one is started, because that answer differs by
//! where the module runs — `new Worker()` in a browser, `wasi_thread_spawn`
//! under a WASI engine that still has it, a native `std::thread` in a
//! test harness.
//!
//! ## Why a hook rather than a declared import
//!
//! The tempting shape is `(import "krio" "agent_spawn" ...)`. It is the
//! wrong one: a declared import must be supplied at *instantiation*,
//! even by a host that never spawns a second agent, so every
//! single-agent build would have to stub it. An installable hook lets a
//! module that never clusters instantiate with no host support, and lets
//! a harness point spawning at whatever it actually has.
//!
//! A host that does want the import shape simply installs a hook that
//! calls it — that is one line of glue, and it stays in the host's hands
//! rather than in the module's ABI.
//!
//! ## What a browser host installs
//!
//! ```text
//! krio_wasm::set_spawn(|agent| {
//!     // JS side, via whatever bindings the host uses:
//!     //   const w = new Worker(url, { type: "module" });
//!     //   w.postMessage({ module, memory, agent: agent.0, tlsBase });
//!     // The worker instantiates the SAME module against the SAME
//!     // memory, runs __wasm_init_tls on its own block, sets its stack
//!     // pointer, and calls cluster.run(AgentId(agent)).
//!     Ok(())
//! });
//! ```
//!
//! Two invariants a host must not break, both of which produce silent
//! corruption rather than a clean failure:
//!
//! * the worker instantiates the **same module** against the **same
//!   memory** — a suspension must never resume on a different instance;
//! * the worker does **not** re-run data-segment initialisation, or it
//!   resets every shared static (the epoch clock included) under the
//!   agents already running.

use core::sync::atomic::{AtomicUsize, Ordering};

use krio_runtime::AgentId;

/// How a host creates agent `n`.
///
/// Returns once the agent has been *requested*, not once it is running —
/// a browser worker takes a network round trip to start. The cluster
/// does not care: an agent that has not appeared yet simply has nothing
/// stolen from it.
pub type SpawnFn = fn(AgentId) -> Result<(), SpawnError>;

/// Why an agent could not be created.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SpawnError {
    /// No host hook installed. On wasm this is the normal state until
    /// the host calls [`set_spawn`]; there is no default because there
    /// is no way for the module to guess.
    NoHost,
    /// The host declined — out of workers, isolation missing, policy.
    Refused,
}

impl core::fmt::Display for SpawnError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            SpawnError::NoHost => f.write_str(
                "krio-wasm: no agent spawner installed — call krio_wasm::set_spawn() \
                 with a hook that starts a worker",
            ),
            SpawnError::Refused => f.write_str("krio-wasm: the host refused to start an agent"),
        }
    }
}

static SPAWN: AtomicUsize = AtomicUsize::new(0);

/// Install the host's agent spawner.
///
/// Last one wins. Lives in shared memory like every other static here,
/// so installing it on the bootstrapping agent covers the cluster.
pub fn set_spawn(spawner: SpawnFn) {
    SPAWN.store(spawner as usize, Ordering::Release);
}

/// Ask the host to bring up agent `agent`.
pub fn spawn_agent(agent: AgentId) -> Result<(), SpawnError> {
    let installed = SPAWN.load(Ordering::Acquire);
    if installed == 0 {
        return Err(SpawnError::NoHost);
    }
    // SAFETY: only ever written by `set_spawn` from a `SpawnFn`, and a
    // function pointer stays valid for the life of the program.
    let spawner: SpawnFn = unsafe { core::mem::transmute(installed) };
    spawner(agent)
}

/// Is a spawner installed?
///
/// Worth checking before building a multi-agent cluster, so a host can
/// fall back to one agent deliberately instead of discovering it when
/// the first spawn fails.
pub fn can_spawn() -> bool {
    SPAWN.load(Ordering::Acquire) != 0
}
