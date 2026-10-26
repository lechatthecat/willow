"""Linux/binutils generated-code census; temporary binaries stay outside docs."""
import json
import os
from pathlib import Path
import re
import subprocess
import sys
import tempfile

compiler = str(Path(sys.argv[1]).resolve())
rows = []
with tempfile.TemporaryDirectory(prefix="string-metadata-") as temporary:
    root = Path(temporary)
    for count in (1, 8, 32):
        source = root / "sites.wi"
        source.write_text("\n".join(
            f"fn size{i}(s: String) -> i64 {{ return s.len(); }}"
            for i in range(count)
        ) + 'fn main() { let s = "é水🌲";' + "".join(
            f"println(size{i}(s));" for i in range(count)
        ) + "}")
        binary = root / "sites"
        subprocess.run([compiler, "build", str(source), "--release", "-o", str(binary)],
                       env={**os.environ, "WILLOW_KEEP_OBJECT": "1"}, check=True,
                       capture_output=True)
        obj = str(binary) + ".o"
        symbols = subprocess.check_output(["nm", "-S", "--defined-only", obj], text=True)
        sizes = [int(match[1], 16) for line in symbols.splitlines()
                 if (match := re.search(r"^[0-9a-f]+ ([0-9a-f]+) [Tt] size[0-9]+$", line))]
        assembly = subprocess.check_output(["objdump", "-dr", obj], text=True)
        calls = assembly.count("willow_string_len")
        assert len(sizes) == count, symbols
        assert calls == 0, assembly
        rows.append({"sites": count, "length_runtime_relocations": calls,
                     "function_sizes_bytes": sizes, "total_function_bytes": sum(sizes)})
print(json.dumps(rows, indent=2))
