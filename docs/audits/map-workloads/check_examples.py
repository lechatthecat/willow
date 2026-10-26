"""Focused acceptance examples only; never a full example audit."""
import os
from pathlib import Path
import subprocess
import tempfile

ROOT = Path(__file__).resolve().parents[3]
EXPECTED = {
    "maps": "2\n31\n25\n-1\ntrue\nfalse\ntwo\n",
    "map_inference": "42\n2\n",
    "map_key_value_kinds": "16\n1\n37\ntwo\n42\ntrue\nfalse\n",
    "map_scalar_storage": "-9\n42\n0\n-1\n2\n{0: 0, 7: 42}\ntrue\nfalse\n1\n-2.5\n",
    "frozen_map": "3\n2\ntrue\n150\n",
    "frozen_map_gc": "449\ntwo!\n?\n1536\n2\n",
    "map_string_local": "9\n7\n3\nfalse\n2\n",
}
with tempfile.TemporaryDirectory(prefix="willow-map-examples-") as directory:
    for name, expected in EXPECTED.items():
        binary = str(Path(directory) / (name + (".exe" if os.name == "nt" else "")))
        subprocess.run([str(ROOT / "target/release/willow"), "build", str(ROOT / "example" / (name + ".wi")), "--release", "-o", binary], check=True, cwd=ROOT, capture_output=True)
        for stress in ["", "minor"]:
            result = subprocess.run([binary], check=True, capture_output=True, text=True, timeout=120, env={**os.environ, "WILLOW_WORKERS": "4", "WILLOW_GC_STRESS": stress})
            assert result.stdout == expected, (name, stress, result.stdout, expected)
            print(f"PASS {name} stress={stress or 'off'}", flush=True)
