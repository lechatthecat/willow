#!/usr/bin/env python3
"""Focused Linux x86-64 release stack audit; temporary artifacts are removed.
Run after cargo build --release --bin willow. Requires GNU objdump.
"""
import json
import platform
import re
import resource
import subprocess
import tempfile
from pathlib import Path

ROOT = Path(__file__).resolve().parents[3]
COMPILER = ROOT / "target/release/willow"
CHAIN = (ROOT / "example/closure_chain_stack.wi").read_text().replace("chain(10000,", "chain(100000,")
PLAIN = """fn chain(n: i64) -> i64 {
    if n == 0 { return 0; }
    return 1 + chain(n - 1);
}
fn main() { println(chain(100000)); }
"""


def stack_limit(kib):
    def apply():
        resource.setrlimit(resource.RLIMIT_STACK, (kib * 1024, resource.getrlimit(resource.RLIMIT_STACK)[1]))
        resource.setrlimit(resource.RLIMIT_CORE, (0, 0))
    return apply


def frames(binary):
    assembly = subprocess.check_output(["objdump", "-d", str(binary)], text=True)
    result = {}
    for name in ("chain", "$lambda.0", "$lambda.1"):
        match = re.search(r"^[0-9a-f]+ <" + re.escape(name) + r">:\n(.*?)(?=\n\n)", assembly, re.M | re.S)
        if match is None:
            continue
        body = match[1]
        allocation = re.search(r"sub\s+\$0x([0-9a-f]+),%rsp", body)
        assert "push   %rbp" in body, name
        result[name] = {
            "frame_bytes_including_call_and_saved_rbp": 16 + (int(allocation[1], 16) if allocation else 0),
            "instruction_count": len(re.findall(r"^\s+[0-9a-f]+:", body, re.M)),
        }
    return result


def main():
    assert platform.system() == "Linux" and platform.machine() == "x86_64"
    with tempfile.TemporaryDirectory(prefix="willow-closure-stack-") as temporary:
        directory = Path(temporary)
        for kind, template in (("plain", PLAIN), ("closure", CHAIN), ("async_closure", CHAIN.replace("fn main()", "async fn main()"))):
            for depth in (1000, 10000, 25000, 50000, 100000):
                source = directory / "input.wi"
                binary = directory / "program"
                source.write_text(template.replace("100000", str(depth)))
                build = subprocess.run([str(COMPILER), "build", str(source), "--release", "-o", str(binary)], capture_output=True, text=True, timeout=120)
                assert build.returncode == 0, build.stderr
                measured = frames(binary)
                expected_frames = {"chain"} if kind == "plain" else {"chain", "$lambda.0", "$lambda.1"}
                assert measured.keys() == expected_frames, measured
                for kib in (8192, 32768):
                    run = subprocess.run([str(binary)], capture_output=True, text=True, timeout=60, preexec_fn=stack_limit(kib))
                    success = run.returncode == 0 and run.stdout.strip() == str(depth)
                    if not success:
                        assert run.returncode != 0 and "stack overflow" in run.stderr, run
                    expected_success = not (depth == 100000 and (kind == "async_closure" or (kind == "closure" and kib == 8192)))
                    assert success == expected_success, run
                    print(json.dumps(dict(kind=kind, depth=depth, stack_kib=kib, success=success, returncode=run.returncode, stdout=run.stdout.strip(), stderr=run.stderr.strip(), frames=measured)), flush=True)


if __name__ == "__main__":
    main()
