#!/usr/bin/env python3
"""Group `serve.py` results by workload and check each run's invariant.

Runs with different parameters produce legitimately different step
counts. Merging them yields nonsense — a 41x "speedup" that was really
one workload's wall clock over another's. So group by workload first,
and never average across groups.
"""
import json
import re
import sys
from collections import defaultdict

path = sys.argv[1] if len(sys.argv) > 1 else "/tmp/krio-results.txt"

runs = defaultdict(lambda: defaultdict(dict))
env_for = {}

for line in open(path):
    if not line.startswith("RESULT "):
        continue
    d = json.loads(line[len("RESULT "):])
    w = d.get("workload")
    if w is None:
        # Older page: recover the workload from the env blob.
        m = re.search(r"(\d+) tasks x (\d+) rounds", d.get("env", ""))
        if not m:
            continue
        w = {"tasks": int(m.group(1)), "rounds": int(m.group(2))}
        w["expectSteps"] = w["tasks"] * (w["rounds"] + 1)
    key = (w["tasks"], w["rounds"], w["expectSteps"])
    env_for.setdefault(key, d.get("env", ""))
    for mode in ("injector", "stolen"):
        for r in d["rows"][mode]:
            # Keyed by agent count, so a repeat run replaces rather than
            # doubling up.
            runs[key][mode][r["agents"]] = r

if not runs:
    sys.exit(f"no results in {path}")

TITLES = {
    "injector": "spawn() — shared injector, every agent pulls its own",
    "stolen": "spawn_on(agent 0) — one agent owns it all; the rest must steal",
}

for (tasks, rounds, expect), modes in sorted(runs.items()):
    print(f"\n═══ {tasks} tasks x {rounds} rounds — {expect} steps per run")
    for line in env_for[(tasks, rounds, expect)].splitlines():
        if not line.startswith("agents:"):
            print(f"    {line}")
    for mode, title in TITLES.items():
        rs = [modes[mode][a] for a in sorted(modes[mode])]
        if not rs:
            continue
        base = next((r["ms"] for r in rs if r["agents"] == 1), None)
        print(f"\n  {title}")
        print(f"  {'agents':>6}  {'ms':>6}  {'speedup':>8}  {'max conc':>8}"
              f"  {'steals':>6}  {'steps':>6}      per agent")
        for r in rs:
            share = [int(x) for x in r["share"].split()]
            total = r.get("steps", sum(share))
            ok = total == expect and r["maxConcurrent"] == r["agents"]
            speedup = f"{base / r['ms']:.2f}x" if base and r["ms"] else "—"
            print(f"  {r['agents']:>6}  {r['ms']:>6.0f}  {speedup:>8}"
                  f"  {r['maxConcurrent']:>8}  {r['steals']:>6}"
                  f"  {total:>6} {'✓' if ok else '✗'}    {' '.join(map(str, share))}")
