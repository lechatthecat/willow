#!/usr/bin/env bash
set -euo pipefail
audit=docs/audits/gc-survivor-copy
for name in object_churn array_literal_churn; do
  rtk proxy target/release/willow build "$audit/${name}_stress.wi" -o "target/gc-survivor-copy-$name" --release
  for mode in minor relocate; do
    rtk proxy env WILLOW_GC_STRESS="$mode" "target/gc-survivor-copy-$name" > "$audit/$name-$mode.txt"
  done
done
rtk proxy python3 - <<'PY'
from pathlib import Path
p = Path("docs/audits/gc-survivor-copy")
expected = sum(i % 1000 for i in range(10000 - 1024))
for name in ("object_churn", "array_literal_churn"):
    for mode in ("minor", "relocate"):
        lines = (p / f"{name}-{mode}.txt").read_text().splitlines()
        assert int(lines[lines.index("END") + 1]) == expected, (name, mode, lines)
        print(name, mode, "checksum", expected, "PASS")
expected_example = "50005000\n43\n6\nalphabetagamma\n10\n5050\n2555840\n87\n1999000\n"
for mode in ("normal", "minor", "relocate"):
    assert (p / f"example-{mode}.txt").read_text() == expected_example
print("array_literal_churn example: normal/minor/relocate PASS")
PY
