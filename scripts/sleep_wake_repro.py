#!/usr/bin/env python3
"""Linux/GNU-linker regression: deterministic early wakes and a logical clock."""
import argparse
import os
from pathlib import Path
import subprocess
import tempfile


def run(command, **kwargs):
    result = subprocess.run(command, capture_output=True, text=True,
                            timeout=60, **kwargs)
    if result.returncode:
        raise RuntimeError(f"{command}: exit {result.returncode}\n{result.stdout}{result.stderr}")
    return result


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--compiler", type=Path, default=Path("target/debug/willowc"))
    args = parser.parse_args()
    compiler = args.compiler.resolve()
    runtime = compiler.parent / "libwillow_runtime.a"
    with tempfile.TemporaryDirectory(prefix="willow-sleep-wake-") as directory:
        directory = Path(directory)
        wrapper = directory / "wake.c"
        wrapper.write_text(r'''#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
static int64_t clock_ms = 100;
static int64_t last_remaining = 0;
static int parks = 0;
extern void __real_willow_sched_sleep(int64_t);
extern uint64_t willow_sched_current_task(void);
extern void willow_sched_wake(uint64_t);
int64_t __wrap_willow_monotonic_millis(void) { return clock_ms; }
void __wrap_willow_sched_sleep(int64_t remaining) {
    int64_t expected = getenv("FAST_FORWARD") && parks == 1 ? 1 : last_remaining - 1;
    if (remaining < 0 || (last_remaining > 1 && remaining != expected)) {
        fprintf(stderr, "deadline restarted or overflowed: %lld -> %lld\n",
                (long long)last_remaining, (long long)remaining);
        exit(2);
    }
    last_remaining = remaining;
    parks++;
    // Register a real timer, then inject a non-timer wake before its deadline.
    __real_willow_sched_sleep(remaining);
    if (getenv("FAST_FORWARD") && parks == 1) clock_ms = INT64_MAX - 1;
    else clock_ms++;
    willow_sched_wake(willow_sched_current_task());
}
__attribute__((destructor)) static void report(void) {
    fprintf(stderr, "parks=%d remaining=%lld\n", parks, (long long)last_remaining);
}
''')
        for ticks in [8, 1, 64, 0, -1, 9223372036854775807]:
            source = directory / "sleep.wi"
            source.write_text(f"""fn duration() -> i64 {{ println("operand"); return {ticks}; }}
async fn main() {{ await sleep(duration()); println("done"); }}
""")
            binary = directory / "sleep"
            run([str(compiler), "build", str(source), "-o", str(binary)],
                env=dict(os.environ, WILLOW_KEEP_OBJECT="1"))
            run(["cc", str(binary) + ".o", str(wrapper), str(runtime), "-o", str(binary),
                 "-Wl,--wrap=willow_sched_sleep", "-Wl,--wrap=willow_monotonic_millis",
                 "-Wl,--gc-sections", "-no-pie", "-lm", "-lpthread", "-ldl"])
            environment = dict(os.environ, WILLOW_WORKERS="1")
            if ticks == 9223372036854775807:
                environment["FAST_FORWARD"] = "1"
            result = run([str(binary)], env=environment)
            assert result.stdout == "operand\ndone\n", result.stdout
            parks = max(1, ticks + 1) if ticks != 9223372036854775807 else 2
            expected = f"parks={parks} remaining={int(ticks > 0)}\n"
            assert result.stderr == expected, (ticks, result.stderr, expected)
            print(f"ticks={ticks} {result.stderr.strip()}")


if __name__ == "__main__":
    main()
