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

## Status — Phase 2 v1

The direct-yield lowering is in. Hosts wire their IR up by
implementing `krio-stackless::CoroCfg` for their CFG and `AsyncHooks`
for marker classification, then call `transform_to_state_machine`.
For each yielding function the transform splits at every yield
site, returns the layout (`resume_entries`, `yield_blocks`,
`block_kinds`), and the host's codegen reads it to emit the
dispatcher prologue and per-block lowering.

| Phase | What it gives you | Status |
|---|---|---|
| **1** | Public type contract + stub | ✅ shipped |
| **2 v1** | Direct-yield split (one yield per block, at tail; no cross-fn) | ✅ shipped |
| **2 v2** | Captures-to-fields lift + mid-block yield | planned |
| **3** | Cross-function call dispatch | planned |

### v1 caps (refused with `TransformError`)

- **`SuspensionInBranchedBlock`** — yield is not the last statement
  in its block. v2 will split mid-block by replicating the post-yield
  tail across control-flow successors.
- **`LiveValueAcrossSuspension`** — a value defined before a yield
  is used after it. v2 adds the captures lift via `yield_saves` /
  `resume_loads` (currently always empty in the layout).
- **`Unimplemented`** — `SuspensionSite::CrossFnCall` classification.
  Phase 3 handles cross-function dispatch.

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
