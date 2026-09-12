#!/usr/bin/env python3
"""Compare two runtime archives using identical generated release programs.

Example: python3 scripts/benchmark_runtime_depth.py --before /tmp/before.a \
    --after target/release/libwillow_runtime.a --output /tmp/depth-results.json
Run on an otherwise idle host. Archive build profiles must match.
"""

import argparse
import json
import os
import pathlib
import platform
import statistics
import subprocess
import tempfile
import time


def cases():
    chain = """
fn a(n: i64) -> i64 { CONDITION return n + 1; }
fn b(n: i64) -> i64 { return a(n) + 1; }
fn c(n: i64) -> i64 { return b(n) + 1; }
fn d(n: i64) -> i64 { return c(n) + 1; }
fn main() {
    let mut i = 0; let mut total = 0;
    while i < 1000000 { total = total + d(i); i = i + 1; }
    println(total);
}
"""
    yield "panic_capable_chain", chain.replace(
        "CONDITION", 'if n < 0 { panic("negative"); }'
    )
    yield "pure_chain_control", chain.replace("CONDITION", "")
    yield "panic_stable_region", """
fn step(n: i64) -> i64 { if n < 0 { panic("negative"); } return n + 1; }
fn chain(n: i64) -> i64 {
    let a = step(n); let b = step(a); return step(b);
}
fn main() {
    let mut i = 0; let mut total = 0;
    while i < 1000000 { total = total + chain(i); i = i + 1; }
    println(total);
}
"""
    yield "worker_context_switches", """
async fn worker(n: i64) -> i64 {
    let mut i = 0;
    while i < 1000 {
        if true {
            defer match recover() { Some(_) => {}, None => {} }
            if i % 10 == 0 { panic("worker request"); }
        }
        await yield();
        i = i + 1;
    }
    return n;
}
async fn main() {
    let a = worker(1); let b = worker(2); let c = worker(3); let d = worker(4);
    println(await a + await b + await c + await d);
}
"""
    yield "fib_control", """
fn fib(n: i64) -> i64 {
    if n <= 1 { return n; }
    return fib(n - 1) + fib(n - 2);
}
fn main() { println(fib(30)); }
"""
    for rate, failure in [(0, "false"), (1, "i % 100 == 0"), (10, "i % 10 == 0")]:
        yield f"requests_{rate}pct", """
fn request(fail: bool) {
    if true {
        defer match recover() { Some(_) => {}, None => {} }
        if fail { panic("request failed"); }
    }
}
fn main() {
    let mut i = 0;
    while i < 10000 { request(FAILURE); i = i + 1; }
    println(i);
}
""".replace("FAILURE", failure)
    yield "gc_roots_and_recovery", """
class Item {
    pub value: i64;
    pub init(self, value: i64) { self.value = value; }
}
fn read(item: Item, fail: bool) -> i64 {
    if fail { panic("root cleanup"); }
    return item.value;
}
fn main() {
    let mut i = 0; let mut total = 0;
    while i < 10000 {
        if true {
            defer match recover() { Some(_) => {}, None => {} }
            let item = new Item(i);
            total = total + read(item, i % 10 == 0);
        }
        i = i + 1;
    }
    println(total);
}
"""


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--compiler", type=pathlib.Path, default=pathlib.Path("target/debug/willowc.exe" if os.name == "nt" else "target/debug/willowc"))
    parser.add_argument("--before-compiler", type=pathlib.Path)
    parser.add_argument("--after-compiler", type=pathlib.Path)
    parser.add_argument("--before", required=True, type=pathlib.Path)
    parser.add_argument("--after", required=True, type=pathlib.Path)
    parser.add_argument("--output", required=True, type=pathlib.Path)
    parser.add_argument("--iterations", type=int, default=9)
    args = parser.parse_args()
    if args.iterations < 1:
        parser.error("iterations must be positive")
    results = {"platform": platform.platform(), "machine": platform.machine(), "cases": []}
    with tempfile.TemporaryDirectory(prefix="willow-depth-bench-") as directory:
        root = pathlib.Path(directory)
        for name, source in cases():
            entry = root / f"{name}.wi"
            entry.write_text(source)
            binaries = {}
            expected = None
            samples = {"before": [], "after": []}
            for label in samples:
                binary = root / (f"{name}-{label}" + (".exe" if os.name == "nt" else ""))
                compiler = getattr(args, f"{label}_compiler") or args.compiler
                compilation = subprocess.run([
                    str(compiler.resolve()), "build", str(entry), "--release",
                    "--runtime-lib", str(getattr(args, label).resolve()), "-o", str(binary)
                ], capture_output=True)
                if compilation.returncode:
                    raise RuntimeError(f"{name}/{label} compile failed:\n"
                                       + compilation.stdout.decode(errors="replace")
                                       + compilation.stderr.decode(errors="replace"))
                binaries[label] = binary
                output = subprocess.check_output([str(binary)], timeout=60)
                if expected is None:
                    expected = output
                assert output == expected, f"{name}: archives disagree on output"
            # Alternate order to reduce systematic warm-up/drift bias.
            for iteration in range(args.iterations):
                order = ["before", "after"] if iteration % 2 == 0 else ["after", "before"]
                for label in order:
                    started = time.perf_counter()
                    output = subprocess.check_output([str(binaries[label])], timeout=60)
                    samples[label].append((time.perf_counter() - started) * 1000)
                    assert output == expected, f"{name}: nondeterministic output"
            row = {"name": name, "stdout": expected.decode().strip(), "samples_ms": samples}
            row["artifact_bytes"] = {label: binary.stat().st_size for label, binary in binaries.items()}
            row["median_ms"] = {label: statistics.median(values) for label, values in samples.items()}
            results["cases"].append(row)
            print(name, row["median_ms"], flush=True)
    args.output.write_text(json.dumps(results, indent=2) + "\n")


if __name__ == "__main__":
    main()
