#!/usr/bin/env python3
"""Focused regression checks; deliberately excludes full suites and CI."""
import json
from pathlib import Path
import subprocess
import time

runtime_filters = [
    "inline_satb", "gc::satb::tests::", "satb_child_first_filter_preserves",
    "satb_registration_owns_buffer_and_unwind_unregister_drains_it",
    "closing_phase_publishes_deletions_directly",
    "concurrent_closure_and_sweep_preserve_a_new_active_tlab",
    "overlap_graph_copy_promotion_and_snapshot_only_reachability",
    "overlap_containers_and_async_frame_preserve_rewritten_edges",
    "stress_region_05_minor_major_and_remembered_set_interleave",
    "stress_region_11_generated_graph_preserves_edges_through_moving_collection",
    "bridge_installs_thread_and_nested_stable_contexts",
    "shadow_roots_unregister_empty_bridge_keeps_admission",
]
commands = [["cargo", "fmt", "--all", "--", "--check"]]
commands += [["cargo", "test", "-p", "willow_runtime", "--lib", name] + (["--", "--ignored"] if name.startswith("stress_region_") else []) for name in runtime_filters]
commands += [["cargo", "test", "-p", "willow", "--lib", "inline_satb_codegen", "--", "--nocapture"]]
commands += [["cargo", "test", "-p", "willow_abi", "--lib", "fingerprint::tests"]]
commands += [["cargo", "clippy", "--workspace", "--all-targets", "--target", target, "--", "-D", "warnings"]
             for target in ["x86_64-unknown-linux-gnu", "x86_64-pc-windows-msvc", "aarch64-apple-darwin", "x86_64-apple-darwin"]]
results = []
output = Path(__file__).with_name("validation.json")
for command in commands:
    start = time.monotonic()
    result = subprocess.run(command, capture_output=True, text=True)
    selected = [line for line in (result.stdout + result.stderr).splitlines()
                if line.startswith(("test ", "sites=", "error", "    Finished"))]
    row = dict(command=command, exit_code=result.returncode, seconds=time.monotonic()-start, results=selected)
    results.append(row)
    output.write_text(json.dumps(results, indent=2)+"\n")
    print(" ".join(command), result.returncode, flush=True)
    if result.returncode:
        print(result.stdout, result.stderr)
        raise SystemExit(result.returncode)
    if "test" in command and not any(line.startswith("test result: ok.") and " 0 passed" not in line for line in selected):
        raise SystemExit("test filter executed no passing tests")
