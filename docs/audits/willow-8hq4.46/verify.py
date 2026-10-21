"""Focused examples and Linux x86-64 generated-code size evidence."""
import json
import os
from pathlib import Path
import re
import subprocess
import tempfile

HERE = Path(__file__).resolve().parent
ROOT = HERE.parents[2]
COMPILER = ROOT / "target/release/willow"
EXPECTED = {
    "parallel_map": "[25, 1, 16, 4, 9]\n[10, 2, 8, 4, 6]\n",
    "parallel_map_cancel": "cancelled\n5997\n2997\n[15, 3, 12, 6, 9]\n320\n",
    "lir_parallel_map": "[25, 1, 16, 4, 9]\n[15, 11, 14, 12, 13]\n[10, 2, 8, 4, 6]\n[35, 11, 26, 14, 19]\n46\n[25, 1, 16, 4, 9] [15, 11, 14, 12, 13]\n[]\ncancelled\n",
    "parallel_context_bridge": "4294967297\n-4294967297\n9223372036854775807\n-9223372036854775807\n0\n",
}


def run(args, **kwargs):
    return subprocess.run(args, cwd=ROOT, check=True, text=True,
                          capture_output=True, timeout=120, **kwargs).stdout


with tempfile.TemporaryDirectory(prefix="parallel-map-audit-") as temp:
    temp = Path(temp)
    for name, expected in EXPECTED.items():
        binary = temp / name
        run([COMPILER, "build", ROOT / "example" / (name + ".wi"),
             "-o", binary, "--release"])
        for workers, stress in [("1", ""), ("8", ""), ("8", "relocate")]:
            env = dict(os.environ, WILLOW_WORKERS=workers, WILLOW_GC_STRESS=stress,
                       WILLOW_TASK_BUDGET="1")
            actual = run([binary], env=env)
            assert actual == expected, (name, workers, stress, actual)
            print(json.dumps(dict(example=name, workers=int(workers), stress=stress,
                                  budget=1, result="pass")), flush=True)
    counts = []
    for size in [64, 1024, 65536, 4000000]:
        source = temp / "shape.wi"
        source.write_text((HERE / "parallel_map.wi").read_text().replace("4000000", str(size)))
        binary = temp / "shape"
        run([COMPILER, "build", source, "-o", binary, "--release"])
        assembly = run(["objdump", "-d", "--demangle",
                        "--disassemble=willow_runtime::parallel::poll_chunk", binary])
        instructions = re.findall(r"^\s*[0-9a-f]+:\s*((?:[0-9a-f]{2}\s)+)", assembly, re.M)
        byte_count = sum(len(line.split()) for line in instructions)
        mapper_calls = len(re.findall(r"\bcall\s+\*%rax", assembly))
        assert byte_count > 0 and mapper_calls == 1, assembly
        assert not re.search(r"NativeFrameLayout::(?:slot_offset|slot_count)|WordLayout::word", assembly)
        counts.append(byte_count)
        print(json.dumps(dict(elements=size, poll_code_bytes=byte_count,
                              emitted_mapper_calls=mapper_calls)), flush=True)
        if size == 4000000:
            # Strip the temporary binary pathname; the instructions are public
            # runtime code, with no private inputs or build artifacts retained.
            (HERE / "poll-chunk.txt").write_text(assembly.replace(str(binary), "<audit-binary>"))
    assert len(set(counts)) == 1, counts
