#!/usr/bin/env python3
"""Linux RSS regression probe. Build artifacts live only in a temporary directory.
Run with --runtime pointing at an explicitly built baseline or candidate archive.
The compiler is held constant; stdout validates both live nodes in every ring slot.
"""
import argparse
import json
import os
from pathlib import Path
import platform
import re
import subprocess
import tempfile

ROOT = Path(__file__).resolve().parents[3]
p = argparse.ArgumentParser()
p.add_argument("--compiler", type=Path, default=ROOT / "target/release/willow")
p.add_argument("--runtime", type=Path, required=True)
p.add_argument("--output", type=Path, required=True)
p.add_argument("--label", required=True)
p.add_argument("--repeats", type=int, default=3)
a = p.parse_args()
if a.repeats < 1:
    p.error("--repeats must be positive")
source = (ROOT / "example/async_allocation_churn.wi").read_text()
# The example prints a stable bound; measurements record the exact count.
source = source.replace("println(gc_minor_collections() > 0);", "println(gc_minor_collections());")
env = {k: v for k, v in os.environ.items()
       if not k.startswith("WILLOW_GC_") and k != "WILLOW_WORKERS"}
cases = [(n, "default", "call") for n in (0, 1000000, 10000000, 40000000, 120000000)]
cases += [(10000000, w, "call") for w in ("1", "4")]
cases += [(40000000, "default", "inline")]
rows = []
with tempfile.TemporaryDirectory(prefix="willow-jz15-118-") as tmp:
    tmp = Path(tmp)
    for n, workers, shape in cases:
        for mode in ("sync", "async"):
            text = source.replace("churn(100000, keep);", f"churn({n}, keep);")
            if n == 0:
                text = text.replace("keep[j].next.unwrap().v", "0")
            if mode == "sync":
                text = text.replace("async fn main()", "fn main()")
            if shape == "inline":
                body = text.split("fn churn(n: i64, keep: Array<Node>) {\n", 1)[1].split("\n}\n", 1)[0]
                body = re.sub(r"\bi\b", "k", body).replace("k < n", f"k < {n}")
                text = source.replace("churn(100000, keep);", body)
                if mode == "sync":
                    text = text.replace("async fn main()", "fn main()")
            src = tmp / "probe.wi"
            binary = tmp / "probe"
            src.write_text(text)
            subprocess.run([str(a.compiler.resolve()), "build", str(src), "--release",
                            "--runtime-lib", str(a.runtime.resolve()), "-o", str(binary)],
                           check=True, stdout=subprocess.DEVNULL, env=env)
            run_env = dict(env)
            if workers != "default":
                run_env["WILLOW_WORKERS"] = workers
            expected = 0 if n == 0 else 128 * n - 4096
            for repeat in range(a.repeats):
                stats = tmp / "time.txt"
                run = subprocess.run(["/usr/bin/time", "-f", "%M %e", "-o", str(stats), str(binary)],
                                     check=True, capture_output=True, text=True, env=run_env)
                values = list(map(int, run.stdout.split()))
                assert values[0] == expected, (n, mode, values)
                rss, elapsed = stats.read_text().split()
                row = dict(label=a.label, platform=platform.system()+"-"+platform.machine(),
                           n=n, workers=workers, shape=shape, mode=mode, repeat=repeat,
                           rss_kib=int(rss), seconds=float(elapsed), checksum=values[0],
                           minor_collections=values[1], allocated_nodes=64 + 2*n)
                rows.append(row)
                print(json.dumps(row), flush=True)
a.output.write_text("".join(json.dumps(row)+"\n" for row in rows))

# A small fixed allowance covers scheduler/collector thread and task overhead.
# Enforce the ticket's <2x + fixed-overhead criterion on every candidate sample.
if a.label.startswith("after"):
    sync_min = {}
    for row in rows:
        if row["mode"] == "sync":
            key = (row["n"], row["workers"], row["shape"])
            sync_min[key] = min(sync_min.get(key, row["rss_kib"]), row["rss_kib"])
    for row in rows:
        if row["mode"] == "async":
            key = (row["n"], row["workers"], row["shape"])
            assert row["rss_kib"] < 2 * sync_min[key] + 4096, row
