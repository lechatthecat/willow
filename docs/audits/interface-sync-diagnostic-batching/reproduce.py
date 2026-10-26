#!/usr/bin/env python3
"""Generate independent invalid implementations and check diagnostic growth."""
import pathlib
import subprocess
import tempfile

ROOT = pathlib.Path(__file__).resolve().parents[3]
COMPILER = ROOT / "target/debug/willow"
for size in (16, 64, 256):
    with tempfile.TemporaryDirectory(prefix="willow-marker-batch-") as temporary:
        root = pathlib.Path(temporary)
        players = ["import std::collections::Array; pub interface Player extends Sync {}"]
        writers = ["import players; fn id<T>(x: T) -> T { return x; }"]
        for index in range(size):
            players.append(f"pub class Bag{index} implements Player {{ pub data: Array<i64>; }}")
            players.append(f"pub class Counter{index} implements Player {{ pub value: i64; }}")
            writers.append(f"fn write{index}(p: players::Counter{index}) {{ p.value = 1; }}")
        writers.append("fn main() {}")
        (root / "players.wi").write_text("\n".join(players))
        (root / "main.wi").write_text("\n".join(writers))
        result = subprocess.run([str(COMPILER), "check", str(root / "main.wi")],
                                capture_output=True, text=True, check=False)
        actual = result.stderr.count("error[E2406]")
        assert result.returncode != 0 and actual == 2 * size, result.stderr
        assert result.stderr.count("error[") == actual, result.stderr
        print(f"pairs={size} reassigned_fields={size} diagnostics={actual}")
