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

## Status — Phase 3 v2

Direct-yield split + captures lift + cross-function call dispatch
+ multiple suspensions per original block are all in. Hosts wire
their IR up by implementing `krio-stackless::CoroCfg` and
`AsyncHooks`, pass a precomputed `LivenessMap`, and get back a
`StateMachineLayout` covering all three suspension shapes via
`BlockKind::{DirectYield, CrossFnCallInit, CrossFnCallResume}` —
freely interleaved within a single source block.

| Phase | What it gives you | Status |
|---|---|---|
| **1** | Public type contract + stub | ✅ shipped |
| **2 v1** | Direct-yield split | ✅ shipped |
| **2 v2** | Captures-to-fields lift via `LivenessMap` | ✅ shipped |
| **3 v1** | Cross-function call dispatch (Init / Resume pair) | ✅ shipped |
| **3 v2** | Multiple suspensions per source block + mid-block direct yields | ✅ shipped |

For each cross-fn call site the transform:
1. Splits the source block at the call → `bb` becomes the
   `CrossFnCallInit` block (statements through the call) and
   `post_call` (a fresh block) holds the post-call code.
2. Creates a synthetic `resume_check` block. The dispatcher's
   `br_table` lands here on resume from a yielded child.
3. Records `BlockKind::CrossFnCallInit { resume_check_block, ... }`
   on `bb` and `BlockKind::CrossFnCallResume { done_block, ... }`
   on `resume_check`.
4. Runs the captures lift against the `resume_check` block (NOT
   `post_call`) — saved values are loaded there before the host's
   helper invokes the child poll fn.

The host's lowering then:
- In `bb`: emits "advance own state, push child frame, save args,
  return Pending" instead of the original call.
- In `resume_check`: emits "invoke child's poll fn, peek kind,
  on Yield propagate up, on Done pop the child frame and goto
  `done_block`."

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

### Multi-suspension within one block

When a single source block contains several suspensions, each one
splits the running tail of the previous split:

```text
Original:    [a, Yield, b, cross_call(f), c, Yield, d]
After split: bb        = [a, Yield]                     ← yields, state→1
             tail_y    = [b, cross_call(f)]             ← Init, state→2
             post_call = [c, Yield]                     ← yields, state→3
             tail_y2   = [d]
             resume_check = []                          ← synthetic
```

Liveness keyed on the *original* `(block, idx)` (e.g. `(bb, 5)`
for the second yield) is mapped internally to the correct
post-split yielding block (`post_call`) and resume entry
(`tail_y2`). Hosts don't need to track splits to write their
liveness map.

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
— the wren_lift implementation. Krio-async tracks the same shape
(state-machine layout + per-frame slot table) and has now lifted
all of wren_lift's v1 caps: live-across-suspension values, mid-
block direct yields, multiple suspensions per source block, and
cross-function dispatch all work.
