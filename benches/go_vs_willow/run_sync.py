#!/usr/bin/env python3
"""Build and measure equivalent synchronous Willow/Go workloads."""

from __future__ import annotations

import argparse
import json
import os
from pathlib import Path
import shutil
import statistics
import subprocess
import tempfile
import time


ROOT = Path(__file__).resolve().parents[2]
SUITE = Path(__file__).resolve().parent
SYNC = SUITE / "sync"
BUILD = SUITE / "build" / "sync"
WILLOWC = ROOT / "target/release/willowc"
DEFAULT_GO = Path("/usr/local/go/bin/go")
GO = Path(os.environ.get("GO", "")) if os.environ.get("GO") else DEFAULT_GO
if not GO.exists():
    GO = Path(shutil.which("go") or GO)
GNU_TIME = Path("/usr/bin/time")

CASES = (
    "leibniz_pow",
    "leibniz_reduced",
    "fibonacci",
    "array_sum",
    "linked_list",
)

EXPECTED = {
    "fibonacci": "102334155",
    "array_sum": "2497500000",
    "linked_list": "499999500000",
}


def build() -> None:
    BUILD.mkdir(parents=True, exist_ok=True)
    subprocess.run(
        ["cargo", "build", "--release", "--bin", "willowc"],
        cwd=ROOT,
        check=True,
    )
    for case in CASES:
        subprocess.run(
            [
                str(WILLOWC),
                "build",
                str(SYNC / "willow" / f"{case}.wi"),
                "-o",
                str(BUILD / f"willow-{case}"),
                "--release",
            ],
            cwd=ROOT,
            check=True,
        )
    go_env = os.environ.copy()
    go_env["GOCACHE"] = str(BUILD / "go-cache")
    subprocess.run(
        [
            str(GO),
            "build",
            "-trimpath",
            "-o",
            str(BUILD / "go-bench"),
            str(SYNC / "go" / "main.go"),
        ],
        cwd=SYNC,
        env=go_env,
        check=True,
    )


def command(language: str, case: str) -> list[str]:
    if language == "go":
        return [str(BUILD / "go-bench"), case]
    return [str(BUILD / f"willow-{case}")]


def validate(case: str, stdout: str) -> str:
    value = stdout.strip()
    if case.startswith("leibniz_"):
        try:
            result = float(value)
        except ValueError as error:
            raise RuntimeError(f"{case} printed a non-float result: {value!r}") from error
        if abs(result - 3.141592663589326) > 1e-12:
            raise RuntimeError(f"{case} printed an unexpected result: {value!r}")
    elif value != EXPECTED[case]:
        raise RuntimeError(f"{case} printed {value!r}, expected {EXPECTED[case]!r}")
    return value


def one_run(language: str, case: str, workers: int) -> dict[str, int | float | str]:
    env = os.environ.copy()
    env["GOMAXPROCS"] = str(workers)
    env["WILLOW_WORKERS"] = str(workers)
    with tempfile.NamedTemporaryFile(prefix="willow-sync-time-", delete=False) as timing:
        timing_path = Path(timing.name)
    try:
        full_command = [
            str(GNU_TIME),
            "-f",
            "METRICS\\t%U\\t%S\\t%M",
            "-o",
            str(timing_path),
            *command(language, case),
        ]
        start_ns = time.monotonic_ns()
        completed = subprocess.run(
            full_command,
            cwd=ROOT,
            env=env,
            text=True,
            stdout=subprocess.PIPE,
            stderr=subprocess.PIPE,
            timeout=300,
        )
        elapsed_ns = time.monotonic_ns() - start_ns
        if completed.returncode != 0:
            raise RuntimeError(
                f"{language} {case} exited {completed.returncode}: {completed.stderr}"
            )
        output = validate(case, completed.stdout)
        metrics_line = next(
            line for line in timing_path.read_text().splitlines() if line.startswith("METRICS\t")
        )
        _, user_s, system_s, max_rss_kib = metrics_line.split("\t")
        return {
            "language": language,
            "benchmark": case,
            "elapsed_ns": elapsed_ns,
            "user_s": float(user_s),
            "system_s": float(system_s),
            "max_rss_kib": int(max_rss_kib),
            "output": output,
        }
    finally:
        timing_path.unlink(missing_ok=True)


def median_summary(results: list[dict[str, int | float | str]]) -> None:
    print("\nbenchmark,language,median_wall_ms,median_user_s,median_sys_s,median_max_rss_kib")
    for case in CASES:
        for language in ("willow", "go"):
            selected = [
                result
                for result in results
                if result["benchmark"] == case and result["language"] == language
            ]
            if not selected:
                continue
            print(
                f"{case},{language},"
                f"{statistics.median(int(result['elapsed_ns']) for result in selected) / 1_000_000:.3f},"
                f"{statistics.median(float(result['user_s']) for result in selected):.3f},"
                f"{statistics.median(float(result['system_s']) for result in selected):.3f},"
                f"{int(statistics.median(int(result['max_rss_kib']) for result in selected))}"
            )


def main() -> None:
    parser = argparse.ArgumentParser()
    parser.add_argument("cases", nargs="*", choices=CASES, default=list(CASES))
    parser.add_argument("--trials", type=int, default=5)
    parser.add_argument("--workers", type=int, default=8)
    parser.add_argument("--no-build", action="store_true")
    args = parser.parse_args()
    if args.trials < 1:
        parser.error("--trials must be at least 1")
    if args.workers < 1:
        parser.error("--workers must be at least 1")
    selected_cases = args.cases or list(CASES)
    if not args.no_build:
        build()

    results: list[dict[str, int | float | str]] = []
    for case in selected_cases:
        for trial in range(args.trials):
            # Alternate launch order so one language does not consistently get
            # the first (colder) run of every pair.
            languages = ("willow", "go") if trial % 2 == 0 else ("go", "willow")
            for language in languages:
                result = one_run(language, case, args.workers)
                result["trial"] = trial + 1
                results.append(result)
                print(json.dumps(result, sort_keys=True), flush=True)
    median_summary(results)


if __name__ == "__main__":
    main()
