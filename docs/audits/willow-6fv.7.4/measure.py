#!/usr/bin/env python3
"""Five independent processes per shape/worker count. No private inputs or CI."""
import json, os, platform, random, statistics, subprocess, sys, tempfile
from pathlib import Path
ROOT = Path(__file__).resolve().parents[3]
OUT = Path(__file__).resolve().parent

def ci(values):
    rng = random.Random(74)
    samples = sorted(statistics.median(rng.choices(values, k=len(values))) for _ in range(10000))
    return [samples[249], samples[9749]]

def main():
    exe = ROOT / "target/release/examples/gc_m3_profile"
    subprocess.run(["rtk", "proxy", "cargo", "build", "-p", "willow_runtime", "--release", "--example", "gc_m3_profile"], cwd=ROOT, check=True)
    rows = []
    for n, fanout in [(4096, 1), (4096, 4), (32768, 4), (32768, 16)]:
        for workers in [1, 2, 4, 8, 16]:
            for run in range(5):
                env = dict(os.environ, WILLOW_GC_PROFILE="1", WILLOW_GC_MARK_THREADS=str(workers))
                with tempfile.TemporaryDirectory() as tmp:
                    rss = Path(tmp) / "rss"
                    result = subprocess.run(["/usr/bin/time", "-f", "%M", "-o", str(rss), str(exe), str(n), str(fanout)], cwd=ROOT, env=env, text=True, capture_output=True, timeout=120, check=True)
                    events = [json.loads(line) for line in result.stdout.splitlines() if line.startswith("{")]
                    application = next(e for e in events if e["event"] == "application")
                    assert application["heap_after"] == 0
                    rows.append(dict(objects=n, fanout=fanout, workers=workers, run=run, rss_kib=int(rss.read_text()), events=events))
            print(f"objects={n} fanout={fanout} workers={workers}: five processes complete", flush=True)
    (OUT / "measurements.jsonl").write_text("".join(json.dumps(row) + "\n" for row in rows))
    summary = []
    for n, fanout, workers in sorted({(r["objects"], r["fanout"], r["workers"]) for r in rows}):
        group = [r for r in rows if (r["objects"], r["fanout"], r["workers"]) == (n, fanout, workers)]
        elapsed = [next(e["elapsed_ns"] for e in r["events"] if e["event"] == "application") for r in group]
        summary.append(dict(objects=n, fanout=fanout, workers=workers, median_ns=statistics.median(elapsed), median_95_bootstrap_ci_ns=ci(elapsed), peak_rss_kib=max(r["rss_kib"] for r in group)))
    (OUT / "summary.json").write_text(json.dumps(dict(platform=platform.platform(), cpu_count=os.cpu_count(), profile=True, processes_per_cell=5, results=summary), indent=2) + "\n")
if __name__ == "__main__": main()
