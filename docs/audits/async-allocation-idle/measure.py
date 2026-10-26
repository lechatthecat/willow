#!/usr/bin/env python3
"""Build paired churn probes and record Linux process time/switches and futex calls.
Run after building the release willow binary and willow_runtime library.
Binaries/sources generated from the checked-in example stay in a temporary dir.
"""
import argparse
import json
import os
from pathlib import Path
import resource
import shutil
import subprocess
import tempfile
import time

parser = argparse.ArgumentParser()
parser.add_argument("--output", type=Path, required=True)
parser.add_argument("--counts", type=int, nargs="+", default=[1000000, 10000000, 40000000])
parser.add_argument("--trials", type=int, default=3)
parser.add_argument("--workers", type=lambda s: s if s == "default" else int(s),
                    nargs="+", default=["default", 1, 4, 16])
parser.add_argument("--existing", type=Path, help="reuse a prebuilt sync/async pair (one count only)")
parser.add_argument("--strace", action="store_true")
args = parser.parse_args()
if args.existing and len(args.counts) != 1:
    parser.error("--existing requires exactly one loop count")
if min(args.counts) < 64 or args.trials < 1 or any(w != "default" and w < 1 for w in args.workers):
    parser.error("counts must be >=64, trials/workers positive")
repo = Path(__file__).resolve().parents[3]
source = (repo / "example/async_allocation_churn.wi").read_text()
with tempfile.TemporaryDirectory(prefix="willow-churn-") as tmp, args.output.open("w") as out:
    tmp = Path(tmp)
    for count in args.counts:
        binaries = {}
        for mode in ["sync", "async"]:
            binary = (args.existing if args.existing else tmp) / mode
            if not args.existing:
                code = source.replace("100000, keep", f"{count}, keep")
                if mode == "sync":
                    code = code.replace("async fn main()", "fn main()")
                path = tmp / f"{mode}.wi"
                path.write_text(code)
                subprocess.run([str(repo / "target/release/willow"), "build", str(path), "--release", "-o", str(binary)], check=True, capture_output=True)
            binaries[mode] = binary
        for workers in args.workers:
            env = dict(os.environ)
            if workers == "default":
                env.pop("WILLOW_WORKERS", None)
            else:
                env["WILLOW_WORKERS"] = str(workers)
            for trial in range(args.trials):
                for mode, binary in binaries.items():
                    before = resource.getrusage(resource.RUSAGE_CHILDREN)
                    start = time.monotonic_ns()
                    result = subprocess.run([str(binary)], env=env, check=True, capture_output=True, text=True, timeout=120)
                    wall = (time.monotonic_ns() - start) / 1e9
                    after = resource.getrusage(resource.RUSAGE_CHILDREN)
                    expected = f"{128 * count - 4096}\ntrue\n"
                    assert result.stdout == expected, (result.stdout, expected)
                    record = dict(count=count, workers=workers, mode=mode, trial=trial + 1, wall_s=wall,
                                  user_s=after.ru_utime-before.ru_utime, sys_s=after.ru_stime-before.ru_stime,
                                  voluntary=after.ru_nvcsw-before.ru_nvcsw, involuntary=after.ru_nivcsw-before.ru_nivcsw,
                                  output=result.stdout.strip())
                    out.write(json.dumps(record) + "\n")
                    out.flush()
            if args.strace:
                assert shutil.which("strace"), "strace required"
                trace = subprocess.run(["strace", "-qq", "-f", "-c", "-e", "futex", str(binaries["async"])], env=env,
                                       capture_output=True, text=True, timeout=120, check=True)
                out.write(json.dumps(dict(count=count, workers=workers, strace=trace.stderr)) + "\n")
