# krio-stackless

> MIR-level state-machine transform for stackless coroutines.
>
> One member of the [`krio`](../../README.md) coroutine framework
> family. See the workspace README for how this fits alongside
> `krio-async`, `krio-fiber`, and `krio-preempt`.

`krio-stackless` takes a control-flow graph annotated with
cooperative-concurrency markers and rewrites it into a plain CFG with
no suspension semantics left. Each coroutine becomes a switch-on-state
machine; the surrounding region becomes an inline executor loop.
Everything stays in one stack
frame — no heap allocation, no closure environment, no ABI changes.

The transform sits one level above codegen: feed it your MIR, get back
the same MIR with the coroutines flattened, then hand the result to
LLVM / Cranelift / your interpreter / wherever.

---

## Status

| Phase | What it gives you | Status |
|---|---|---|
| **1** | Standalone crate, in-tree, concrete IR types | ✅ shipped |
| **2** | `CoroCfg` + `CoroHooks` traits — plug in your IR; `Executor` trait — swap scheduling models | ✅ shipped |
| **2.1** | Crate is dependency-free; adapters live with their host compiler | ✅ shipped |
| **2.5** | `WakerExecutor` for waker-driven async (sibling to `CooperativeExecutor`) | planned |
| **3** | Standalone repo + crates.io | planned |

The crate has no runtime dependencies — the transform operates
against any IR that implements the traits. Adapters for specific
compilers ship with the host (e.g. `zura_mir::krio_adapter` in the
parent workspace), not with krio.

Validation:

- `tests/toy_adapter.rs` — hand-rolled minimal CFG IR with no
  external deps. Runs the transform against pseudocode-style markers
  and asserts shape parity.
- A separate test suite in the host compiler (`zura_mir`'s
  `krio_integration` tests) exercises the same transform against a
  real, production IR.

If a behaviour is awkward in the toy adapter, the trait surface is
leaking the host adapter's accidental shape and gets fixed before
Phase 3.

---

## What the input looks like

The transform expects a CFG whose statements include marker rvalues
that bracket a concurrency region and the coroutines inside it:

```
region_begin                       // open a concurrency region
  coroutine_begin                  // open coroutine #1
    ... user statements ...
    suspend                        // a yield point
    ... more user statements ...
  coroutine_end
  coroutine_begin                  // open coroutine #2
    ...
  coroutine_end
region_end
```

Suspension points come in three flavours:

| Kind            | When it suspends                           |
|-----------------|--------------------------------------------|
| `yield`         | always                                      |
| `recv` (guarded)| only if the channel is empty                |
| `send`          | always — runs the send first, then yields   |

The peek-then-yield protocol for guarded recv lets the transform stay
allocation-free: it never has to capture the channel state across a
suspension boundary.

---

## What the output looks like

For each coroutine the transform allocates two locals:

```
state_N : i64 mut       // 0 = entry, 1..K = after each suspend, DONE = done
poll_N  : i64 mut       // 0 = Ready, 1 = Pending
```

and rewrites the body into a switch-driven state machine:

```
dispatch_N:
  switch state_N {
    0    -> entry_block
    1    -> resume_block_1
    ...
    K    -> resume_block_K
    DONE -> exit_N
  }

resume_block_i:
  // statements that originally followed the i-th suspend
  ...

at each old suspend site:
  state_N = i
  poll_N  = Pending
  goto exit_N

at coroutine_end:
  poll_N  = Ready
  state_N = DONE
  goto exit_N
```

The region is replaced with an inline executor loop:

```
loop_top:
  all_done = true
  for each coroutine N {
    if state_N == DONE { skip }
    else {
      goto dispatch_N             // run a turn
      // exit_N falls back here
      if poll_N == Ready { state_N = DONE }
      else               { all_done = false }
    }
  }
  if all_done { goto after_region } else { goto loop_top }
```

The original markers are erased (`Nop`) and the spawn-body statements
are moved into their own block so the executor's switch can dispatch
each coroutine independently.

---

## Why a peek for guarded recv?

Without it, a coroutine that calls `recv` on an empty channel has no
graceful way to give up the turn — the recv would have to either block
the executor (defeating cooperation) or return a sentinel that every
caller has to handle by hand.

The peek splits the recv into two halves:

```
suspend slot:
  ready = is_ready(channel)
  if ready -> resume_block (real recv runs)
  else     -> yield_bb (state += 1; poll = Pending; goto exit)
```

On the next turn the dispatch lands back at the suspend slot, the peek
runs again, and we either take the value or yield again. No allocation,
no callbacks, no waker registration.

---

## Why coroutine 0's body needs special treatment

When the IR builder lowers the source

```
region_begin
  coroutine_begin
    body...
  coroutine_end
  coroutine_begin
    body...
  coroutine_end
region_end
```

the first coroutine's body lands in the same block that holds
`region_begin`. To make the dispatch's "state 0 → entry" arm work, the
transform splits that block: markers stay (now `Nop`), body moves into
a fresh block, and the dispatch is retargeted to the new block. The
original block becomes `state_init + goto loop_top`.

---

## Public API

Generic entry point — works against any IR that implements the traits:

```rust
pub fn run_with<C, H, E>(cfg: &mut C, hooks: &mut H, executor: &mut E)
where
    C: CoroCfg,
    H: CoroHooks<Cfg = C>,
    E: Executor<C>;
```

Discovery primitives (advanced consumers who want to drive their own
executor):

```rust
pub fn find_regions<C, H>(cfg: &C, hooks: &H) -> Vec<Region<C::BlockId>>;
pub fn find_suspension_points<C, H>(
    cfg: &C, hooks: &H, coroutine: &Coroutine<C::BlockId>,
) -> Vec<(C::BlockId, usize, SuspKind)>;
```

`run_with` is idempotent: it processes one region at a time and
re-discovers after each rewrite, so nested regions inside coroutines
work correctly.

Host compilers typically wrap `run_with` in a no-arg convenience
that pre-fills hooks + executor; see `zura_mir::krio_adapter::run`
in the parent workspace for an example.

### Trait surface (Phase 2)

`CoroCfg` (about a dozen methods, each maps to a real call site in
the algorithm):

```rust
pub trait CoroCfg {
    type BlockId: Copy + Eq + Ord + Hash + Debug;
    type LocalId: Copy + Eq + Ord + Hash + Debug;

    // Read access
    fn block_count(&self) -> usize;
    fn statement_count(&self, bb: Self::BlockId) -> usize;
    fn block_ids(&self) -> Vec<Self::BlockId>;

    // Construction
    fn new_block(&mut self) -> Self::BlockId;
    fn new_state_local(&mut self) -> Self::LocalId;     // i64 mut
    fn new_bool_local(&mut self) -> Self::LocalId;      // bool
    fn new_mut_bool_local(&mut self) -> Self::LocalId;  // bool mut

    // Statement emission
    fn emit_assign_i64(&mut self, bb: Self::BlockId, local: Self::LocalId, v: i64);
    fn emit_assign_bool(&mut self, bb: Self::BlockId, local: Self::LocalId, v: bool);
    fn emit_eq_check_i64(&mut self, bb: Self::BlockId,
                         dest: Self::LocalId, lhs: Self::LocalId, rhs: i64);
    fn replace_with_nop(&mut self, bb: Self::BlockId, idx: usize);
    fn split_after(&mut self, src: Self::BlockId, idx: usize) -> Self::BlockId;
    fn prepend_assign_i64(&mut self, bb: Self::BlockId, local: Self::LocalId, v: i64);

    // Terminator manipulation
    fn set_goto(&mut self, bb: Self::BlockId, target: Self::BlockId);
    fn set_branch(&mut self, bb: Self::BlockId, cond: Self::LocalId,
                  t: Self::BlockId, f: Self::BlockId);
    fn set_switch(&mut self, bb: Self::BlockId, discr: Self::LocalId,
                  targets: Vec<(i64, Self::BlockId)>, otherwise: Self::BlockId);
    fn redirect_targets(&mut self, bb: Self::BlockId,
                        from: Self::BlockId, to: Self::BlockId);
}
```

`CoroHooks` (consumer-specific surgery):

```rust
pub trait CoroHooks {
    type Cfg: CoroCfg;
    fn classify_marker(&self, cfg: &Self::Cfg, bb: BlockId, idx: usize)
        -> Option<Marker>;
    fn emit_guarded_recv_peek(&mut self, cfg: &mut Self::Cfg,
                              bb: BlockId, idx: usize, resume_bb: BlockId)
        -> LocalId;
}
```

`Marker` enumerates the seven categories the algorithm cares about:
`RegionBegin`, `RegionEnd`, `CoroutineBegin`, `CoroutineEnd`, `Yield`,
`GuardedRecv`, `ProducingSend`. The consumer maps their IR's marker
statements onto these.

### IR-builder contract

The transform assumes the consumer's IR builder:

1. Assigns block IDs in source/control-flow order so that
   `block_ids()` returns them roughly top-down.
2. Places each marker into its own block (or at least keeps each
   coroutine's begin/end markers + each suspension point as the only
   "interesting" statement in its block).
3. Wires inter-block control flow with `goto` between markers.

The Zura MIR builder satisfies all three; the toy adapter test
demonstrates the same shape over a hand-rolled IR.

### Executor pluggability

```rust
pub trait Executor<C: CoroCfg> {
    fn finalize_region(&mut self, cfg: &mut C,
                       region: &Region<C::BlockId>,
                       machines: &[Machine<C::BlockId, C::LocalId>]);
}
```

Today: `CooperativeExecutor` (round-robin polling loop). The Phase 2.5
plan adds `WakerExecutor` — same state-machine transform, different
finalization strategy (per-coroutine poll fns + waker registration
instead of a loop).

---

## Roadmap

### Phase 2.5 — `WakerExecutor`

The state-machine transform stays neutral about how the dispatch
blocks get called. The cooperative path wraps them in a round-robin
loop; the waker path emits a per-coroutine poll function whose
suspend sites register a `Waker` and return `Pending`. Same
machinery, different finalization. Tracked behind the `Executor`
trait.

### Sibling — preemptive scheduling

Preemption gets its own crate (`krio-preempt` or similar). The
state-machine transform doesn't apply because preemption can land
mid-instruction — the runtime needs separate stacks + a
context-switch primitive (target-specific asm) instead. Krio's
output can be one task type the preemptive scheduler runs, but the
algorithms don't share infrastructure.

### Out of scope

- **Closure capture / first-class coroutine values.** Captures stay
  in the enclosing stack frame because everything's inline.
  Returning a live coroutine from a function would need a fat
  closure representation that krio doesn't build.
- **Optimisation passes.** krio produces correct but unoptimised
  CFGs. Run your usual jump-threading / dead-block-removal afterward.

---

## License

TBD — will inherit from the spin-out repo. Treat the in-tree copy as
internal until then.
