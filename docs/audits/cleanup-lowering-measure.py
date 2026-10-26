#!/usr/bin/env python3
"""Compare identical generated sources using two prebuilt compilers (Linux ELF).

Build baseline from detached 7644eb5 with cargo +1.96.0 build --release --bin
willow, copy its binary, then build current with the same toolchain/profile.
Example:
  python3 docs/audits/cleanup-lowering-measure.py --baseline BASE --current CURRENT \
    --runtime target/release/libwillow_runtime.a --output comparison.json
Sources, binaries and objects remain in --work-dir (default a temporary directory).
Timings include compiler startup, parsing, lowering, codegen AND native linking;
they exclude compiler/runtime builds. Warm up each shape/compiler, alternate
baseline/current samples, preserve every sample, and compare median durations.
"""
import argparse
import hashlib
import json
import os
from pathlib import Path
import platform
import statistics
import struct
import subprocess
import tempfile
import time


def digest(path):
    return hashlib.sha256(Path(path).read_bytes()).hexdigest()


def sources(n):
    nested = "fn work(x: i64) -> i64 {\n"
    nested += "if x >= 0 { defer println(x);\n" * n
    nested += "println(x);\n" + "}\n" * n + "return x; }\nfn main() { println(work(1)); }\n"
    exits = "fn work(x: i64) -> i64 { defer println(x);\n"
    exits += "".join(f"if x == {i} {{ defer println({i}); return {i}; }}\n" for i in range(n))
    exits += "return -1; }\nfn main() { println(work(1)); }\n"
    defers = "fn work(x: i64) {\n" + "".join(f"defer println(x + {i});\n" for i in range(n))
    defers += "println(x); }\nfn main() { work(1); }\n"
    locks = "async fn work(m: Mutex<i64>) {\n"
    locks += "await sleep(0); lock m as mut v { defer println(v); v = v + 1; }\n" * n
    locks += "}\nasync fn main() { await work(Mutex::new(0)); }\n"
    return {"deep_scopes": nested, "fanout_returns": exits, "many_defers": defers, "await_lock": locks}


def executable_bytes(path):
    data = path.read_bytes()
    if data[:6] != b"\x7fELF\x02\x01":
        raise ValueError(f"expected little-endian ELF64 object: {path}")
    offset = struct.unpack_from("<Q", data, 40)[0]
    entry_size, count = struct.unpack_from("<HH", data, 58)
    # SHF_EXECINSTR counts actual generated code, excluding runtime/linker code.
    return sum(struct.unpack_from("<Q", data, offset + i * entry_size + 32)[0]
               for i in range(count)
               if struct.unpack_from("<Q", data, offset + i * entry_size + 8)[0] & 4)


def compile_one(compiler, runtime, source, output):
    env = os.environ.copy()
    env["WILLOW_KEEP_OBJECT"] = "1"
    start = time.perf_counter_ns()
    result = subprocess.run([str(compiler), "build", str(source), "-o", str(output),
                             "--release", "--runtime-lib", str(runtime)],
                            env=env, capture_output=True, text=True)
    elapsed = (time.perf_counter_ns() - start) / 1e9
    if result.returncode:
        raise RuntimeError(f"compile failed ({compiler}, {source}):\n{result.stdout}\n{result.stderr}")
    obj = output.with_suffix(".o")
    return {"seconds": elapsed, "object_bytes": obj.stat().st_size,
            "executable_section_bytes": executable_bytes(obj)}


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--baseline", required=True, type=Path)
    parser.add_argument("--current", type=Path)
    parser.add_argument("--runtime", required=True, type=Path)
    parser.add_argument("--output", required=True, type=Path)
    parser.add_argument("--work-dir", type=Path)
    parser.add_argument("--repeats", type=int, default=5)
    parser.add_argument("--sizes", nargs="+", type=int, default=[8, 16, 32])
    args = parser.parse_args()
    if args.repeats < 3:
        parser.error("at least three repeats required")
    work = (args.work_dir or Path(tempfile.mkdtemp(prefix="willow-cleanup-measure-"))).resolve()
    work.mkdir(parents=True, exist_ok=True)
    compilers = {"baseline": args.baseline.resolve()}
    if args.current:
        compilers["current"] = args.current.resolve()
    runtime = args.runtime.resolve()
    report = {"baseline_revision": "7644eb5", "platform": platform.platform(),
              "method": "one warmup per shape/compiler; alternating samples; wall time includes linking",
              "runtime_sha256": digest(runtime), "compiler_sha256": {k: digest(v) for k,v in compilers.items()},
              "repeats": args.repeats, "measurements": []}
    for n in args.sizes:
        for shape, source in sources(n).items():
            path = work / f"{shape}_{n}.wi"
            path.write_text(source)
            row = {"shape": shape, "n": n, "source_bytes": len(source.encode()),
                   "source_sha256": digest(path), "samples": {k: [] for k in compilers}}
            for label, compiler in compilers.items():
                compile_one(compiler, runtime, path, work / f"{shape}_{n}_{label}")
            for repeat in range(args.repeats):
                order = list(compilers.items())
                if repeat % 2:
                    order.reverse()
                for label, compiler in order:
                    row["samples"][label].append(compile_one(compiler, runtime, path, work / f"{shape}_{n}_{label}"))
            row["median_seconds"] = {k: statistics.median(s["seconds"] for s in v) for k,v in row["samples"].items()}
            for label, samples in row["samples"].items():
                for field in ("object_bytes", "executable_section_bytes"):
                    if len({s[field] for s in samples}) != 1:
                        raise RuntimeError(f"nondeterministic {field}: {shape}, {label}")
            if args.current:
                row["current_over_baseline_time"] = row["median_seconds"]["current"] / row["median_seconds"]["baseline"]
                row["current_minus_baseline_code_bytes"] = row["samples"]["current"][0]["executable_section_bytes"] - row["samples"]["baseline"][0]["executable_section_bytes"]
            report["measurements"].append(row)
            args.output.write_text(json.dumps(report, indent=2) + "\n")
            print(shape, n, row["median_seconds"], flush=True)


if __name__ == "__main__":
    main()
