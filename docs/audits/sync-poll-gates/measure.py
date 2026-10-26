#!/usr/bin/env python3
"""Linux/GNU linker-wrap gate lookup counts; no runtime source modifications."""
import json
import os
from pathlib import Path
import subprocess
import tempfile
import time

ROOT = Path(__file__).resolve().parents[3]
COMPILER = Path(os.environ.get("WILLOWC", ROOT / "target/release/willow"))
WRAPPER = r"""
#include <stdint.h>
#include <stdio.h>
static uint64_t counter, stop;
extern int32_t *__real_willow_sync_poll_counter(void);
extern const uint8_t *__real_willow_gc_stop_flag(void);
int32_t *__wrap_willow_sync_poll_counter(void) {
    counter++; return __real_willow_sync_poll_counter();
}
const uint8_t *__wrap_willow_gc_stop_flag(void) {
    stop++; return __real_willow_gc_stop_flag();
}
__attribute__((destructor)) static void report(void) {
    fprintf(stderr, "GATES %lu %lu\n", counter, stop);
}
"""

def main():
    with tempfile.TemporaryDirectory(prefix="sync-poll-gates-") as temporary:
        work = Path(temporary)
        (work / "wrap.c").write_text(WRAPPER)
        subprocess.run(["/usr/bin/cc", "-O2", "-c", str(work / "wrap.c"), "-o", str(work / "wrap.o")], check=True)
        (work / "cc").write_text('#!/bin/sh\nexec /usr/bin/cc "$@" ' + str(work / "wrap.o") + ' -Wl,--wrap=willow_sync_poll_counter -Wl,--wrap=willow_gc_stop_flag\n')
        (work / "cc").chmod(0o755)
        env = dict(os.environ, PATH=str(work) + os.pathsep + os.environ["PATH"])
        sources = [(name, ROOT / "benches/go_vs_willow/sync/willow" / (name + ".wi")) for name in ["fibonacci", "closure_call", "virtual_dispatch"]]
        for depth in [8, 12, 16, 20]:
            source = work / ("recursive_" + str(depth) + ".wi")
            source.write_text("fn left(n: i64) -> i64 { if n < 1 { return 1; } return right(n-1) + right(n-1); }\nfn right(n: i64) -> i64 { if n < 1 { return 1; } return left(n-1) + left(n-1); }\nfn main() { println(left(" + str(depth) + ")); }\n")
            sources.append((source.stem, source))
        for name, source in sources:
            binary = work / name
            subprocess.run([str(COMPILER), "build", str(source), "--release", "-o", str(binary)], cwd=ROOT, env=env, check=True, stdout=subprocess.DEVNULL)
            start = time.monotonic()
            result = subprocess.run([str(binary)], check=True, capture_output=True, text=True)
            print(json.dumps(dict(workload=name, seconds=time.monotonic()-start, stdout=result.stdout.strip(), counts=result.stderr.strip())), flush=True)

if __name__ == "__main__":
    main()
