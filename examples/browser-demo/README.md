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
 agents   wall (ms)   speedup  max concurrent   steals   steps per agent
      1         230     1.00x               1        0   2400
      2         120     1.91x               2        0   1200 1200
      4          65     3.56x               4        0   600 600 600 600
      8          45     5.14x               8        0   400 250 275 325 275 300 325 250

spawn_on(agent 0) — one agent owns it all; the rest must steal
      1         228     1.00x               1        0   2400
      2         119     1.91x               2       48   1200 1200
      4          62     3.66x               4       72   600 600 600 600
      8          41     5.58x               8       81   375 275 300 325 275 275 300 275
```

`max concurrent` is the overlap proof — agents inside `Task::step` at
the same instant, counted by the observer rather than inferred from a
clock. The two tables separate two things that look alike: the first
balances through the injector and needs no steals at all, so only the
second shows the deque doing its job.

## Headless, without a WebDriver

ChromeDriver has to match Chrome's major version exactly, which is a
reliable way to be blocked. The page reports its results back to
`serve.py` instead, so a headless run needs no driver:

```sh
"/Applications/Google Chrome.app/Contents/MacOS/Google Chrome" \
  --headless=new --disable-gpu \
  'http://127.0.0.1:8099/index.html?agents=4'
# serve.py prints: RESULT {"env":...,"rows":...}
```

Avoid `--virtual-time-budget` for timing: it makes `performance.now()`
meaningless while Workers burn real CPU, and reports wall times of zero.
The counters stay correct because they are atomics, not clocks.
