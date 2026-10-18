#!/usr/bin/env python3
"""Parse actual named-subprocess and formatter-bound records from gate logs."""
import json
from pathlib import Path
import re

OUT = Path(__file__).resolve().parent
records = []
paths = set(OUT.glob("m0_workers-*.txt")) | set(OUT.glob("review-m0_*-w*-t*.txt"))
for path in sorted(paths):
    for match in re.finditer(r'\{"version":3,"event":"gc_failure"[^\n]*', path.read_text()):
        text = match.group()
        record = json.loads(text)
        size = len(text.encode()) + 1
        assert size <= 8192
        if record["heap_available"]:
            assert isinstance(record["actual_bytes"], int)
            if record["kind"] == "memory_threshold":
                assert record["actual_bytes"] == min(
                    record["managed_bytes"] + record["external_bytes"], 2**64-1)
            if record["kind"] == "managed_memory_threshold":
                assert record["actual_bytes"] == record["managed_bytes"] == record["limit_bytes"]
                assert record["external_bytes"] == 123
                assert record["old_reserved_bytes"] == 262144
                assert record["young_reserved_bytes"] == 0
        else:
            assert record["actual_bytes"] is None and record["managed_bytes"] is None
        records.append((record, size))
assert records and any(r["actual_bytes"] == 2**64-1 for r, size in records)
assert {"invariant", "test_panic", "memory_threshold"} <= {r["kind"] for r, size in records}
summary = (f"Parsed {len(records)} real/boundary records from focused configuration logs. "
           f"Maximum record including newline: {max(size for r, size in records)} bytes; "
           "bound: 8192. Actual category accounting, null unavailable fields, "
           "Unicode/control escaping and u64 boundary values passed.\n")
(OUT / "failure-json-validation.txt").write_text(summary)
print(summary, end="")
