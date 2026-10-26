#!/usr/bin/env python3
"""Paired focused runtime benchmark, using the checked-in run.py sampler.

Build before/after executables in BIN_DIR named {benchmark}-{before,after},
using the same compiler and --runtime-lib for the two runtime static libraries.
Usage: python3 measure.py BIN_DIR OUTPUT.jsonl [--trials 9]
"""
import argparse
import importlib.util
import json
import os
from pathlib import Path
import statistics
import subprocess
import tempfile

ROOT = Path(__file__).resolve().parents[3]
import sys
sys.path.insert(0, str(ROOT / 'benches/go_vs_willow'))
spec = importlib.util.spec_from_file_location('benchmark_run', ROOT / 'benches/go_vs_willow/run.py')
harness = importlib.util.module_from_spec(spec)
spec.loader.exec_module(harness)
p = argparse.ArgumentParser()
p.add_argument('binaries', type=Path)
p.add_argument('output', type=Path)
p.add_argument('--trials', type=int, default=9)
p.add_argument('--gc-rounds', type=int, default=1000)
a = p.parse_args()
rows = []
# Reuse one_run unchanged, overriding only executable selection.
for trial in range(a.trials):
    for benchmark, values in [('spawn_join_tree', [10, 5]), ('gc_scheduler', [1000, a.gc_rounds])]:
        for variant in (['before', 'after'] if trial % 2 == 0 else ['after', 'before']):
            binary = a.binaries.resolve() / f'{benchmark}-{variant}'
            harness.command = lambda language, bench, args, workers: [str(binary), *map(str, args)]
            row = dict(harness.one_run('willow', benchmark, values, 8), trial=trial,
                       benchmark=benchmark, variant=variant, values=values, workers=8)
            expected_metric = 'LEAVES' if benchmark == 'spawn_join_tree' else 'KEPT_OBJECTS'
            expected = values[0] ** values[1] if benchmark == 'spawn_join_tree' else values[0] * values[1]
            assert row['metrics'][expected_metric] == expected, row
            rows.append(row)
            print(json.dumps(row), flush=True)
for trial in range(3):
    for variant in ['before', 'after']:
        with tempfile.NamedTemporaryFile() as trace:
            result = subprocess.run(['strace', '-f', '-c', '-e', 'trace=futex', '-o', trace.name,
                                     str(a.binaries.resolve() / f'spawn_join_tree-{variant}'), '10', '5'],
                                    env=dict(os.environ, WILLOW_WORKERS='8'),
                                    capture_output=True, text=True, check=True)
            assert '100000' in result.stdout
            line = next(line for line in Path(trace.name).read_text().splitlines()
                        if line.rstrip().endswith('futex'))
            calls = int(line.split()[3])
        row = dict(variant=variant, trial=trial, futex_calls=calls, tasks=111111,
                   futex_per_task=calls / 111111, workers=8)
        rows.append(row)
        print(json.dumps(row), flush=True)
a.output.write_text(''.join(json.dumps(row) + '\n' for row in rows))
for benchmark in ['spawn_join_tree', 'gc_scheduler']:
    for variant in ['before', 'after']:
        selected = [row for row in rows if row.get('benchmark') == benchmark and row['variant'] == variant]
        print(benchmark, variant, 'median_ms', statistics.median(row['elapsed_ns'] for row in selected) / 1e6)
