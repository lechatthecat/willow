"""Focused deterministic acceptance-consumer tests; no collector suite runs."""
import copy
import json
import random
import unittest
import tempfile
from pathlib import Path
import suite
import faults


def fixture():
    events, before, after = suite.OBS.fixture()
    stop_before = dict(version=2, flags=1, reset_generation=0, last_sequence=0,
                       active_sequence=0, events_dropped=0, events_pending=0,
                       requests=[0]*4, completed=[0]*4, aborted=[0]*4,
                       trace_errors=0, completed_cycles=0)
    stop_after = dict(stop_before, last_sequence=1, requests=[1,0,0,0], completed=[1,0,0,0], completed_cycles=1)
    events += [dict(version=2,event='gc_stop_request',sequence=1,reset_generation=0,reason=1,timestamp_ns=10),
               dict(version=2,event='gc_stop_release',sequence=1,reset_generation=0,reason=1,timestamp_ns=30,stopped_ns=15,outcome=0)]
    records = [dict(schema=1,event='endpoint',label='before',timestamp_ns=0,observation=before,stop=stop_before)]
    for i in range(2):
        sample = dict(schema=1,event='sample',index=i,elapsed_ns=40,operations=4,checksum=7,timestamp_ns=5+40*i,
                      allocation_bytes=100+100*i,committed_bytes=1000,reserved_bytes=1000,rss_bytes=2000,rss_valid=1,
                      live_bytes=500,quarantine_bytes=50,reclaimed_bytes=100*i,copied_bytes=10*i,promoted_bytes=20*i,
                      fragmentation_bytes=50,peak_committed_bytes=1000,peak_rss_bytes=2000,wasted_bytes=i*10,barrier_ops=2*i,gc_cpu_ns=10*i,cpu_missing_cycles=0,goal_bytes=1000,cycles=i)
        records.append(sample)
    records.append(dict(schema=1,event='endpoint',label='after',timestamp_ns=50,observation=after,stop=stop_after))
    return records, events, dict(warmup=1,cycles=1,checksum=7)


def parse(records, events, case):
    return suite.parse_run('\n'.join(map(json.dumps, records)), '\n'.join(map(json.dumps, events)), case)


class Statistics(unittest.TestCase):
    def test_compaction_preserves_failure_identity_and_requires_outcome(self):
        with tempfile.TemporaryDirectory() as name:
            folder = Path(name)
            raw = folder / 'trace.ndjson'
            raw.write_text('broken trace\n')
            expected = suite.digest(raw)
            with self.assertRaises(ValueError): suite.compact_capture(folder)
            self.assertTrue(raw.exists())
            suite.write_json(folder / 'result.json', dict(status='invalid', error='lost'))
            suite.compact_capture(folder)
            self.assertFalse(raw.exists())
            self.assertEqual(json.loads((folder / 'raw-files.json').read_text())['trace.ndjson']['sha256'], expected)
            self.assertEqual(suite.capture_once(folder, {}, {}, None, 0)['status'], 'invalid')
            suite.compact_capture(folder)
            self.assertTrue((folder / 'raw-files.json').exists())

    def test_percentiles_nearest_rank_tails(self):
        q=suite.quantiles(list(range(1,1001)))
        self.assertEqual([q[k] for k in ('p50','p95','p99','p99.9','max')],[500,950,990,999,1000])

    def test_empty_quantiles_unavailable(self):
        self.assertIsNone(suite.quantiles([]))

    def test_five_process_minimum(self):
        with self.assertRaises(ValueError): suite.estimate([1]*4)

    def test_confidence_interval_deterministic(self):
        self.assertEqual(suite.estimate([1,2,3,4,5]),suite.estimate([1,2,3,4,5]))
        self.assertEqual(suite.estimate([3]*5)['ci95'],[3,3])

    def test_union_nested_and_adjacent(self):
        self.assertEqual(suite.union([(2,3),(1,5),(5,8),(10,11)]),[[1,8],[10,11]])

    def test_union_backwards_rejected(self):
        with self.assertRaises(ValueError): suite.union([(2,1)])

    def test_mmu_empty_full_and_too_short(self):
        self.assertEqual(suite.mmu([],0,100,10),1)
        self.assertEqual(suite.mmu([(0,100)],0,100,10),0)
        self.assertIsNone(suite.mmu([],0,9,10))

    def test_mmu_clips_and_deduplicates(self):
        self.assertEqual(suite.mmu([(-10,5),(2,5)],0,100,10),.5)

    def test_mmu_differential_against_exhaustive_windows(self):
        rng=random.Random(1)
        for count in (1,4,16,64):
            spans=[sorted(rng.sample(range(101),2)) for _ in range(count)]
            covered={t for a,b in spans for t in range(a,b)}
            for window in (1,10,30,100):
                expected=1-max(sum(t in covered for t in range(s,s+window)) for s in range(101-window))/window
                self.assertAlmostEqual(suite.mmu(spans,0,100,window),expected)

    def test_union_increasing_fragmented_sizes(self):
        for n in (16,64,256,1024):
            self.assertEqual(len(suite.union([(i*2,i*2+1) for i in range(n)])),n)


class Consumer(unittest.TestCase):
    def test_complete_measurement_never_claims_release(self):
        result=parse(*fixture())
        self.assertTrue(result['measurement_complete'])
        self.assertNotIn('release_pass',result)
        self.assertEqual(result['pause_ns']['1']['max'],20)

    def test_three_axes_and_counter_deltas(self):
        result=parse(*fixture())
        self.assertEqual(result['application_ops_per_second'],1e8)
        self.assertEqual(result['memory']['committed_bytes']['sampled_peak'],1000)
        self.assertEqual(result['reclaimed_bytes_per_cycle'],100)
        self.assertEqual(result['barrier_ops_per_application_op'],.5)

    def test_warmup_latency_excluded(self):
        r,e,c=fixture();r[1]['elapsed_ns']=999999
        self.assertEqual(parse(r,e,c)['latency_ns']['max'],40)

    def test_checksum_failure(self):
        r,e,c=fixture();r[2]['checksum']=8
        with self.assertRaises(ValueError): parse(r,e,c)

    def test_missing_duplicate_endpoint(self):
        for duplicate in (False,True):
            r,e,c=fixture()
            if duplicate:r.append(copy.deepcopy(r[-1]))
            else:r.pop()
            with self.assertRaises(ValueError):parse(r,e,c)

    def test_missing_duplicate_sample(self):
        for duplicate in (False,True):
            r,e,c=fixture()
            if duplicate:r.insert(1,copy.deepcopy(r[1]))
            else:r.pop(1)
            with self.assertRaises(ValueError):parse(r,e,c)

    def test_unknown_schema_and_record(self):
        for key,value in [('schema',2),('event','unknown')]:
            r,e,c=fixture();r[1][key]=value
            with self.assertRaises(ValueError):parse(r,e,c)

    def test_counter_regression(self):
        r,e,c=fixture();r[2]['allocation_bytes']=0
        with self.assertRaises(ValueError):parse(r,e,c)

    def test_missing_cpu_and_rss(self):
        for key,value in [('rss_valid',0),('cpu_missing_cycles',1)]:
            r,e,c=fixture();r[2][key]=value
            with self.assertRaises(ValueError):parse(r,e,c)

    def test_zero_progress_and_elapsed(self):
        for key in ('elapsed_ns','operations'):
            r,e,c=fixture();r[2][key]=0
            with self.assertRaises(ValueError):parse(r,e,c)

    def test_invalid_numeric_fields(self):
        for value in (-1,False,float('nan'),1.5):
            r,e,c=fixture();r[2]['elapsed_ns']=value
            with self.assertRaises(ValueError):parse(r,e,c)

    def test_saturated_cpu_cannot_pass_release_comparison(self):
        records, events, case = ReleaseAccounting.complete_cycles_fixture()
        baseline = parse(records, events, case)
        budgets = dict(throughput_min_ratio=.97, cpu_max_ratio=1, footprint_max_ratio=1.1,
                       pause_p99_aim_ns=2_000_000, local_wait_max_ns=100)
        for value in (2**64-1, 2**64, 2**80):
            invalid = copy.deepcopy(records)
            for sample in invalid[1:-1]:
                sample['gc_cpu_ns'] = value
            with self.subTest(value=value), self.assertRaisesRegex(ValueError, 'application scalar'):
                candidate = parse(invalid, events, case)
                suite.comparison([baseline]*5, [candidate]*5, budgets)

    def test_saturated_sample_measurements_rejected_including_warmup(self):
        records, events, case = ReleaseAccounting.complete_cycles_fixture()
        fields = set(records[1]) - {'event', 'schema', 'index', 'timestamp_ns', 'checksum', 'workload_checksum'}
        for field in fields:
            for index in (1, 2):
                for value in (2**64-1, 2**64):
                    invalid = copy.deepcopy(records)
                    invalid[index][field] = value
                    with self.subTest(field=field, index=index, value=value), self.assertRaises(ValueError):
                        parse(invalid, events, case)
        records, events, case = fixture()
        extra = dict(records[1], index=1, timestamp_ns=6)
        records[2]['index'] = 2
        records.insert(2, extra)
        case.update(warmup=2)
        records[1]['gc_cpu_ns'] = 2**64-1  # Earlier than the delta's initial sample.
        with self.assertRaisesRegex(ValueError, 'application scalar'):
            parse(records, events, case)

    def test_full_width_checksums_are_not_saturation_counters(self):
        records, events, case = fixture()
        case.update(checksum=2**64-1, workload_checksum=2**64-1)
        for sample in records[1:-1]:
            sample.update(checksum=2**64-1, workload_checksum=2**64-1)
        self.assertTrue(parse(records, events, case)['measurement_complete'])
        records[1]['checksum'] = 2**64
        case['checksum'] = 2**64
        with self.assertRaises(ValueError):
            parse(records, events, case)

    def test_out_of_capture_observation_waits_cannot_disappear(self):
        records, events, case = fixture()
        for record in records:
            record['timestamp_ns'] *= 1_000_000
        for event in events:
            if 'timestamp_ns' in event:
                event['timestamp_ns'] *= 1_000_000
            if 'stopped_ns' in event:
                event['stopped_ns'] *= 1_000_000
        control = parse(records, events, case)
        self.assertEqual(control['local_stall_ns']['4']['max'], 20_000_000)
        budgets = dict(throughput_min_ratio=.97, cpu_max_ratio=1, footprint_max_ratio=1.1,
                       pause_p99_aim_ns=30_000_000, local_wait_max_ns=5_000_000)
        self.assertFalse(suite.comparison([control]*5, [control]*5, budgets)['bounded_wait']['passed'])
        for event in events:
            if event.get('event') == 'gc_observation':
                event['timestamp_ns'] += 60_000_000
        with self.assertRaisesRegex(ValueError, 'observation timestamp'):
            parse(records, events, case)

    def test_observation_capture_checks_cover_every_role_and_operation(self):
        for role in (0, 1):
            records, events, case = fixture()
            for event in events:
                if event.get('event') == 'gc_observation' and event['thread'] == 2:
                    event['role'] = role
            self.assertTrue(parse(records, events, case)['measurement_complete'])
            for index, event in enumerate(events):
                if event.get('event') != 'gc_observation':
                    continue
                for timestamp in (-1, 51, 2**64-1, 2**64):
                    invalid = copy.deepcopy(events)
                    invalid[index]['timestamp_ns'] = timestamp
                    with self.subTest(role=role, op=event['op'], timestamp=timestamp), self.assertRaisesRegex(ValueError, 'observation timestamp'):
                        parse(records, invalid, case)
        records, events, case = fixture()
        records[0]['timestamp_ns'] = 1  # Registration at zero is outside capture, in warmup.
        with self.assertRaisesRegex(ValueError, 'observation timestamp'):
            parse(records, events, case)

    def test_valid_observation_measurement_boundary_clipping(self):
        records, events, case = fixture()
        records[1]['timestamp_ns'] = 22
        records[2]['timestamp_ns'] = 35
        result = parse(records, events, case)
        self.assertTrue(result['measurement_complete'])
        self.assertEqual(result['local_stall_ns']['4']['max'], 13)
        self.assertEqual(result['local_stall_ns']['2']['max'], 8)
        self.assertEqual(result['assist_by_mutator_ns']['1']['max'], 3)
        self.assertEqual(result['max_application_mutators'], 2)
        self.assertEqual(set(result['mmu_by_mutator']), {'1', '2'})

    def test_stops_outside_capture_are_rejected_before_window_filter(self):
        for begin, stopped, end, capture_start in ((60, 65, 10_000_060, 0),
                                                  (1, 2, 3, 4),
                                                  (3, 4, 10, 4),
                                                  (40, 45, 60, 0)):
            records, events, case = fixture()
            records[0]['timestamp_ns'] = capture_start
            events[-2]['timestamp_ns'] = begin
            events[-1].update(timestamp_ns=end, stopped_ns=stopped)
            with self.subTest(begin=begin, end=end), self.assertRaisesRegex(ValueError, 'capture'):
                parse(records, events, case)

    def test_stops_within_capture_keep_window_clipping(self):
        for begin, stopped, end in ((0, 1, 4), (1, 2, 10), (40, 45, 50), (46, 47, 50)):
            records, events, case = fixture()
            events[-2]['timestamp_ns'] = begin
            events[-1].update(timestamp_ns=end, stopped_ns=stopped)
            result = parse(records, events, case)
            self.assertTrue(result['measurement_complete'])
            self.assertNotIn('1', result['pause_ns'])

    def test_sample_timestamp_range_and_capture_bounds(self):
        for index in (1, 2):
            for value in (None, True, -1, 1.5, '45', 2**64-1, 2**64, 51):
                records, events, case = fixture()
                if value is None:
                    del records[index]['timestamp_ns']
                else:
                    records[index]['timestamp_ns'] = value
                with self.subTest(index=index, value=value), self.assertRaises(ValueError):
                    parse(records, events, case)
        records, events, case = fixture()
        records[0]['timestamp_ns'] = 6
        with self.assertRaisesRegex(ValueError, 'sample timestamp'):
            parse(records, events, case)

    def test_all_sample_timestamps_are_strictly_ordered(self):
        for warmup in (1, 2):
            for middle in (4, 5, 46):
                records, events, case = fixture()
                extra = dict(records[1], index=1, timestamp_ns=middle)
                records[2]['index'] = 2
                records.insert(2, extra)
                case.update(warmup=warmup, cycles=3-warmup)
                with self.subTest(warmup=warmup, middle=middle), self.assertRaisesRegex(ValueError, 'sample timestamp'):
                    parse(records, events, case)
        records, events, case = fixture()
        records[1]['timestamp_ns'] = 0
        records[2]['timestamp_ns'] = 50
        self.assertTrue(parse(records, events, case)['measurement_complete'])

    def test_injected_trace_loss(self):
        r,e,c=fixture();e.pop(3)
        with self.assertRaises(ValueError):parse(r,e,c)

    def test_injected_short_stop_not_hidden(self):
        r,e,c=fixture();e[-1]['timestamp_ns']=16
        self.assertEqual(parse(r,e,c)['pause_ns']['1']['max'],6)

    def test_injected_lock_stall_visible(self):
        result=parse(*fixture())
        self.assertEqual(result['local_stall_ns']['4']['max'],20)
        self.assertEqual(result['observation_coverage']['global_gc_wait_union_ns'],10)

    def test_assist_attribution_not_added_twice(self):
        result=parse(*fixture())
        self.assertEqual(result['assist_by_mutator_ns']['1']['max'],3)

    def test_assist_benefit_requires_every_trial_to_separate(self):
        def run(peak, p50, assist=True, committed=1000, recovery=1):
            return dict(assist_by_mutator_ns={'1':{'max':1}} if assist else {},
                        overload_phase_peak_bytes=dict(max=peak,p50=p50),
                        peak_committed_bytes=committed,stop_coverage=dict(by_reason=[0,0,0,recovery]))
        M=1<<20;L=640*M
        # 640 MiB limit: assisted may overshoot by max(5%, 16 MiB)=32 MiB; unassisted must exceed 704 MiB.
        on,off=[run(L+32*M,L)]*5,[run(1040*M,1000*M,False)]*5
        self.assertEqual(suite.assist_gates(on,off,L,5,1.1),(True,True))
        # One assisted window beyond the allowed overshoot fails the phase route.
        self.assertEqual(suite.assist_gates(on[:4]+[run(L+32*M+1,L)],off,L,5,1.1),(True,False))
        self.assertEqual(suite.assist_gates(on[:4]+[run(int(L*1.05)+M,L)],off,L,5,1.1),(True,False))
        # One unassisted median within ratio*limit means Assist was not required.
        self.assertEqual(suite.assist_gates(on,off[:4]+[run(704*M,704*M,False)],L,5,1.1),(True,False))
        self.assertEqual(suite.assist_gates(on[:4],off,L,5,1.1),(False,False))
        self.assertEqual(suite.assist_gates(on[:4]+[run(L,L,False)],off,L,5,1.1),(False,False))
        self.assertEqual(suite.assist_gates(on,off[:4],L,5,1.1),(True,False))
        self.assertEqual(suite.assist_gates(on,off,0,5,1.1),(True,False))
        # Small limits: 16 MiB floor exceeds the ratio bound, so no phase-route gap exists.
        self.assertEqual(suite.assist_gates([run(100*M,100*M)]*5,[run(200*M,200*M,False)]*5,100*M,5,1.1),(True,False))
        # Whole-process footprint or fallback separation remain independent routes.
        self.assertEqual(suite.assist_gates(on,[run(L,L,False,committed=1200)]*5,0,5,1.1),(True,True))
        self.assertEqual(suite.assist_gates([run(L,L,recovery=0)]*5,off,0,5,1.1),(True,True))

    def test_fallback_preserved(self):
        r,e,c=fixture()
        for event in e[-2:]:event['reason']=4
        r[-1]['stop'].update(requests=[0,0,0,1],completed=[0,0,0,1])
        self.assertEqual(parse(r,e,c)['stop_coverage']['by_reason'][3],1)

    def test_r1_r2_classified_recovery_is_observable_not_zero_stw_gate(self):
        one = parse(*fixture())
        recovery = copy.deepcopy(one)
        recovery['stop_coverage']['by_reason'][3] = 2
        budgets = dict(throughput_min_ratio=.97, cpu_max_ratio=1,
                       footprint_max_ratio=1.1, pause_p99_aim_ns=2000000,
                       local_wait_max_ns=100)
        result = suite.comparison([one]*5, [recovery]*5, budgets)
        self.assertEqual(result['fallbacks']['count'], 10)
        self.assertEqual(result['fallbacks']['scope'], 'diagnostic')
        self.assertTrue(result['release_pass'])
        strict = suite.comparison([one]*5, [recovery]*5,
                                  dict(budgets, fallback_max_count=0))
        self.assertFalse(strict['fallbacks']['passed'])
        self.assertFalse(strict['release_pass'])
        recovery['gc_cpu_seconds_per_gib'] *= 1.01
        self.assertFalse(suite.comparison([one]*5, [recovery]*5, budgets)['release_pass'])

    def test_missing_stop_rejected(self):
        r,e,c=fixture();e.pop()
        with self.assertRaises(ValueError):parse(r,e,c)

    def test_transport_sink_error_rejected(self):
        r,e,c=fixture();r[-1]['observation']['trace_errors']=1
        with self.assertRaises(ValueError):parse(r,e,c)

    def test_aborted_interval_rejected(self):
        r,e,c=fixture();e[6]['aborted']=1
        with self.assertRaises(ValueError):parse(r,e,c)

    def test_no_universal_budget_claim(self):
        one=parse(*fixture());budgets=dict(throughput_min_ratio=.97,cpu_max_ratio=1,footprint_max_ratio=1.1,pause_p99_aim_ns=2000000,local_wait_max_ns=100)
        result=suite.comparison([one]*5,[one]*5,budgets)
        self.assertTrue(result['throughput']['passed'])
        self.assertTrue(result['release_pass'])

    def test_ratio_cpu_footprint_and_wait_regressions(self):
        one=parse(*fixture());two=copy.deepcopy(one)
        two['application_ops_per_second']*=.96
        two['gc_cpu_seconds_per_gib']*=1.01
        two['peak_committed_bytes']*=1.11
        budgets=dict(throughput_min_ratio=.97,cpu_max_ratio=1,footprint_max_ratio=1.1,pause_p99_aim_ns=2000000,local_wait_max_ns=10)
        result=suite.comparison([one]*5,[two]*5,budgets)
        for key in ('throughput','cpu','footprint','bounded_wait'):self.assertFalse(result[key]['passed'])


class ReleaseAccounting(unittest.TestCase):
    def test_indexed_mmu_matches_exact_for_many_short_lifetimes(self):
        rng=random.Random(9)
        for n in (0,1,16,128):
            spans=[sorted(rng.sample(range(1001),2)) for _ in range(n)]
            index=suite.MmuIndex(spans)
            for _ in range(100):
                a,b=sorted(rng.sample(range(1001),2));w=rng.choice((1,10,100))
                self.assertEqual(index.minimum(a,b,w),suite.mmu(spans,a,b,w))
            self.assertLessEqual(len(index.windows),3)

    def test_elasticity_constant_and_heap_proportional(self):
        for exponent in (0,.25,1):
            result=suite.elasticity([(n,[n**exponent]*5) for n in (64,256,1024)])
            self.assertAlmostEqual(result['slope'],exponent)
            for bound in result['ci95']:self.assertAlmostEqual(bound,exponent)

    def test_elasticity_rejects_missing_repetitions_and_sizes(self):
        for data in ([(64,[1]*5)],[(n,[1]*4) for n in (64,256,1024)],[(n,[0]*5) for n in (64,256,1024)]):
            with self.assertRaises(ValueError):suite.elasticity(data)

    def test_phase_accounting_and_combined_old_pauses(self):
        r,e,c=fixture()
        e += [dict(version=1,event='gc_start',epoch=1,kind=2,timestamp_ns=6,heap_bytes=1000),
              dict(version=1,event='gc_end',epoch=1,kind=2,timestamp_ns=44,marked_bytes=512,mark_ns=20,reclaimed_bytes=120,
                   pause_ns=20,scanned_bytes=512,root_scan_bytes=8,heap_bytes=880)]
        e += [dict(version=3,event=stage,epoch=1,timestamp_ns=t) for stage,t in [('MarkComplete',30),('SweepComplete',42),('Quiescent',44)]]
        result=parse(r,e,c)
        self.assertEqual(result['combined_old_cycle_pause_ns']['max'],20)
        self.assertEqual(result['mark_bytes_per_second'],512e9/20)
        self.assertEqual(result['sweep_reclaimed_bytes_per_second'],120e9/12)
        self.assertEqual(result['wasted_bytes'],10)
        self.assertEqual(result['peak_committed_bytes'],1000)
        e.pop()
        with self.assertRaises(ValueError):parse(r,e,c)

    @staticmethod
    def complete_cycles_fixture():
        records, events, case = fixture()
        case['require_cycles'] = True
        records[-1]['stop']['completed_cycles'] = 2
        records[2]['cycles'] = 2
        for sample in records[1:-1]:
            sample.update(sweep_objects=1, sweep_bytes=1, sweep_ns=1)
        for epoch, start, mark, sweep, end in [(1, 6, 10, 14, 16), (2, 31, 35, 40, 44)]:
            events.extend([
                dict(version=1, event='gc_start', epoch=epoch, kind=2, timestamp_ns=start, heap_bytes=1000),
                dict(version=1, event='gc_end', epoch=epoch, kind=2, timestamp_ns=end,
                     marked_bytes=512, mark_ns=4, reclaimed_bytes=120,
                     pause_ns=1, scanned_bytes=512, root_scan_bytes=8, heap_bytes=880),
            ])
            events.extend(dict(version=3, event=stage, epoch=epoch, timestamp_ns=time)
                          for stage, time in [('MarkComplete', mark), ('SweepComplete', sweep), ('Quiescent', end)])
        return records, events, case

    def test_complete_epoch_loss_rejected_against_independent_counters(self):
        records, events, case = self.complete_cycles_fixture()
        self.assertTrue(parse(records, events, case)['measurement_complete'])
        for missing in (1, 2):
            with self.subTest(epoch=missing):
                truncated = [event for event in events
                             if not (event.get('epoch') == missing and event.get('version') in (1, 3))]
                with self.assertRaisesRegex(ValueError, 'cycle coverage'):
                    parse(records, truncated, case)

    def test_cycle_coverage_includes_warmup_and_boundary_crossing_cycles(self):
        for warmup in (False, True):
            records, events, case = self.complete_cycles_fixture()
            times = {'gc_start': 1, 'MarkComplete': 2, 'SweepComplete': 3, 'gc_end': 4, 'Quiescent': 4}
            for event in events:
                if event.get('epoch') == 1 and event.get('version') in (1, 3) and (warmup or event['event'] == 'gc_start'):
                    event['timestamp_ns'] = times[event['event']]
            if warmup:
                records[1]['cycles'] = 1
            result = parse(records, events, case)
            self.assertTrue(result['measurement_complete'])
            self.assertEqual(result['combined_old_cycle_pause_ns']['count'], 1)

    @staticmethod
    def cycle_window_fixture(window):
        records, events, case = ReleaseAccounting.complete_cycles_fixture()
        times = {'gc_start': 1, 'MarkComplete': 2, 'SweepComplete': 3, 'gc_end': 4, 'Quiescent': 4}
        for event in events:
            if event.get('epoch') == 1 and event.get('version') in (1, 3):
                if window == 'warmup' or (window == 'boundary' and event['event'] == 'gc_start'):
                    event['timestamp_ns'] = times[event['event']]
        if window == 'warmup':
            records[1]['cycles'] = 1
        return records, events, case

    def test_milestones_respect_cycle_end_and_capture_in_every_window(self):
        for window in ('measured', 'warmup', 'boundary'):
            for mutation in ('all_after_capture', 'mark_after_end', 'sweep_after_end', 'quiescent_after_capture'):
                records, events, case = self.cycle_window_fixture(window)
                epoch = {event['event']: event for event in events
                         if event.get('epoch') == 1 and event.get('version') in (1, 3)}
                end = epoch['gc_end']['timestamp_ns']
                if mutation == 'all_after_capture':
                    times = (100, 200, 300)
                elif mutation == 'mark_after_end':
                    times = (end + 1, end + 2, end + 3)
                elif mutation == 'sweep_after_end':
                    times = (epoch['MarkComplete']['timestamp_ns'], end + 1, end + 2)
                else:
                    times = (epoch['MarkComplete']['timestamp_ns'], epoch['SweepComplete']['timestamp_ns'], 51)
                for stage, time in zip(('MarkComplete', 'SweepComplete', 'Quiescent'), times):
                    epoch[stage]['timestamp_ns'] = time
                with self.subTest(window=window, mutation=mutation), self.assertRaises(ValueError):
                    parse(records, events, case)

    def test_invalid_capture_bounds_reject_cycle_evidence(self):
        for endpoint, time in ((0, 11), (-1, 15), (0, 51), (0, None),
                               (-1, True), (-1, -1), (-1, '50'), (-1, 2**64-1)):
            records, events, case = self.complete_cycles_fixture()
            records[endpoint]['timestamp_ns'] = time
            with self.subTest(endpoint=endpoint, time=time), self.assertRaises(ValueError):
                parse(records, events, case)

    def test_post_end_quiescent_within_capture_is_valid(self):
        for window in ('measured', 'warmup', 'boundary'):
            for time in (49, 50):
                records, events, case = self.cycle_window_fixture(window)
                next(event for event in events if event.get('epoch') == 1
                     and event.get('event') == 'Quiescent')['timestamp_ns'] = time
                result = parse(records, events, case)
                self.assertTrue(result['measurement_complete'])
                self.assertEqual(result['combined_old_cycle_pause_ns']['count'], 2 if window == 'measured' else 1)

    def test_cycle_record_schema_rejected_in_every_window(self):
        mutations = [('event', 'garbage'), ('kind', None), ('kind', 0), ('kind', 1), ('kind', 3), ('kind', True)]
        for field in ('epoch', 'timestamp_ns', 'heap_bytes', 'pause_ns', 'mark_ns',
                      'marked_bytes', 'scanned_bytes', 'root_scan_bytes', 'reclaimed_bytes'):
            mutations.extend((field, value) for value in (None, -4096, True, 1.5, '1', 2**64-1, 2**64))
        for window in ('measured', 'warmup', 'boundary'):
            for field, value in mutations:
                records, events, case = self.complete_cycles_fixture()
                for event in events:
                    if event.get('epoch') == 2 and event.get('version') in (1, 3):
                        if window == 'warmup':
                            event['timestamp_ns'] = {'gc_start': 1, 'MarkComplete': 2,
                                                     'SweepComplete': 3, 'Quiescent': 4, 'gc_end': 4}[event['event']]
                        elif window == 'boundary' and event['event'] == 'gc_start':
                            event['timestamp_ns'] = 1
                target = next(event for event in events if event.get('event') == 'gc_end' and event['epoch'] == 2)
                if value is None:
                    del target[field]
                else:
                    target[field] = value
                with self.subTest(window=window, field=field, value=value), self.assertRaises(ValueError):
                    parse(records, events, case)

    def test_cycle_start_schema_and_supported_matching_kinds(self):
        for kind in (1, 2):
            records, events, case = self.complete_cycles_fixture()
            for event in events:
                if event.get('version') == 1:
                    event['kind'] = kind
            self.assertTrue(parse(records, events, case)['measurement_complete'])
        for field in ('event', 'epoch', 'kind', 'timestamp_ns', 'heap_bytes'):
            for value in (None, True, -1, 'garbage'):
                records, events, case = self.complete_cycles_fixture()
                target = next(event for event in events if event.get('event') == 'gc_start')
                if value is None:
                    del target[field]
                else:
                    target[field] = value
                with self.subTest(field=field, value=value), self.assertRaises(ValueError):
                    parse(records, events, case)

    def test_cycle_validation_precedes_measurement_window_filter(self):
        for mutation in ('reversed', 'boundary_missing', 'warmup_missing', 'boundary_misordered'):
            records, events, case = self.complete_cycles_fixture()
            for event in events:
                if event.get('epoch') == 1 and event.get('version') in (1, 3):
                    if mutation == 'reversed' and event['event'] == 'gc_start':
                        event['timestamp_ns'] = 100
                    elif mutation.startswith('boundary') and event['event'] == 'gc_start':
                        event['timestamp_ns'] = 1
                    elif mutation == 'warmup_missing':
                        event['timestamp_ns'] = {'gc_start': 1, 'MarkComplete': 2,
                                                 'SweepComplete': 3, 'Quiescent': 4, 'gc_end': 4}[event['event']]
                    if mutation == 'boundary_misordered' and event['event'] == 'SweepComplete':
                        event['timestamp_ns'] = 9
            if mutation.endswith('missing'):
                events = [event for event in events
                          if not (event.get('epoch') == 1 and event.get('version') == 3)]
            with self.subTest(mutation=mutation), self.assertRaises(ValueError):
                parse(records, events, case)

    def test_invalid_cycle_and_milestone_timestamps_rejected(self):
        for stage in ('gc_start', 'gc_end', 'MarkComplete', 'SweepComplete', 'Quiescent'):
            for value in (None, True, -1, 1.5, '1', 2**64-1):
                records, events, case = self.complete_cycles_fixture()
                next(event for event in events if event.get('event') == stage)['timestamp_ns'] = value
                with self.subTest(stage=stage, value=value), self.assertRaises(ValueError):
                    parse(records, events, case)

    def test_duplicate_milestones_rejected_even_outside_measured_window(self):
        for stage in ('MarkComplete', 'SweepComplete', 'Quiescent'):
            for boundary in (False, True):
                records, events, case = self.complete_cycles_fixture()
                if boundary:
                    next(event for event in events if event.get('event') == 'gc_start')['timestamp_ns'] = 1
                event = next(event for event in events if event.get('event') == stage)
                events.append(copy.deepcopy(event))
                with self.subTest(stage=stage, boundary=boundary), self.assertRaisesRegex(ValueError, 'duplicate milestone'):
                    parse(records, events, case)

    def test_invalid_or_inconsistent_completed_cycle_counters_rejected(self):
        for value in (None, True, -1, 1, 3, 2**64-1):
            records, events, case = self.complete_cycles_fixture()
            records[-1]['stop']['completed_cycles'] = value
            with self.subTest(counter=value), self.assertRaises(ValueError):
                parse(records, events, case)
        records, events, case = self.complete_cycles_fixture()
        records[2]['cycles'] = 3
        with self.assertRaisesRegex(ValueError, 'cycle coverage'):
            parse(records, events, case)

    def test_full_run_rejects_missing_phase_stream(self):
        r,e,c=fixture();c['require_cycles']=True
        with self.assertRaises(ValueError):parse(r,e,c)

    def test_independent_workload_checksum_is_required(self):
        r,e,c=fixture();c['workload_checksum']=55
        with self.assertRaises(ValueError):parse(r,e,c)

    def test_differential_validation_cannot_accept_changed_work(self):
        one=parse(*fixture());two=copy.deepcopy(one);two['checksums']=[99]
        budgets=dict(throughput_min_ratio=.97,cpu_max_ratio=1,footprint_max_ratio=1.1,pause_p99_aim_ns=2000000,local_wait_max_ns=100)
        result=suite.comparison([one]*5,[two]*5,budgets)
        self.assertFalse(result['differential']['passed'])
        self.assertFalse(result['release_pass'])

    def test_calibration_freezes_reference_limits_without_relaxation(self):
        one=parse(*fixture())
        budgets=dict(throughput_min_ratio=.97,cpu_max_ratio=1,footprint_max_ratio=1.1,pause_p99_aim_ns=2000000,local_wait_max_ns=100)
        frozen=suite.calibrate([one]*5,budgets)
        self.assertEqual(frozen['absolute_limits']['throughput_min'],one['application_ops_per_second']*.97)
        self.assertEqual(frozen['absolute_limits']['peak_committed_max'],1100)
        budgets['footprint_max_ratio']=100
        self.assertEqual(frozen['budgets']['footprint_max_ratio'],1.1)

    def test_sweep_visit_throughput_is_distinct_from_reclamation(self):
        r,e,c=fixture()
        r[1].update(sweep_objects=10,sweep_bytes=100,sweep_ns=100)
        r[2].update(sweep_objects=30,sweep_bytes=300,sweep_ns=150)
        result=parse(r,e,c)
        self.assertEqual(result['major_sweep_objects_per_second'],20e9/50)
        self.assertEqual(result['major_sweep_visited_bytes_per_second'],200e9/50)
        self.assertIsNone(result['sweep_reclaimed_bytes_per_second'])

    def test_offline_analysis_preserves_failed_raw_input(self):
        r,e,c=fixture()
        with tempfile.TemporaryDirectory() as directory:
            folder=Path(directory)
            raw='\n'.join(map(json.dumps,r))
            (folder/'stdout.ndjson').write_text(raw)
            (folder/'trace.ndjson').write_text('\n'.join(map(json.dumps,e)))
            self.assertEqual(suite.analyze_saved(folder,c)['status'],'measured')
            outcome=suite.analyze_saved(folder,dict(c,checksum=99))
            self.assertEqual(outcome['status'],'invalid')
            self.assertEqual((folder/'stdout.ndjson').read_text(),raw)

    def test_peak_footprint_gate_uses_transient_high_water(self):
        one=parse(*fixture())
        r,e,c=fixture();r[2]['peak_committed_bytes']=2048
        two=parse(r,e,c)
        self.assertEqual(two['memory']['committed_bytes']['sampled_peak'],1000)
        self.assertEqual(two['peak_committed_bytes'],2048)
        budgets=dict(throughput_min_ratio=.97,cpu_max_ratio=1,footprint_max_ratio=1.1,pause_p99_aim_ns=2000000,local_wait_max_ns=100)
        self.assertFalse(suite.comparison([one]*5,[two]*5,budgets)['footprint']['passed'])

    def test_hard_cap_rejection_is_independent_of_other_axes(self):
        r,e,c=fixture();c['env']={'WILLOW_GC_MEMORY_LIMIT':'999'}
        result=parse(r,e,c)
        self.assertFalse(result['hard_cap_pass'])
        budgets=dict(throughput_min_ratio=.97,cpu_max_ratio=1,footprint_max_ratio=1.1,pause_p99_aim_ns=2000000,local_wait_max_ns=100)
        comparison=suite.comparison([result]*5,[result]*5,budgets)
        self.assertTrue(comparison['throughput']['passed'])
        self.assertFalse(comparison['hard_cap']['passed'])
        self.assertFalse(comparison['release_pass'])

    def test_sweep_and_waste_counter_regression_rejected(self):
        for key in ('sweep_objects','sweep_bytes','sweep_ns','wasted_bytes'):
            r,e,c=fixture();r[1][key]=100;r[2][key]=99
            with self.assertRaises(ValueError):parse(r,e,c)

    def test_binary_spool_merge_preserves_events_and_legacy(self):
        import tempfile, struct
        from pathlib import Path
        with tempfile.TemporaryDirectory() as tmp:
            folder=Path(tmp)
            native=[dict(version=2,event='legacy'),dict(version=4,event='gc_observation',sequence=2,progress=0),dict(version=4,event='gc_observation_checkpoint',sequence=2,progress=2)]
            (folder/'runtime.ndjson').write_text(''.join(json.dumps(e)+'\n' for e in native))
            (folder/'observations.bin').write_bytes(b'WGC4BIN1'+b'\x01'+struct.pack('<17Q',1,*([0]*16))+b'\x02'+struct.pack('<7Q',1,0,0,0,0,0,1))
            suite.merge_trace(folder)
            result=[json.loads(l) for l in (folder/'trace.ndjson').read_text().splitlines()]
            self.assertEqual(result[0],native[0])
            self.assertEqual([e['sequence'] for e in result[1:]],[1,1,2,2])
            self.assertEqual(result[1]['event'],'gc_observation')
            self.assertEqual(result[-1],native[-1])

    def test_binary_spool_corruption_is_rejected(self):
        import tempfile, struct
        from pathlib import Path
        for data in [b'',b'WGC4BIN1\x03',b'WGC4BIN1\x01'+bytes(135),b'WGC4BIN1\x02'+bytes(55),b'WGC4BIN1'+b'\x01'+struct.pack('<17Q',2,*([0]*16))+b'\x01'+struct.pack('<17Q',1,*([0]*16))]:
            with tempfile.TemporaryDirectory() as tmp:
                folder=Path(tmp);(folder/'runtime.ndjson').write_text('')
                (folder/'observations.bin').write_bytes(data)
                with self.assertRaises(ValueError):suite.merge_trace(folder)

    def test_rss_only_regression_rejects_candidate(self):
        one=parse(*fixture());two=dict(one,peak_rss_bytes=one['peak_rss_bytes']*2)
        budgets=dict(throughput_min_ratio=.97,cpu_max_ratio=1,footprint_max_ratio=1.1,pause_p99_aim_ns=2000000,local_wait_max_ns=100)
        result=suite.comparison([one]*5,[two]*5,budgets)
        self.assertTrue(result['footprint']['passed'])
        self.assertFalse(result['rss']['passed'])
        self.assertFalse(result['release_pass'])

    def test_real_run_requires_measurement_counters(self):
        for key in ('wasted_bytes','sweep_bytes','sweep_ns','sweep_objects'):
            r,e,c=fixture();c['require_cycles']=True
            r[1].pop(key,None)
            with self.assertRaisesRegex(ValueError,'measurement counter'):parse(r,e,c)

    def test_resume_never_replaces_failed_or_interrupted_trial(self):
        import tempfile
        from pathlib import Path
        with tempfile.TemporaryDirectory() as tmp:
            folder=Path(tmp)
            (folder/'stdout.ndjson').write_text('preserved partial output')
            self.assertEqual(suite.capture_once(folder,{}, {},None,1)['status'],'invalid')
            original=dict(status='invalid',error='lost events',returncode=0)
            suite.write_json(folder/'result.json',original)
            self.assertEqual(suite.capture_once(folder,{}, {},None,1),original)
            self.assertEqual((folder/'stdout.ndjson').read_text(),'preserved partial output')


class WorkloadEvidenceTests(unittest.TestCase):
    def test_burst_overlap_excludes_warmup_and_churn_uses_batch_retained_ratio(self):
        r,e,c=fixture()
        r[1].update(burst_allocation_overlap=64,burst_mutation_overlap=512)
        r[2].update(burst_allocation_overlap=192,burst_mutation_overlap=1024)
        result=parse(r,e,c)
        self.assertEqual(result['burst_allocation_overlap'],128)
        self.assertEqual(result['burst_mutation_overlap'],512)
        self.assertAlmostEqual(result['allocation_to_retained_ratio'],.2)
        r[2]['burst_mutation_overlap']=0
        with self.assertRaisesRegex(ValueError,'counter decreased'):parse(r,e,c)

    def test_missing_overlap_or_zero_retained_heap_is_not_positive_evidence(self):
        r,e,c=fixture()
        r[1]['live_bytes']=r[2]['live_bytes']=0
        result=parse(r,e,c)
        self.assertEqual(result['burst_allocation_overlap'],0)
        self.assertEqual(result['burst_mutation_overlap'],0)
        self.assertIsNone(result['allocation_to_retained_ratio'])


class PhaseElasticityTests(unittest.TestCase):
    @staticmethod
    def inputs():
        cases=[];results=[]
        for size in (1,4,16):
            case=dict(id='heap_linked_'+str(size),live_bytes=size)
            cases.append(case)
            for _ in range(5):
                results.append(dict(status='measured',case=case['id'],variant='concurrent',
                    summary=dict(pause_ns={'1':{'p99':100},'2':{'p99':size}},
                                 combined_old_cycle_pause_ns={'p99':100+size})))
        return cases,results

    def test_large_constant_pause_cannot_hide_heap_proportional_remark(self):
        cases,results=self.inputs()
        value=suite.scaling_results(cases,{'concurrent':{}},results,5,.5)['linked:concurrent']
        self.assertLess(value['individual']['ci95'][1],.5)
        self.assertLess(value['combined']['ci95'][1],.5)
        self.assertAlmostEqual(value['phases']['2']['slope'],1)
        self.assertFalse(value['passed'])

    def test_missing_required_phase_is_not_a_passing_slope(self):
        cases,results=self.inputs()
        del results[5]['summary']['pause_ns']['2']
        value=suite.scaling_results(cases,{'concurrent':{}},results,5,.5)['linked:concurrent']
        self.assertEqual(value['phase_processes_per_size']['2'],[5,4,5])
        self.assertNotIn('2',value['phases'])
        self.assertFalse(value['passed'])


class ReleaseCoverageTests(unittest.TestCase):
    def test_canonical_nightly_covers_contract_and_pr_is_explicitly_reduced(self):
        manifest=json.loads(Path(suite.__file__).with_name('matrix.json').read_text())
        self.assertEqual(suite.missing_workload_coverage(manifest['profiles']['nightly'],manifest['variants']),[])
        missing=suite.missing_workload_coverage(manifest['profiles']['pr'],manifest['variants'])
        self.assertIn('tasks_millions_measured',missing)
        self.assertIn('linked_fixed_root_heap_scaling',missing)

    def test_deleted_workloads_configurations_or_confounded_roots_cannot_certify(self):
        manifest=json.loads(Path(suite.__file__).with_name('matrix.json').read_text())
        cases=manifest['profiles']['nightly'];variants=manifest['variants']
        missing=suite.missing_workload_coverage([c for c in cases if c['id'] not in ('ffi_pins','hard_cap_8')],{k:v for k,v in variants.items() if k!='pacer_off'})
        self.assertIn('workload:ffi_pins',missing)
        self.assertIn('hard_cap_workers_1_2_4_8',missing)
        self.assertIn('configuration:pacer_off',missing)
        changed=copy.deepcopy(cases)
        next(c for c in changed if c['id']=='heap_linked_1024')['roots']=4096
        self.assertIn('linked_fixed_root_heap_scaling',suite.missing_workload_coverage(changed,variants))

    def test_duplicate_case_ids_reject_before_reusing_any_capture(self):
        from types import SimpleNamespace
        with tempfile.TemporaryDirectory() as tmp:
            path=Path(tmp)/'matrix.json'
            case=dict(id='duplicate',warmup=1,cycles=1)
            path.write_text(json.dumps(dict(schema=1,repeats=5,profiles={'nightly':[case,case]})))
            with self.assertRaisesRegex(ValueError,'duplicate workload case id'):
                suite.run_suite(SimpleNamespace(manifest=path,profile='nightly'))


class RuntimeFaultDecisionTests(unittest.TestCase):
    def test_each_injection_must_fail_its_actual_release_gate(self):
        decisions={fault:{gate:{'passed':False},'release_pass':False}
                   for fault,gate in faults.TARGET_GATES.items()}
        self.assertTrue(faults.targeted_gates_reject(decisions))
        for fault,gate in faults.TARGET_GATES.items():
            for field in (gate,'release_pass'):
                broken=copy.deepcopy(decisions)
                if field==gate:broken[fault][gate]['passed']=True
                else:broken[fault]['release_pass']=True
                self.assertFalse(faults.targeted_gates_reject(broken))
            broken=copy.deepcopy(decisions)
            del broken[fault]
            self.assertFalse(faults.targeted_gates_reject(broken))
        self.assertFalse(faults.targeted_gates_reject({}))

    def test_trace_loss_requires_independent_runtime_loss_evidence(self):
        for flags,dropped,expected in ((3,7,7),(1,0,0),(0,7,0),(9,7,0),(3,0,0)):
            row=dict(event='endpoint',label='after',observation=dict(flags=flags,dropped=dropped))
            self.assertEqual(faults.transport_loss_count(json.dumps(row)),expected)
        self.assertEqual(faults.transport_loss_count(''),0)
        row=json.dumps(dict(event='endpoint',label='after',observation=dict(flags=3,dropped=7)))
        self.assertEqual(faults.transport_loss_count(row+'\n'+row),0)


if __name__ == '__main__':
    unittest.main()
