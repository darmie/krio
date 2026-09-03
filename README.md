# krio

> Coroutine framework family — stackless transforms, stackful fibers,
> cross-function async, preemptive scheduling — sharing a common
> vocabulary.

`krio` is a workspace of small, focused crates that together cover the
coroutine implementation strategies a language designer or runtime
author actually picks between in practice. Each variant is independent,
addresses a coherent execution model, and speaks the same vocabulary
(`Marker`, `Suspension`, `CfgId`) defined by `krio-core`.

```
krio
├── krio-core         — shared vocabulary: Marker, Suspension,
│                       CfgId, Task trait (deps: none)
├── krio-runtime      — Scheduler trait + RoundRobin scheduler;
│                       drives any Task to completion
├── krio-stackless    — per-function state-machine transform
│                       (CooperativeExecutor + WakerExecutor)
├── krio-async        — cross-function stackless state-machine
│                       transform (function-colour + frame stack)
├── krio-fiber        — Wren/Lua-style stackful runtime
│                       (Fiber implements krio-core::Task)
├── krio-preempt      — preemptive scheduler (planned)
├── krio-parallel     — work stealing across agents sharing one
│                       address space (threads / Web Workers)
└── krio-wasm         — the WebAssembly backend for krio-parallel;
                        the only crate that knows about browsers
```

## Picking a variant

| You want…                                                   | Use                |
|---|---|
| Structured concurrency blocks (`scope { ... }`), single fn  | `krio-stackless`   |
| `async fn` / `suspend fun` style with function colour        | `krio-async`       |
| First-class fibers, yield from any depth, simple programmer model | `krio-fiber`       |
| Forced timeslicing — fibers can't starve each other          | `krio-preempt`     |
| Tasks spread over OS threads or Web Workers                  | `krio-parallel`    |
| …and the target is wasm                                      | `+ krio-wasm`      |

The variants are not mutually exclusive — most languages ship two or
three. A microkernel might use `krio-stackless` for the hot path
(zero-alloc, fits in one frame) and `krio-fiber` for general user-mode
tasks (per-fiber stack, but suspension Just Works).

## Status

| Crate            | Status              |
|---|---|
| `krio-core`      | ✅ shipped          |
| `krio-runtime`   | ✅ shipped — RoundRobin scheduler |
| `krio-stackless` | ✅ shipped — CooperativeExecutor + WakerExecutor |
| `krio-fiber`     | ✅ shipped — Fiber on x86_64 (SysV + MS x64) + aarch64 (non-Windows) |
| `krio-async`     | ✅ Phase 3 v2 — direct-yield + captures lift + cross-fn dispatch + multi-suspension blocks |
| `krio-preempt`   | 🟨 v1 — TimeSliceScheduler (cooperative slicing); real signal preempt deferred |
| `krio-parallel`  | 🟨 v1 — Cluster: bounded Chase–Lev deques, injector overflow, role-derived budgets. Needs a `Park` backend per target |
| `krio-wasm`      | 🟨 v1 — shared-memory parking, epoch clock, agent-spawn hook. Verified on wasm under wasmtime; browser harness not yet written |

## Tradeoffs at a glance

| Variant         | Per-coroutine cost         | Yield from any call depth | Function colour required | Allocation |
|---|---|---|---|---|
| stackless       | 2 i64 locals + a few blocks | no                        | no                       | none       |
| async           | sized state struct          | yes                       | yes                      | per-call type, no per-instance |
| fiber           | 1 stack page (~4-32 KB)     | yes                       | no                       | per-fiber  |
| preempt         | 1 stack page + sched state  | yes                       | no                       | per-fiber  |

## Design principle

> Don't unify what doesn't unify.

Stackless and stackful are different models with different costs.
`krio` keeps them in separate crates so a consumer pays for what they
use, and the trait surfaces don't leak the wrong abstraction. The
shared vocabulary in `krio-core` is small on purpose — it's enough
that mixing variants in one program doesn't require translation
shims, but it doesn't pretend the implementations are interchangeable.

## License

MIT OR Apache-2.0 (see [LICENSE](LICENSE)).

## Running across threads or Web Workers

`krio-parallel` drives `Task`s on a **cluster** of agents sharing one
address space — OS threads natively, Web Workers in a browser built
with `+atomics`.

The currency is `Box<dyn Task + Send>`, and that bound is the whole
safety argument. Under `+atomics` a Web Worker *is* a thread as far as
the type system is concerned: thread-locals are per-instance, statics
are shared. So `Send` already means exactly "may be handed to another
agent", and no new marker trait is needed to say it.

The consequence lands where it should. A `krio_fiber::Fiber` holds a raw
pointer, so it is `!Send`, so it cannot be spawned onto a cluster at all:

```text
error[E0277]: `*mut c_void` cannot be sent between threads safely
```

That is correct rather than unfortunate — a suspended stack is not
relocatable. Balance fibers by *placement* instead, before creation:
pick the least-loaded agent and build the fiber there.

Agents are not symmetric, and the API says so. `run()` parks when idle
and belongs to a worker; `drive_once()` never parks and is the only
entry point a browser's main thread may use, because
`memory.atomic.wait32` throws there. How long a pass runs comes from
`AgentRole` rather than a setting — the value that decides how an agent
waits also decides how long it runs, so a host cannot starve a UI thread
by forgetting to configure something.

### Clocks on targets that don't have one

`SystemTime::now()` traps on `wasm32-unknown-unknown` with *time not
implemented on this platform*. `krio-fiber` therefore reads its
deadlines through an installable clock, and `krio-preempt` reads
`krio_fiber::now_ms()` rather than keeping one of its own — so a slice
and the deadline it sets are always measured against the same origin:

```rust
static EPOCH_MS: AtomicU64 = AtomicU64::new(0);   // bumped by one agent
krio_fiber::set_clock(|| EPOCH_MS.load(Ordering::Relaxed) as f64);
```

A deadline check is then one relaxed load from shared memory — no
syscall, and no crossing into JS on a path a coroutine polls in its hot
loop.

### On WebAssembly

`krio-wasm` is the backend, and the only crate in the family that knows
browsers exist. What a host has to supply turned out to be smaller than
expected:

- **Parking needs nothing.** `memory.atomic.wait32` and
  `memory.atomic.notify` are core wasm instructions, so an agent sleeps
  and is woken without the host in the loop.
- **Agent creation needs a hook** — `krio_wasm::set_spawn`. A wasm
  module cannot start a Worker, and it should not hard-code an import
  for it either: a declared import must be supplied at instantiation
  even by a host that never spawns anything.
- **The clock needs a ticker** — call `publish_epoch_ms` on an interval.

Requires `-Ctarget-feature=+atomics,+bulk-memory,+mutable-globals` with
`-Zbuild-std`, and cross-origin isolation (`COOP: same-origin` +
`COEP: require-corp`) for `SharedArrayBuffer`. `cluster_support()`
returns `None` when the build cannot host a cluster, so the natural
spelling refuses to start rather than silently running one agent.

The browser main thread never parks: `memory.atomic.wait32` throws
there. It drives the cluster with `drive_once()` and is woken by the
same `notify` via `Atomics.waitAsync` on the JS side — so a waker never
has to know which kind of agent it is waking.

The whole path is exercised on a real engine, not just compiled:
`cargo test -p krio-wasm --target wasm32-wasip1-threads` under wasmtime
runs four agents over one shared linear memory, including an agent
parked indefinitely on `atomic.wait32` and woken by another agent's
`notify`.
