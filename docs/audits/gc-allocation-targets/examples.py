"""Four GC regressions in debug/release and normal/allocation/minor stress."""
import json
import os
from pathlib import Path
import subprocess
ROOT = Path(__file__).resolve().parents[3]
HERE = Path(__file__).resolve().parent
out = ROOT / "target/runtime_compare/allocation"
with (HERE / "examples.jsonl").open("w") as output:
    for release in [False, True]:
        for name in ["gc_linked_list", "gc_old_cycle", "gc_safety_temporaries", "match_binding_gc_roots"]:
            binary = out / (name + ("-release" if release else "-debug"))
            cmd = [str(ROOT / "target/release/willow"), "build", str(ROOT / ("example/" + name + ".wi")), "-o", str(binary)] + (["--release"] if release else [])
            built = subprocess.run(cmd, cwd=ROOT, capture_output=True, text=True, timeout=300)
            assert built.returncode == 0, built.stdout + built.stderr
            expected = None
            for stress in ["", "alloc", "minor"]:
                env = dict(os.environ, WILLOW_WORKERS="8", WILLOW_GC_STRESS=stress)
                result = subprocess.run([str(binary)], cwd=ROOT, env=env, capture_output=True, text=True, timeout=120)
                if expected is None:
                    expected = result.stdout
                if name == "gc_linked_list":
                    assert result.stdout == "6\n"
                if name == "gc_old_cycle":
                    assert result.stdout == "499500\n" * 3
                row = dict(example=name, release=release, stress=stress, status=result.returncode, stdout=result.stdout, stderr=result.stderr)
                output.write(json.dumps(row) + "\n"); output.flush()
                assert result.returncode == 0 and result.stdout == expected, row
            print(name, release, "normal/alloc/minor passed", flush=True)
