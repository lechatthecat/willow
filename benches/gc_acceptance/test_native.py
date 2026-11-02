"""Focused native operation-accounting regression, using the release probe."""
import json
import os
from pathlib import Path
import subprocess
import unittest


class OperationAccounting(unittest.TestCase):
    def test_zero_and_nonzero_checksum_count_executed_work(self):
        binary = Path(os.environ.get("GC_ACCEPTANCE_BINARY", "target/release/examples/gc_acceptance")).resolve()
        for shape in ("tasks", "cancellation"):
            for operations in (1, 2):
                with self.subTest(shape=shape, operations=operations):
                    env = {k: v for k, v in os.environ.items()
                           if not k.startswith(("WILLOW_", "GC_ACCEPTANCE_"))}
                    env.update(WILLOW_WORKERS="1", GC_ACCEPTANCE_OPERATIONS=str(operations))
                    run = subprocess.run([str(binary), shape, "65536", "4", "1", "1"],
                                         env=env, text=True, capture_output=True, timeout=180)
                    self.assertEqual(run.returncode, 0, run.stderr)
                    samples = [r for line in run.stdout.splitlines()
                               if (r := json.loads(line)).get("event") == "sample"]
                    self.assertEqual(len(samples), 2)
                    for sample in samples:
                        self.assertEqual(sample["workload_checksum"], operations * (operations - 1) // 2)
                        self.assertEqual(sample["checksum"], 32640)
                        self.assertEqual(sample["operations"], 512 + operations)


if __name__ == "__main__":
    unittest.main()
