#!/usr/bin/env python3
"""End-to-end GC protocol injections. Requires gc-acceptance-faults build."""
import argparse
import json
import os
from pathlib import Path
import subprocess
import suite


TARGET_GATES = dict(short_stw='pause_aim', fallback='fallbacks',
                    mark_wait='bounded_wait', lock_stall='bounded_wait')


def targeted_gates_reject(decisions):
    """The release evaluator itself must reject every intended fault axis."""
    return set(decisions) == set(TARGET_GATES) and all(
        decisions[fault].get(gate, {}).get('passed') is False
        and decisions[fault].get('release_pass') is False
        for fault, gate in TARGET_GATES.items())


def transport_loss_count(stdout):
    """Require independent runtime loss counters, not a generic parser error."""
    records=[json.loads(line) for line in stdout.splitlines()]
    endpoints=[r.get('observation',{}) for r in records
               if r.get('event')=='endpoint' and r.get('label')=='after']
    if len(endpoints)!=1:return 0
    flags=endpoints[0].get('flags');dropped=endpoints[0].get('dropped')
    return dropped if type(flags) is int and flags&2 and type(dropped) is int and dropped>0 else 0


def main():
    parser=argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--binary',type=Path,required=True)
    parser.add_argument('--output',type=Path,required=True)
    args=parser.parse_args();args.output.mkdir(parents=True,exist_ok=False)
    case=dict(shape='linked',warmup=2,cycles=8,checksum=32640,require_cycles=True)
    budgets=dict(throughput_min_ratio=.97,cpu_max_ratio=1,footprint_max_ratio=1.1,pause_p99_aim_ns=2_000_000,local_wait_max_ns=10_000_000,fallback_max_count=0)
    binary_hash=suite.digest(args.binary)
    sources={p:suite.digest(p) for p in (Path(__file__),Path(suite.__file__),Path(suite.STOP.__file__),Path(suite.OBS.__file__))}
    suite.write_json(args.output/'frozen.json',dict(schema=1,case=case,budgets=budgets,binary_sha256=binary_hash,repeats=5,source_sha256={str(p.resolve().relative_to(suite.ROOT)):h for p,h in sources.items()},revision=subprocess.check_output(["git","rev-parse","HEAD"],cwd=suite.ROOT,text=True).strip()))
    rows=[];baseline=[];samples={}
    for fault in ('none','short_stw','fallback','mark_wait','lock_stall','trace_loss'):
        for repeat in range(5):
            folder=args.output/f'{fault}-{repeat}';folder.mkdir()
            env={k:v for k,v in os.environ.items() if not k.startswith(('WILLOW_','GC_ACCEPTANCE_'))}
            env.update(WILLOW_WORKERS='1',WILLOW_GC_MARK_THREADS='1',WILLOW_GC_TRACE_VERSION='4',WILLOW_GC_STOP_TRACE_CAPACITY='16384',WILLOW_GC_TRACE_CAPACITY='1' if fault=='trace_loss' else '524288',WILLOW_GC_TRACE=str((folder/'runtime.ndjson').resolve()),GC_ACCEPTANCE_EVENT_SPOOL=str((folder/'observations.bin').resolve()))
            if fault not in ('none','trace_loss'):env['GC_ACCEPTANCE_FAULT']=fault
            if suite.digest(args.binary)!=binary_hash:raise RuntimeError('fault probe binary changed during confirmation')
            command=[str(args.binary.resolve()),'linked','65536','4','2','8']
            suite.write_json(folder/'invocation.json',dict(command=command,configuration={k:v for k,v in env.items() if k.startswith(('WILLOW_','GC_ACCEPTANCE_'))}))
            with (folder/'stdout.ndjson').open('w') as stdout,(folder/'stderr.txt').open('w') as stderr:
                run=subprocess.run(command,env=env,stdout=stdout,stderr=stderr,timeout=30)
            record=dict(fault=fault,repeat=repeat,returncode=run.returncode,expected_rejection=False)
            if run.returncode: raise RuntimeError(f'{fault} exited {run.returncode}; not an expected gate rejection')
            try:
                suite.merge_trace(folder)
                result=suite.parse_run((folder/'stdout.ndjson').read_text(),folder/'trace.ndjson',case)
                if fault=='none':baseline.append(result)
                else:
                    samples.setdefault(fault,[]).append(result)
                    if fault=='short_stw':record['expected_rejection']=result['pause_ns'].get('1',{}).get('p99',0)>budgets['pause_p99_aim_ns']
                    elif fault=='fallback':record['expected_rejection']=result['stop_coverage']['by_reason'][3]>0
                    elif fault=='mark_wait':record['expected_rejection']=result['local_stall_ns'].get('9',{}).get('max',0)>budgets['local_wait_max_ns']
                    elif fault=='lock_stall':record['expected_rejection']=result['local_stall_ns'].get('4',{}).get('max',0)>budgets['local_wait_max_ns']
            except ValueError as error:
                record['error']=str(error)
                record['runtime_dropped_events']=transport_loss_count((folder/'stdout.ndjson').read_text())
                record['expected_rejection']=fault=='trace_loss' and 'lost' in str(error) and record['runtime_dropped_events']>0
            record['summary']=result if 'error' not in record else None
            rows.append(record);suite.write_json(folder/'result.json',record)
            suite.compact_capture(folder)
            print(json.dumps({k:v for k,v in record.items() if k!='summary'}),flush=True)
    baseline_gate=suite.comparison(baseline,baseline,budgets)
    group_decisions={fault:suite.comparison(baseline,runs,budgets) for fault,runs in samples.items() if len(runs)==5}
    unchanged=suite.digest(args.binary)==binary_hash and all(suite.digest(p)==h for p,h in sources.items())
    passed=unchanged and baseline_gate['release_pass'] and targeted_gates_reject(group_decisions) and all(r['expected_rejection'] for r in rows if r['fault']!='none')
    suite.write_json(args.output/'summary.json',dict(schema=1,binary_sha256=binary_hash,source_unchanged=unchanged,diagnostics_pass=passed,baseline_gate=baseline_gate,group_decisions=group_decisions,runs=rows))
    raise SystemExit(0 if passed else 1)


if __name__=='__main__':main()
