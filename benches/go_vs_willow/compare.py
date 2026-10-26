#!/usr/bin/env python3
"""Run the full Willow-vs-Go-vs-Java comparison and write a dated Markdown report.

`run` executes both existing harnesses (`run_sync.py` and `run.py`) with the
standard workloads for the selected languages, stores every trial record in
`MEASUREMENT_<label>.jsonl`, and renders `MEASUREMENT_<label>.md`. `report`
renders a report to stdout from an existing JSONL file, optionally with delta
columns against a baseline JSONL. Only an explicit --out writes a report file.
"""

from __future__ import annotations

import argparse
import datetime as dt
import json
import os
from pathlib import Path
import platform
import shutil
import statistics
import subprocess
import sys

from javatool import java_version


ROOT = Path(__file__).resolve().parents[2]
SUITE = Path(__file__).resolve().parent
RUN_SYNC = SUITE / "run_sync.py"
RUN_SCHED = SUITE / "run.py"
GO = Path(shutil.which("go") or "/usr/local/go/bin/go")
LANGUAGES = ("willow", "go", "java")
LANGUAGE_NAMES = {"willow": "Willow", "go": "Go", "java": "Java"}

# Scheduler cases with the standard arguments used by every checked-in report.
SCHEDULER_CASES: tuple[tuple[str, list[int]], ...] = (
    ("idle_spawn", [100_000]),
    ("wake_fanout", [100_000]),
    ("yield_switch", [100_000, 100]),
    ("ping_pong", [1_000_000]),
    ("gc_scheduler", [10_000, 100]),
    ("channel_select_fan_in", [1_000_000]),
    ("spawn_join_tree", [10, 5]),
)

SYNC_CASES = (
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

WORK = {
    "general_pow": "20,480,000 powers; 1,024 bases in [1.125, 2.125), exponent 0.375",
    "idle_spawn": "100,000 tasks parked on channels",
    "wake_fanout": "100,000 tasks, one send + one completion each",
    "yield_switch": "100,000 tasks × 100 yields",
    "ping_pong": "1,000,000 channel round trips",
    "gc_scheduler": "10,000 tasks × 100 alloc/yield rounds",
    "channel_select_fan_in": "1,000,000 select receives from 4 producers + cancel",
    "spawn_join_tree": "fanout 10 × depth 5: 111,110 spawn+join",
    "leibniz_pow": "100,000,001 terms, sign via pow (base -1: pow shortcut)",
    "leibniz_pow_nonunit": "100,000,001 terms, pow with base -1.0000001",
    "leibniz_reduced": "100,000,001 terms, sign flip",
    "fibonacci": "naive fib(40)",
    "array_sum": "append + sum 5,000,000 i64",
    "linked_list": "build + traverse 1,000,000 nodes",
    "array_build_only": "push 5,000,000 values (phase)",
    "array_read_sum_only": "index-read 5,000,000 values (phase)",
    "array_write_only": "index-write 5,000,000 values (phase)",
    "array_grow_only": "1,000 arrays × 5,000 pushes (phase)",
    "object_churn": "10,000,000 short-lived objects, 1,024 live (phase)",
    "array_literal_churn": "10,000,000 three-element array literals, 1,024 live (phase)",
    "map_lookup_insert": "1,000,000 inserts + 2,000,000 gets (phase)",
    "virtual_dispatch": "20,000 × 1,000 interface calls, 4 classes (phase)",
    "closure_call": "20,000 × 1,024 indirect calls to capturing closures (phase)",
    "string_build": "2,000,000 strings: 2 int→decimal + 3 concats each (phase)",
}

# Cases that print per-runtime GC counters after the result line.
GC_CASES = (
    ("gc_scheduler", "GC plus scheduler"),
    ("object_churn", "GC under allocation churn"),
    ("array_literal_churn", "GC under array-literal churn"),
)

ORDER = [name for name, _ in SCHEDULER_CASES] + list(SYNC_CASES)
Row = dict[str, object]


def sh(args: list[str], **kwargs) -> str:
    completed = subprocess.run(args, text=True, capture_output=True, **kwargs)
    if completed.returncode != 0:
        # Surface the child's diagnostics; run.py reports the failing language,
        # exit code and stderr only on its own stderr.
        sys.stderr.write(completed.stdout)
        sys.stderr.write(completed.stderr)
        raise subprocess.CalledProcessError(completed.returncode, args, completed.stdout, completed.stderr)
    return completed.stdout


def json_lines(text: str) -> list[Row]:
    return [json.loads(line) for line in text.splitlines() if line.startswith("{")]


def split_environment(records: list[Row]) -> tuple[dict[str, str] | None, list[Row]]:
    """Separate the measurement-time environment record that `run` writes first."""
    env = next((r for r in records if r.get("record") == "environment"), None)
    rows = [r for r in records if r.get("record") != "environment"]
    return (None if env is None else {k: str(v) for k, v in env.items() if k != "record"}), rows


def run_suites(args: argparse.Namespace) -> list[Row]:
    rows: list[Row] = []
    if not args.no_build and "willow" in args.languages:
        subprocess.run(["cargo", "build", "--release", "--bin", "willow"], cwd=ROOT, check=True)
    common = [
        "--trials", str(args.trials), "--workers", str(args.workers), "--languages", ",".join(args.languages),
    ]
    if args.suite in ("all", "sync"):
        cmd = [sys.executable, str(RUN_SYNC), *common]
        if args.no_build:
            cmd.append("--no-build")
        print("+", " ".join(cmd), file=sys.stderr, flush=True)
        rows.extend(json_lines(sh(cmd, cwd=ROOT)))
    if args.suite in ("all", "scheduler"):
        built = args.no_build
        for name, values in SCHEDULER_CASES:
            cmd = [sys.executable, str(RUN_SCHED), name, *map(str, values), *common]
            if built:
                cmd.append("--no-build")
            built = True
            print("+", " ".join(cmd), file=sys.stderr, flush=True)
            rows.extend(json_lines(sh(cmd, cwd=ROOT)))
    return rows


def median(rows: list[Row], key: str) -> float | None:
    values = [float(r[key]) for r in rows if key in r and r[key] is not None]
    return statistics.median(values) if values else None


def metric_median(rows: list[Row], key: str) -> float | None:
    values = [float(r["metrics"][key]) for r in rows if isinstance(r.get("metrics"), dict) and key in r["metrics"]]
    return statistics.median(values) if values else None


def group(rows: list[Row]) -> dict[tuple[str, str], list[Row]]:
    out: dict[tuple[str, str], list[Row]] = {}
    for r in rows:
        out.setdefault((str(r["benchmark"]), str(r["language"])), []).append(r)
    return out


def fmt(value: float | None, digits: int = 3) -> str:
    return "n/a" if value is None else f"{value:,.{digits}f}"


def ratio_cell(w: float | None, other: float | None, other_name: str) -> str:
    """Willow / other as a ratio plus the faster side, e.g. `0.75× (Willow 1.33x)`."""
    if w is None or other is None or other == 0 or w == 0:
        return "n/a"
    r = w / other
    faster = f"Willow {other / w:.2f}x" if r < 1 else f"{other_name} {r:.2f}x"
    return f"{r:.2f}× ({faster})"


def delta_cell(new: float | None, old: float | None) -> str:
    if new is None or old is None or old == 0:
        return "n/a"
    return f"{(new - old) / old * 100:+.1f}%"


def environment() -> dict[str, str]:
    def git(*a: str) -> str:
        try:
            return sh(["git", *a], cwd=ROOT).strip()
        except (subprocess.CalledProcessError, FileNotFoundError):
            return "unknown"

    cpu = "unknown"
    try:
        for line in Path("/proc/cpuinfo").read_text().splitlines():
            if line.startswith("model name"):
                cpu = line.split(":", 1)[1].strip()
                break
    except OSError:
        cpu = platform.processor() or "unknown"
    try:
        go_version = sh([str(GO), "version"]).strip()
    except (subprocess.CalledProcessError, FileNotFoundError):
        go_version = "unknown"
    return {
        "commit": git("rev-parse", "HEAD"),
        "dirty": "dirty" if git("status", "--porcelain", "--untracked-files=no") else "clean",
        "cpu": cpu,
        "cores": str(os.cpu_count()),
        "os": f"{platform.system()} {platform.release()}, {platform.machine()}",
        "go": go_version,
        "java": java_version(),
    }


def languages_in(rows: list[Row]) -> list[str]:
    seen = {str(r["language"]) for r in rows}
    return [language for language in LANGUAGES if language in seen]


def render(
    rows: list[Row],
    label: str,
    baseline: list[Row] | None,
    workers: int,
    jsonl_name: str,
    env: dict[str, str] | None,
) -> str:
    g = group(rows)
    b = group(baseline) if baseline else {}
    langs = languages_in(rows)
    names = [LANGUAGE_NAMES[l] for l in langs]
    others = [l for l in langs if l != "willow"]
    present = [c for c in ORDER if any((c, l) in g for l in langs)]
    trials = sorted({len(v) for v in g.values()})
    # JSONL files written before the environment record existed fall back to
    # the current checkout, labelled as such.
    when = "at measurement time"
    if env is None:
        env, when = environment(), "at report time; the JSONL has no environment record"
    out: list[str] = []

    def med(case: str, lang: str, key: str) -> float | None:
        return median(g.get((case, lang), []), key)

    def bmed(case: str, lang: str, key: str) -> float | None:
        return median(b.get((case, lang), []), key)

    def ms(value: float | None, digits: int = 3) -> str:
        return fmt(None if value is None else value / 1e6, digits)

    out.append(f"# {' vs '.join(names)} measurement — {label}\n")
    out.append(f"- Commit: `{env['commit']}` (working tree {env['dirty']} {when}).")
    out.append(f"- {env['cpu']}, {env['cores']} logical CPUs; {env['os']}.")
    toolchains = ["release Willow compiler and benchmark binaries" if "willow" in langs else None,
                  env["go"] if "go" in langs else None,
                  f"{env['java']} (default JVM flags plus `-XX:ActiveProcessorCount` and virtual-thread parallelism = workers)"
                  if "java" in langs else None]
    out.append("- " + "; ".join(t for t in toolchains if t) + ".")
    out.append(
        f"- Trials per language and case: {'/'.join(map(str, trials))}; medians below. "
        f"`WILLOW_WORKERS={workers}`, `GOMAXPROCS={workers}`, JVM processor count {workers}."
    )
    out.append(
        "- Scheduler timing measures marked phases and runs all trials of one language before the next. "
        "Synchronous timing is whole-process (including JVM startup and JIT warm-up for Java) except "
        "`*_only` cases, which time START/END markers; synchronous launch order rotates and every output is validated."
    )
    out.append("- Build time excluded. CPU affinity/frequency not pinned; background services active.")
    out.append(f"- Raw trial records: [`{jsonl_name}`]({jsonl_name}).\n")

    out.append("## Wall time\n")
    out.append("Ratios are Willow / other; greater than 1 means the other language is faster.\n")
    header = "| Case | Work | " + " | ".join(f"{n} (ms)" for n in names) + " |"
    sep = "|---|---|" + "---:|" * len(langs)
    for o in others:
        header += f" Willow / {LANGUAGE_NAMES[o]} |"
        sep += "---|"
    if b:
        for n in names:
            header += f" {n} Δ vs baseline |"
            sep += "---:|"
    out += [header, sep]
    for c in present:
        walls = {l: med(c, l, "elapsed_ns") for l in langs}
        line = f"| {c} | {WORK.get(c, '')} | " + " | ".join(ms(walls[l]) for l in langs) + " |"
        for o in others:
            line += f" {ratio_cell(walls.get('willow'), walls[o], LANGUAGE_NAMES[o])} |"
        if b:
            for l in langs:
                line += f" {delta_cell(walls[l], bmed(c, l, 'elapsed_ns'))} |"
        out.append(line)
    out.append("")

    sched = [c for c in present if any("cpu_ns" in r for l in langs for r in g.get((c, l), []))]
    if sched:
        out.append("## Scheduler phase CPU and throughput\n")
        out.append(
            "CPU is whole-process `utime + stime` at 10 ms resolution. Willow's driver thread "
            "busy-spins during parallel phases, so `Willow CPU − wall` "
            "approximates worker CPU for comparison with the others.\n"
        )
        header = "| Case | " + " | ".join(f"{n} wall / CPU (ms)" for n in names) + " |"
        sep = "|---|" + "---:|" * len(langs)
        if "willow" in langs:
            header += " Willow CPU − wall (ms) |"
            sep += "---:|"
        header += " " + " | ".join(f"{n} ops/s" for n in names) + " |"
        sep += "---:|" * len(langs)
        out += [header, sep]
        for c in sched:
            cells = [f"{ms(med(c, l, 'elapsed_ns'))} / {ms(med(c, l, 'cpu_ns'), 0)}" for l in langs]
            line = f"| {c} | " + " | ".join(cells) + " |"
            if "willow" in langs:
                ww, wc = med(c, "willow", "elapsed_ns"), med(c, "willow", "cpu_ns")
                line += f" {fmt(None if ww is None or wc is None else (wc - ww) / 1e6, 0)} |"
            line += " " + " | ".join(fmt(med(c, l, "operations_per_second"), 0) for l in langs) + " |"
            out.append(line)
        out.append("")

    if "idle_spawn" in present:
        out.append("## Task footprint (idle_spawn)\n")
        out.append("Willow reports occupied GC heap bytes; Go `TotalAlloc` and JVM thread-allocated bytes are cumulative allocation counters. These are separate metrics.\n")
        out.append("| Metric | " + " | ".join(names) + " |")
        out.append("|---|" + "---:|" * len(langs))
        for label_, key, scale, digits in (
            ("Spawn wall (ms)", "elapsed_ns", 1e6, 3),
            ("RSS delta (MiB)", "rss_delta_bytes", 1 << 20, 2),
            ("RSS after park (MiB)", "rss_ready_bytes", 1 << 20, 2),
            ("RSS bytes / task", "rss_bytes_per_task", 1, 2),
            ("Cumulative allocated bytes (Go/JVM)", "runtime_allocated_bytes", 1, 0),
            ("Occupied GC heap bytes (Willow)", "willow_live_bytes", 1, 0),
        ):
            cells = []
            for l in langs:
                if key == "willow_live_bytes":
                    v = med("idle_spawn", l, "runtime_allocated_bytes") if l == "willow" else None
                elif key == "runtime_allocated_bytes" and l == "willow":
                    v = None
                else:
                    v = med("idle_spawn", l, key)
                cells.append(fmt(None if v is None else v / scale, digits))
            out.append(f"| {label_} | " + " | ".join(cells) + " |")
        out.append("")

    for gc_case, title in GC_CASES:
        if gc_case not in present:
            continue
        wr = g.get((gc_case, "willow"), [])
        gr = g.get((gc_case, "go"), [])
        jr = g.get((gc_case, "java"), [])
        out.append(f"## {title} ({gc_case})\n")
        out.append(
            "Counters are not equivalent across runtimes: Willow exposes no pause time, Go reports "
            "stop-the-world pauses, and the JVM reports cumulative collector time at 1 ms resolution.\n"
        )
        out.append("| Metric | " + " | ".join(names) + " |")
        out.append("|---|" + "---:|" * len(langs))

        def row(label_: str, per_lang: dict[str, str]) -> None:
            out.append(f"| {label_} | " + " | ".join(per_lang.get(l, "n/a") for l in langs) + " |")

        row("GC cycles", {
            "willow": f"{fmt(metric_median(wr, 'WILLOW_MINOR_COLLECTIONS'), 0)} minor / {fmt(metric_median(wr, 'WILLOW_MAJOR_COLLECTIONS'), 0)} major",
            "go": fmt(metric_median(gr, "GO_GC_CYCLES"), 0),
            "java": fmt(metric_median(jr, "JAVA_GC_CYCLES"), 0),
        })
        row("Promoted / moved objects", {
            "willow": f"{fmt(metric_median(wr, 'WILLOW_PROMOTED_OBJECTS'), 0)} / {fmt(metric_median(wr, 'WILLOW_MOVED_OBJECTS'), 0)}",
        })
        row("TLAB fast allocations / refills", {
            "willow": f"{fmt(metric_median(wr, 'WILLOW_TLAB_FAST_ALLOCATIONS'), 0)} / {fmt(metric_median(wr, 'WILLOW_TLAB_REFILLS'), 0)}",
        })
        row("Allocated bytes during phase", {
            "willow": fmt(metric_median(wr, "WILLOW_TOTAL_ALLOC_BYTES"), 0),
            "go": fmt(metric_median(gr, "GO_TOTAL_ALLOC_BYTES"), 0),
            "java": fmt(metric_median(jr, "JAVA_TOTAL_ALLOC_BYTES"), 0),
        })
        row("Heap in use after phase (bytes)", {
            "willow": fmt(metric_median(wr, "WILLOW_LIVE_BYTES"), 0),
            "go": fmt(metric_median(gr, "GO_HEAP_ALLOC_BYTES"), 0),
            "java": fmt(metric_median(jr, "JAVA_HEAP_USED_BYTES"), 0),
        })
        row("GC pause / collector time total (µs)", {
            "go": fmt(_us(metric_median(gr, "GO_GC_PAUSE_TOTAL_NS"))),
            "java": fmt(_us(metric_median(jr, "JAVA_GC_TIME_TOTAL_NS")), 0),
        })
        row("Max GC pause (µs)", {"go": fmt(_us(metric_median(gr, "GO_GC_MAX_PAUSE_NS")))})
        out.append("")

    sync = [c for c in present if c in SYNC_CASES]
    if sync:
        out.append("## Synchronous CPU time and peak RSS\n")
        out.append("User/system are whole-process seconds from GNU `time`; RSS is peak RSS.\n")
        out.append("| Case | " + " | ".join(f"{n} user / sys (s)" for n in names) + " | " + " | ".join(f"{n} RSS (MiB)" for n in names) + " |")
        out.append("|---|" + "---:|" * (2 * len(langs)))
        for c in sync:
            cpu_cells = [f"{fmt(med(c, l, 'user_s'), 2)} / {fmt(med(c, l, 'system_s'), 2)}" for l in langs]
            rss_cells = [fmt(_mib(med(c, l, "max_rss_kib")), 2) for l in langs]
            out.append(f"| {c} | " + " | ".join(cpu_cells) + " | " + " | ".join(rss_cells) + " |")
        out.append("")

    out.append("## Reproduce\n")
    out.append("```sh")
    out.append(
        f"python3 benches/go_vs_willow/compare.py run --label {label} --trials {max(trials) if trials else 5} "
        f"--workers {workers} --languages {','.join(langs)}"
    )
    out.append("```\n")
    return "\n".join(out)


def _us(ns: float | None) -> float | None:
    return None if ns is None else ns / 1e3


def _mib(kib: float | None) -> float | None:
    return None if kib is None else kib / 1024


def main() -> None:
    parser = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    sub = parser.add_subparsers(dest="command", required=True)

    run = sub.add_parser("run", help="build, run both suites, write JSONL + Markdown")
    run.add_argument("--label", default=dt.date.today().isoformat())
    run.add_argument("--trials", type=int, default=5)
    run.add_argument("--workers", type=int, default=8)
    run.add_argument("--suite", choices=("all", "sync", "scheduler"), default="all")
    run.add_argument("--no-build", action="store_true")
    run.add_argument(
        "--languages",
        default=",".join(LANGUAGES),
        help=f"comma-separated subset of {','.join(LANGUAGES)} (default: %(default)s)",
    )
    run.add_argument("--baseline", type=Path, help="previous MEASUREMENT_*.jsonl for delta columns")
    run.add_argument("--out-dir", type=Path, default=SUITE)

    rep = sub.add_parser("report", help="render Markdown from an existing JSONL")
    rep.add_argument("jsonl", type=Path)
    rep.add_argument("--label")
    rep.add_argument("--workers", type=int, default=8)
    rep.add_argument("--baseline", type=Path)
    rep.add_argument("--out", type=Path, help="write Markdown to this path (overwrites if it exists); default: stdout only")

    args = parser.parse_args()
    if args.command == "run":
        if args.trials < 1 or args.workers < 1:
            parser.error("--trials and --workers must be at least 1")
        args.languages = [l for l in dict.fromkeys(p.strip() for p in args.languages.split(",")) if l]
        if not args.languages or any(l not in LANGUAGES for l in args.languages):
            parser.error(f"--languages must name one or more of {','.join(LANGUAGES)}")
        env = environment()
        rows = run_suites(args)
        args.out_dir.mkdir(parents=True, exist_ok=True)
        jsonl = args.out_dir / f"MEASUREMENT_{args.label}.jsonl"
        records = [{"record": "environment", **env}, *rows]
        jsonl.write_text("".join(json.dumps(r, sort_keys=True) + "\n" for r in records))
        md = args.out_dir / f"MEASUREMENT_{args.label}.md"
        label = args.label
    else:
        jsonl = args.jsonl
        env, rows = split_environment(json_lines(jsonl.read_text()))
        label = args.label or jsonl.stem.removeprefix("MEASUREMENT_")
        md = args.out
    baseline = split_environment(json_lines(args.baseline.read_text()))[1] if args.baseline else None
    markdown = render(rows, label, baseline, args.workers, jsonl.name, env)
    if md is not None:
        md.write_text(markdown)
        print(md)
    print(markdown)


if __name__ == "__main__":
    main()
