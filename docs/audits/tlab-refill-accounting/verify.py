#!/usr/bin/env python3
"""Focused, reproducible accounting validation; never runs the full suite."""
from pathlib import Path
import subprocess

root = Path(__file__).resolve().parents[3]
out = Path(__file__).resolve().parent
filters = [
    "refill_accounting_",
    "derived_fast_tlab_counts_survive_refills_retirement_and_unregister",
    "telemetry_merges_fast_tlab_deltas_once_and_on_unregister",
    "test_gc_tlab_refill_slow_path_coordinates_threshold_collection",
    "cumulative_allocation_bytes_merge_fast_path_once_and_scale_by_tlabs",
    "minor_deferral_",
    "threshold_policy_old_lab_pressure_matrix",
    "cumulative_allocation_bytes_merge_old_lab_without_indexing",
    "partial_retirement_refill_flushes_only_remaining_suffix_without_double_accounting",
    "disabled_pacer_preserves_soft_memory_trigger",
    "memory_limit_also_bounds_generated_tlab_reservations",
]
commands = [["cargo", "fmt", "--all", "--", "--check"]]
commands += [["cargo", "test", "-p", "willow_runtime", f, "--", "--nocapture"] for f in filters]
commands += [["cargo", "clippy", "--workspace", "--all-targets", "--target", target, "--", "-D", "warnings"] for target in [
    "x86_64-unknown-linux-gnu", "x86_64-pc-windows-msvc",
    "aarch64-apple-darwin", "x86_64-apple-darwin",
]]
with (out / "validation.txt").open("w") as log:
    for cmd in commands:
        line = "$ " + " ".join(cmd)
        print(line, flush=True)
        log.write(line + "\n")
        log.flush()
        result = subprocess.run(cmd, cwd=root, stdout=log, stderr=subprocess.STDOUT)
        log.write(f"exit={result.returncode}\n")
        log.flush()
        if result.returncode:
            raise SystemExit(result.returncode)
