"""Run checked five-trial comparisons serially, with immutable source evidence."""
import argparse
import hashlib
import json
import os
from pathlib import Path
import subprocess

ROOT = Path(__file__).resolve().parents[3]
HERE = Path(__file__).resolve().parent
parser = argparse.ArgumentParser()
parser.add_argument("label")
parser.add_argument("--cpus", default="0-7")
args = parser.parse_args()
assert args.label.replace("-", "").isalnum()
for key in ("WILLOW_GC_STRESS", "WILLOW_GC_PACER", "WILLOW_GC_NURSERY_BYTES", "WILLOW_GC_MARK_THREADS", "WILLOW_GC_LOG", "WILLOW_GC_PROFILE"):
    assert key not in os.environ, key

def hashes():
    paths = subprocess.check_output(["git", "ls-files", "crates", "src"], cwd=ROOT, text=True).splitlines()
    return {p: hashlib.sha256((ROOT / p).read_bytes()).hexdigest() for p in paths}

before = hashes()
meta = {"head": subprocess.check_output(["git", "rev-parse", "HEAD"], cwd=ROOT, text=True).strip(),
        "source_tree_sha256": hashlib.sha256(json.dumps(before, sort_keys=True).encode()).hexdigest(),
        "changed_source_sha256": {p: before[p] for p in subprocess.check_output(["git", "diff", "--name-only", "--", "crates", "src"], cwd=ROOT, text=True).splitlines()},
        "affinity": args.cpus, "workers": 8,
        "asset_sha256": {p.name: hashlib.sha256(p.read_bytes()).hexdigest() for p in sorted((HERE / "assets").iterdir())}}
(HERE / (args.label + "-metadata.json")).write_text(json.dumps(meta, indent=2) + "\n")
for name, command in [
    ("comparison", ["node", str(HERE / "assets/harness.js"), "run", "linked_list", "object_churn", "gc_scheduler", "virtual_dispatch", "--trials", "5"]),
    ("chain", ["node", str(HERE / "assets/probe.js"), "chain"]),
]:
    with (HERE / (args.label + "-" + name + ".jsonl")).open("w") as out, (HERE / (args.label + "-" + name + ".stderr")).open("w") as err:
        subprocess.run(["taskset", "-c", args.cpus] + command, cwd=ROOT, stdout=out, stderr=err, check=True, timeout=900)
    assert hashes() == before, "source changed during measurement"
    print(name + " complete", flush=True)
