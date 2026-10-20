"""Reproduce generated-code counts; run from the repository root."""
import pathlib
import subprocess
import tempfile

compiler = pathlib.Path("target/debug/willow").resolve()
print("lambdas,branches,lambda_definitions,enum_allocations,value_returns")
for count, branches in [(1, 1), (8, 1), (32, 1), (1, 8), (1, 32), (8, 8)]:
    body = 'Err("end")'
    for branch in range(branches - 1):
        body = f"a == {branch} ? Ok(a) : ({body})"
    bindings = "\n".join(
        f"let f{i}: closure(i64) -> Result<i64, String> = |a| {body};"
        for i in range(count)
    )
    source = f"fn main() {{ {bindings} }}"
    with tempfile.TemporaryDirectory(prefix="willow-lambda-count-") as temporary:
        path = pathlib.Path(temporary) / "main.wi"
        path.write_text(source)
        result = subprocess.run([str(compiler), "build", str(path), "--emit-lir"],
                                text=True, capture_output=True, check=True)
    lir = result.stdout
    actual = (lir.count("fn $lambda@"), lir.count("value: EnumAlloc {"),
              lir.count("Return(Some("))
    assert actual == (count, count * branches, count), (count, branches, actual)
    print(count, branches, *actual, sep=",")
