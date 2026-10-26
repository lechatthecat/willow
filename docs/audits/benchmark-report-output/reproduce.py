"""Focused CLI regression proof; no benchmarks or toolchain builds are run."""
import contextlib
import importlib.util
import io
from pathlib import Path
import shutil
import subprocess
import sys
import tempfile
import unittest
from unittest.mock import patch

ROOT = Path(__file__).resolve().parents[3]
SUITE = ROOT / "benches/go_vs_willow"
SCRIPT = SUITE / "compare.py"
SOURCE = SUITE / "MEASUREMENT_2026-10-25.jsonl"


class ReportOutputTests(unittest.TestCase):
    def setUp(self):
        self.temp = tempfile.TemporaryDirectory()
        self.addCleanup(self.temp.cleanup)
        self.jsonl = Path(self.temp.name) / SOURCE.name
        shutil.copyfile(SOURCE, self.jsonl)
        self.md = self.jsonl.with_suffix(".md")

    def report(self, *args):
        return subprocess.run(
            [sys.executable, str(SCRIPT), "report", str(self.jsonl), *args],
            check=True, capture_output=True, text=True,
        ).stdout

    def test_existing_curated_report_and_input_preserved_with_baseline(self):
        self.md.write_text("curated provenance\n")
        original = self.jsonl.read_bytes()
        output = self.report("--baseline", str(self.jsonl), "--label", "preview")
        self.assertEqual(self.md.read_text(), "curated provenance\n")
        self.assertEqual(self.jsonl.read_bytes(), original)
        self.assertTrue(output.startswith("# Willow vs Go vs Java measurement — preview"))
        self.assertIn("Δ vs baseline", output)

    def test_default_does_not_create_sibling_report(self):
        self.assertTrue(self.report().startswith("# Willow vs Go vs Java"))
        self.assertFalse(self.md.exists())

    def test_generated_reports_have_no_internal_identifiers(self):
        target = Path(self.temp.name) / "public.md"
        for args in ((), ("--baseline", str(self.jsonl)), ("--out", str(target))):
            with self.subTest(args=args):
                output = self.report(*args)
                self.assertIn("## Scheduler phase CPU and throughput", output)
                self.assertIn("busy-spins during parallel phases", output)
                self.assertNotRegex(output, r"\bwillow-[a-z0-9]{3,4}(?:\.\d+)*\b")
                if args and args[0] == "--out":
                    self.assertNotRegex(target.read_text(), r"\bwillow-[a-z0-9]{3,4}(?:\.\d+)*\b")

    def test_explicit_output_creates_and_overwrites_only_requested_file(self):
        self.md.write_text("keep sibling\n")
        target = Path(self.temp.name) / "preview.md"
        for existing in (False, True):
            with self.subTest(existing=existing):
                if existing:
                    target.write_text("replace me\n")
                output = self.report("--out", str(target))
                self.assertEqual(output, str(target) + "\n" + target.read_text() + "\n")
                self.assertTrue(target.read_text().startswith("# Willow vs Go vs Java"))
                self.assertEqual(self.md.read_text(), "keep sibling\n")

    def test_run_still_writes_jsonl_and_markdown(self):
        sys.path.insert(0, str(SUITE))
        self.addCleanup(sys.path.remove, str(SUITE))
        spec = importlib.util.spec_from_file_location("compare", SCRIPT)
        module = importlib.util.module_from_spec(spec)
        spec.loader.exec_module(module)
        env, rows = module.split_environment(module.json_lines(self.jsonl.read_text()))
        args = [str(SCRIPT), "run", "--label", "regression", "--out-dir", self.temp.name]
        with patch.object(sys, "argv", args), patch.object(module, "environment", return_value=env), \
                patch.object(module, "run_suites", return_value=rows), contextlib.redirect_stdout(io.StringIO()):
            module.main()
        result = Path(self.temp.name) / "MEASUREMENT_regression"
        self.assertTrue(result.with_suffix(".jsonl").is_file())
        self.assertTrue(result.with_suffix(".md").read_text().startswith("# Willow vs Go vs Java"))


if __name__ == "__main__":
    unittest.main(verbosity=2)
