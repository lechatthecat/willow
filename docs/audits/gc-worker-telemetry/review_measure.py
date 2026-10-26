#!/usr/bin/env python3
"""Review repair evidence: real closure, integrated prototype A/B, request receipts."""
import json, os, shutil, subprocess, tempfile, sys
from pathlib import Path
ROOT=Path(__file__).resolve().parents[3]
OUT=Path(__file__).resolve().parent

def call(args, **kwargs):
    return subprocess.run(['rtk','proxy',*map(str,args)],cwd=ROOT,check=True,**kwargs)

def main():
    env=dict(os.environ)
    env.pop('WILLOW_GC_PROFILE',None)
    env.pop('WILLOW_GC_NOTIFY',None)
    rows=[]
    with tempfile.TemporaryDirectory(prefix='willow-m3-notification-') as temp:
        temp=Path(temp)
        examples=['gc_allocation_bench','gc_m3_profile']
        for feature in [False,True]:
            args=['cargo','build','-p','willow_runtime','--release']
            if feature: args += ['--features','gc-notification-prototype']
            for example in examples: args += ['--example',example]
            call(args,env=env)
            for example in examples:
                shutil.copy2(ROOT/'target/release/examples'/example,temp/(example+('-notify' if feature else '-default')))
        for mode in ['alloc','chain','graph']:
            example='gc_m3_profile' if mode=='graph' else 'gc_allocation_bench'
            arguments=['4096','4'] if mode=='graph' else ['65536',mode]
            for workers in [1,4,16]:
                for run in range(5):
                    versions=['default','notify'] if run%2==0 else ['notify','default']
                    for version in versions:
                        stats=temp/'stats'
                        result=call(['/usr/bin/time','-f','%M %U %S','-o',stats,temp/(example+'-'+version),*arguments],env=dict(env,WILLOW_GC_MARK_THREADS=str(workers),WILLOW_GC_NOTIFY='1' if version=='notify' else '0'),capture_output=True,text=True,timeout=120)
                        events=[json.loads(line) for line in result.stdout.splitlines() if line.startswith('{')]
                        app=next(e for e in events if e.get('event')=='application') if mode=='graph' else events[0]
                        rss,user,system=stats.read_text().split()
                        rows.append(dict(mode=mode,workers=workers,run=run,version=version,rss_kib=int(rss),process_cpu_s=float(user)+float(system),**app))
                print('integrated comparison',mode,workers,flush=True)
        (OUT/'notification-comparison.json').write_text(json.dumps(rows,indent=2)+'\n')
    for run in range(5):
        result=call(['cargo','test','-p','willow_runtime','--release','--lib','m3_real_closure_last_work_latency_and_cpu','--','--nocapture','--test-threads=1'],env=env,capture_output=True,text=True)
        assert '1 passed' in result.stdout
        (OUT/f'closure-process-{run}.txt').write_text(result.stdout+result.stderr)
    measure_opportunity(env)

def measure_opportunity(env):
    call(['cargo','build','-p','willow_runtime','--release','--example','gc_m3_opportunity'],env=env)
    rows=[]
    for mode in ['idle','busy']:
        for run in range(5):
            result=call([ROOT/'target/release/examples/gc_m3_opportunity',mode],env=dict(env,WILLOW_GC_MEMORY_LIMIT='16777216'),capture_output=True,text=True,timeout=30)
            row=json.loads(result.stdout.strip()); row['run']=run
            assert row['completed_cycles']>=1 and row['heap_after']==0
            rows.append(row)
        print('request receipts',mode,flush=True)
    (OUT/'opportunity.json').write_text(json.dumps(rows,indent=2)+'\n')
if __name__=='__main__':
    if '--opportunity-only' in sys.argv:
        measure_opportunity(dict(os.environ))
    else:
        main()
