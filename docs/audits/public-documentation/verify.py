#!/usr/bin/env python3
"""Validate public documentation names, references and reproducer syntax.

Run from any directory. Only Git's public inventory is read, never ignored
local documentation or specifications. No benchmark or full test suite runs.
"""

import ast
from pathlib import Path
import re
import subprocess
import unittest

ROOT = Path(__file__).resolve().parents[3]


def inventory():
    result = subprocess.check_output(
        ["git", "ls-files", "-z", "--cached", "--others", "--exclude-standard"],
        cwd=ROOT,
    )
    return sorted({p.decode() for p in result.split(b"\0") if p and (ROOT / p.decode()).is_file()})


class PublicDocumentation(unittest.TestCase):
    @classmethod
    def setUpClass(cls):
        cls.files = inventory()
        cls.documents = {}
        for name in cls.files:
            if name.startswith("docs/") or Path(name).suffix in {".md", ".rst", ".txt"} or name == ".gitignore":
                cls.documents[name] = (ROOT / name).read_text()

    def test_public_names(self):
        # Product/tool names, versioned releases and named GC milestones are
        # legitimate uses of the product prefix; opaque issue keys are not.
        allowed = {"cleanup", "edits", "gc", "instruction", "lambda", "literal",
                   "m0", "m3", "m4", "map", "package", "v4", "worker", "write"}
        pattern = re.compile(r"\bwillow-([a-z0-9]+(?:[.-]\d+)*)")
        for name, source in self.documents.items():
            for match in pattern.finditer(name + "\n" + source):
                token = match[1]
                self.assertTrue(token in allowed or re.fullmatch(r"v\d+(?:\.\d+)+", token), name)

    def test_repository_audit_references(self):
        pattern = re.compile(r"docs/audits/[\w./-]+")
        for name, source in self.documents.items():
            if name == ".gitignore":
                continue  # Glob rules are tested separately.
            for match in pattern.finditer(source):
                target = match[0].rstrip(".")
                self.assertTrue((ROOT / target).exists(), (name, target))

    def test_relative_markdown_links(self):
        pattern = re.compile(r"\]\(([^\s)]+)(?:\s+[^)]*)?\)")
        for name, source in self.documents.items():
            if not name.startswith("docs/audits/") or not name.endswith(".md"):
                continue
            for match in pattern.finditer(source):
                target = match[1].split("#")[0]
                if not target or "://" in target or target.startswith(("mailto:", "/")):
                    continue
                self.assertTrue((ROOT / name).parent.joinpath(target).exists(), (name, target))

    def test_public_evidence_is_not_ignored(self):
        names = [name for name in self.files if name.startswith("docs/")]
        result = subprocess.run(
            ["git", "check-ignore", "--no-index", "--stdin"],
            input="\n".join(names) + "\n", text=True, capture_output=True, cwd=ROOT,
        )
        self.assertEqual(result.returncode, 1, result.stdout + result.stderr)
        result = subprocess.run(["git", "check-ignore", "--no-index", "requirements/privacy-probe"],
                                cwd=ROOT, capture_output=True)
        self.assertEqual(result.returncode, 0)

    def test_reproducer_syntax(self):
        for name, source in self.documents.items():
            if not name.startswith("docs/audits/"):
                continue
            if name.endswith(".py"):
                ast.parse(source, filename=name)
            elif name.endswith((".sh", ".js")):
                command = ["bash", "-n"] if name.endswith(".sh") else ["node", "--check"]
                subprocess.run(command + [name], cwd=ROOT, check=True, capture_output=True)

    def test_compiler_and_harness_references(self):
        compiler = (ROOT / "src/backend/cranelift/lir_gen.rs").read_text()
        self.assertIn("../../../docs/audits/map-workloads/word_count.wi", compiler)
        harness = (ROOT / "benches/runtime_compare/harness.js").read_text()
        for directory in ("map-workloads", "parallel-map-chunks"):
            target = f"../../docs/audits/{directory}/measure.js"
            self.assertIn(target, harness)
            self.assertTrue((ROOT / "benches/runtime_compare" / target).is_file())


if __name__ == "__main__":
    unittest.main(verbosity=2)
