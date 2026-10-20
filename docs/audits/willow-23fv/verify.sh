#!/usr/bin/env bash
# Run from the repository root. Reuses the existing local benchmark suite.
set -euo pipefail
audit=docs/audits/willow-23fv
case "${1:-checks}" in
checks)
  rtk cargo fmt --all -- --check
  for filter in minor_deferral minor::overlap cycle_wait::tests nursery:: \
    test_gc_tlab_refill_slow_path_coordinates_threshold_collection \
    test_gc_tlab_large_object_uses_old_region_slow_path; do
    rtk cargo test -p willow_runtime --lib "$filter" -- --nocapture
  done
  for target in x86_64-unknown-linux-gnu x86_64-pc-windows-msvc \
    aarch64-apple-darwin x86_64-apple-darwin; do
    rtk cargo clippy --workspace --all-targets --target "$target" -- -D warnings
  done
  rtk cargo build --release --bin willow
  rtk proxy target/release/willow build example/gc_mutator_registration.wi \
    -o target/willow-23fv-mutators --release
  rtk proxy env WILLOW_WORKERS=8 target/willow-23fv-mutators
  ;;
benchmarks)
  rtk cargo build --release --bin willow
  rtk proxy target/release/willow build benches/go_vs_willow/willow/gc_scheduler.wi \
    -o benches/go_vs_willow/build/gc_scheduler --release
  for name in object_churn array_literal_churn; do
    rtk proxy target/release/willow build "benches/go_vs_willow/sync/willow/$name.wi" \
      -o "benches/go_vs_willow/build/sync/willow-$name" --release
  done
  rtk proxy python3 benches/go_vs_willow/run.py gc_scheduler 10000 100 \
    --trials 5 --workers 8 --languages willow --no-build > "$audit/reproduced-gc.txt"
  rtk proxy python3 benches/go_vs_willow/run_sync.py object_churn array_literal_churn \
    --trials 5 --workers 8 --languages willow --no-build > "$audit/reproduced-churn.txt"
  rtk proxy python3 - <<'PY'
import json
import statistics
from pathlib import Path

audit = Path("docs/audits/willow-23fv")
def rows(path):
    return [json.loads(line) for line in path.read_text().splitlines() if line.startswith("{")]

gc = [r["elapsed_ns"] for r in rows(audit / "reproduced-gc.txt")]
assert len(gc) == 5
print("gc_scheduler ms:", [t / 1e6 for t in gc])
assert statistics.median(gc) <= 110_000_000 and max(gc) <= 150_000_000
baseline = json.loads((audit / "measurements.json").read_text())["runs"]["baseline-2026-10-19"]
churn = rows(audit / "reproduced-churn.txt")
for case in ("object_churn", "array_literal_churn"):
    before = statistics.median(r["elapsed_ns"] for r in baseline if r["benchmark"] == case)
    trials = [r["elapsed_ns"] for r in churn if r["benchmark"] == case]
    assert len(trials) == 5
    after = statistics.median(trials)
    print(case, "median ms:", after / 1e6, "delta %:", (after / before - 1) * 100)
    assert after <= before * 1.05
PY
  ;;
*) exit 2 ;;
esac
