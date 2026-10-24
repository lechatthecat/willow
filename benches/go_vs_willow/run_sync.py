#!/usr/bin/env python3
"""Build and measure equivalent synchronous Willow/Go workloads."""

from __future__ import annotations

import argparse
import json
import math
import os
from pathlib import Path
import shutil
import signal
import statistics
import subprocess
import tempfile
import threading
import time

from javatool import compile_java, java_command


ROOT = Path(__file__).resolve().parents[2]
SUITE = Path(__file__).resolve().parent
SYNC = SUITE / "sync"
BUILD = SUITE / "build" / "sync"
WILLOWC = ROOT / "target/release/willow"
DEFAULT_GO = Path("/usr/local/go/bin/go")
GO = Path(os.environ.get("GO", "")) if os.environ.get("GO") else DEFAULT_GO
if not GO.exists():
    GO = Path(shutil.which("go") or GO)
GNU_TIME = Path("/usr/bin/time")
LANGUAGES = ("willow", "go", "java")
DEFAULT_LANGUAGES = ("willow", "go")

CASES = (
    "general_pow",
    "leibniz_pow",
    "leibniz_pow_nonunit",
    "leibniz_reduced",
    "fibonacci",
    "array_sum",
    "linked_list",
    "array_build_only",
    "array_read_sum_only",
    "array_write_only",
    "array_grow_only",
    "object_churn",
    "array_literal_churn",
    "map_lookup_insert",
    "virtual_dispatch",
    "closure_call",
    "string_build",
)

EXPECTED = {
    "fibonacci": "102334155",
    "array_sum": "2497500000",
    "linked_list": "499999500000",
    "array_build_only": "5000000",
    "array_read_sum_only": "2497500000",
    "array_write_only": "2502500000",
    "array_grow_only": "5000000",
    "object_churn": "4994476800",
    "array_literal_churn": "4994476800",
    "map_lookup_insert": "500000500000",
    "closure_call": "10557340000",
    "string_build": "31301585",
    "virtual_dispatch": "1180000000",
}


# Cases that print START/END markers and are timed between them; the process
# wall time is still recorded for them. The churn cases additionally print
# metric name/value pairs (GO_*/JAVA_*/WILLOW_*) after the result line.
PHASE_CASES = frozenset(
    (
        *(case for case in CASES if case.endswith("_only")),
        "object_churn",
        "array_literal_churn",
        "map_lookup_insert",
        "virtual_dispatch",
        "closure_call",
        "string_build",
    )
)


def build(languages: tuple[str, ...] = DEFAULT_LANGUAGES) -> None:
    BUILD.mkdir(parents=True, exist_ok=True)
    if "willow" in languages:
        subprocess.run(
            ["cargo", "build", "--release", "--bin", "willow"],
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
    if "go" in languages:
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
    if "java" in languages:
        compile_java(SYNC / "java" / "Bench.java", BUILD / "java")


def command(language: str, case: str, workers: int) -> list[str]:
    if language == "go":
        return [str(BUILD / "go-bench"), case]
    if language == "java":
        return java_command(BUILD / "java", workers, [case])
    return [str(BUILD / f"willow-{case}")]


def validate(case: str, stdout: str) -> str:
    value = stdout.strip()
    if case == "general_pow":
        # 70-digit Decimal oracle: sum((9/8 + j/1024)**(3/8), j=0..1023)
        # times 20,000. Allow sequential binary64 summation error, not NaN/inf.
        result = float(value)
        if not math.isfinite(result) or not math.isclose(
            result, 24474119.05663396, rel_tol=1e-8, abs_tol=0.0
        ):
            raise RuntimeError(f"{case} printed an unexpected result: {value!r}")
    elif case == "leibniz_pow_nonunit":
        # Quad-precision oracle with the binary64 base -1.0000001. Integral
        # pow kernels differ (binary exponentiation vs fdlibm), and the last
        # term dominates the alternating sum, so allow pow rounding drift.
        result = float(value)
        if not math.isfinite(result) or not math.isclose(
            result, 3.141812861068075, rel_tol=1e-9, abs_tol=0.0
        ):
            raise RuntimeError(f"{case} printed an unexpected result: {value!r}")
    elif case.startswith("leibniz_"):
        try:
            result = float(value)
        except ValueError as error:
            raise RuntimeError(f"{case} printed a non-float result: {value!r}") from error
        if abs(result - 3.141592663589326) > 1e-12:
            raise RuntimeError(f"{case} printed an unexpected result: {value!r}")
    elif value != EXPECTED[case]:
        raise RuntimeError(f"{case} printed {value!r}, expected {EXPECTED[case]!r}")
    return value


def parse_metrics(case: str, lines: list[str]) -> dict[str, int]:
    """Optional trailing `NAME` / integer pairs after the result line."""
    if len(lines) % 2 != 0:
        raise RuntimeError(f"{case} printed an odd number of metric lines: {lines!r}")
    metrics: dict[str, int] = {}
    for name, value in zip(lines[::2], lines[1::2]):
        if not name.startswith(("GO_", "JAVA_", "WILLOW_")):
            raise RuntimeError(f"{case} printed an unexpected metric name: {name!r}")
        try:
            metrics[name] = int(value)
        except ValueError as error:
            raise RuntimeError(f"{case} printed a non-integer metric {name}: {value!r}") from error
    return metrics


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
            *command(language, case, workers),
        ]
        start_ns = time.monotonic_ns()
        # A dedicated unbuffered reader timestamps markers as they arrive. Stderr
        # goes to a file so a full stderr pipe cannot deadlock the child.
        lines: list[tuple[int, str]] = []
        with tempfile.TemporaryFile() as errors:
            with subprocess.Popen(
                full_command, cwd=ROOT, env=env, stdout=subprocess.PIPE,
                stderr=errors, bufsize=0, start_new_session=True,
            ) as process:
                assert process.stdout is not None

                def read_stdout() -> None:
                    for raw in iter(process.stdout.readline, b""):
                        lines.append((time.monotonic_ns(), raw.decode().strip()))

                reader = threading.Thread(target=read_stdout)
                reader.start()
                try:
                    reader.join(timeout=300)
                    if reader.is_alive():
                        raise subprocess.TimeoutExpired(full_command, 300)
                    process.wait(timeout=max(0.001, 300 - (time.monotonic_ns() - start_ns) / 1e9))
                except subprocess.TimeoutExpired:
                    os.killpg(process.pid, signal.SIGKILL)
                    process.wait()
                    raise
                finally:
                    reader.join()
                process_elapsed_ns = time.monotonic_ns() - start_ns
                if process.returncode != 0:
                    errors.seek(0)
                    raise RuntimeError(
                        f"{language} {case} exited {process.returncode}: "
                        f"{errors.read().decode()}"
                    )
        elapsed_ns = process_elapsed_ns
        metrics: dict[str, int] = {}
        if case in PHASE_CASES:
            if len(lines) < 3 or [line for _, line in lines[:2]] != ["START", "END"]:
                raise RuntimeError(f"{case} printed invalid phase markers: {lines!r}")
            elapsed_ns = lines[1][0] - lines[0][0]
            output = validate(case, lines[2][1])
            metrics = parse_metrics(case, [line for _, line in lines[3:]])
        else:
            output = validate(case, "\n".join(line for _, line in lines))
        metrics_line = next(
            line for line in timing_path.read_text().splitlines() if line.startswith("METRICS\t")
        )
        _, user_s, system_s, max_rss_kib = metrics_line.split("\t")
        return {
            "language": language,
            "benchmark": case,
            "elapsed_ns": elapsed_ns,
            "process_elapsed_ns": process_elapsed_ns,
            "wall_scope": "phase" if case in PHASE_CASES else "process",
            "cpu_rss_scope": "process",
            "user_s": float(user_s),
            "system_s": float(system_s),
            "max_rss_kib": int(max_rss_kib),
            "output": output,
            **({"metrics": metrics} if metrics else {}),
        }
    finally:
        timing_path.unlink(missing_ok=True)


def median_summary(
    results: list[dict[str, int | float | str]], languages: tuple[str, ...]
) -> None:
    print("\nbenchmark,language,median_wall_ms,median_user_s,median_sys_s,median_max_rss_kib")
    for case in CASES:
        for language in languages:
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
    parser.add_argument(
        "--languages",
        default=",".join(DEFAULT_LANGUAGES),
        help=f"comma-separated subset of {','.join(LANGUAGES)} (default: %(default)s)",
    )
    args = parser.parse_args()
    if args.trials < 1:
        parser.error("--trials must be at least 1")
    if args.workers < 1:
        parser.error("--workers must be at least 1")
    languages = tuple(dict.fromkeys(part.strip() for part in args.languages.split(",") if part.strip()))
    unknown = [language for language in languages if language not in LANGUAGES]
    if unknown or not languages:
        parser.error(f"--languages must name one or more of {','.join(LANGUAGES)}")
    selected_cases = args.cases or list(CASES)
    if not args.no_build:
        build(languages)

    results: list[dict[str, int | float | str]] = []
    for case in selected_cases:
        for trial in range(args.trials):
            # Rotate launch order so one language does not consistently get
            # the first (colder) run of every group.
            shift = trial % len(languages)
            for language in languages[shift:] + languages[:shift]:
                result = one_run(language, case, args.workers)
                result["trial"] = trial + 1
                results.append(result)
                print(json.dumps(result, sort_keys=True), flush=True)
    median_summary(results, languages)


if __name__ == "__main__":
    main()
