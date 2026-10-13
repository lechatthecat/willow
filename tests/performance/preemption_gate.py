#!/usr/bin/env python3
"""Reproducible, opt-in A8 gate. Requires cargo, git and an idle native host.

Run: python3 tests/performance/preemption_gate.py --output PATH
Only two historical crate directories and Cargo.lock are read from Git.
No checkout, reset, worktree, network access or private specification is used.
"""
import argparse
import hashlib
import io
import json
import math
import os
from pathlib import Path
import platform
import statistics
import subprocess
import tarfile
import tempfile
from datetime import datetime, timezone

ROOT = Path(__file__).resolve().parents[2]
BASELINE = "29d709faec149209acd86c706c4f79aeead59416"
THRESHOLDS = {
    "never_task_added_ns_per_iteration": 2.0,
    "task_helper_added_ns_per_iteration": 50.0,
    "wake_p99_ns": 20_000_000,
    "async_ratio": 1.10,
}
ENV = {"WILLOW_WORKERS": "1", "WILLOW_TASK_BUDGET": "1024", "WILLOW_TIME_QUANTUM_MS": "10"}


def command(argv, **kwargs):
    return subprocess.check_output(argv, text=True, **kwargs).strip()


def evaluate(samples):
    """Fail closed on absent, invalid or incomparable evidence."""
    required = {"control", "never", "task-control", "task-helper", "wake", "async", "baseline-async"}
    if set(samples) != required:
        raise ValueError("missing or unexpected workload")
    for name, rows in samples.items():
        if len(rows) < (100 if name == "wake" else 7):
            raise ValueError("insufficient samples: " + name)
        for row in rows:
            for key in ("elapsed_ns", "operations"):
                value = row.get(key)
                if type(value) is not int or value <= 0:
                    raise ValueError("invalid " + key)
            if name == "wake":
                latency = row.get("latency_ns")
                if type(latency) is not int or not 0 < latency <= row["elapsed_ns"]:
                    raise ValueError("invalid wake latency")
            if name not in ("async", "baseline-async"):
                calls = row.get("slow_calls")
                if type(calls) is not int or not 0 <= calls <= row["operations"]:
                    raise ValueError("invalid slow call count")
                if type(row.get("checksum")) is not int:
                    raise ValueError("invalid checksum")
        if len({row["operations"] for row in rows}) != 1:
            raise ValueError("unequal workload sizes")
    pairs = [("control", "never"), ("task-control", "task-helper"), ("baseline-async", "async")]
    for before, after in pairs:
        if len(samples[before]) != len(samples[after]):
            raise ValueError("unpaired samples")
        for a, b in zip(samples[before], samples[after]):
            if a["operations"] != b["operations"]:
                raise ValueError("unequal operations")
            if before != "baseline-async" and ("checksum" not in a or a["checksum"] != b.get("checksum")):
                raise ValueError("unequal checksum")
    if any(row.get("slow_calls") != 0 for row in samples["never"]):
        raise ValueError("never-task slow path")
    if any(row.get("slow_calls", 0) <= 0 for name in ("task-helper", "wake") for row in samples[name]):
        raise ValueError("preemption not exercised")
    def delta(before, after):
        return statistics.median((b["elapsed_ns"] - a["elapsed_ns"]) / a["operations"]
                                 for a, b in zip(samples[before], samples[after]))
    wake = sorted(row["latency_ns"] for row in samples["wake"])
    metrics = {
        "never_task_added_ns_per_iteration": delta("control", "never"),
        "task_helper_added_ns_per_iteration": delta("task-control", "task-helper"),
        "wake_p99_ns": wake[math.ceil(len(wake) * .99) - 1],
        "async_ratio": statistics.median(b["elapsed_ns"] / a["elapsed_ns"]
                                          for a, b in zip(samples["baseline-async"], samples["async"])),
    }
    return {"metrics": metrics, "thresholds": THRESHOLDS,
            "passed": all(value <= THRESHOLDS[key] for key, value in metrics.items())}


def baseline_probe(scratch, revision):
    archive = subprocess.check_output(["git", "archive", revision, "crates/willow_runtime", "crates/willow_abi", "Cargo.lock"], cwd=ROOT)
    base = scratch / "baseline"
    base.mkdir()
    with tarfile.open(fileobj=io.BytesIO(archive)) as bundle:
        bundle.extractall(base, filter="data")
    (base / "Cargo.toml").write_text('[workspace]\nmembers = ["crates/willow_runtime", "crates/willow_abi"]\nresolver = "3"\n[profile.release.package.willow_runtime]\ncodegen-units = 1\n')
    examples = base / "crates/willow_runtime/examples"
    examples.mkdir(exist_ok=True)
    source = "#![no_main]\n" + (ROOT / "crates/willow_runtime/examples/preemption_gate/async_probe.rs").read_text()
    source += '\n#[unsafe(no_mangle)] pub extern "C" fn __willow_static_init() {}\n#[unsafe(no_mangle)] pub extern "C" fn willow_user_main() { willow_runtime::gc::willow_gc_init(); let n = std::env::args().nth(2).unwrap().parse::<u64>().unwrap(); println!("{{\\"elapsed_ns\\":{},\\"operations\\":{}}}", run(n), 2 * (n + 1)); }\n'
    (examples / "a8_baseline.rs").write_text(source)
    subprocess.run(["cargo", "build", "--offline", "--release", "--target-dir", str(base / "target"), "-p", "willow_runtime", "--example", "a8_baseline"], cwd=base, check=True)
    return base / "target/release/examples" / ("a8_baseline.exe" if os.name == "nt" else "a8_baseline")


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    action = parser.add_mutually_exclusive_group(required=True)
    action.add_argument("--output", type=Path)
    action.add_argument("--check", type=Path, help="recheck saved raw samples without building/running")
    parser.add_argument("--samples", type=int, default=11)
    parser.add_argument("--iterations", type=int, default=5_000_000)
    parser.add_argument("--async-iterations", type=int, default=20_000)
    args = parser.parse_args()
    if args.check:
        result = evaluate(json.loads(args.check.read_text())["samples"])
        print(json.dumps(result, indent=2))
        raise SystemExit(0 if result["passed"] else 1)
    if args.samples < 7 or args.iterations < 1024 or args.async_iterations < 1:
        parser.error("need >=7 samples, >=1024 loop iterations and positive async iterations")
    revision = command(["git", "rev-parse", BASELINE], cwd=ROOT)
    subprocess.run(["cargo", "build", "--offline", "--release", "--target-dir", str(ROOT / "target"), "-p", "willow_runtime", "--example", "preemption_gate"], cwd=ROOT, check=True)
    binary = ROOT / "target/release/examples" / ("preemption_gate.exe" if os.name == "nt" else "preemption_gate")
    env = {k: v for k, v in os.environ.items() if not k.startswith("WILLOW_")}
    env.update(ENV)
    samples = {name: [] for name in ("control", "never", "task-control", "task-helper", "async", "baseline-async", "wake")}
    with tempfile.TemporaryDirectory(prefix="willow-a8-") as directory:
        baseline = baseline_probe(Path(directory), revision)
        def run(name, count=None):
            exe = baseline if name == "baseline-async" else binary
            mode = "async" if name == "baseline-async" else name
            if count is None:
                count = args.async_iterations if mode == "async" else args.iterations
            return json.loads(command([str(exe), mode, str(count)], env=env, timeout=60))
        for name in samples:
            run(name)  # warm-up, excluded from measured samples
        for index in range(args.samples):
            for pair in (("control", "never"), ("task-control", "task-helper"), ("baseline-async", "async")):
                for name in pair if index % 2 == 0 else reversed(pair):
                    samples[name].append(run(name))
        for _ in range(100):
            samples["wake"].append(run("wake"))
        scaling = []
        for count in (4096, 16384, 65536, 262144):
            rows = {name: run(name, count) for name in ("control", "never", "task-control", "task-helper")}
            assert len({row["checksum"] for row in rows.values()}) == 1
            assert rows["never"]["slow_calls"] == 0
            # A first partial grant plus one runtime check per 64 safepoints.
            # Wall-clock expiration may shorten grants, so leave a bounded
            # allowance; this checks linear work, not a wall-clock inference.
            assert count // 64 <= rows["task-helper"]["slow_calls"] <= count // 64 + count // 1024 + 2
            scaling.append({"iterations": count, "rows": rows})
        result = evaluate(samples)
        result.update({"schema": 1, "platform": platform.platform(), "machine": platform.machine(),
                       "rustc": command(["rustc", "--version", "--verbose"]), "baseline_revision": revision,
                       "measured_at": datetime.now(timezone.utc).isoformat(),
                       "candidate_revision": command(["git", "rev-parse", "HEAD"], cwd=ROOT),
                       "environment": ENV, "samples": samples, "scaling": scaling,
                       "source_sha256": {str(path.relative_to(ROOT)): hashlib.sha256(path.read_bytes()).hexdigest()
                                         for path in (ROOT / "crates/willow_runtime/examples/preemption_gate.rs",
                                                      ROOT / "crates/willow_runtime/examples/preemption_gate/async_probe.rs",
                                                      Path(__file__).resolve(), ROOT / "Cargo.lock")},
                       "probe_sha256": hashlib.sha256(binary.read_bytes()).hexdigest(),
                       "baseline_probe_sha256": hashlib.sha256(baseline.read_bytes()).hexdigest(),
                       "foreign_native_execution": "unverified"})
    args.output.parent.mkdir(parents=True, exist_ok=True)
    args.output.write_text(json.dumps(result, indent=2) + "\n")
    print(json.dumps({key: result[key] for key in ("metrics", "thresholds", "passed")}, indent=2))
    raise SystemExit(0 if result["passed"] else 1)


if __name__ == "__main__":
    main()
