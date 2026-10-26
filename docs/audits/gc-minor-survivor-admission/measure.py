#!/usr/bin/env python3
"""Build a public-source baseline and run only the explicit M4 release probe.

No checkout/reset, staging, private files, or CI access. Results describe this
microbenchmark, not a universally bounded collector. See contract.md for the gate.
"""
import argparse
import io
import hashlib
import json
import math
import os
from pathlib import Path
import platform
import re
import subprocess
import tarfile
import tempfile

ROOT = Path(__file__).resolve().parents[3]
OUT = Path(__file__).resolve().parent
PROBE = Path("crates/willow_runtime/src/gc/minor_m4_probe.rs")
MINOR = Path("crates/willow_runtime/src/gc/minor.rs")
BASE = "8af6c52ba8f6e95e7de14d84f12e9faf98d70995"


def run(args, cwd, env=None):
    return subprocess.run(args, cwd=cwd, env=env, text=True, capture_output=True, check=True)


def baseline():
    if run(["git", "ls-tree", "-r", "--name-only", BASE, "requirements"], ROOT).stdout.strip():
        raise RuntimeError("refusing to archive private material")
    directory = Path(tempfile.mkdtemp(prefix="willow-m4-baseline-"))
    archive = subprocess.check_output(["git", "archive", BASE], cwd=ROOT)
    with tarfile.open(fileobj=io.BytesIO(archive)) as source:
        source.extractall(directory, filter="data")
    probe = (ROOT / PROBE).read_text()
    probe = re.sub(r"        // M4_COMPONENTS_BEGIN.*?// M4_COMPONENTS_END\n", "", probe, flags=re.S)
    (directory / PROBE).write_text(probe)
    with (directory / MINOR).open("a") as source:
        source.write('\n#[cfg(test)]\n#[path = "minor_m4_probe.rs"]\nmod m4_probe;\n')
    return directory


def build(directory, label):
    result = run(["cargo", "test", "-p", "willow_runtime", "--release", "m4_pause_scaling_probe", "--no-run", "--message-format=json"], directory)
    (OUT / f"build-{label}.txt").write_text(result.stderr)
    for line in result.stdout.splitlines():
        record = json.loads(line)
        if record.get("executable") and record.get("target", {}).get("name") == "willow_runtime":
            return record["executable"]
    raise RuntimeError("runtime test executable not found")


def percentile(values, fraction):
    return sorted(values)[max(0, math.ceil(len(values) * fraction) - 1)]


def measure(binary, label, case, parameters, count, raw, trial=0):
    env = os.environ.copy()
    env.update({f"M4_{key.upper()}": str(value) for key, value in parameters.items()})
    env["M4_SAMPLES"] = str(count + 20)
    with tempfile.NamedTemporaryFile() as rss:
        command = ["/usr/bin/time", "-f", "%M", "-o", rss.name, binary,
                      "gc::minor::m4_probe::m4_pause_scaling_probe", "--exact", "--ignored", "--nocapture"]
        result = subprocess.run(command, cwd=ROOT, env=env, text=True, capture_output=True)
        (OUT / f"{case}-{trial}-{label}.stdout").write_text(result.stdout)
        (OUT / f"{case}-{trial}-{label}.stderr").write_text(result.stderr)
        (OUT / f"{case}-{trial}-{label}.status").write_text(str(result.returncode) + "\n")
        result.check_returncode()
        peak_rss = int(Path(rss.name).read_text().strip()) * 1024
    rows = []
    components = []
    for line in result.stdout.splitlines():
        if line.startswith("M4 "):
            rows.append(json.loads(line[3:]))
        elif line.startswith("M4_COMPONENTS "):
            components.append(json.loads(line[len("M4_COMPONENTS "):]))
    assert len(rows) == count + 20
    rows = rows[20:]
    components = components[20:]
    for index, row in enumerate(rows):
        record = {"build": label, "case": case, "trial": trial, **row}
        if components:
            record["components_ns"] = components[index]
        raw.write(json.dumps(record) + "\n")
    pauses = [row["pause_ns"] for row in rows]
    return {
        "build": label, "case": case, "trial": trial, "parameters": parameters, "samples": count,
        "p99_ns": percentile(pauses, .99), "p999_ns": percentile(pauses, .999), "max_ns": max(pauses),
        "gc_cpu_ns_per_allocated_byte": sum(r["gc_cpu_ns"] for r in rows) / sum(r["live_bytes"] for r in rows),
        "objects_per_second": sum(r["objects"] for r in rows) * 1e9 / sum(r["elapsed_ns"] for r in rows),
        "peak_process_rss_bytes": peak_rss,
        "reserved_after_bytes": max(r["reserved_after"] for r in rows),
        "metadata_objects": max(r["metadata_objects"] for r in rows),
        "root_values": max(r["root_values"] for r in rows),
        "mean_components_ns": [sum(c[i] for c in components) / len(components) for i in range(6)] if components else None,
    }


def runtime_hash():
    return hashlib.sha256(b"".join(
        str(p.relative_to(ROOT)).encode() + p.read_bytes()
        for p in sorted((ROOT / "crates/willow_runtime/src").rglob("*.rs"))
    )).hexdigest()


def main():
    parser = argparse.ArgumentParser()
    global OUT
    parser.add_argument("--output-directory", type=Path)
    parser.add_argument("--baseline-directory", type=Path)
    parser.add_argument("--prepare-only", action="store_true")
    parser.add_argument("--samples", type=int, default=500)
    parser.add_argument("--case", action="append")
    parser.add_argument("--trials", type=int, default=1)
    args = parser.parse_args()
    if args.output_directory:
        OUT = args.output_directory.resolve()
        OUT.mkdir(parents=True, exist_ok=False)
    base = args.baseline_directory or baseline()
    if args.prepare_only:
        print(base)
        return
    source_hash = runtime_hash()
    binaries = {"baseline": build(base, "baseline"), "candidate": build(ROOT, "candidate")}
    default = dict(chunks=16, roots=256, pins=0, workers=0, remembered=32)
    cases = [("medium_candidate", default, max(2000, args.samples))]
    for key, values in {"chunks": [1, 64], "roots": [1, 4096], "pins": [64, 1024], "workers": [1, 4, 8], "remembered": [0, 256]}.items():
        for value in values:
            cases.append((f"{key}_{value}", {**default, key: value}, args.samples))
    if args.case:
        assert set(args.case) <= {case[0] for case in cases}, "unknown case"
        cases = [case for case in cases if case[0] in args.case]
    stem = "replicates" if args.case or args.trials != 1 else "measurements"
    raw_name = "replicates-samples.jsonl" if stem == "replicates" else "samples.jsonl"
    summaries = []
    with (OUT / raw_name).open("w") as raw:
        for trial in range(args.trials):
            for case, parameters, count in cases:
                order = list(binaries.items())
                if trial % 2:
                    order.reverse()
                for label, binary in order:
                    summary = measure(binary, label, case, parameters, count, raw, trial)
                    summaries.append(summary)
                    (OUT / "completed.jsonl").open("a").write(json.dumps(summary) + "\n")
                    print(json.dumps(summary), flush=True)
    assert runtime_hash() == source_hash, "runtime source changed during measurement"
    (OUT / f"{stem}.json").write_text(json.dumps({
        "baseline_commit": BASE, "platform": platform.platform(),
        "probe_sha256": hashlib.sha256((ROOT / PROBE).read_bytes()).hexdigest(),
        "runtime_source_sha256": source_hash,
        "rustc": run(["rustc", "--version"], ROOT).stdout.strip(),
        "scope": "user-adopted environment-specific first-survival workload; contract.md",
        "summaries": summaries,
    }, indent=2) + "\n")


if __name__ == "__main__":
    main()
