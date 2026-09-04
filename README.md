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
| `krio-fiber`     | ✅ shipped — Fiber on x86_64 (SysV + MS x64), aarch64 (non-Windows, incl. iOS), x86-32 (SysV) and riscv64 (RV64GC); host-routed `yield_now` elsewhere |
| `krio-async`     | ✅ Phase 3 v2 — direct-yield + captures lift + cross-fn dispatch + multi-suspension blocks |
| `krio-preempt`   | 🟨 v1 — TimeSliceScheduler (cooperative slicing); real signal preempt deferred |
| `krio-parallel`  | 🟨 v1 — Cluster: bounded Chase–Lev deques, injector overflow, role-derived budgets, waker registry, stop-the-world safepoints. Needs a `Park` backend per target |
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

### Waiting tasks

`Suspension` draws a distinction worth honouring: `Yielded` means *give
someone else a turn*, `Pending` means *I am waiting on something*.
Re-queueing both loses nothing, but an agent holding one `Pending` task
spins a core polling a channel that cannot have changed. So a `Pending`
task is moved out of the run queues until it is woken:

```rust
// The host owns the channel, so the host says when it is ready.
// The TaskId comes from TaskObserver::on_step_begin.
cluster.wake(task_id);
```

krio does not own channels — the stackless transform emits the peek and
leaves the recv to the host — so it cannot know when a wait is over, and
does not guess. Threading a waker through `Task::step` would be the
other design, and would mean adding a parameter to a trait that
`RoundRobin` and `Fiber` also implement and neither could use.

Waking early is safe. A wake that arrives before the task has finished
parking is recorded and applied when it does, so the race between "step
returned Pending" and "the event fired on another agent" cannot lose a
wakeup.

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

### Fibers on WebAssembly

`Fiber` is **not** available on wasm, and says so rather than pretending:

```text
krio-fiber: native fibers are unavailable on this target
```

A wasm module has no addressable stack and no instruction that moves
between two, so there is nothing to switch. That stays true until the
stack-switching proposal ships (it is Phase 3).

Suspension is a different question, and it *is* available. The
capability exists one level up in the host — JSPI in a browser (Chrome
137+, Firefox 153+, Safari 27 beta), `wasmtime`'s async support on a
server, an explicit scheduler anywhere else — so `yield_now()` routes
there instead of switching:

```rust
krio_fiber::set_suspender(|| host_suspend());   // wasm targets only
```

That keeps library code written against krio's free functions —
`yield_now`, `should_yield_early`, `is_cancelled` — compiling and
behaving on wasm, which matters because those calls are scattered
through a language's standard library, far from any scheduler.

Without a suspender installed, `yield_now()` panics. A suspend that
silently did nothing would turn a yield point into a no-op and change
what the program means.

**krio deliberately does not choose how.** There are three ways a host
can make that call suspend — engine suspension via JSPI, a worker per
fiber over shared memory, an Asyncify transform — and they are not
interchangeable. Which is right depends on where the module runs, which
is the harness's knowledge rather than the program's. So the program
marks the point at which it can be suspended, and the host decides how.

One invariant a host must not break: never return from a suspension on a
different instance, or with a different memory.


### Stopping the world

A collector needs every agent stopped somewhere safe before it can scan,
and only the scheduler knows where those points are:

```rust
cluster.stop_the_world(AgentId(0), || collector.collect());
```

krio takes no view on what happens inside — it does not know what a root
is. It only guarantees that while the closure runs, no other agent is
inside `Task::step`.

Agents reach the barrier three ways. Between steps the scheduler checks
on their behalf, so a task made of many short steps costs its author
nothing. An idle agent is woken to report in — it is not mutating, but
the barrier counts arrivals. The third is the hard one.

**Hot loops.** A game loop or physics tick is one `step` that runs for
milliseconds and never returns to the scheduler. wasm has no signals and
no way to suspend another agent's stack, so nothing can interrupt it —
the loop has to ask, which is the answer HotSpot and Go reach too:

```rust
while running {
    if cluster.safepoint_requested() {   // one relaxed load
        cluster.enter_safepoint();
    }
    ...
}
```

A host that never polls corrupts nothing; it just cannot be collected
until that loop finishes, and `stop_the_world` waits. A pause is
diagnosable, a torn heap is not.

The browser main thread cannot block, so it uses `request_safepoint()`,
polls `world_is_stopped()`, and calls `resume_world()` when finished.

## Architecture support

`krio-fiber` needs a context switch per (architecture, ABI). Everything
else in the family is portable Rust and builds anywhere.

| arch / ABI | fibers | verified by |
|---|---|---|
| x86_64 SysV (Linux, macOS) | ✅ | native CI, both profiles |
| x86_64 MS x64 (Windows) | ✅ | native CI, both profiles |
| aarch64 (Linux, macOS, **iOS**) | ✅ | native CI, both profiles |
| **x86-32 SysV** (i686) | ✅ | qemu / 32-bit compat, both profiles |
| **riscv64 (RV64GC)** | ✅ | qemu-riscv64, both profiles |
| aarch64 Windows | ❌ panic stub | TEB swap tried and insufficient; see `arch.rs` |
| wasm32 | ❌ panic stub | no switchable stack exists |

Two stubs remain. wasm32 has no switchable stack at all — see
`krio_fiber::set_suspender` for what runs there instead. ARM64 Windows
is closer but not there: the TEB stack-bounds swap was written and run
on a `windows-11-arm` runner, and release still dies with `0xe06d7363`
on the first panic crossing a fiber boundary while debug passes. The
suspect is missing SEH unwind data on the asm blocks rather than the
TEB pair; `arch.rs` records the detail.

Windows fiber stacks have no guard page on either architecture:
`stack.rs` falls back to a heap `Box<[u8]>` off unix, so an overflow
corrupts the heap rather than trapping. That is a Windows-wide gap, not
an architecture one, and wants `VirtualAlloc` + `PAGE_NOACCESS`.

iOS needs nothing special: `aarch64-apple-ios` is `target_arch =
"aarch64"`, so it takes the same AAPCS64 switch and
`mmap` guard-page stack as macOS, which CI exercises natively.

Targets without a switch are not stuck. `krio_fiber::set_suspender`
routes `yield_now` to a host suspender there, and the rest of the family
— including `krio-parallel`, which is `no_std` and never spawns a thread
of its own — works regardless. A single-agent cluster driven by
`drive_once` needs no threading support at all.


## License

MIT OR Apache-2.0 (see [LICENSE](LICENSE)).

