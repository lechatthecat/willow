#!/usr/bin/env python3
"""V4 transport consumer. Independent endpoint snapshots are mandatory.

This is a measurement check, not the collector's zero-STW acceptance gate.
Input endpoints come from observations::snapshot(), not from trace lines.
"""
import argparse
import collections
import json
import heapq
from pathlib import Path
import unittest


def check(lines, before, after):
    required = {'sequence', 'flags', 'dropped', 'active', 'registered',
                'unregistered', 'progress', 'trace_errors', 'reset_generation'}
    for endpoint in (before, after):
        if required - endpoint.keys():
            raise ValueError('missing independent endpoint')
        if any(type(endpoint[k]) is not int or not 0 <= endpoint[k] < 2**64 for k in required):
            raise ValueError('invalid endpoint scalar')
        if endpoint['flags'] != 1 or endpoint['dropped'] or endpoint['trace_errors']:
            raise ValueError('disabled, lost, saturated or broken measurement')
        if endpoint['active'] or endpoint['registered'] != endpoint['unregistered']:
            raise ValueError('endpoint must be outside registered lifetimes and intervals')
    if after['reset_generation'] != before['reset_generation']:
        raise ValueError('runtime reset during observation')
    if any(after[k] < before[k] for k in required):
        raise ValueError('counter decreased')
    if after['progress'] <= before['progress']:
        raise ValueError('no mutator progress evidence')
    low, high = before['sequence'], after['sequence']
    sequence = low
    active, members, progress = {}, set(), {}
    registered = unregistered = 0
    registration_progress = {}
    roles = {}
    role_progress = collections.Counter()
    timeline = collections.defaultdict(list)
    checkpoint = None
    work = collections.Counter()
    intervals = collections.Counter()
    for line in lines:
        record = json.loads(line)
        if not isinstance(record, dict):
            raise ValueError('invalid record')
        if record.get('version') in (1, 2, 3):
            continue  # Legacy cycle/stop/milestone transport has its own checker.
        if record.get('version') != 4:
            raise ValueError('unknown version')
        if record.get('event') == 'gc_observation_checkpoint':
            if low <= record['sequence'] <= high:
                if record['sequence'] != sequence:
                    raise ValueError('checkpoint sequence gap')
                if record['flags'] != 1 or record['dropped']:
                    raise ValueError('invalid checkpoint coverage')
                checkpoint = record
            continue
        if record.get('event') != 'gc_observation':
            raise ValueError('unknown event')
        keys = ('sequence', 'timestamp_ns', 'epoch', 'thread', 'role', 'op', 'kind',
                'identity', 'aborted', 'progress', 'objects', 'slots', 'bytes',
                'metadata_bytes', 'budget_checks', 'yields', 'restarts')
        if any(type(record.get(k)) is not int or not 0 <= record[k] < 2**64 for k in keys):
            raise ValueError('missing/invalid event scalar')
        seq = record['sequence']
        if not low < seq <= high:
            continue
        if seq != sequence + 1:
            raise ValueError('duplicate, lost, or reordered event')
        sequence = seq
        thread, op, kind, now = (record[k] for k in ('thread', 'op', 'kind', 'timestamp_ns'))
        role = record['role']
        if role not in (0, 1) or roles.setdefault(thread, role) != role:
            raise ValueError('unknown or changing thread role')
        if record['aborted']:
            raise ValueError('unwound measurement')
        if op != 2:  # End retains the begin's progress and epoch by design.
            if record['progress'] < progress.get(thread, 0):
                raise ValueError('thread progress decreased')
            progress[thread] = record['progress']
        if op == 3:
            if thread in members:
                raise ValueError('duplicate registration')
            members.add(thread)
            registration_progress[thread] = record['progress']
            registered += 1
            if role == 0:
                timeline[thread].append((now, seq, 'register', thread))
        elif op == 4:
            if thread not in members:
                raise ValueError('unmatched unregister')
            delta = record['progress'] - registration_progress.pop(thread)
            if delta <= 0:
                raise ValueError('registered mutator has no progress evidence')
            role_progress[role] += delta
            members.remove(thread)
            unregistered += 1
            if role == 0:
                timeline[thread].append((now, seq, 'unregister', thread))
        elif op == 1:
            if kind > 11:
                raise ValueError('unknown interval kind')
            active[seq] = record
            if role == 0 and (kind <= 4 or kind in (8, 9, 10, 11)):
                timeline[thread].append((now, seq, 'begin', thread))
        elif op == 2:
            start = active.pop(record['identity'], None)
            if start is None or any(start[k] != record[k] for k in ('thread', 'kind', 'epoch')):
                raise ValueError('unmatched interval')
            if now < start['timestamp_ns']:
                raise ValueError('backwards interval')
            intervals[kind] += now - start['timestamp_ns']
            if role == 0 and (kind <= 4 or kind in (8, 9, 10, 11)):
                timeline[thread].append((now, seq, 'end', thread))
        elif op in (6, 7):
            if (op == 6 and kind >= 6) or (op == 7 and kind != 0):
                raise ValueError('unknown work phase')
            for field in ('objects', 'slots', 'bytes', 'metadata_bytes', 'budget_checks', 'yields', 'restarts'):
                work[f'{op}:{kind}:{field}'] += record[field]
        else:
            raise ValueError('unknown operation')
    if sequence != high or active or members:
        raise ValueError('truncated or unfinished observation')
    if checkpoint is None or any(checkpoint[k] != after[k] for k in required - {'trace_errors', 'reset_generation'}):
        raise ValueError('missing final checkpoint or independent endpoint mismatch')
    if registered != after['registered'] - before['registered'] or unregistered != registered:
        raise ValueError('lifecycle counters disagree')

    # Both boundaries have no registered threads, so unregister has flushed
    # every TLS batch. Per-lifetime deltas must exactly cover the global delta.
    if role_progress[0] + role_progress[1] != after['progress'] - before['progress']:
        raise ValueError('thread progress disagrees with independent endpoints')

    if not role_progress[0]:
        raise ValueError('no application mutator progress evidence')

    # Merge monotonic application thread timelines: union of simultaneous GC-attributed
    # waits across the registered population, including overlapping nested spans.
    members, waits = set(), collections.Counter()
    blocked = 0
    previous = None
    union = 0
    for events in timeline.values():
        if any(a[:2] > b[:2] for a,b in zip(events, events[1:])):
            raise ValueError('thread timestamps went backwards')
    for now, _, op, thread in heapq.merge(*timeline.values()):
        if previous is not None and members and blocked == len(members):
            union += now - previous
        if op == 'register':
            members.add(thread)
            blocked += waits[thread] > 0
        elif op == 'unregister':
            blocked -= waits[thread] > 0
            members.remove(thread)
        elif op == 'begin':
            if thread in members and waits[thread] == 0:
                blocked += 1
            waits[thread] += 1
        else:
            waits[thread] -= 1
            if thread in members and waits[thread] == 0:
                blocked -= 1
        previous = now
    return {'global_gc_wait_union_ns': union, 'events': high-low,
            'interval_ns': dict(intervals), 'work': dict(work),
            'application_progress': role_progress[0], 'marker_progress': role_progress[1]}


def endpoint(sequence=0, **updates):
    result = dict(sequence=sequence, flags=1, dropped=0, active=0,
                  registered=0, unregistered=0, progress=0, trace_errors=0, reset_generation=0)
    result.update(updates)
    return result


def fixture():
    records = []
    def event(op, thread, ns, kind=0, identity=0):
        records.append(dict(version=4, event='gc_observation', sequence=len(records)+1,
                            timestamp_ns=ns, epoch=1, thread=thread, role=0, op=op, kind=kind,
                            identity=identity, aborted=0, progress=int(op == 4),
                            objects=0, slots=0, bytes=0, metadata_bytes=0,
                            budget_checks=0, yields=0, restarts=0))
    event(3,1,0); event(3,2,0)
    event(1,1,10,2); event(1,2,20,4)
    event(1,1,25,1); event(2,1,28,1,5)
    event(2,1,30,2,3); event(2,2,40,4,4)
    event(4,1,50); event(4,2,50)
    after = endpoint(10, registered=2, unregistered=2, progress=2)
    records.append(dict(version=4,event='gc_observation_checkpoint',**{k:v for k,v in after.items() if k!='trace_errors'}))
    return records, endpoint(), after


class Tests(unittest.TestCase):
    def run_records(self, records, before, after):
        return check(map(json.dumps, records), before, after)

    def test_hidden_global_wait_and_nested_union(self):
        self.assertEqual(self.run_records(*fixture())['global_gc_wait_union_ns'],10)

    def test_registration_and_cycle_entry_coordination_contention(self):
        for nested in (False, True):
            records=[]
            template=fixture()[0][0]
            def emit(op,thread,ns,kind=0,identity=0):
                records.append(dict(template,sequence=len(records)+1,op=op,
                                    thread=thread,timestamp_ns=ns,kind=kind,
                                    identity=identity,progress=int(op==4)))
                return len(records)
            emit(3,1,0)
            emit(3,2,0)  # Registration published before coordination acquisition.
            lock=emit(1,2,5,11)
            collector=emit(1,1,10,8)
            if nested:
                join=emit(1,2,15,9)
                emit(2,2,25,9,join)
            emit(2,1,30,8,collector)
            emit(2,2,35,11,lock)
            emit(4,1,40); emit(4,2,40)
            after=endpoint(len(records),registered=2,unregistered=2,progress=2)
            records.append(dict(version=4,event='gc_observation_checkpoint',**after))
            result=self.run_records(records,endpoint(),after)
            self.assertEqual(result['global_gc_wait_union_ns'],20)

    def test_nonoverlap_and_zero_wait_with_intact_progress(self):
        records,before,after=fixture()
        for i in (3,7,8,9): records[i]['timestamp_ns']+=20
        self.assertEqual(self.run_records(records,before,after)['global_gc_wait_union_ns'],0)

    def test_reject_missing_duplicate_truncated_and_unwound(self):
        for mutation in ('missing','duplicate','truncated','unwound','identity','backwards','thread'):
            with self.subTest(mutation=mutation):
                records,before,after=fixture()
                if mutation=='missing': records.pop(3)
                elif mutation=='duplicate': records.insert(3,records[3])
                elif mutation=='truncated': records.pop()
                elif mutation=='unwound': records[6]['aborted']=1
                elif mutation=='identity': records[6]['identity']=99
                elif mutation=='backwards': records[6]['timestamp_ns']=1
                else: records[6]['thread']=99
                with self.assertRaises(ValueError): self.run_records(records,before,after)

    def test_reject_disabled_lost_saturated_errors_and_empty(self):
        for field,value in [('flags',0),('flags',3),('flags',5),('trace_errors',1),('active',1),('dropped',1),('progress',0),('sequence',11),('reset_generation',1)]:
            with self.subTest(field=field,value=value):
                records,before,after=fixture(); after[field]=value
                with self.assertRaises(ValueError): self.run_records(records,before,after)
        with self.assertRaises(ValueError): check([],endpoint(),endpoint())

    def test_increasing_nested_depth_and_thread_fanout(self):
        for threads in (1, 4, 16):
            for depth in (1, 8, 64):
                records=[]
                template=fixture()[0][0]
                def emit(op,thread,ns,identity=0):
                    records.append(dict(template,sequence=len(records)+1,op=op,
                                        thread=thread,timestamp_ns=ns,identity=identity,
                                        progress=int(op==4),kind=9))
                for thread in range(threads): emit(3,thread,0)
                starts={}
                for thread in range(threads):
                    for level in range(depth):
                        emit(1,thread,10+level)
                        starts[thread,level]=len(records)
                for thread in range(threads):
                    for level in reversed(range(depth)):
                        emit(2,thread,1000-level,starts[thread,level])
                for thread in range(threads): emit(4,thread,2000)
                after=endpoint(len(records),registered=threads,unregistered=threads,progress=threads)
                records.append(dict(version=4,event='gc_observation_checkpoint',**after))
                result=self.run_records(records,endpoint(),after)
                self.assertEqual(result['events'],threads*(2*depth+2))
                self.assertEqual(result['global_gc_wait_union_ns'],990)

    def test_running_marker_does_not_mask_application_wait(self):
        records,before,after=fixture()
        # Marker is registered for the entire application window, progresses,
        # and performs work without any wait interval of its own.
        marker=dict(records[0],thread=99,role=1)
        records.insert(0,dict(marker,op=3,timestamp_ns=0,progress=0))
        records.insert(-1,dict(marker,op=7,timestamp_ns=25,progress=3))
        records.insert(-1,dict(marker,op=4,timestamp_ns=50,progress=7))
        for seq,record in enumerate(records[:-1],1):
            record['sequence']=seq
            if record['op']==2: record['identity']+=1
        after.update(sequence=13,registered=3,unregistered=3,progress=9)
        records[-1].update(after)
        result=self.run_records(records,before,after)
        self.assertEqual(result['global_gc_wait_union_ns'],10)
        self.assertEqual(result['application_progress'],2)
        self.assertEqual(result['marker_progress'],7)
        # Neither dropping marker progress nor inventing extra progress may pass,
        # even if the checkpoint agrees with the independent endpoint.
        for reported in (2, 109):
            after['progress']=reported
            records[-1]['progress']=reported
            with self.assertRaisesRegex(ValueError,'thread progress disagrees'):
                self.run_records(records,before,after)

    def test_progress_reconciles_with_independent_endpoint_delta(self):
        for prior in (0, 1000):
            for reported in (1, 2, 102):
                with self.subTest(prior=prior,reported=reported):
                    records,before,after=fixture()
                    before['progress']=prior
                    after['progress']=prior+reported
                    records[-1]['progress']=after['progress']
                    if reported==2:
                        result=self.run_records(records,before,after)
                        self.assertEqual(result['application_progress'],2)
                    else:
                        with self.assertRaisesRegex(ValueError,'thread progress disagrees'):
                            self.run_records(records,before,after)

    def test_role_coverage_rejects_missing_unknown_changed_and_marker_only(self):
        for mutation in ('missing','unknown','changed','marker_only'):
            with self.subTest(mutation=mutation):
                records,before,after=fixture()
                if mutation=='missing': records[0].pop('role')
                elif mutation=='unknown': records[0]['role']=2
                elif mutation=='changed': records[6]['role']=1
                else:
                    for record in records[:-1]: record['role']=1
                with self.assertRaises(ValueError): self.run_records(records,before,after)

    def test_work_units_and_lifecycle(self):
        records,before,after=fixture()
        records.insert(-1,dict(records[0],sequence=11,op=6,kind=1,thread=3,objects=2,slots=4,bytes=32,metadata_bytes=8,budget_checks=3,yields=1,restarts=1))
        after['sequence']=11;records[-1]['sequence']=11
        result=self.run_records(records,before,after)
        self.assertEqual(result['work']['6:1:bytes'],32)
        self.assertEqual(result['work']['6:1:restarts'],1)


if __name__ == '__main__':
    parser=argparse.ArgumentParser()
    parser.add_argument('--self-test',action='store_true')
    parser.add_argument('--trace',type=Path)
    parser.add_argument('--endpoints',type=Path)
    args=parser.parse_args()
    if args.self_test: unittest.main(argv=['gc_observation_check'])
    else:
        endpoints=json.loads(args.endpoints.read_text())
        with args.trace.open() as stream: print(json.dumps(check(stream,endpoints['before'],endpoints['after']),sort_keys=True))
