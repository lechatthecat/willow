#!/usr/bin/env python3
"""Check length-independent generated IR for a fixed-array parameter."""
import pathlib
import re
import subprocess
import tempfile

root = pathlib.Path(__file__).resolve().parents[3]
compiler = root / "target/debug/willow"
with tempfile.TemporaryDirectory(prefix="fixed-array-counts-") as directory:
    path = pathlib.Path(directory) / "probe.wi"
    previous = None
    for length in (1, 32, 1024, 1048576):
        path.write_text(
            f"pub fn length(xs: [i64; {length}]) -> i64 {{ return xs.len(); }} "
            "fn main() {}\n"
        )
        result = subprocess.run(
            [str(compiler), "build", str(path), "--emit-lir"],
            text=True, capture_output=True, check=True,
        )
        # The same file and function names eliminate path/identifier noise.
        ir = re.sub(r"Span \{[^}]*\}", "Span {}", result.stdout)
        assert "ArrayLen" in ir and "fn length" in ir
        if previous is not None:
            assert ir == previous, f"IR grew with declared length {length}"
        previous = ir
        print(f"length={length} lir_bytes={len(ir.encode())} lir_lines={len(ir.splitlines())}")
