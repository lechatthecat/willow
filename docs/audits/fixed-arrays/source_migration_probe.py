#!/usr/bin/env python3
"""Reproduce the current generic source-method boundary without private inputs."""
import pathlib
import subprocess
import tempfile

root = pathlib.Path(__file__).resolve().parents[3]
compiler = root / "target/debug/willow"
cases = [
    ("ordinary_generic", """
class Buffer<T> { pub value: T; pub fn get(self) -> T { return self.value; } }
fn main() { let b = new Buffer<i64>(42); println(b.get()); }
""", None),
    ("generic_render", """
class Buffer<T> { pub value: T; pub fn render(self) -> String { return self.value.toString(); } }
fn main() { let b = new Buffer<i64>(42); println(b.render()); }
""", "type parameter `T` has no method `toString`"),
    ("generic_string_only_method", """
class Buffer<T> { pub value: T; pub fn join(self, separator: String) -> String { return self.value + separator; } }
fn main() { let b = new Buffer<String>("value"); println(b.join(",")); }
""", "error[E0202]"),
    ("constrained_render", """
class Buffer<T> { pub value: T; pub fn render() -> String where T: i64 | f64 | bool | String { return self.value.toString(); } }
fn main() { println(new Buffer<i64>(42).render()); }
""", None),
    ("constrained_string_method", """
class Buffer<T> { pub value: T; pub fn join(separator: String) -> String where T: String { return self.value + separator; } }
fn main() { println(new Buffer<String>("value").join(",")); }
""", None),
    ("source_array_methods", """
import std::collections::Array;
fn main() { let xs: Array<String> = Array::new(); xs.push("value"); println(xs.join(",")); println(xs.toString()); println(xs.freeze()[0]); println(xs.pop()); }
""", None),
]
with tempfile.TemporaryDirectory(prefix="source-array-methods-") as directory:
    for name, source, expected in cases:
        path = pathlib.Path(directory) / f"{name}.wi"
        path.write_text(source)
        result = subprocess.run([str(compiler), "check", str(path)], text=True, capture_output=True)
        if expected is None:
            assert result.returncode == 0, result.stderr
        else:
            assert result.returncode != 0 and expected in result.stderr, result.stderr
        messages = [line for line in result.stderr.splitlines() if line.startswith("error[")]
        print(f"{name}: exit={result.returncode}")
        for message in messages:
            print(message)
