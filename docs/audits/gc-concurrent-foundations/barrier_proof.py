#!/usr/bin/env python3
"""Narrow generated-store proof; fixed inputs, no example-suite scan."""
import json
import os
from pathlib import Path
import subprocess

ROOT = Path(__file__).resolve().parents[3]
OUT = Path(__file__).resolve().parent

def run(name, argv, env=None):
    p = subprocess.run(["rtk", "proxy", *argv], cwd=ROOT, env=env,
                       text=True, stdout=subprocess.PIPE, stderr=subprocess.STDOUT)
    (OUT / (name + ".txt")).write_text(p.stdout)
    assert p.returncode == 0, (name, p.returncode, p.stdout[-4000:])
    return p.stdout

def proof(name, source, mode, expected):
    binary = "/tmp/willow-m0-" + name
    run(name + "-build", ["target/debug/willow", "build", str(source),
        "--runtime-lib", "target/debug/libwillow_runtime.a", "-o", binary])
    trace = OUT / (name + ".jsonl")
    trace.unlink(missing_ok=True)
    env = dict(os.environ, WILLOW_WORKERS="1", WILLOW_GC_MARK_THREADS="1",
               WILLOW_GC_STRESS=mode, WILLOW_GC_VERIFY_REGIONS="1",
               WILLOW_GC_VERIFY_BARRIER="1", WILLOW_GC_TRACE_VERSION="3",
               WILLOW_GC_TRACE=str(trace))
    output = run(name, [binary], env)
    assert output == expected, (name, output)
    records = [json.loads(line) for line in trace.read_text().splitlines()]
    samples = [r for r in records if r["event"] == "gc_verification"]
    assert samples and all(r["barrier_variants_valid"] for r in samples)
    counts = [max(r["barrier_variants"][i] for r in samples) for i in range(13)]
    print(name, "stress=" + mode, "barriers=" + str(counts), flush=True)
    return counts

if __name__ == "__main__":
    run("compiler-build", ["cargo", "build", "--bin", "willow"])
    run("runtime-build", ["cargo", "build", "-p", "willow_runtime"])
    expected = "".join(f"{7*i}\n{i}\n" for i in range(1, 9))
    moving = proof("generated-moving", "example/gc_barrier_variants.wi",
                   "relocate,await,scheduler", expected)
    proof("generated-all", "example/gc_barrier_variants.wi", "all", expected)
    # >4KiB payload forces an old enum owner with a young child. This proves
    # the EnumPayload call instead of hoping concurrent marking overlaps it.
    count = 520
    source = OUT / "wide_enum.wi"
    source.write_text("class Node { pub value: i64; }\nenum Wide { Full(" +
        ", ".join(["Node"] * count) + ") }\nfn main() { let n = new Node(42); " +
        "let wide = Wide::Full(" + ", ".join(["n"] * count) + "); " +
        "gc_collect(); match wide { Wide::Full(" +
        ", ".join(["_"] * (count-1) + ["last"]) +
        ") => println(last.value) } }\n")
    wide = proof("generated-wide-enum", source, "minor", "42\n")
    # The first edge remembers the owner; its remaining 519 initializing
    # stores correctly use the compiler's remembered-owner fast exemption.
    assert wide[3] == 1, wide
    # Compiler destinations: Object/Array/Enum/Interface/Frame/Indirect/Static
    # and lock commits. Map and ContainerInternal are runtime-owned calls.
    for destination in [1, 2, 4, 5, 6, 7, 8, 10, 11, 12, 13]:
        assert max(moving[destination-1], wide[destination-1]) > 0, destination
    assert moving[2] > 0, "native MapValue call was not executed"
