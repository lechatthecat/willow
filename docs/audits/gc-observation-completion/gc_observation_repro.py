#!/usr/bin/env python3
"""Reproduce V4 parsing and disabled/aggregate/trace costs; binaries stay in target."""
import argparse
import json
import os
from pathlib import Path
import statistics
import subprocess
import sys
import tempfile
sys.dont_write_bytecode = True
from gc_observation_check import check


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument('--binary', type=Path, required=True)
    parser.add_argument('--output', type=Path, required=True)
    parser.add_argument('--repeats', type=int, default=7)
    args = parser.parse_args()
    binary = args.binary.resolve()
    results = []
    with tempfile.TemporaryDirectory(prefix='willow-v4-') as temp:
        for repeat in range(args.repeats):
            for mode in ('disabled', 'measure', 'trace'):
                env = {k:v for k,v in os.environ.items() if not k.startswith('WILLOW_GC_')}
                trace = Path(temp)/f'{repeat}-{mode}.ndjson'
                if mode == 'measure': env['WILLOW_GC_MEASURE']='1'
                if mode == 'trace': env.update(WILLOW_GC_TRACE_VERSION='4',WILLOW_GC_TRACE=str(trace))
                run = subprocess.run([str(binary)],env=env,text=True,capture_output=True,check=True)
                records=[json.loads(line) for line in run.stdout.splitlines() if line.startswith('{')]
                endpoints,cost=records
                if mode == 'trace':
                    with trace.open() as stream: verification=check(stream,endpoints['before'],endpoints['after'])
                else: verification=None
                results.append(dict(mode=mode,repeat=repeat,cost=cost,verification=verification,endpoints=endpoints))
    medians={mode:{key:statistics.median(r['cost'][key] for r in results if r['mode']==mode)
                   for key in results[0]['cost']} for mode in ('disabled','measure','trace')}
    ratios={mode:medians[mode]['allocations_per_second']/medians['disabled']['allocations_per_second']
            for mode in ('measure','trace')}
    args.output.write_text(json.dumps(dict(medians=medians,throughput_relative_to_disabled=ratios,runs=results),indent=2)+'\n')
    print(json.dumps(dict(medians=medians,throughput_relative_to_disabled=ratios),indent=2))


if __name__=='__main__': main()
