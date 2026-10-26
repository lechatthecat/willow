#!/usr/bin/env python3
"""Focused M0 checks only; never runs the full test suite or reads local specs."""
import os
import json
import re
from pathlib import Path
import subprocess
import sys

ROOT = Path(__file__).resolve().parents[3]
OUTPUT = Path(__file__).resolve().parent

def run(name, args, env=None):
    settings = {key: (env or os.environ).get(key) for key in (
        "WILLOW_WORKERS", "WILLOW_GC_MARK_THREADS", "WILLOW_GC_VERIFY_REGIONS", "WILLOW_GC_VERIFY_BARRIER")}
    (OUTPUT / f"{name}-config.json").write_text(json.dumps({"argv": args, "settings": settings}, indent=2) + "\n")
    result = subprocess.run(["rtk", "proxy", *args], cwd=ROOT, env=env,
                            stdout=subprocess.PIPE, stderr=subprocess.STDOUT, text=True)
    (OUTPUT / f"{name}.txt").write_text(result.stdout)
    print(f"{name}: exit={result.returncode}", flush=True)
    if "test" in args and not re.search(r"test result: ok\. [1-9][0-9]* passed;", result.stdout):
        if result.returncode == 0:
            raise AssertionError(f"{name}: filter selected no tests")
    if result.returncode:
        print(result.stdout[-6000:], flush=True)
        sys.exit(result.returncode)

if __name__ == "__main__":
    run("fmt", ["cargo", "fmt", "--all", "--", "--check"])
    run("clippy-host", ["cargo", "clippy", "--workspace", "--all-targets", "--", "-D", "warnings"])
    for target in ["x86_64-unknown-linux-gnu", "x86_64-pc-windows-msvc", "aarch64-apple-darwin", "x86_64-apple-darwin"]:
        run(f"clippy-{target}", ["cargo", "clippy", "--workspace", "--all-targets", "--target", target, "--", "-D", "warnings"])
    for workers in [1, 2, 4, 8]:
        env = dict(os.environ, WILLOW_WORKERS=str(workers), WILLOW_GC_MARK_THREADS=str(workers),
                   WILLOW_GC_VERIFY_REGIONS="1", WILLOW_GC_VERIFY_BARRIER="1")
        for threads in [1, 4]:
            for focused in ["m0_", "milestone_"]:
                run(f"{focused}workers-{workers}-threads-{threads}",
                    ["cargo", "test", "-p", "willow_runtime", focused, "--", f"--test-threads={threads}", "--nocapture"], env)
    run("telemetry-regression", ["cargo", "test", "-p", "willow_runtime", "gc_telemetry::", "--", "--test-threads=4"])
    run("example", ["cargo", "run", "-p", "willow_runtime", "--example", "gc_milestones"])
    for focused in [
        "unchanged_high_reservations_do_not_collect_per_managed_allocation",
        "short_lived_string_churn_bounds_reservations_and_rss",
        "marker_panics_publish_failure_and_retire_worker_collector_and_assist",
        "satb_native_container_overwrites_and_removals_publish_old_references",
        "allocation_failures_terminate_before_returning_to_callers",
        "legacy_root_owner_",
        "root_handshake_refuses_an_unregistered_owner_with_a_nonempty_stack",
        "registered_peers_do_not_hide_an_unregistered_foreign_root_owner",
        "a_mutator_that_dies_registered_reports_instead_of_hanging_the_collector",
        "a_registered_mutator_that_panics_still_unregisters",
        "test_gc_minor_collection_defers_roots_parked_from_runtime_code",
        "tenure_budget_failure_retains_survivor_chunk_as_pinned",
        "gc::free_spans::tests::",
        "scalar_stores_skip_the_write_barrier",
        "explicit_collect_joins_active_cycles_without_busy_polling",
        "budget_wait_unwind_completes_generation_and_allows_next_election",
        "normal_handle_concurrent_snapshot_churn_uses_live_atomic_slots",
        "scoped_loan_park_resume_cancel_panic_and_stack_alias",
        "minor_deferral_concurrent_reclamation_and_pin_release",
        "concurrent_edge_publication_and_new_allocation_survive_remark",
    ]:
        run(focused.replace("::", "-"), ["cargo", "test", "-p", "willow_runtime", focused,
            "--", "--test-threads=1", "--nocapture"],
            dict(os.environ, WILLOW_GC_VERIFY_REGIONS="1", WILLOW_GC_VERIFY_BARRIER="1")
            if focused == "satb_native_container_overwrites_and_removals_publish_old_references" else None)
    for focused in ["satb_codegen_loads_old_before_each_reference_store_in_linear_code",
                    "interface_pair_stores_trace_only_object_word_in_linear_code"]:
        run(focused, ["cargo", "test", "-p", "willow", "--lib", focused, "--", "--nocapture"])
    run("barrier-proof", ["python3", str(OUTPUT / "barrier_proof.py")])
    run("abi-build", ["cc", "-std=c11", "-Wall", "-Wextra", "-Werror",
        str(OUTPUT / "abi_layout.c"), "-o", "/tmp/willow-m0-abi"])
    run("abi-layout", ["/tmp/willow-m0-abi"])
    rust = (OUTPUT / "m0_workers-1-threads-1.txt").read_text()
    c = (OUTPUT / "abi-layout.txt").read_text().strip()
    assert c.startswith("m0_abi ") and c in rust, ("C/Rust ABI mismatch", c)
    run("failure-json-parse", ["python3", str(OUTPUT / "validate_failure_json.py")])
