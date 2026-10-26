"""Focused tests and required lint gates; no full local test suite."""
from pathlib import Path
import subprocess

ROOT = Path(__file__).resolve().parents[3]
commands = [
    ["cargo", "fmt", "--all", "--", "--check"],
    ["cargo", "test", "-p", "willow_runtime", "map::tests::", "--", "--nocapture"],
    ["cargo", "test", "-p", "willow_runtime", "native_memory::tests::", "--", "--nocapture"],
    ["cargo", "test", "-p", "willow_runtime", "string::tests::utf8_byte_entries_preserve_content"],
    ["cargo", "test", "-p", "willow_runtime", "string::tests::invalid_utf8_is_process_fatal"],
    ["cargo", "test", "--lib", "local_maps_confinement", "--", "--nocapture"],
    ["cargo", "test", "--lib", "local_string_maps_", "--", "--nocapture"],
    ["cargo", "test", "--test", "integration", "codegen::maps::"],
    ["cargo", "test", "--test", "integration", "codegen::collection_inference::map_inference"],
    ["python3", "docs/audits/map-workloads/check_examples.py"],
    ["cargo", "clippy", "--workspace", "--all-targets", "--", "-D", "warnings"],
]
commands += [["cargo", "clippy", "--workspace", "--all-targets", "--target", target, "--", "-D", "warnings"] for target in [
    "x86_64-unknown-linux-gnu", "x86_64-pc-windows-msvc", "aarch64-apple-darwin", "x86_64-apple-darwin"]]
with (Path(__file__).parent / "validation.txt").open("w") as log:
    for command in commands:
        line = "$ " + " ".join(command)
        print(line, flush=True)
        log.write(line + "\n"); log.flush()
        result = subprocess.run(command, cwd=ROOT, stdout=log, stderr=subprocess.STDOUT)
        log.write(f"exit={result.returncode}\n"); log.flush()
        if result.returncode:
            raise SystemExit(result.returncode)
