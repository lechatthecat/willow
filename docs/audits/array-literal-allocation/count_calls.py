#!/usr/bin/env python3
"""Build temporary binaries and count executed allocation/store/panic ABI calls.
Run from repository root after cargo build -p willow --bin willow.
"""
import ast
import pathlib
import subprocess
import tempfile

COMPILER = pathlib.Path("target/debug/willow").resolve()
SYMBOLS = ["willow_array_new_literal", "willow_array_new", "willow_panic_depth", "willow_array_set"]
with tempfile.TemporaryDirectory(prefix="willow-literal-count-") as directory:
    root = pathlib.Path(directory)
    for count in [1, 8, 32, 128]:
        source = root / "main.wi"
        binary = root / "probe"
        source.write_text(f"""import std::collections::Array;
fn literal(i: i64) -> Array<i64> {{ return [i, i, i]; }}
fn main() {{ let mut i = 0; let mut sum = 0;
while i < {count} {{ let a = literal(i); sum = sum + a[1]; i = i + 1; }}
println(sum); }}
""")
        subprocess.run([str(COMPILER), "build", str(source), "--release", "-o", str(binary)], check=True, capture_output=True)
        script = root / "count.gdb"
        script.write_text("""set pagination off
set confirm off
python
import gdb
counts = {}
class Counter(gdb.Breakpoint):
    def __init__(self, name):
        super().__init__(name, internal=True)
        self.name = name
        counts[name] = 0
    def stop(self):
        counts[self.name] += 1
        return False
""" + "\n".join(f"Counter({name!r})" for name in SYMBOLS) + """
end
run
python
print('CALL_COUNTS ' + repr(counts))
end
""")
        output = subprocess.run(["gdb", "--batch", "-x", str(script), str(binary)], check=True, text=True, capture_output=True)
        line = next(line for line in output.stdout.splitlines() if line.startswith("CALL_COUNTS "))
        counts = ast.literal_eval(line.removeprefix("CALL_COUNTS "))
        assert counts == {name: count if name == "willow_array_new_literal" else 0 for name in SYMBOLS}, output.stdout + output.stderr
        print(f"literals={count} {line}")
