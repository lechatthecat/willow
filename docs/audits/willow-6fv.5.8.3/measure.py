#!/usr/bin/env python3
"""Focused reference-host pacer measurements; no full test suite."""
import hashlib
import json
import os
import pathlib
import platform
import subprocess

root = pathlib.Path(__file__).resolve().parents[3]
rows = []
for repeat in range(3):
    result = subprocess.run([
        "rtk", "proxy", "cargo", "test", "-p", "willow_runtime", "--release", "--lib",
        "gc::pacer::tests::pacer_reference_measurement", "--", "--exact", "--ignored", "--nocapture",
    ], cwd=root, capture_output=True, text=True, check=True)
    for line in result.stdout.splitlines():
        if not line.startswith(("PACER_MODEL ", "PACER_RUNTIME ")):
            continue
        kind, *fields = line.split()
        row = {"kind": kind, "repeat": repeat}
        for field in fields:
            key, value = field.split("=", 1)
            row[key] = value == "true" if value in ("true", "false") else int(value)
        rows.append(row)
    print(f"completed repeat {repeat + 1}", flush=True)
sources = ["crates/willow_runtime/src/gc/pacer.rs", "crates/willow_runtime/src/gc/mark_workers.rs", "crates/willow_runtime/src/gc.rs", "crates/willow_runtime/src/gc_telemetry.rs", "crates/willow_runtime/src/gc_telemetry/pacing.rs"]
report = {
    "source_sha256": {name: hashlib.sha256((root / name).read_bytes()).hexdigest() for name in sources},
    "system": platform.system(), "machine": platform.machine(),
    "affinity_cpus": len(os.sched_getaffinity(0)) if hasattr(os, "sched_getaffinity") else os.cpu_count(),
    "rustc": subprocess.check_output(["rtk", "proxy", "rustc", "--version"], text=True).strip(),
    "measurements": rows,
}
path = pathlib.Path(__file__).with_name("measurements.json")
path.write_text(json.dumps(report, indent=2) + "\n")
print(path)
