"""Focused tests of evidence validation and executable A8 thresholds."""
import copy
import json
from pathlib import Path
import subprocess
import sys
import tempfile
import unittest
from preemption_gate import evaluate


def evidence():
    rows = {}
    for name in ("control", "never", "task-control", "task-helper", "async", "baseline-async", "wake"):
        rows[name] = [dict(elapsed_ns=1_000_000, operations=100_000, checksum=42,
                          slow_calls=int(name in ("task-helper", "wake")), latency_ns=10_000)
                      for _ in range(100 if name == "wake" else 7)]
    return rows


class GateTests(unittest.TestCase):
    def test_runnable_gate_exit_status(self):
        with tempfile.TemporaryDirectory() as directory:
            path = Path(directory) / "samples.json"
            rows = evidence()
            for expected in (0, 1):
                if expected:
                    for row in rows["async"]:
                        row["elapsed_ns"] *= 2
                path.write_text(json.dumps({"samples": rows}))
                result = subprocess.run(
                    [sys.executable, str(Path(__file__).with_name("preemption_gate.py")), "--check", str(path)],
                    capture_output=True, text=True, timeout=10,
                )
                self.assertEqual(result.returncode, expected, result.stderr)
                self.assertEqual(json.loads(result.stdout)["passed"], expected == 0)

    def test_valid_equal_controls_pass(self):
        self.assertTrue(evaluate(evidence())["passed"])

    def test_each_threshold_is_enforced(self):
        for name, field, value in (("never", "elapsed_ns", 1_200_001),
                                   ("task-helper", "elapsed_ns", 6_000_001),
                                   ("wake", "latency_ns", 20_000_001),
                                   ("async", "elapsed_ns", 1_100_001)):
            with self.subTest(name=name):
                rows = evidence()
                for row in rows[name]:
                    row[field] = value
                    if name == "wake":
                        row["elapsed_ns"] = value + 1
                self.assertFalse(evaluate(rows)["passed"])

    def test_threshold_boundaries_are_inclusive(self):
        rows = evidence()
        for name, value in (("never", 1_200_000), ("task-helper", 6_000_000), ("async", 1_100_000)):
            for row in rows[name]:
                row["elapsed_ns"] = value
        for row in rows["wake"]:
            row.update(elapsed_ns=20_000_001, latency_ns=20_000_000)
        self.assertTrue(evaluate(rows)["passed"])

    def test_missing_or_extra_workloads(self):
        for name in evidence():
            rows = evidence()
            del rows[name]
            with self.subTest(name=name), self.assertRaises(ValueError):
                evaluate(rows)
        rows = evidence()
        rows["extra"] = copy.deepcopy(rows["never"])
        with self.assertRaises(ValueError):
            evaluate(rows)

    def test_insufficient_samples(self):
        for name in evidence():
            rows = evidence()
            rows[name].pop()
            with self.subTest(name=name), self.assertRaises(ValueError):
                evaluate(rows)

    def test_invalid_numbers(self):
        for key in ("elapsed_ns", "operations"):
            for bad in (0, -1, float("nan"), float("inf"), "1", None, True):
                rows = evidence()
                rows["never"][0][key] = bad
                with self.subTest(key=key, bad=bad), self.assertRaises(ValueError):
                    evaluate(rows)

    def test_unequal_work(self):
        for name in ("never", "task-helper", "async"):
            rows = evidence()
            for row in rows[name]:
                row["operations"] += 1
            with self.subTest(name=name), self.assertRaises(ValueError):
                evaluate(rows)

    def test_changed_size_within_arm(self):
        rows = evidence()
        rows["control"][0]["operations"] += 1
        with self.assertRaises(ValueError):
            evaluate(rows)

    def test_unpaired_samples(self):
        rows = evidence()
        rows["async"].append(copy.deepcopy(rows["async"][0]))
        with self.assertRaises(ValueError):
            evaluate(rows)

    def test_checksums_are_required_and_equal(self):
        for name in ("control", "never", "task-control", "task-helper"):
            for remove in (True, False):
                rows = evidence()
                if remove:
                    del rows[name][0]["checksum"]
                else:
                    rows[name][0]["checksum"] += 1
                with self.subTest(name=name, remove=remove), self.assertRaises(ValueError):
                    evaluate(rows)

    def test_no_task_never_enters_runtime_poll(self):
        rows = evidence()
        rows["never"][0]["slow_calls"] = 1
        with self.assertRaises(ValueError):
            evaluate(rows)

    def test_task_paths_must_reach_runtime_poll(self):
        for name in ("task-helper", "wake"):
            rows = evidence()
            rows[name][0]["slow_calls"] = 0
            with self.subTest(name=name), self.assertRaises(ValueError):
                evaluate(rows)

    def test_wake_must_be_positive_and_inside_elapsed_interval(self):
        for bad in (0, -1, 1_000_001, float("nan"), float("inf"), True, "1", None):
            rows = evidence()
            rows["wake"][0]["latency_ns"] = bad
            with self.subTest(bad=bad), self.assertRaises(ValueError):
                evaluate(rows)

    def test_invalid_slow_counts_and_checksums(self):
        for key, values in (("slow_calls", (-1, 100_001, True, None, "0")),
                            ("checksum", (None, "42", True, float("nan")))):
            for value in values:
                rows = evidence()
                rows["never"][0][key] = value
                with self.subTest(key=key, value=value), self.assertRaises(ValueError):
                    evaluate(rows)

    def test_p99_nearest_rank_not_max_or_median(self):
        rows = evidence()
        rows["wake"][-1].update(latency_ns=30_000_000, elapsed_ns=31_000_000)
        self.assertTrue(evaluate(rows)["passed"])
        rows["wake"][-2].update(latency_ns=30_000_000, elapsed_ns=31_000_000)
        self.assertFalse(evaluate(rows)["passed"])

    def test_signed_overhead_is_not_clamped(self):
        rows = evidence()
        for row in rows["never"]:
            row["elapsed_ns"] -= 1
        self.assertLess(evaluate(rows)["metrics"]["never_task_added_ns_per_iteration"], 0)


if __name__ == "__main__":
    unittest.main()
