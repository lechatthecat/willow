#!/usr/bin/env python3
"""Build isolated probes and record peak RSS; GNU time on Linux is required.
Run against each revision with its own release CLI/runtime, keeping JSON evidence.
"""
import argparse
import json
import os
from pathlib import Path
import subprocess
import tempfile

p = argparse.ArgumentParser()
p.add_argument("--compiler", type=Path, default=Path("target/release/willow"))
p.add_argument("--runtime", type=Path, default=Path("target/release/libwillow_runtime.a"))
p.add_argument("--output", type=Path, required=True)
p.add_argument("--repeats", type=int, default=5)
p.add_argument("--existing", type=Path, help="measure existing sync and async growing-40000 binaries")
a = p.parse_args()
rows = []
with tempfile.TemporaryDirectory(prefix="willow-growing-") as tmp:
    tmp = Path(tmp)
    for workload in (["growing"] if a.existing else ["growing", "large", "helper", "empty"]):
        for count in ([40000] if a.existing else ([10000, 20000, 40000] if workload != "empty" else [0])):
            for mode in ["sync", "async"]:
                binary = tmp / f"{workload}-{count}-{mode}"
                if a.existing:
                    binary = a.existing.resolve() / mode
                else:
                    body = f'let mut s = ""; let mut i = 0; while i < {count} {{ s = s + "abc"; i += 1; }} println(s.len());'
                    if workload == "large":
                        body = f'let mut chunk = "abcd"; let mut j = 0; while j < 16 {{ chunk = chunk + chunk; j += 1; }} let mut s = ""; let mut i = 0; let mut total = 0; while i < {count // 10} {{ s = chunk + "abc"; total += s.len(); i += 1; }} println(total);'
                    prefix = ""
                    if workload == "helper":
                        prefix = "fn build() { " + body + " }\n"
                        body = "build();"
                    if workload == "empty":
                        body = "println(0);"
                    source = binary.with_suffix(".wi")
                    source.write_text(prefix + ("async " if mode == "async" else "") + "fn main() { " + body + " }\n")
                    subprocess.run([str(a.compiler.resolve()), "build", str(source), "--release", "--runtime-lib", str(a.runtime.resolve()), "-o", str(binary)], check=True, capture_output=True, text=True)
                expected = 0 if workload == "empty" else (count // 10 * 262147 if workload == "large" else count * 3)
                for markers in ["default", "3"]:
                    for trial in range(a.repeats):
                        env = {k: v for k, v in os.environ.items() if not k.startswith("WILLOW_GC_")}
                        if markers != "default":
                            env["WILLOW_GC_MARK_THREADS"] = markers
                        timing = tmp / "time"
                        run = subprocess.run(["/usr/bin/time", "-f", "%M %e", "-o", str(timing), str(binary)], env=env, capture_output=True, text=True, check=True, timeout=60)
                        assert run.stdout.strip() == str(expected), run.stdout
                        rss, elapsed = timing.read_text().split()
                        row = dict(workload=workload, count=count, mode=mode, markers=markers, trial=trial + 1, rss_kib=int(rss), elapsed_seconds=float(elapsed))
                        rows.append(row)
                        print(json.dumps(row), flush=True)
a.output.write_text(json.dumps(rows, indent=2) + "\n")
if not a.existing:
    # Existing cold scheduler major runway (32 MiB cap), plus request and
    # sweep runways (1 MiB each), independent of input size or elapsed time.
    overhead_kib = (32 + 1 + 1) * 1024
    for workload, count, markers in sorted({(r["workload"], r["count"], r["markers"]) for r in rows}):
        group = [r for r in rows if (r["workload"], r["count"], r["markers"]) == (workload, count, markers)]
        sync = [r["rss_kib"] for r in group if r["mode"] == "sync"]
        task = [r["rss_kib"] for r in group if r["mode"] == "async"]
        assert max(task) < 2 * min(sync) + overhead_kib, (workload, count, markers, sync, task)
        # A reproducible ceiling on observed run-to-run spread, not an
        # assertion that OS RSS is deterministic to the page.
        assert max(task) - min(task) < 8 * 1024, (workload, count, markers, task)

