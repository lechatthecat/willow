"""Compare explicit runtime archives using the existing churn harness.

Usage: python3 docs/audits/gc-churn-recovery/measure-runtime.py LABEL ARCHIVE
Build archives from the selected revision with the repository release profile.
The current release compiler and identical benchmark sources isolate runtime cost.
Run without competing compilation or other timed workloads.
"""
import json
from pathlib import Path
import subprocess
import sys
import tempfile

ROOT = Path(__file__).resolve().parents[3]
sys.path.insert(0, str(ROOT / "benches/go_vs_willow"))
import run_sync

label, archive = sys.argv[1:]
archive = str(Path(archive).resolve())
with tempfile.TemporaryDirectory(prefix="gc-churn-") as directory:
    run_sync.BUILD = Path(directory)
    for case in ("array_grow_only", "array_literal_churn", "object_churn"):
        subprocess.run(
            [str(run_sync.WILLOWC), "build", str(run_sync.SYNC / "willow" / f"{case}.wi"),
             "--release", "--runtime-lib", archive,
             "-o", str(run_sync.BUILD / f"willow-{case}")],
            cwd=ROOT, check=True, stdout=subprocess.DEVNULL,
        )
        for trial in range(5):
            row = run_sync.one_run("willow", case, 8)
            row.update(label=label, trial=trial)
            print(json.dumps(row, sort_keys=True), flush=True)
