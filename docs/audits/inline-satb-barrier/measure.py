#!/usr/bin/env python3
"""Compare separately built before/after executables; no source mutation."""
import argparse
import hashlib
import json
import math
import os
from pathlib import Path
import statistics
import subprocess
import tempfile
import time

parser = argparse.ArgumentParser()
parser.add_argument("--baseline", type=Path, required=True)
parser.add_argument("--candidate", type=Path, required=True)
parser.add_argument("--output", type=Path, required=True)
parser.add_argument("--runs", type=int, default=5)
args = parser.parse_args()
assert args.runs >= 5
rows = []
for case, expected in [("linked_list", "499999500000"), ("object_churn", "4994476800")]:
    for run in range(args.runs):
        for variant in (["baseline", "candidate"] if run % 2 == 0 else ["candidate", "baseline"]):
            binary = (getattr(args, variant) / case).resolve()
            env = os.environ.copy()
            env["WILLOW_WORKERS"] = "2"
            with tempfile.NamedTemporaryFile() as metrics:
                start = time.monotonic()
                result = subprocess.run(["/usr/bin/time", "-f", "%e %U %S %M", "-o", metrics.name, str(binary)],
                                        env=env, capture_output=True, text=True, check=True, timeout=180)
                elapsed = time.monotonic() - start
                wall, user, system, rss = Path(metrics.name).read_text().split()
            lines = result.stdout.splitlines()
            assert expected in lines, result.stdout
            rows.append(dict(case=case, run=run, variant=variant, seconds=elapsed,
                             user_seconds=float(user), system_seconds=float(system), rss_kib=int(rss),
                             sha256=hashlib.sha256(binary.read_bytes()).hexdigest(), stdout=lines))
summary = []
for case in ["linked_list", "object_churn"]:
    item = dict(case=case)
    for variant in ["baseline", "candidate"]:
        subset = [r for r in rows if r["case"] == case and r["variant"] == variant]
        samples = [r["seconds"] for r in subset]
        mean = statistics.mean(samples)
        # Student t, df=4 for the default five independent process runs.
        half = 2.776 * statistics.stdev(samples) / math.sqrt(len(samples))
        item[variant] = dict(median_seconds=statistics.median(samples), mean_seconds=mean,
                             approximate_95pct_ci=[mean-half, mean+half],
                             median_rss_kib=statistics.median(r["rss_kib"] for r in subset))
    item["time_ratio"] = item["candidate"]["median_seconds"] / item["baseline"]["median_seconds"]
    summary.append(item)
args.output.write_text(json.dumps(dict(workers=2, runs=args.runs, rows=rows, summary=summary), indent=2)+"\n")
print(json.dumps(summary, indent=2))
