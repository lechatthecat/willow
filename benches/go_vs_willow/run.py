#!/usr/bin/env python3
"""Build and run equivalent Willow/Go scheduler microbenchmarks."""

from __future__ import annotations

import argparse
import json
import os
from pathlib import Path
import queue
import shutil
import statistics
import subprocess
import threading
import time


ROOT = Path(__file__).resolve().parents[2]
SUITE = Path(__file__).resolve().parent
BUILD = SUITE / "build"
WILLOWC = ROOT / "target/release/willowc"
GO = Path(shutil.which("go") or "/usr/local/go/bin/go")


def build() -> None:
    BUILD.mkdir(exist_ok=True)
    if not WILLOWC.exists():
        subprocess.run(["cargo", "build", "--release", "--bin", "willowc"], cwd=ROOT, check=True)
    for source in (SUITE / "willow").glob("*.wi"):
        subprocess.run(
            [str(WILLOWC), "build", str(source), "-o", str(BUILD / source.stem), "--release"],
            cwd=ROOT,
            check=True,
        )
    go_env = os.environ.copy()
    go_env["GOCACHE"] = str(BUILD / "go-cache")
    subprocess.run(
        [str(GO), "build", "-o", str(BUILD / "go-bench"), str(SUITE / "go/main.go")],
        cwd=SUITE,
        env=go_env,
        check=True,
    )


def rss_bytes(pid: int) -> int:
    status = Path(f"/proc/{pid}/status").read_text()
    for line in status.splitlines():
        if line.startswith("VmRSS:"):
            return int(line.split()[1]) * 1024
    # Some short-lived processes expose a reduced status file in containers.
    resident_pages = int(Path(f"/proc/{pid}/statm").read_text().split()[1])
    return resident_pages * os.sysconf("SC_PAGE_SIZE")


def cpu_runtime_ns(pid: int) -> int:
    stat = Path(f"/proc/{pid}/stat").read_text()
    # Strip pid + parenthesized comm so spaces in the executable name are safe.
    fields = stat[stat.rfind(")") + 2 :].split()
    ticks = int(fields[11]) + int(fields[12])  # process utime + stime
    return ticks * 1_000_000_000 // os.sysconf("SC_CLK_TCK")


def command(language: str, benchmark: str, values: list[int]) -> list[str]:
    args = [str(value) for value in values]
    if language == "go":
        return [str(BUILD / "go-bench"), benchmark, *args]
    return [str(BUILD / benchmark), *args]


def one_run(language: str, benchmark: str, values: list[int], workers: int) -> dict[str, float | int | str]:
    env = os.environ.copy()
    env["GOMAXPROCS"] = str(workers)
    env["WILLOW_WORKERS"] = str(workers)
    process = subprocess.Popen(
        command(language, benchmark, values),
        cwd=ROOT,
        env=env,
        stdout=subprocess.PIPE,
        stderr=subprocess.PIPE,
        bufsize=0,
    )
    assert process.stdout is not None
    lines: queue.Queue[tuple[int, bytes] | None] = queue.Queue()

    def read_stdout() -> None:
        while True:
            line = process.stdout.readline()
            if not line:
                break
            lines.put((time.monotonic_ns(), line))
        lines.put(None)

    reader = threading.Thread(target=read_stdout, daemon=True)
    reader.start()
    markers: dict[str, int] = {}
    cpu: dict[str, int] = {}
    rss: dict[str, int] = {}
    output: list[str] = []
    deadline = time.monotonic() + 180
    while True:
        if time.monotonic() > deadline:
            process.kill()
            raise TimeoutError(f"timed out: {language} {benchmark} {values}")
        try:
            item = lines.get(timeout=0.1)
        except queue.Empty:
            continue
        if item is None:
            break
        timestamp, raw_line = item
        value = raw_line.decode().strip()
        output.append(value)
        if value in {"BASELINE", "SPAWN_START", "SPAWN_DONE", "READY", "START", "END"}:
            markers[value] = timestamp
            if value in {"SPAWN_START", "SPAWN_DONE", "START", "END"}:
                cpu[value] = cpu_runtime_ns(process.pid)
            if value in {"BASELINE", "READY"}:
                rss[value] = rss_bytes(process.pid)
    stderr = process.stderr.read().decode() if process.stderr is not None else ""
    return_code = process.wait()
    if return_code != 0:
        raise RuntimeError(f"{language} exited {return_code}: {stderr}\nstdout: {output}")

    result: dict[str, float | int | str] = {
        "language": language,
        "benchmark": benchmark,
        "count": values[0],
    }
    if benchmark == "idle_spawn":
        result["elapsed_ns"] = markers["SPAWN_DONE"] - markers["SPAWN_START"]
        result["cpu_ns"] = cpu["SPAWN_DONE"] - cpu["SPAWN_START"]
        result["rss_baseline_bytes"] = rss["BASELINE"]
        result["rss_ready_bytes"] = rss["READY"]
        result["rss_delta_bytes"] = rss["READY"] - rss["BASELINE"]
        result["rss_bytes_per_task"] = (rss["READY"] - rss["BASELINE"]) / values[0]
        result["runtime_allocated_bytes"] = int(output[output.index("READY") - 1])
    else:
        result["elapsed_ns"] = markers["END"] - markers["START"]
        result["cpu_ns"] = cpu["END"] - cpu["START"]
        operations = values[0] * (values[1] if benchmark in {"yield_switch", "gc_scheduler"} else 1)
        result["operations"] = operations
        result["operations_per_second"] = operations * 1_000_000_000 / result["elapsed_ns"]
        if benchmark == "gc_scheduler":
            metric_names = [name for name in output if name == "KEPT_OBJECTS" or name.startswith(("GO_", "WILLOW_"))]
            result["metrics"] = {name: int(output[output.index(name) + 1]) for name in metric_names}
    if len(values) > 1:
        result["rounds"] = values[1]
    return result


def main() -> None:
    parser = argparse.ArgumentParser()
    parser.add_argument(
        "benchmark",
        choices=["idle_spawn", "wake_fanout", "yield_switch", "ping_pong", "gc_scheduler"],
    )
    parser.add_argument("values", nargs="+", type=int)
    parser.add_argument("--trials", type=int, default=3)
    parser.add_argument("--workers", type=int, default=8)
    parser.add_argument("--no-build", action="store_true")
    args = parser.parse_args()
    if not args.no_build:
        build()
    results = [
        one_run(language, args.benchmark, args.values, args.workers)
        for language in ("willow", "go")
        for _ in range(args.trials)
    ]
    for result in results:
        print(json.dumps(result, sort_keys=True))
    for language in ("willow", "go"):
        timings = [int(r["elapsed_ns"]) for r in results if r["language"] == language]
        print(f"{language}: median_elapsed_ns={int(statistics.median(timings))}")


if __name__ == "__main__":
    main()
