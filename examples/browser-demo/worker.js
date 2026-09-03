// One agent. Instantiates the SAME module against the SAME memory as
// the main thread, then blocks in `run()` until the cluster shuts down.
import { initSync, run_agent } from './pkg/krio_browser_demo.js';

self.onmessage = (e) => {
  const { module, memory, agent, alsoSpawn } = e.data;
  // The invariant a host must not break: same module, same memory.
  initSync({ module, memory });

  // Nested spawn: this Worker starts more Workers itself, handing on the
  // same module and the same memory. That is the shape a language runtime
  // needs when its own main thread lives on a Worker and user code calls
  // Thread.create — the spawner is not the page.
  // Ask the *page* to start them rather than calling `new Worker()` here.
  //
  // A dedicated Worker's children are started through its parent's event
  // loop. This Worker is about to block forever in run(), so a child it
  // creates directly would never start — measured, not guessed. Proxying
  // to an agent that never blocks is the only arrangement that works,
  // and it is why agent creation is a host hook: only the host knows who
  // is safe to ask.
  if ((alsoSpawn ?? []).length) {
    self.postMessage({ pleaseSpawn: alsoSpawn });
  }

  // Announce readiness on a channel rather than to whoever started us.
  //
  // A Worker that has entered `run_agent` is blocked in
  // memory.atomic.wait32 and cannot service its message queue — so a
  // child posting to its parent would never be relayed. Anything that
  // routes messages through an agent deadlocks the moment that agent
  // blocks, which is the normal state for an idle one.
  new BroadcastChannel('krio-ready').postMessage({ ready: agent });
  self.postMessage({ ready: agent });
  // Blocks on memory.atomic.wait32 whenever the cluster runs dry.
  // Legal here; it would throw on the main thread.
  run_agent(agent);
};
