use super::support::*;

#[test]
fn fixed_arrays_values_and_calls() {
    let source = r#"
fn identity<T>(x: T) -> T { return x; }
fn sum(xs: [i64; 2]) -> i64 { return xs[0] + xs[1]; }
fn make() -> [i64; 2] { return [20, 22]; }
class Holder { pub values: [String; 2]; }
class Nested { pub values: [[String; 1]; 1]; }
fn main() {
    let mut xs: [i64; 2] = [1, 2];
    xs[0] = 40;
    println(sum(xs));
    println(sum(make()));
    println(identity(xs)[0]);
    let empty: [i64; 0] = [];
    println(empty.len());
    let bs: [bool; 2] = [true, false];
    println(bs[0]);
    let fs: [f64; 1] = [1.5];
    println(fs[0]);
    let nested: [[i64; 2]; 2] = [[1, 2], [3, 4]];
    println(nested[1][0]);
    for x in xs { println(x); }
    let holder = new Holder(["one", "two"]);
    gc_collect();
    println(holder.values[1]);
    let nested_holder = new Nested([["nested"]]);
    println(nested_holder.values[0][0]);
    let choose: [i64; 2] = true ? [3, 4] : [5, 6];
    println(choose[0]);
}
"#;
    for run in [
        compile_and_run,
        compile_and_run_release,
        compile_and_run_gc_stress,
    ] {
        let (out, ok) = run(source);
        assert!(ok, "{out}");
        assert_eq!(out, "42\n42\n40\n0\ntrue\n1.5\n3\n40\n2\ntwo\nnested\n3\n");
    }
}

#[test]
fn fixed_arrays_reject_length_and_mutability_errors() {
    for (body, diagnostic) in [
        ("let xs: [i64; 2] = [1];", "array length mismatch"),
        ("let xs: [i64; 0] = [1];", "array length mismatch"),
        ("let xs: [i64; 1] = [];", "array length mismatch"),
        (
            "let xs: [i64; 2] = [1, 2]; xs.push(3);",
            "fixed arrays cannot be resized",
        ),
        (
            "let xs: [i64; 2] = [1, 2]; xs.pop();",
            "fixed arrays cannot be resized",
        ),
        (
            "let xs: [i64; 2] = [1, 2]; let ys: [i64; 1] = xs;",
            "[i64; 1]",
        ),
        ("let xs: [i64; 2] = [1, true];", "array element expects"),
        ("let xs: [i64; -1] = [];", "non-negative integer"),
        ("let xs: [i64; 1.5] = [];", "non-negative integer"),
        ("let n = 2; let xs: [i64; n] = [];", "non-negative integer"),
        ("let xs: [i64; 1] = [1]; xs[0] = true;", "bool"),
        (
            "let xs: [i64; 1] = [1]; println(xs[true]);",
            "index must be",
        ),
        ("let xs: [[i64; 2]; 1] = [[1]];", "array length mismatch"),
    ] {
        assert_compile_error_contains(&format!("fn main() {{ {body} }}"), &[diagnostic]);
    }
}

#[test]
fn fixed_arrays_preserve_dynamic_import_and_type_separation() {
    for source in [
        "import std::collections::Array; fn main() { let xs: [i64; 1] = [1]; let ys: Array<i64> = xs; }",
        "fn main() { let xs = [1]; let ys: [i64; 1] = xs; }",
        "fn main() { let xs: Array<i64> = [1]; }",
    ] {
        assert!(expect_compile_error(source), "{source}");
    }
}

#[test]
fn fixed_arrays_nested_calls_async_and_gc() {
    let source = r#"
import std::collections::Array;
fn first<T>(xs: [T; 1]) -> T { return xs[0]; }
fn invoke(f: fn([i64; 1]) -> i64, xs: [i64; 1]) -> i64 { return f(xs); }
fn read(xs: [i64; 1]) -> i64 { return xs[0]; }
enum Wrapped { Values([String; 1]), Empty }
async fn main() {
    let xs: [i64; 1] = [42];
    println(invoke(read, xs));
    println(first(true ? [42] : [0]));
    let strings: [String; 1] = ["kept"];
    let wrapped = Wrapped::Values(strings);
    let dynamic: Array<[String; 1]> = [strings];
    let opt: Option<[i64; 1]> = Some(xs);
    await sleep(1);
    gc_collect();
    println(dynamic[0][0]);
    match wrapped { Values(v) => println(v[0]), Empty => println("bad") }
    match opt { Some(v) => println(v[0]), None => println(0) }
    let frozen = strings.freeze();
    println(frozen[0]);
    println(strings.join(","));
    println(xs.toString());
    let f = |v: [i64; 1]| v[0];
    println(f(xs));
}
"#;
    for run in [
        compile_and_run,
        compile_and_run_release,
        compile_and_run_gc_stress,
    ] {
        let (out, ok) = run(source);
        assert!(ok, "{out}");
        assert_eq!(out, "42\n42\nkept\nkept\n42\nkept\nkept\n[42]\n42\n");
    }
}

#[test]
fn fixed_arrays_bounds_recovery() {
    let source = r#"
fn main() {
    let xs: [i64; 1] = [1];
    if true { defer match recover() { Some(_) => println("read"), None => println("bad") } println(xs[1]); }
    if true { defer match recover() { Some(_) => println("write"), None => println("bad") } xs[-1] = 2; }
}
"#;
    let (out, ok) = compile_and_run(source);
    assert!(ok, "{out}");
    assert_eq!(out, "read\nwrite\n");
}

#[test]
fn fixed_arrays_imported_payload_and_async_reference() {
    let project = TestProject::new(
        "fixed_arrays_modules",
        &[
            (
                "values.wi",
                "module values; pub enum E { Pair([i64; 2]), Empty } pub fn pair() -> E { return E::Pair([20, 22]); }",
            ),
            (
                "main.wi",
                r#"
import values;
import values::E;
async fn borrow(xs: &mut [i64; 2]) -> i64 { return xs[0]; }
async fn first(xs: [i64; 2]) -> i64 { await sleep(1); return xs[0]; }
async fn main() {
    let mut xs: [i64; 2] = [40, 2];
    println(xs[0]);
    match values::pair() { E::Pair(v) => { println(v[0] + v[1]); for x in v { println(x); } }, E::Empty => println(0) }
}
"#,
            ),
        ],
    );
    let result = project.compile("main.wi");
    assert!(
        result.status.success(),
        "{}",
        String::from_utf8_lossy(&result.stderr)
    );
    let result = project.run_with_env_timeout(
        &[("WILLOW_GC_STRESS", "alloc,scheduler")],
        std::time::Duration::from_secs(30),
    );
    assert!(
        result.status.success(),
        "{}",
        String::from_utf8_lossy(&result.stderr)
    );
    assert_eq!(String::from_utf8_lossy(&result.stdout), "40\n42\n20\n22\n");
}

#[test]
fn fixed_arrays_example() {
    let (out, ok) = compile_and_run(include_str!("../../example/fixed_arrays.wi"));
    assert!(ok, "{out}");
    assert_eq!(out, "42\nsecond\n0\n");
}

#[test]
fn fixed_arrays_generic_branch_and_enum_holes() {
    let source = r#"
fn first<T>(xs: [T; 2]) -> T { return xs[0]; }
fn consume<T>(xs: [Option<T>; 2]) -> i64 { return xs.len(); }
fn pick<T>(xs: [T; 0], value: T) -> T { return value; }
fn main() {
    println(first(true ? [1, 2] : [3, 4]));
    println(first(match true { true => [5, 6], false => [7, 8] }));
    println(consume([None, Some(42)]));
    println(consume([Some(42), None]));
    println(pick([], 42));
}
"#;
    let (out, ok) = compile_and_run(source);
    assert!(ok, "{out}");
    assert_eq!(out, "1\n5\n2\n2\n42\n");
}

#[test]
fn fixed_arrays_generic_void_is_not_an_inference_hole() {
    assert!(expect_compile_error(
        "fn noop() {} fn accept<T>(xs: [T; 2]) {} fn main() { accept([noop(), 1]); }"
    ));
}
