"""Focused GC gates; deliberately excludes full suites and CI polling."""
import json
from pathlib import Path
import subprocess
import time

ROOT = Path(__file__).resolve().parents[3]
HERE = Path(__file__).resolve().parent
commands = [["cargo", "fmt", "--all", "--", "--check"],
            ["cargo", "clippy", "--workspace", "--all-targets", "--", "-D", "warnings"]]
for target in ["x86_64-unknown-linux-gnu", "x86_64-pc-windows-msvc", "aarch64-apple-darwin", "x86_64-apple-darwin"]:
    commands.append(["cargo", "clippy", "--workspace", "--all-targets", "--target", target, "--", "-D", "warnings"])
filters = ["gc::pacer::tests", "tenuring_feedback", "persistent_survival", "minor_survivor_cost_and_page_index_scale", "promotion_cursor", "fully_forwarded_chunks", "gc::minor::overlap_tests", "memory_limit_bounds_region_reservations", "gc::memory_control::tests", "concurrent_chain_continuations", "concurrent_nonleaf_fanout", "expired_assist_budget", "assist_remark_boundary", "marker_panics_publish_failure", "cached_epoch_region_bounds_and_query_counts"]
for name in filters:
    commands.append(["cargo", "test", "-p", "willow_runtime", "--lib", name, "--", "--nocapture"])
for name in ["gc::pacer::tests", "persistent_survival", "concurrent_chain_continuations", "expired_assist_budget"]:
    commands.append(["cargo", "test", "-p", "willow_runtime", "--release", "--lib", name, "--", "--nocapture"])
for release in [[], ["--release"]]:
    for name in ["stress_region_07", "stress_region_11"]:
        commands.append(["cargo", "test", "-p", "willow_runtime"] + release + ["--lib", name, "--", "--ignored", "--nocapture"])
with (HERE / "validation.jsonl").open("w") as output:
    for i, cmd in enumerate(commands):
        log = HERE / ("gate-%02d.txt" % i)
        print(i, " ".join(cmd), flush=True)
        started = time.monotonic()
        with log.open("w") as capture:
            result = subprocess.run(cmd, cwd=ROOT, stdout=capture, stderr=subprocess.STDOUT, timeout=900)
        record = dict(command=cmd, status=result.returncode, seconds=time.monotonic()-started, log=log.name)
        output.write(json.dumps(record)+"\n"); output.flush()
        assert result.returncode == 0, record
        if "test" in cmd:
            assert "running 0 tests" not in log.read_text(), record
