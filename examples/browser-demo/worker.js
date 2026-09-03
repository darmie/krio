// One agent. Instantiates the SAME module against the SAME memory as
// the main thread, then blocks in `run()` until the cluster shuts down.
import { initSync, run_agent } from './pkg/krio_browser_demo.js';

self.onmessage = (e) => {
  const { module, memory, agent } = e.data;
  // The invariant a host must not break: same module, same memory.
  initSync({ module, memory });
  self.postMessage({ ready: agent });
  // Blocks on memory.atomic.wait32 whenever the cluster runs dry.
  // Legal here; it would throw on the main thread.
  run_agent(agent);
};
