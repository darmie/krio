# krio-async

> Cross-function stackless coroutines.
>
> One member of the [`krio`](../../README.md) coroutine framework
> family. See the workspace README for how this fits alongside
> `krio-stackless`, `krio-fiber`, and `krio-preempt`.

`krio-async` extends `krio-stackless`'s per-function state machine
with **function colour propagation**: a yield can suspend the whole
call stack up to the nearest async boundary, not just the current
function. Locals that live across a suspension are lifted from
stack slots into a per-frame slot table on a runtime fiber-style
frame stack.

## Status — Phase 1

The public **type contract** is in. Hosts can wire their IR to the
trait surfaces (`SuspendingFns`, `BlockKind`, `StateMachineLayout`,
`FrameState`, `TransformError`) and call `transform_to_state_machine`
today. For a function the host marks non-suspending, the call
returns a trivial layout. For a suspending function, it returns
`TransformError::Unimplemented` until Phase 2 lands the lowering.

| Phase | What it gives you | Status |
|---|---|---|
| **1** | Public type contract + stub | ✅ shipped |
| **2** | Direct-yield lowering (no cross-fn) | planned |
| **3** | Cross-function call dispatch | planned |

The design is a generalised port of the AOT state-machine lowering
in the `wren_lift` Wren JIT/AOT runtime — that codebase already
validates the shape against a real production language; krio-async
lifts the algorithm out of Wren-specific types so any host can
drive it.

## How a host integrates

1. Implement `SuspendingFns` over your function-id type. Compute
   the transitive yield-reachable set (taint analysis over the
   call graph) once, then answer `is_suspending(fn_id)` from the
   set.
2. For each function in the suspending set, call
   `transform_to_state_machine(cfg, fn_id, suspending)`. The
   returned `StateMachineLayout` tells you:
   - which blocks are resume entries (your dispatcher's `br_table`
     targets)
   - which blocks have a yielding `Return` (so you emit the
     pre-Return save + kind=Yield stamp)
   - which values to save / load at each suspension boundary
   - which call sites are direct yields vs cross-fn (the
     `BlockKind` discriminator)
3. Maintain a per-fiber stack of `FrameState`s at runtime. Each
   active suspending call owns one frame; the dispatcher reads
   the deepest frame's `state_id` to know where to resume.

## Reference

`/Users/amaterasu/Vibranium/wren_lift/src/codegen/aot_state_machine.rs`
— the wren_lift implementation. Phase 2 will mirror its `v1` cap
set (no live-across-suspension values, no suspension inside
branched blocks) and lift those caps over Phase 3+.
