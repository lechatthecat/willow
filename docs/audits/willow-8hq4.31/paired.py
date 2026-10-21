"""Run prebuilt before/after binaries using the existing local suite timer.

Build target/8hq4-31-{before,after}-{object_churn,array_literal_churn}
with the same release compiler and the respective runtime source revision.
Run from repository root; local benches/go_vs_willow is required.
"""
import json
from pathlib import Path
import sys

sys.path.insert(0, str(Path("benches/go_vs_willow").resolve()))
import run_sync

for case in ("object_churn", "array_literal_churn"):
    for trial in range(1, 8):
        # Alternate the first treatment to reduce ordering bias.
        for variant in (("before", "after") if trial % 2 else ("after", "before")):
            binary = Path(f"target/8hq4-31-{variant}-{case}").resolve()
            run_sync.command = lambda language, case, workers: [str(binary)]
            row = run_sync.one_run("willow", case, 8)
            print(json.dumps(dict(row, variant=variant, trial=trial)), flush=True)
