#!/usr/bin/env python3
"""Summarize retained process data; deterministic bootstrap, no reruns."""
import hashlib,json,platform,random,re,statistics,subprocess
from pathlib import Path
OUT=Path(__file__).resolve().parent
ROOT=OUT.parents[2]

def interval(a,b):
    rng=random.Random(674)
    samples=sorted(statistics.median(rng.choices(b,k=len(b)))/statistics.median(rng.choices(a,k=len(a))) for _ in range(10000))
    return samples[249],samples[9749]

def main():
    comparisons=json.loads((OUT/'comparison.json').read_text())
    lines=['# M3 local measurements', '', 'Five independent processes per comparison cell. CPU timing is nanoseconds. Confidence intervals are deterministic bootstrap 95% intervals for the ratio of medians (10,000 resamples; seed 674). Five processes are a small sample; these are fixture-specific estimates.', '', '## Profiling-disabled application comparison', '', '| Workload | Markers | Throughput ratio (candidate/base) | 95% CI | Worst RSS ratio |', '|---|---:|---:|---:|---:|']
    for mode in ['alloc','chain']:
        for workers in [1,4,16]:
            groups={v:[r for r in comparisons['rows'] if (r['mode'],r['workers'],r['version'])==(mode,workers,v)] for v in ['baseline','current']}
            a=[1e9/r['elapsed_ns'] for r in groups['baseline']]; b=[1e9/r['elapsed_ns'] for r in groups['current']]
            low,high=interval(a,b)
            ratio=statistics.median(b)/statistics.median(a)
            rss=max(r['rss_kib'] for r in groups['current'])/max(r['rss_kib'] for r in groups['baseline'])
            lines.append(f'| {mode} | {workers} | {ratio:.4f} | [{low:.4f}, {high:.4f}] | {rss:.4f} |')
    lines += ['', 'These fixtures check instrumentation/regression risk. They do not prove a production notification optimization: the separately measured integrated prototype appears below. No generic standard-set claim is made.', '', '## Mark-worker scaling (profiling enabled)', '', '| Objects | Fanout | Markers | Median application ms | Median 95% CI ms | Peak RSS KiB |', '|---:|---:|---:|---:|---:|---:|']
    for r in json.loads((OUT/'summary.json').read_text())['results']:
        ci=r['median_95_bootstrap_ci_ns']
        lines.append(f"| {r['objects']} | {r['fanout']} | {r['workers']} | {r['median_ns']/1e6:.3f} | [{ci[0]/1e6:.3f}, {ci[1]/1e6:.3f}] | {r['peak_rss_kib']} |")
    rows=[json.loads(line) for line in (OUT/'measurements.jsonl').read_text().splitlines()]
    rows=[json.loads(line) for line in (OUT/'measurements.jsonl').read_text().splitlines()]
    lines += ['', 'One separately registered CPU-active mutator runs during 32 explicit collections in each process. Its own thread CPU/wall ratio is measured, not inferred from empty queues. Cycle p99 below is nearest-rank over 160 calls (five processes x 32); this includes collector completion, not just a global pause.', '', '| Objects | Fanout | Markers | Cycle p99 ms | Median mutator CPU/wall | Median mutator operations/s |', '|---:|---:|---:|---:|---:|---:|']
    for n,fanout,w in sorted({(r['objects'],r['fanout'],r['workers']) for r in rows}):
        samples=[next(e for e in r['events'] if e['event']=='mutator') for r in rows if (r['objects'],r['fanout'],r['workers'])==(n,fanout,w)]
        cycles=sorted(t for e in samples for t in e['cycle_ns'])
        p99=cycles[(99*len(cycles)+99)//100-1]
        duty=statistics.median(e['cpu_ns']/e['wall_ns'] for e in samples if e['cpu_valid'])
        ops=statistics.median(e['operations']*1e9/e['wall_ns'] for e in samples)
        lines.append(f'| {n} | {fanout} | {w} | {p99/1e6:.3f} | {duty:.4f} | {ops:.0f} |')
    lines += ['', 'The raw worker rows report CPU, work, queue-depth samples and steals separately. These shapes do not demonstrate consistent benefit from 8/16 markers; keep the current default and fallback stealing. Shared-graph duplicate claims and the sequential chain frontier are visible limitations; no bitmap partitioning experiment is justified by these data alone.', '', '## Lock distributions', '', '| Kind | Samples (all graph runs) | Largest per-process p99 upper bound ns | Maximum ns |', '|---|---:|---:|---:|']
    for kind,name in enumerate(['SATB registry acquisition','SATB private-buffer acquisition','SATB seal slice','sweep heap acquisition','sweep heap hold']):
        items=[e for r in rows for e in r['events'] if e['event']=='lock' and e['kind']==kind]
        lines.append(f"| {name} | {sum(e['count'] for e in items)} | {max(e['p99_upper_ns'] for e in items)} | {max(e['max_ns'] for e in items)} |")
    lines += ['', 'Each p99 is a power-of-two histogram bound for one process; the maximum of these is not a pooled p99. No evidence here supports replacing the bounded sweep cursor with region-local locks. Native hook/release time remains outside the heap lock.', '', '## SATB and termination', '', 'The following are exact lines from the focused final test log; SATB measurements are instrumented debug measurements, not application-throughput evidence:', '', '```text']
    for line in (OUT/'m3_.txt').read_text().splitlines():
        match=re.search(r'm3_(?:satb|termination|saturation) .+',line)
        if match: lines.append(match.group())
    lines += ['```', '', 'No registry collisions were observed at <=64 producers in this fixture. Above 64, collisions appear; profiling itself prolongs holds. This does not justify a TLS redesign or an extrapolated production collision rate. Seal capture verifies exactly producers x 4096 deletions.', '', '## Integrated termination prototype', '', 'Normal build versus `gc-notification-prototype` with `WILLOW_GC_NOTIFY=1`. Five fresh processes per cell, alternating order, profiling disabled. Both execute the same closure and stopped recheck; only the feature build can interrupt idle waits after independently repaying collector CPU. Runnable retry work retains its 1 ms floor. RSS is peak process RSS; CPU from GNU time is coarse (10 ms), so zeros do not establish zero CPU cost.', '', '| Application | Markers | Throughput ratio notify/default | 95% CI | Peak RSS ratio | Median process CPU s default/notify |', '|---|---:|---:|---:|---:|---:|']
    notify=json.loads((OUT/'notification-comparison.json').read_text())
    for mode in ['alloc','chain','graph']:
        for w in [1,4,16]:
            groups={v:[r for r in notify if (r['mode'],r['workers'],r['version'])==(mode,w,v)] for v in ['default','notify']}
            a=[1e9/r['elapsed_ns'] for r in groups['default']]; b=[1e9/r['elapsed_ns'] for r in groups['notify']]
            low,high=interval(a,b)
            rss=max(r['rss_kib'] for r in groups['notify'])/max(r['rss_kib'] for r in groups['default'])
            cpu=[statistics.median(r['process_cpu_s'] for r in groups[v]) for v in ['default','notify']]
            lines.append(f'| {mode} | {w} | {statistics.median(b)/statistics.median(a):.4f} | [{low:.4f}, {high:.4f}] | {rss:.4f} | {cpu[0]:.2f}/{cpu[1]:.2f} |')
    closure=[]
    for path in sorted(OUT.glob('closure-process-*.txt')):
        for m in re.finditer(r'm3_termination notification=(false|true) samples=100 p50_ns=(\d+) p99_ns=(\d+) closure_cpu_ns=(\d+) cpu_valid=true',path.read_text()):
            closure.append(dict(mode=m[1],p50=int(m[2]),p99=int(m[3]),cpu=int(m[4])))
    assert len(closure)==10
    lines += ['', 'Actual closure fixture: 100 last-batch publications per mode per process, five release processes. The last batch completes at 0/100/500/1500 microseconds after closure observes its active reader. Latency starts at final accounting publication immediately before owner release, ends when the real closure returns a sweep plan after stopped recheck. CPU covers only closure, excluding thread creation/join. These are controlled closure-tail measurements, not whole-heap/application latency distributions.', '', '| Wait policy | Median process p99 ns | p99 range ns | Median closure CPU ns (100 closures) |', '|---|---:|---:|---:|']
    for mode in ['false','true']:
        group=[r for r in closure if r['mode']==mode]
        lines.append(f"| {'notification' if mode=='true' else 'paced sleep'} | {statistics.median(r['p99'] for r in group):.0f} | {min(r['p99'] for r in group)}..{max(r['p99'] for r in group)} | {statistics.median(r['cpu'] for r in group):.0f} |")
    a=[r['cpu'] for r in closure if r['mode']=='false']; b=[r['cpu'] for r in closure if r['mode']=='true']
    low,high=interval(a,b)
    lines += ['', f'Closure CPU notify/default ratio: {statistics.median(b)/statistics.median(a):.4f}, 95% CI [{low:.4f}, {high:.4f}].', '', 'Decision: do not enable notification in normal builds. Real closure-tail latency improves, but application benefit is not consistent; the four-marker graph median exceeds the 3% regression allowance and CPU non-regression is not established. Retain the integrated feature only as a reproducible experiment. This decision uses the enabled prototype comparison, not the unrelated baseline instrumentation comparison.', '', '## Opportunity admission / Q10', '', '| CPU load | Optional admitted | Mandatory completed | Mandatory wait range ms | Process busy ppm | System idle ppm |', '|---|---:|---:|---|---|---|']
    for busy in [False,True]:
        group=[r for r in json.loads((OUT/'opportunity.json').read_text()) if r['busy']==busy]
        bounds=lambda key:f"{min(r[key] for r in group)}..{max(r[key] for r in group)}"
        lines.append(f"| {'saturated' if busy else 'idle'} | {sum(r['optional_accepted'] for r in group)}/5 | 5/5 | {min(r['required_ns'] for r in group)/1e6:.3f}..{max(r['required_ns'] for r in group)/1e6:.3f} | {bounds('process_busy_ppm')} | {bounds('system_idle_ppm')} |")
    lines += ['', 'Decision: keep early GC explicit and opt-in. Admit only fresh measured process/system spare capacity; missing/stale samples cannot defer required GC. The measured probe waits for its submitted asynchronous request receipt (without a second explicit collection) and completed required collection in every saturated run, with a 16 MiB hard limit and zero remaining managed bytes. The deterministic four-allocating-mutator test independently exercised a 2 MiB cap. Foreign-root receipt regressions separately prove that early/handshake skips do not complete requests, retained asynchronous requests recover after release, and shutdown leaves canceled receipts incomplete (m3_.txt). These finite tests are evidence against starvation in the exercised cases, not a universal scheduling-latency guarantee.', '', '## Native execution', '', 'Linux x86_64: executed. macOS arm64/x86_64 and Windows x86_64: Clippy cross-compiled; native execution unverified. CI and full local suites were not run. See audit.md for scope, complexity, lock order and limitations.']
    (OUT/'results.md').write_text('\n'.join(lines)+'\n')
    paths=subprocess.check_output(['rtk','proxy','git','diff','--name-only'],cwd=ROOT,text=True).splitlines()
    paths += ['crates/willow_runtime/src/gc_telemetry/m3.rs','crates/willow_runtime/src/gc/opportunity.rs','crates/willow_runtime/examples/gc_m3_profile.rs','crates/willow_runtime/examples/gc_m3_opportunity.rs']
    hashes={p:hashlib.sha256((ROOT/p).read_bytes()).hexdigest() for p in paths if not p.startswith('requirements/') and (ROOT/p).is_file()}
    (OUT/'manifest.json').write_text(json.dumps(dict(platform=platform.platform(),baseline=comparisons['baseline'],source_sha256=hashes,rustc=subprocess.check_output(['rtk','proxy','rustc','-V'],text=True).strip()),indent=2)+'\n')
if __name__=='__main__':main()
