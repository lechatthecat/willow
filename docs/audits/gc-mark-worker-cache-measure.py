#!/usr/bin/env python3
"""Focused before/after runtime measurement; run with label and runtime archive."""
import collections
import json
import os
from pathlib import Path
import subprocess
import sys
import tempfile
import time

ROOT = Path(__file__).resolve().parents[2]
sys.path.insert(0, str(ROOT / "benches/go_vs_willow"))
import run_sync

label, archive = sys.argv[1:]
archive = str(Path(archive).resolve())
os.environ.pop("WILLOW_GC_MARK_THREADS", None)
env = os.environ.copy()
env["WILLOW_WORKERS"] = "8"
rows = []
with tempfile.TemporaryDirectory(prefix="willow-worker-count-") as tmp:
    tmp = Path(tmp)
    def build(source, output):
        subprocess.run([str(ROOT / "target/release/willow"), "build", str(source),
                        "--release", "--runtime-lib", archive, "-o", str(output)],
                       cwd=ROOT, check=True, stdout=subprocess.DEVNULL)
    for n in (10000, 40000, 80000):
        source = tmp / "concat.wi"
        source.write_text('fn main() { let mut s = ""; let mut i = 0; while i < '
                          + str(n) + ' { s = s + "abc"; i += 1; } println(s.len()); }\n')
        binary = tmp / "concat"
        build(source, binary)
        for trial in range(7):
            for mode in (("default", "override") if trial % 2 == 0 else ("override", "default")):
                run_env = env.copy()
                if mode == "override":
                    run_env["WILLOW_GC_MARK_THREADS"] = "3"
                start = time.perf_counter()
                out = subprocess.check_output([str(binary)], env=run_env, text=True)
                elapsed = time.perf_counter() - start
                assert out.strip() == str(n * 3), out
                rows.append(dict(label=label, case=f"cat{n}", mode=mode, trial=trial, seconds=elapsed))
        trace = tmp / "trace"
        subprocess.run(["strace", "-f", "-e", "trace=openat", "-o", str(trace), str(binary)],
                       env=env, check=True, stdout=subprocess.DEVNULL)
        counts = collections.Counter()
        for line in trace.read_text().splitlines():
            if '"/proc/self/cgroup"' in line:
                counts["proc_self_cgroup"] += 1
            if 'cpu.max"' in line:
                counts["cpu_max"] += 1
        rows.append(dict(label=label, case=f"cat{n}", opens=dict(counts)))
    run_sync.BUILD = tmp
    for case in ("array_grow_only", "array_literal_churn", "object_churn"):
        build(run_sync.SYNC / "willow" / f"{case}.wi", tmp / f"willow-{case}")
        for trial in range(5):
            row = run_sync.one_run("willow", case, 8)
            row.update(label=label, trial=trial)
            rows.append(row)
output = ROOT / "docs/audits" / f"gc-mark-worker-cache-{label}.jsonl"
output.write_text("".join(json.dumps(row, sort_keys=True) + "\n" for row in rows))
print(output)
