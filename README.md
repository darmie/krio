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
└── krio-preempt      — preemptive scheduler (planned)
```

## Picking a variant

| You want…                                                   | Use                |
|---|---|
| Structured concurrency blocks (`scope { ... }`), single fn  | `krio-stackless`   |
| `async fn` / `suspend fun` style with function colour        | `krio-async`       |
| First-class fibers, yield from any depth, simple programmer model | `krio-fiber`       |
| Forced timeslicing — fibers can't starve each other          | `krio-preempt`     |

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
| `krio-fiber`     | ✅ shipped — Fiber on x86_64 (SysV + MS x64) + aarch64 |
| `krio-async`     | ✅ Phase 3 v2 — direct-yield + captures lift + cross-fn dispatch + multi-suspension blocks |
| `krio-preempt`   | 🟨 v1 — TimeSliceScheduler (cooperative slicing); real signal preempt deferred |

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
