#!/usr/bin/env python3
"""Exercise actual V2 runtime stops and independently check the trace.

Outputs source-independent runtime measurements only; generated executables and
cargo build artifacts stay in Cargo's target directory. Requires Python/Rust.
"""

import argparse
import json
import os
from pathlib import Path
import subprocess
import sys

sys.dont_write_bytecode = True
from gc_stop_trace_check import check


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--output", type=Path, required=True)
    parser.add_argument("--major-only", action="store_true",
                        help="measure normal old cycles separately from the remaining minor stop")
    args = parser.parse_args()
    output = args.output.resolve()
    output.mkdir(parents=True, exist_ok=True)
    trace = output / "stops.ndjson"
    # The runtime trace writer appends. A fresh measurement must not mix runs.
    if trace.exists():
        parser.error("use a fresh output directory: stops.ndjson already exists")
    root = Path(__file__).resolve().parents[1]
    env = dict(os.environ, WILLOW_GC_TRACE=str(trace), WILLOW_GC_TRACE_VERSION="2",
               WILLOW_STOP_TRACE_PROBE_DIR=str(output))
    if args.major_only:
        env["WILLOW_STOP_TRACE_PROBE_MAJOR_ONLY"] = "1"
    subprocess.run(["cargo", "test", "--locked", "-p", "willow_runtime",
                    "gc_telemetry::tests::stop_trace_probe", "--", "--exact"],
                   cwd=root, env=env, check=True)
    before = json.loads((output / "before.json").read_text())
    after = json.loads((output / "after.json").read_text())
    with trace.open() as stream:
        result = check(stream, before, after)
    assert result["by_reason"] == ([0, 0, 0, 0] if args.major_only else [0, 0, 1, 0]), result
    assert result["zero_stw"] == args.major_only, result
    (output / "result.json").write_text(json.dumps(result, indent=2) + "\n")
    print(json.dumps(result))


if __name__ == "__main__":
    main()
