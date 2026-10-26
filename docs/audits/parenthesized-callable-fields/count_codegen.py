"""Count emitted LIR for callable-field calls; run after cargo build --bin willow."""
import pathlib
import subprocess
import tempfile

compiler = pathlib.Path("target/debug/willow").resolve()
header = "class H { pub f: closure(i64) -> i64; } fn main() { let h = new H(|x: i64| x + 1);"
for shape in ("siblings", "nested"):
    for size in (16, 64, 256):
        body = "(h.f)(1);" * size if shape == "siblings" else "(h.f)(" * size + "0" + ")" * size + ";"
        with tempfile.TemporaryDirectory() as tmp:
            source = pathlib.Path(tmp) / "calls.wi"
            source.write_text(header + body + "}")
            lir = subprocess.check_output([str(compiler), "build", str(source), "--emit-lir"], text=True)
        loads = lir.count("value: FieldLoad {")
        calls = lir.count("value: IndirectCall {")
        instructions = sum(line.startswith("  ") for line in lir.splitlines())
        assert (loads, calls) == (size, size), (shape, size, loads, calls)
        print(f"{shape} n={size} field_loads={loads} indirect_calls={calls} instructions={instructions}")
