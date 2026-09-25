use super::*;

// ── Spawn / task: additional type and behaviour coverage ────────────────────

/// A void-returning async function can be started and awaited without a value.
#[test]
fn test_spawn_void_function_await_completes() {
    let (out, ok) = compile_and_run(
        r#"
async fn say() {
    println("hi");
}

async fn main() {
    let h = say();
    await h;
    println("done");
}
"#,
    );
    assert!(ok, "void task await should compile and run");
    assert_eq!(out, "hi\ndone\n");
}

/// An async function returning bool produces the correct value when awaited.
#[test]
fn test_spawn_bool_return_await_value() {
    let (out, ok) = compile_and_run(
        r#"
async fn is_even(x: i64) -> bool {
    return x % 2 == 0;
}

async fn main() {
    let h1 = is_even(4);
    let h2 = is_even(7);
    println(await h1);
    println(await h2);
}
"#,
    );
    assert!(ok, "bool-return task await should compile and run");
    assert_eq!(out, "true\nfalse\n");
}

/// An async function returning f64 produces the correct value when awaited.
#[test]
fn test_spawn_f64_return_await_value() {
    let (out, ok) = compile_and_run(
        r#"
async fn half(x: f64) -> f64 {
    return x / 2.0;
}

async fn main() {
    let h = half(10.0);
    let r = await h;
    println(r);
}
"#,
    );
    assert!(ok, "f64-return task await should compile and run");
    assert_eq!(out.trim(), "5");
}

/// Function with three i64 parameters can be spawned; all args are forwarded.
#[test]
fn test_spawn_three_argument_function() {
    let (out, ok) = compile_and_run(
        r#"
async fn sum3(a: i64, b: i64, c: i64) -> i64 {
    return a + b + c;
}

async fn main() {
    let h = sum3(10, 20, 30);
    println(await h);
}
"#,
    );
    assert!(ok, "three-arg spawn should compile and run");
    assert_eq!(out, "60\n");
}

/// Awaited task results can be used directly inside an arithmetic expression.
#[test]
fn test_spawn_await_result_used_in_expression() {
    let (out, ok) = compile_and_run(
        r#"
async fn square(x: i64) -> i64 {
    return x * x;
}

async fn main() {
    let a = square(3);
    let b = square(4);
    println(await a + await b);
}
"#,
    );
    assert!(ok, "await result in expression should compile and run");
    assert_eq!(out, "25\n");
}

/// The same function can be spawned multiple times; each task is independent.
#[test]
fn test_spawn_same_function_twice_produces_independent_results() {
    let (out, ok) = compile_and_run(
        r#"
async fn double(x: i64) -> i64 {
    return x * 2;
}

async fn main() {
    let h1 = double(5);
    let h2 = double(6);
    println(await h1);
    println(await h2);
}
"#,
    );
    assert!(ok, "two spawns of same function should compile and run");
    assert_eq!(out, "10\n12\n");
}

/// Release-mode task await produces the same output as debug mode.
#[test]
fn test_spawn_in_release_mode_produces_correct_output() {
    let id = unique_test_id();
    let src_path = temp_path(format!("willow_spawn_rel_{}.wi", id));
    let bin_path = temp_path(format!("willow_spawn_rel_{}", id));

    let source = r#"
async fn square(x: i64) -> i64 { return x * x; }
async fn main() {
    let h = square(7);
    println(await h);
}
"#;
    std::fs::write(&src_path, source).unwrap();

    let compiler = env!("CARGO_BIN_EXE_willow");
    let output = std::process::Command::new(compiler)
        .args(["build", &src_path, "-o", &bin_path, "--release"])
        .output()
        .expect("failed to compile");

    assert!(
        output.status.success(),
        "release spawn build should succeed"
    );

    let run = std::process::Command::new(&bin_path)
        .output()
        .expect("failed to run binary");

    let _ = std::fs::remove_file(&src_path);
    let _ = std::fs::remove_file(&bin_path);
    let _ = std::fs::remove_file(format!("{bin_path}.wsmap"));

    assert!(run.status.success(), "release spawn binary should run");
    assert_eq!(
        String::from_utf8_lossy(&run.stdout).trim(),
        "49",
        "release spawn should produce correct output"
    );
}

/// Awaiting a non-awaitable type (e.g. i64) in value position must be an error.
#[test]
fn test_await_of_non_awaitable_in_value_position_reports_e0803() {
    assert_compile_error_contains(
        r#"
async fn main() {
    let x: i64 = 42;
    println(await x);
}
"#,
        &[
            "error[E0803]",
            "cannot await value of type `i64`",
            "await only `Task<T>` values",
        ],
    );
}
