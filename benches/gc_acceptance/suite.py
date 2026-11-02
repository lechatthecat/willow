#!/usr/bin/env python3
"""Versioned, fail-closed GC measurement runner. See README.md for scope."""
import argparse
import bisect
import collections
import concurrent.futures
import hashlib
import heapq
import struct
import importlib.util
import json
import math
import os
from pathlib import Path
import platform
import random
import statistics
import subprocess
import sys

sys.dont_write_bytecode = True
ROOT = Path(__file__).resolve().parents[2]


def module(name, path):
    spec = importlib.util.spec_from_file_location(name, ROOT / path)
    result = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(result)
    return result


STOP = module('stop_check', 'scripts/gc_stop_trace_check.py')
OBS = module('observation_check', 'benches/gc_acceptance/observation_check.py')
PLATFORMS = ('Linux-x86_64', 'Darwin-arm64', 'Darwin-x86_64', 'Windows-AMD64')


def quantiles(values):
    if not values:
        return None
    ordered = sorted(values)
    return {**{label: ordered[max(0, math.ceil(len(ordered) * q) - 1)]
               for label, q in [('p50', .5), ('p95', .95), ('p99', .99), ('p99.9', .999)]},
            'max': ordered[-1], 'count': len(values)}


def estimate(values):
    """Deterministic percentile bootstrap of the process median, 95% interval."""
    if len(values) < 5:
        raise ValueError('at least five independent process runs required')
    rng = random.Random(0)
    medians = sorted(statistics.median(rng.choices(values, k=len(values))) for _ in range(2000))
    return dict(median=statistics.median(values), ci95=[medians[49], medians[1949]],
                tails=quantiles(values), method='2000 bootstrap process medians; seed=0')


def union(intervals):
    result = []
    for start, end in sorted(intervals):
        if start > end:
            raise ValueError('backwards interval')
        if result and start <= result[-1][1]:
            result[-1][1] = max(end, result[-1][1])
        else:
            result.append([start, end])
    return result


def mmu(intervals, start, end, window):
    """Exact sliding-window minimum utilization; overlapping GC counted once.

    Union O(E log E); worst window must touch a union endpoint (possibly
    shifted by window). Prefix integration evaluates O(E) candidates in O(log E).
    """
    if end - start < window:
        return None
    spans = union((max(a, start), min(b, end)) for a, b in intervals if a < end and b > start)
    points, totals, total = [], [], 0
    for a, b in spans:
        points.extend([a, b])
        totals.extend([total, total + b - a])
        total += b - a
    def integral(t):
        i = bisect.bisect_right(points, t) - 1
        return 0 if i < 0 else totals[i] + (t - points[i] if i % 2 == 0 else 0)
    candidates = {start, end - window}
    for point in points:
        candidates.add(max(start, min(end - window, point)))
        candidates.add(max(start, min(end - window, point - window)))
    return 1 - max(integral(t + window) - integral(t) for t in candidates) / window


class MmuIndex:
    """Shared stop prefix integral and range-max index: O(log E) per lifetime.

    Build each of the three window indexes once, not once per short async worker.
    Assist-bearing lifetimes merge only their overlapping stops/local intervals.
    """
    def __init__(self, intervals):
        self.spans=union(intervals)
        self.starts=[a for a,b in self.spans];self.ends=[b for a,b in self.spans]
        self.points=[];self.totals=[];total=0
        for a,b in self.spans:
            self.points.extend((a,b));self.totals.extend((total,total+b-a));total+=b-a
        self.windows={}
    def integral(self,t):
        i=bisect.bisect_right(self.points,t)-1
        return 0 if i<0 else self.totals[i]+(t-self.points[i] if i%2==0 else 0)
    def overlapping(self,a,b):
        return self.spans[bisect.bisect_right(self.ends,a):bisect.bisect_left(self.starts,b)]
    def minimum(self,a,b,w):
        if b-a<w:return None
        if w not in self.windows:
            points=sorted(set(self.points+[x-w for x in self.points]))
            size=1
            while size<len(points):size*=2
            tree=[0]*(2*size)
            for i,t in enumerate(points):tree[size+i]=self.integral(t+w)-self.integral(t)
            for i in range(size-1,0,-1):tree[i]=max(tree[2*i],tree[2*i+1])
            self.windows[w]=(points,tree,size)
        points,tree,size=self.windows[w]
        left=bisect.bisect_left(points,a)+size;right=bisect.bisect_right(points,b-w)+size
        worst=max(self.integral(a+w)-self.integral(a),self.integral(b)-self.integral(b-w))
        while left<right:
            if left%2:worst=max(worst,tree[left]);left+=1
            if right%2:right-=1;worst=max(worst,tree[right])
            left//=2;right//=2
        return 1-worst/w


def mutator_mmu(stops, assists, lifetimes):
    global_index=MmuIndex(stops);result={}
    for thread,lives in lifetimes.items():
        local=MmuIndex(assists.get(thread,[]))
        windows={str(w):[] for w in (10**6,10**7,10**8)}
        for a,b in lives:
            local_spans=local.overlapping(a,b)
            for w in (10**6,10**7,10**8):
                value=mmu(global_index.overlapping(a,b)+local_spans,a,b,w) if local_spans else global_index.minimum(a,b,w)
                windows[str(w)].append(value)
        result[str(thread)]=windows
    return result


def parse_run(stdout, trace, case):
    records = [json.loads(line) for line in stdout.splitlines() if line.strip()]
    if any(r.get('schema') != 1 for r in records):
        raise ValueError('unknown application schema')
    endpoints = [r for r in records if r['event'] == 'endpoint']
    if len(endpoints) != 2 or [r['label'] for r in endpoints] != ['before', 'after']:
        raise ValueError('missing/duplicate independent endpoints')
    before, after = endpoints
    capture_start, capture_end = before.get('timestamp_ns'), after.get('timestamp_ns')
    if (any(type(t) is not int or not 0 <= t < 2**64-1 for t in (capture_start, capture_end))
            or capture_start > capture_end):
        raise ValueError('invalid capture timestamps')
    def lines():
        if isinstance(trace, Path):
            with trace.open() as stream:
                yield from stream
        else:
            yield from trace.splitlines()
    samples = [r for r in records if r['event'] == 'sample']
    if len(samples) != case['warmup'] + case['cycles'] or [r['index'] for r in samples] != list(range(len(samples))):
        raise ValueError('missing or duplicated application samples')
    if any(r['event'] not in ('sample', 'endpoint') for r in records):
        raise ValueError('unknown application record')
    previous_sample_time = None
    for sample in samples:
        timestamp = sample.get('timestamp_ns')
        if (type(timestamp) is not int or not 0 <= timestamp < 2**64-1
                or not capture_start <= timestamp <= capture_end
                or (previous_sample_time is not None and timestamp <= previous_sample_time)):
            raise ValueError('invalid/out-of-order sample timestamp within capture')
        previous_sample_time = timestamp
        # Saturating telemetry must not become a plausible zero delta. Check
        # every sample, including warmup; checksums alone use all 64 bits as data.
        if any(type(v) is not int or not 0 <= v < 2**64
               or (v == 2**64-1 and k not in ('checksum', 'workload_checksum'))
               for k, v in sample.items() if k != 'event'):
            raise ValueError('invalid/saturated application scalar')
        if sample['checksum'] != case['checksum'] or sample.get('workload_checksum', 0) != case.get('workload_checksum', 0) or not sample['operations'] or not sample['elapsed_ns']:
            raise ValueError('application checksum/progress mismatch')
        if case.get('require_cycles') and any(type(sample.get(k)) is not int or not 0 <= sample[k] < 2**64-1 for k in ('wasted_bytes','peak_committed_bytes','peak_rss_bytes','sweep_objects','sweep_bytes','sweep_ns')):
            raise ValueError('missing/saturated measurement counter')
        if sample['rss_valid'] != 1 or sample['cpu_missing_cycles']:
            raise ValueError('missing RSS or CPU measurement')
    measured = samples[case['warmup']:]
    initial, final = samples[case['warmup'] - 1], samples[-1]
    low, high = initial['timestamp_ns'], final['timestamp_ns']
    if high <= low:
        raise ValueError('empty measurement window')
    counters = ('allocation_bytes', 'reclaimed_bytes', 'copied_bytes', 'promoted_bytes', 'barrier_ops', 'gc_cpu_ns', 'cycles')
    if any(b[k] < a[k] for a, b in zip(samples, samples[1:]) for k in counters):
        raise ValueError('counter decreased')
    delta = {k: final[k] - initial[k] for k in counters}
    if case.get('require_cycles') and any(type(final.get(k)) is not int or final[k] <= 0 for k in ('peak_committed_bytes','peak_rss_bytes')):
        raise ValueError('missing process high-water measurements')
    if not delta['allocation_bytes'] or not delta['cycles']:
        raise ValueError('no allocation or completed cycle')
    requests, pauses, gc_intervals, local, assists = {}, collections.defaultdict(list), [], collections.defaultdict(list), collections.defaultdict(list)
    active = {}
    cycle_starts, cycle_ends, milestones = {}, {}, collections.defaultdict(dict)
    measured_stops = []
    lifetimes, registrations = {}, {}
    assist_intervals = collections.defaultdict(list)
    legacy=[]
    def checked_lines():
        for line in lines():
            e=json.loads(line)
            if e.get('version') in (1,2):legacy.append(line)
            if e.get('version') == 1:
                if type(e['version']) is not int or e.get('event') not in ('gc_start', 'gc_end'):
                    raise ValueError('invalid cycle event name/version')
                if type(e.get('kind')) is not int or e['kind'] not in (1, 2):
                    raise ValueError('unsupported cycle kind')
                fields = ('epoch', 'timestamp_ns', 'heap_bytes')
                if e['event'] == 'gc_end':
                    fields += ('pause_ns', 'mark_ns', 'marked_bytes', 'scanned_bytes',
                               'root_scan_bytes', 'reclaimed_bytes')
                if (any(type(e.get(field)) is not int or not 0 <= e[field] < 2**64-1
                        for field in fields) or e['epoch'] == 0):
                    raise ValueError('missing/invalid unsigned cycle field')
                target = cycle_starts if e['event'] == 'gc_start' else cycle_ends
                if e['epoch'] in target: raise ValueError('duplicate cycle event')
                target[e['epoch']] = e
            elif e.get('version') == 3 and e['event'] in ('MarkComplete', 'SweepComplete', 'Quiescent'):
                if e['event'] in milestones[e['epoch']]:
                    raise ValueError('duplicate milestone')
                milestones[e['epoch']][e['event']] = e['timestamp_ns']
            elif e.get('version') == 2:
                timestamp = e.get('timestamp_ns')
                if (type(timestamp) is not int or not 0 <= timestamp < 2**64-1
                        or not capture_start <= timestamp <= capture_end):
                    raise ValueError('stop timestamp outside capture or invalid')
                if e['event'] == 'gc_stop_request':
                    requests[e['sequence']] = e['timestamp_ns']
                else:
                    begin, end = requests[e['sequence']], e['timestamp_ns']
                    if low <= begin and end <= high:
                        pauses[str(e['reason'])].append(end - begin)
                        measured_stops.append((begin,end,e['reason']))
                    if begin < high and end > low:
                        gc_intervals.append((begin, end))
            elif e.get('event') == 'gc_observation':
                timestamp = e.get('timestamp_ns')
                if (type(timestamp) is not int or not 0 <= timestamp < 2**64-1
                        or not capture_start <= timestamp <= capture_end):
                    raise ValueError('observation timestamp outside capture or invalid')
                if e['op'] == 3 and e['role'] == 0:
                    registrations[e['thread']] = e['timestamp_ns']
                elif e['op'] == 4 and e['role'] == 0:
                    a = max(low, registrations.pop(e['thread']))
                    b = min(high, e['timestamp_ns'])
                    if a < b:
                        lifetimes.setdefault(e['thread'], []).append((a, b))
                elif e['op'] == 1:
                    active[e['sequence']] = e
                elif e['op'] == 2:
                    begin = active.pop(e['identity'])
                    a, b = begin['timestamp_ns'], e['timestamp_ns']
                    if e['role'] == 0 and a < high and b > low:
                        local[str(e['kind'])].append(min(b, high) - max(a, low))
                        if e['kind'] == 1:
                            assists[str(e['thread'])].append(min(b, high) - max(a, low))
                            assist_intervals[e['thread']].append((a, b))
            yield line
    observations = OBS.check(checked_lines(), before['observation'], after['observation'])
    stop = STOP.check(legacy, before['stop'], after['stop'])
    combined, mark_bytes, mark_ns, sweep_bytes, sweep_ns = [], 0, 0, 0, 0
    old_stops = sorted((a,b) for a,b,reason in measured_stops if reason != 3)
    ends = [b for a,b in old_stops]
    starts = [a for a,b in old_stops]
    prefix = [0]
    for a,b in old_stops: prefix.append(prefix[-1]+b-a)
    if case.get('require_cycles') and (not cycle_ends or cycle_starts.keys() != cycle_ends.keys()):
        raise ValueError('missing cycle stream')
    if case.get('require_cycles'):
        # Start/end pairs can disappear together. The endpoint snapshots count
        # completed collections independently of the streamed cycle records.
        first = before['stop'].get('completed_cycles')
        last = after['stop'].get('completed_cycles')
        if (any(type(value) is not int or not 0 <= value < 2**64-1 for value in (first, last))
                or last < first or len(cycle_ends) != last - first
                or any(not first <= sample['cycles'] <= last or sample['cycles'] == 2**64-1
                       for sample in samples)):
            raise ValueError('incomplete cycle coverage against completed-cycle counters')
    for epoch, end_event in cycle_ends.items():
        start_event=cycle_starts.get(epoch)
        if start_event is None: raise ValueError('missing cycle start')
        if start_event['kind'] != end_event['kind']:
            raise ValueError('mismatched cycle kinds')
        a,b=start_event['timestamp_ns'],end_event['timestamp_ns']
        if any(type(t) is not int or not 0 <= t < 2**64-1 for t in (a, b)) or a > b:
            raise ValueError('invalid cycle timestamps')
        m=milestones[epoch]
        if (set(m) != {'MarkComplete','SweepComplete','Quiescent'}
                or any(type(t) is not int or not 0 <= t < 2**64-1 for t in m.values())
                or not capture_start <= a <= m['MarkComplete'] <= m['SweepComplete'] <= b <= capture_end
                or not m['SweepComplete'] <= m['Quiescent'] <= capture_end):
            raise ValueError('missing/misordered phase events')
        # Quiescent may publish after gc_end at election release, but must
        # remain within the independent capture endpoints.
        # Warmup/boundary cycles still require intact, ordered evidence. Only
        # their contribution to measured statistics is excluded here.
        if not low <= a <= b <= high: continue
        mark_bytes += end_event['marked_bytes']; mark_ns += end_event['mark_ns']
        sweep_bytes += end_event['reclaimed_bytes']; sweep_ns += m['SweepComplete']-m['MarkComplete']
        if end_event['kind'] == 2:
            left=bisect.bisect_left(starts,a);right=bisect.bisect_right(ends,b)
            combined.append(prefix[right]-prefix[left])
    if any(b.get(k,0)<a.get(k,0) for a,b in zip(samples,samples[1:]) for k in ('wasted_bytes','peak_committed_bytes','peak_rss_bytes','sweep_objects','sweep_bytes','sweep_ns','burst_allocation_overlap','burst_mutation_overlap')):
        raise ValueError('counter decreased')
    sweep_delta={k:final.get(k,0)-initial.get(k,0) for k in ('sweep_objects','sweep_bytes','sweep_ns')}
    majors=sorted((v['timestamp_ns'],milestones[k]['MarkComplete']) for k,v in cycle_starts.items() if v['kind']==2 and 'MarkComplete' in milestones[k])
    major_starts=[a for a,b in majors]
    minor_overlap=0
    for value in cycle_starts.values():
        if value['kind']==1:
            i=bisect.bisect_right(major_starts,value['timestamp_ns'])-1
            minor_overlap+=int(i>=0 and value['timestamp_ns']<majors[i][1])
    active_count=0; max_mutators=0
    for _,change in sorted((time,change) for spans in lifetimes.values() for a,b in spans for time,change in ((a,1),(b,-1))):
        active_count+=change;max_mutators=max(max_mutators,active_count)
    retained = statistics.median(r['live_bytes'] for r in measured)
    duration = sum(r['elapsed_ns'] for r in measured)
    operations = sum(r['operations'] for r in measured)
    return dict(schema=1, measurement_complete=True,
                scope='instrumented runtime workload; release decision requires comparison',
                max_application_mutators=max_mutators,
                latency_ns=quantiles([r['elapsed_ns'] for r in measured]),
                application_ops_per_second=operations * 1e9 / duration,
                allocation_bytes_per_second=delta['allocation_bytes'] * 1e9 / duration,
                gc_cpu_seconds_per_gib=delta['gc_cpu_ns'] / 1e9 * 2**30 / delta['allocation_bytes'],
                memory={k: dict(sampled_peak=max(r[k] for r in measured), steady=statistics.median(r[k] for r in measured))
                        for k in ('committed_bytes', 'reserved_bytes', 'rss_bytes', 'live_bytes', 'quarantine_bytes', 'fragmentation_bytes')},
                reclaimed_bytes_per_cycle=delta['reclaimed_bytes'] / delta['cycles'],
                concurrent_minor_cycles=minor_overlap,
                allocation_to_retained_ratio=delta['allocation_bytes']/len(measured)/retained if retained else None,
                burst_allocation_overlap=final.get('burst_allocation_overlap',0)-initial.get('burst_allocation_overlap',0),
                burst_mutation_overlap=final.get('burst_mutation_overlap',0)-initial.get('burst_mutation_overlap',0),
                overload_phase_peak_bytes=quantiles([r.get("overload_peak",0) for r in measured]),
                channel_overlap=final.get("channel_overlap",0)-initial.get("channel_overlap",0),
                sweep_overlap=final.get("sweep_overlap",0)-initial.get("sweep_overlap",0),
                copied_bytes=delta['copied_bytes'], promoted_bytes=delta['promoted_bytes'],
                wasted_bytes=final.get('wasted_bytes',0)-initial.get('wasted_bytes',0),
                peak_committed_bytes=final.get('peak_committed_bytes'), peak_rss_bytes=final.get('peak_rss_bytes'),
                mark_bytes_per_second=mark_bytes*1e9/mark_ns if mark_ns else None,
                sweep_reclaimed_bytes_per_second=sweep_bytes*1e9/sweep_ns if sweep_ns else None,
                major_sweep_visited_bytes_per_second=sweep_delta['sweep_bytes']*1e9/sweep_delta['sweep_ns'] if sweep_delta['sweep_ns'] else None,
                major_sweep_objects_per_second=sweep_delta['sweep_objects']*1e9/sweep_delta['sweep_ns'] if sweep_delta['sweep_ns'] else None,
                goal_bytes=final['goal_bytes'],
                sampled_overshoot_bytes=max(max(0,max(r['committed_bytes'],r['rss_bytes'])-r['goal_bytes']) for r in measured),
                hard_cap_pass=(final.get('peak_committed_bytes',0)<=int(case.get('env',{}).get('WILLOW_GC_MEMORY_LIMIT',2**64-1))),
                soft_footprint_ratio=(max(statistics.median(r[k] for r in measured) for k in ('reserved_bytes','rss_bytes'))/int(case['env']['WILLOW_GC_MEMORY_LIMIT_BYTES']) if case.get('env',{}).get('WILLOW_GC_MEMORY_LIMIT_BYTES') else None),
                soft_live_ratio=(final['live_bytes']/int(case['env']['WILLOW_GC_MEMORY_LIMIT_BYTES']) if case.get('env',{}).get('WILLOW_GC_MEMORY_LIMIT_BYTES') else None),
                combined_old_cycle_pause_ns=quantiles(combined),
                checksums=[r.get('workload_checksum',0) for r in measured],
                barrier_ops_per_application_op=delta['barrier_ops'] / operations,
                pause_ns={k: quantiles(v) for k, v in pauses.items()},
                local_stall_ns={k: quantiles(v) for k, v in local.items()},
                assist_by_mutator_ns={k: quantiles(v) for k, v in assists.items()},
                mmu_by_mutator=mutator_mmu(gc_intervals,assist_intervals,lifetimes),
                stop_coverage=stop, observation_coverage=observations)


def comparison(baseline, candidate, budgets):
    metrics = {'throughput': ('application_ops_per_second', lambda x: x),
               'cpu': ('gc_cpu_seconds_per_gib', lambda x: x),
               'footprint': ('peak_committed_bytes', lambda x: x),
               'rss': ('peak_rss_bytes', lambda x: x)}
    result = {}
    for label, (key, transform) in metrics.items():
        a = estimate([transform(r[key]) for r in baseline])
        b = estimate([transform(r[key]) for r in candidate])
        ratio = b['median'] / a['median'] if a['median'] else None
        passed = ratio is not None and (ratio >= budgets['throughput_min_ratio'] if label == 'throughput' else ratio <= budgets[('footprint' if label=='rss' else label) + '_max_ratio'])
        result[label] = dict(baseline=a, candidate=b, ratio=ratio, passed=passed)
    result['pause_aim'] = dict(target_ns=budgets['pause_p99_aim_ns'],
                               scope='reference-host aim, not a portable guarantee',
                               observed_max_p99_ns=max((q['p99'] for r in candidate for q in r['pause_ns'].values()), default=None))
    result['bounded_wait'] = dict(limit_ns=budgets['local_wait_max_ns'], passed=all(q['max'] <= budgets['local_wait_max_ns'] for r in candidate for q in r['local_stall_ns'].values()))
    # R1/R2 permits classified stops. A strict fallback cap is used only by
    # the explicit fault diagnostic; counts and pause/work costs remain visible.
    fallback_count = sum(r['stop_coverage']['by_reason'][3] for r in candidate)
    fallback_limit = budgets.get('fallback_max_count')
    result['fallbacks'] = dict(count=fallback_count, limit=fallback_limit,
        scope='diagnostic' if fallback_limit is None else 'explicit fault-injection cap',
        passed=fallback_limit is None or fallback_count <= fallback_limit)
    result['hard_cap'] = dict(passed=all(r['hard_cap_pass'] for r in candidate))
    result['differential'] = dict(passed=all(r['checksums'] == baseline[0]['checksums'] for r in baseline + candidate))
    observed=result['pause_aim']['observed_max_p99_ns']
    result['pause_aim']['passed'] = observed is not None and observed <= budgets['pause_p99_aim_ns']
    result['release_pass'] = all(result[k]['passed'] for k in ('throughput','cpu','footprint','rss','pause_aim','bounded_wait','fallbacks','differential','hard_cap'))
    return result


def elasticity(points):
    """Log/log pause-vs-live-heap slope, bootstrap by process at each heap size."""
    if len(points) < 3 or any(size <= 0 or len(runs) < 5 or any(v <= 0 for v in runs) for size,runs in points):
        raise ValueError('elasticity needs three heap sizes and five positive process samples each')
    points=sorted(points)
    if len({size for size,_ in points}) != len(points): raise ValueError('duplicate heap size')
    x=[math.log(size) for size,_ in points]; mean=statistics.mean(x)
    denominator=sum((v-mean)**2 for v in x)
    def slope(values):
        y=[math.log(v) for v in values]; ym=statistics.mean(y)
        return sum((a-mean)*(b-ym) for a,b in zip(x,y))/denominator
    rng=random.Random(1)
    slopes=sorted(slope([statistics.median(rng.choices(runs,k=len(runs))) for _,runs in points]) for _ in range(2000))
    return dict(slope=slope([statistics.median(runs) for _,runs in points]), ci95=[slopes[49],slopes[1949]], processes_per_size=[len(runs) for _,runs in points])


def scaling_results(cases, variants, results, repeats, maximum):
    """Index once; per-phase slopes cannot be hidden by another larger pause."""
    groups=collections.defaultdict(list)
    for result in results:
        if result.get('status')=='measured' and 'case' in result and 'variant' in result:
            groups[(result['case'],result['variant'])].append(result['summary'])
    output={}
    for shape in ('linked','fanout'):
        selected=[c for c in cases if c['id'].startswith('heap_'+shape+'_')]
        if len(selected)<3:continue
        for variant, config in variants.items():
            rows=[(c['live_bytes'],groups[(c['id'],variant)]) for c in selected]
            if any(len(samples)!=repeats for _,samples in rows):continue
            if any(not r['combined_old_cycle_pause_ns'] for _,samples in rows for r in samples):continue
            individual=[(size,[max(q['p99'] for q in r['pause_ns'].values()) for r in samples]) for size,samples in rows]
            combined=[(size,[r['combined_old_cycle_pause_ns']['p99'] for r in samples]) for size,samples in rows]
            reasons=sorted({reason for _,samples in rows for r in samples for reason in r['pause_ns']})
            phases={};counts={}
            for reason in reasons:
                points=[(size,[r['pause_ns'][reason]['p99'] for r in samples if reason in r['pause_ns']]) for size,samples in rows]
                counts[reason]=[len(values) for _,values in sorted(points)]
                if all(len(values)==repeats for _,values in points):phases[reason]=elasticity(points)
            required=[] if config.get('WILLOW_GC_CONCURRENT_MARK')=='0' else ['1','2']
            value=dict(individual=elasticity(individual),combined=elasticity(combined),phases=phases,
                       phase_processes_per_size=counts,required_phases=required)
            value['passed']=set(required)<=phases.keys() and all(
                v['ci95'][1]<maximum for v in [value['individual'],value['combined'],*phases.values()])
            output[shape+':'+variant]=value
    return output


def calibrate(baseline, budgets):
    # Freeze distributions before the candidate. Candidate numbers never tune gates.
    return dict(schema=1, budgets=dict(budgets),
                throughput=estimate([r['application_ops_per_second'] for r in baseline]),
                cpu=estimate([r['gc_cpu_seconds_per_gib'] for r in baseline]),
                footprint=estimate([r['peak_committed_bytes'] for r in baseline]),
                rss=estimate([r['peak_rss_bytes'] for r in baseline]),
                pause_p99=estimate([max(q['p99'] for q in r['pause_ns'].values()) for r in baseline]),
                absolute_limits=dict(throughput_min=statistics.median(r['application_ops_per_second'] for r in baseline)*budgets['throughput_min_ratio'], gc_cpu_max=statistics.median(r['gc_cpu_seconds_per_gib'] for r in baseline)*budgets['cpu_max_ratio'], peak_rss_max=statistics.median(r['peak_rss_bytes'] for r in baseline)*budgets['footprint_max_ratio'], peak_committed_max=statistics.median(r['peak_committed_bytes'] for r in baseline)*budgets['footprint_max_ratio']),
                scope='frozen reference-host limits; candidate results never relax budgets')


def digest(path):
    with path.open('rb') as source:
        return hashlib.file_digest(source, 'sha256').hexdigest()


def write_json(path, value):
    path.write_text(json.dumps(value, indent=2, allow_nan=False) + '\n')



def compact_capture(folder):
    """Keep outcomes and streaming hashes; remove only regenerable raw dumps.

    Never erase an unsummarized capture. Failed outcomes remain in result.json
    and are reused as failures on resume, rather than replaced by another trial.
    """
    if not (folder / 'result.json').exists():
        raise ValueError('cannot compact without a saved outcome')
    files = [folder / name for name in
             ('trace.ndjson', 'runtime.ndjson', 'observations.bin')]
    existing = [p for p in files if p.exists()]
    if not existing:
        return
    identity_path = folder / 'raw-files.json'
    identities = json.loads(identity_path.read_text()) if identity_path.exists() else {}
    for path in existing:
        identities[path.name] = dict(bytes=path.stat().st_size, sha256=digest(path))
    write_json(identity_path, identities)
    for path in existing:
        path.unlink()


def merge_trace(folder):
    """Merge disjoint, ordered drains without hiding gaps or reordering a sink.

    Native legacy events keep their order. V4 is a two-way sequence merge with
    checkpoints after events at that sequence. Memory is O(1) records.
    """
    native=folder/'runtime.ndjson'; spool=folder/'observations.bin'
    if not native.exists() and not spool.exists(): return
    def key(e): return (e['sequence'], e['event']=='gc_observation_checkpoint', e.get('progress',0))
    def ordered(records):
        previous=None
        for e in records:
            current=key(e)
            if previous is not None and current<previous: raise ValueError('out-of-order observation sink')
            previous=current
            yield e
    def native_records():
        with native.open() as source:
            for line in source:
                e=json.loads(line)
                if e.get('version')==4: yield e
    def binary_records():
        fields=('sequence timestamp_ns epoch thread role op kind identity aborted progress objects slots bytes metadata_bytes budget_checks yields restarts').split()
        checkpoints=('sequence flags dropped active registered unregistered progress').split()
        with spool.open('rb') as source:
            if source.read(8)!=b'WGC4BIN1': raise ValueError('invalid observation spool header')
            while tag:=source.read(1):
                if tag not in (b'\x01',b'\x02'): raise ValueError('invalid observation spool tag')
                names=fields if tag==b'\x01' else checkpoints
                data=source.read(8*len(names))
                if len(data)!=8*len(names): raise ValueError('truncated observation spool')
                yield dict(version=4,event='gc_observation' if tag==b'\x01' else 'gc_observation_checkpoint',**dict(zip(names,struct.unpack('<'+'Q'*len(names),data))))
    with (folder/'trace.ndjson').open('w') as out:
        with native.open() as source:
            for line in source:
                if json.loads(line).get('version')!=4: out.write(line)
        for e in heapq.merge(ordered(native_records()),ordered(binary_records()),key=key):
            out.write(json.dumps(e,separators=(',',':'))+'\n')

def assist_gates(assisted, unassisted, phase_limit, repeats, ratio):
    """Assist must run in every assisted trial and be shown necessary: either
    every assisted mark window stays within the limit's allowed overshoot
    (max(5%, 16 MiB)) while every unassisted median window exceeds ratio*limit,
    or whole-process peaks/fallbacks separate."""
    exercised=len(assisted)==repeats and all(r['assist_by_mutator_ns'] for r in assisted)
    if not exercised or len(unassisted)!=repeats:
        return exercised,False
    allowed=phase_limit+max(phase_limit*5//100,16<<20)
    bound=phase_limit*ratio
    phase=bool(phase_limit) and allowed<bound and all(0<r['overload_phase_peak_bytes']['max']<=allowed for r in assisted) and all(r['overload_phase_peak_bytes']['p50']>bound for r in unassisted)
    footprint=statistics.median(r['peak_committed_bytes'] for r in unassisted)>ratio*statistics.median(r['peak_committed_bytes'] for r in assisted)
    fallback=all(r['stop_coverage']['by_reason'][3]==0 for r in assisted) and all(r['stop_coverage']['by_reason'][3]>0 for r in unassisted)
    return True,phase or footprint or fallback


def analyze_saved(folder, case):
    """Analyze preserved files only; never execute the measured application."""
    try:
        merge_trace(folder)
        return dict(status='measured',summary=parse_run((folder/'stdout.ndjson').read_text(),folder/'trace.ndjson',case))
    except (ValueError,KeyError,TypeError,OSError) as error:
        return dict(status='invalid',error=str(error))


def capture(folder, case, config, binary, timeout):
    folder.mkdir()
    env={k:v for k,v in os.environ.items() if not k.startswith(('WILLOW_','GC_ACCEPTANCE_'))}
    env.update(case.get('env',{}));env.update(config)
    env.update(WILLOW_GC_TRACE_VERSION='4',WILLOW_GC_STOP_TRACE_CAPACITY='16384',WILLOW_GC_MEASURE='1',WILLOW_GC_TRACE_CAPACITY='524288',WILLOW_GC_VERIFY_BARRIER='1',WILLOW_GC_TRACE=str((folder/'runtime.ndjson').resolve()),GC_ACCEPTANCE_EVENT_SPOOL=str((folder/'observations.bin').resolve()),GC_ACCEPTANCE_OPERATIONS=str(case.get('operations',1024)))
    command=[str(binary),case['shape'],str(case['live_bytes']),str(case['roots']),str(case['warmup']),str(case['cycles'])]
    write_json(folder/'invocation.json',dict(command=command,gc_env={k:v for k,v in env.items() if k.startswith(('WILLOW_','GC_ACCEPTANCE_'))}))
    try:
        with (folder/'stdout.ndjson').open('w') as stdout,(folder/'stderr.txt').open('w') as stderr:
            run=subprocess.run(command,env=env,stdout=stdout,stderr=stderr,timeout=timeout)
        if run.returncode: return dict(status='invalid',returncode=run.returncode,error=f'child exited {run.returncode}')
        return dict(status='captured',returncode=0)
    except (ValueError,KeyError,TypeError,OSError,subprocess.TimeoutExpired) as error:
        return dict(status='invalid',error=str(error))


def capture_once(folder, case, config, binary, timeout):
    # Resume only a frozen run. Never replace a failed/partial native execution.
    if folder.exists():
        result=folder/'result.json'
        if result.exists():return json.loads(result.read_text())
        return dict(status='invalid',error='interrupted capture; original files retained')
    return capture(folder,case,config,binary,timeout)


def host_cpu():
    if platform.system()=='Linux':
        try:
            for line in Path('/proc/cpuinfo').read_text().splitlines():
                if line.startswith('model name'):return line.split(':',1)[1].strip()
        except OSError:pass
    return platform.processor() or os.environ.get('PROCESSOR_IDENTIFIER','unavailable')


def missing_workload_coverage(cases, variants):
    """The release matrix cannot silently omit a required workload or axis."""
    by_shape=collections.defaultdict(list)
    for case in cases:by_shape[case['shape']].append(case)
    required=('linked','array','fanout','cyclic','churn','burst','tasks','cancellation',
              'pingpong','fanin','producer_consumer','young','ffi_pins','catalog',
              'fragmented','workers','overload','young_overlap','sweep_overlap')
    missing=['workload:'+shape for shape in required if not by_shape[shape]]
    configurations=dict(stw_reference=('WILLOW_GC_CONCURRENT_MARK','0'),
                        concurrent=('WILLOW_GC_CONCURRENT_MARK','1'),
                        pacer_off=('WILLOW_GC_PACER','0'),assist_off=('WILLOW_GC_ASSIST','0'))
    for name,(key,value) in configurations.items():
        if variants.get(name,{}).get(key)!=value:missing.append('configuration:'+name)
    for shape in ('linked','fanout'):
        sizes=collections.defaultdict(set)
        for case in by_shape[shape]:sizes[case['roots']].add(case['live_bytes'])
        if not any(roots>0 and len(values)>=3 and (64<<20) in values and max(values)>=(1<<30) for roots,values in sizes.items()):
            missing.append(shape+'_fixed_root_heap_scaling')
    roots=collections.defaultdict(set)
    for case in by_shape['linked']:roots[case['live_bytes']].add(case['roots'])
    if not any({1,64,4096}<=values for values in roots.values()):missing.append('root_count_scaling')
    workers={int(c.get('env',{}).get('WILLOW_WORKERS','1')) for c in by_shape['workers']}
    if not {1,5,8}<=workers:missing.append('mutator_counts_1_5_many')
    hard={int(c.get('env',{}).get('WILLOW_WORKERS','1')) for c in by_shape['workers'] if int(c.get('env',{}).get('WILLOW_GC_MEMORY_LIMIT','0'))>0}
    if not {1,2,4,8}<=hard:missing.append('hard_cap_workers_1_2_4_8')
    if not {85,95,105}<={c.get('soft_target_percent') for c in cases}:missing.append('soft_limit_85_95_105')
    for shape in ('tasks','cancellation'):
        if not any(c['operations']*c['cycles']>=2_000_000 for c in by_shape[shape]):missing.append(shape+'_millions_measured')
    return sorted(missing)

def run_suite(args):
    args.manifest = args.manifest.resolve()
    manifest = json.loads(args.manifest.read_text())
    if manifest['schema'] != 1 or manifest['repeats'] < 5:
        raise ValueError('unsupported manifest or fewer than five process runs')
    analysis_workers=getattr(args,'analysis_workers',1)
    if not 1<=analysis_workers<=5: raise ValueError('analysis workers must be 1..5')
    cases = manifest['profiles'][args.profile]
    if len({c['id'] for c in cases})!=len(cases):raise ValueError('duplicate workload case id')
    for c in cases:
        if c['warmup'] < 1 or c['cycles'] < 1:
            raise ValueError('positive warmup and measured cycles required')
    selected_cases=getattr(args,'case',None)
    if selected_cases:
        if not set(selected_cases) <= {c['id'] for c in cases}: raise ValueError('unknown case')
        cases=[c for c in cases if c['id'] in selected_cases]
    binary = args.binary.resolve()
    resume=getattr(args,"resume",False)
    args.output.mkdir(parents=True, exist_ok=resume)
    # Freeze before any child starts. Never capture arbitrary environment/secrets.
    source_paths = sorted((ROOT / 'crates/willow_runtime').rglob('*.rs'))
    source_paths += [ROOT / 'Cargo.lock', ROOT / 'Cargo.toml', ROOT / 'crates/willow_runtime/Cargo.toml', Path(__file__), args.manifest, ROOT/'scripts/gc_stop_trace_check.py', ROOT/'benches/gc_acceptance/observation_check.py']
    source_hashes = {str(p.relative_to(ROOT)): digest(p) for p in source_paths}
    platform_id = platform.system() + '-' + platform.machine()
    frozen = dict(schema=1, manifest=manifest, profile=args.profile, selected_cases=selected_cases, binary_sha256=digest(binary),
                  source_sha256=source_hashes, platform=platform_id, os=platform.platform(),
                  cpu=host_cpu(), logical_cpus=os.cpu_count(),
                  power=args.power, ram_bytes=args.ram_bytes, analysis_workers=analysis_workers,
                  revision=subprocess.check_output(['git', 'rev-parse', 'HEAD'], cwd=ROOT, text=True).strip(),
                  rustc=subprocess.check_output(['rustc', '-Vv'], text=True), build_flags=args.build_flags,
                  platforms={p: ('measured here' if p == platform_id else 'unverified') for p in PLATFORMS})
    if resume:
        if json.loads((args.output/'frozen.json').read_text())!=frozen:raise ValueError('resume requires identical frozen source, binary, inputs and host')
    else:write_json(args.output / 'frozen.json', frozen)
    if resume:(args.output/'runs.ndjson').write_text('')
    results = []
    soft_calibrations={}; calibration_runs=[]
    for case in cases:
        if 'soft_target_percent' in case:
            key=tuple(case[k] for k in ('shape','live_bytes','roots','warmup','cycles','operations'))
            if key not in soft_calibrations:
                probe=dict(case,env={k:v for k,v in case.get('env',{}).items() if k!='WILLOW_GC_MEMORY_LIMIT_BYTES'})
                samples=[]
                # An identical unlimited workload is calibrated before any limit
                # variant; all five processes and failures remain in the evidence.
                for repeat in range(manifest['repeats']):
                    folder=args.output/f'soft-calibration-{len(soft_calibrations)}-{repeat}'
                    row=capture_once(folder,probe,manifest['variants']['concurrent'],binary,manifest['timeout_seconds'])
                    if row['status']=='captured':row.update(analyze_saved(folder,probe))
                    write_json(folder/'result.json',row);compact_capture(folder);calibration_runs.append(row)
                    if row['status']=='measured':samples.append(row['summary'])
                if len(samples)!=manifest['repeats']:raise ValueError('soft-limit calibration incomplete; preserved failed trials')
                soft_calibrations[key]=statistics.median(max(r['memory'][k]['steady'] for k in ('reserved_bytes','rss_bytes')) for r in samples)
            case['env']['WILLOW_GC_MEMORY_LIMIT_BYTES']=str(math.ceil(soft_calibrations[key]*100/case['soft_target_percent']))
            write_json(args.output/(case['id']+'-limit-calibration.json'),dict(baseline_steady_footprint=soft_calibrations[key],target_percent=case['soft_target_percent'],resolved_limit=int(case['env']['WILLOW_GC_MEMORY_LIMIT_BYTES']),rule='median of five unlimited same-input concurrent runs; maximum of steady managed reservation and RSS'))
        names = list(manifest['variants'])
        if names[0] != 'stw_reference': raise ValueError('reference must run before candidates')
        for name in names:
            config=manifest['variants'][name]
            baseline=[r['summary'] for r in results if r['case']==case['id'] and r['variant']=='stw_reference' and r['status']=='measured']
            if name != 'stw_reference' and len(baseline)==manifest['repeats']:
                calibration=calibrate(baseline,manifest['budgets'])
                calibration_path=args.output / (case['id']+'-calibration.json')
                if not calibration_path.exists(): write_json(calibration_path,calibration)
            batch=[]
            for repeat in range(manifest['repeats']):
                folder = args.output / f"{case['id']}-{repeat}-{name}"
                record=capture_once(folder,case,config,binary,manifest['timeout_seconds'])
                record.update(case=case['id'],repeat=repeat,variant=name)
                batch.append((folder,record))
                write_json(folder / 'result.json', record)
            # No analyzer overlaps a measured child. Parallelism is confined to
            # the five preserved traces after the entire variant has exited.
            pending=[(folder,record) for folder,record in batch if record['status']=='captured']
            if analysis_workers==1:
                analyzed=[analyze_saved(folder,case) for folder,_ in pending]
            else:
                with concurrent.futures.ProcessPoolExecutor(max_workers=analysis_workers) as pool:
                    analyzed=list(pool.map(analyze_saved,[folder for folder,_ in pending],[case]*len(pending)))
            for (_,record),outcome in zip(pending,analyzed):record.update(outcome)
            for folder,record in batch:
                write_json(folder / 'result.json', record)
                compact_capture(folder)
                results.append(record)
                with (args.output / 'runs.ndjson').open('a') as output:
                    output.write(json.dumps(record, allow_nan=False) + '\n')
                print(json.dumps({k:v for k,v in record.items() if k!='summary'}), flush=True)
    comparisons = {}
    for case in cases:
        groups = {name: [r['summary'] for r in results if r['case'] == case['id'] and r['variant'] == name and r['status'] == 'measured'] for name in manifest['variants']}
        # No survivor-only comparisons after a failure.
        if all(len(v) == manifest['repeats'] for v in groups.values()):
            comparisons[case['id']] = {name: comparison(groups['stw_reference'], runs, manifest['budgets']) for name,runs in groups.items() if name != 'stw_reference'}
    unchanged = all(digest(ROOT / name) == value for name,value in source_hashes.items()) and digest(binary) == frozen['binary_sha256']
    elasticities=scaling_results(cases,manifest['variants'],results,manifest['repeats'],manifest['budgets']['elasticity_max'])
    fault_evidence=getattr(args,'fault_evidence',None)
    faults=json.loads(fault_evidence.read_text()) if fault_evidence else {}
    fault_gate=faults.get('diagnostics_pass') is True and faults.get('binary_sha256')==frozen['binary_sha256']
    def runs(case,variant='concurrent'):
        return [r['summary'] for r in results if r['case']==case and r['variant']==variant and r['status']=='measured']
    assisted,unassisted=runs('overload'),runs('overload','assist_off')
    overload_case=next((c for c in cases if c['id']=='overload'),{})
    assist_exercised,assist_benefit=assist_gates(assisted,unassisted,overload_case.get('assist_phase_limit_bytes',0),manifest['repeats'],manifest['budgets']['footprint_max_ratio'])
    overlap=runs('young_overlap')
    coverage_gates=dict(assist_exercised=assist_exercised,assist_required_benefit=assist_benefit,
        end_cycle_burst=len(runs('burst'))==manifest['repeats'] and all(r['burst_allocation_overlap']>0 and r['burst_mutation_overlap']>0 for r in runs('burst')),
        short_lived_dominance=len(runs('churn'))==manifest['repeats'] and all(r['allocation_to_retained_ratio'] is not None and r['allocation_to_retained_ratio']>=4 for r in runs('churn')),
        forced_minor_overlap=len(overlap)==manifest['repeats'] and all(r['concurrent_minor_cycles']>0 for r in overlap),
        channel_gc_overlap=len(runs('producer_consumer'))==manifest['repeats'] and all(r['channel_overlap']>0 for r in runs('producer_consumer')),
        sweep_allocation_overlap=len(runs('sweep_overlap'))==manifest['repeats'] and all(r['sweep_overlap']>0 for r in runs('sweep_overlap')))
    coverage_gates['soft_limit_states']=all(len(runs('soft_'+str(percent)))==manifest['repeats'] and all((r['soft_footprint_ratio']>1 if percent>100 else abs(r['soft_footprint_ratio']-percent/100)<=.05) for r in runs('soft_'+str(percent))) for percent in (85,95,105))
    missing=missing_workload_coverage(cases,manifest['variants'])
    evaluated=[r['release_pass'] for group in comparisons.values() for name,r in group.items() if name in manifest['release_variants']]
    complete=unchanged and all(r['status']=='measured' for r in results)
    write_json(args.output / 'summary.json', dict(schema=1, release_pass=complete and not missing and all(coverage_gates.values()) and fault_gate and not selected_cases and args.profile=='nightly' and bool(evaluated) and all(evaluated) and len(elasticities)==2*len(manifest['variants']) and all(v['passed'] for k,v in elasticities.items() if k.split(':')[1] in manifest['release_variants']), source_unchanged=unchanged,
        measurement_complete=unchanged and all(r['status']=='measured' for r in results),
        calibration_runs=calibration_runs, runtime_fault_gate=fault_gate, coverage_gates=coverage_gates, missing_release_coverage=missing, comparisons=comparisons, elasticities=elasticities, selected_cases=selected_cases,
        platforms=frozen['platforms']))
    return 0 if unchanged and all(r['status']=='measured' for r in results) else 2


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--manifest', type=Path, default=Path(__file__).with_name('matrix.json'))
    parser.add_argument('--binary', type=Path, required=True)
    parser.add_argument('--output', type=Path, required=True)
    parser.add_argument('--analysis-workers',type=int,default=1,help='1..5 offline analyzers; never overlap measured processes')
    parser.add_argument('--fault-evidence',type=Path,help='Same-binary real runtime injection summary required for release approval')
    parser.add_argument('--resume',action='store_true',help='Resume identical frozen run; retain invalid/interrupted trials without replacement')
    parser.add_argument('--profile', choices=['pr', 'nightly'], default='pr')
    parser.add_argument('--case',action='append',help='Focused case selection; cannot certify the full suite')
    parser.add_argument('--power', required=True, help='predeclared power/governor state, or unavailable')
    parser.add_argument('--ram-bytes', type=int, required=True)
    parser.add_argument('--build-flags', required=True)
    args = parser.parse_args()
    raise SystemExit(run_suite(args))


if __name__ == '__main__':
    main()
