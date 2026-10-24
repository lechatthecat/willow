"""Summarize all fixed-contract process pairs without excluding failed gates."""
import json
from pathlib import Path
import random
import statistics
import sys

directory = Path(sys.argv[1])
data = json.loads((directory / "replicates.json").read_text())
rows = data["summaries"]
base = sorted((r for r in rows if r["build"] == "baseline"), key=lambda r: r["trial"])
candidate = sorted((r for r in rows if r["build"] == "candidate"), key=lambda r: r["trial"])
assert len(base) == len(candidate) == 5
assert [r["trial"] for r in base] == list(range(5))
assert [r["trial"] for r in candidate] == list(range(5))
assert all(r["case"] == "medium_candidate" and r["samples"] == 2000 for r in rows)
assert len(list(directory.glob("*.status"))) == 10
assert all(p.read_text().strip() == "0" for p in directory.glob("*.status"))
for line in (directory / "replicates-samples.jsonl").open():
    row = json.loads(line)
    if "components_ns" in row:
        assert sum(row["components_ns"]) == row["pause_ns"]


def ratio(key):
    return statistics.median(r[key] for r in candidate) / statistics.median(r[key] for r in base)


def interval(key):
    rng = random.Random(675)
    values = []
    for _ in range(10000):
        indices = rng.choices(range(5), k=5)
        values.append(statistics.median(candidate[i][key] for i in indices)
                      / statistics.median(base[i][key] for i in indices))
    values.sort()
    return values[249], values[9749]


throughput = ratio("objects_per_second")
cpu = ratio("gc_cpu_ns_per_allocated_byte")
peak = max(r["peak_process_rss_bytes"] for r in candidate) / max(r["peak_process_rss_bytes"] for r in base)
gates = {
    "median_throughput": throughput >= .97,
    "peak_footprint": peak <= 1.10,
    "gc_cpu_per_allocated_byte": cpu <= 1.0,
    "every_process_p99": all(r["p99_ns"] <= 2_000_000 for r in candidate),
}
lines = ["# Fixed-contract confirmation", "", "Environment-specific; workload adopted after exploratory results.",
         "Five new independent processes per build; every measured run retained.", "",
         "| Trial | Build | p99 ms | p99.9 ms | Max ms | Objects/s | Peak RSS bytes | GC CPU ns/byte |",
         "|---|---|---:|---:|---:|---:|---:|---:|"]
for a, b in zip(base, candidate):
    for r in (a, b):
        lines.append(f"| {r['trial']} | {r['build']} | {r['p99_ns']/1e6:.6f} | {r['p999_ns']/1e6:.6f} | {r['max_ns']/1e6:.6f} | {r['objects_per_second']:.0f} | {r['peak_process_rss_bytes']} | {r['gc_cpu_ns_per_allocated_byte']:.6f} |")
lines += ["", "Candidate/baseline ratios (median of processes; paired bootstrap 95% interval):", ""]
for key in ("objects_per_second", "peak_process_rss_bytes", "gc_cpu_ns_per_allocated_byte"):
    lo, hi = interval(key)
    lines.append(f"- {key}: {ratio(key):.6f}, [{lo:.6f}, {hi:.6f}].")
lines += [f"- Worst-process peak footprint ratio: {peak:.6f}.", "",
          "The five-process bootstrap is descriptive and has limited tail precision;",
          "it does not prove a universal bound or erase host scheduling noise.", "", "Fixed gates:", ""]
lines += [f"- {key}: {'PASS' if ok else 'FAIL'}." for key, ok in gates.items()]
lines += ["", "Foreign native execution and CI remain unverified.", ""]
(directory / "report.md").write_text("\n".join(lines))
print(json.dumps({"gates": gates, "throughput_ratio": throughput, "cpu_ratio": cpu, "peak_ratio": peak}))
