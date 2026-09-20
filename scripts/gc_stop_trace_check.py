#!/usr/bin/env python3
"""Check V2 stop traces against independent before/after runtime snapshots.

This checks stop measurement completeness, not full ApexGC acceptance. Endpoints
must be captured from the same isolated process via willow_gc_stats_snapshot(2).
Never manufacture endpoints by counting lines in the trace being checked.
"""

import argparse
import collections
import json
from pathlib import Path
import unittest


def check(lines, before, after):
    required = {
        "version", "flags", "reset_generation", "last_sequence", "active_sequence",
        "events_dropped", "events_pending", "requests", "completed", "aborted",
        "trace_errors", "completed_cycles",
    }
    for endpoint in [before, after]:
        if not isinstance(endpoint, dict):
            raise ValueError("invalid snapshot record")
        if required - endpoint.keys():
            raise ValueError("missing independent snapshot fields")
        if endpoint["version"] != 2 or endpoint["flags"] != 1:
            raise ValueError("unsupported, saturated, lost, or invalid measurement")
        if endpoint["active_sequence"] or endpoint["events_pending"] or endpoint["events_dropped"]:
            raise ValueError("snapshot is not a complete interval boundary")
        for field in ["requests", "completed", "aborted"]:
            if len(endpoint[field]) != 4 or any(type(x) is not int or x < 0 for x in endpoint[field]):
                raise ValueError("invalid per-reason counters")
        for field in required - {"requests", "completed", "aborted"}:
            if type(endpoint[field]) is not int or endpoint[field] < 0:
                raise ValueError("invalid scalar counter")
        if endpoint["requests"] != endpoint["completed"] or any(
                a > c for a, c in zip(endpoint["aborted"], endpoint["completed"])):
            raise ValueError("inconsistent endpoint counters")
    if before["reset_generation"] != after["reset_generation"]:
        raise ValueError("measurement reset during observation")
    if before["trace_errors"] or after["trace_errors"]:
        raise ValueError("trace destination failed")
    delta = [b - a for a, b in zip(before["requests"], after["requests"])]
    if any(x < 0 for x in delta):
        raise ValueError("request counters decreased")
    if delta != [b - a for a, b in zip(before["completed"], after["completed"])]:
        raise ValueError("uncompleted stop request")
    if after["aborted"] != before["aborted"]:
        raise ValueError("collection aborted")
    count = sum(delta)
    low, high = before["last_sequence"], after["last_sequence"]
    if high - low != count:
        raise ValueError("counter and sequence mismatch")
    if after["completed_cycles"] <= before["completed_cycles"]:
        raise ValueError("no completed collection observed; empty data cannot pass")

    requests, releases = {}, {}
    for line in lines:
        event = json.loads(line)
        if not isinstance(event, dict):
            raise ValueError("invalid event record")
        if event.get("version") == 1:
            continue  # V1 cycle events coexist with the opt-in V2 stop stream.
        kind = event.get("event")
        if event.get("version") != 2 or kind not in {"gc_stop_request", "gc_stop_release"}:
            raise ValueError("unknown event schema")
        seq = event["sequence"]
        if type(seq) is not int:
            raise ValueError("invalid event sequence")
        if not low < seq <= high:
            continue
        if event["reset_generation"] != after["reset_generation"]:
            raise ValueError("event generation mismatch")
        target = requests if kind == "gc_stop_request" else releases
        if seq in target:
            raise ValueError("duplicate stop event")
        target[seq] = event
    # Do not allocate a range proportional to an untrusted advertised count.
    if len(requests) != count or requests.keys() != releases.keys():
        raise ValueError("missing/truncated stop events")
    reasons = collections.Counter()
    previous_end = 0
    # Count/range validation above proves this is bounded by the parsed input.
    # Sequence order is collector order, so no O(S log S) timestamp sort is needed.
    for seq in range(low + 1, high + 1):
        start = requests[seq]
        end = releases[seq]
        reason = start["reason"]
        if type(reason) is not int or reason not in range(1, 5) or end["reason"] != reason:
            raise ValueError("stop reason mismatch")
        if end["outcome"] != 0:
            raise ValueError("aborted/incomplete stop")
        begin, stopped, finish = start["timestamp_ns"], end["stopped_ns"], end["timestamp_ns"]
        if any(type(x) is not int or x < 0 for x in [begin, stopped, finish]):
            raise ValueError("invalid event time")
        if not begin <= stopped <= finish:
            raise ValueError("invalid stop ordering")
        if begin < previous_end:
            raise ValueError("overlapping global stops violate collector protocol")
        previous_end = finish
        reasons[reason] += 1
    if [reasons[i] for i in range(1, 5)] != delta:
        raise ValueError("event counts disagree with runtime counters")
    return {"measurement_complete": True, "global_stw_requests": count,
            "zero_stw": count == 0, "by_reason": delta,
            "scope": "global stop requests only; not full ApexGC acceptance"}


class InjectedEvents(unittest.TestCase):
    def setUp(self):
        self.before = dict(version=2, flags=1, reset_generation=0, last_sequence=0,
                           active_sequence=0, events_dropped=0, events_pending=0,
                           requests=[0]*4, completed=[0]*4, aborted=[0]*4,
                           trace_errors=0, completed_cycles=0)
        self.after = {**self.before, "completed_cycles": 1}
        self.lines = []

    def add_stop(self, seq=1, start=10):
        self.after.update(last_sequence=seq, requests=[seq, 0, 0, 0], completed=[seq, 0, 0, 0])
        base = dict(version=2, sequence=seq, reset_generation=0, reason=1)
        self.lines += [json.dumps(dict(base, event="gc_stop_request", timestamp_ns=start)),
                       json.dumps(dict(base, event="gc_stop_release", timestamp_ns=start+20,
                                       stopped_ns=start+5, outcome=0))]

    def test_complete_zero_and_nonzero(self):
        self.assertTrue(check(self.lines, self.before, self.after)["zero_stw"])
        self.add_stop()
        self.assertFalse(check(self.lines, self.before, self.after)["zero_stw"])

    def test_empty_observation_is_not_evidence(self):
        with self.assertRaises(ValueError):
            check([], self.before, self.before)

    def test_missing_truncated_and_duplicate(self):
        self.add_stop()
        for lines in [[], self.lines[:1], [self.lines[0], "{"], self.lines * 2]:
            with self.subTest(lines=lines), self.assertRaises(ValueError):
                check(lines, self.before, self.after)

    def test_error_reset_active_loss_and_saturation(self):
        for field, value in [("flags", 3), ("flags", 5), ("flags", 9),
                             ("trace_errors", 1), ("reset_generation", 1),
                             ("active_sequence", 1), ("events_dropped", 1),
                             ("events_pending", 1), ("aborted", [1, 0, 0, 0])]:
            with self.subTest(field=field), self.assertRaises(ValueError):
                check([], self.before, dict(self.after, **{field: value}))

    def test_overlap_rejected_and_publication_order_irrelevant(self):
        self.add_stop(1, 10)
        self.add_stop(2, 40)
        self.assertEqual(check(reversed(self.lines), self.before, self.after)["global_stw_requests"], 2)
        self.lines = []
        self.add_stop(1, 10)
        self.add_stop(2, 15)
        with self.assertRaises(ValueError):
            check(self.lines, self.before, self.after)


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--self-test", action="store_true")
    parser.add_argument("--trace", type=Path)
    parser.add_argument("--before", type=Path)
    parser.add_argument("--after", type=Path)
    args = parser.parse_args()
    if args.self_test:
        suite = unittest.defaultTestLoader.loadTestsFromTestCase(InjectedEvents)
        raise SystemExit(not unittest.TextTestRunner().run(suite).wasSuccessful())
    if not all([args.trace, args.before, args.after]):
        parser.error("--trace, --before and --after are required")
    try:
        with args.trace.open() as trace:
            result = check(trace, json.loads(args.before.read_text()), json.loads(args.after.read_text()))
        print(json.dumps(result))
        raise SystemExit(0 if result["zero_stw"] else 1)
    except (ValueError, KeyError, TypeError, OSError) as error:
        print(json.dumps({"measurement_complete": False, "error": str(error)}))
        raise SystemExit(2) from error


if __name__ == "__main__":
    main()
