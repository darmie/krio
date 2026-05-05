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

## Status — Phase 2 v2

Direct-yield split + captures-to-fields lift are in. Hosts wire
their IR up by implementing `krio-stackless::CoroCfg` for their
CFG and `AsyncHooks` for marker classification, then pass a
precomputed `LivenessMap` to `transform_to_state_machine`. The
transform splits at every yield site, allocates slots for live-
across values, and returns a `StateMachineLayout` with
`resume_entries`, `yield_blocks`, `yield_saves`, `resume_loads`,
and `block_kinds`. The host's codegen reads it to emit the
dispatcher prologue, the per-block lowering, the save/load helper
calls, and the use rewrites.

| Phase | What it gives you | Status |
|---|---|---|
| **1** | Public type contract + stub | ✅ shipped |
| **2 v1** | Direct-yield split | ✅ shipped |
| **2 v2** | Captures-to-fields lift via `LivenessMap` | ✅ shipped |
| **3 (was v3)** | Mid-block yield (host-side normalisation preferred) | not planned in krio-async — fixable host-side |
| **3 (cross-fn)** | Cross-function call dispatch | planned |

### What the host owns

- **Liveness analysis** — host's dataflow framework, passed in via
  `LivenessMap`. Krio-async never re-derives it.
- **Type-aware save/load helpers** — host emits the actual
  `runtime_save(frame, slot, v)` / `runtime_load(frame, slot)`
  calls. Krio-async hands out the slot indices and the values
  to save; the helper signature is the host's runtime ABI.
- **Use rewriting** — host's IR's normal use-rewrite machinery
  rebinds the loaded values. Krio-async tells the host
  *which* values to rewrite and *where*.

### What the host delegates

- Block splitting at yield points.
- State ID numbering.
- Slot allocation (one slot per unique LocalId across the
  function — pass smaller liveness sets if you want tighter
  packing).
- The contract: "if you save `(slot, v)` here, you load `slot`
  there."

### Caps still in place

- **`SuspensionInBranchedBlock`** — yield is not the last statement
  in its block. The host should normalise its CFG (insert an
  explicit branch) before calling krio-async; mid-block split is
  out of scope for the library.
- **`Unimplemented`** — `SuspensionSite::CrossFnCall`. Phase 3
  handles cross-function dispatch.

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
