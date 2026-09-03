# krio-parallel in the browser

The same demonstration as `crates/krio-parallel/examples/parallel.rs`,
with Web Workers where that one uses OS threads. The cluster code is
identical — only the bootstrap differs, which is the claim the whole
wasm design rests on.

Agent 0 is the **main thread** and drives with `drive_once()`, because
`memory.atomic.wait32` throws there. Agents 1..N-1 are **Workers** and
block in `run()`. One `notify` wakes either kind.

## Build and run

```sh
RUSTFLAGS='-Ctarget-feature=+atomics,+bulk-memory,+mutable-globals \
  -Clink-arg=--import-memory -Clink-arg=--shared-memory \
  -Clink-arg=--max-memory=536870912 \
  -Clink-arg=--export=__wasm_init_tls -Clink-arg=--export=__tls_size \
  -Clink-arg=--export=__tls_align -Clink-arg=--export=__tls_base' \
  cargo build --release --target wasm32-unknown-unknown -Zbuild-std=std,panic_abort

wasm-bindgen --target web --out-dir pkg \
  target/wasm32-unknown-unknown/release/krio_browser_demo.wasm

python3 serve.py 8099     # serves with COOP/COEP
open 'http://127.0.0.1:8099/index.html?agents=4'
```

Query parameters: `agents`, `tasks`, `rounds`, `per_round`.

## Three link arguments that are not optional

* `--import-memory` — without it each Worker instantiates its **own**
  memory. Nothing errors; the agents simply operate on separate address
  spaces, and the demo reports plausible-looking parallelism that is
  measuring nothing. Check with
  `WebAssembly.Module.imports(m).filter(i => i.kind === 'memory')`.
* `--shared-memory --max-memory=N` — a shared memory must be bounded.
* `--export=__wasm_init_tls` and friends — wasm-bindgen needs them to
  give each Worker its own thread-locals. Without them it refuses to
  generate the glue, which is the one failure here that is loud.

Cross-origin isolation is required for `SharedArrayBuffer`; `serve.py`
sends the two headers. The page checks `crossOriginIsolated` and refuses
rather than degrading silently.

## Measured

Chrome 152, 10 hardware threads, 96 tasks x 24 rounds:

```
spawn() — shared injector, every agent pulls its own
  agents      ms   speedup  max conc  steals   steps      per agent
       1     225     1.00x         1       0    2400 ✓    2400
       2     116     1.94x         2       0    2400 ✓    1200 1200
       4      63     3.58x         4       0    2400 ✓    650 600 550 600
       8      42     5.39x         8       0    2400 ✓    425 300 225 275 350 250 275 300

spawn_on(agent 0) — one agent owns it all; the rest must steal
       1     218     1.00x         1       0    2400 ✓    2400
       2     112     1.95x         2      48    2400 ✓    1200 1200
       4      62     3.51x         4      72    2400 ✓    600 600 600 600
       8      36     6.11x         8      83    2400 ✓    325 275 275 300 300 300 300 325
```

`max conc` is the overlap proof — agents inside `Task::step` at the same
instant, counted by the observer rather than inferred from a clock.
Trust it and `steals` over the speedup, which swings with whatever else
the machine is doing.

The two tables separate two things that look alike: the first balances
through the injector and needs no steals at all, so only the second
shows the deque doing its job.

### It also proves the memory is shared

Worth spelling out, because a demo that quietly ran on eight separate
address spaces would look much the same:

* **steals > 0** — a steal is one agent CAS-ing *another agent's* deque.
* **max conc 8** — one `AtomicUsize`, incremented by eight Workers
  before any of them decremented it.
* **steps totalling 2400** — per-agent counters written by Workers and
  read by the main thread. Unshared, it would see its own and seven
  zeros.

The page also checks `crossOriginIsolated` and
`memory.buffer instanceof SharedArrayBuffer` up front and refuses rather
than degrading.

### Every run checks itself

A run must produce `tasks * (rounds + 1)` steps and reach
`max conc == agents`. The page shows ✓ or ✗ per row, so a run that
measured the wrong thing says so on the page instead of looking
plausible in a summary later.

## Headless, without a WebDriver

ChromeDriver has to match Chrome's major version exactly, which is a
reliable way to be blocked. The page reports its results back to
`serve.py` instead, so a headless run needs no driver:

```sh
python3 serve.py 8099 > /tmp/krio-results.txt &

"/Applications/Google Chrome.app/Contents/MacOS/Google Chrome" \
  --headless=new --disable-gpu --user-data-dir=/tmp/krio-run \
  'http://127.0.0.1:8099/index.html?agents=4&tasks=96&rounds=24&per_round=60000'
# serve.py appends: RESULT {"workload":…,"rows":…}

./report.py /tmp/krio-results.txt
```

Three things that cost real time to learn:

* **Kill by profile, not by pid.** Killing the launcher leaves Chrome's
  children running, and several live instances competing for CPU wreck
  the timings. Launch each run with its own `--user-data-dir` and
  `pkill -f` that path afterwards.
* **Never `--virtual-time-budget` for timing.** It makes
  `performance.now()` meaningless while Workers burn real CPU, and
  reports wall times of zero. The counters stay correct — they are
  atomics, not clocks.
* **Group results by workload.** Two runs with different parameters
  produce legitimately different step counts, and averaging across them
  invents numbers: merging a 48x12 run into a 96x24 sweep once produced
  a "41x speedup" that was one workload's clock over another's. Each
  payload names its own workload and `report.py` groups on it.

## The spawner must never block

`?nested=1` runs the arrangement a language runtime needs: agent 1
decides who else exists, rather than the page deciding up front. It is
what `Thread.create` looks like when the runtime's own main thread lives
on a Worker.

It only works because the request is **proxied to the page**. A
dedicated Worker's children start through its parent's event loop, so an
agent that calls `new Worker()` and then blocks in `run()` leaves its
child permanently unstarted. Measured in Chrome 152:

```
parent spawns child, then blocks    -> child never runs
parent spawns child, stays awake    -> child runs
parent asks the page, then blocks   -> child runs
```

For a runtime this is the common path, not an edge case —
`Thread.create` followed by a join or a mutex acquire is ordinary code.
So `worker.js` posts `{ pleaseSpawn: [...] }` and the page obliges,
because the page is the one agent that never blocks.

Both arrangements reach the same place:

```
page spawns all          agents=8 maxconc=8 steals=82 steps=2400 ok
agent 1 asks the page    agents=8 maxconc=8 steals=80 steps=2400 ok
```
