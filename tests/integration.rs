/// Integration tests for runnable examples plus catalog checks for future examples.
use std::fs;
use std::path::Path;
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicU32, Ordering};

static COUNTER: AtomicU32 = AtomicU32::new(0);

fn collect_wi_files(root: &str) -> Vec<String> {
    fn visit(dir: &Path, files: &mut Vec<String>) {
        for entry in fs::read_dir(dir).unwrap_or_else(|err| {
            panic!("failed to read directory {}: {err}", dir.display());
        }) {
            let path = entry.expect("failed to read directory entry").path();
            if path.is_dir() {
                visit(&path, files);
            } else if path.extension().and_then(|ext| ext.to_str()) == Some("wi") {
                files.push(path.to_string_lossy().replace('\\', "/"));
            }
        }
    }

    let mut files = Vec::new();
    visit(Path::new(root), &mut files);
    files.sort();
    files
}

fn compile_and_run(source: &str) -> (String, bool) {
    let id = COUNTER.fetch_add(1, Ordering::Relaxed);
    let src_path = format!("/tmp/willow_test_{}.wi", id);
    let bin_path = format!("/tmp/willow_test_{}", id);

    fs::write(&src_path, source).unwrap();

    let compiler = env!("CARGO_BIN_EXE_willowc");
    let status = Command::new(compiler)
        .args(["build", &src_path, "-o", &bin_path])
        .stderr(Stdio::null())
        .status()
        .expect("failed to run compiler");

    if !status.success() {
        let _ = fs::remove_file(&src_path);
        return (String::new(), false);
    }

    let out = Command::new(&bin_path)
        .output()
        .expect("failed to run binary");

    let _ = fs::remove_file(&src_path);
    let _ = fs::remove_file(&bin_path);

    (String::from_utf8_lossy(&out.stdout).into_owned(), true)
}

fn compile_file_and_run(src_path: &str) -> (String, bool) {
    compile_file_and_run_with_args(src_path, &[])
}

fn compile_file_and_run_with_args(src_path: &str, extra_args: &[&str]) -> (String, bool) {
    let id = COUNTER.fetch_add(1, Ordering::Relaxed);
    let bin_path = format!("/tmp/willow_example_test_{}", id);

    let compiler = env!("CARGO_BIN_EXE_willowc");
    let mut command = Command::new(compiler);
    command.args(["build", src_path, "-o", &bin_path]);
    command.args(extra_args);
    command.stderr(Stdio::null());
    let status = command.status().expect("failed to run compiler");

    if !status.success() {
        return (String::new(), false);
    }

    let out = Command::new(&bin_path)
        .output()
        .expect("failed to run binary");

    let _ = fs::remove_file(&bin_path);

    (String::from_utf8_lossy(&out.stdout).into_owned(), true)
}

fn compile_temp_project_and_run(files: &[(&str, &str)], entry: &str) -> (String, bool) {
    let id = COUNTER.fetch_add(1, Ordering::Relaxed);
    let dir_path = format!("/tmp/willow_project_test_{}", id);
    let bin_path = format!("/tmp/willow_project_test_{}_bin", id);

    fs::create_dir_all(&dir_path).unwrap();
    for (relative_path, source) in files {
        let path = Path::new(&dir_path).join(relative_path);
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).unwrap();
        }
        fs::write(path, source).unwrap();
    }

    let src_path = Path::new(&dir_path).join(entry);
    let compiler = env!("CARGO_BIN_EXE_willowc");
    let output = Command::new(compiler)
        .args(["build", src_path.to_str().unwrap(), "-o", &bin_path])
        .output()
        .expect("failed to run compiler");

    if !output.status.success() {
        eprintln!("{}", String::from_utf8_lossy(&output.stderr));
        let _ = fs::remove_dir_all(&dir_path);
        let _ = fs::remove_file(&bin_path);
        return (String::new(), false);
    }

    let out = Command::new(&bin_path)
        .output()
        .expect("failed to run binary");

    let _ = fs::remove_dir_all(&dir_path);
    let _ = fs::remove_file(&bin_path);

    (String::from_utf8_lossy(&out.stdout).into_owned(), true)
}

/// Compile source that is expected to fail; returns true if compiler rejected it.
fn expect_compile_error(source: &str) -> bool {
    let id = COUNTER.fetch_add(1, Ordering::Relaxed);
    let src_path = format!("/tmp/willow_err_{}.wi", id);
    let bin_path = format!("/tmp/willow_err_{}", id);

    fs::write(&src_path, source).unwrap();

    let compiler = env!("CARGO_BIN_EXE_willowc");
    let status = Command::new(compiler)
        .args(["build", &src_path, "-o", &bin_path])
        .stderr(Stdio::null())
        .status()
        .expect("failed to run compiler");

    let _ = fs::remove_file(&src_path);
    let _ = fs::remove_file(&bin_path);

    !status.success()
}

fn compile_error_stderr(source: &str) -> String {
    let id = COUNTER.fetch_add(1, Ordering::Relaxed);
    let src_path = format!("/tmp/willow_diag_{}.wi", id);
    let bin_path = format!("/tmp/willow_diag_{}", id);

    fs::write(&src_path, source).unwrap();

    let compiler = env!("CARGO_BIN_EXE_willowc");
    let out = Command::new(compiler)
        .args(["build", &src_path, "-o", &bin_path])
        .output()
        .expect("failed to run compiler");

    let _ = fs::remove_file(&src_path);
    let _ = fs::remove_file(&bin_path);

    assert!(
        !out.status.success(),
        "expected compile error, got success; stdout: {}; stderr: {}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );

    String::from_utf8_lossy(&out.stderr).into_owned()
}

fn assert_compile_error_contains(source: &str, expected_parts: &[&str]) {
    let stderr = compile_error_stderr(source);
    for part in expected_parts {
        assert!(
            stderr.contains(part),
            "stderr did not contain `{part}`:\n{stderr}"
        );
    }
}

// ── Basic output ─────────────────────────────────────────────────────────────

#[test]
fn test_println_i64() {
    let (out, ok) = compile_and_run("fn main() { println(42); }");
    assert!(ok, "compilation failed");
    assert_eq!(out.trim(), "42");
}

#[test]
fn test_print_no_newline() {
    let (out, ok) = compile_and_run("fn main() { print(1); print(2); println(3); }");
    assert!(ok, "compilation failed");
    assert_eq!(out.trim(), "123");
}

#[test]
fn test_println_bool() {
    let (out, ok) = compile_and_run("fn main() { println(true); println(false); }");
    assert!(ok, "compilation failed");
    assert_eq!(out, "true\nfalse\n");
}

#[test]
fn test_println_f64() {
    let (out, ok) = compile_and_run("fn main() { println(2.5); println(-0.5); }");
    assert!(ok, "compilation failed");
    assert_eq!(out, "2.5\n-0.5\n");
}

#[test]
fn test_print_expression_results() {
    let src = r#"
fn main() {
    print(1 + 2);
    print(3 * 4);
    println(5 == 5);
}
"#;
    let (out, ok) = compile_and_run(src);
    assert!(ok, "compilation failed");
    assert_eq!(out, "312true\n");
}

#[test]
fn test_comments_are_ignored() {
    let src = r#"
fn main() {
    // Comments can sit on their own line.
    println(1); // And after statements.
}
"#;
    let (out, ok) = compile_and_run(src);
    assert!(ok, "compilation failed");
    assert_eq!(out.trim(), "1");
}

// ── Example files ───────────────────────────────────────────────────────────

#[test]
fn test_runnable_example_files_compile_and_run() {
    let cases = [
        ("example/arithmetic.wi", "27\n15\n126\n3\n3\n54\n3\ntrue\n"),
        ("example/booleans.wi", "true\nfalse\ntrue\ntrue\n"),
        ("example/class_hierarchy.wi", "3\n"),
        ("example/class.wi", "42\n"),
        ("example/control_flow.wi", "120\n"),
        ("example/early_return.wi", "7\n0\n12\n"),
        ("example/example.wi", "50\ntrue\n"),
        ("example/fib.wi", "55\n"),
        ("example/floats.wi", "4\ntrue\n-4\n"),
        ("example/functions.wi", "25\ntrue\n"),
        ("example/hello.wi", "50"),
        ("example/mutability.wi", "6\n15\ntrue\n"),
        ("example/nested_loops.wi", "30\n"),
        ("example/print_test.wi", "1230\n42\ntrue\nfalsetrue\n"),
        ("example/recursion.wi", "3628800\n1024\n6\n"),
        ("example/ternary.wi", "1\n-1\n0\n20\n99\n15\n8\n1\n"),
        ("example/types.wi", "10\n2.5\n10\n78.5397\ntrue\n"),
    ];

    let mut expected_paths = cases
        .iter()
        .map(|(path, _)| path.to_string())
        .collect::<Vec<_>>();
    expected_paths.sort();
    let mut actual_paths = fs::read_dir("example")
        .expect("missing example directory")
        .filter_map(|entry| {
            let path = entry.expect("failed to read example entry").path();
            if path.is_file() && path.extension().and_then(|ext| ext.to_str()) == Some("wi") {
                Some(path.to_string_lossy().replace('\\', "/"))
            } else {
                None
            }
        })
        .collect::<Vec<_>>();
    actual_paths.sort();
    assert_eq!(
        actual_paths, expected_paths,
        "every root example/*.wi file should have an output assertion"
    );

    for (path, expected) in cases {
        let (out, ok) = compile_file_and_run(path);
        assert!(ok, "{path} failed to compile or run");
        assert_eq!(out, expected, "{path} output mismatch");
    }
}

#[test]
fn test_future_examples_are_documented_not_compiled() {
    let future_examples = collect_wi_files("example/future");
    assert!(
        future_examples.len() >= 10,
        "future example catalog should stay broad"
    );

    for path in future_examples {
        let source = fs::read_to_string(&path).unwrap_or_else(|err| {
            panic!("failed to read future example {path}: {err}");
        });

        assert!(
            source.contains("// status: future"),
            "{path} must be marked as a future example"
        );
        assert!(
            source.contains("// feature:"),
            "{path} must name the language feature it documents"
        );
    }
}

#[test]
fn test_future_example_catalog_covers_planned_features() {
    let combined = collect_wi_files("example/future")
        .iter()
        .map(|path| fs::read_to_string(path).unwrap())
        .collect::<Vec<_>>()
        .join("\n");

    let required_fragments = [
        "import ",
        "class ",
        "extends ",
        "String",
        "enum ",
        "match ",
        "[i64]",
        "for ",
        "nil",
        "interface ",
        "implements ",
    ];

    for fragment in required_fragments {
        assert!(
            combined.contains(fragment),
            "future examples should cover `{fragment}`"
        );
    }
}

#[test]
fn test_example_readme_explains_runnable_and_future_examples() {
    let readme = fs::read_to_string("example/README.md").expect("missing example README");

    assert!(readme.contains("Root `*.wi` files"));
    assert!(readme.contains("future/**/*.wi"));
    assert!(readme.contains("// status: future"));
}

#[test]
fn test_release_example_build_runs() {
    let (out, ok) = compile_file_and_run_with_args("example/functions.wi", &["--release"]);
    assert!(ok, "release compilation failed");
    assert_eq!(out, "25\ntrue\n");
}

#[test]
fn test_import_as_alias_module_call() {
    let math = r#"
pub fn double(x: i64) -> i64 {
    return x * 2;
}

pub fn is_positive(x: i64) -> bool {
    return x > 0;
}
"#;
    let main = r#"
import math as m;

fn main() {
    let x = m::double(21);

    println(x);
    println(m::is_positive(x));
}
"#;

    let (out, ok) =
        compile_temp_project_and_run(&[("math.wi", math), ("main.wi", main)], "main.wi");
    assert!(ok, "import alias project failed to compile or run");
    assert_eq!(out, "42\ntrue\n");
}

// ── Arithmetic ───────────────────────────────────────────────────────────────

#[test]
fn test_arithmetic_i64() {
    let src = r#"
fn main() {
    let a = 10;
    let b = 3;
    println(a + b);
    println(a - b);
    println(a * b);
    println(a / b);
    println(a % b);
}
"#;
    let (out, ok) = compile_and_run(src);
    assert!(ok, "compilation failed");
    assert_eq!(out, "13\n7\n30\n3\n1\n");
}

#[test]
fn test_arithmetic_f64() {
    let src = r#"
fn main() {
    let x: f64 = 2.5;
    let y: f64 = 4.0;
    println(x * y);
}
"#;
    let (out, ok) = compile_and_run(src);
    assert!(ok, "compilation failed");
    assert_eq!(out.trim(), "10");
}

#[test]
fn test_operator_precedence_and_parentheses() {
    let src = r#"
fn main() {
    println(1 + 2 * 3);
    println((1 + 2) * (3 + 4));
    println(20 / (3 + 2));
    println(20 % (3 + 2));
}
"#;
    let (out, ok) = compile_and_run(src);
    assert!(ok, "compilation failed");
    assert_eq!(out, "7\n21\n4\n0\n");
}

#[test]
fn test_arithmetic_left_associative() {
    let src = r#"
fn main() {
    println(20 - 5 - 3);
    println(100 / 5 / 2);
    println(29 % 10 % 4);
}
"#;
    let (out, ok) = compile_and_run(src);
    assert!(ok, "compilation failed");
    assert_eq!(out, "12\n10\n1\n");
}

#[test]
fn test_negative_values_in_expressions() {
    let src = r#"
fn main() {
    let a = -10;
    let b = 4;

    println(a + b);
    println(a * -b);
    println((a - b) / 2);
}
"#;
    let (out, ok) = compile_and_run(src);
    assert!(ok, "compilation failed");
    assert_eq!(out, "-6\n40\n-7\n");
}

#[test]
fn test_f64_comparison_and_unary_neg() {
    let src = r#"
fn main() {
    let x: f64 = -2.5;
    let y: f64 = 5.0;
    println(x < 0.0);
    println(y / 2.0);
    println(x != y);
}
"#;
    let (out, ok) = compile_and_run(src);
    assert!(ok, "compilation failed");
    assert_eq!(out, "true\n2.5\ntrue\n");
}

#[test]
fn test_f64_equality_and_false_comparisons() {
    let src = r#"
fn main() {
    let x: f64 = 1.5;
    let y: f64 = 2.5;

    println(x == y);
    println(x != y);
    println(y <= x);
    println(y > x);
}
"#;
    let (out, ok) = compile_and_run(src);
    assert!(ok, "compilation failed");
    assert_eq!(out, "false\ntrue\nfalse\ntrue\n");
}

// ── Variables ────────────────────────────────────────────────────────────────

#[test]
fn test_let_mut() {
    let src = r#"
fn main() {
    let mut a = 10;
    a = 20;
    println(a);
}
"#;
    let (out, ok) = compile_and_run(src);
    assert!(ok, "compilation failed");
    assert_eq!(out.trim(), "20");
}

#[test]
fn test_type_annotation() {
    let src = r#"
fn main() {
    let x: i64 = 99;
    println(x);
}
"#;
    let (out, ok) = compile_and_run(src);
    assert!(ok, "compilation failed");
    assert_eq!(out.trim(), "99");
}

#[test]
fn test_mutable_f64_assignment() {
    let src = r#"
fn main() {
    let mut x: f64 = 1.5;
    x = x + 2.5;

    println(x);
}
"#;
    let (out, ok) = compile_and_run(src);
    assert!(ok, "compilation failed");
    assert_eq!(out.trim(), "4");
}

#[test]
fn test_block_scope_shadowing_restores_outer_binding() {
    let src = r#"
fn main() {
    let x = 1;

    if true {
        let x = 2;
        println(x);
    }

    println(x);
}
"#;
    let (out, ok) = compile_and_run(src);
    assert!(ok, "compilation failed");
    assert_eq!(out, "2\n1\n");
}

#[test]
fn test_nested_block_shadowing_restores_each_outer_binding() {
    let src = r#"
fn main() {
    let x = 1;

    if true {
        let x = 2;

        if true {
            let x = 3;
            println(x);
        }

        println(x);
    }

    println(x);
}
"#;
    let (out, ok) = compile_and_run(src);
    assert!(ok, "compilation failed");
    assert_eq!(out, "3\n2\n1\n");
}

// ── Control flow ─────────────────────────────────────────────────────────────

#[test]
fn test_if_else() {
    let src = r#"
fn main() {
    let x = 5;
    if x > 3 {
        println(1);
    } else {
        println(0);
    }
}
"#;
    let (out, ok) = compile_and_run(src);
    assert!(ok, "compilation failed");
    assert_eq!(out.trim(), "1");
}

#[test]
fn test_if_without_else() {
    let src = r#"
fn main() {
    let mut value = 1;

    if true {
        value = value + 41;
    }

    println(value);
}
"#;
    let (out, ok) = compile_and_run(src);
    assert!(ok, "compilation failed");
    assert_eq!(out.trim(), "42");
}

#[test]
fn test_while_loop() {
    let src = r#"
fn main() {
    let mut i = 0;
    while i < 5 {
        println(i);
        i = i + 1;
    }
}
"#;
    let (out, ok) = compile_and_run(src);
    assert!(ok, "compilation failed");
    assert_eq!(out, "0\n1\n2\n3\n4\n");
}

#[test]
fn test_while_zero_iterations() {
    let src = r#"
fn main() {
    let mut count = 0;

    while false {
        count = count + 1;
    }

    println(count);
}
"#;
    let (out, ok) = compile_and_run(src);
    assert!(ok, "compilation failed");
    assert_eq!(out.trim(), "0");
}

#[test]
fn test_nested_if_inside_while() {
    let src = r#"
fn main() {
    let mut i = 0;
    let mut total = 0;

    while i < 6 {
        if i % 2 == 0 {
            total = total + i;
        } else {
            total = total + 1;
        }
        i = i + 1;
    }

    println(total);
}
"#;
    let (out, ok) = compile_and_run(src);
    assert!(ok, "compilation failed");
    assert_eq!(out.trim(), "9");
}

#[test]
fn test_while_factorial_accumulator() {
    let src = r#"
fn main() {
    let mut n = 1;
    let mut acc = 1;

    while n <= 6 {
        acc = acc * n;
        n = n + 1;
    }

    println(acc);
}
"#;
    let (out, ok) = compile_and_run(src);
    assert!(ok, "compilation failed");
    assert_eq!(out.trim(), "720");
}

#[test]
fn test_bool_condition_from_expression() {
    let src = r#"
fn main() {
    let a = 10;
    let b = 20;

    if (a < b && b == 20) || false {
        println(1);
    } else {
        println(0);
    }
}
"#;
    let (out, ok) = compile_and_run(src);
    assert!(ok, "compilation failed");
    assert_eq!(out.trim(), "1");
}

// ── Functions ────────────────────────────────────────────────────────────────

#[test]
fn test_function_call() {
    let src = r#"
fn add(a: i64, b: i64) -> i64 {
    return a + b;
}
fn main() {
    println(add(10, 32));
}
"#;
    let (out, ok) = compile_and_run(src);
    assert!(ok, "compilation failed");
    assert_eq!(out.trim(), "42");
}

#[test]
fn test_nested_calls_and_bool_return() {
    let src = r#"
fn midpoint(a: f64, b: f64) -> f64 {
    return (a + b) / 2.0;
}

fn above_midpoint(a: f64, b: f64, limit: f64) -> bool {
    return midpoint(a, b) > limit;
}

fn main() {
    println(midpoint(3.0, 5.0));
    println(above_midpoint(3.0, 5.0, 3.5));
    println(above_midpoint(3.0, 5.0, 4.0));
}
"#;
    let (out, ok) = compile_and_run(src);
    assert!(ok, "compilation failed");
    assert_eq!(out, "4\ntrue\nfalse\n");
}

#[test]
fn test_recursive_fib() {
    let src = r#"
fn fib(n: i64) -> i64 {
    if n <= 1 {
        return n;
    }
    return fib(n - 1) + fib(n - 2);
}
fn main() {
    println(fib(10));
}
"#;
    let (out, ok) = compile_and_run(src);
    assert!(ok, "compilation failed");
    assert_eq!(out.trim(), "55");
}

#[test]
fn test_recursive_factorial_function() {
    let src = r#"
fn factorial(n: i64) -> i64 {
    if n <= 1 {
        return 1;
    }

    return n * factorial(n - 1);
}

fn main() {
    println(factorial(6));
}
"#;
    let (out, ok) = compile_and_run(src);
    assert!(ok, "compilation failed");
    assert_eq!(out.trim(), "720");
}

#[test]
fn test_mutual_recursion() {
    let src = r#"
fn is_even(n: i64) -> bool {
    if n == 0 {
        return true;
    }

    return is_odd(n - 1);
}

fn is_odd(n: i64) -> bool {
    if n == 0 {
        return false;
    }

    return is_even(n - 1);
}

fn main() {
    println(is_even(8));
    println(is_odd(8));
    println(is_odd(9));
}
"#;
    let (out, ok) = compile_and_run(src);
    assert!(ok, "compilation failed");
    assert_eq!(out, "true\nfalse\ntrue\n");
}

#[test]
fn test_pub_function() {
    let src = r#"
pub fn double(x: i64) -> i64 {
    return x * 2;
}
fn main() {
    println(double(21));
}
"#;
    let (out, ok) = compile_and_run(src);
    assert!(ok, "compilation failed");
    assert_eq!(out.trim(), "42");
}

#[test]
fn test_forward_function_call() {
    let src = r#"
fn main() {
    println(triple(14));
}

fn triple(x: i64) -> i64 {
    return x * 3;
}
"#;
    let (out, ok) = compile_and_run(src);
    assert!(ok, "compilation failed");
    assert_eq!(out.trim(), "42");
}

#[test]
fn test_return_from_both_if_branches() {
    let src = r#"
fn sign(n: i64) -> i64 {
    if n < 0 {
        return -1;
    } else {
        return 1;
    }
}

fn main() {
    println(sign(-8));
    println(sign(8));
}
"#;
    let (out, ok) = compile_and_run(src);
    assert!(ok, "compilation failed");
    assert_eq!(out, "-1\n1\n");
}

#[test]
fn test_function_returning_bool_used_as_if_condition() {
    let src = r#"
fn in_range(value: i64, min: i64, max: i64) -> bool {
    return value >= min && value <= max;
}

fn main() {
    if in_range(7, 1, 10) {
        println(1);
    } else {
        println(0);
    }
}
"#;
    let (out, ok) = compile_and_run(src);
    assert!(ok, "compilation failed");
    assert_eq!(out.trim(), "1");
}

#[test]
fn test_void_function_without_explicit_return() {
    let src = r#"
fn emit_twice(value: i64) {
    println(value);
    println(value);
}

fn main() {
    emit_twice(9);
}
"#;
    let (out, ok) = compile_and_run(src);
    assert!(ok, "compilation failed");
    assert_eq!(out, "9\n9\n");
}

#[test]
fn test_void_function_and_early_return() {
    let src = r#"
fn emit(flag: bool) {
    if flag {
        println(1);
        return;
    }

    println(0);
}

fn main() {
    emit(true);
    emit(false);
}
"#;
    let (out, ok) = compile_and_run(src);
    assert!(ok, "compilation failed");
    assert_eq!(out, "1\n0\n");
}

// ── Classes ─────────────────────────────────────────────────────────────────

#[test]
fn test_class_declarations_parse_with_top_level_main() {
    let src = r#"
pub open class Animal {
    age: i64;

    pub open fn speak(self) -> i64 {
        return 1;
    }
}

pub class Dog extends Animal {
    pub override fn speak(self) -> i64 {
        return 2;
    }
}

fn main() {
    println(42);
}
"#;
    let (out, ok) = compile_and_run(src);
    assert!(ok, "compilation failed");
    assert_eq!(out.trim(), "42");
}

// ── Boolean operators ─────────────────────────────────────────────────────────

#[test]
fn test_bool_operators() {
    let src = r#"
fn main() {
    println(true && false);
    println(true || false);
    println(!true);
}
"#;
    let (out, ok) = compile_and_run(src);
    assert!(ok, "compilation failed");
    assert_eq!(out, "false\ntrue\nfalse\n");
}

#[test]
fn test_bool_operator_precedence() {
    let src = r#"
fn main() {
    println(true || false && false);
    println((true || false) && false);
    println(!false && true);
}
"#;
    let (out, ok) = compile_and_run(src);
    assert!(ok, "compilation failed");
    assert_eq!(out, "true\nfalse\ntrue\n");
}

// ── Comparisons ───────────────────────────────────────────────────────────────

#[test]
fn test_comparisons() {
    let src = r#"
fn main() {
    println(1 == 1);
    println(1 != 2);
    println(3 < 5);
    println(5 <= 5);
    println(6 > 4);
    println(7 >= 7);
}
"#;
    let (out, ok) = compile_and_run(src);
    assert!(ok, "compilation failed");
    assert_eq!(out, "true\ntrue\ntrue\ntrue\ntrue\ntrue\n");
}

#[test]
fn test_comparison_false_cases() {
    let src = r#"
fn main() {
    println(1 == 2);
    println(1 != 1);
    println(3 < 2);
    println(5 <= 4);
    println(6 > 9);
    println(7 >= 8);
}
"#;
    let (out, ok) = compile_and_run(src);
    assert!(ok, "compilation failed");
    assert_eq!(out, "false\nfalse\nfalse\nfalse\nfalse\nfalse\n");
}

// ── Negative numbers ──────────────────────────────────────────────────────────

#[test]
fn test_unary_neg() {
    let src = r#"
fn main() {
    let x = -42;
    println(x);
}
"#;
    let (out, ok) = compile_and_run(src);
    assert!(ok, "compilation failed");
    assert_eq!(out.trim(), "-42");
}

// ── Compile error cases ───────────────────────────────────────────────────────

#[test]
fn test_diagnostics_include_codes_and_help() {
    let cases: Vec<(&str, &[&str])> = vec![
        (
            r#"
fn main() {
    let x = 1;
    x = 2;
}
"#,
            &[
                "error[E0301]",
                "cannot assign to immutable variable `x`",
                "help: declare it as mutable",
            ],
        ),
        (
            r#"
fn main() {
    let x = 1
}
"#,
            &["error[E0101]", "expected `;` after statement"],
        ),
        (
            r#"
fn main() {
    let x: i64 = true;
}
"#,
            &["error[E0201]", "expected `i64`, found `bool`"],
        ),
        (
            r#"
fn main() {
    if 1 {
        println(1);
    }
}
"#,
            &[
                "error[E0203]",
                "condition must be `bool`",
                "help: use an explicit comparison",
            ],
        ),
        (
            r#"
fn main() {
    println(missing);
}
"#,
            &["error[E0350]", "cannot find variable `missing`"],
        ),
        (
            r#"
fn main() {
    let @x = 1;
}
"#,
            &["error[E0050]", "invalid character `@`"],
        ),
        (
            r#"
fn add(a: i64, b: i64) -> i64 {
    return a + b;
}

fn main() {
    println(add(1));
}
"#,
            &[
                "error[E0201]",
                "function `add` takes 2 argument(s) but 1 were supplied",
                "wrong number of arguments",
            ],
        ),
        (
            r#"
fn main() {
    missing();
}
"#,
            &[
                "error[E0350]",
                "cannot find function `missing`",
                "not found in this scope",
            ],
        ),
        (
            r#"
fn main() {
    println("hello);
}
"#,
            &[
                "error[E0051]",
                "unterminated string literal",
                "string starts here but never ends",
            ],
        ),
    ];

    for (source, expected_parts) in cases {
        let stderr = compile_error_stderr(source);
        for part in expected_parts {
            assert!(
                stderr.contains(part),
                "stderr did not contain `{part}`:\n{stderr}"
            );
        }
    }
}

#[test]
fn test_diagnostic_wrong_argument_type_details() {
    assert_compile_error_contains(
        r#"
fn takes_i64(x: i64) -> i64 {
    return x;
}

fn main() {
    println(takes_i64(true));
}
"#,
        &["error[E0201]", "expected `i64`", "found `bool`"],
    );
}

#[test]
fn test_diagnostic_return_type_mismatch_details() {
    assert_compile_error_contains(
        r#"
fn bad() -> i64 {
    return false;
}

fn main() {
    println(1);
}
"#,
        &["error[E0201]", "expected `i64`", "found `bool`"],
    );
}

#[test]
fn test_diagnostic_arithmetic_type_mismatch_details() {
    assert_compile_error_contains(
        r#"
fn main() {
    println(1 + true);
}
"#,
        &["error[E0202]", "operator `+`", "`i64`", "`bool`"],
    );
}

#[test]
fn test_diagnostic_logical_operand_details() {
    assert_compile_error_contains(
        r#"
fn main() {
    println(true && 1);
}
"#,
        &["error[E0202]", "logical operator requires `bool` operands"],
    );
}

#[test]
fn test_diagnostic_unary_not_requires_bool_details() {
    assert_compile_error_contains(
        r#"
fn main() {
    println(!1);
}
"#,
        &["error[E0202]", "unary `!`", "requires `bool`"],
    );
}

#[test]
fn test_diagnostic_while_condition_details() {
    assert_compile_error_contains(
        r#"
fn main() {
    while 1 {
        println(1);
    }
}
"#,
        &["error[E0203]", "condition must be `bool`", "found `i64`"],
    );
}

#[test]
fn test_diagnostic_assignment_type_mismatch_details() {
    assert_compile_error_contains(
        r#"
fn main() {
    let mut x = 1;
    x = false;
}
"#,
        &["error[E0201]", "expected `i64`", "found `bool`"],
    );
}

#[test]
fn test_diagnostic_immutable_assignment_points_to_declaration() {
    assert_compile_error_contains(
        r#"
fn main() {
    let x = 1;
    x = 2;
}
"#,
        &[
            "error[E0301]",
            "cannot assign to immutable variable `x`",
            "4 |     x = 2;",
            "^ cannot assign",
            "3 |     let x = 1;",
            "declared immutable here",
            "help: declare it as mutable: `let mut x = ...`",
        ],
    );
}

#[test]
fn test_diagnostic_parameter_assignment_has_parameter_code() {
    assert_compile_error_contains(
        r#"
fn reset(x: i64) {
    x = 0;
}

fn main() {
    reset(1);
}
"#,
        &[
            "error[E0302]",
            "cannot assign to immutable parameter `x`",
            "introduce a mutable local variable",
        ],
    );
}

#[test]
fn test_diagnostic_duplicate_variable_points_to_redefinition() {
    assert_compile_error_contains(
        r#"
fn main() {
    let x = 1;
    let x = 2;
    println(x);
}
"#,
        &[
            "error[E0351]",
            "variable `x` is already defined",
            "previous definition",
        ],
    );
}

#[test]
fn test_diagnostic_local_scope_does_not_leak() {
    assert_compile_error_contains(
        r#"
fn main() {
    if true {
        let inside = 1;
    }

    println(inside);
}
"#,
        &["error[E0350]", "cannot find variable `inside`"],
    );
}

#[test]
fn test_diagnostic_invalid_character_is_source_aware() {
    assert_compile_error_contains(
        r#"
fn main() {
    let @x = 1;
}
"#,
        &[
            "error[E0050]",
            "invalid character `@`",
            " --> /tmp/willow_diag_",
            ":3:9",
            "3 |     let @x = 1;",
            "        ^ invalid character",
        ],
    );
}

#[test]
fn test_diagnostic_single_ampersand_is_invalid() {
    assert_compile_error_contains(
        r#"
fn main() {
    println(true & false);
}
"#,
        &[
            "error[E0050]",
            "invalid character `&`",
            ":3:18",
            "3 |     println(true & false);",
            "                 ^ invalid character",
        ],
    );
}

#[test]
fn test_diagnostic_single_pipe_is_invalid() {
    assert_compile_error_contains(
        r#"
fn main() {
    println(true | false);
}
"#,
        &[
            "error[E0050]",
            "invalid character `|`",
            ":3:18",
            "3 |     println(true | false);",
            "                 ^ invalid character",
        ],
    );
}

#[test]
fn test_diagnostic_unterminated_string_is_source_aware() {
    assert_compile_error_contains(
        r#"
fn main() {
    println("hello);
}
"#,
        &[
            "error[E0051]",
            "unterminated string literal",
            ":3:13",
            "3 |     println(\"hello);",
            "string starts here but never ends",
        ],
    );
}

#[test]
fn test_diagnostic_missing_return_value_from_non_void_function() {
    assert_compile_error_contains(
        r#"
fn bad() -> i64 {
    return;
}

fn main() {
    println(1);
}
"#,
        &["error[E0201]", "expected `i64`", "found `void`"],
    );
}

#[test]
fn test_diagnostic_return_value_from_void_function() {
    assert_compile_error_contains(
        r#"
fn bad() {
    return 1;
}

fn main() {
    bad();
}
"#,
        &["error[E0201]", "expected `void`", "found `i64`"],
    );
}

#[test]
fn test_error_immutable_assign() {
    assert!(expect_compile_error(
        r#"
fn main() {
    let x = 1;
    x = 2;
}
"#
    ));
}

#[test]
fn test_error_type_mismatch() {
    assert!(expect_compile_error(
        r#"
fn main() {
    let x: i64 = true;
}
"#
    ));
}

#[test]
fn test_error_condition_not_bool() {
    assert!(expect_compile_error(
        r#"
fn main() {
    let x = 5;
    if x { println(x); }
}
"#
    ));
}

#[test]
fn test_error_undefined_variable() {
    assert!(expect_compile_error(
        r#"
fn main() {
    println(undefined_var);
}
"#
    ));
}

#[test]
fn test_error_invalid_char() {
    assert!(expect_compile_error(
        r#"
fn main() {
    let @x = 1;
}
"#
    ));
}

#[test]
fn test_error_missing_semicolon() {
    assert!(expect_compile_error(
        r#"
fn main() {
    let x = 1
}
"#
    ));
}

#[test]
fn test_error_wrong_argument_count() {
    assert!(expect_compile_error(
        r#"
fn add(a: i64, b: i64) -> i64 {
    return a + b;
}

fn main() {
    println(add(1));
}
"#
    ));
}

#[test]
fn test_error_wrong_argument_type() {
    assert!(expect_compile_error(
        r#"
fn takes_i64(x: i64) -> i64 {
    return x;
}

fn main() {
    println(takes_i64(true));
}
"#
    ));
}

#[test]
fn test_error_unknown_function() {
    assert!(expect_compile_error(
        r#"
fn main() {
    missing();
}
"#
    ));
}

#[test]
fn test_error_return_type_mismatch() {
    assert!(expect_compile_error(
        r#"
fn bad() -> i64 {
    return false;
}

fn main() {
    println(1);
}
"#
    ));
}

#[test]
fn test_error_arithmetic_type_mismatch() {
    assert!(expect_compile_error(
        r#"
fn main() {
    println(1 + true);
}
"#
    ));
}

#[test]
fn test_error_logical_operand_not_bool() {
    assert!(expect_compile_error(
        r#"
fn main() {
    println(true && 1);
}
"#
    ));
}

#[test]
fn test_error_unary_not_requires_bool() {
    assert!(expect_compile_error(
        r#"
fn main() {
    println(!1);
}
"#
    ));
}

#[test]
fn test_error_while_condition_not_bool() {
    assert!(expect_compile_error(
        r#"
fn main() {
    while 1 {
        println(1);
    }
}
"#
    ));
}

#[test]
fn test_error_assign_to_parameter() {
    assert!(expect_compile_error(
        r#"
fn reset(x: i64) {
    x = 0;
}

fn main() {
    reset(1);
}
"#
    ));
}

#[test]
fn test_error_unterminated_string() {
    assert!(expect_compile_error(
        r#"
fn main() {
    println("hello);
}
"#
    ));
}

// ── Multiple independent diagnostics ─────────────────────────────────────────

#[test]
fn test_multiple_type_errors_in_different_functions_all_reported() {
    // Two independent arithmetic type mismatches in separate functions.
    // Both should be reported in a single compile run.
    let stderr = compile_error_stderr(
        r#"
fn bad_add(a: i64, b: bool) -> i64 {
    return a + b;
}

fn bad_mul(x: bool, y: i64) -> i64 {
    return x * y;
}

fn main() {
    bad_add(1, true);
    bad_mul(false, 2);
}
"#,
    );
    let error_count = stderr.matches("error[").count();
    assert!(
        error_count >= 2,
        "expected at least 2 independent errors, got {error_count}:\n{stderr}"
    );
}

#[test]
fn test_multiple_parse_errors_all_reported() {
    // Two functions with missing closing braces.
    // The parser recovers after each item and should surface both errors.
    let stderr = compile_error_stderr(
        r#"
fn a() -> i64 {
    return @

fn b() -> i64 {
    return #
}

fn main() {}
"#,
    );
    let error_count = stderr.matches("error[").count();
    assert!(
        error_count >= 2,
        "expected at least 2 parse errors, got {error_count}:\n{stderr}"
    );
}

#[test]
fn test_parse_errors_and_type_errors_both_reported() {
    // One function has a parse error (missing RHS); another has a type error.
    // The pipeline should continue past the parse error and report both.
    let stderr = compile_error_stderr(
        r#"
fn bad_parse() -> i64 {
    return 1 +;
}

fn bad_type(x: bool) -> i64 {
    return x + 1;
}

fn main() {}
"#,
    );
    let error_count = stderr.matches("error[").count();
    assert!(
        error_count >= 2,
        "expected parse error and type error both reported, got {error_count}:\n{stderr}"
    );
}
