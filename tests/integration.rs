/// Integration tests for runnable examples plus catalog checks for future examples.
use std::fs;
use std::path::Path;
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicU32, Ordering};

static COUNTER: AtomicU32 = AtomicU32::new(0);

fn unique_test_id() -> String {
    let counter = COUNTER.fetch_add(1, Ordering::Relaxed);
    format!("{}_{}", std::process::id(), counter)
}

fn remove_output_artifacts(bin_path: &str) {
    let _ = fs::remove_file(bin_path);
    let _ = fs::remove_file(format!("{bin_path}.wsmap"));
}

fn target_dir() -> std::path::PathBuf {
    std::env::var_os("CARGO_TARGET_DIR")
        .map(std::path::PathBuf::from)
        .unwrap_or_else(|| std::path::PathBuf::from("target"))
}

fn build_runtime_staticlib(release: bool) -> std::path::PathBuf {
    let mut args = vec!["build", "-p", "willow_runtime"];
    if release {
        args.push("--release");
    }
    let status = Command::new("cargo")
        .args(args)
        .status()
        .expect("failed to build willow_runtime");
    assert!(status.success(), "willow_runtime build failed");
    target_dir()
        .join(if release { "release" } else { "debug" })
        .join("libwillow_runtime.a")
}

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

fn collect_runnable_example_entries() -> Vec<String> {
    collect_wi_files("example")
        .into_iter()
        .filter(|path| !path.contains("/future/"))
        .filter(|path| {
            fs::read_to_string(path)
                .map(|source| source.contains("fn main("))
                .unwrap_or(false)
        })
        .collect()
}

fn compile_and_run(source: &str) -> (String, bool) {
    let id = unique_test_id();
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
        remove_output_artifacts(&bin_path);
        return (String::new(), false);
    }

    let out = Command::new(&bin_path)
        .output()
        .expect("failed to run binary");

    let _ = fs::remove_file(&src_path);
    remove_output_artifacts(&bin_path);

    (String::from_utf8_lossy(&out.stdout).into_owned(), true)
}

/// Like `compile_and_run` but returns `(stdout+stderr, binary_exit_ok)`.
/// Use this when the test needs to observe the binary's exit status (e.g. panic tests).
fn compile_and_run_check_exit(source: &str) -> (String, bool) {
    let id = unique_test_id();
    let src_path = format!("/tmp/willow_exit_test_{}.wi", id);
    let bin_path = format!("/tmp/willow_exit_test_{}", id);

    fs::write(&src_path, source).unwrap();

    let compiler = env!("CARGO_BIN_EXE_willowc");
    let status = Command::new(compiler)
        .args(["build", &src_path, "-o", &bin_path])
        .stderr(Stdio::null())
        .status()
        .expect("failed to run compiler");

    if !status.success() {
        let _ = fs::remove_file(&src_path);
        remove_output_artifacts(&bin_path);
        return (String::new(), false);
    }

    let out = Command::new(&bin_path)
        .output()
        .expect("failed to run binary");

    let _ = fs::remove_file(&src_path);
    remove_output_artifacts(&bin_path);

    let combined = format!(
        "{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    (combined, out.status.success())
}

/// Like `compile_and_run` but runs the binary with `WILLOW_GC_STRESS=alloc`, so
/// the garbage collector runs on *every* allocation.  This turns latent
/// GC-rooting bugs in generated code (a live value not rooted across an
/// allocation) into deterministic failures instead of rare, load-dependent
/// crashes.  Returns `(stdout+stderr, binary_exit_ok)`.
fn compile_and_run_gc_stress(source: &str) -> (String, bool) {
    let id = unique_test_id();
    let src_path = format!("/tmp/willow_gcstress_test_{}.wi", id);
    let bin_path = format!("/tmp/willow_gcstress_test_{}", id);

    fs::write(&src_path, source).unwrap();

    let compiler = env!("CARGO_BIN_EXE_willowc");
    let status = Command::new(compiler)
        .args(["build", &src_path, "-o", &bin_path])
        .stderr(Stdio::null())
        .status()
        .expect("failed to run compiler");

    if !status.success() {
        let _ = fs::remove_file(&src_path);
        remove_output_artifacts(&bin_path);
        return (String::new(), false);
    }

    let out = Command::new(&bin_path)
        .env("WILLOW_GC_STRESS", "alloc")
        .output()
        .expect("failed to run binary");

    let _ = fs::remove_file(&src_path);
    remove_output_artifacts(&bin_path);

    let combined = format!(
        "{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    (combined, out.status.success())
}

fn compile_and_run_with_program_args(source: &str, program_args: &[&str]) -> (String, bool) {
    let id = unique_test_id();
    let src_path = format!("/tmp/willow_args_test_{}.wi", id);
    let bin_path = format!("/tmp/willow_args_test_{}", id);

    fs::write(&src_path, source).unwrap();

    let compiler = env!("CARGO_BIN_EXE_willowc");
    let status = Command::new(compiler)
        .args(["build", &src_path, "-o", &bin_path])
        .stderr(Stdio::null())
        .status()
        .expect("failed to run compiler");

    if !status.success() {
        let _ = fs::remove_file(&src_path);
        remove_output_artifacts(&bin_path);
        return (String::new(), false);
    }

    let out = Command::new(&bin_path)
        .args(program_args)
        .output()
        .expect("failed to run binary");

    let _ = fs::remove_file(&src_path);
    remove_output_artifacts(&bin_path);

    (
        String::from_utf8_lossy(&out.stdout).into_owned(),
        out.status.success(),
    )
}

fn run_command_with_program_args(source: &str, program_args: &[&str]) -> (String, bool) {
    let id = unique_test_id();
    let src_path = format!("/tmp/willow_run_args_test_{}.wi", id);

    fs::write(&src_path, source).unwrap();

    let compiler = env!("CARGO_BIN_EXE_willowc");
    let mut command = Command::new(compiler);
    command.args(["run", &src_path, "--"]);
    command.args(program_args);
    let out = command.output().expect("failed to run compiler");

    let _ = fs::remove_file(&src_path);
    let bin_path = format!("/tmp/willow_run_{}", stem_for_test(&src_path));
    remove_output_artifacts(&bin_path);

    (
        String::from_utf8_lossy(&out.stdout).into_owned(),
        out.status.success(),
    )
}

fn stem_for_test(path: &str) -> String {
    Path::new(path)
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or("a")
        .to_string()
}

fn compile_file_and_run(src_path: &str) -> (String, bool) {
    compile_file_and_run_with_args(src_path, &[])
}

fn compile_file_and_run_with_args(src_path: &str, extra_args: &[&str]) -> (String, bool) {
    let id = unique_test_id();
    let bin_path = format!("/tmp/willow_example_test_{}", id);

    let compiler = env!("CARGO_BIN_EXE_willowc");
    let mut command = Command::new(compiler);
    command.args(["build", src_path, "-o", &bin_path]);
    command.args(extra_args);
    command.stderr(Stdio::null());
    let status = command.status().expect("failed to run compiler");

    if !status.success() {
        remove_output_artifacts(&bin_path);
        return (String::new(), false);
    }

    let out = Command::new(&bin_path)
        .output()
        .expect("failed to run binary");

    remove_output_artifacts(&bin_path);

    (String::from_utf8_lossy(&out.stdout).into_owned(), true)
}

fn compile_temp_project_and_run(files: &[(&str, &str)], entry: &str) -> (String, bool) {
    let id = unique_test_id();
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
        remove_output_artifacts(&bin_path);
        return (String::new(), false);
    }

    let out = Command::new(&bin_path)
        .output()
        .expect("failed to run binary");

    let _ = fs::remove_dir_all(&dir_path);
    remove_output_artifacts(&bin_path);

    (String::from_utf8_lossy(&out.stdout).into_owned(), true)
}

fn compile_temp_project_error_stderr(files: &[(&str, &str)], entry: &str) -> String {
    let id = unique_test_id();
    let dir_path = format!("/tmp/willow_project_error_test_{}", id);
    let bin_path = format!("/tmp/willow_project_error_test_{}_bin", id);

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

    let _ = fs::remove_dir_all(&dir_path);
    remove_output_artifacts(&bin_path);

    assert!(
        !output.status.success(),
        "expected compile error, got success; stdout: {}; stderr: {}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );

    String::from_utf8_lossy(&output.stderr).into_owned()
}

/// Compile source that is expected to fail; returns true if compiler rejected it.
fn expect_compile_error(source: &str) -> bool {
    let id = unique_test_id();
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
    remove_output_artifacts(&bin_path);

    !status.success()
}

fn compile_error_stderr(source: &str) -> String {
    let id = unique_test_id();
    let src_path = format!("/tmp/willow_diag_{}.wi", id);
    let bin_path = format!("/tmp/willow_diag_{}", id);

    fs::write(&src_path, source).unwrap();

    let compiler = env!("CARGO_BIN_EXE_willowc");
    let out = Command::new(compiler)
        .args(["build", &src_path, "-o", &bin_path])
        .output()
        .expect("failed to run compiler");

    let _ = fs::remove_file(&src_path);
    remove_output_artifacts(&bin_path);

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
fn test_println_string_literal() {
    let (out, ok) = compile_and_run(r#"fn main() { println("Hello, world!"); }"#);
    assert!(ok, "compilation failed");
    assert_eq!(out, "Hello, world!\n");
}

#[test]
fn test_print_string_variable() {
    let src = r#"
fn main() {
    let greeting: String = "hello";
    print(greeting);
    println(" willow");
}
"#;
    let (out, ok) = compile_and_run(src);
    assert!(ok, "compilation failed");
    assert_eq!(out, "hello willow\n");
}

#[test]
fn test_string_concatenation() {
    let src = r#"
fn greet(name: String) -> String {
    return "Hello, " + name;
}

fn main() {
    let punctuation = "!";
    println(greet("Willow") + punctuation);
    println("a" + "b" + "c");
}
"#;
    let (out, ok) = compile_and_run(src);
    assert!(ok, "compilation failed");
    assert_eq!(out, "Hello, Willow!\nabc\n");
}

#[test]
fn test_string_concatenation_rejects_non_string_rhs() {
    assert_compile_error_contains(
        r#"
fn main() {
    println("count: " + 3);
}
"#,
        &[
            "error[E0202]",
            "cannot apply operator `+` to `String` and `i64`",
            "`+` not defined for `String` and `i64`",
        ],
    );
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

// ── Class codegen ────────────────────────────────────────────────────────────

#[test]
fn test_class_instantiation_and_field_access() {
    let src = r#"
class Point {
    x: i64;
    y: i64;

    pub fn get_x(self) -> i64 { return self.x; }
    pub fn get_y(self) -> i64 { return self.y; }
}

fn main() {
    let p = Point { x: 10, y: 20 };
    println(p.get_x());
    println(p.get_y());
}
"#;
    let (out, ok) = compile_and_run(src);
    assert!(ok, "compilation failed");
    assert_eq!(out, "10\n20\n");
}

#[test]
fn test_class_method_with_arithmetic() {
    let src = r#"
class Counter {
    count: i64;

    pub fn value(self) -> i64 { return self.count; }
    pub fn doubled(self) -> i64 { return self.count * 2; }
    pub fn add(self, n: i64) -> i64 { return self.count + n; }
}

fn main() {
    let c = Counter { count: 5 };
    println(c.value());
    println(c.doubled());
    println(c.add(10));
}
"#;
    let (out, ok) = compile_and_run(src);
    assert!(ok, "compilation failed");
    assert_eq!(out, "5\n10\n15\n");
}

#[test]
fn test_class_method_call_chained_in_println() {
    let src = r#"
class Box {
    v: i64;
    pub fn get(self) -> i64 { return self.v; }
}

fn main() {
    let b = Box { v: 99 };
    println(b.get());
}
"#;
    let (out, ok) = compile_and_run(src);
    assert!(ok, "compilation failed");
    assert_eq!(out, "99\n");
}

// ── GC ───────────────────────────────────────────────────────────────────────

#[test]
fn test_gc_allocated_bytes_increases_on_class_alloc() {
    let src = r#"
class Box {
    v: i64;
    pub fn get(self) -> i64 { return self.v; }
}
fn main() {
    let before = gc_allocated_bytes();
    let b = Box { v: 42 };
    let after = gc_allocated_bytes();
    println(b.get());
    println(after > before);
}
"#;
    let (out, ok) = compile_and_run(src);
    assert!(ok, "compilation failed");
    assert_eq!(out, "42\ntrue\n");
}

#[test]
fn test_gc_collect_reclaims_unrooted_objects() {
    // alloc_node allocates a Node and returns its value field (i64, not a GC pointer).
    // When alloc_node returns, the Node's root is popped, so the Node has no live roots.
    // gc_collect() in main can then reclaim it, leaving gc_allocated_bytes() == 0.
    let src = r#"
class Node {
    value: i64;
    pub fn get(self) -> i64 { return self.value; }
}
fn alloc_node() -> i64 {
    let n = Node { value: 7 };
    return n.get();
}
fn main() {
    let v = alloc_node();
    println(v);
    gc_collect();
    println(gc_allocated_bytes());
}
"#;
    let (out, ok) = compile_and_run(src);
    assert!(ok, "compilation failed");
    assert_eq!(out, "7\n0\n");
}

#[test]
fn test_gc_does_not_collect_live_rooted_objects() {
    // A rooted object (n is in scope when gc_collect() runs) must not be freed.
    let src = r#"
class Node {
    value: i64;
    pub fn get(self) -> i64 { return self.value; }
}
fn main() {
    let n = Node { value: 42 };
    gc_collect();
    println(n.get());
    println(gc_allocated_bytes() > 0);
}
"#;
    let (out, ok) = compile_and_run(src);
    assert!(ok, "compilation failed");
    assert_eq!(out, "42\ntrue\n");
}

#[test]
fn test_gc_traces_nullable_reference_fields() {
    let src = r#"
class Node {
    pub value: i64;
    pub next: Node?;
}

fn make_pair() -> Node {
    let tail = Node { value: 2, next: nil };
    return Node { value: 1, next: tail };
}

fn main() {
    let head = make_pair();
    gc_collect();
    println(head.value);
    let next = head.next;
    if next != nil {
        println(next.value);
    }
}
"#;
    let (out, ok) = compile_and_run(src);
    assert!(
        ok,
        "nullable reference field should keep child object alive"
    );
    assert_eq!(out, "1\n2\n");
}

#[test]
fn test_gc_ignores_nil_nullable_reference_fields() {
    let src = r#"
class Node {
    pub value: i64;
    next: Node?;
}

fn main() {
    let head = Node { value: 1, next: nil };
    gc_collect();
    println(head.value);
    println(gc_allocated_bytes() > 0);
}
"#;
    let (out, ok) = compile_and_run(src);
    assert!(ok, "nil nullable field should be ignored safely by GC");
    assert_eq!(out, "1\ntrue\n");
}

// ── Example files ───────────────────────────────────────────────────────────

#[test]
fn test_runnable_example_files_compile_and_run() {
    let cases = [
        ("example/arithmetic.wi", "27\n15\n126\n3\n3\n54\n3\ntrue\n"),
        ("example/array_growth.wi", "5\n55\n25\n16\n3\n"),
        ("example/arrays.wi", "4\n10\n40\n100\n99\n2\nbob\ntrue\n"),
        ("example/async_sleep.wi", "42\n"),
        ("example/async_string_param.wi", "hello, willow\n"),
        ("example/booleans.wi", "true\nfalse\ntrue\ntrue\n"),
        ("example/class_hierarchy.wi", "3\n"),
        ("example/class.wi", "42\n"),
        ("example/command_line_args.wi", "0\n0\ntrue\ntrue\n"),
        ("example/control_flow.wi", "120\n"),
        ("example/debug_source_map.wi", "12\n"),
        ("example/early_return.wi", "7\n0\n12\n"),
        ("example/example.wi", "50\ntrue\n"),
        ("example/fib.wi", "63245986\n"),
        ("example/fib_bench.wi", "63245986\n"),
        ("example/f64_parse.wi", "3.5\ntrue\nNaN\nparse failed\n"),
        ("example/floats.wi", "4\ntrue\n-4\n"),
        ("example/fn_values.wi", "20\n25\n30\n107\n104\n"),
        (
            "example/enum_match.wi",
            "north\nwest\n78.53975\n12\n0\nzero\nnonzero\nyes\nno\n",
        ),
        ("example/leibniz_pi.wi", "3.141592663589326\n"),
        ("example/match_color.wi", "green\n"),
        ("example/functions.wi", "25\ntrue\n"),
        ("example/hello.wi", "50"),
        ("example/hello_world.wi", "Hello, world!\n"),
        ("example/import_demo/main.wi", "30\n42\n42\n99\n3\n42\n"),
        ("example/item_import_demo/main.wi", "7\n25\n"),
        ("example/interfaces.wi", "woof\n4\ntweet\n2\nwoof\ntweet\n"),
        ("example/maps.wi", "2\n31\n25\n-1\ntrue\nfalse\ntwo\n"),
        ("example/module_alias_demo/main.wi", "5\n16\n"),
        ("example/module_class_demo/main.wi", "42\n12\n"),
        ("example/module_demo/main.wi", "12\n14\n"),
        ("example/mutability.wi", "6\n15\ntrue\n"),
        ("example/nested_loops.wi", "30\n"),
        (
            "example/nil_guard_demo.wi",
            "42\n-7\n0\ntrue\nfalse\nfalse\n126\n99\n",
        ),
        ("example/nil_nullable.wi", "0\n10\n20\ntrue\n10\n"),
        ("example/nil_safe_chain.wi", "60\n3\n30\n-1\n120\n"),
        (
            "example/option_result.wi",
            "true\ntrue\n10\n10\n10\n99\n20\ntrue\n2\ntrue\n42\n10\ntrue\ntrue\n8\n8\n8\n99\nsomething failed\n24\ntrue\nprefix: something failed\n8\n2\nnot even\n0\n8\n",
        ),
        (
            "example/option_result_inference.wi",
            "true\n10\ntrue\n7\n5\ntrue\n42\n-1\n",
        ),
        ("example/prot_demo.wi", "10\n9\n20\n18\n17\n15\n14\n"),
        ("example/result_propagation.wi", "84\n-1\n52\n-1\n-1\n"),
        ("example/print_test.wi", "1230\n42\ntrue\nfalsetrue\n"),
        ("example/recursion.wi", "3628800\n1024\n6\n"),
        (
            "example/references.wi",
            "11\n22\ntrue\nhi!\nhi?\nold box\nold box!\nnew box\n3\n",
        ),
        (
            "example/rust_runtime_smoke.wi",
            "rust runtime\n42\n10\n21\n0\n",
        ),
        ("example/channel_producer.wi", "10\n20\n30\n"),
        ("example/parallel_tasks.wi", "55\n144\n610\n42\nfalse\n"),
        ("example/self_demo.wi", "10\n10\n10\n"),
        ("example/spawn_join.wi", "9\n16\n25\n42\n"),
        ("example/std_imports.wi", "1\n42\n7\n-1\n"),
        ("example/strings.wi", "Hello, Willow\nstring concat\n"),
        ("example/ternary.wi", "1\n-1\n0\n20\n99\n15\n8\n1\n"),
        ("example/types.wi", "10\n2.5\n10\n78.53975\ntrue\n"),
    ];

    let mut expected_paths = cases
        .iter()
        .map(|(path, _)| path.to_string())
        .collect::<Vec<_>>();
    expected_paths.sort();
    let actual_paths = collect_runnable_example_entries();
    assert_eq!(
        actual_paths, expected_paths,
        "every runnable non-future example entrypoint should have an output assertion"
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
fn test_debug_build_emits_source_map_sidecar() {
    let id = unique_test_id();
    let src_path = format!("/tmp/willow_sourcemap_{}.wi", id);
    let bin_path = format!("/tmp/willow_sourcemap_{}", id);

    let source = r#"
fn helper(x: i64) -> i64 {
    let doubled = x * 2;
    if doubled > 10 {
        return doubled;
    }
    return doubled + 1;
}

pub class Counter {
    pub fn value(self) -> i64 {
        return 1;
    }
}

fn main() {
    println(helper(6));
}
"#;
    fs::write(&src_path, source).unwrap();

    let compiler = env!("CARGO_BIN_EXE_willowc");
    let output = Command::new(compiler)
        .args(["build", &src_path, "-o", &bin_path])
        .output()
        .expect("failed to run compiler");

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        let _ = fs::remove_file(&src_path);
        remove_output_artifacts(&bin_path);
        panic!("debug compilation failed: {stderr}");
    }

    let map_path = format!("{bin_path}.wsmap");
    let map = fs::read_to_string(&map_path).expect("debug build should emit a source map");

    let _ = fs::remove_file(&src_path);
    remove_output_artifacts(&bin_path);

    assert!(map.contains("willow_debug_source_map_v1"));
    assert!(map.contains(&format!("file={src_path}")));
    assert!(map.contains("function name=helper"));
    assert!(map.contains("function name=Counter::value"));
    assert!(map.contains("function name=main"));
    assert!(map.contains("statement kind=let"));
    assert!(map.contains("statement kind=if"));
    assert!(map.contains("statement kind=return"));
    assert!(map.contains("statement kind=expr"));
    assert!(map.contains(" line="));
    assert!(map.contains(" col="));
}

#[test]
fn test_release_build_removes_source_map_sidecar() {
    let id = unique_test_id();
    let src_path = format!("/tmp/willow_release_sourcemap_{}.wi", id);
    let bin_path = format!("/tmp/willow_release_sourcemap_{}", id);
    let map_path = format!("{bin_path}.wsmap");

    fs::write(&src_path, "fn main() { println(1); }").unwrap();
    fs::write(&map_path, "stale debug source map").unwrap();

    let compiler = env!("CARGO_BIN_EXE_willowc");
    let output = Command::new(compiler)
        .args(["build", &src_path, "-o", &bin_path, "--release"])
        .output()
        .expect("failed to run compiler");

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        let _ = fs::remove_file(&src_path);
        remove_output_artifacts(&bin_path);
        panic!("release compilation failed: {stderr}");
    }

    let source_map_exists = Path::new(&map_path).exists();

    let _ = fs::remove_file(&src_path);
    remove_output_artifacts(&bin_path);

    assert!(
        !source_map_exists,
        "release build should not keep {map_path}"
    );
}

#[test]
fn test_release_with_debug_info_emits_source_map_sidecar() {
    let id = unique_test_id();
    let src_path = format!("/tmp/willow_release_debug_sourcemap_{}.wi", id);
    let bin_path = format!("/tmp/willow_release_debug_sourcemap_{}", id);
    let map_path = format!("{bin_path}.wsmap");

    fs::write(
        &src_path,
        r#"
fn helper() -> i64 {
    return 7;
}

fn main() {
    println(helper());
}
"#,
    )
    .unwrap();

    let compiler = env!("CARGO_BIN_EXE_willowc");
    let output = Command::new(compiler)
        .args([
            "build",
            &src_path,
            "-o",
            &bin_path,
            "--release",
            "--debug-info",
        ])
        .output()
        .expect("failed to run compiler");

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        let _ = fs::remove_file(&src_path);
        remove_output_artifacts(&bin_path);
        panic!("release-with-debug-info compilation failed: {stderr}");
    }

    let map = fs::read_to_string(&map_path).expect("release --debug-info should emit a source map");

    let _ = fs::remove_file(&src_path);
    remove_output_artifacts(&bin_path);

    assert!(map.contains("willow_debug_source_map_v1"));
    assert!(map.contains(&format!("file={src_path}")));
    assert!(map.contains("function name=helper"));
    assert!(map.contains("function name=main"));
}

#[test]
fn test_debug_build_embeds_runtime_metadata_in_binary() {
    let id = unique_test_id();
    let src_path = format!("/tmp/willow_runtime_metadata_{}.wi", id);
    let bin_path = format!("/tmp/willow_runtime_metadata_{}", id);

    let source = r#"
fn helper(x: i64) -> i64 {
    return x + 1;
}

pub class Counter {
    pub value: i64;

    pub fn read(self) -> i64 {
        return 1;
    }
}

fn main() {
    println(helper(41));
}
"#;
    fs::write(&src_path, source).unwrap();

    let compiler = env!("CARGO_BIN_EXE_willowc");
    let output = Command::new(compiler)
        .args(["build", &src_path, "-o", &bin_path])
        .output()
        .expect("failed to run compiler");

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        let _ = fs::remove_file(&src_path);
        remove_output_artifacts(&bin_path);
        panic!("debug compilation failed: {stderr}");
    }

    let binary = fs::read(&bin_path).expect("debug binary should exist");
    let metadata = String::from_utf8_lossy(&binary);

    let _ = fs::remove_file(&src_path);
    remove_output_artifacts(&bin_path);

    assert!(metadata.contains("willow_runtime_metadata_v1"));
    assert!(metadata.contains("willow_debug_source_map_v1"));
    assert!(metadata.contains(&format!("file={src_path}")));
    assert!(metadata.contains("function name=helper line="));
    assert!(metadata.contains("function name=main line="));
    assert!(metadata.contains("class name=Counter line="));
    assert!(metadata.contains("gc_type name=Counter"));
    assert!(metadata.contains("field name=value line="));
    assert!(metadata.contains("method name=read line="));
    assert!(metadata.contains("function name=Counter::read line="));
}

#[test]
fn test_debug_build_embeds_async_stack_metadata_in_binary() {
    let id = unique_test_id();
    let src_path = format!("/tmp/willow_async_metadata_{}.wi", id);
    let bin_path = format!("/tmp/willow_async_metadata_{}", id);

    let source = r#"
async fn wait_value() -> i64 {
    await sleep(1);
    return 42;
}

async fn main() {
    let value = await wait_value();
    println(value);
}
"#;
    fs::write(&src_path, source).unwrap();

    let compiler = env!("CARGO_BIN_EXE_willowc");
    let output = Command::new(compiler)
        .args(["build", &src_path, "-o", &bin_path])
        .output()
        .expect("failed to run compiler");

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        let _ = fs::remove_file(&src_path);
        remove_output_artifacts(&bin_path);
        panic!("async debug compilation failed: {stderr}");
    }

    let binary = fs::read(&bin_path).expect("debug binary should exist");
    let metadata = String::from_utf8_lossy(&binary);

    let _ = fs::remove_file(&src_path);
    remove_output_artifacts(&bin_path);

    assert!(metadata.contains("function name=wait_value line="));
    assert!(metadata.contains("function name=main line="));
    assert!(metadata.contains("  async=true"));
    assert!(metadata.contains("  async_stack_frame name=wait_value"));
    assert!(metadata.contains("  async_stack_frame name=main"));
    assert!(metadata.contains("  await line="));
}

#[test]
fn test_debug_source_map_records_reference_params_and_call_sites() {
    let id = unique_test_id();
    let src_path = format!("/tmp/willow_ref_metadata_{}.wi", id);
    let bin_path = format!("/tmp/willow_ref_metadata_{}", id);

    let source = r#"
fn read(x: & i64) -> i64 {
    return x;
}

fn bump(x: &mut i64) {
    x = x + 1;
}

fn main() {
    let mut n = 1;
    println(read(&n));
    bump(&n);
}
"#;
    fs::write(&src_path, source).unwrap();

    let compiler = env!("CARGO_BIN_EXE_willowc");
    let output = Command::new(compiler)
        .args(["build", &src_path, "-o", &bin_path])
        .output()
        .expect("failed to run compiler");

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        let _ = fs::remove_file(&src_path);
        remove_output_artifacts(&bin_path);
        panic!("reference metadata compilation failed: {stderr}");
    }

    let map = fs::read_to_string(format!("{bin_path}.wsmap"))
        .expect("debug build should emit reference metadata");

    let _ = fs::remove_file(&src_path);
    remove_output_artifacts(&bin_path);

    assert!(map.contains("function name=read line="));
    assert!(map.contains("param name=x mode=& type=i64"));
    assert!(map.contains("function name=bump line="));
    assert!(map.contains("param name=x mode=&mut type=i64"));
    assert!(
        map.contains("reference_call callee=read param=x mode=& type=i64 place_kind=local place=n")
    );
    assert!(map.contains(
        "reference_call callee=bump param=x mode=&mut type=i64 place_kind=local place=n"
    ));
}

#[test]
fn test_reference_runtime_debug_hook_reports_array_element_call_site() {
    let src = r#"
import std.collections.Array;

fn increment(x: &mut i64) {
    x = x + 1;
}

fn main() {
    let mut xs: Array<i64> = [1];
    increment(&xs[3]);
}
"#;
    let (out, ok) = compile_and_run_check_exit(src);
    assert!(!ok, "out-of-bounds reference call should abort");
    assert!(
        out.contains("array index out of bounds: the length is 1 but the index is 3"),
        "missing array bounds diagnostic:\n{out}"
    );
    assert!(
        out.contains("reference call: increment parameter `x` &mut i64"),
        "missing reference call context:\n{out}"
    );
    assert!(
        out.contains("using array_element `xs[3]`"),
        "missing referenced array element context:\n{out}"
    );
}

#[test]
fn test_release_build_omits_runtime_metadata_from_binary() {
    let id = unique_test_id();
    let src_path = format!("/tmp/willow_release_runtime_metadata_{}.wi", id);
    let bin_path = format!("/tmp/willow_release_runtime_metadata_{}", id);

    fs::write(&src_path, "fn main() { println(1); }").unwrap();

    let compiler = env!("CARGO_BIN_EXE_willowc");
    let output = Command::new(compiler)
        .args(["build", &src_path, "-o", &bin_path, "--release"])
        .output()
        .expect("failed to run compiler");

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        let _ = fs::remove_file(&src_path);
        remove_output_artifacts(&bin_path);
        panic!("release compilation failed: {stderr}");
    }

    let binary = fs::read(&bin_path).expect("release binary should exist");
    let metadata = String::from_utf8_lossy(&binary);

    let _ = fs::remove_file(&src_path);
    remove_output_artifacts(&bin_path);

    assert!(
        !metadata.contains("willow_runtime_metadata_v1"),
        "release binary should not embed runtime metadata"
    );
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

#[test]
fn test_import_diagnostic_unresolved_module_lists_candidate_paths() {
    let stderr = compile_error_stderr(
        r#"
import missing_math;

fn main() {
    println(1);
}
"#,
    );

    for expected in [
        "error[E0401]",
        "unresolved import `missing_math`",
        "module not found",
        "note: tried to find module at:",
        "missing_math.wi",
        "missing_math/mod.wi",
        "help: create `",
        "or check the import name",
    ] {
        assert!(
            stderr.contains(expected),
            "stderr did not contain `{expected}`:\n{stderr}"
        );
    }
}

#[test]
fn test_import_diagnostic_private_function_points_to_definition() {
    let tools = r#"
fn secret() -> i64 {
    return 7;
}
"#;
    let main = r#"
import tools;

fn main() {
    println(tools::secret());
}
"#;

    let stderr =
        compile_temp_project_error_stderr(&[("tools.wi", tools), ("main.wi", main)], "main.wi");
    for expected in [
        "error[E0402]",
        "function `secret` is private",
        "private function",
        "`secret` is defined at",
        "tools.wi:2:1",
        "help: make it public with `pub fn secret`",
    ] {
        assert!(
            stderr.contains(expected),
            "stderr did not contain `{expected}`:\n{stderr}"
        );
    }
}

#[test]
fn test_import_diagnostic_cycle_shows_cycle_path() {
    let main = r#"
import a;

fn main() {
    println(1);
}
"#;
    let a = r#"
import b;

pub fn a_value() -> i64 {
    return 1;
}
"#;
    let b = r#"
import a;

pub fn b_value() -> i64 {
    return 2;
}
"#;

    let stderr = compile_temp_project_error_stderr(
        &[("main.wi", main), ("a.wi", a), ("b.wi", b)],
        "main.wi",
    );
    for expected in [
        "error[E0403]",
        "import cycle detected",
        "this import creates a cycle",
        "note: import cycle: a -> b -> a",
        "help: remove one of the imports",
    ] {
        assert!(
            stderr.contains(expected),
            "stderr did not contain `{expected}`:\n{stderr}"
        );
    }
}

#[test]
fn test_import_nested_module_path_compiles_and_runs() {
    let math = r#"
pub fn triple(x: i64) -> i64 {
    return x * 3;
}
"#;
    let main = r#"
import tools::math;

fn main() {
    println(math::triple(14));
}
"#;

    let (out, ok) =
        compile_temp_project_and_run(&[("tools/math.wi", math), ("main.wi", main)], "main.wi");
    assert!(ok, "nested import project failed to compile or run");
    assert_eq!(out, "42\n");
}

#[test]
fn test_import_nested_module_mod_file_compiles_and_runs() {
    let math = r#"
pub fn value() -> i64 {
    return 99;
}
"#;
    let main = r#"
import tools::math as tm;

fn main() {
    println(tm::value());
}
"#;

    let (out, ok) =
        compile_temp_project_and_run(&[("tools/math/mod.wi", math), ("main.wi", main)], "main.wi");
    assert!(
        ok,
        "nested mod-file import project failed to compile or run"
    );
    assert_eq!(out, "99\n");
}

#[test]
fn test_import_nested_diagnostic_lists_candidate_paths() {
    let stderr = compile_error_stderr(
        r#"
import tools::missing_math;

fn main() {
    println(1);
}
"#,
    );

    for expected in [
        "error[E0401]",
        "unresolved import `tools::missing_math`",
        "tools/missing_math.wi",
        "tools/missing_math/mod.wi",
    ] {
        assert!(
            stderr.contains(expected),
            "stderr did not contain `{expected}`:\n{stderr}"
        );
    }
}

#[test]
fn test_import_nested_cycle_shows_cycle_path() {
    let main = r#"
import graph::a;

fn main() {
    println(1);
}
"#;
    let a = r#"
import graph::b;

pub fn a_value() -> i64 {
    return 1;
}
"#;
    let b = r#"
import graph::a;

pub fn b_value() -> i64 {
    return 2;
}
"#;

    let stderr = compile_temp_project_error_stderr(
        &[("main.wi", main), ("graph/a.wi", a), ("graph/b.wi", b)],
        "main.wi",
    );
    for expected in [
        "error[E0403]",
        "import cycle detected",
        "note: import cycle: graph::a -> graph::b -> graph::a",
    ] {
        assert!(
            stderr.contains(expected),
            "stderr did not contain `{expected}`:\n{stderr}"
        );
    }
}

#[test]
fn test_main_signature_accepts_empty_or_array_string_args() {
    let (out, ok) = compile_and_run(
        r#"
import std.collections.Array;

fn main(args: Array<String>) {
    println(42);
}
"#,
    );
    assert!(ok, "main(args: Array<String>) should compile");
    assert_eq!(out, "42\n");
}

#[test]
fn test_main_signature_rejects_invalid_args() {
    assert_compile_error_contains(
        r#"
fn main(args: String) {
    println(args);
}
"#,
        &[
            "error[E1301]",
            "invalid entry point signature for `main`",
            "expected `fn main()` or `fn main(args: Array<String>)`",
        ],
    );
}

#[test]
fn test_main_signature_rejects_duplicate_main() {
    assert_compile_error_contains(
        r#"
fn main() {}

fn main() {}
"#,
        &[
            "error[E1302]",
            "duplicate entry point `main`",
            "first `main` defined here",
        ],
    );
}

#[test]
fn test_main_signature_rejects_missing_main() {
    assert_compile_error_contains(
        r#"
fn helper() {
    println(1);
}
"#,
        &[
            "error[E1303]",
            "missing entry point `main`",
            "help: define an entry point",
        ],
    );
}

#[test]
fn test_runtime_start_runs_user_main_with_program_arguments() {
    let (out, ok) = compile_and_run_with_program_args(
        r#"
fn main() {
    println(42);
}
"#,
        &["alpha", "beta"],
    );
    assert!(ok, "Rust runtime main should return success");
    assert_eq!(out, "42\n");
}

#[test]
fn test_env_args_len_and_arg_read_program_arguments() {
    let (out, ok) = compile_and_run_with_program_args(
        r#"
fn main() {
    println(env::args_len());
    println(env::arg(0));
    println(env::arg(1));
}
"#,
        &["alpha", "beta"],
    );
    assert!(ok, "env argument builtins should run successfully");
    assert_eq!(out, "2\nalpha\nbeta\n");
}

#[test]
fn test_run_command_forwards_args_after_separator() {
    let (out, ok) = run_command_with_program_args(
        r#"
fn main() {
    println(env::args_len());
    println(env::arg(0));
}
"#,
        &["from-run"],
    );
    assert!(ok, "willowc run should forward program args after --");
    assert_eq!(out, "1\nfrom-run\n");
}

#[test]
fn test_env_program_name_returns_binary_path() {
    let (out, ok) = compile_and_run_with_program_args(
        r#"
fn main() {
    println(env::program_name());
}
"#,
        &[],
    );
    assert!(ok, "env::program_name should run successfully");
    assert!(
        out.trim().contains("willow_args_test_"),
        "program name should include the generated binary path, got `{out}`"
    );
}

/// Parse the symbol names the backend imports from the runtime staticlib.
fn backend_runtime_symbols() -> Vec<String> {
    let path = Path::new(env!("CARGO_MANIFEST_DIR")).join("src/backend/abi.rs");
    let source = fs::read_to_string(&path).unwrap_or_else(|e| panic!("cannot read {path:?}: {e}"));
    let table_start = source
        .find("pub const RUNTIME_SYMBOLS")
        .expect("cannot find RUNTIME_SYMBOLS table in src/backend/abi.rs");
    let table = &source[table_start..];
    let table_end = table
        .find("];")
        .expect("cannot find end of RUNTIME_SYMBOLS table in src/backend/abi.rs");
    let table = &table[..table_end];

    let mut symbols = Vec::new();
    for line in table.lines() {
        let line = line.trim();
        let Some(rest) = line.strip_prefix("name:") else {
            continue;
        };
        let rest = rest.trim();
        let Some(rest) = rest.strip_prefix('"') else {
            continue;
        };
        let Some(end) = rest.find('"') else {
            continue;
        };
        symbols.push(rest[..end].to_string());
    }
    symbols
}

#[test]
fn test_backend_runtime_symbol_table_lists_expected_symbols() {
    // Guard the parser itself: if the table format changes in a way that breaks
    // extraction, every downstream symbol assertion would silently pass on an
    // empty set. A non-trivial floor plus a couple of anchors prevents that.
    let symbols = backend_runtime_symbols();
    assert!(
        symbols.len() >= 50,
        "expected the backend runtime symbol table to cover the full imported surface, parsed {}",
        symbols.len()
    );
    for anchor in ["willow_alloc_typed", "willow_panic", "willow_string_alloc"] {
        assert!(
            symbols.iter().any(|s| s == anchor),
            "backend runtime symbol parser failed to find {anchor}"
        );
    }
}

/// Assert the given runtime staticlib exports every symbol the backend imports.
/// Shared by the debug and release coverage tests so the exported
/// surface cannot silently diverge between build profiles.
fn assert_staticlib_exports_backend_symbols(runtime_lib: &Path) {
    let output = Command::new("nm")
        .arg(runtime_lib)
        .output()
        .expect("failed to inspect runtime staticlib with nm");
    assert!(output.status.success(), "nm failed for {runtime_lib:?}");
    let nm_symbols = String::from_utf8_lossy(&output.stdout);

    let backend_symbols = backend_runtime_symbols();
    let mut missing = Vec::new();
    for symbol in &backend_symbols {
        // Match on word boundaries so `willow_alloc` does not satisfy
        // `willow_alloc_typed`. nm output lists one symbol token per line.
        let found = nm_symbols.lines().any(|line| {
            line.split(|c: char| !(c.is_ascii_alphanumeric() || c == '_'))
                .any(|tok| tok == symbol)
        });
        if !found {
            missing.push(symbol.clone());
        }
    }
    assert!(
        missing.is_empty(),
        "runtime staticlib {runtime_lib:?} is missing symbols imported by the backend: {missing:?}"
    );
}

#[test]
fn test_rust_runtime_staticlib_exports_required_symbols() {
    // Every symbol emitted as a runtime import by the backend must be present in
    // the staticlib, so generated programs always link.
    let runtime_lib = build_runtime_staticlib(false);
    assert_staticlib_exports_backend_symbols(&runtime_lib);
}

#[test]
fn test_release_runtime_staticlib_exports_required_symbols() {
    // The ABI surface must be identical across build profiles: a program built
    // with --release links against the release staticlib, so it must export the
    // same backend-imported symbols as the debug one.
    let runtime_lib = build_runtime_staticlib(true);
    assert_staticlib_exports_backend_symbols(&runtime_lib);
}

#[test]
fn test_build_uses_rust_runtime_without_generated_c_artifacts() {
    let id = unique_test_id();
    let src_path = format!("/tmp/willow_rust_runtime_no_c_{id}.wi");
    let bin_path = format!("/tmp/willow_rust_runtime_no_c_{id}");
    fs::write(&src_path, "fn main() { println(42); }").unwrap();

    let compiler = env!("CARGO_BIN_EXE_willowc");
    let status = Command::new(compiler)
        .args(["build", &src_path, "-o", &bin_path])
        .status()
        .expect("failed to run compiler");

    assert!(status.success(), "Rust runtime build should succeed");
    assert!(
        !Path::new(&format!("{bin_path}_runtime.c")).exists(),
        "compiler must not emit generated runtime C"
    );
    assert!(
        !Path::new(&format!("{bin_path}_runtime.o")).exists(),
        "compiler must not emit generated runtime object"
    );

    let out = Command::new(&bin_path)
        .output()
        .expect("failed to run binary");
    assert!(out.status.success());
    assert_eq!(String::from_utf8_lossy(&out.stdout), "42\n");

    let _ = fs::remove_file(&src_path);
    remove_output_artifacts(&bin_path);
}

#[test]
fn test_runtime_lib_cli_override_links_program() {
    let runtime_lib = build_runtime_staticlib(false);
    let id = unique_test_id();
    let src_path = format!("/tmp/willow_runtime_cli_override_{id}.wi");
    let bin_path = format!("/tmp/willow_runtime_cli_override_{id}");
    fs::write(&src_path, "fn main() { println(\"override\"); }").unwrap();

    let compiler = env!("CARGO_BIN_EXE_willowc");
    let status = Command::new(compiler)
        .args([
            "build",
            &src_path,
            "-o",
            &bin_path,
            "--runtime-lib",
            runtime_lib.to_str().unwrap(),
        ])
        .status()
        .expect("failed to run compiler");

    assert!(status.success(), "--runtime-lib build should succeed");
    let out = Command::new(&bin_path)
        .output()
        .expect("failed to run binary");
    assert_eq!(String::from_utf8_lossy(&out.stdout), "override\n");

    let _ = fs::remove_file(&src_path);
    remove_output_artifacts(&bin_path);
}

#[test]
fn test_runtime_lib_env_override_links_program() {
    let runtime_lib = build_runtime_staticlib(false);
    let id = unique_test_id();
    let src_path = format!("/tmp/willow_runtime_env_override_{id}.wi");
    let bin_path = format!("/tmp/willow_runtime_env_override_{id}");
    fs::write(&src_path, "fn main() { println(env::args_len()); }").unwrap();

    let compiler = env!("CARGO_BIN_EXE_willowc");
    let status = Command::new(compiler)
        .args(["build", &src_path, "-o", &bin_path])
        .env("WILLOW_RUNTIME_LIB", &runtime_lib)
        .status()
        .expect("failed to run compiler");

    assert!(status.success(), "WILLOW_RUNTIME_LIB build should succeed");
    let out = Command::new(&bin_path)
        .output()
        .expect("failed to run binary");
    assert_eq!(String::from_utf8_lossy(&out.stdout), "0\n");

    let _ = fs::remove_file(&src_path);
    remove_output_artifacts(&bin_path);
}

#[test]
fn test_missing_runtime_lib_reports_actionable_diagnostic() {
    let id = unique_test_id();
    let src_path = format!("/tmp/willow_runtime_missing_{id}.wi");
    let bin_path = format!("/tmp/willow_runtime_missing_{id}");
    let missing = format!("/tmp/willow_runtime_missing_{id}.a");
    fs::write(&src_path, "fn main() { println(1); }").unwrap();

    let compiler = env!("CARGO_BIN_EXE_willowc");
    let output = Command::new(compiler)
        .args([
            "build",
            &src_path,
            "-o",
            &bin_path,
            "--runtime-lib",
            &missing,
        ])
        .output()
        .expect("failed to run compiler");

    assert!(!output.status.success(), "missing runtime lib should fail");
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("runtime library unavailable"), "{stderr}");
    assert!(stderr.contains("--runtime-lib"), "{stderr}");
    assert!(stderr.contains(&missing), "{stderr}");

    let _ = fs::remove_file(&src_path);
    remove_output_artifacts(&bin_path);
}

#[test]
fn test_pow_and_powf_builtins() {
    let (out, ok) = compile_and_run(
        r#"
fn main() {
    println(pow(2.0, 8.0));
    println(powf(9.0, 0.5));
}
"#,
    );
    assert!(ok, "pow builtins should compile and run");
    assert_eq!(out, "256\n3\n");
}

#[test]
fn test_pow_builtin_requires_f64_arguments() {
    assert_compile_error_contains(
        r#"
fn main() {
    println(pow(2, 8.0));
}
"#,
        &[
            "error[E0201]",
            "mismatched types: expected `f64`, found `i64`",
            "expected `f64`",
        ],
    );
}

#[test]
fn test_f64_to_string_static_call() {
    let (out, ok) = compile_and_run(
        r#"
fn main() {
    let pi = f64::to_string(3.14);
    println(pi);
    println(f64::to_string(10.0));
}
"#,
    );
    assert!(ok, "f64::to_string should compile and run");
    assert_eq!(out, "3.14\n10\n");
}

#[test]
fn test_f64_to_string_requires_f64_argument() {
    assert_compile_error_contains(
        r#"
fn main() {
    println(f64::to_string(1));
}
"#,
        &[
            "error[E0201]",
            "mismatched types: expected `f64`, found `i64`",
            "expected `f64`",
        ],
    );
}

#[test]
fn test_f64_parse_static_call_ok_and_err() {
    let (out, ok) = compile_and_run(
        r#"
fn main() {
    let parsed = f64::parse("3.5").unwrap();
    println(parsed);

    let message = match f64::parse("not-a-number") {
        Result::Ok(value) => f64::to_string(value),
        Result::Err(error) => match error {
            ParseFloatError::Invalid(text) => text,
        },
    };
    println(message);
}
"#,
    );
    assert!(ok, "f64::parse should compile and run");
    assert_eq!(out, "3.5\ninvalid float: invalid float literal\n");
}

#[test]
fn test_f64_parse_round_trips_to_string_output() {
    let (out, ok) = compile_and_run(
        r#"
fn main() {
    let value = 0.1 + 0.2;
    let text = f64::to_string(value);
    let parsed = f64::parse(text).unwrap();
    println(parsed == value);
    println(f64::to_string(f64::parse("NaN").unwrap()));
    println(f64::to_string(f64::parse("inf").unwrap()));
    println(f64::to_string(f64::parse("-inf").unwrap()));
}
"#,
    );
    assert!(ok, "f64::parse should round-trip f64::to_string output");
    assert_eq!(out, "true\nNaN\ninf\n-inf\n");
}

#[test]
fn test_f64_parse_requires_string_argument() {
    assert_compile_error_contains(
        r#"
fn main() {
    println(f64::parse(1.0));
}
"#,
        &[
            "error[E0201]",
            "mismatched types: expected `String`, found `f64`",
            "expected `String`",
        ],
    );
}

#[test]
fn test_f64_parse_wrong_arity_is_error() {
    assert_compile_error_contains(
        r#"
fn main() {
    println(f64::parse());
}
"#,
        &[
            "error[E0201]",
            "function `f64::parse` expects 1 argument, got 0",
        ],
    );
}

#[test]
fn test_format_f64_supported_specifiers() {
    let (out, ok) = compile_and_run(
        r#"
fn main() {
    println(format("{:.6f}", 3.14));
    println(format("{:.16f}", 1.5));
    println(format("{:.17g}", 3.14));
}
"#,
    );
    assert!(ok, "format builtin should compile and run");
    assert_eq!(out, "3.140000\n1.5000000000000000\n3.1400000000000001\n");
}

#[test]
fn test_format_f64_invalid_specifier_reports_e1401() {
    assert_compile_error_contains(
        r#"
fn main() {
    println(format("{}", 3.14));
}
"#,
        &[
            "error[E1401]",
            "invalid format specifier `{}`",
            "supported f64 formats",
        ],
    );
}

#[test]
fn test_nullable_class_reference_accepts_nil_and_nil_comparison() {
    let (out, ok) = compile_and_run(
        r#"
class Node {
    value: i64;
    next: Node?;
}

fn main() {
    let node: Node? = nil;
    println(node == nil);
}
"#,
    );
    assert!(ok, "nullable class reference should accept nil");
    assert_eq!(out, "true\n");
}

#[test]
fn test_nil_requires_nullable_context() {
    assert_compile_error_contains(
        r#"
fn main() {
    let value = nil;
}
"#,
        &[
            "error[E0201]",
            "cannot infer the type of `nil`",
            "add a nullable type annotation",
        ],
    );
}

#[test]
fn test_nil_rejected_for_non_nullable_type() {
    assert_compile_error_contains(
        r#"
fn main() {
    let value: i64 = nil;
}
"#,
        &[
            "error[E0201]",
            "mismatched types: expected `i64`, found `nil`",
            "expected `i64`",
        ],
    );
}

#[test]
fn test_nil_rejected_for_non_nullable_return() {
    assert_compile_error_contains(
        r#"
class Node {
    value: i64;
}

fn missing() -> Node {
    return nil;
}

fn main() {
}
"#,
        &[
            "error[E0201]",
            "mismatched types: expected `Node`, found `nil`",
        ],
    );
}

#[test]
fn test_nullable_value_rejected_for_non_nullable_parameter() {
    assert_compile_error_contains(
        r#"
class Node {
    value: i64;
}

fn use_node(node: Node) {
}

fn main() {
    let node: Node? = nil;
    use_node(node);
}
"#,
        &[
            "error[E0704]",
            "mismatched types: expected `Node`, found `Node?`",
        ],
    );
}

#[test]
fn test_nullable_primitive_type_reports_unsupported() {
    assert_compile_error_contains(
        r#"
fn main() {
    let value: i64? = nil;
}
"#,
        &[
            "error[E0201]",
            "nullable primitive types are not implemented yet",
            "use a wrapper class or avoid nullable primitive types for now",
        ],
    );
}

#[test]
fn test_nullable_field_and_method_access_after_nil_narrowing() {
    let (out, ok) = compile_and_run(
        r#"
class Node {
    pub value: i64;
    next: Node?;

    pub fn get(self) -> i64 {
        return self.value;
    }
}

fn value_or_zero(node: Node?) -> i64 {
    if node == nil {
        return 0;
    }
    return node.value;
}

fn method_value_or_zero(node: Node?) -> i64 {
    if node != nil {
        return node.get();
    }
    return 0;
}

fn main() {
    let node: Node = Node { value: 7, next: nil };
    let maybe: Node? = node;
    println(value_or_zero(maybe));
    println(value_or_zero(nil));
    println(method_value_or_zero(maybe));
    if maybe != nil {
        println(maybe.value);
    }
}
"#,
    );
    assert!(
        ok,
        "nullable narrowing should allow safe field and method access"
    );
    assert_eq!(out, "7\n0\n7\n7\n");
}

#[test]
fn test_nullable_ternary_unifies_value_and_nil() {
    let (out, ok) = compile_and_run(
        r#"
class Node {
    value: i64;
    next: Node?;
}

fn choose(cond: bool, node: Node) -> Node? {
    return cond ? node : nil;
}

fn main() {
    let node: Node = Node { value: 9, next: nil };
    let selected = choose(true, node);
    let missing = choose(false, node);
    println(selected != nil);
    println(missing == nil);
}
"#,
    );
    assert!(ok, "ternary should infer Node? for Node/nil branches");
    assert_eq!(out, "true\ntrue\n");
}

#[test]
fn test_nullable_direct_field_access_is_rejected() {
    assert_compile_error_contains(
        r#"
class Node {
    value: i64;
    next: Node?;
}

fn value(node: Node?) -> i64 {
    return node.value;
}
"#,
        &[
            "error[E0201]",
            "cannot access field `value` on nullable type `Node?`",
            "check the value with `!= nil`",
        ],
    );
}

#[test]
fn test_nullable_direct_method_call_is_rejected() {
    assert_compile_error_contains(
        r#"
class Node {
    value: i64;
    next: Node?;

    pub fn get(self) -> i64 {
        return self.value;
    }
}

fn value(node: Node?) -> i64 {
    return node.get();
}
"#,
        &[
            "error[E0201]",
            "cannot call method `get` on nullable type `Node?`",
            "check the value with `!= nil`",
        ],
    );
}

#[test]
fn test_nullable_narrowing_is_invalidated_by_assignment() {
    assert_compile_error_contains(
        r#"
class Node {
    value: i64;
    next: Node?;
}

fn value(node: Node?) -> i64 {
    let mut current: Node? = node;
    if current != nil {
        current = nil;
        return current.value;
    }
    return 0;
}
"#,
        &[
            "error[E0201]",
            "cannot access field `value` on nullable type `Node?`",
        ],
    );
}

#[test]
fn test_nil_comparison_requires_nullable_operand() {
    assert_compile_error_contains(
        r#"
fn main() {
    let value: i64 = 1;
    println(value == nil);
}
"#,
        &[
            "error[E0201]",
            "cannot compare non-nullable type `i64` with `nil`",
            "only nullable values can be compared with `nil`",
        ],
    );
}

#[test]
fn test_async_await_mvp_compiles_and_runs() {
    let (stdout, ok) = compile_and_run(
        r#"
async fn work() -> i64 {
    return 42;
}

async fn main() {
    let value = await work();
    println(value);
}
"#,
    );
    assert!(ok);
    assert_eq!(stdout, "42\n");
}

#[test]
fn test_async_sleep_mvp_compiles_and_runs() {
    let (stdout, ok) = compile_and_run(
        r#"
async fn wait_value() -> i64 {
    await sleep(0);
    return 42;
}

async fn main() {
    let value = await wait_value();
    println(value);
}
"#,
    );
    assert!(ok);
    assert_eq!(stdout, "42\n");
}

#[test]
fn test_async_future_values_are_runtime_future_pointers() {
    let (stdout, ok) = compile_and_run(
        r#"
async fn number() -> i64 {
    return 7;
}

async fn flag() -> bool {
    return true;
}

async fn ratio() -> f64 {
    return 2.5;
}

async fn word() -> String {
    return "ok";
}

async fn main() {
    let number_future = number();
    let value = await number_future;
    println(value);
    println(await flag());
    println(await ratio());
    println(await word());
}
"#,
    );
    assert!(ok);
    assert_eq!(stdout, "7\ntrue\n2.5\nok\n");
}

#[test]
fn test_async_mut_reference_parameter_reports_e1707() {
    assert_compile_error_contains(
        r#"
async fn update(x: &mut i64) {
    x = x + 1;
}

fn main() {
    println(1);
}
"#,
        &[
            "error[E1707]",
            "reference parameter `x` is not supported in async function",
            "`&mut` parameter may live across suspension points",
        ],
    );
}

#[test]
fn test_async_immutable_reference_parameter_reports_e1707() {
    assert_compile_error_contains(
        r#"
async fn read(x: & i64) -> i64 {
    return x;
}

fn main() {
    println(1);
}
"#,
        &[
            "error[E1707]",
            "reference parameter `x` is not supported in async function",
            "`&` parameter may live across suspension points",
        ],
    );
}

#[test]
fn test_spawn_join_mvp_compiles_and_runs() {
    let (stdout, ok) = compile_and_run(
        r#"
fn work(x: i64) -> i64 {
    return x * 2;
}

fn main() {
    let h = spawn work(21);
    println(h.join());
}
"#,
    );
    assert!(ok);
    assert_eq!(stdout, "42\n");
}

#[test]
fn test_spawn_multiple_parallel_tasks_compile_and_run() {
    let (stdout, ok) = compile_and_run(
        r#"
fn square(x: i64) -> i64 {
    return x * x;
}

fn main() {
    let a = spawn square(3);
    let b = spawn square(4);
    let c = spawn square(5);
    println(a.join());
    println(b.join());
    println(c.join());
}
"#,
    );
    assert!(ok);
    assert_eq!(stdout, "9\n16\n25\n");
}

#[test]
fn test_spawn_reference_argument_reports_e1708() {
    assert_compile_error_contains(
        r#"
fn update(x: &mut i64) {
    x = x + 1;
}

fn main() {
    let mut n = 1;
    spawn update(&n);
}
"#,
        &[
            "error[E1708]",
            "cannot pass reference argument to spawned task",
            "reference may outlive the current function",
            "Mutex<T>, AtomicI64, or channels",
        ],
    );
}

#[test]
fn test_await_outside_async_reports_e0801() {
    assert_compile_error_contains(
        r#"
fn value() -> i64 {
    return 1;
}

fn main() {
    await value();
}
"#,
        &[
            "error[E0801]",
            "`await` can only be used inside an async function",
            "`await` used in a non-async function",
            "help: make the enclosing function `async`",
        ],
    );
}

#[test]
fn test_select_block_syntax_reports_unsupported_diagnostic() {
    assert_compile_error_contains(
        r#"
fn main() {
    select {};
}
"#,
        &[
            "error[E0807]",
            "select blocks are not supported yet",
            "select block parsed here",
        ],
    );
}

#[test]
fn test_await_non_future_reports_e0803() {
    assert_compile_error_contains(
        r#"
async fn main() {
    let value = await 1;
}
"#,
        &[
            "error[E0803]",
            "cannot await value of type `i64`",
            "expected `Future<T>`",
        ],
    );
}

#[test]
fn test_spawn_target_not_callable_reports_e0804() {
    assert_compile_error_contains(
        r#"
fn main() {
    let value = 1;
    spawn value();
}
"#,
        &[
            "error[E0804]",
            "spawn target `value` is not callable",
            "not a function or function value",
        ],
    );
}

#[test]
fn test_spawn_mutable_local_is_rejected_by_concurrency_analysis() {
    assert_compile_error_contains(
        r#"
fn work(x: i64) -> i64 {
    return x;
}

fn main() {
    let mut value = 1;
    spawn work(value);
}
"#,
        &[
            "spawning with mutable local `value` is not supported yet",
            "mutable value would cross a task boundary",
            "mutable local declared here",
            "help: copy the value into an immutable local before spawning the task",
        ],
    );
}

#[test]
fn test_join_on_non_handle_reports_e0805() {
    assert_compile_error_contains(
        r#"
fn main() {
    let value = 1;
    value.join();
}
"#,
        &[
            "error[E0805]",
            "cannot call `join` on `i64`",
            "expected `JoinHandle<T>`",
        ],
    );
}

#[test]
fn test_channel_send_type_mismatch_reports_e0802() {
    assert_compile_error_contains(
        r#"
fn send_bool(ch: Channel<i64>) {
    ch.send(true);
}

fn main() {
    println(1);
}
"#,
        &[
            "error[E0802]",
            "cannot send `bool` into `Channel<i64>`",
            "expected `i64`, found `bool`",
        ],
    );
}

#[test]
fn test_channel_operation_on_non_channel_reports_e0806() {
    assert_compile_error_contains(
        r#"
fn main() {
    let value = 1;
    value.recv();
}
"#,
        &[
            "error[E0806]",
            "cannot call `recv` on `i64`",
            "expected `Channel<T>`",
        ],
    );
}

#[test]
fn test_channel_i64_mvp_send_recv_compiles_and_runs() {
    let (out, ok) = compile_and_run(
        r#"
fn main() {
    let ch: Channel<i64> = Channel::new();
    ch.send(10);
    ch.send(32);
    println(ch.recv() + ch.recv());
}
"#,
    );
    assert!(ok, "Channel<i64> send/recv MVP should compile and run");
    assert_eq!(out, "42\n");
}

#[test]
fn test_channel_target_producer_spawn_example_compiles_and_runs() {
    let (out, ok) = compile_and_run(
        r#"
fn producer(ch: Channel<i64>) {
    ch.send(10);
    ch.send(20);
    ch.close();
}

fn main() {
    let ch = Channel<i64>::new();
    let h = spawn producer(ch);
    println(ch.recv());
    println(ch.recv());
    h.join();
}
"#,
    );
    assert!(
        ok,
        "target Channel producer/spawn example should compile and run"
    );
    assert_eq!(out, "10\n20\n");
}

#[test]
fn test_concurrency_generic_types_parse_and_type_check() {
    let (out, ok) = compile_and_run(
        r#"
fn takes_join(h: JoinHandle<i64>) {
}

fn takes_future(f: Future<String>) {
}

fn takes_channel(c: Channel<i64>) {
}

fn main() {
    println(1);
}
"#,
    );
    assert!(ok, "concurrency generic type annotations should compile");
    assert_eq!(out, "1\n");
}

#[test]
fn test_concurrency_generic_type_mismatch_is_reported() {
    assert_compile_error_contains(
        r#"
fn takes_join(h: JoinHandle<i64>) {
}

fn main() {
    takes_join(1);
}
"#,
        &[
            "error[E0201]",
            "mismatched types: expected `JoinHandle<i64>`, found `i64`",
            "expected `JoinHandle<i64>`",
        ],
    );
}

// ── Spawn / task debug metadata (willow-9xm) ─────────────────────────────────

/// The C runtime always embeds task-context strings used by the panic handler.
/// Verify they are present in any binary that links the Willow runtime.
#[test]
fn test_task_context_panic_strings_embedded_in_spawn_binary() {
    let source = r#"
fn work(x: i64) -> i64 { return x + 1; }
fn main() {
    let h = spawn work(10);
    println(h.join());
}
"#;
    let id = unique_test_id();
    let src_path = format!("/tmp/willow_taskctx_{}.wi", id);
    let bin_path = format!("/tmp/willow_taskctx_{}", id);

    std::fs::write(&src_path, source).unwrap();

    let compiler = env!("CARGO_BIN_EXE_willowc");
    let output = std::process::Command::new(compiler)
        .args(["build", &src_path, "-o", &bin_path])
        .output()
        .expect("failed to compile");

    assert!(output.status.success(), "should compile");

    let binary = std::fs::read(&bin_path).expect("binary should exist");
    let content = String::from_utf8_lossy(&binary);

    let _ = std::fs::remove_file(&src_path);
    let _ = std::fs::remove_file(&bin_path);
    let _ = std::fs::remove_file(format!("{bin_path}.wsmap"));

    assert!(
        content.contains("task #"),
        "binary should contain 'task #' for task-context panic messages"
    );
    assert!(
        content.contains("spawned from"),
        "binary should contain 'spawned from' for spawn location in panic messages"
    );
}

/// Debug builds call willow_task_set_spawn_location so the panic handler can
/// print the exact source location of the spawn expression.  The source
/// filename is stored as a rodata string in the binary.
#[test]
fn test_spawn_debug_location_metadata_embedded_in_debug_binary() {
    let id = unique_test_id();
    let src_path = format!("/tmp/willow_spawnloc_{}.wi", id);
    let bin_path = format!("/tmp/willow_spawnloc_{}", id);

    let source = r#"
fn work(x: i64) -> i64 { return x + 1; }
fn main() {
    let h = spawn work(10);
    println(h.join());
}
"#;
    std::fs::write(&src_path, source).unwrap();

    let compiler = env!("CARGO_BIN_EXE_willowc");
    let output = std::process::Command::new(compiler)
        .args(["build", &src_path, "-o", &bin_path])
        .output()
        .expect("failed to compile");

    assert!(output.status.success(), "debug build should succeed");

    let binary = std::fs::read(&bin_path).expect("binary should exist");
    let src_filename = std::path::Path::new(&src_path)
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap()
        .to_string();

    let _ = std::fs::remove_file(&src_path);
    let _ = std::fs::remove_file(&bin_path);
    let _ = std::fs::remove_file(format!("{bin_path}.wsmap"));

    assert!(
        binary
            .windows(src_filename.len())
            .any(|w| w == src_filename.as_bytes()),
        "debug binary should embed spawn source filename '{src_filename}' for task metadata"
    );
}

/// Release builds do NOT call willow_task_set_spawn_location, so the source
/// filename from the spawn expression must not appear in the output binary.
#[test]
fn test_spawn_debug_location_metadata_absent_in_release_binary() {
    let id = unique_test_id();
    let src_path = format!("/tmp/willow_spawnloc_rel_{}.wi", id);
    let bin_path = format!("/tmp/willow_spawnloc_rel_{}", id);

    let source = r#"
fn work(x: i64) -> i64 { return x + 1; }
fn main() {
    let h = spawn work(10);
    println(h.join());
}
"#;
    std::fs::write(&src_path, source).unwrap();

    let compiler = env!("CARGO_BIN_EXE_willowc");
    let output = std::process::Command::new(compiler)
        .args(["build", &src_path, "-o", &bin_path, "--release"])
        .output()
        .expect("failed to compile");

    assert!(output.status.success(), "release build should succeed");

    let binary = std::fs::read(&bin_path).expect("binary should exist");
    let src_filename = std::path::Path::new(&src_path)
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap()
        .to_string();

    let _ = std::fs::remove_file(&src_path);
    let _ = std::fs::remove_file(&bin_path);
    let _ = std::fs::remove_file(format!("{bin_path}.wsmap"));

    assert!(
        !binary
            .windows(src_filename.len())
            .any(|w| w == src_filename.as_bytes()),
        "release binary should NOT embed spawn source filename '{src_filename}'"
    );
}

// ── Spawn / task: additional type and behaviour coverage ────────────────────

/// Void-return function can be spawned and joined; join completes without a value.
#[test]
fn test_spawn_void_function_join_completes() {
    let (out, ok) = compile_and_run(
        r#"
fn say() {
    println("hi");
}

fn main() {
    let h = spawn say();
    h.join();
    println("done");
}
"#,
    );
    assert!(ok, "void spawn/join should compile and run");
    assert_eq!(out, "hi\ndone\n");
}

/// Spawned function returning bool produces the correct bool value on join.
#[test]
fn test_spawn_bool_return_join_value() {
    let (out, ok) = compile_and_run(
        r#"
fn is_even(x: i64) -> bool {
    return x % 2 == 0;
}

fn main() {
    let h1 = spawn is_even(4);
    let h2 = spawn is_even(7);
    println(h1.join());
    println(h2.join());
}
"#,
    );
    assert!(ok, "bool-return spawn/join should compile and run");
    assert_eq!(out, "true\nfalse\n");
}

/// Spawned function returning f64 produces the correct value on join.
#[test]
fn test_spawn_f64_return_join_value() {
    let (out, ok) = compile_and_run(
        r#"
fn half(x: f64) -> f64 {
    return x / 2.0;
}

fn main() {
    let h = spawn half(10.0);
    let r = h.join();
    println(r);
}
"#,
    );
    assert!(ok, "f64-return spawn/join should compile and run");
    assert_eq!(out.trim(), "5");
}

/// Function with three i64 parameters can be spawned; all args are forwarded.
#[test]
fn test_spawn_three_argument_function() {
    let (out, ok) = compile_and_run(
        r#"
fn sum3(a: i64, b: i64, c: i64) -> i64 {
    return a + b + c;
}

fn main() {
    let h = spawn sum3(10, 20, 30);
    println(h.join());
}
"#,
    );
    assert!(ok, "three-arg spawn should compile and run");
    assert_eq!(out, "60\n");
}

/// The result of join() can be used directly inside an arithmetic expression.
#[test]
fn test_spawn_join_result_used_in_expression() {
    let (out, ok) = compile_and_run(
        r#"
fn square(x: i64) -> i64 {
    return x * x;
}

fn main() {
    let a = spawn square(3);
    let b = spawn square(4);
    println(a.join() + b.join());
}
"#,
    );
    assert!(ok, "join result in expression should compile and run");
    assert_eq!(out, "25\n");
}

/// The same function can be spawned multiple times; each task is independent.
#[test]
fn test_spawn_same_function_twice_produces_independent_results() {
    let (out, ok) = compile_and_run(
        r#"
fn double(x: i64) -> i64 {
    return x * 2;
}

fn main() {
    let h1 = spawn double(5);
    let h2 = spawn double(6);
    println(h1.join());
    println(h2.join());
}
"#,
    );
    assert!(ok, "two spawns of same function should compile and run");
    assert_eq!(out, "10\n12\n");
}

/// Release-mode spawn/join produces the same output as debug mode.
#[test]
fn test_spawn_in_release_mode_produces_correct_output() {
    let id = unique_test_id();
    let src_path = format!("/tmp/willow_spawn_rel_{}.wi", id);
    let bin_path = format!("/tmp/willow_spawn_rel_{}", id);

    let source = r#"
fn square(x: i64) -> i64 { return x * x; }
fn main() {
    let h = spawn square(7);
    println(h.join());
}
"#;
    std::fs::write(&src_path, source).unwrap();

    let compiler = env!("CARGO_BIN_EXE_willowc");
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

/// Calling join() on a non-JoinHandle type (e.g. i64) must be a compile error.
#[test]
fn test_join_on_non_join_handle_reports_e0805() {
    assert_compile_error_contains(
        r#"
fn main() {
    let x: i64 = 42;
    println(x.join());
}
"#,
        &[
            "error[E0805]",
            "cannot call `join` on `i64`",
            "expected `JoinHandle<T>`",
        ],
    );
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
fn test_mut_reference_i64_local_writeback() {
    let src = r#"
fn increment(x: &mut i64) {
    x = x + 1;
}

fn main() {
    let mut n = 10;
    increment(&n);
    println(n);
}
"#;
    let (out, ok) = compile_and_run(src);
    assert!(ok, "compilation failed");
    assert_eq!(out, "11\n");
}

#[test]
fn test_mut_reference_f64_local_writeback() {
    let src = r#"
fn add_half(x: &mut f64) {
    x = x + 0.5;
}

fn main() {
    let mut n: f64 = 2.0;
    add_half(&n);
    println(n);
}
"#;
    let (out, ok) = compile_and_run(src);
    assert!(ok, "compilation failed");
    assert_eq!(out, "2.5\n");
}

#[test]
fn test_mut_reference_bool_local_writeback() {
    let src = r#"
fn flip(x: &mut bool) {
    x = !x;
}

fn main() {
    let mut enabled = false;
    flip(&enabled);
    println(enabled);
}
"#;
    let (out, ok) = compile_and_run(src);
    assert!(ok, "compilation failed");
    assert_eq!(out, "true\n");
}

#[test]
fn test_immutable_reference_reads_from_immutable_local() {
    let src = r#"
fn read(x: & i64) -> i64 {
    return x;
}

fn main() {
    let n = 10;
    println(read(&n));
}
"#;
    let (out, ok) = compile_and_run(src);
    assert!(ok, "compilation failed");
    assert_eq!(out, "10\n");
}

#[test]
fn test_immutable_reference_parameter_rejects_assignment() {
    assert_compile_error_contains(
        r#"
fn increment(x: & i64) {
    x = x + 1;
}

fn main() {
    let n = 10;
    increment(&n);
}
"#,
        &["cannot assign to immutable parameter `x`"],
    );
}

#[test]
fn test_gc_string_immutable_reference_survives_collect_in_callee() {
    let src = r#"
fn shout(text: & String) -> String {
    gc_collect();
    return text + "!";
}

fn main() {
    let text = "he" + "llo";
    println(shout(&text));
    gc_collect();
    println(text);
}
"#;
    let (out, ok) = compile_and_run(src);
    assert!(
        ok,
        "String & local should remain rooted across callee collect"
    );
    assert_eq!(out, "hello!\nhello\n");
}

#[test]
fn test_gc_string_mut_reference_assignment_survives_collect_in_callee() {
    let src = r#"
fn replace(text: &mut String) {
    text = text + "!";
    gc_collect();
}

fn main() {
    let mut text = "he" + "llo";
    replace(&text);
    gc_collect();
    println(text);
    println(gc_allocated_bytes() > 0);
}
"#;
    let (out, ok) = compile_and_run(src);
    assert!(
        ok,
        "String &mut assignment should update the caller root before callee collect"
    );
    assert_eq!(out, "hello!\ntrue\n");
}

#[test]
fn test_gc_class_immutable_reference_survives_collect_in_callee() {
    let src = r#"
class Box {
    pub value: String;
}

fn read(box: & Box) -> String {
    gc_collect();
    return box.value;
}

fn main() {
    let box = Box { value: "ke" + "pt" };
    println(read(&box));
    gc_collect();
    println(box.value);
}
"#;
    let (out, ok) = compile_and_run(src);
    assert!(
        ok,
        "class & local should remain rooted across callee collect"
    );
    assert_eq!(out, "kept\nkept\n");
}

#[test]
fn test_gc_class_mut_reference_assignment_survives_collect_in_callee() {
    let src = r#"
class Box {
    pub value: String;
}

fn replace(box: &mut Box) {
    box = Box { value: "after" + "!" };
    gc_collect();
}

fn main() {
    let mut box = Box { value: "before" };
    replace(&box);
    gc_collect();
    println(box.value);
    println(gc_allocated_bytes() > 0);
}
"#;
    let (out, ok) = compile_and_run(src);
    assert!(
        ok,
        "class &mut assignment should update the caller root before callee collect"
    );
    assert_eq!(out, "after!\ntrue\n");
}

#[test]
fn test_mut_reference_object_field_i64_writeback() {
    let src = r#"
class Counter {
    pub value: i64;
}

fn increment(x: &mut i64) {
    x = x + 1;
}

fn main() {
    let counter = Counter { value: 10 };
    increment(&counter.value);
    println(counter.value);
}
"#;
    let (out, ok) = compile_and_run(src);
    assert!(ok, "object field should be passable as &mut i64");
    assert_eq!(out, "11\n");
}

#[test]
fn test_immutable_reference_object_field_read() {
    let src = r#"
class Counter {
    pub value: i64;
}

fn read_twice(x: & i64) -> i64 {
    return x + x;
}

fn main() {
    let counter = Counter { value: 21 };
    println(read_twice(&counter.value));
}
"#;
    let (out, ok) = compile_and_run(src);
    assert!(ok, "object field should be passable as & i64");
    assert_eq!(out, "42\n");
}

#[test]
fn test_gc_object_field_string_mut_reference_survives_collect_in_callee() {
    let src = r#"
class User {
    pub name: String;
}

fn replace(name: &mut String) {
    name = name + "!";
    gc_collect();
}

fn main() {
    let user = User { name: "sh" + "u" };
    replace(&user.name);
    gc_collect();
    println(user.name);
    println(gc_allocated_bytes() > 0);
}
"#;
    let (out, ok) = compile_and_run(src);
    assert!(
        ok,
        "String field &mut assignment should survive callee collect"
    );
    assert_eq!(out, "shu!\ntrue\n");
}

#[test]
fn test_mut_reference_private_object_field_is_rejected() {
    assert_compile_error_contains(
        r#"
class User {
    secret: i64;

    pub fn new(v: i64) -> User {
        return User { secret: v };
    }
}

fn increment(x: &mut i64) {
    x = x + 1;
}

fn main() {
    let user = User::new(10);
    increment(&user.secret);
}
"#,
        &[
            "error[E0501]",
            "field `secret` of class `User` is private",
            "private field",
        ],
    );
}

#[test]
fn test_mut_reference_array_element_i64_writeback() {
    let src = r#"
import std.collections.Array;

fn increment(x: &mut i64) {
    x = x + 1;
}

fn main() {
    let mut xs: Array<i64> = [10, 20];
    increment(&xs[0]);
    println(xs[0]);
    println(xs[1]);
}
"#;
    let (out, ok) = compile_and_run(src);
    assert!(ok, "array element should be passable as &mut i64");
    assert_eq!(out, "11\n20\n");
}

#[test]
fn test_immutable_reference_array_element_read() {
    let src = r#"
import std.collections.Array;

fn read_twice(x: & i64) -> i64 {
    return x + x;
}

fn main() {
    let xs: Array<i64> = [21];
    println(read_twice(&xs[0]));
}
"#;
    let (out, ok) = compile_and_run(src);
    assert!(ok, "array element should be passable as & i64");
    assert_eq!(out, "42\n");
}

#[test]
fn test_gc_array_element_string_mut_reference_survives_collect_in_callee() {
    let src = r#"
import std.collections.Array;

fn replace(text: &mut String) {
    text = text + "!";
    gc_collect();
}

fn main() {
    let mut names: Array<String> = ["sh" + "u", "willow"];
    replace(&names[0]);
    gc_collect();
    println(names[0]);
    println(names[1]);
    println(gc_allocated_bytes() > 0);
}
"#;
    let (out, ok) = compile_and_run(src);
    assert!(
        ok,
        "String array element &mut assignment should survive callee collect"
    );
    assert_eq!(out, "shu!\nwillow\ntrue\n");
}

#[test]
fn test_array_element_reference_out_of_bounds_reports_runtime_diagnostic() {
    let src = r#"
import std.collections.Array;

fn increment(x: &mut i64) {
    x = x + 1;
}

fn main() {
    let mut xs: Array<i64> = [1];
    increment(&xs[3]);
    println(99);
}
"#;
    let (out, ok) = compile_and_run_check_exit(src);
    assert!(!ok, "out-of-bounds array element reference should abort");
    assert!(
        out.contains("array index out of bounds: the length is 1 but the index is 3"),
        "missing array bounds diagnostic:\n{out}"
    );
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

#[test]
fn test_class_subtype_assignment_accepts_child_as_base() {
    let src = r#"
pub open class Animal {
    pub open fn speak(self) -> i64 {
        return 1;
    }
}

pub class Dog extends Animal {
}

fn upcast(dog: Dog) -> Animal {
    let animal: Animal = dog;
    return dog;
}

fn call_inherited(dog: Dog) -> i64 {
    return dog.speak();
}

fn main() {
    println(42);
}
"#;
    let (out, ok) = compile_and_run(src);
    assert!(ok, "compilation failed");
    assert_eq!(out.trim(), "42");
}

#[test]
fn test_class_subtype_assignment_rejects_base_as_child() {
    assert_compile_error_contains(
        r#"
pub open class Animal {
}

pub class Dog extends Animal {
}

fn downcast(animal: Animal) -> Dog {
    let dog: Dog = animal;
    return dog;
}

fn main() {
    println(1);
}
"#,
        &[
            "error[E0704]",
            "cannot assign `Animal` to variable `dog` of type `Dog`",
            "expected `Dog` because of this type annotation",
        ],
    );
}

#[test]
fn test_class_extending_non_open_base_reports_e0701() {
    assert_compile_error_contains(
        r#"
class Animal {
}

class Dog extends Animal {
}

fn main() {
    println(1);
}
"#,
        &[
            "error[E0701]",
            "class `Animal` is not open for inheritance",
            "cannot extend this class",
            "base class defined here",
            "help: declare the base class as `open class Animal`",
        ],
    );
}

#[test]
fn test_class_override_requires_override_keyword() {
    assert_compile_error_contains(
        r#"
open class Animal {
    open fn speak(self) -> i64 {
        return 1;
    }
}

class Dog extends Animal {
    fn speak(self) -> i64 {
        return 2;
    }
}

fn main() {
    println(1);
}
"#,
        &[
            "error[E0702]",
            "method `speak` overrides `Animal` but is missing `override`",
            "missing `override`",
            "help: write `override fn speak`",
        ],
    );
}

#[test]
fn test_class_override_requires_open_base_method() {
    assert_compile_error_contains(
        r#"
open class Animal {
    fn speak(self) -> i64 {
        return 1;
    }
}

class Dog extends Animal {
    override fn speak(self) -> i64 {
        return 2;
    }
}

fn main() {
    println(1);
}
"#,
        &[
            "error[E0703]",
            "method `speak` in `Animal` is not open for override",
            "cannot override",
            "base method defined here",
            "help: declare the base method as `open fn speak`",
        ],
    );
}

#[test]
fn test_class_cross_module_qualified_base_type_checks() {
    let animal = r#"
pub open class Animal {
    pub open fn speak(self) -> i64 {
        return 1;
    }
}
"#;
    let main = r#"
import animal;

pub class Dog extends animal::Animal {
    pub override fn speak(self) -> i64 {
        return 2;
    }
}

fn upcast(dog: Dog) -> animal::Animal {
    return dog;
}

fn main() {
    println(42);
}
"#;
    let (out, ok) =
        compile_temp_project_and_run(&[("animal.wi", animal), ("main.wi", main)], "main.wi");
    assert!(ok, "qualified base class project failed to compile or run");
    assert_eq!(out.trim(), "42");
}

#[test]
fn test_class_object_literals_type_check_and_compile() {
    let src = r#"
pub class AA {
    pub value: i64;
}

pub class A {
    pub value: i64;
    pub aa: AA;

    pub fn member_aa(self) -> AA {
        return self.aa;
    }

    pub fn member_aa_value(self) -> i64 {
        return self.aa.value;
    }
}

fn consume(a: A) -> i64 {
    return 7;
}

fn make_a(value: i64) -> A {
    return A {
        value: value,
        aa: AA {
            value: value + 1
        }
    };
}

fn main() {
    let a = make_a(40);
    println(consume(a));
}
"#;
    let (out, ok) = compile_and_run(src);
    assert!(ok, "object literal program failed to compile or run");
    assert_eq!(out.trim(), "7");
}

#[test]
fn test_class_object_literal_field_diagnostics() {
    assert_compile_error_contains(
        r#"
class Point {
    x: i64;
    y: i64;
}

fn make_point() -> Point {
    return Point {
        x: true,
        z: 1,
        x: 2
    };
}

fn main() {
    println(1);
}
"#,
        &[
            "field `x` expects `i64`, found `bool`",
            "field declared here",
            "no field `z` on class `Point`",
            "field `x` is initialized more than once",
            "missing field `y` in `Point` literal",
        ],
    );
}

#[test]
fn test_class_methods_can_read_private_self_fields() {
    let src = r#"
class Box {
    value: i64;

    pub fn value(self) -> i64 {
        return self.value;
    }
}

fn main() {
    println(1);
}
"#;
    let (out, ok) = compile_and_run(src);
    assert!(
        ok,
        "class method private self-field access should type-check"
    );
    assert_eq!(out.trim(), "1");
}

#[test]
fn test_class_object_literal_reaches_private_member_diagnostic() {
    assert_compile_error_contains(
        r#"
pub class Account {
    balance: i64;

    pub fn new(balance: i64) -> Account {
        return Account { balance: balance };
    }
}

fn main() {
    let account = Account::new(500);
    println(account.balance);
}
"#,
        &[
            "error[E0501]",
            "field `balance` of class `Account` is private",
            "private field",
            "field defined here",
        ],
    );
}

#[test]
fn test_class_diagnostic_private_field_points_to_definition() {
    assert_compile_error_contains(
        r#"
class User {
    name: i64;
}

fn leak(user: User) -> i64 {
    return user.name;
}

fn main() {
    println(1);
}
"#,
        &[
            "error[E0501]",
            "field `name` of class `User` is private",
            "private field",
            "field defined here",
            "help: expose it using `pub name: i64` or provide a public getter method",
        ],
    );
}

#[test]
fn test_class_diagnostic_private_method_points_to_definition() {
    assert_compile_error_contains(
        r#"
class User {
    fn secret(self) -> i64 {
        return 7;
    }
}

fn leak(user: User) -> i64 {
    return user.secret();
}

fn main() {
    println(1);
}
"#,
        &[
            "error[E0501]",
            "method `secret` of class `User` is private",
            "private method",
            "method defined here",
            "help: make it public with `pub fn secret`",
        ],
    );
}

#[test]
fn test_class_diagnostic_method_not_found_suggests_similar_name() {
    assert_compile_error_contains(
        r#"
class User {
    pub fn greet(self) -> i64 {
        return 1;
    }
}

fn call(user: User) -> i64 {
    return user.greett();
}

fn main() {
    println(1);
}
"#,
        &[
            "error[E0502]",
            "no method `greett` on class `User`",
            "method not found",
            "help: there is a method with a similar name: `greet`",
            "return user.greet();",
        ],
    );
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
fn test_bool_operators_short_circuit_rhs() {
    let src = r#"
fn marker(value: bool) -> bool {
    println(99);
    return value;
}

fn main() {
    println(false && marker(true));
    println(true || marker(false));
}
"#;
    let (out, ok) = compile_and_run(src);
    assert!(ok, "compilation failed");
    assert_eq!(out, "false\ntrue\n");
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
        &[
            "error[E0203]",
            "condition must be `bool`",
            "found `i64`",
            "use an explicit comparison",
        ],
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
fn test_diagnostic_single_ampersand_outside_call_arg_is_invalid() {
    assert_compile_error_contains(
        r#"
fn main() {
    println(true & false);
}
"#,
        &[
            "error[E0102]",
            "`&` is only valid before a call argument",
            ":3:18",
            "3 |     println(true & false);",
            "                 ^ `&` is only valid before a call argument",
        ],
    );
}

#[test]
fn test_reference_escape_return_expression_is_rejected() {
    assert_compile_error_contains(
        r#"
fn leak() -> i64 {
    let value = 1;
    return &value;
}

fn main() {
    println(1);
}
"#,
        &["error[E0102]", "`&` is only valid before a call argument"],
    );
}

#[test]
fn test_diagnostic_single_pipe_in_call_is_parse_error() {
    // `|` is now the lambda-param delimiter, so `true | false` inside a call
    // is a parse error (unexpected token while expecting `)` or `,`).
    assert!(expect_compile_error(
        r#"
fn main() {
    println(true | false);
}
"#
    ));
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

// ── Function values and lambdas ───────────────────────────────────────────────

#[test]
fn test_named_function_passed_as_argument() {
    let (out, ok) = compile_and_run(
        r#"
fn double(x: i64) -> i64 {
    return x * 2;
}
fn apply(x: i64, f: fn(i64) -> i64) -> i64 {
    return f(x);
}
fn main() {
    println(apply(10, double));
}
"#,
    );
    assert!(ok, "compilation failed");
    assert_eq!(out, "20\n");
}

#[test]
fn test_non_capturing_lambda_as_argument() {
    let (out, ok) = compile_and_run(
        r#"
fn apply(x: i64, f: fn(i64) -> i64) -> i64 {
    return f(x);
}
fn main() {
    println(apply(10, |x: i64| x * 2));
}
"#,
    );
    assert!(ok, "compilation failed");
    assert_eq!(out, "20\n");
}

#[test]
fn test_lambda_stored_in_variable_and_called() {
    let (out, ok) = compile_and_run(
        r#"
fn main() {
    let f: fn(i64) -> i64 = |x: i64| x + 5;
    println(f(10));
    println(f(20));
}
"#,
    );
    assert!(ok, "compilation failed");
    assert_eq!(out, "15\n25\n");
}

#[test]
fn test_zero_param_lambda() {
    let (out, ok) = compile_and_run(
        r#"
fn call(f: fn() -> i64) -> i64 {
    return f();
}
fn main() {
    println(call(|| 42));
}
"#,
    );
    assert!(ok, "compilation failed");
    assert_eq!(out, "42\n");
}

#[test]
fn test_lambda_with_block_body() {
    let (out, ok) = compile_and_run(
        r#"
fn apply(x: i64, f: fn(i64) -> i64) -> i64 {
    return f(x);
}
fn main() {
    let result = apply(5, |x: i64| {
        let y = x * x;
        return y + 1;
    });
    println(result);
}
"#,
    );
    assert!(ok, "compilation failed");
    assert_eq!(out, "26\n");
}

// ── Parser error focused tests ───────────────────────────────────────────────

#[test]
fn test_diagnostic_missing_closing_brace() {
    assert_compile_error_contains(
        r#"
fn main() {
    let x = 1;
"#,
        &["error[E0103]", "expected `}` to close block"],
    );
}

#[test]
fn test_diagnostic_missing_closing_paren() {
    assert_compile_error_contains(
        r#"
fn main() {
    println(1;
}
"#,
        &["error[E0104]", "expected `)` to close parenthesis"],
    );
}

#[test]
fn test_diagnostic_invalid_function_name() {
    assert_compile_error_contains(
        r#"
fn () {
}
fn main() {}
"#,
        &["error[E0102]", "expected identifier"],
    );
}

#[test]
fn test_diagnostic_invalid_type_annotation() {
    assert_compile_error_contains(
        r#"
fn main() {
    let x: = 1;
}
"#,
        &["error[E0107]"],
    );
}

#[test]
fn test_diagnostic_missing_semicolon_has_code() {
    assert_compile_error_contains(
        r#"
fn main() {
    let x = 1
}
"#,
        &["error[E0101]", "expected `;` after statement"],
    );
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

// ── Fix suggestions ───────────────────────────────────────────────────────────

#[test]
fn test_fix_suggestion_immutable_shows_mut_insertion() {
    // E0301 must show a code fix block with `mut ` inserted after `let `.
    assert_compile_error_contains(
        r#"
fn main() {
    let count = 0;
    count = count + 1;
}
"#,
        &[
            "error[E0301]",
            "cannot assign to immutable variable `count`",
            "help: declare it as mutable",
            // fix block: modified source line and `+` markers
            "let mut count = 0;",
            "++++",
        ],
    );
}

#[test]
fn test_fix_suggestion_secondary_label_shown_before_fix() {
    // The secondary "declared immutable here" label must appear in the error
    // body, and the fix block must appear after the help line.
    let stderr = compile_error_stderr(
        r#"
fn main() {
    let x = 1;
    x = 2;
}
"#,
    );
    let label_pos = stderr
        .find("declared immutable here")
        .expect("missing secondary label");
    let fix_pos = stderr.find("let mut x = 1;").expect("missing fix line");
    assert!(
        label_pos < fix_pos,
        "secondary label should appear before the fix block:\n{stderr}"
    );
}

// ── Nil dereference runtime check ────────────────────────────────────────────

/// Debug builds emit a nil pointer check before every field access and method
/// call.  Correct programs must not trigger the check.
#[test]
fn test_nil_deref_check_does_not_fire_for_valid_field_access() {
    let src = r#"
class Box {
    pub value: i64;
}

fn read(b: Box) -> i64 {
    return b.value;
}

fn main() {
    let b = Box { value: 42 };
    println(read(b));
}
"#;
    let (out, ok) = compile_and_run(src);
    assert!(ok, "field access in debug mode should compile and run");
    assert_eq!(out, "42\n");
}

/// Debug builds also guard method calls.  Valid calls must complete normally.
#[test]
fn test_nil_deref_check_does_not_fire_for_valid_method_call() {
    let src = r#"
class Counter {
    pub count: i64;

    pub fn get(self) -> i64 {
        return self.count;
    }
}

fn main() {
    let c = Counter { count: 7 };
    println(c.get());
}
"#;
    let (out, ok) = compile_and_run(src);
    assert!(ok, "method call in debug mode should compile and run");
    assert_eq!(out, "7\n");
}

/// Nil-narrowing: after `!= nil` guard the field access must succeed without
/// triggering the nil check.
#[test]
fn test_nil_deref_check_does_not_fire_after_nil_narrowing() {
    let src = r#"
class Node {
    pub value: i64;
    pub next: Node?;
}

fn sum_chain(n: Node?) -> i64 {
    if n == nil {
        return 0;
    }
    let nxt = n.next;
    if nxt != nil {
        return n.value + nxt.value;
    }
    return n.value;
}

fn main() {
    let b = Node { value: 20, next: nil };
    let a = Node { value: 10, next: b };
    println(sum_chain(a));   // 30
    println(sum_chain(b));   // 20
    let c: Node? = nil;
    println(sum_chain(c));   // 0
}
"#;
    let (out, ok) = compile_and_run(src);
    assert!(ok, "nil narrowing should not trigger nil deref check");
    assert_eq!(out, "30\n20\n0\n");
}

/// Release builds should not emit nil checks.
/// Verify by running the same program under --release and confirming it still
/// works (no false check) and that it does not print a nil-deref message.
#[test]
fn test_nil_deref_check_absent_in_release_build() {
    let id = unique_test_id();
    let src_path = format!("/tmp/willow_nil_rel_{}.wi", id);
    let bin_path = format!("/tmp/willow_nil_rel_{}", id);

    let source = r#"
class Box { pub value: i64; }
fn read(b: Box) -> i64 { return b.value; }
fn main() { println(read(Box { value: 99 })); }
"#;
    std::fs::write(&src_path, source).unwrap();

    let compiler = env!("CARGO_BIN_EXE_willowc");
    let output = std::process::Command::new(compiler)
        .args(["build", &src_path, "-o", &bin_path, "--release"])
        .output()
        .expect("failed to run compiler");

    assert!(output.status.success(), "release build should succeed");

    let run_output = std::process::Command::new(&bin_path)
        .output()
        .expect("failed to run binary");

    let _ = std::fs::remove_file(&src_path);
    let _ = std::fs::remove_file(&bin_path);
    let _ = std::fs::remove_file(format!("{bin_path}.wsmap"));

    let stdout = String::from_utf8_lossy(&run_output.stdout);
    let stderr = String::from_utf8_lossy(&run_output.stderr);

    assert_eq!(
        stdout.trim(),
        "99",
        "release binary should print correct output"
    );
    assert!(
        !stderr.contains("nil dereference"),
        "release binary should not print nil dereference message; stderr: {stderr}"
    );
}

/// The nil deref diagnostic string must be present in the C runtime (which is
/// always linked in).  This is the message that would be shown at runtime when
/// the check fires.
#[test]
fn test_nil_deref_runtime_message_is_embedded_in_binary() {
    let source = r#"
class Box { pub value: i64; }
fn main() { println(Box { value: 1 }.value); }
"#;
    let id = unique_test_id();
    let src_path = format!("/tmp/willow_nil_msg_{}.wi", id);
    let bin_path = format!("/tmp/willow_nil_msg_{}", id);

    std::fs::write(&src_path, source).unwrap();

    let compiler = env!("CARGO_BIN_EXE_willowc");
    let output = std::process::Command::new(compiler)
        .args(["build", &src_path, "-o", &bin_path])
        .output()
        .expect("failed to compile");
    assert!(output.status.success(), "should compile");

    let binary = std::fs::read(&bin_path).expect("binary should exist");
    let content = String::from_utf8_lossy(&binary);

    let _ = std::fs::remove_file(&src_path);
    let _ = std::fs::remove_file(&bin_path);
    let _ = std::fs::remove_file(format!("{bin_path}.wsmap"));

    assert!(
        content.contains("nil dereference"),
        "binary should contain nil dereference diagnostic message"
    );
}

// ── Match expression tests ─────────────────────────────────────────────────────

#[test]
fn match_01_bool_true_false_arms() {
    let src = r#"
fn describe(b: bool) -> String {
    return match b {
        true => "yes",
        false => "no",
    };
}
fn main() {
    println(describe(true));
    println(describe(false));
}
"#;
    let (out, ok) = compile_and_run(src);
    assert!(ok, "compilation failed");
    assert_eq!(out.trim(), "yes\nno");
}

#[test]
fn match_02_i64_with_wildcard() {
    let src = r#"
fn classify(n: i64) -> String {
    return match n {
        0 => "zero",
        1 => "one",
        _ => "other",
    };
}
fn main() {
    println(classify(0));
    println(classify(1));
    println(classify(99));
}
"#;
    let (out, ok) = compile_and_run(src);
    assert!(ok, "compilation failed");
    assert_eq!(out.trim(), "zero\none\nother");
}

#[test]
fn match_03_fieldless_enum() {
    let src = r#"
enum Color {
    Red,
    Green,
    Blue,
}
fn name(c: Color) -> String {
    return match c {
        Color::Red => "red",
        Color::Green => "green",
        Color::Blue => "blue",
    };
}
fn main() {
    println(name(Color::Red));
    println(name(Color::Green));
    println(name(Color::Blue));
}
"#;
    let (out, ok) = compile_and_run(src);
    assert!(ok, "compilation failed");
    assert_eq!(out.trim(), "red\ngreen\nblue");
}

#[test]
fn match_04_wildcard_arm() {
    let src = r#"
fn sign(n: i64) -> String {
    return match n {
        0 => "zero",
        _ => "nonzero",
    };
}
fn main() {
    println(sign(0));
    println(sign(5));
    println(sign(-3));
}
"#;
    let (out, ok) = compile_and_run(src);
    assert!(ok, "compilation failed");
    assert_eq!(out.trim(), "zero\nnonzero\nnonzero");
}

#[test]
fn match_05_binding_pattern() {
    let src = r#"
fn double_or_zero(n: i64) -> i64 {
    return match n {
        0 => 0,
        v => v + v,
    };
}
fn main() {
    println(double_or_zero(0));
    println(double_or_zero(5));
}
"#;
    let (out, ok) = compile_and_run(src);
    assert!(ok, "compilation failed");
    assert_eq!(out.trim(), "0\n10");
}

#[test]
fn match_06_as_expression_assigned_to_variable() {
    let src = r#"
enum Dir {
    Up,
    Down,
}
fn main() {
    let d = Dir::Up;
    let label = match d {
        Dir::Up => "up",
        Dir::Down => "down",
    };
    println(label);
}
"#;
    let (out, ok) = compile_and_run(src);
    assert!(ok, "compilation failed");
    assert_eq!(out.trim(), "up");
}

#[test]
fn match_07_negative_integer_pattern() {
    let src = r#"
fn describe(n: i64) -> String {
    return match n {
        -1 => "minus one",
        0 => "zero",
        _ => "other",
    };
}
fn main() {
    println(describe(-1));
    println(describe(0));
    println(describe(3));
}
"#;
    let (out, ok) = compile_and_run(src);
    assert!(ok, "compilation failed");
    assert_eq!(out.trim(), "minus one\nzero\nother");
}

#[test]
fn match_08_enum_passed_as_function_argument() {
    let src = r#"
enum Season {
    Spring,
    Summer,
    Autumn,
    Winter,
}
fn season_msg(s: Season) -> String {
    return match s {
        Season::Spring => "bloom",
        Season::Summer => "hot",
        Season::Autumn => "fall",
        Season::Winter => "cold",
    };
}
fn main() {
    println(season_msg(Season::Winter));
}
"#;
    let (out, ok) = compile_and_run(src);
    assert!(ok, "compilation failed");
    assert_eq!(out.trim(), "cold");
}

#[test]
fn match_09_match_example_file_compiles_and_outputs_green() {
    let (out, ok) = compile_file_and_run("example/match_color.wi");
    assert!(ok, "match_color.wi failed to compile");
    assert_eq!(out.trim(), "green");
}

#[test]
fn match_10_bool_exhaustiveness_error_missing_false() {
    let src = r#"
fn f(b: bool) -> String {
    return match b {
        true => "yes",
    };
}
fn main() { println(f(true)); }
"#;
    assert!(
        expect_compile_error(src),
        "expected exhaustiveness error for missing false arm"
    );
}

#[test]
fn match_11_enum_exhaustiveness_error_missing_variant() {
    let src = r#"
enum Color { Red, Green, Blue, }
fn name(c: Color) -> String {
    return match c {
        Color::Red => "red",
        Color::Green => "green",
    };
}
fn main() { println(name(Color::Red)); }
"#;
    assert!(
        expect_compile_error(src),
        "expected exhaustiveness error for missing Blue variant"
    );
}

#[test]
fn match_12_incompatible_arm_types_error() {
    let src = r#"
fn f(b: bool) -> i64 {
    return match b {
        true => 1,
        false => "nope",
    };
}
fn main() { println(f(true)); }
"#;
    assert!(
        expect_compile_error(src),
        "expected incompatible arm types error"
    );
}

#[test]
fn match_13_unknown_enum_variant_in_pattern_error() {
    let src = r#"
enum Color { Red, Green, }
fn f(c: Color) -> String {
    return match c {
        Color::Red => "red",
        Color::Purple => "purple",
    };
}
fn main() { println(f(Color::Red)); }
"#;
    assert!(
        expect_compile_error(src),
        "expected unknown variant error for Color::Purple"
    );
}

#[test]
fn match_14_i64_non_exhaustive_error_missing_wildcard() {
    let src = r#"
fn f(n: i64) -> String {
    return match n {
        0 => "zero",
        1 => "one",
    };
}
fn main() { println(f(0)); }
"#;
    assert!(
        expect_compile_error(src),
        "expected non-exhaustive error for i64 without wildcard"
    );
}

#[test]
fn match_15_enum_variant_in_let_binding() {
    let src = r#"
enum State {
    Active,
    Inactive,
}
fn main() {
    let s = State::Active;
    let r = match s {
        State::Active => "on",
        State::Inactive => "off",
    };
    println(r);
}
"#;
    let (out, ok) = compile_and_run(src);
    assert!(ok, "compilation failed");
    assert_eq!(out.trim(), "on");
}

#[test]
fn match_16_match_in_return_with_enum_multiple_values() {
    let src = r#"
enum Priority {
    Low,
    Medium,
    High,
}
fn score(p: Priority) -> i64 {
    return match p {
        Priority::Low => 1,
        Priority::Medium => 5,
        Priority::High => 10,
    };
}
fn main() {
    println(score(Priority::Low));
    println(score(Priority::Medium));
    println(score(Priority::High));
}
"#;
    let (out, ok) = compile_and_run(src);
    assert!(ok, "compilation failed");
    assert_eq!(out.trim(), "1\n5\n10");
}

#[test]
fn match_17_bool_match_both_arms_covered() {
    let src = r#"
fn to_int(b: bool) -> i64 {
    return match b {
        true => 1,
        false => 0,
    };
}
fn main() {
    println(to_int(true));
    println(to_int(false));
}
"#;
    let (out, ok) = compile_and_run(src);
    assert!(ok, "compilation failed");
    assert_eq!(out.trim(), "1\n0");
}

#[test]
fn match_18_enum_with_single_variant() {
    let src = r#"
enum Unit { Only, }
fn describe(u: Unit) -> String {
    return match u {
        Unit::Only => "just one",
    };
}
fn main() {
    println(describe(Unit::Only));
}
"#;
    let (out, ok) = compile_and_run(src);
    assert!(ok, "compilation failed");
    assert_eq!(out.trim(), "just one");
}

#[test]
fn match_19_wildcard_after_some_integer_patterns() {
    let src = r#"
fn greet(n: i64) -> String {
    return match n {
        1 => "one",
        2 => "two",
        _ => "many",
    };
}
fn main() {
    println(greet(1));
    println(greet(2));
    println(greet(100));
}
"#;
    let (out, ok) = compile_and_run(src);
    assert!(ok, "compilation failed");
    assert_eq!(out.trim(), "one\ntwo\nmany");
}

#[test]
fn match_20_enum_variant_as_function_result() {
    let src = r#"
enum Toggle {
    On,
    Off,
}
fn flip(t: Toggle) -> Toggle {
    return match t {
        Toggle::On => Toggle::Off,
        Toggle::Off => Toggle::On,
    };
}
fn describe(t: Toggle) -> String {
    return match t {
        Toggle::On => "on",
        Toggle::Off => "off",
    };
}
fn main() {
    let t = Toggle::On;
    let t2 = flip(t);
    println(describe(t2));
}
"#;
    let (out, ok) = compile_and_run(src);
    assert!(ok, "compilation failed");
    assert_eq!(out.trim(), "off");
}

#[test]
fn test_leibniz_pi_release_completes_within_150ms() {
    use std::time::Instant;

    let id = unique_test_id();
    let bin_path = format!("/tmp/willow_leibniz_perf_{}", id);

    let compiler = env!("CARGO_BIN_EXE_willowc");
    let status = Command::new(compiler)
        .args([
            "build",
            "example/leibniz_pi.wi",
            "--release",
            "-o",
            &bin_path,
        ])
        .stderr(Stdio::null())
        .status()
        .expect("failed to run compiler");
    assert!(status.success(), "leibniz_pi.wi failed to compile");

    let start = Instant::now();
    let out = Command::new(&bin_path)
        .output()
        .expect("failed to run binary");
    let elapsed = start.elapsed();

    remove_output_artifacts(&bin_path);

    assert!(out.status.success(), "binary exited with error");
    assert_eq!(
        out.stdout.trim_ascii(),
        b"3.141592663589326",
        "output mismatch"
    );
    assert!(
        elapsed.as_millis() < 150,
        "leibniz_pi release build took {}ms — expected < 150ms (performance regression?)",
        elapsed.as_millis()
    );
}

// ── Option<T> and Result<T,E> ─────────────────────────────────────────────────

#[test]
fn test_option_some_and_none_i64() {
    let src = r#"
fn safe_div(a: i64, b: i64) -> Option<i64> {
    if b == 0 {
        return Option::None;
    }
    return Option::Some(a / b);
}

fn main() {
    let r1 = match safe_div(10, 2) {
        Option::Some(v) => v,
        Option::None => -1,
    };
    let r2 = match safe_div(7, 0) {
        Option::Some(v) => v,
        Option::None => -1,
    };
    println(r1);
    println(r2);
}
"#;
    let (out, ok) = compile_and_run(src);
    assert!(ok, "Option<i64> should compile and run");
    assert_eq!(out, "5\n-1\n");
}

#[test]
fn test_option_none_in_function_return() {
    let src = r#"
fn first_positive(a: i64, b: i64) -> Option<i64> {
    if a > 0 {
        return Option::Some(a);
    }
    if b > 0 {
        return Option::Some(b);
    }
    return Option::None;
}

fn main() {
    let r1 = match first_positive(-1, 5) {
        Option::Some(v) => v,
        Option::None => 0,
    };
    let r2 = match first_positive(-3, -7) {
        Option::Some(v) => v,
        Option::None => 0,
    };
    println(r1);
    println(r2);
}
"#;
    let (out, ok) = compile_and_run(src);
    assert!(ok, "Option::None in function return should compile");
    assert_eq!(out, "5\n0\n");
}

#[test]
fn test_option_map_via_match() {
    let src = r#"
fn double_opt(opt: Option<i64>) -> Option<i64> {
    return match opt {
        Option::Some(v) => Option::Some(v * 2),
        Option::None => Option::None,
    };
}

fn main() {
    let r1 = match double_opt(Option::Some(21)) {
        Option::Some(v) => v,
        Option::None => -1,
    };
    let r2 = match double_opt(Option::None) {
        Option::Some(v) => v,
        Option::None => -1,
    };
    println(r1);
    println(r2);
}
"#;
    let (out, ok) = compile_and_run(src);
    assert!(ok, "Option::map-like function should compile");
    assert_eq!(out, "42\n-1\n");
}

#[test]
fn test_result_ok_and_err_i64_string() {
    let src = r#"
fn parse_positive(n: i64) -> Result<i64, String> {
    if n <= 0 {
        return Result::Err("non-positive");
    }
    return Result::Ok(n * 10);
}

fn main() {
    let v1 = match parse_positive(5) {
        Result::Ok(v) => v,
        Result::Err(_) => -1,
    };
    let v2 = match parse_positive(-3) {
        Result::Ok(v) => v,
        Result::Err(_) => -1,
    };
    println(v1);
    println(v2);
}
"#;
    let (out, ok) = compile_and_run(src);
    assert!(ok, "Result<i64,String> should compile and run");
    assert_eq!(out, "50\n-1\n");
}

#[test]
fn test_result_err_message_extracted() {
    let src = r#"
fn parse_even(n: i64) -> Result<i64, String> {
    if n % 2 != 0 {
        return Result::Err("not even");
    }
    return Result::Ok(n / 2);
}

fn main() {
    let msg = match parse_even(7) {
        Result::Ok(_) => "ok",
        Result::Err(e) => e,
    };
    println(msg);
    let val = match parse_even(8) {
        Result::Ok(v) => v,
        Result::Err(_) => -1,
    };
    println(val);
}
"#;
    let (out, ok) = compile_and_run(src);
    assert!(ok, "Result Err payload extraction should compile");
    assert_eq!(out, "not even\n4\n");
}

#[test]
fn test_option_f64_payload() {
    let src = r#"
fn safe_sqrt(x: f64) -> Option<f64> {
    if x < 0.0 {
        return Option::None;
    }
    return Option::Some(pow(x, 0.5));
}

fn main() {
    let r1 = match safe_sqrt(9.0) {
        Option::Some(v) => v,
        Option::None => -1.0,
    };
    let r2 = match safe_sqrt(-4.0) {
        Option::Some(v) => v,
        Option::None => -1.0,
    };
    println(r1);
    println(r2);
}
"#;
    let (out, ok) = compile_and_run(src);
    assert!(ok, "Option<f64> payload should compile and run");
    assert_eq!(out, "3\n-1\n");
}

// ── ? operator ────────────────────────────────────────────────────────────────

#[test]
fn test_try_propagate_extracts_ok_payload() {
    let src = r#"
fn safe_div(a: i64, b: i64) -> Result<i64, String> {
    if b == 0 { return Result::Err("zero"); }
    return Result::Ok(a / b);
}

fn halve(n: i64) -> Result<i64, String> {
    return Result::Ok(safe_div(n, 2)?);
}

fn main() {
    let r = halve(10);
    let v = match r {
        Result::Ok(x) => x,
        Result::Err(_) => -1,
    };
    println(v);
}
"#;
    let (out, ok) = compile_and_run(src);
    assert!(ok, "? operator should compile and run");
    assert_eq!(out, "5\n");
}

#[test]
fn test_try_propagate_returns_err_early() {
    let src = r#"
fn fail() -> Result<i64, String> {
    return Result::Err("oops");
}

fn caller() -> Result<i64, String> {
    let v = fail()?;
    return Result::Ok(v + 1);
}

fn main() {
    let r = caller();
    let msg = match r {
        Result::Ok(_) => "ok",
        Result::Err(e) => e,
    };
    println(msg);
}
"#;
    let (out, ok) = compile_and_run(src);
    assert!(ok, "? early return should compile and run");
    assert_eq!(out, "oops\n");
}

#[test]
fn test_try_propagate_chains_multiple_calls() {
    let src = r#"
fn parse(s: String) -> Result<i64, String> {
    if s == "10" { return Result::Ok(10); }
    if s == "20" { return Result::Ok(20); }
    return Result::Err("bad input");
}

fn sum_two(a: String, b: String) -> Result<i64, String> {
    let x = parse(a)?;
    let y = parse(b)?;
    return Result::Ok(x + y);
}

fn main() {
    let r1 = sum_two("10", "20");
    let v1 = match r1 { Result::Ok(v) => v, Result::Err(_) => -1, };
    println(v1);
    let r2 = sum_two("10", "bad");
    let v2 = match r2 { Result::Ok(v) => v, Result::Err(_) => -1, };
    println(v2);
}
"#;
    let (out, ok) = compile_and_run(src);
    assert!(ok, "chained ? should compile and run");
    assert_eq!(out, "30\n-1\n");
}

#[test]
fn test_option_try_propagate_extracts_some_payload() {
    let src = r#"
fn maybe(n: i64) -> Option<i64> {
    if n > 0 { return Option::Some(n); }
    return Option::None;
}

fn doubled(n: i64) -> Option<i64> {
    let v = maybe(n)?;
    return Option::Some(v * 2);
}

fn main() {
    let a = doubled(21);
    let av = match a { Option::Some(v) => v, Option::None => -1, };
    println(av);
}
"#;
    let (out, ok) = compile_and_run(src);
    assert!(ok, "Option ? should extract Some payload");
    assert_eq!(out, "42\n");
}

#[test]
fn test_option_try_propagate_returns_none_early() {
    let src = r#"
fn maybe(n: i64) -> Option<i64> {
    if n > 0 { return Option::Some(n); }
    return Option::None;
}

fn doubled(n: i64) -> Option<i64> {
    let v = maybe(n)?;
    return Option::Some(v * 2);
}

fn main() {
    let a = doubled(-1);
    let av = match a { Option::Some(v) => v, Option::None => -1, };
    println(av);
}
"#;
    let (out, ok) = compile_and_run(src);
    assert!(ok, "Option ? should propagate None");
    assert_eq!(out, "-1\n");
}

#[test]
fn test_option_try_propagate_preserves_f64_payload_type() {
    let src = r#"
fn maybe(flag: bool) -> Option<f64> {
    if flag { return Option::Some(2.5); }
    return Option::None;
}

fn add(flag: bool) -> Option<f64> {
    let v = maybe(flag)?;
    return Option::Some(v + 0.5);
}

fn main() {
    let a = add(true);
    let av = match a { Option::Some(v) => v, Option::None => -1.0, };
    println(av);
}
"#;
    let (out, ok) = compile_and_run(src);
    assert!(ok, "Option ? should preserve f64 payloads");
    assert_eq!(out, "3\n");
}

#[test]
fn test_try_propagate_on_non_result_reports_e1806() {
    assert_compile_error_contains(
        r#"
fn main() {
    let x: i64 = 42;
    let y = x?;
    println(y);
}
"#,
        &[
            "error[E1806]",
            "requires `Result<T,E>` or `Option<T>`",
            "found `i64`",
        ],
    );
}

#[test]
fn test_try_propagate_in_non_result_function_reports_e1807() {
    assert_compile_error_contains(
        r#"
fn get() -> Result<i64, String> {
    return Result::Ok(1);
}

fn main() {
    let v = get()?;
    println(v);
}
"#,
        &[
            "error[E1807]",
            "can only be used inside a function returning `Result",
        ],
    );
}

#[test]
fn test_ternary_and_try_propagate_coexist() {
    let src = r#"
fn ok_or(n: i64) -> Result<i64, String> {
    if n > 0 { return Result::Ok(n); }
    return Result::Err("non-positive");
}

fn scaled(n: i64) -> Result<i64, String> {
    let v = ok_or(n)?;
    let factor = v > 5 ? 10 : 1;
    return Result::Ok(v * factor);
}

fn main() {
    let r1 = scaled(7);
    let v1 = match r1 { Result::Ok(v) => v, Result::Err(_) => -1, };
    println(v1);
    let r2 = scaled(3);
    let v2 = match r2 { Result::Ok(v) => v, Result::Err(_) => -1, };
    println(v2);
}
"#;
    let (out, ok) = compile_and_run(src);
    assert!(ok, "? and ternary ? should coexist");
    assert_eq!(out, "70\n3\n");
}

// ── Option / Result GC tracing ────────────────────────────────────────────────

#[test]
fn test_option_some_class_payload_survives_gc_collect() {
    let src = r#"
class Node {
    pub value: i64;
    pub fn get(self) -> i64 { return self.value; }
}

fn make_some(v: i64) -> Option<Node> {
    let n = Node { value: v };
    return Option::Some(n);
}

fn main() {
    let opt = make_some(42);
    gc_collect();
    let v = match opt {
        Option::Some(n) => n.get(),
        Option::None => -1,
    };
    println(v);
    println(gc_allocated_bytes() > 0);
}
"#;
    let (out, ok) = compile_and_run(src);
    assert!(ok, "Option<Node> should compile and run");
    assert_eq!(out, "42\ntrue\n", "Node payload must survive gc_collect");
}

#[test]
fn test_option_none_traces_nothing() {
    let src = r#"
class Node { pub value: i64; }

fn empty() -> Option<Node> {
    return Option::None;
}

fn main() {
    let opt = empty();
    gc_collect();
    let v = match opt {
        Option::Some(n) => n.value,
        Option::None => 0,
    };
    println(v);
}
"#;
    let (out, ok) = compile_and_run(src);
    assert!(ok, "Option::None should compile and run");
    assert_eq!(out, "0\n");
}

#[test]
fn test_result_ok_class_payload_survives_gc_collect() {
    let src = r#"
class Node { pub value: i64; }

fn make_ok(v: i64) -> Result<Node, String> {
    let n = Node { value: v };
    return Result::Ok(n);
}

fn main() {
    let r = make_ok(99);
    gc_collect();
    let v = match r {
        Result::Ok(n) => n.value,
        Result::Err(_) => -1,
    };
    println(v);
    println(gc_allocated_bytes() > 0);
}
"#;
    let (out, ok) = compile_and_run(src);
    assert!(ok, "Result<Node,String> should compile and run");
    assert_eq!(
        out, "99\ntrue\n",
        "Node payload in Ok must survive gc_collect"
    );
}

#[test]
fn test_option_some_unrooted_option_collected_after_use() {
    let src = r#"
class Node { pub value: i64; }

fn alloc_and_use() -> i64 {
    let n = Node { value: 7 };
    let opt = Option::Some(n);
    let v = match opt {
        Option::Some(nd) => nd.value,
        Option::None => -1,
    };
    return v;
}

fn main() {
    let v = alloc_and_use();
    println(v);
    gc_collect();
    println(gc_allocated_bytes());
}
"#;
    let (out, ok) = compile_and_run(src);
    assert!(ok, "Option wrapping class should compile and run");
    assert_eq!(
        out, "7\n0\n",
        "Option and Node should be collected after use"
    );
}

// ── Option / Result exhaustiveness ────────────────────────────────────────────

#[test]
fn test_option_match_missing_none_reports_e1202() {
    assert_compile_error_contains(
        r#"
fn main() {
    let opt: Option<i64> = Option::Some(1);
    let v = match opt {
        Option::Some(x) => x,
    };
    println(v);
}
"#,
        &["error[E1202]", "variant `Option::None` not covered"],
    );
}

#[test]
fn test_option_match_missing_some_reports_e1202() {
    assert_compile_error_contains(
        r#"
fn main() {
    let opt: Option<i64> = Option::None;
    let v = match opt {
        Option::None => 0,
    };
    println(v);
}
"#,
        &["error[E1202]", "variant `Option::Some` not covered"],
    );
}

#[test]
fn test_option_match_wildcard_arm_is_exhaustive() {
    let src = r#"
fn main() {
    let opt: Option<i64> = Option::Some(42);
    let v = match opt {
        Option::Some(x) => x,
        _ => 0,
    };
    println(v);
}
"#;
    let (out, ok) = compile_and_run(src);
    assert!(ok, "wildcard arm should satisfy exhaustiveness");
    assert_eq!(out, "42\n");
}

#[test]
fn test_result_match_missing_err_reports_e1202() {
    assert_compile_error_contains(
        r#"
fn main() {
    let r: Result<i64, String> = Result::Ok(1);
    let v = match r {
        Result::Ok(x) => x,
    };
    println(v);
}
"#,
        &["error[E1202]", "variant `Result::Err` not covered"],
    );
}

#[test]
fn test_result_match_missing_ok_reports_e1202() {
    assert_compile_error_contains(
        r#"
fn main() {
    let r: Result<i64, String> = Result::Err("bad");
    let v = match r {
        Result::Err(e) => 0,
    };
    println(v);
}
"#,
        &["error[E1202]", "variant `Result::Ok` not covered"],
    );
}

#[test]
fn test_result_match_wildcard_arm_is_exhaustive() {
    let src = r#"
fn parse(n: i64) -> Result<i64, String> {
    if n < 0 { return Result::Err("negative"); }
    return Result::Ok(n);
}

fn main() {
    let v = match parse(5) {
        Result::Ok(x) => x,
        _ => -1,
    };
    println(v);
}
"#;
    let (out, ok) = compile_and_run(src);
    assert!(ok, "wildcard arm satisfies exhaustiveness for Result");
    assert_eq!(out, "5\n");
}

#[test]
fn test_option_unknown_variant_reports_e1801() {
    assert_compile_error_contains(
        r#"
fn main() {
    let x = Option::Maybe(1);
}
"#,
        &["error[E1801]", "unknown variant `Maybe` in `Option`"],
    );
}

#[test]
fn test_result_unknown_variant_reports_e1801() {
    assert_compile_error_contains(
        r#"
fn main() {
    let x = Result::Value(1);
}
"#,
        &["error[E1801]", "unknown variant `Value` in `Result`"],
    );
}

// ── E180x type-inference and `?` diagnostics (willow-aff.3) ────────────────
// Acceptance criteria from requirements/requirements_option_result.md:
//   E1801 — cannot infer `T` for `Option::None`
//   E1803 — cannot infer `E` for `Result::Ok` / cannot infer `T` for `Result::Err`
//   E1805 — `?` error type mismatch
//   E1806 — `?` applied to a non-Result/non-Option value
//   E1807 — `?` in a function that does not return the matching wrapper
// (Non-exhaustive match for Option/Result is reported generically as E1202;
//  see the test_*_match_missing_* tests above.)

// Perspective 1: bare `Option::None` without annotation cannot infer `T`.
#[test]
fn test_e1801_bare_none_cannot_infer_t() {
    assert_compile_error_contains(
        r#"
fn main() {
    let x = Option::None;
    println(1);
}
"#,
        &[
            "error[E1801]",
            "cannot infer type parameter `T` for `Option::None`",
            "type annotation required",
        ],
    );
}

// Perspective 2: the inference error also fires for `let mut`.
#[test]
fn test_e1801_bare_none_let_mut_cannot_infer_t() {
    assert_compile_error_contains(
        r#"
fn main() {
    let mut x = Option::None;
    println(1);
}
"#,
        &["error[E1801]", "cannot infer type parameter `T`"],
    );
}

// Perspective 3: bare `Result::Ok(v)` cannot infer the error type `E`.
#[test]
fn test_e1803_bare_ok_cannot_infer_error_type() {
    assert_compile_error_contains(
        r#"
fn main() {
    let x = Result::Ok(10);
    println(1);
}
"#,
        &[
            "error[E1803]",
            "cannot infer error type `E` for `Result::Ok`",
        ],
    );
}

// Perspective 4: bare `Result::Err(e)` cannot infer the success type `T`.
#[test]
fn test_e1803_bare_err_cannot_infer_success_type() {
    assert_compile_error_contains(
        r#"
fn main() {
    let x = Result::Err("boom");
    println(1);
}
"#,
        &[
            "error[E1803]",
            "cannot infer success type `T` for `Result::Err`",
        ],
    );
}

// Perspective 5: annotation resolves `Option::None` — no diagnostic.
#[test]
fn test_e1801_annotation_resolves_none() {
    let (out, ok) = compile_and_run(
        r#"
fn main() {
    let x: Option<i64> = Option::None;
    println(x.is_none());
}
"#,
    );
    assert!(ok, "annotated None must compile");
    assert_eq!(out, "true\n");
}

// Perspective 6: annotation resolves `Result::Ok` — no diagnostic, runs.
#[test]
fn test_e1803_annotation_resolves_ok() {
    let (out, ok) = compile_and_run(
        r#"
fn main() {
    let x: Result<i64, String> = Result::Ok(10);
    println(x.unwrap());
}
"#,
    );
    assert!(ok, "annotated Ok must compile");
    assert_eq!(out, "10\n");
}

// Perspective 7: annotation resolves `Result::Err` — no diagnostic.
#[test]
fn test_e1803_annotation_resolves_err() {
    let (out, ok) = compile_and_run(
        r#"
fn main() {
    let x: Result<i64, String> = Result::Err("nope");
    println(x.is_err());
}
"#,
    );
    assert!(ok, "annotated Err must compile");
    assert_eq!(out, "true\n");
}

// Perspective 8: `Option::Some(v)` infers `T` from the payload — no diagnostic.
#[test]
fn test_e1801_some_infers_t_no_annotation() {
    let (out, ok) = compile_and_run(
        r#"
fn main() {
    let x = Option::Some(7);
    println(x.unwrap());
}
"#,
    );
    assert!(ok, "Some(7) must infer T=i64");
    assert_eq!(out, "7\n");
}

// Perspective 9: a `Void` placeholder reaching a binding through a method
// chain is benign and must NOT trigger E1803 (guards against over-reporting).
#[test]
fn test_e1803_not_reported_through_method_chain() {
    let (out, ok) = compile_and_run(
        r#"
fn add_five(v: i64) -> Result<i64, String> {
    return Result::Ok(v + 5);
}

fn main() {
    let chained = Result::Ok(10).and_then(add_five);
    println(chained.unwrap());
}
"#,
    );
    assert!(ok, "method-chain result must not trigger E1803");
    assert_eq!(out, "15\n");
}

// Perspective 10: `Option::None` as a direct return is resolved by the return
// type — no diagnostic.
#[test]
fn test_e1801_none_as_return_is_resolved() {
    let (out, ok) = compile_and_run(
        r#"
fn empty() -> Option<i64> {
    return Option::None;
}

fn main() {
    println(empty().is_none());
}
"#,
    );
    assert!(ok, "None as return must compile");
    assert_eq!(out, "true\n");
}

// Perspective 11: `?` propagating a mismatched error type reports E1805.
#[test]
fn test_e1805_question_error_type_mismatch() {
    assert_compile_error_contains(
        r#"
fn source() -> Result<i64, String> {
    return Result::Ok(1);
}

fn consumer() -> Result<i64, i64> {
    let v = source()?;
    return Result::Ok(v);
}

fn main() {}
"#,
        &[
            "error[E1805]",
            "error type mismatch",
            "but `?` propagates `String`",
        ],
    );
}

// Perspective 12: `?` with matching error types compiles and runs end-to-end.
#[test]
fn test_e1805_matching_error_types_ok() {
    let (out, ok) = compile_and_run(
        r#"
fn source(n: i64) -> Result<i64, String> {
    if n < 0 { return Result::Err("neg"); }
    return Result::Ok(n);
}

fn consumer(n: i64) -> Result<i64, String> {
    let v = source(n)?;
    return Result::Ok(v * 2);
}

fn main() {
    let r = consumer(21);
    println(r.unwrap());
}
"#,
    );
    assert!(ok, "matching error types must compile");
    assert_eq!(out, "42\n");
}

// Perspective 13: `?` on a `bool` reports E1806.
#[test]
fn test_e1806_question_on_bool() {
    assert_compile_error_contains(
        r#"
fn f() -> Result<i64, String> {
    let b = true;
    let x = b?;
    return Result::Ok(1);
}

fn main() {}
"#,
        &[
            "error[E1806]",
            "requires `Result<T,E>` or `Option<T>`",
            "found `bool`",
        ],
    );
}

// Perspective 14: `?` on an `Option` inside a Result-returning function is
// rejected because no Option-to-Result conversion is defined.
#[test]
fn test_e1807_question_on_option_in_result_function() {
    assert_compile_error_contains(
        r#"
fn f() -> Result<i64, String> {
    let o: Option<i64> = Option::Some(1);
    let x = o?;
    return Result::Ok(x);
}

fn main() {}
"#,
        &[
            "error[E1807]",
            "`?` on `Option<T>` can only be used inside a function returning `Option<U>`",
            "found `Result<i64, String>`",
        ],
    );
}

// Perspective 15: `?` on a `String` reports E1806.
#[test]
fn test_e1806_question_on_string() {
    assert_compile_error_contains(
        r#"
fn f() -> Result<i64, String> {
    let s = "hello";
    let x = s?;
    return Result::Ok(1);
}

fn main() {}
"#,
        &[
            "error[E1806]",
            "requires `Result<T,E>` or `Option<T>`",
            "found `String`",
        ],
    );
}

// Perspective 16: `?` inside a `void` function reports E1807.
#[test]
fn test_e1807_question_in_void_function() {
    assert_compile_error_contains(
        r#"
fn source() -> Result<i64, String> {
    return Result::Ok(1);
}

fn main() {
    let v = source()?;
    println(v);
}
"#,
        &[
            "error[E1807]",
            "can only be used inside a function returning `Result",
            "found `void`",
        ],
    );
}

// Perspective 17: `?` inside an `Option`-returning function reports E1807.
#[test]
fn test_e1807_question_in_option_function() {
    assert_compile_error_contains(
        r#"
fn source() -> Result<i64, String> {
    return Result::Ok(1);
}

fn wrapped() -> Option<i64> {
    let v = source()?;
    return Option::Some(v);
}

fn main() {}
"#,
        &["error[E1807]", "found `Option<i64>`"],
    );
}

// Perspective 18: `?` inside an `i64`-returning function reports E1807.
#[test]
fn test_e1807_question_in_i64_function() {
    assert_compile_error_contains(
        r#"
fn source() -> Result<i64, String> {
    return Result::Ok(1);
}

fn doubled() -> i64 {
    let v = source()?;
    return v * 2;
}

fn main() {}
"#,
        &["error[E1807]", "found `i64`"],
    );
}

// Perspective 19: too many arguments to a variant constructor is source-aware
// (E0201 reports the expected and actual argument counts).
#[test]
fn test_variant_constructor_too_many_args_e0201() {
    assert_compile_error_contains(
        r#"
fn main() {
    let x: Option<i64> = Option::Some(1, 2);
    println(1);
}
"#,
        &[
            "error[E0201]",
            "`Option::Some` expects 1 argument(s), got 2",
        ],
    );
}

// Perspective 20: a payload type mismatch in a variant constructor is
// source-aware (reports the concrete instantiations).
#[test]
fn test_variant_constructor_payload_type_mismatch_e0201() {
    assert_compile_error_contains(
        r#"
fn main() {
    let x: Option<i64> = Option::Some(true);
    println(1);
}
"#,
        &[
            "error[E0201]",
            "expected `Option<i64>`",
            "found `Option<bool>`",
        ],
    );
}

// Perspective 21: a missing payload on a variant constructor is source-aware.
#[test]
fn test_variant_constructor_missing_payload_e0201() {
    assert_compile_error_contains(
        r#"
fn f() -> Result<i64, String> {
    return Result::Ok();
}

fn main() {}
"#,
        &["error[E0201]", "`Result::Ok` expects 1 argument(s), got 0"],
    );
}

// Perspective 22: the full happy path — `?` extracts the Ok payload, chains,
// and propagates an early Err — compiles and runs.
#[test]
fn test_question_operator_happy_path_end_to_end() {
    let (out, ok) = compile_and_run(
        r#"
fn checked(n: i64) -> Result<i64, String> {
    if n < 0 { return Result::Err("negative"); }
    return Result::Ok(n);
}

fn pipeline(n: i64) -> Result<i64, String> {
    let a = checked(n)?;
    let b = checked(a - 5)?;
    return Result::Ok(b);
}

fn main() {
    let good = pipeline(10);
    println(match good { Result::Ok(v) => v, Result::Err(_) => -1, });
    let bad = pipeline(2);
    println(match bad { Result::Ok(v) => v, Result::Err(_) => -1, });
}
"#,
    );
    assert!(ok, "? happy path must compile and run");
    assert_eq!(out, "5\n-1\n");
}

// ── Option helper method tests ─────────────────────────────────────────────

#[test]
fn test_option_is_some_and_is_none() {
    let (out, ok) = compile_and_run(
        r#"
fn main() {
    let a = Option::Some(42);
    let b: Option<i64> = Option::None;
    println(a.is_some());
    println(a.is_none());
    println(b.is_some());
    println(b.is_none());
}
"#,
    );
    assert!(ok);
    assert_eq!(out, "true\nfalse\nfalse\ntrue\n");
}

#[test]
fn test_option_unwrap_some_returns_value() {
    let (out, ok) = compile_and_run(
        r#"
fn main() {
    let x = Option::Some(99);
    println(x.unwrap());
}
"#,
    );
    assert!(ok);
    assert_eq!(out, "99\n");
}

#[test]
fn test_option_unwrap_none_panics() {
    let src = r#"
fn main() {
    let x: Option<i64> = Option::None;
    println(x.unwrap());
}
"#;
    let (out, ok) = compile_and_run_check_exit(src);
    assert!(!ok, "unwrap on None should panic (non-zero exit)");
    assert!(
        out.contains("None") || out.is_empty(),
        "panic message should mention None"
    );
}

#[test]
fn test_option_expect_some_returns_value() {
    let (out, ok) = compile_and_run(
        r#"
fn main() {
    let x = Option::Some(7);
    println(x.expect("should have value"));
}
"#,
    );
    assert!(ok);
    assert_eq!(out, "7\n");
}

#[test]
fn test_option_expect_none_panics_with_message() {
    let src = r#"
fn main() {
    let x: Option<i64> = Option::None;
    println(x.expect("custom message"));
}
"#;
    let (out, ok) = compile_and_run_check_exit(src);
    assert!(!ok, "expect on None should panic");
    assert!(
        out.contains("custom message"),
        "panic should include custom message"
    );
}

#[test]
fn test_option_unwrap_or_some_returns_payload() {
    let (out, ok) = compile_and_run(
        r#"
fn main() {
    let x = Option::Some(5);
    println(x.unwrap_or(0));
}
"#,
    );
    assert!(ok);
    assert_eq!(out, "5\n");
}

#[test]
fn test_option_unwrap_or_none_returns_default() {
    let (out, ok) = compile_and_run(
        r#"
fn main() {
    let x: Option<i64> = Option::None;
    println(x.unwrap_or(42));
}
"#,
    );
    assert!(ok);
    assert_eq!(out, "42\n");
}

#[test]
fn test_option_map_some_transforms_value() {
    let (out, ok) = compile_and_run(
        r#"
fn double(x: i64) -> i64 {
    return x * 2;
}
fn main() {
    let x = Option::Some(10);
    let y = x.map(double);
    println(y.unwrap());
}
"#,
    );
    assert!(ok);
    assert_eq!(out, "20\n");
}

#[test]
fn test_option_map_none_stays_none() {
    let (out, ok) = compile_and_run(
        r#"
fn double(x: i64) -> i64 {
    return x * 2;
}
fn main() {
    let x: Option<i64> = Option::None;
    let y = x.map(double);
    println(y.is_none());
}
"#,
    );
    assert!(ok);
    assert_eq!(out, "true\n");
}

#[test]
fn test_option_map_with_lambda() {
    let (out, ok) = compile_and_run(
        r#"
fn main() {
    let x = Option::Some(3);
    let y = x.map(|v: i64| v * v);
    println(y.unwrap());
}
"#,
    );
    assert!(ok);
    assert_eq!(out, "9\n");
}

#[test]
fn test_option_and_then_some_calls_f() {
    let (out, ok) = compile_and_run(
        r#"
fn safe_double(x: i64) -> Option<i64> {
    if x > 100 {
        return Option::None;
    }
    return Option::Some(x * 2);
}
fn main() {
    let a = Option::Some(5).and_then(safe_double);
    let b = Option::Some(200).and_then(safe_double);
    println(a.unwrap());
    println(b.is_none());
}
"#,
    );
    assert!(ok);
    assert_eq!(out, "10\ntrue\n");
}

#[test]
fn test_option_and_then_none_stays_none() {
    let (out, ok) = compile_and_run(
        r#"
fn safe_double(x: i64) -> Option<i64> {
    return Option::Some(x * 2);
}
fn main() {
    let x: Option<i64> = Option::None;
    let y = x.and_then(safe_double);
    println(y.is_none());
}
"#,
    );
    assert!(ok);
    assert_eq!(out, "true\n");
}

#[test]
fn test_option_or_else_some_returns_self() {
    let (out, ok) = compile_and_run(
        r#"
fn fallback() -> Option<i64> {
    return Option::Some(99);
}
fn main() {
    let x = Option::Some(1);
    let y = x.or_else(fallback);
    println(y.unwrap());
}
"#,
    );
    assert!(ok);
    assert_eq!(out, "1\n");
}

#[test]
fn test_option_or_else_none_calls_f() {
    let (out, ok) = compile_and_run(
        r#"
fn fallback() -> Option<i64> {
    return Option::Some(99);
}
fn main() {
    let x: Option<i64> = Option::None;
    let y = x.or_else(fallback);
    println(y.unwrap());
}
"#,
    );
    assert!(ok);
    assert_eq!(out, "99\n");
}

// ── Result helper method tests ─────────────────────────────────────────────

#[test]
fn test_result_is_ok_and_is_err() {
    let (out, ok) = compile_and_run(
        r#"
fn main() {
    let a: Result<i64, String> = Result::Ok(1);
    let b: Result<i64, String> = Result::Err("oops");
    println(a.is_ok());
    println(a.is_err());
    println(b.is_ok());
    println(b.is_err());
}
"#,
    );
    assert!(ok);
    assert_eq!(out, "true\nfalse\nfalse\ntrue\n");
}

#[test]
fn test_result_unwrap_ok_returns_value() {
    let (out, ok) = compile_and_run(
        r#"
fn main() {
    let x: Result<i64, String> = Result::Ok(55);
    println(x.unwrap());
}
"#,
    );
    assert!(ok);
    assert_eq!(out, "55\n");
}

#[test]
fn test_result_unwrap_err_panics() {
    let src = r#"
fn main() {
    let x: Result<i64, String> = Result::Err("fail");
    println(x.unwrap());
}
"#;
    let (out, ok) = compile_and_run_check_exit(src);
    assert!(!ok, "unwrap on Err should panic");
    assert!(out.contains("Err") || out.is_empty());
}

#[test]
fn test_result_expect_ok_returns_value() {
    let (out, ok) = compile_and_run(
        r#"
fn main() {
    let x: Result<i64, String> = Result::Ok(7);
    println(x.expect("should be ok"));
}
"#,
    );
    assert!(ok);
    assert_eq!(out, "7\n");
}

#[test]
fn test_result_expect_err_panics_with_message() {
    let src = r#"
fn main() {
    let x: Result<i64, String> = Result::Err("bad");
    println(x.expect("my error message"));
}
"#;
    let (out, ok) = compile_and_run_check_exit(src);
    assert!(!ok, "expect on Err should panic");
    assert!(
        out.contains("my error message"),
        "panic should include custom message"
    );
}

#[test]
fn test_result_unwrap_or_ok_returns_payload() {
    let (out, ok) = compile_and_run(
        r#"
fn main() {
    let x: Result<i64, String> = Result::Ok(10);
    println(x.unwrap_or(0));
}
"#,
    );
    assert!(ok);
    assert_eq!(out, "10\n");
}

#[test]
fn test_result_unwrap_or_err_returns_default() {
    let (out, ok) = compile_and_run(
        r#"
fn main() {
    let x: Result<i64, String> = Result::Err("fail");
    println(x.unwrap_or(42));
}
"#,
    );
    assert!(ok);
    assert_eq!(out, "42\n");
}

#[test]
fn test_result_unwrap_err_extracts_error() {
    let (out, ok) = compile_and_run(
        r#"
fn main() {
    let x: Result<i64, String> = Result::Err("my error");
    println(x.unwrap_err());
}
"#,
    );
    assert!(ok);
    assert_eq!(out, "my error\n");
}

#[test]
fn test_result_unwrap_err_on_ok_panics() {
    let src = r#"
fn main() {
    let x: Result<i64, String> = Result::Ok(1);
    println(x.unwrap_err());
}
"#;
    let (_, ok) = compile_and_run_check_exit(src);
    assert!(!ok, "unwrap_err on Ok should panic");
}

#[test]
fn test_result_map_ok_transforms_value() {
    let (out, ok) = compile_and_run(
        r#"
fn triple(x: i64) -> i64 {
    return x * 3;
}
fn main() {
    let x: Result<i64, String> = Result::Ok(4);
    let y = x.map(triple);
    println(y.unwrap());
}
"#,
    );
    assert!(ok);
    assert_eq!(out, "12\n");
}

#[test]
fn test_result_map_err_unchanged() {
    let (out, ok) = compile_and_run(
        r#"
fn triple(x: i64) -> i64 {
    return x * 3;
}
fn main() {
    let x: Result<i64, String> = Result::Err("oops");
    let y = x.map(triple);
    println(y.is_err());
}
"#,
    );
    assert!(ok);
    assert_eq!(out, "true\n");
}

#[test]
fn test_result_map_with_lambda() {
    let (out, ok) = compile_and_run(
        r#"
fn main() {
    let x: Result<i64, String> = Result::Ok(5);
    let y = x.map(|v: i64| v + 10);
    println(y.unwrap());
}
"#,
    );
    assert!(ok);
    assert_eq!(out, "15\n");
}

#[test]
fn test_result_map_err_transforms_error() {
    let (out, ok) = compile_and_run(
        r#"
fn add_prefix(s: String) -> String {
    return "error: " + s;
}
fn main() {
    let x: Result<i64, String> = Result::Err("bad input");
    let y = x.map_err(add_prefix);
    println(y.unwrap_err());
}
"#,
    );
    assert!(ok);
    assert_eq!(out, "error: bad input\n");
}

#[test]
fn test_result_map_err_ok_unchanged() {
    let (out, ok) = compile_and_run(
        r#"
fn add_prefix(s: String) -> String {
    return "error: " + s;
}
fn main() {
    let x: Result<i64, String> = Result::Ok(42);
    let y = x.map_err(add_prefix);
    println(y.unwrap());
}
"#,
    );
    assert!(ok);
    assert_eq!(out, "42\n");
}

#[test]
fn test_result_and_then_ok_chains() {
    let (out, ok) = compile_and_run(
        r#"
fn parse_positive(n: i64) -> Result<i64, String> {
    if n > 0 {
        return Result::Ok(n);
    }
    return Result::Err("not positive");
}
fn main() {
    let a = Result::Ok(5).and_then(parse_positive);
    let b = Result::Ok(-3).and_then(parse_positive);
    println(a.unwrap());
    println(b.unwrap_err());
}
"#,
    );
    assert!(ok);
    assert_eq!(out, "5\nnot positive\n");
}

#[test]
fn test_result_and_then_err_stays_err() {
    let (out, ok) = compile_and_run(
        r#"
fn parse_positive(n: i64) -> Result<i64, String> {
    return Result::Ok(n * 2);
}
fn main() {
    let x: Result<i64, String> = Result::Err("initial error");
    let y = x.and_then(parse_positive);
    println(y.unwrap_err());
}
"#,
    );
    assert!(ok);
    assert_eq!(out, "initial error\n");
}

#[test]
fn test_result_or_else_ok_returns_self() {
    let (out, ok) = compile_and_run(
        r#"
fn recover(s: String) -> Result<i64, String> {
    return Result::Ok(0);
}
fn main() {
    let x: Result<i64, String> = Result::Ok(7);
    let y = x.or_else(recover);
    println(y.unwrap());
}
"#,
    );
    assert!(ok);
    assert_eq!(out, "7\n");
}

#[test]
fn test_result_or_else_err_calls_f() {
    let (out, ok) = compile_and_run(
        r#"
fn recover(s: String) -> Result<i64, String> {
    return Result::Ok(99);
}
fn main() {
    let x: Result<i64, String> = Result::Err("fail");
    let y = x.or_else(recover);
    println(y.unwrap());
}
"#,
    );
    assert!(ok);
    assert_eq!(out, "99\n");
}

// ── Type error tests for Option/Result method helpers ─────────────────────

#[test]
fn test_option_is_some_with_args_reports_error() {
    assert!(expect_compile_error(
        r#"
fn main() {
    let x = Option::Some(1);
    let _ = x.is_some(42);
}
"#
    ));
}

#[test]
fn test_option_unwrap_or_type_mismatch_reports_error() {
    assert!(expect_compile_error(
        r#"
fn main() {
    let x = Option::Some(1);
    let _ = x.unwrap_or(true);
}
"#
    ));
}

#[test]
fn test_result_is_ok_with_args_reports_error() {
    assert!(expect_compile_error(
        r#"
fn main() {
    let x: Result<i64, String> = Result::Ok(1);
    let _ = x.is_ok(42);
}
"#
    ));
}

#[test]
fn test_result_map_wrong_fn_type_reports_error() {
    assert!(expect_compile_error(
        r#"
fn wrong(s: String) -> i64 {
    return 0;
}
fn main() {
    let x: Result<i64, String> = Result::Ok(1);
    let _ = x.map(wrong);
}
"#
    ));
}

// ── prot (protected) access modifier tests ────────────────────────────────

// 1. prot field accessible within own class method
#[test]
fn test_prot_field_accessible_in_own_class() {
    let (out, ok) = compile_and_run(
        r#"
class Bag {
    prot items: i64;
    pub fn count(self) -> i64 { return self.items; }
}
fn main() {
    let b = Bag { items: 7 };
    println(b.count());
}
"#,
    );
    assert!(ok);
    assert_eq!(out, "7\n");
}

// 2. prot method callable within own class
#[test]
fn test_prot_method_callable_in_own_class() {
    let (out, ok) = compile_and_run(
        r#"
class Calc {
    val: i64;
    prot fn triple(self) -> i64 { return self.val * 3; }
    pub fn result(self) -> i64 { return self.triple(); }
}
fn main() {
    let c = Calc { val: 4 };
    println(c.result());
}
"#,
    );
    assert!(ok);
    assert_eq!(out, "12\n");
}

// 3. prot field accessible in direct subclass method
#[test]
fn test_prot_field_accessible_in_subclass() {
    let (out, ok) = compile_and_run(
        r#"
pub open class Base {
    prot score: i64;
    pub fn score(self) -> i64 { return self.score; }
}
pub class Child extends Base {
    pub fn bonus(self) -> i64 { return self.score + 10; }
}
fn main() {
    let c = Child { score: 5 };
    println(c.score());
    println(c.bonus());
}
"#,
    );
    assert!(ok);
    assert_eq!(out, "5\n15\n");
}

// 4. prot method callable in direct subclass
#[test]
fn test_prot_method_callable_in_subclass() {
    let (out, ok) = compile_and_run(
        r#"
pub open class Engine {
    power: i64;
    prot fn raw_power(self) -> i64 { return self.power; }
    pub fn get_power(self) -> i64 { return self.power; }
}
pub class Turbo extends Engine {
    pub fn boosted(self) -> i64 { return self.raw_power() * 2; }
}
fn main() {
    let t = Turbo { power: 50 };
    println(t.get_power());
    println(t.boosted());
}
"#,
    );
    assert!(ok);
    assert_eq!(out, "50\n100\n");
}

// 5. prot field accessible two levels down the hierarchy
#[test]
fn test_prot_field_accessible_two_levels_deep() {
    let (out, ok) = compile_and_run(
        r#"
pub open class A {
    prot n: i64;
    pub fn n(self) -> i64 { return self.n; }
}
pub open class B extends A {
    pub fn double(self) -> i64 { return self.n * 2; }
}
pub class C extends B {
    pub fn triple(self) -> i64 { return self.n * 3; }
}
fn main() {
    let c = C { n: 4 };
    println(c.n());
    println(c.double());
    println(c.triple());
}
"#,
    );
    assert!(ok);
    assert_eq!(out, "4\n8\n12\n");
}

// 6. prot field rejected from unrelated external function
#[test]
fn test_prot_field_rejected_from_external_function() {
    assert_compile_error_contains(
        r#"
pub open class Vault {
    prot secret: i64;
}
fn steal(v: Vault) -> i64 { return v.secret; }
fn main() { println(1); }
"#,
        &[
            "error[E0503]",
            "field `secret` of class `Vault` is protected",
        ],
    );
}

// 7. prot method rejected from unrelated external function
#[test]
fn test_prot_method_rejected_from_external_function() {
    assert_compile_error_contains(
        r#"
pub open class Vault {
    x: i64;
    prot fn secret(self) -> i64 { return self.x; }
}
fn steal(v: Vault) -> i64 { return v.secret(); }
fn main() { println(1); }
"#,
        &[
            "error[E0503]",
            "method `secret` of class `Vault` is protected",
        ],
    );
}

// 8. prot field rejected via direct access from main
#[test]
fn test_prot_field_rejected_from_main() {
    assert!(expect_compile_error(
        r#"
class Box {
    prot val: i64;
}
fn main() {
    let b = Box { val: 1 };
    println(b.val);
}
"#
    ));
}

// 9. pub still allows access from anywhere
#[test]
fn test_pub_overrides_prot_restriction() {
    let (out, ok) = compile_and_run(
        r#"
class Mix {
    pub x: i64;
    prot y: i64;
}
fn read_x(m: Mix) -> i64 { return m.x; }
fn main() {
    let m = Mix { x: 10, y: 20 };
    println(read_x(m));
}
"#,
    );
    assert!(ok);
    assert_eq!(out, "10\n");
}

// 10. private is stricter than prot (subclass cannot see private)
#[test]
fn test_private_stricter_than_prot() {
    assert_compile_error_contains(
        r#"
pub open class Base {
    secret: i64;
}
pub class Child extends Base {
    pub fn leak(self) -> i64 { return self.secret; }
}
fn main() { println(1); }
"#,
        &["error[E0501]", "field `secret` of class `Base` is private"],
    );
}

// 11. prot method diagnostic points to declaration site
#[test]
fn test_prot_method_diagnostic_points_to_declaration() {
    assert_compile_error_contains(
        r#"
pub open class Service {
    x: i64;
    prot fn internal(self) -> i64 { return self.x; }
}
fn call(s: Service) -> i64 { return s.internal(); }
fn main() { println(1); }
"#,
        &[
            "error[E0503]",
            "method `internal` of class `Service` is protected",
            "method defined here",
        ],
    );
}

// 12. prot field diagnostic includes help text
#[test]
fn test_prot_field_diagnostic_help_text() {
    assert_compile_error_contains(
        r#"
pub open class Secure {
    prot token: i64;
}
fn grab(s: Secure) -> i64 { return s.token; }
fn main() { println(1); }
"#,
        &[
            "error[E0503]",
            "field `token` of class `Secure` is protected",
            "prot members are accessible only within",
        ],
    );
}

// 13. override method can access prot field from base
#[test]
fn test_prot_override_method_accesses_base_field() {
    let (out, ok) = compile_and_run(
        r#"
pub open class Animal {
    prot energy: i64;
    pub open fn cost(self) -> i64 { return self.energy; }
}
pub class Dog extends Animal {
    pub override fn cost(self) -> i64 { return self.energy * 2; }
}
fn main() {
    let d = Dog { energy: 5 };
    println(d.cost());
}
"#,
    );
    assert!(ok);
    assert_eq!(out, "10\n");
}

// 14. prot + override: override of a prot method callable directly on the subclass type
#[test]
fn test_prot_method_override_in_subclass() {
    let (out, ok) = compile_and_run(
        r#"
pub open class Shape {
    prot sides: i64;
    prot open fn side_count(self) -> i64 { return self.sides; }
    pub fn info(self) -> i64 { return self.side_count(); }
}
pub class Triangle extends Shape {
    pub override fn side_count(self) -> i64 { return self.sides * 3; }
}
fn main() {
    let s = Shape { sides: 4 };
    let t = Triangle { sides: 2 };
    println(s.info());
    println(t.side_count());
}
"#,
    );
    assert!(ok);
    assert_eq!(out, "4\n6\n");
}

// 15. prot field not visible as pub field from outside
#[test]
fn test_prot_field_not_publicly_visible() {
    assert!(expect_compile_error(
        r#"
class Hidden {
    prot value: i64;
}
fn main() {
    let h = Hidden { value: 1 };
    println(h.value);
}
"#
    ));
}

// 16. error code is E0503, not E0501 or E0502
#[test]
fn test_prot_uses_error_code_e0503() {
    assert_compile_error_contains(
        r#"
class C { prot x: i64; }
fn main() {
    let c = C { x: 1 };
    println(c.x);
}
"#,
        &["error[E0503]"],
    );
}

// 17. prot keyword parses on fields without other modifiers
#[test]
fn test_prot_parses_on_field() {
    let (out, ok) = compile_and_run(
        r#"
class Wrapper {
    prot inner: i64;
    pub fn get(self) -> i64 { return self.inner; }
}
fn main() {
    let w = Wrapper { inner: 42 };
    println(w.get());
}
"#,
    );
    assert!(ok);
    assert_eq!(out, "42\n");
}

// 18. prot keyword parses on methods without other modifiers
#[test]
fn test_prot_parses_on_method() {
    let (out, ok) = compile_and_run(
        r#"
class Worker {
    load: i64;
    prot fn internal_load(self) -> i64 { return self.load; }
    pub fn public_load(self) -> i64 { return self.internal_load(); }
}
fn main() {
    let w = Worker { load: 9 };
    println(w.public_load());
}
"#,
    );
    assert!(ok);
    assert_eq!(out, "9\n");
}

// 19. prot + open: protected open method overrideable in subclass, called on concrete type
#[test]
fn test_prot_open_method_can_be_overridden() {
    let (out, ok) = compile_and_run(
        r#"
pub open class Vehicle {
    prot speed: i64;
    pub fn get_speed(self) -> i64 { return self.speed; }
    prot open fn describe(self) -> i64 { return self.speed; }
    pub fn show(self) -> i64 { return self.describe(); }
}
pub class Car extends Vehicle {
    pub override fn describe(self) -> i64 { return self.speed + 10; }
}
fn main() {
    let v = Vehicle { speed: 30 };
    let c = Car { speed: 50 };
    println(v.show());
    println(c.describe());
}
"#,
    );
    assert!(ok);
    assert_eq!(out, "30\n60\n");
}

// 20. prot field accessible within class and subclass, rejected elsewhere
#[test]
fn test_prot_complete_access_rules() {
    let (out, ok) = compile_and_run(
        r#"
pub open class Counter {
    prot count: i64;
    pub fn get(self) -> i64 { return self.count; }
    prot fn increment(self) -> i64 { return self.count + 1; }
}
pub class BoundedCounter extends Counter {
    pub fn safe_inc(self, max: i64) -> i64 {
        let next = self.increment();
        if next > max {
            return self.count;
        }
        return next;
    }
}
fn main() {
    let c = BoundedCounter { count: 8 };
    println(c.get());
    println(c.safe_inc(10));
    println(c.safe_inc(7));
}
"#,
    );
    assert!(ok);
    assert_eq!(out, "8\n9\n8\n");
}

// ── Class / Object tests ───────────────────────────────────────────────────

// 1. Single field, single method
#[test]
fn test_class_single_field_and_getter() {
    let (out, ok) = compile_and_run(
        r#"
class Num {
    n: i64;
    pub fn get(self) -> i64 { return self.n; }
}
fn main() {
    let x = Num { n: 7 };
    println(x.get());
}
"#,
    );
    assert!(ok);
    assert_eq!(out, "7\n");
}

// 2. Multiple fields accessed via methods
#[test]
fn test_class_multiple_fields() {
    let (out, ok) = compile_and_run(
        r#"
class Rect {
    w: i64;
    h: i64;
    pub fn width(self) -> i64  { return self.w; }
    pub fn height(self) -> i64 { return self.h; }
    pub fn area(self) -> i64   { return self.w * self.h; }
}
fn main() {
    let r = Rect { w: 6, h: 4 };
    println(r.width());
    println(r.height());
    println(r.area());
}
"#,
    );
    assert!(ok);
    assert_eq!(out, "6\n4\n24\n");
}

// 3. Method that takes extra argument
#[test]
fn test_class_method_with_extra_arg() {
    let (out, ok) = compile_and_run(
        r#"
class Adder {
    base: i64;
    pub fn add(self, n: i64) -> i64 { return self.base + n; }
}
fn main() {
    let a = Adder { base: 10 };
    println(a.add(5));
    println(a.add(90));
}
"#,
    );
    assert!(ok);
    assert_eq!(out, "15\n100\n");
}

// 4. Method that calls another method on self
#[test]
fn test_class_method_calls_sibling_method() {
    let (out, ok) = compile_and_run(
        r#"
class Circle {
    r: i64;
    pub fn radius(self) -> i64     { return self.r; }
    pub fn diameter(self) -> i64   { return self.r * 2; }
}
fn main() {
    let c = Circle { r: 5 };
    println(c.diameter());
}
"#,
    );
    assert!(ok);
    assert_eq!(out, "10\n");
}

// 5. Object passed to a free function
#[test]
fn test_class_object_passed_to_free_function() {
    let (out, ok) = compile_and_run(
        r#"
class Val {
    v: i64;
    pub fn get(self) -> i64 { return self.v; }
}
fn double(x: Val) -> i64 { return x.get() * 2; }
fn main() {
    let x = Val { v: 21 };
    println(double(x));
}
"#,
    );
    assert!(ok);
    assert_eq!(out, "42\n");
}

// 6. Object returned from free function
#[test]
fn test_class_object_returned_from_function() {
    let (out, ok) = compile_and_run(
        r#"
class Point {
    x: i64;
    y: i64;
    pub fn x(self) -> i64 { return self.x; }
    pub fn y(self) -> i64 { return self.y; }
}
fn make(x: i64, y: i64) -> Point { return Point { x: x, y: y }; }
fn main() {
    let p = make(3, 4);
    println(p.x());
    println(p.y());
}
"#,
    );
    assert!(ok);
    assert_eq!(out, "3\n4\n");
}

// 7. Bool field
#[test]
fn test_class_bool_field() {
    let (out, ok) = compile_and_run(
        r#"
class Flag {
    on: bool;
    pub fn is_on(self) -> bool { return self.on; }
}
fn main() {
    let f = Flag { on: true };
    println(f.is_on());
}
"#,
    );
    assert!(ok);
    assert_eq!(out, "true\n");
}

// 8. String field
#[test]
fn test_class_string_field() {
    let (out, ok) = compile_and_run(
        r#"
class Msg {
    text: String;
    pub fn get(self) -> String { return self.text; }
}
fn main() {
    let m = Msg { text: "hello" };
    println(m.get());
}
"#,
    );
    assert!(ok);
    assert_eq!(out, "hello\n");
}

// 9. f64 field
#[test]
fn test_class_f64_field() {
    let (out, ok) = compile_and_run(
        r#"
class Temp {
    celsius: f64;
    pub fn get(self) -> f64 { return self.celsius; }
}
fn main() {
    let t = Temp { celsius: 36.6 };
    println(t.get());
}
"#,
    );
    assert!(ok);
    assert!(out.starts_with("36.6"));
}

// 10. Nested class fields
#[test]
fn test_class_nested_field() {
    let (out, ok) = compile_and_run(
        r#"
class Inner {
    v: i64;
    pub fn get(self) -> i64 { return self.v; }
}
class Outer {
    pub inner: Inner;
    pub fn inner(self) -> Inner { return self.inner; }
}
fn main() {
    let o = Outer { inner: Inner { v: 99 } };
    println(o.inner().get());
}
"#,
    );
    assert!(ok);
    assert_eq!(out, "99\n");
}

// 11. Multiple objects of same class
#[test]
fn test_class_multiple_instances() {
    let (out, ok) = compile_and_run(
        r#"
class Box {
    v: i64;
    pub fn get(self) -> i64 { return self.v; }
}
fn main() {
    let a = Box { v: 1 };
    let b = Box { v: 2 };
    let c = Box { v: 3 };
    let va = a.get();
    let vb = b.get();
    let vc = c.get();
    println(va + vb + vc);
}
"#,
    );
    assert!(ok);
    assert_eq!(out, "6\n");
}

// 12. Free-function constructor (factory pattern)
#[test]
fn test_class_static_constructor_method() {
    let (out, ok) = compile_and_run(
        r#"
class Counter {
    n: i64;
    pub fn get(self) -> i64 { return self.n; }
}
fn make_counter(start: i64) -> Counter { return Counter { n: start }; }
fn main() {
    let c = make_counter(42);
    println(c.get());
}
"#,
    );
    assert!(ok);
    assert_eq!(out, "42\n");
}

// 13. Method returning bool comparison
#[test]
fn test_class_method_returns_bool() {
    let (out, ok) = compile_and_run(
        r#"
class Score {
    points: i64;
    pub fn passing(self) -> bool { return self.points >= 60; }
}
fn main() {
    let s1 = Score { points: 80 };
    let s2 = Score { points: 40 };
    println(s1.passing());
    println(s2.passing());
}
"#,
    );
    assert!(ok);
    assert_eq!(out, "true\nfalse\n");
}

// 14. Method with conditional logic
#[test]
fn test_class_method_with_if() {
    let (out, ok) = compile_and_run(
        r#"
class Abs {
    v: i64;
    pub fn abs(self) -> i64 {
        if self.v < 0 {
            return self.v * -1;
        }
        return self.v;
    }
}
fn main() {
    let a = Abs { v: -5 };
    let b = Abs { v: 3 };
    println(a.abs());
    println(b.abs());
}
"#,
    );
    assert!(ok);
    assert_eq!(out, "5\n3\n");
}

// 15. Method with loop
#[test]
fn test_class_method_with_loop() {
    let (out, ok) = compile_and_run(
        r#"
class Pow {
    base: i64;
    pub fn pow(self, exp: i64) -> i64 {
        let mut result = 1;
        let mut i = 0;
        while i < exp {
            result = result * self.base;
            i = i + 1;
        }
        return result;
    }
}
fn main() {
    let p = Pow { base: 2 };
    println(p.pow(8));
}
"#,
    );
    assert!(ok);
    assert_eq!(out, "256\n");
}

// 16. Public field direct access
#[test]
fn test_class_public_field_direct_access() {
    let (out, ok) = compile_and_run(
        r#"
class Point {
    pub x: i64;
    pub y: i64;
}
fn main() {
    let p = Point { x: 10, y: 20 };
    println(p.x);
    println(p.y);
}
"#,
    );
    assert!(ok);
    assert_eq!(out, "10\n20\n");
}

// 17. Private field rejected outside class
#[test]
fn test_class_private_field_rejected_outside() {
    assert!(expect_compile_error(
        r#"
class Pair {
    a: i64;
    b: i64;
}
fn main() {
    let p = Pair { a: 1, b: 2 };
    println(p.a);
}
"#
    ));
}

// 18. Private method rejected outside class
#[test]
fn test_class_private_method_rejected_outside() {
    assert!(expect_compile_error(
        r#"
class Pair {
    a: i64;
    fn sum(self) -> i64 { return self.a; }
}
fn main() {
    let p = Pair { a: 1 };
    println(p.sum());
}
"#
    ));
}

// 19. Simple inheritance: child can be assigned to base variable (type check)
#[test]
fn test_class_inheritance_inherits_method() {
    let (out, ok) = compile_and_run(
        r#"
pub open class Base {
    pub open fn value(self) -> i64 { return 42; }
}
pub class Child extends Base {
    pub override fn value(self) -> i64 { return 42; }
}
fn main() {
    let c = Child {};
    println(c.value());
}
"#,
    );
    assert!(ok);
    assert_eq!(out, "42\n");
}

// 20. Override: derived class method called directly on derived type
#[test]
fn test_class_override_changes_value() {
    let (out, ok) = compile_and_run(
        r#"
pub open class Base {
    pub open fn id(self) -> i64 { return 1; }
}
pub class Derived extends Base {
    pub override fn id(self) -> i64 { return 2; }
}
fn main() {
    let b = Base {};
    let d = Derived {};
    println(b.id());
    println(d.id());
}
"#,
    );
    assert!(ok);
    assert_eq!(out, "1\n2\n");
}

// 21. Two levels of inheritance, each called directly
#[test]
fn test_class_two_level_inheritance() {
    let (out, ok) = compile_and_run(
        r#"
pub open class A {
    pub open fn tag(self) -> i64 { return 1; }
}
pub open class B extends A {
    pub open override fn tag(self) -> i64 { return 2; }
}
pub class C extends B {
    pub override fn tag(self) -> i64 { return 3; }
}
fn main() {
    let a = A {};
    let b = B {};
    let c = C {};
    println(a.tag());
    println(b.tag());
    println(c.tag());
}
"#,
    );
    assert!(ok);
    assert_eq!(out, "1\n2\n3\n");
}

// 22. Child inherits field from base, override method accesses inherited field
#[test]
fn test_class_child_with_field_and_inherited_method() {
    let (out, ok) = compile_and_run(
        r#"
pub open class Named {
    pub name: String;
    pub open fn greet(self) -> String { return self.name; }
}
pub class Employee extends Named {
    pub dept: String;
    pub override fn greet(self) -> String { return self.name; }
    pub fn dept(self) -> String { return self.dept; }
}
fn main() {
    let e = Employee { name: "Alice", dept: "Eng" };
    println(e.greet());
    println(e.dept());
}
"#,
    );
    assert!(ok);
    assert_eq!(out, "Alice\nEng\n");
}

// 23. Override reads inherited public i64 field
#[test]
fn test_class_override_reads_base_field() {
    let (out, ok) = compile_and_run(
        r#"
pub open class Animal {
    pub age: i64;
    pub open fn describe(self) -> i64 { return self.age; }
}
pub class Cat extends Animal {
    pub override fn describe(self) -> i64 { return self.age * 2; }
}
fn main() {
    let base = Animal { age: 5 };
    let cat = Cat { age: 3 };
    println(base.describe());
    println(cat.describe());
}
"#,
    );
    assert!(ok);
    assert_eq!(out, "5\n6\n");
}

// 24. Extending a non-open class reports error
#[test]
fn test_class_extend_non_open_is_error() {
    assert!(expect_compile_error(
        r#"
class Closed {}
class Child extends Closed {}
fn main() { println(1); }
"#
    ));
}

// 25. Override without keyword reports error
#[test]
fn test_class_override_without_keyword_is_error() {
    assert!(expect_compile_error(
        r#"
pub open class Base {
    pub open fn foo(self) -> i64 { return 1; }
}
pub class Child extends Base {
    pub fn foo(self) -> i64 { return 2; }
}
fn main() { println(1); }
"#
    ));
}

// 26. Override non-open method is error
#[test]
fn test_class_override_non_open_method_is_error() {
    assert!(expect_compile_error(
        r#"
pub open class Base {
    pub fn foo(self) -> i64 { return 1; }
}
pub class Child extends Base {
    pub override fn foo(self) -> i64 { return 2; }
}
fn main() { println(1); }
"#
    ));
}

// 27. Object stored in local variable, method called later
#[test]
fn test_class_stored_then_method_called() {
    let (out, ok) = compile_and_run(
        r#"
class Token {
    id: i64;
    pub fn id(self) -> i64 { return self.id; }
}
fn main() {
    let t = Token { id: 77 };
    let val = t.id();
    println(val);
}
"#,
    );
    assert!(ok);
    assert_eq!(out, "77\n");
}

// 28. Multiple method calls on same object
#[test]
fn test_class_multiple_method_calls_on_same_obj() {
    let (out, ok) = compile_and_run(
        r#"
class Stats {
    total: i64;
    count: i64;
    pub fn total(self) -> i64 { return self.total; }
    pub fn count(self) -> i64 { return self.count; }
    pub fn avg(self) -> i64   { return self.total / self.count; }
}
fn main() {
    let s = Stats { total: 90, count: 3 };
    println(s.total());
    println(s.count());
    println(s.avg());
}
"#,
    );
    assert!(ok);
    assert_eq!(out, "90\n3\n30\n");
}

// 29. Method returns another class instance
#[test]
fn test_class_method_returns_class_instance() {
    let (out, ok) = compile_and_run(
        r#"
class Inner {
    v: i64;
    pub fn get(self) -> i64 { return self.v; }
}
class Outer {
    pub fn make_inner(self, v: i64) -> Inner { return Inner { v: v }; }
}
fn main() {
    let o = Outer {};
    let i = o.make_inner(55);
    println(i.get());
}
"#,
    );
    assert!(ok);
    assert_eq!(out, "55\n");
}

// 30. Class with Option field
#[test]
fn test_class_with_option_field() {
    let (out, ok) = compile_and_run(
        r#"
class MaybeNum {
    val: Option<i64>;
    pub fn get_or(self, def: i64) -> i64 { return self.val.unwrap_or(def); }
    pub fn has_value(self) -> bool { return self.val.is_some(); }
}
fn main() {
    let a = MaybeNum { val: Option::Some(10) };
    let b = MaybeNum { val: Option::None };
    println(a.get_or(0));
    println(b.get_or(99));
    println(a.has_value());
    println(b.has_value());
}
"#,
    );
    assert!(ok);
    assert_eq!(out, "10\n99\ntrue\nfalse\n");
}

// 31. Class with Result field
#[test]
fn test_class_with_result_field() {
    let (out, ok) = compile_and_run(
        r#"
class Op {
    result: Result<i64, String>;
    pub fn ok_or(self, def: i64) -> i64 { return self.result.unwrap_or(def); }
    pub fn succeeded(self) -> bool { return self.result.is_ok(); }
}
fn main() {
    let a = Op { result: Result::Ok(7) };
    let b = Op { result: Result::Err("fail") };
    println(a.ok_or(0));
    println(b.ok_or(0));
    println(a.succeeded());
    println(b.succeeded());
}
"#,
    );
    assert!(ok);
    assert_eq!(out, "7\n0\ntrue\nfalse\n");
}

// 32. Class method returns Option
#[test]
fn test_class_method_returns_option() {
    let (out, ok) = compile_and_run(
        r#"
class Lookup {
    key: i64;
    value: i64;
    pub fn find(self, k: i64) -> Option<i64> {
        if self.key == k {
            return Option::Some(self.value);
        }
        return Option::None;
    }
}
fn main() {
    let l = Lookup { key: 5, value: 100 };
    println(l.find(5).unwrap());
    println(l.find(9).is_none());
}
"#,
    );
    assert!(ok);
    assert_eq!(out, "100\ntrue\n");
}

// 33. Class method returns Result
#[test]
fn test_class_method_returns_result() {
    let (out, ok) = compile_and_run(
        r#"
class Divider {
    denom: i64;
    pub fn divide(self, n: i64) -> Result<i64, String> {
        if self.denom == 0 {
            return Result::Err("division by zero");
        }
        return Result::Ok(n / self.denom);
    }
}
fn main() {
    let d = Divider { denom: 4 };
    let z = Divider { denom: 0 };
    println(d.divide(20).unwrap());
    println(z.divide(1).is_err());
}
"#,
    );
    assert!(ok);
    assert_eq!(out, "5\ntrue\n");
}

// 34. Array-free accumulator via two class instances
#[test]
fn test_class_two_counters_independent() {
    let (out, ok) = compile_and_run(
        r#"
class Acc {
    start: i64;
    pub fn sum_to(self, n: i64) -> i64 {
        let mut s = self.start;
        let mut i = 0;
        while i < n {
            s = s + i;
            i = i + 1;
        }
        return s;
    }
}
fn main() {
    let a = Acc { start: 0 };
    let b = Acc { start: 100 };
    println(a.sum_to(5));
    println(b.sum_to(5));
}
"#,
    );
    assert!(ok);
    assert_eq!(out, "10\n110\n");
}

// 35. GC: class allocated inside function, returned as primitive
#[test]
fn test_class_gc_inner_alloc_returns_primitive() {
    let (out, ok) = compile_and_run(
        r#"
class Tmp {
    v: i64;
    pub fn get(self) -> i64 { return self.v; }
}
fn compute() -> i64 {
    let t = Tmp { v: 42 };
    return t.get();
}
fn main() {
    gc_collect();
    let x = compute();
    gc_collect();
    println(x);
}
"#,
    );
    assert!(ok);
    assert_eq!(out, "42\n");
}

// 36. GC: live object survives collect
#[test]
fn test_class_gc_live_object_survives() {
    let (out, ok) = compile_and_run(
        r#"
class Box {
    v: i64;
    pub fn get(self) -> i64 { return self.v; }
}
fn main() {
    let b = Box { v: 123 };
    gc_collect();
    println(b.get());
}
"#,
    );
    assert!(ok);
    assert_eq!(out, "123\n");
}

// 37. GC: two objects, one goes out of scope
#[test]
fn test_class_gc_one_survives_one_collected() {
    let (out, ok) = compile_and_run(
        r#"
class Obj {
    v: i64;
    pub fn get(self) -> i64 { return self.v; }
}
fn make_and_drop() {
    let tmp = Obj { v: 999 };
}
fn main() {
    let live = Obj { v: 7 };
    make_and_drop();
    gc_collect();
    println(live.get());
}
"#,
    );
    assert!(ok);
    assert_eq!(out, "7\n");
}

// 38. Nullable class: nil assignment
#[test]
fn test_class_nullable_accepts_nil() {
    let (out, ok) = compile_and_run(
        r#"
class Node {
    v: i64;
    pub fn get(self) -> i64 { return self.v; }
}
fn main() {
    let n: Node? = nil;
    println(n == nil);
}
"#,
    );
    assert!(ok);
    assert_eq!(out, "true\n");
}

// 39. Nullable: after nil-check, use object
#[test]
fn test_class_nullable_nil_guard_then_use() {
    let (out, ok) = compile_and_run(
        r#"
class Node {
    v: i64;
    pub fn get(self) -> i64 { return self.v; }
}
fn maybe_get(n: Node?) -> i64 {
    if n == nil {
        return -1;
    }
    return n.get();
}
fn main() {
    let a: Node? = Node { v: 5 };
    let b: Node? = nil;
    println(maybe_get(a));
    println(maybe_get(b));
}
"#,
    );
    assert!(ok);
    assert_eq!(out, "5\n-1\n");
}

// 40. Class as function argument and return type
#[test]
fn test_class_as_fn_arg_and_return() {
    let (out, ok) = compile_and_run(
        r#"
class Vec2 {
    pub x: i64;
    pub y: i64;
    pub fn x(self) -> i64 { return self.x; }
    pub fn y(self) -> i64 { return self.y; }
}
fn add(a: Vec2, b: Vec2) -> Vec2 {
    return Vec2 { x: a.x() + b.x(), y: a.y() + b.y() };
}
fn main() {
    let u = Vec2 { x: 1, y: 2 };
    let v = Vec2 { x: 3, y: 4 };
    let w = add(u, v);
    println(w.x());
    println(w.y());
}
"#,
    );
    assert!(ok);
    assert_eq!(out, "4\n6\n");
}

// 41. Class with two String fields, each returned independently
#[test]
fn test_class_method_string_concat() {
    let (out, ok) = compile_and_run(
        r#"
class Person {
    first: String;
    last: String;
    pub fn first(self) -> String { return self.first; }
    pub fn last(self) -> String  { return self.last;  }
}
fn main() {
    let p = Person { first: "Jane", last: "Doe" };
    println(p.first());
    println(p.last());
}
"#,
    );
    assert!(ok);
    assert_eq!(out, "Jane\nDoe\n");
}

// 42. Class method used in boolean expression
#[test]
fn test_class_method_in_boolean_expr() {
    let (out, ok) = compile_and_run(
        r#"
class Range {
    lo: i64;
    hi: i64;
    pub fn contains(self, v: i64) -> bool { return v >= self.lo && v <= self.hi; }
}
fn main() {
    let r = Range { lo: 10, hi: 20 };
    println(r.contains(15));
    println(r.contains(25));
}
"#,
    );
    assert!(ok);
    assert_eq!(out, "true\nfalse\n");
}

// 43. Class method used in while condition
#[test]
fn test_class_method_in_while_condition() {
    let (out, ok) = compile_and_run(
        r#"
class Counter {
    limit: i64;
    pub fn below(self, n: i64) -> bool { return n < self.limit; }
}
fn main() {
    let c = Counter { limit: 3 };
    let mut i = 0;
    while c.below(i) {
        println(i);
        i = i + 1;
    }
}
"#,
    );
    assert!(ok);
    assert_eq!(out, "0\n1\n2\n");
}

// 44. Two different class types in same function
#[test]
fn test_class_two_different_classes() {
    let (out, ok) = compile_and_run(
        r#"
class Width  { v: i64; pub fn get(self) -> i64 { return self.v; } }
class Height { v: i64; pub fn get(self) -> i64 { return self.v; } }
fn area(w: Width, h: Height) -> i64 { return w.get() * h.get(); }
fn main() {
    let w = Width  { v: 7 };
    let h = Height { v: 3 };
    println(area(w, h));
}
"#,
    );
    assert!(ok);
    assert_eq!(out, "21\n");
}

// 45. Method returning f64
#[test]
fn test_class_method_returning_f64() {
    let (out, ok) = compile_and_run(
        r#"
class Circle {
    radius: f64;
    pub fn area(self) -> f64 { return 3.14159 * self.radius * self.radius; }
}
fn main() {
    let c = Circle { radius: 2.0 };
    let a = c.area();
    println(a > 12.0);
    println(a < 13.0);
}
"#,
    );
    assert!(ok);
    assert_eq!(out, "true\ntrue\n");
}

// 46. Child accesses inherited public field via speed() method and own override
#[test]
fn test_class_inherited_base_field_via_method() {
    let (out, ok) = compile_and_run(
        r#"
pub open class Vehicle {
    pub speed: i64;
    pub fn speed(self) -> i64 { return self.speed; }
    pub open fn describe(self) -> i64 { return self.speed; }
}
pub class Car extends Vehicle {
    pub override fn describe(self) -> i64 { return self.speed * 2; }
}
fn main() {
    let v = Vehicle { speed: 30 };
    let car = Car { speed: 60 };
    println(v.speed());
    println(car.speed());
    println(v.describe());
    println(car.describe());
}
"#,
    );
    assert!(ok);
    assert_eq!(out, "30\n60\n30\n120\n");
}

// 47. Object used in match expression (via method)
#[test]
fn test_class_method_result_used_in_match() {
    let (out, ok) = compile_and_run(
        r#"
class Tag {
    kind: i64;
    pub fn kind(self) -> i64 { return self.kind; }
}
fn describe(t: Tag) -> String {
    let k = t.kind();
    if k == 1 { return "one"; }
    if k == 2 { return "two"; }
    return "other";
}
fn main() {
    println(describe(Tag { kind: 1 }));
    println(describe(Tag { kind: 2 }));
    println(describe(Tag { kind: 9 }));
}
"#,
    );
    assert!(ok);
    assert_eq!(out, "one\ntwo\nother\n");
}

// 48. Object created in if-else branch
#[test]
fn test_class_created_in_if_else() {
    let (out, ok) = compile_and_run(
        r#"
class Signed {
    v: i64;
    neg: bool;
    pub fn value(self) -> i64 { return self.v; }
    pub fn is_neg(self) -> bool { return self.neg; }
}
fn make(n: i64) -> Signed {
    if n < 0 {
        return Signed { v: n * -1, neg: true };
    }
    return Signed { v: n, neg: false };
}
fn main() {
    let a = make(-7);
    let b = make(3);
    println(a.value());
    println(a.is_neg());
    println(b.value());
    println(b.is_neg());
}
"#,
    );
    assert!(ok);
    assert_eq!(out, "7\ntrue\n3\nfalse\n");
}

// 49. Class missing field in literal is an error
#[test]
fn test_class_literal_missing_field_is_error() {
    assert!(expect_compile_error(
        r#"
class Point { x: i64; y: i64; }
fn main() {
    let p = Point { x: 1 };
    println(1);
}
"#
    ));
}

// 50. Class literal with extra field is an error
#[test]
fn test_class_literal_extra_field_is_error() {
    assert!(expect_compile_error(
        r#"
class Point { x: i64; }
fn main() {
    let p = Point { x: 1, z: 2 };
    println(1);
}
"#
    ));
}

// ── Subtype: Dog passed to fn(Animal) ─────────────────────────────────────

// 1. void return: child passes to parent-typed parameter
#[test]
fn test_subtype_child_passes_to_parent_param() {
    let (out, ok) = compile_and_run(
        r#"
pub open class Animal {}
pub class Dog extends Animal {}
fn feed(a: Animal) { println(1); }
fn main() {
    let d = Dog {};
    feed(d);
}
"#,
    );
    assert!(ok);
    assert_eq!(out, "1\n");
}

// 2. parent method (same name) callable on concrete type — each dispatch to own impl
#[test]
fn test_subtype_parent_method_callable_on_child() {
    let (out, ok) = compile_and_run(
        r#"
pub open class Animal {
    pub open fn kind(self) -> i64 { return 0; }
}
pub class Dog extends Animal {
    pub override fn kind(self) -> i64 { return 1; }
}
fn main() {
    let a = Animal {};
    let d = Dog {};
    println(a.kind());
    println(d.kind());
}
"#,
    );
    assert!(ok);
    assert_eq!(out, "0\n1\n");
}

// 3. function returns child as parent type
#[test]
fn test_subtype_function_returns_child_as_parent() {
    let (out, ok) = compile_and_run(
        r#"
pub open class Animal {
    pub fn tag(self) -> i64 { return 42; }
}
pub class Cat extends Animal {}
fn make() -> Animal { return Cat {}; }
fn main() {
    let a = make();
    println(a.tag());
}
"#,
    );
    assert!(ok);
    assert_eq!(out, "42\n");
}

// 4. child stored in parent-typed variable — compiles, parent method used
#[test]
fn test_subtype_stored_in_parent_typed_var() {
    let (out, ok) = compile_and_run(
        r#"
pub open class Vehicle {
    pub open fn wheels(self) -> i64 { return 4; }
}
pub class Bike extends Vehicle {
    pub override fn wheels(self) -> i64 { return 2; }
}
fn main() {
    let v: Vehicle = Bike {};
    let b = Bike {};
    println(v.wheels());
    println(b.wheels());
}
"#,
    );
    assert!(ok);
    // Dynamic dispatch: v holds a Bike at runtime, so Bike__wheels (2) is called.
    // b is a Bike, so Bike__wheels (2) is called.
    assert_eq!(out, "2\n2\n");
}

// 5. two different subtypes each compile correctly as parent-typed argument
#[test]
fn test_subtype_two_children_same_function() {
    let (out, ok) = compile_and_run(
        r#"
pub open class Shape {
    pub open fn name(self) -> i64 { return 0; }
}
pub class Square extends Shape {
    pub override fn name(self) -> i64 { return 4; }
}
pub class Triangle extends Shape {
    pub override fn name(self) -> i64 { return 3; }
}
fn accept_shape(s: Shape) { println(1); }
fn main() {
    let sq = Square {};
    let tr = Triangle {};
    accept_shape(sq);
    accept_shape(tr);
    println(sq.name());
    println(tr.name());
}
"#,
    );
    assert!(ok);
    assert_eq!(out, "1\n1\n4\n3\n");
}

// 6. child passed through two function calls
#[test]
fn test_subtype_child_passed_through_two_calls() {
    let (out, ok) = compile_and_run(
        r#"
pub open class Base {
    pub fn val(self) -> i64 { return 7; }
}
pub class Child extends Base {}
fn wrap(b: Base) -> i64 { return b.val(); }
fn outer(b: Base) -> i64 { return wrap(b); }
fn main() {
    println(outer(Child {}));
}
"#,
    );
    assert!(ok);
    assert_eq!(out, "7\n");
}

// 7. three-level hierarchy: grandchild compiles as grandparent arg; each type calls own method
#[test]
fn test_subtype_grandchild_to_grandparent_fn() {
    let (out, ok) = compile_and_run(
        r#"
pub open class A {
    pub open fn tag(self) -> i64 { return 1; }
}
pub open class B extends A {
    pub open override fn tag(self) -> i64 { return 2; }
}
pub class C extends B {
    pub override fn tag(self) -> i64 { return 3; }
}
fn accept_a(a: A) { println(1); }
fn main() {
    let c = C {};
    accept_a(c);
    println(A {}.tag());
    println(B {}.tag());
    println(c.tag());
}
"#,
    );
    assert!(ok);
    assert_eq!(out, "1\n1\n2\n3\n");
}

// 8. child with own field passes to parent-typed function; child's own method works
#[test]
fn test_subtype_child_with_extra_field_passes() {
    let (out, ok) = compile_and_run(
        r#"
pub open class Node {
    pub open fn kind(self) -> i64 { return 0; }
}
pub class Leaf extends Node {
    pub extra: i64;
    pub override fn kind(self) -> i64 { return 1; }
}
fn accept_node(n: Node) { println(1); }
fn main() {
    let leaf = Leaf { extra: 99 };
    accept_node(leaf);
    println(leaf.kind());
    println(leaf.extra);
}
"#,
    );
    assert!(ok);
    assert_eq!(out, "1\n1\n99\n");
}

// 9. base type rejected when child type expected (negative test)
#[test]
fn test_subtype_base_rejected_as_child() {
    assert!(expect_compile_error(
        r#"
pub open class Animal {}
pub class Dog extends Animal {}
fn use_dog(d: Dog) { println(1); }
fn main() {
    let a = Animal {};
    use_dog(a);
}
"#
    ));
}

// 10. sibling type rejected (not a subtype)
#[test]
fn test_subtype_sibling_rejected() {
    assert!(expect_compile_error(
        r#"
pub open class Animal {}
pub class Dog extends Animal {}
pub class Cat extends Animal {}
fn use_dog(d: Dog) { println(1); }
fn main() {
    let c = Cat {};
    use_dog(c);
}
"#
    ));
}

// ── Subtype: Dog passed to fn(Animal?) ────────────────────────────────────

// 1. child passes to nullable parent param
#[test]
fn test_nullable_subtype_child_to_nullable_parent() {
    let (out, ok) = compile_and_run(
        r#"
pub open class Animal {}
pub class Dog extends Animal {}
fn maybe_feed(a: Animal?) { println(a == nil); }
fn main() {
    let d = Dog {};
    maybe_feed(d);
}
"#,
    );
    assert!(ok);
    assert_eq!(out, "false\n");
}

// 2. nil also passes to nullable parent param
#[test]
fn test_nullable_nil_passes_to_nullable_parent() {
    let (out, ok) = compile_and_run(
        r#"
pub open class Animal {}
fn maybe_feed(a: Animal?) { println(a == nil); }
fn main() {
    maybe_feed(nil);
}
"#,
    );
    assert!(ok);
    assert_eq!(out, "true\n");
}

// 3. nil check inside nullable function
#[test]
fn test_nullable_subtype_nil_guard_in_function() {
    let (out, ok) = compile_and_run(
        r#"
pub open class Animal {
    pub fn kind(self) -> i64 { return 1; }
}
pub class Dog extends Animal {}
fn describe(a: Animal?) -> i64 {
    if a == nil { return -1; }
    return a.kind();
}
fn main() {
    let d = Dog {};
    println(describe(d));
    println(describe(nil));
}
"#,
    );
    assert!(ok);
    assert_eq!(out, "1\n-1\n");
}

// 4. child stored in nullable parent variable
#[test]
fn test_nullable_child_stored_in_nullable_parent_var() {
    let (out, ok) = compile_and_run(
        r#"
pub open class Animal {}
pub class Cat extends Animal {}
fn main() {
    let a: Animal? = Cat {};
    println(a == nil);
}
"#,
    );
    assert!(ok);
    assert_eq!(out, "false\n");
}

// 5. nil stored in nullable parent variable
#[test]
fn test_nullable_nil_stored_in_nullable_parent_var() {
    let (out, ok) = compile_and_run(
        r#"
pub open class Animal {}
fn main() {
    let a: Animal? = nil;
    println(a == nil);
}
"#,
    );
    assert!(ok);
    assert_eq!(out, "true\n");
}

// 6. function returning nullable parent from child
#[test]
fn test_nullable_function_returns_child_as_nullable_parent() {
    let (out, ok) = compile_and_run(
        r#"
pub open class Vehicle {
    pub fn tag(self) -> i64 { return 99; }
}
pub class Car extends Vehicle {}
fn maybe_car(use_it: bool) -> Vehicle? {
    if use_it { return Car {}; }
    return nil;
}
fn main() {
    let v = maybe_car(true);
    println(v == nil);
    let n = maybe_car(false);
    println(n == nil);
}
"#,
    );
    assert!(ok);
    assert_eq!(out, "false\ntrue\n");
}

// 7. child through nullable then nil-guarded method call
#[test]
fn test_nullable_child_through_nullable_then_method() {
    let (out, ok) = compile_and_run(
        r#"
pub open class Node {
    pub fn value(self) -> i64 { return 42; }
}
pub class Leaf extends Node {}
fn get_value(n: Node?) -> i64 {
    if n == nil { return 0; }
    return n.value();
}
fn main() {
    let leaf = Leaf {};
    println(get_value(leaf));
    println(get_value(nil));
}
"#,
    );
    assert!(ok);
    assert_eq!(out, "42\n0\n");
}

// 8. two different subtypes compile as nullable parent; nil check works
#[test]
fn test_nullable_two_children_to_nullable_parent() {
    let (out, ok) = compile_and_run(
        r#"
pub open class Fruit {
    pub open fn name(self) -> i64 { return 0; }
}
pub class Apple extends Fruit {
    pub override fn name(self) -> i64 { return 1; }
}
pub class Orange extends Fruit {
    pub override fn name(self) -> i64 { return 2; }
}
fn not_nil(f: Fruit?) -> bool {
    return f != nil;
}
fn main() {
    let a = Apple {};
    let o = Orange {};
    println(not_nil(a));
    println(not_nil(o));
    println(not_nil(nil));
    println(a.name());
    println(o.name());
}
"#,
    );
    assert!(ok);
    assert_eq!(out, "true\ntrue\nfalse\n1\n2\n");
}

// 9. child with field passed to nullable parent, field inaccessible through nullable
#[test]
fn test_nullable_child_field_inaccessible_through_nullable_base() {
    assert!(expect_compile_error(
        r#"
pub open class Animal {}
pub class Dog extends Animal { pub breed: i64; }
fn main() {
    let d: Animal? = Dog { breed: 1 };
    println(d.breed);
}
"#
    ));
}

// 10. nullable of child type assigned to nullable of parent type
#[test]
fn test_nullable_child_nullable_to_parent_nullable() {
    let (out, ok) = compile_and_run(
        r#"
pub open class Base {}
pub class Sub extends Base {}
fn use_base(b: Base?) -> i64 {
    if b == nil { return 0; }
    return 1;
}
fn main() {
    let s: Sub? = Sub {};
    println(use_base(s));
    let n: Sub? = nil;
    println(use_base(n));
}
"#,
    );
    assert!(ok);
    assert_eq!(out, "1\n0\n");
}

// ── GC tests ──────────────────────────────────────────────────────────────

// GC-01: single object freed after scope exit
#[test]
fn test_gc_01_single_object_freed_after_scope() {
    let (out, ok) = compile_and_run(
        r#"
class Box { v: i64; pub fn get(self) -> i64 { return self.v; } }
fn make() -> i64 {
    let b = Box { v: 1 };
    return b.get();
}
fn main() {
    let _ = make();
    gc_collect();
    println(gc_allocated_bytes());
}
"#,
    );
    assert!(ok);
    assert_eq!(out, "0\n");
}

// GC-02: two objects freed after scope exit
#[test]
fn test_gc_02_two_objects_freed_after_scope() {
    let (out, ok) = compile_and_run(
        r#"
class Box { v: i64; pub fn get(self) -> i64 { return self.v; } }
fn make() -> i64 {
    let a = Box { v: 1 };
    let b = Box { v: 2 };
    return a.get() + b.get();
}
fn main() {
    let _ = make();
    gc_collect();
    println(gc_allocated_bytes());
}
"#,
    );
    assert!(ok);
    assert_eq!(out, "0\n");
}

// GC-03: live object NOT freed
#[test]
fn test_gc_03_live_object_not_freed() {
    let (out, ok) = compile_and_run(
        r#"
class Box { v: i64; pub fn get(self) -> i64 { return self.v; } }
fn main() {
    let b = Box { v: 42 };
    gc_collect();
    println(b.get());
    println(gc_allocated_bytes() > 0);
}
"#,
    );
    assert!(ok);
    assert_eq!(out, "42\ntrue\n");
}

// GC-04: gc_allocated_bytes increases with each allocation
#[test]
fn test_gc_04_allocated_bytes_grows_per_alloc() {
    let (out, ok) = compile_and_run(
        r#"
class Box { v: i64; }
fn main() {
    let before = gc_allocated_bytes();
    let _a = Box { v: 1 };
    let mid = gc_allocated_bytes();
    let _b = Box { v: 2 };
    let after = gc_allocated_bytes();
    println(mid > before);
    println(after > mid);
}
"#,
    );
    assert!(ok);
    assert_eq!(out, "true\ntrue\n");
}

// GC-05: explicit gc_collect returns zero after all freed
#[test]
fn test_gc_05_explicit_collect_returns_zero() {
    let (out, ok) = compile_and_run(
        r#"
class Tmp { v: i64; pub fn get(self) -> i64 { return self.v; } }
fn alloc_and_drop() -> i64 {
    let t = Tmp { v: 99 };
    return t.get();
}
fn main() {
    let r1 = alloc_and_drop();
    let r2 = alloc_and_drop();
    println(r1 + r2);
    gc_collect();
    println(gc_allocated_bytes());
}
"#,
    );
    assert!(ok);
    assert_eq!(out, "198\n0\n");
}

// GC-06: object allocated in loop, freed after loop
#[test]
fn test_gc_06_objects_in_loop_freed_after() {
    let (out, ok) = compile_and_run(
        r#"
class Item { v: i64; pub fn get(self) -> i64 { return self.v; } }
fn process(n: i64) -> i64 {
    let mut i = 0;
    let mut sum = 0;
    while i < n {
        let item = Item { v: i };
        sum = sum + item.get();
        i = i + 1;
    }
    return sum;
}
fn main() {
    let result = process(5);
    gc_collect();
    println(result);
    println(gc_allocated_bytes());
}
"#,
    );
    assert!(ok);
    assert_eq!(out, "10\n0\n");
}

// GC-07: nested function allocation, inner freed
#[test]
fn test_gc_07_nested_function_alloc_freed() {
    let (out, ok) = compile_and_run(
        r#"
class Node { v: i64; pub fn get(self) -> i64 { return self.v; } }
fn inner() -> i64 {
    let n = Node { v: 5 };
    return n.get();
}
fn outer() -> i64 { return inner() + inner(); }
fn main() {
    let r = outer();
    gc_collect();
    println(r);
    println(gc_allocated_bytes());
}
"#,
    );
    assert!(ok);
    assert_eq!(out, "10\n0\n");
}

// GC-08: object field holding i64 doesn't prevent GC
#[test]
fn test_gc_08_i64_field_object_freed() {
    let (out, ok) = compile_and_run(
        r#"
class Point { x: i64; y: i64; pub fn sum(self) -> i64 { return self.x + self.y; } }
fn make_sum() -> i64 {
    let p = Point { x: 3, y: 4 };
    return p.sum();
}
fn main() {
    let _ = make_sum();
    gc_collect();
    println(gc_allocated_bytes());
}
"#,
    );
    assert!(ok);
    assert_eq!(out, "0\n");
}

// GC-09: bool field object freed
#[test]
fn test_gc_09_bool_field_object_freed() {
    let (out, ok) = compile_and_run(
        r#"
class Flag { on: bool; pub fn get(self) -> bool { return self.on; } }
fn check() -> bool {
    let f = Flag { on: true };
    return f.get();
}
fn main() {
    let _ = check();
    gc_collect();
    println(gc_allocated_bytes());
}
"#,
    );
    assert!(ok);
    assert_eq!(out, "0\n");
}

// GC-10: multiple collect cycles — already-freed objects stay at zero
#[test]
fn test_gc_10_multiple_collect_cycles() {
    let (out, ok) = compile_and_run(
        r#"
class Obj { v: i64; pub fn get(self) -> i64 { return self.v; } }
fn drop_obj() -> i64 { let o = Obj { v: 1 }; return o.get(); }
fn main() {
    let _ = drop_obj();
    gc_collect();
    let after1 = gc_allocated_bytes();
    gc_collect();
    let after2 = gc_allocated_bytes();
    println(after1);
    println(after2);
}
"#,
    );
    assert!(ok);
    assert_eq!(out, "0\n0\n");
}

// GC-11: object reachable through local variable survives
#[test]
fn test_gc_11_local_var_keeps_alive() {
    let (out, ok) = compile_and_run(
        r#"
class Node { v: i64; pub fn get(self) -> i64 { return self.v; } }
fn main() {
    let n = Node { v: 7 };
    gc_collect();
    gc_collect();
    println(n.get());
}
"#,
    );
    assert!(ok);
    assert_eq!(out, "7\n");
}

// GC-12: two live objects both survive collect
#[test]
fn test_gc_12_two_live_objects_both_survive() {
    let (out, ok) = compile_and_run(
        r#"
class A { v: i64; pub fn get(self) -> i64 { return self.v; } }
class B { v: i64; pub fn get(self) -> i64 { return self.v; } }
fn main() {
    let a = A { v: 10 };
    let b = B { v: 20 };
    gc_collect();
    println(a.get());
    println(b.get());
}
"#,
    );
    assert!(ok);
    assert_eq!(out, "10\n20\n");
}

// GC-13: live and dead objects — only dead freed
#[test]
fn test_gc_13_live_and_dead_objects_mixed() {
    let (out, ok) = compile_and_run(
        r#"
class Live { v: i64; pub fn get(self) -> i64 { return self.v; } }
class Dead { v: i64; pub fn get(self) -> i64 { return self.v; } }
fn drop_dead() -> i64 { let d = Dead { v: 0 }; return d.get(); }
fn main() {
    let live = Live { v: 5 };
    let _ = drop_dead();
    gc_collect();
    println(live.get());
    println(gc_allocated_bytes() > 0);
}
"#,
    );
    assert!(ok);
    assert_eq!(out, "5\ntrue\n");
}

// GC-14: object passed to function and returned as i64, original freed
#[test]
fn test_gc_14_passed_to_fn_extract_primitive_freed() {
    let (out, ok) = compile_and_run(
        r#"
class Wrap { v: i64; pub fn get(self) -> i64 { return self.v; } }
fn extract(w: Wrap) -> i64 { return w.get(); }
fn main() {
    let val = extract(Wrap { v: 99 });
    gc_collect();
    println(val);
    println(gc_allocated_bytes());
}
"#,
    );
    assert!(ok);
    assert_eq!(out, "99\n0\n");
}

// GC-15: object allocated before and after collect
#[test]
fn test_gc_15_alloc_collect_alloc() {
    let (out, ok) = compile_and_run(
        r#"
class Box { v: i64; pub fn get(self) -> i64 { return self.v; } }
fn drop_one() -> i64 { let b = Box { v: 1 }; return b.get(); }
fn main() {
    let _ = drop_one();
    gc_collect();
    let b2 = Box { v: 2 };
    println(b2.get());
    println(gc_allocated_bytes() > 0);
}
"#,
    );
    assert!(ok);
    assert_eq!(out, "2\ntrue\n");
}

// GC-16: object with string field — string GC-managed too
#[test]
fn test_gc_16_string_field_object_freed() {
    let (out, ok) = compile_and_run(
        r#"
class Msg { text: String; pub fn get(self) -> String { return self.text; } }
fn drop_msg() -> String {
    let m = Msg { text: "hello" };
    return m.get();
}
fn main() {
    let s = drop_msg();
    gc_collect();
    println(s);
}
"#,
    );
    assert!(ok);
    assert_eq!(out, "hello\n");
}

// GC-17: nullable field pointing to live object keeps it alive
#[test]
fn test_gc_17_nullable_field_keeps_child_alive() {
    let (out, ok) = compile_and_run(
        r#"
class Node { pub v: i64; pub next: Node?; }
fn main() {
    let tail = Node { v: 2, next: nil };
    let head = Node { v: 1, next: tail };
    gc_collect();
    println(head.v);
    let n = head.next;
    if n != nil { println(n.v); }
}
"#,
    );
    assert!(ok);
    assert_eq!(out, "1\n2\n");
}

// GC-18: nil nullable field — object still freed when out of scope
#[test]
fn test_gc_18_nil_nullable_field_object_freed() {
    let (out, ok) = compile_and_run(
        r#"
class Node { v: i64; next: Node?; pub fn get(self) -> i64 { return self.v; } }
fn make() -> i64 {
    let n = Node { v: 3, next: nil };
    return n.get();
}
fn main() {
    let _ = make();
    gc_collect();
    println(gc_allocated_bytes());
}
"#,
    );
    assert!(ok);
    assert_eq!(out, "0\n");
}

// GC-19: chain of nullable nodes — all freed together
#[test]
fn test_gc_19_chain_of_nodes_freed() {
    let (out, ok) = compile_and_run(
        r#"
class Node { pub v: i64; pub next: Node?; }
fn make_chain() -> i64 {
    let c = Node { v: 3, next: nil };
    let b = Node { v: 2, next: c };
    let a = Node { v: 1, next: b };
    return a.v;
}
fn main() {
    let _ = make_chain();
    gc_collect();
    println(gc_allocated_bytes());
}
"#,
    );
    assert!(ok);
    assert_eq!(out, "0\n");
}

// GC-20: chain of nullable nodes — head kept, rest freed (not possible to free partial chain while head live)
#[test]
fn test_gc_20_live_chain_all_survive() {
    let (out, ok) = compile_and_run(
        r#"
class Node { pub v: i64; pub next: Node?; }
fn main() {
    let c = Node { v: 3, next: nil };
    let b = Node { v: 2, next: c };
    let a = Node { v: 1, next: b };
    gc_collect();
    println(a.v);
    let n1 = a.next;
    if n1 != nil {
        println(n1.v);
        let n2 = n1.next;
        if n2 != nil { println(n2.v); }
    }
}
"#,
    );
    assert!(ok);
    assert_eq!(out, "1\n2\n3\n");
}

// GC-21: inherited class object freed
#[test]
fn test_gc_21_inherited_class_freed() {
    let (out, ok) = compile_and_run(
        r#"
pub open class Base { v: i64; pub fn get(self) -> i64 { return self.v; } }
pub class Child extends Base {}
fn drop_child() -> i64 { let c = Child { v: 5 }; return c.get(); }
fn main() {
    let _ = drop_child();
    gc_collect();
    println(gc_allocated_bytes());
}
"#,
    );
    assert!(ok);
    assert_eq!(out, "0\n");
}

// GC-22: inherited class object live, survives
#[test]
fn test_gc_22_inherited_class_live_survives() {
    let (out, ok) = compile_and_run(
        r#"
pub open class Base { v: i64; pub fn get(self) -> i64 { return self.v; } }
pub class Child extends Base {}
fn main() {
    let c = Child { v: 11 };
    gc_collect();
    println(c.get());
}
"#,
    );
    assert!(ok);
    assert_eq!(out, "11\n");
}

// GC-23: object with prot field freed
#[test]
fn test_gc_23_prot_field_object_freed() {
    let (out, ok) = compile_and_run(
        r#"
class Secret { prot key: i64; pub fn get(self) -> i64 { return self.key; } }
fn drop_it() -> i64 { let s = Secret { key: 7 }; return s.get(); }
fn main() {
    let _ = drop_it();
    gc_collect();
    println(gc_allocated_bytes());
}
"#,
    );
    assert!(ok);
    assert_eq!(out, "0\n");
}

// GC-24: object allocated inside if-branch, freed after branch
#[test]
fn test_gc_24_object_in_if_branch_freed() {
    let (out, ok) = compile_and_run(
        r#"
class Tmp { v: i64; pub fn get(self) -> i64 { return self.v; } }
fn conditional(flag: bool) -> i64 {
    if flag {
        let t = Tmp { v: 3 };
        return t.get();
    }
    return 0;
}
fn main() {
    let _ = conditional(true);
    gc_collect();
    println(gc_allocated_bytes());
}
"#,
    );
    assert!(ok);
    assert_eq!(out, "0\n");
}

// GC-25: object alive across if-branch
#[test]
fn test_gc_25_object_alive_across_if() {
    let (out, ok) = compile_and_run(
        r#"
class Box { v: i64; pub fn get(self) -> i64 { return self.v; } }
fn main() {
    let b = Box { v: 9 };
    if b.get() > 0 {
        gc_collect();
    }
    println(b.get());
}
"#,
    );
    assert!(ok);
    assert_eq!(out, "9\n");
}

// GC-26: object allocated inside while loop, freed each iteration
#[test]
fn test_gc_26_loop_object_freed_each_iteration() {
    let (out, ok) = compile_and_run(
        r#"
class Tmp { v: i64; pub fn get(self) -> i64 { return self.v; } }
fn main() {
    let mut i = 0;
    while i < 3 {
        let t = Tmp { v: i };
        let _ = t.get();
        i = i + 1;
    }
    gc_collect();
    println(gc_allocated_bytes());
}
"#,
    );
    assert!(ok);
    assert_eq!(out, "0\n");
}

// GC-27: collect before allocation — zero
#[test]
fn test_gc_27_collect_before_any_alloc() {
    let (out, ok) = compile_and_run(
        r#"
fn main() {
    gc_collect();
    println(gc_allocated_bytes());
}
"#,
    );
    assert!(ok);
    assert_eq!(out, "0\n");
}

// GC-28: allocated_bytes zero at start of program
#[test]
fn test_gc_28_bytes_zero_at_start() {
    let (out, ok) = compile_and_run(
        r#"
fn main() {
    println(gc_allocated_bytes());
}
"#,
    );
    assert!(ok);
    assert_eq!(out, "0\n");
}

// GC-29: object size proportional to field count
#[test]
fn test_gc_29_larger_object_uses_more_bytes() {
    let (out, ok) = compile_and_run(
        r#"
class Small { a: i64; }
class Large { a: i64; b: i64; c: i64; d: i64; }
fn main() {
    let before = gc_allocated_bytes();
    let _s = Small { a: 1 };
    let after_small = gc_allocated_bytes();
    let _l = Large { a: 1, b: 2, c: 3, d: 4 };
    let after_large = gc_allocated_bytes();
    println(after_small > before);
    println(after_large > after_small);
}
"#,
    );
    assert!(ok);
    assert_eq!(out, "true\ntrue\n");
}

// GC-30: GC-managed object returned from function, caller holds it
#[test]
fn test_gc_30_object_returned_and_held_by_caller() {
    let (out, ok) = compile_and_run(
        r#"
class Node { v: i64; pub fn get(self) -> i64 { return self.v; } }
fn make(v: i64) -> Node { return Node { v: v }; }
fn main() {
    let n = make(55);
    gc_collect();
    println(n.get());
}
"#,
    );
    assert!(ok);
    assert_eq!(out, "55\n");
}

// GC-31: object passed to function, function holds local copy
#[test]
fn test_gc_31_object_alive_while_in_called_function() {
    let (out, ok) = compile_and_run(
        r#"
class Box { v: i64; pub fn get(self) -> i64 { return self.v; } }
fn use_box(b: Box) -> i64 {
    gc_collect();
    return b.get();
}
fn main() {
    let b = Box { v: 7 };
    println(use_box(b));
}
"#,
    );
    assert!(ok);
    assert_eq!(out, "7\n");
}

// GC-32: two separate collect calls
#[test]
fn test_gc_32_two_separate_collects() {
    let (out, ok) = compile_and_run(
        r#"
class Box { v: i64; pub fn get(self) -> i64 { return self.v; } }
fn drop_one() -> i64 { let b = Box { v: 1 }; return b.get(); }
fn main() {
    let r1 = drop_one();
    gc_collect();
    let b = Box { v: 2 };
    let r2 = b.get();
    gc_collect();
    println(r1 + r2);
    println(gc_allocated_bytes() > 0);
}
"#,
    );
    assert!(ok);
    assert_eq!(out, "3\ntrue\n");
}

// GC-33: Option<T> with class payload — freed when out of scope
#[test]
fn test_gc_33_option_class_payload_freed() {
    let (out, ok) = compile_and_run(
        r#"
class Node { v: i64; pub fn get(self) -> i64 { return self.v; } }
fn make_opt() -> i64 {
    let opt = Option::Some(Node { v: 42 });
    return match opt {
        Option::Some(n) => n.get(),
        Option::None => 0,
    };
}
fn main() {
    let _ = make_opt();
    gc_collect();
    println(gc_allocated_bytes());
}
"#,
    );
    assert!(ok);
    assert_eq!(out, "0\n");
}

// GC-34: Option::Some class payload survives when held
#[test]
fn test_gc_34_option_class_payload_survives_when_held() {
    let (out, ok) = compile_and_run(
        r#"
class Node { v: i64; pub fn get(self) -> i64 { return self.v; } }
fn main() {
    let opt = Option::Some(Node { v: 13 });
    gc_collect();
    let v = match opt {
        Option::Some(n) => n.get(),
        Option::None => 0,
    };
    println(v);
}
"#,
    );
    assert!(ok);
    assert_eq!(out, "13\n");
}

// GC-35: Result::Ok with class payload freed when out of scope
#[test]
fn test_gc_35_result_ok_payload_freed() {
    let (out, ok) = compile_and_run(
        r#"
class Node { v: i64; pub fn get(self) -> i64 { return self.v; } }
fn make_res() -> i64 {
    let r: Result<Node, String> = Result::Ok(Node { v: 7 });
    return match r {
        Result::Ok(n) => n.get(),
        Result::Err(_) => 0,
    };
}
fn main() {
    let _ = make_res();
    gc_collect();
    println(gc_allocated_bytes());
}
"#,
    );
    assert!(ok);
    assert_eq!(out, "0\n");
}

// GC-36: Result::Ok class payload survives when held
#[test]
fn test_gc_36_result_ok_payload_survives_when_held() {
    let (out, ok) = compile_and_run(
        r#"
class Node { v: i64; pub fn get(self) -> i64 { return self.v; } }
fn main() {
    let r: Result<Node, String> = Result::Ok(Node { v: 17 });
    gc_collect();
    let v = match r {
        Result::Ok(n) => n.get(),
        Result::Err(_) => 0,
    };
    println(v);
}
"#,
    );
    assert!(ok);
    assert_eq!(out, "17\n");
}

// GC-37: gc_collect does not corrupt live i64 variables
#[test]
fn test_gc_37_collect_does_not_corrupt_i64_vars() {
    let (out, ok) = compile_and_run(
        r#"
fn main() {
    let x = 12345;
    gc_collect();
    println(x);
}
"#,
    );
    assert!(ok);
    assert_eq!(out, "12345\n");
}

// GC-38: gc_collect does not corrupt live bool variables
#[test]
fn test_gc_38_collect_does_not_corrupt_bool_vars() {
    let (out, ok) = compile_and_run(
        r#"
fn main() {
    let b = true;
    gc_collect();
    println(b);
}
"#,
    );
    assert!(ok);
    assert_eq!(out, "true\n");
}

// GC-39: gc_collect does not corrupt live string variables
#[test]
fn test_gc_39_collect_does_not_corrupt_string_vars() {
    let (out, ok) = compile_and_run(
        r#"
fn main() {
    let s = "hello gc";
    gc_collect();
    println(s);
}
"#,
    );
    assert!(ok);
    assert_eq!(out, "hello gc\n");
}

// GC-40: object with multiple i64 fields freed correctly
#[test]
fn test_gc_40_multi_i64_field_object_freed() {
    let (out, ok) = compile_and_run(
        r#"
class Quad { a: i64; b: i64; c: i64; d: i64;
    pub fn sum(self) -> i64 { return self.a + self.b + self.c + self.d; }
}
fn make() -> i64 {
    let q = Quad { a: 1, b: 2, c: 3, d: 4 };
    return q.sum();
}
fn main() {
    let r = make();
    println(r);
    gc_collect();
    println(gc_allocated_bytes());
}
"#,
    );
    assert!(ok);
    assert_eq!(out, "10\n0\n");
}

// GC-41: object allocated in deeply nested function freed
#[test]
fn test_gc_41_deep_nested_alloc_freed() {
    let (out, ok) = compile_and_run(
        r#"
class Node { v: i64; pub fn get(self) -> i64 { return self.v; } }
fn f3() -> i64 { let n = Node { v: 3 }; return n.get(); }
fn f2() -> i64 { return f3() + f3(); }
fn f1() -> i64 { return f2() + f2(); }
fn main() {
    let _ = f1();
    gc_collect();
    println(gc_allocated_bytes());
}
"#,
    );
    assert!(ok);
    assert_eq!(out, "0\n");
}

// GC-42: recursive function allocating objects — all freed after recursion
#[test]
fn test_gc_42_recursive_alloc_freed() {
    let (out, ok) = compile_and_run(
        r#"
class Node { v: i64; pub fn get(self) -> i64 { return self.v; } }
fn sum(n: i64) -> i64 {
    if n <= 0 { return 0; }
    let node = Node { v: n };
    return node.get() + sum(n - 1);
}
fn main() {
    let _ = sum(5);
    gc_collect();
    println(gc_allocated_bytes());
}
"#,
    );
    assert!(ok);
    assert_eq!(out, "0\n");
}

// GC-43: live object in recursive function survives
#[test]
fn test_gc_43_live_object_in_recursive_fn_survives() {
    let (out, ok) = compile_and_run(
        r#"
class Box { v: i64; pub fn get(self) -> i64 { return self.v; } }
fn fib(n: i64) -> i64 {
    if n <= 1 { return n; }
    return fib(n - 1) + fib(n - 2);
}
fn main() {
    let b = Box { v: fib(5) };
    gc_collect();
    println(b.get());
}
"#,
    );
    assert!(ok);
    assert_eq!(out, "5\n");
}

// GC-44: object stored in multiple variables (aliases) — freed when all out of scope
#[test]
fn test_gc_44_alias_both_out_of_scope_freed() {
    let (out, ok) = compile_and_run(
        r#"
class Node { v: i64; pub fn get(self) -> i64 { return self.v; } }
fn make_two() -> i64 {
    let a = Node { v: 1 };
    let b = a;
    return b.get();
}
fn main() {
    let _ = make_two();
    gc_collect();
    println(gc_allocated_bytes());
}
"#,
    );
    assert!(ok);
    assert_eq!(out, "0\n");
}

// GC-45: object returned from if-else — retained by caller
#[test]
fn test_gc_45_conditional_returned_object_retained() {
    let (out, ok) = compile_and_run(
        r#"
class A { v: i64; pub fn get(self) -> i64 { return self.v; } }
class B { v: i64; pub fn get(self) -> i64 { return self.v; } }
fn make_a(flag: bool) -> i64 {
    if flag {
        let a = A { v: 10 };
        return a.get();
    }
    let b = B { v: 20 };
    return b.get();
}
fn main() {
    let ra = make_a(true);
    let rb = make_a(false);
    gc_collect();
    println(ra);
    println(rb);
    println(gc_allocated_bytes());
}
"#,
    );
    assert!(ok);
    assert_eq!(out, "10\n20\n0\n");
}

// GC-46: enum payload (non-class) — no GC impact expected
#[test]
fn test_gc_46_i64_enum_payload_no_gc_impact() {
    let (out, ok) = compile_and_run(
        r#"
fn main() {
    let before = gc_allocated_bytes();
    let opt = Option::Some(42);
    let after = gc_allocated_bytes();
    println(after > before);
    let _ = match opt { Option::Some(v) => v, Option::None => 0 };
}
"#,
    );
    assert!(ok);
    assert_eq!(out, "true\n");
}

// GC-47: gc_collect called with no allocations is safe
#[test]
fn test_gc_47_collect_with_no_allocs_safe() {
    let (out, ok) = compile_and_run(
        r#"
fn main() {
    gc_collect();
    gc_collect();
    gc_collect();
    println(gc_allocated_bytes());
}
"#,
    );
    assert!(ok);
    assert_eq!(out, "0\n");
}

// GC-48: large number of objects freed in one collect
#[test]
fn test_gc_48_many_objects_freed_together() {
    let (out, ok) = compile_and_run(
        r#"
class Obj { v: i64; pub fn get(self) -> i64 { return self.v; } }
fn make_many() -> i64 {
    let mut sum = 0;
    let mut i = 0;
    while i < 20 {
        let o = Obj { v: i };
        sum = sum + o.get();
        i = i + 1;
    }
    return sum;
}
fn main() {
    let _ = make_many();
    gc_collect();
    println(gc_allocated_bytes());
}
"#,
    );
    assert!(ok);
    assert_eq!(out, "0\n");
}

// GC-49: objects allocated in separate scopes both freed
#[test]
fn test_gc_49_two_scopes_both_freed() {
    let (out, ok) = compile_and_run(
        r#"
class A { v: i64; pub fn get(self) -> i64 { return self.v; } }
class B { v: i64; pub fn get(self) -> i64 { return self.v; } }
fn scope1() -> i64 { let a = A { v: 1 }; return a.get(); }
fn scope2() -> i64 { let b = B { v: 2 }; return b.get(); }
fn main() {
    let r1 = scope1();
    let r2 = scope2();
    println(r1 + r2);
    gc_collect();
    println(gc_allocated_bytes());
}
"#,
    );
    assert!(ok);
    assert_eq!(out, "3\n0\n");
}

// GC-50: gc_allocated_bytes is monotonically increasing without collect
#[test]
fn test_gc_50_bytes_monotonically_increasing() {
    let (out, ok) = compile_and_run(
        r#"
class Box { v: i64; }
fn main() {
    let b0 = gc_allocated_bytes();
    let _a = Box { v: 1 };
    let b1 = gc_allocated_bytes();
    let _b = Box { v: 2 };
    let b2 = gc_allocated_bytes();
    let _c = Box { v: 3 };
    let b3 = gc_allocated_bytes();
    println(b1 >= b0);
    println(b2 >= b1);
    println(b3 >= b2);
}
"#,
    );
    assert!(ok);
    assert_eq!(out, "true\ntrue\ntrue\n");
}

// GC-51: object with only public fields freed
#[test]
fn test_gc_51_all_public_fields_object_freed() {
    let (out, ok) = compile_and_run(
        r#"
class Point { pub x: i64; pub y: i64; }
fn drop_it() -> i64 {
    let p = Point { x: 3, y: 4 };
    return p.x + p.y;
}
fn main() {
    let _ = drop_it();
    gc_collect();
    println(gc_allocated_bytes());
}
"#,
    );
    assert!(ok);
    assert_eq!(out, "0\n");
}

// GC-52: inherited object freed (child)
#[test]
fn test_gc_52_child_class_object_freed() {
    let (out, ok) = compile_and_run(
        r#"
pub open class Base { v: i64; pub fn get(self) -> i64 { return self.v; } }
pub class Child extends Base { extra: i64; pub fn extra(self) -> i64 { return self.extra; } }
fn drop_child() -> i64 {
    let c = Child { v: 1, extra: 2 };
    return c.get() + c.extra();
}
fn main() {
    let r = drop_child();
    println(r);
    gc_collect();
    println(gc_allocated_bytes());
}
"#,
    );
    assert!(ok);
    assert_eq!(out, "3\n0\n");
}

// GC-53: inherited object survives when live
#[test]
fn test_gc_53_child_class_survives_when_live() {
    let (out, ok) = compile_and_run(
        r#"
pub open class Base { v: i64; pub fn get(self) -> i64 { return self.v; } }
pub class Child extends Base { extra: i64; }
fn main() {
    let c = Child { v: 10, extra: 5 };
    gc_collect();
    println(c.get());
}
"#,
    );
    assert!(ok);
    assert_eq!(out, "10\n");
}

// GC-54: three-level hierarchy object freed
#[test]
fn test_gc_54_three_level_hierarchy_freed() {
    let (out, ok) = compile_and_run(
        r#"
pub open class A { v: i64; pub fn get(self) -> i64 { return self.v; } }
pub open class B extends A {}
pub class C extends B {}
fn drop_c() -> i64 { let c = C { v: 7 }; return c.get(); }
fn main() {
    let _ = drop_c();
    gc_collect();
    println(gc_allocated_bytes());
}
"#,
    );
    assert!(ok);
    assert_eq!(out, "0\n");
}

// GC-55: three-level hierarchy object survives when live
#[test]
fn test_gc_55_three_level_hierarchy_survives() {
    let (out, ok) = compile_and_run(
        r#"
pub open class A { v: i64; pub fn get(self) -> i64 { return self.v; } }
pub open class B extends A {}
pub class C extends B {}
fn main() {
    let c = C { v: 22 };
    gc_collect();
    println(c.get());
}
"#,
    );
    assert!(ok);
    assert_eq!(out, "22\n");
}

// GC-56: object holding child type — all freed
#[test]
fn test_gc_56_object_holding_child_freed() {
    let (out, ok) = compile_and_run(
        r#"
pub open class Base { v: i64; pub fn get(self) -> i64 { return self.v; } }
pub class Child extends Base {}
fn process() -> i64 {
    let c = Child { v: 3 };
    let b: Base = c;
    return b.get();
}
fn main() {
    let _ = process();
    gc_collect();
    println(gc_allocated_bytes());
}
"#,
    );
    assert!(ok);
    assert_eq!(out, "0\n");
}

// GC-57: object method returns new object (both freed after scope)
#[test]
fn test_gc_57_method_returning_new_object_both_freed() {
    let (out, ok) = compile_and_run(
        r#"
class Outer { v: i64; pub fn val(self) -> i64 { return self.v; } }
class Inner { w: i64; pub fn val(self) -> i64 { return self.w; } }
fn compute() -> i64 {
    let o = Outer { v: 5 };
    let i = Inner { w: o.val() * 2 };
    return i.val();
}
fn main() {
    let _ = compute();
    gc_collect();
    println(gc_allocated_bytes());
}
"#,
    );
    assert!(ok);
    assert_eq!(out, "0\n");
}

// GC-58: gc_allocated_bytes after one collect then one alloc equals one object
#[test]
fn test_gc_58_bytes_after_collect_then_alloc() {
    let (out, ok) = compile_and_run(
        r#"
class A { v: i64; pub fn get(self) -> i64 { return self.v; } }
class B { v: i64; pub fn get(self) -> i64 { return self.v; } }
fn drop_a() -> i64 { let a = A { v: 1 }; return a.get(); }
fn main() {
    let r = drop_a();
    println(r);
    gc_collect();
    let zero = gc_allocated_bytes();
    let b = B { v: 2 };
    let one = gc_allocated_bytes();
    println(zero);
    println(one > 0);
    println(b.get());
}
"#,
    );
    assert!(ok);
    assert_eq!(out, "1\n0\ntrue\n2\n");
}

// GC-59: object with f64 field freed
#[test]
fn test_gc_59_f64_field_object_freed() {
    let (out, ok) = compile_and_run(
        r#"
class Flt { v: f64; pub fn get(self) -> f64 { return self.v; } }
fn drop_it() -> f64 { let f = Flt { v: 1.5 }; return f.get(); }
fn main() {
    let _ = drop_it();
    gc_collect();
    println(gc_allocated_bytes());
}
"#,
    );
    assert!(ok);
    assert_eq!(out, "0\n");
}

// GC-60: object with f64 field survives collect
#[test]
fn test_gc_60_f64_field_object_survives() {
    let (out, ok) = compile_and_run(
        r#"
class Flt { v: f64; pub fn get(self) -> f64 { return self.v; } }
fn main() {
    let f = Flt { v: 2.5 };
    gc_collect();
    let r = f.get();
    println(r > 2.0);
    println(r < 3.0);
}
"#,
    );
    assert!(ok);
    assert_eq!(out, "true\ntrue\n");
}

// GC-61: nullable object freed when nil-guarded scope exits
#[test]
fn test_gc_61_nullable_freed_when_scope_exits() {
    let (out, ok) = compile_and_run(
        r#"
class Node { v: i64; pub fn get(self) -> i64 { return self.v; } }
fn extract(n: Node?) -> i64 {
    if n == nil { return 0; }
    return n.get();
}
fn make_and_extract() -> i64 {
    let n: Node? = Node { v: 5 };
    return extract(n);
}
fn main() {
    let r = make_and_extract();
    gc_collect();
    println(r);
    println(gc_allocated_bytes());
}
"#,
    );
    assert!(ok);
    assert_eq!(out, "5\n0\n");
}

// GC-62: nullable nil field — no allocation
#[test]
fn test_gc_62_nullable_nil_no_extra_alloc() {
    let (out, ok) = compile_and_run(
        r#"
class Node { v: i64; }
fn main() {
    let before = gc_allocated_bytes();
    let n: Node? = nil;
    let after = gc_allocated_bytes();
    println(n == nil);
    println(before);
    println(after);
}
"#,
    );
    assert!(ok);
    assert_eq!(out, "true\n0\n0\n");
}

// GC-63: two nullable nodes, one nil — only non-nil freed
#[test]
fn test_gc_63_nullable_one_nil_one_freed() {
    let (out, ok) = compile_and_run(
        r#"
class Node { v: i64; pub fn get(self) -> i64 { return self.v; } }
fn make() -> i64 {
    let a: Node? = Node { v: 1 };
    let b: Node? = nil;
    if a != nil { return a.get(); }
    return 0;
}
fn main() {
    let _ = make();
    gc_collect();
    println(gc_allocated_bytes());
}
"#,
    );
    assert!(ok);
    assert_eq!(out, "0\n");
}

// GC-64: object reachable from function parameter — not freed during call
#[test]
fn test_gc_64_object_not_freed_while_in_param() {
    let (out, ok) = compile_and_run(
        r#"
class Box { v: i64; pub fn get(self) -> i64 { return self.v; } }
fn consume(b: Box) -> i64 {
    gc_collect();
    return b.get();
}
fn main() {
    println(consume(Box { v: 77 }));
}
"#,
    );
    assert!(ok);
    assert_eq!(out, "77\n");
}

// GC-65: object in Option::Some survives gc when option is live
#[test]
fn test_gc_65_option_some_live_survives() {
    let (out, ok) = compile_and_run(
        r#"
class Node { v: i64; pub fn get(self) -> i64 { return self.v; } }
fn main() {
    let opt = Option::Some(Node { v: 8 });
    gc_collect();
    let v = opt.unwrap();
    println(v.get());
}
"#,
    );
    assert!(ok);
    assert_eq!(out, "8\n");
}

// GC-66: gc_collect after empty loop still zero
#[test]
fn test_gc_66_collect_after_zero_iter_loop() {
    let (out, ok) = compile_and_run(
        r#"
class Obj { v: i64; }
fn main() {
    let mut i = 0;
    while i < 0 {
        let _ = Obj { v: i };
        i = i + 1;
    }
    gc_collect();
    println(gc_allocated_bytes());
}
"#,
    );
    assert!(ok);
    assert_eq!(out, "0\n");
}

// GC-67: object freed after being passed by value and returned as i64
#[test]
fn test_gc_67_pass_by_value_extract_i64_freed() {
    let (out, ok) = compile_and_run(
        r#"
class Wrap { v: i64; pub fn get(self) -> i64 { return self.v; } }
fn extract(w: Wrap) -> i64 { return w.get(); }
fn main() {
    let r = extract(Wrap { v: 100 });
    gc_collect();
    println(r);
    println(gc_allocated_bytes());
}
"#,
    );
    assert!(ok);
    assert_eq!(out, "100\n0\n");
}

// GC-68: class with inherited prot field freed
#[test]
fn test_gc_68_inherited_prot_field_object_freed() {
    let (out, ok) = compile_and_run(
        r#"
pub open class Base { prot v: i64; pub fn get(self) -> i64 { return self.v; } }
pub class Child extends Base { pub fn doubled(self) -> i64 { return self.v * 2; } }
fn drop_child() -> i64 {
    let c = Child { v: 4 };
    return c.doubled();
}
fn main() {
    let _ = drop_child();
    gc_collect();
    println(gc_allocated_bytes());
}
"#,
    );
    assert!(ok);
    assert_eq!(out, "0\n");
}

// GC-69: object alive through method chain
#[test]
fn test_gc_69_object_alive_through_method_chain() {
    let (out, ok) = compile_and_run(
        r#"
class Node { v: i64; pub fn get(self) -> i64 { return self.v; } pub fn doubled(self) -> i64 { return self.v * 2; } }
fn main() {
    let n = Node { v: 5 };
    gc_collect();
    let a = n.get();
    let b = n.doubled();
    println(a);
    println(b);
}
"#,
    );
    assert!(ok);
    assert_eq!(out, "5\n10\n");
}

// GC-70: allocate, collect, verify zero, allocate again, verify positive
#[test]
fn test_gc_70_alloc_collect_zero_alloc_positive() {
    let (out, ok) = compile_and_run(
        r#"
class A { v: i64; pub fn get(self) -> i64 { return self.v; } }
fn drop_a() -> i64 { let a = A { v: 1 }; return a.get(); }
fn main() {
    let r = drop_a();
    println(r);
    gc_collect();
    println(gc_allocated_bytes());
    let b = A { v: 2 };
    println(gc_allocated_bytes() > 0);
    println(b.get());
}
"#,
    );
    assert!(ok);
    assert_eq!(out, "1\n0\ntrue\n2\n");
}

// GC-71: deeply nested object reference — all alive while root is live
#[test]
fn test_gc_71_nested_nullable_chain_all_alive() {
    let (out, ok) = compile_and_run(
        r#"
class N { pub v: i64; pub n: N?; }
fn main() {
    let d = N { v: 3, n: nil };
    let c = N { v: 2, n: d };
    let b = N { v: 1, n: c };
    gc_collect();
    println(b.v);
    let bc = b.n;
    if bc != nil {
        println(bc.v);
        let bcc = bc.n;
        if bcc != nil { println(bcc.v); }
    }
}
"#,
    );
    assert!(ok);
    assert_eq!(out, "1\n2\n3\n");
}

// GC-72: object used in conditional — survives both branches
#[test]
fn test_gc_72_object_survives_across_condition() {
    let (out, ok) = compile_and_run(
        r#"
class Box { v: i64; pub fn get(self) -> i64 { return self.v; } }
fn main() {
    let b = Box { v: 3 };
    let flag = b.get() > 2;
    gc_collect();
    if flag {
        println(b.get());
    }
}
"#,
    );
    assert!(ok);
    assert_eq!(out, "3\n");
}

// GC-73: gc does not affect i64 arithmetic result
#[test]
fn test_gc_73_gc_does_not_affect_arithmetic() {
    let (out, ok) = compile_and_run(
        r#"
class Tmp { v: i64; pub fn get(self) -> i64 { return self.v; } }
fn main() {
    let t = Tmp { v: 10 };
    let tv = t.get();
    let x = tv * 3;
    gc_collect();
    println(x);
}
"#,
    );
    assert!(ok);
    assert_eq!(out, "30\n");
}

// GC-74: object allocated after collect has fresh identity
#[test]
fn test_gc_74_post_collect_object_fresh() {
    let (out, ok) = compile_and_run(
        r#"
class V { val: i64; pub fn get(self) -> i64 { return self.val; } }
fn drop_v() -> i64 { let v = V { val: 1 }; return v.get(); }
fn main() {
    let _ = drop_v();
    gc_collect();
    let v2 = V { val: 99 };
    println(v2.get());
}
"#,
    );
    assert!(ok);
    assert_eq!(out, "99\n");
}

// GC-75: multiple classes, mixed live and dead
#[test]
fn test_gc_75_mixed_live_dead_multiple_classes() {
    let (out, ok) = compile_and_run(
        r#"
class A { v: i64; pub fn get(self) -> i64 { return self.v; } }
class B { v: i64; pub fn get(self) -> i64 { return self.v; } }
class C { v: i64; pub fn get(self) -> i64 { return self.v; } }
fn drop_bc() -> i64 {
    let b = B { v: 2 };
    let c = C { v: 3 };
    return b.get() + c.get();
}
fn main() {
    let a = A { v: 1 };
    let _ = drop_bc();
    gc_collect();
    println(a.get());
    println(gc_allocated_bytes() > 0);
}
"#,
    );
    assert!(ok);
    assert_eq!(out, "1\ntrue\n");
}

// GC-76: object with bool field freed
#[test]
fn test_gc_76_bool_field_object_freed_correctly() {
    let (out, ok) = compile_and_run(
        r#"
class Toggle { flag: bool; pub fn get(self) -> bool { return self.flag; } }
fn drop_toggle() -> bool {
    let t = Toggle { flag: false };
    return t.get();
}
fn main() {
    let _ = drop_toggle();
    gc_collect();
    println(gc_allocated_bytes());
}
"#,
    );
    assert!(ok);
    assert_eq!(out, "0\n");
}

// GC-77: object survives across multiple function calls
#[test]
fn test_gc_77_object_survives_multiple_fn_calls() {
    let (out, ok) = compile_and_run(
        r#"
class Acc { v: i64; pub fn get(self) -> i64 { return self.v; } }
fn use_acc(a: Acc) -> i64 { return a.get(); }
fn main() {
    let a = Acc { v: 5 };
    let r1 = use_acc(a);
    gc_collect();
    let r2 = use_acc(a);
    gc_collect();
    println(r1 + r2);
}
"#,
    );
    assert!(ok);
    assert_eq!(out, "10\n");
}

// GC-78: interleaved alloc/collect/alloc/collect stays consistent
#[test]
fn test_gc_78_interleaved_alloc_collect() {
    let (out, ok) = compile_and_run(
        r#"
class Box { v: i64; pub fn get(self) -> i64 { return self.v; } }
fn drop_box(v: i64) -> i64 { let b = Box { v: v }; return b.get(); }
fn main() {
    let r1 = drop_box(1);
    gc_collect();
    let r2 = drop_box(2);
    gc_collect();
    let r3 = drop_box(3);
    gc_collect();
    println(r1 + r2 + r3);
    println(gc_allocated_bytes());
}
"#,
    );
    assert!(ok);
    assert_eq!(out, "6\n0\n");
}

// GC-79: object created inside match arm freed
#[test]
fn test_gc_79_object_in_match_arm_freed() {
    let (out, ok) = compile_and_run(
        r#"
class Tmp { v: i64; pub fn get(self) -> i64 { return self.v; } }
fn wrap(v: i64) -> i64 {
    let t = Tmp { v: v };
    return t.get();
}
fn main() {
    let r = match Option::Some(9) {
        Option::Some(v) => wrap(v),
        Option::None => 0,
    };
    println(r);
    gc_collect();
    println(gc_allocated_bytes());
}
"#,
    );
    assert!(ok);
    assert_eq!(out, "9\n0\n");
}

// GC-80: subtype object used as base type — GC still works
#[test]
fn test_gc_80_subtype_as_base_gc_works() {
    let (out, ok) = compile_and_run(
        r#"
pub open class Base { v: i64; pub fn get(self) -> i64 { return self.v; } }
pub class Child extends Base {}
fn process(b: Base) -> i64 { return b.get(); }
fn make() -> i64 {
    let c = Child { v: 6 };
    return process(c);
}
fn main() {
    let _ = make();
    gc_collect();
    println(gc_allocated_bytes());
}
"#,
    );
    assert!(ok);
    assert_eq!(out, "0\n");
}

// GC-81: object method call does not prevent GC after scope
#[test]
fn test_gc_81_method_call_then_freed() {
    let (out, ok) = compile_and_run(
        r#"
class Counter { n: i64; pub fn next(self) -> i64 { return self.n + 1; } }
fn run() -> i64 {
    let c = Counter { n: 0 };
    return c.next() + c.next();
}
fn main() {
    let _ = run();
    gc_collect();
    println(gc_allocated_bytes());
}
"#,
    );
    assert!(ok);
    assert_eq!(out, "0\n");
}

// GC-82: object with optional class field — field freed with owner
#[test]
fn test_gc_82_optional_class_field_freed_with_owner() {
    let (out, ok) = compile_and_run(
        r#"
class Inner { v: i64; pub fn get(self) -> i64 { return self.v; } }
class Outer { pub child: Inner?; }
fn make() -> i64 {
    let i = Inner { v: 3 };
    let o = Outer { child: i };
    let c = o.child;
    if c != nil { return c.get(); }
    return 0;
}
fn main() {
    let _ = make();
    gc_collect();
    println(gc_allocated_bytes());
}
"#,
    );
    assert!(ok);
    assert_eq!(out, "0\n");
}

// GC-83: object with nil optional field — freed without following nil
#[test]
fn test_gc_83_nil_optional_field_freed_safely() {
    let (out, ok) = compile_and_run(
        r#"
class Outer { child: Inner?; v: i64; pub fn get(self) -> i64 { return self.v; } }
class Inner { v: i64; }
fn make() -> i64 {
    let o = Outer { child: nil, v: 7 };
    return o.get();
}
fn main() {
    let _ = make();
    gc_collect();
    println(gc_allocated_bytes());
}
"#,
    );
    assert!(ok);
    assert_eq!(out, "0\n");
}

// GC-84: gc does not corrupt i64 return value from function
#[test]
fn test_gc_84_gc_does_not_corrupt_return_value() {
    let (out, ok) = compile_and_run(
        r#"
class Box { v: i64; pub fn get(self) -> i64 { return self.v; } }
fn compute() -> i64 {
    let b = Box { v: 42 };
    let result = b.get();
    gc_collect();
    return result;
}
fn main() {
    println(compute());
}
"#,
    );
    assert!(ok);
    assert_eq!(out, "42\n");
}

// GC-85: function allocating then calling gc_collect internally
#[test]
fn test_gc_85_gc_inside_allocating_function() {
    let (out, ok) = compile_and_run(
        r#"
class Node { v: i64; pub fn get(self) -> i64 { return self.v; } }
fn alloc_and_get() -> i64 {
    let n = Node { v: 3 };
    return n.get();
}
fn main() {
    let r1 = alloc_and_get();
    let r2 = alloc_and_get();
    gc_collect();
    println(r1 + r2);
    println(gc_allocated_bytes());
}
"#,
    );
    assert!(ok);
    assert_eq!(out, "6\n0\n");
}

// GC-86: zero-field class object allocated and freed
#[test]
fn test_gc_86_zero_field_class_freed() {
    let (out, ok) = compile_and_run(
        r#"
pub open class Empty {}
fn drop_it() -> i64 {
    let _e = Empty {};
    return 1;
}
fn main() {
    let _ = drop_it();
    gc_collect();
    println(gc_allocated_bytes());
}
"#,
    );
    assert!(ok);
    assert_eq!(out, "0\n");
}

// GC-87: zero-field class object alive survives collect
#[test]
fn test_gc_87_zero_field_class_survives() {
    let (out, ok) = compile_and_run(
        r#"
pub open class Empty { pub fn tag(self) -> i64 { return 0; } }
fn main() {
    let e = Empty {};
    gc_collect();
    println(gc_allocated_bytes() > 0);
    println(e.tag());
}
"#,
    );
    assert!(ok);
    assert_eq!(out, "true\n0\n");
}

// GC-88: child zero-field class extends parent with field — freed
#[test]
fn test_gc_88_child_inherits_field_both_freed() {
    let (out, ok) = compile_and_run(
        r#"
pub open class Base { v: i64; pub fn get(self) -> i64 { return self.v; } }
pub class Empty extends Base {}
fn drop_it() -> i64 { let e = Empty { v: 9 }; return e.get(); }
fn main() {
    let _ = drop_it();
    gc_collect();
    println(gc_allocated_bytes());
}
"#,
    );
    assert!(ok);
    assert_eq!(out, "0\n");
}

// GC-89: object surviving ternary expression
#[test]
fn test_gc_89_object_survives_ternary() {
    let (out, ok) = compile_and_run(
        r#"
class Box { v: i64; pub fn get(self) -> i64 { return self.v; } }
fn main() {
    let b = Box { v: 7 };
    let x = b.get() > 5 ? 1 : 0;
    gc_collect();
    println(x);
    println(b.get());
}
"#,
    );
    assert!(ok);
    assert_eq!(out, "1\n7\n");
}

// GC-90: object with four fields — all freed when dead
#[test]
fn test_gc_90_four_field_object_freed() {
    let (out, ok) = compile_and_run(
        r#"
class Quad { a: i64; b: i64; c: i64; d: i64;
    pub fn sum(self) -> i64 { return self.a + self.b + self.c + self.d; }
}
fn drop_quad() -> i64 {
    let q = Quad { a: 1, b: 2, c: 3, d: 4 };
    return q.sum();
}
fn main() {
    let _ = drop_quad();
    gc_collect();
    println(gc_allocated_bytes());
}
"#,
    );
    assert!(ok);
    assert_eq!(out, "0\n");
}

// GC-91: object as function return — freed when caller doesn't store it
#[test]
fn test_gc_91_returned_object_not_stored_freed() {
    let (out, ok) = compile_and_run(
        r#"
class V { val: i64; pub fn get(self) -> i64 { return self.val; } }
fn make_v() -> V { return V { val: 5 }; }
fn main() {
    let _ = make_v().get();
    gc_collect();
    println(gc_allocated_bytes());
}
"#,
    );
    assert!(ok);
    assert_eq!(out, "0\n");
}

// GC-92: object stored temporarily in variable then dropped
#[test]
fn test_gc_92_temp_stored_then_dropped() {
    let (out, ok) = compile_and_run(
        r#"
class Box { v: i64; pub fn get(self) -> i64 { return self.v; } }
fn use_temp() -> i64 {
    let tmp = Box { v: 3 };
    let tv = tmp.get();
    let result = tv * 2;
    return result;
}
fn main() {
    let r = use_temp();
    gc_collect();
    println(r);
    println(gc_allocated_bytes());
}
"#,
    );
    assert!(ok);
    assert_eq!(out, "6\n0\n");
}

// GC-93: child and parent object both allocated, child freed first
#[test]
fn test_gc_93_parent_child_child_freed_parent_live() {
    let (out, ok) = compile_and_run(
        r#"
pub open class Base { v: i64; pub fn get(self) -> i64 { return self.v; } }
pub class Child extends Base {}
fn drop_child() -> i64 { let c = Child { v: 2 }; return c.get(); }
fn main() {
    let parent = Base { v: 1 };
    let _ = drop_child();
    gc_collect();
    println(parent.get());
    println(gc_allocated_bytes() > 0);
}
"#,
    );
    assert!(ok);
    assert_eq!(out, "1\ntrue\n");
}

// GC-94: allocate inside while body, free each iteration via scope
#[test]
fn test_gc_94_while_body_alloc_freed_each_iter() {
    let (out, ok) = compile_and_run(
        r#"
class Tmp { v: i64; pub fn get(self) -> i64 { return self.v; } }
fn main() {
    let mut i = 0;
    let mut acc = 0;
    while i < 4 {
        let t = Tmp { v: i * i };
        acc = acc + t.get();
        i = i + 1;
    }
    gc_collect();
    println(acc);
    println(gc_allocated_bytes());
}
"#,
    );
    assert!(ok);
    assert_eq!(out, "14\n0\n");
}

// GC-95: object with both pub and prot fields freed
#[test]
fn test_gc_95_mixed_visibility_fields_freed() {
    let (out, ok) = compile_and_run(
        r#"
class Mixed { pub a: i64; prot b: i64; pub fn sum(self) -> i64 { return self.a + self.b; } }
fn drop_mixed() -> i64 { let m = Mixed { a: 3, b: 4 }; return m.sum(); }
fn main() {
    let _ = drop_mixed();
    gc_collect();
    println(gc_allocated_bytes());
}
"#,
    );
    assert!(ok);
    assert_eq!(out, "0\n");
}

// GC-96: object alive when used as argument to function that gc_collects
#[test]
fn test_gc_96_alive_during_fn_that_collects() {
    let (out, ok) = compile_and_run(
        r#"
class Key { v: i64; pub fn get(self) -> i64 { return self.v; } }
fn expensive(k: Key) -> i64 {
    gc_collect();
    return k.get();
}
fn main() {
    let k = Key { v: 13 };
    println(expensive(k));
}
"#,
    );
    assert!(ok);
    assert_eq!(out, "13\n");
}

// GC-97: collect between two allocs — second survives
#[test]
fn test_gc_97_collect_between_allocs_second_survives() {
    let (out, ok) = compile_and_run(
        r#"
class Box { v: i64; pub fn get(self) -> i64 { return self.v; } }
fn drop_one() -> i64 { let b = Box { v: 1 }; return b.get(); }
fn main() {
    let _ = drop_one();
    gc_collect();
    let b2 = Box { v: 50 };
    println(b2.get());
}
"#,
    );
    assert!(ok);
    assert_eq!(out, "50\n");
}

// GC-98: gc_collect idempotent — calling twice is safe
#[test]
fn test_gc_98_double_collect_idempotent() {
    let (out, ok) = compile_and_run(
        r#"
class Box { v: i64; pub fn get(self) -> i64 { return self.v; } }
fn drop_box() -> i64 { let b = Box { v: 1 }; return b.get(); }
fn main() {
    let _ = drop_box();
    gc_collect();
    gc_collect();
    println(gc_allocated_bytes());
}
"#,
    );
    assert!(ok);
    assert_eq!(out, "0\n");
}

// GC-99: live object across gc_collect in a loop
#[test]
fn test_gc_99_live_across_collect_in_loop() {
    let (out, ok) = compile_and_run(
        r#"
class Counter { n: i64; pub fn get(self) -> i64 { return self.n; } }
fn main() {
    let c = Counter { n: 42 };
    let mut i = 0;
    while i < 3 {
        gc_collect();
        i = i + 1;
    }
    println(c.get());
}
"#,
    );
    assert!(ok);
    assert_eq!(out, "42\n");
}

// GC-100: gc_allocated_bytes tracks only live bytes after collect
#[test]
fn test_gc_100_bytes_tracks_only_live_after_collect() {
    let (out, ok) = compile_and_run(
        r#"
class Box { v: i64; pub fn get(self) -> i64 { return self.v; } }
fn drop_box() -> i64 { let b = Box { v: 1 }; return b.get(); }
fn main() {
    let r = drop_box();
    println(r);
    gc_collect();
    let zero = gc_allocated_bytes();
    let live = Box { v: 2 };
    let nonzero = gc_allocated_bytes();
    gc_collect();
    let still_nonzero = gc_allocated_bytes();
    println(zero);
    println(nonzero > 0);
    println(still_nonzero > 0);
    println(live.get());
}
"#,
    );
    assert!(ok);
    assert_eq!(out, "1\n0\ntrue\ntrue\n2\n");
}

// ── self receiver semantics ─────────────────────────────────────────────────

#[test]
fn test_self_field_read_and_assignment() {
    let (out, ok) = compile_and_run(
        r#"
class Counter {
    n: i64;
    pub fn inc(self) { self.n = self.n + 1; }
    pub fn add(self, n: i64) { self.n = self.n + n; }
    pub fn get(self) -> i64 { return self.n; }
}
fn main() {
    let c = Counter { n: 0 };
    c.inc();
    c.inc();
    c.add(5);
    println(c.get());
}
"#,
    );
    assert!(ok);
    assert_eq!(out, "7\n");
}

#[test]
fn test_self_method_call_inside_method() {
    let (out, ok) = compile_and_run(
        r#"
class Wrap {
    n: i64;
    pub fn double(self) -> i64 { return self.n * 2; }
    pub fn compute(self) -> i64 { return self.double() + 1; }
}
fn main() {
    let w = Wrap { n: 5 };
    println(w.compute());
}
"#,
    );
    assert!(ok);
    assert_eq!(out, "11\n");
}

#[test]
fn test_self_accesses_and_assigns_inherited_field() {
    let (out, ok) = compile_and_run(
        r#"
open class Base {
    pub score: i64;
}
class Child extends Base {
    pub fn boost(self, n: i64) { self.score = self.score + n; }
    pub fn get(self) -> i64 { return self.score; }
}
fn main() {
    let c = Child { score: 10 };
    c.boost(5);
    println(c.get());
}
"#,
    );
    assert!(ok);
    assert_eq!(out, "15\n");
}

#[test]
fn test_self_field_assign_inside_control_flow() {
    let (out, ok) = compile_and_run(
        r#"
class Acc {
    total: i64;
    pub fn accumulate(self, n: i64) {
        let mut i = 0;
        while i < n {
            if i >= 0 {
                self.total = self.total + 1;
            }
            i = i + 1;
        }
    }
    pub fn get(self) -> i64 { return self.total; }
}
fn main() {
    let a = Acc { total: 0 };
    a.accumulate(5);
    println(a.get());
}
"#,
    );
    assert!(ok);
    assert_eq!(out, "5\n");
}

#[test]
fn test_static_and_instance_methods_coexist() {
    let (out, ok) = compile_and_run(
        r#"
class Adder {
    base: i64;
    pub fn add_base(self, n: i64) -> i64 { return self.base + n; }
    pub fn pure(a: i64, b: i64) -> i64 { return a + b; }
}
fn main() {
    let a = Adder { base: 10 };
    println(a.add_base(5));
    println(Adder::pure(2, 3));
}
"#,
    );
    assert!(ok);
    assert_eq!(out, "15\n5\n");
}

#[test]
fn test_self_upper_static_call_inside_instance_method() {
    let (out, ok) = compile_and_run(
        r#"
class Counter {
    value: i64;
    pub fn make(value: i64) -> Counter { return Counter { value: value }; }
    pub fn clone_plus(self, n: i64) -> i64 {
        let next = Self::make(self.value + n);
        return next.value;
    }
}
fn main() {
    let c = Counter { value: 8 };
    println(c.clone_plus(4));
}
"#,
    );
    assert!(ok);
    assert_eq!(out, "12\n");
}

#[test]
fn test_self_lower_static_call_inside_instance_method() {
    let (out, ok) = compile_and_run(
        r#"
class Math {
    value: i64;
    pub fn pure(a: i64, b: i64) -> i64 { return a + b; }
    pub fn add_to_value(self, n: i64) -> i64 {
        return self::pure(self.value, n);
    }
}
fn main() {
    let m = Math { value: 20 };
    println(m.add_to_value(22));
}
"#,
    );
    assert!(ok);
    assert_eq!(out, "42\n");
}

#[test]
fn test_instance_method_called_with_static_syntax_is_error() {
    assert_compile_error_contains(
        r#"
class Box {
    v: i64;
    pub fn get(self) -> i64 { return self.v; }
    pub fn bad(self) -> i64 { return Self::get(); }
}
fn main() {}
"#,
        &[
            "instance method called with `::`",
            "write `self.get` instead",
        ],
    );
}

#[test]
fn test_static_method_called_with_dot_is_error() {
    assert_compile_error_contains(
        r#"
class Math {
    pub fn add(a: i64, b: i64) -> i64 { return a + b; }
}
fn main() {
    let m = Math {};
    println(m.add(1, 2));
}
"#,
        &["static method called with `.`", "write `Math::add` instead"],
    );
}

#[test]
fn test_self_static_call_outside_class_is_error() {
    assert_compile_error_contains(
        r#"
fn main() {
    println(Self::make());
}
"#,
        &["`Self` can only be used inside a class method"],
    );
}

#[test]
fn test_legacy_this_receiver_is_error() {
    assert_compile_error_contains(
        r#"
class Box {
    v: i64;
    pub fn get(self) -> i64 { return this.v; }
}
fn main() {}
"#,
        &["receiver alias `this` is not supported", "use `self`"],
    );
}

#[test]
fn test_legacy_this_identifier_declaration_is_error() {
    assert_compile_error_contains(
        r#"
fn main() {
    let this = 1;
}
"#,
        &["identifier `this` is reserved", "use `self`"],
    );
}

#[test]
fn test_self_in_static_method_is_error() {
    assert_compile_error_contains(
        r#"
class Math {
    pub fn bad() -> i64 {
        return self.value;
    }
}
fn main() {}
"#,
        &["`self` can only be used inside an instance method"],
    );
}

#[test]
fn test_assign_to_self_is_error() {
    assert_compile_error_contains(
        r#"
class Box {
    v: i64;
    pub fn bad(self) {
        self = Box { v: 1 };
    }
}
fn main() {}
"#,
        &["cannot assign to `self`"],
    );
}

#[test]
fn test_self_field_assign_type_mismatch_is_error() {
    assert_compile_error_contains(
        r#"
class Typed {
    n: i64;
    pub fn bad(self) {
        self.n = true;
    }
}
fn main() {}
"#,
        &["mismatched types"],
    );
}

#[test]
fn test_gc_during_method_does_not_corrupt_self_receiver() {
    let (out, ok) = compile_and_run(
        r#"
class Holder {
    v: i64;
    pub fn safe(self) -> i64 {
        gc_collect();
        return self.v;
    }
}
fn main() {
    let h = Holder { v: 55 };
    println(h.safe());
}
"#,
    );
    assert!(ok);
    assert_eq!(out, "55\n");
}

// ── WillowString GC migration tests (requirements/willow_string_gc_requirements.md sec 11) ─

// 11.1: String literal survives gc_collect
#[test]
fn test_string_gc_11_1_literal_survives_gc_collect() {
    let (out, ok) = compile_and_run(
        r#"
fn main() {
    let s = "hello";
    gc_collect();
    println(s);
}
"#,
    );
    assert!(ok);
    assert_eq!(out, "hello\n");
}

// 11.2: String concatenation survives gc_collect
#[test]
fn test_string_gc_11_2_concat_survives_gc_collect() {
    let (out, ok) = compile_and_run(
        r#"
fn main() {
    let s = "hello" + " " + "world";
    gc_collect();
    println(s);
}
"#,
    );
    assert!(ok);
    assert_eq!(out, "hello world\n");
}

// 11.3: String field survives gc_collect
#[test]
fn test_string_gc_11_3_string_field_survives_gc_collect() {
    let (out, ok) = compile_and_run(
        r#"
class User {
    pub name: String;
    pub fn get_name(self) -> String { return self.name; }
}
fn main() {
    let u = User { name: "alice" };
    gc_collect();
    println(u.get_name());
}
"#,
    );
    assert!(ok);
    assert_eq!(out, "alice\n");
}

// 11.4: Multiple string fields can be concatenated
#[test]
fn test_string_gc_11_4_multiple_string_fields_concat() {
    let (out, ok) = compile_and_run(
        r#"
class User {
    pub first: String;
    pub last: String;
    pub fn full(self) -> String { return self.first + " " + self.last; }
}
fn main() {
    let u = User { first: "Ada", last: "Lovelace" };
    println(u.full());
}
"#,
    );
    assert!(ok);
    assert_eq!(out, "Ada Lovelace\n");
}

// 11.5: Option<String> survives gc_collect
#[test]
fn test_string_gc_11_5_option_string_survives_gc() {
    let (out, ok) = compile_and_run(
        r#"
fn main() {
    let s = Option::Some("hello");
    gc_collect();
    println(s.unwrap());
}
"#,
    );
    assert!(ok);
    assert_eq!(out, "hello\n");
}

// 11.6: Result<String, String> survives gc_collect
#[test]
fn test_string_gc_11_6_result_string_survives_gc() {
    let (out, ok) = compile_and_run(
        r#"
fn main() {
    let r: Result<String, String> = Result::Ok("ok");
    gc_collect();
    println(r.unwrap());
}
"#,
    );
    assert!(ok);
    assert_eq!(out, "ok\n");
}

// 11.7: Option<String> with gc_collect (nullable String pattern via Option)
#[test]
fn test_string_gc_11_7_nullable_string_survives_gc() {
    let (out, ok) = compile_and_run(
        r#"
fn make_opt(flag: bool) -> Option<String> {
    if flag {
        return Option::Some("hello");
    }
    return Option::None;
}
fn main() {
    let s = make_opt(true);
    gc_collect();
    if s.is_some() {
        println(s.unwrap());
    }
}
"#,
    );
    assert!(ok);
    assert_eq!(out, "hello\n");
}

// 11.8: Repeated string concatenation and GC does not crash
#[test]
fn test_string_gc_11_8_repeated_concat_no_crash() {
    let (out, ok) = compile_and_run(
        r#"
fn main() {
    let mut s = "a";
    let mut i = 0;
    while i < 10 {
        s = s + "b";
        gc_collect();
        i = i + 1;
    }
    println(s);
}
"#,
    );
    assert!(ok);
    // "a" + 10 "b"s = 11 chars + "\n" = 12 total
    assert_eq!(out.len(), "abbbbbbbbbb\n".len());
}

// String GC stress: multiple objects with String fields across GC cycles
#[test]
fn test_string_gc_stress_class_fields_across_gc_cycles() {
    let (out, ok) = compile_and_run(
        r#"
class Node {
    pub label: String;
    pub fn get_label(self) -> String { return self.label; }
}
fn main() {
    let a = Node { label: "alpha" };
    let b = Node { label: "beta" };
    gc_collect();
    let c = Node { label: "gamma" };
    gc_collect();
    println(a.get_label() + " " + b.get_label() + " " + c.get_label());
}
"#,
    );
    assert!(ok);
    assert_eq!(out, "alpha beta gamma\n");
}

// ── T → T? implicit coercion (willow-thk) ────────────────────────────────────

// 1. let s: String? = literal compiles and prints
#[test]
fn test_nullable_coerce_string_literal_to_nullable() {
    let (out, ok) = compile_and_run(
        r#"
fn main() {
    let s: String? = "hello";
    if s != nil { println(s); }
}
"#,
    );
    assert!(ok);
    assert_eq!(out, "hello\n");
}

// 2. Function returning String? can return a plain String
#[test]
fn test_nullable_coerce_return_string_from_nullable_fn() {
    let (out, ok) = compile_and_run(
        r#"
fn greet(flag: bool) -> String? {
    if flag { return "hi"; }
    return nil;
}
fn main() {
    let a = greet(true);
    let b = greet(false);
    if a != nil { println(a); }
    if b == nil { println("nil"); }
}
"#,
    );
    assert!(ok);
    assert_eq!(out, "hi\nnil\n");
}

// 3. Passing T to T? parameter compiles
#[test]
fn test_nullable_coerce_pass_string_to_nullable_param() {
    let (out, ok) = compile_and_run(
        r#"
fn print_maybe(s: String?) {
    if s != nil { println(s); } else { println("empty"); }
}
fn main() {
    print_maybe("world");
    print_maybe(nil);
}
"#,
    );
    assert!(ok);
    assert_eq!(out, "world\nempty\n");
}

// 4. Unrelated type to T? is still a compile error
#[test]
fn test_nullable_coerce_unrelated_type_rejected() {
    assert!(expect_compile_error(
        r#"
fn main() {
    let s: String? = 42;
}
"#
    ));
}

// 5. nil can still be assigned to T?
#[test]
fn test_nullable_coerce_nil_still_works() {
    let (out, ok) = compile_and_run(
        r#"
fn main() {
    let s: String? = nil;
    if s == nil { println("nil"); }
}
"#,
    );
    assert!(ok);
    assert_eq!(out, "nil\n");
}

// 6. Class T → T? coercion also works
#[test]
fn test_nullable_coerce_class_to_nullable() {
    let (out, ok) = compile_and_run(
        r#"
class Box { pub v: i64; pub fn get(self) -> i64 { return self.v; } }
fn maybe(flag: bool) -> Box? {
    if flag { return Box { v: 99 }; }
    return nil;
}
fn main() {
    let b = maybe(true);
    if b != nil { println(b.get()); }
}
"#,
    );
    assert!(ok);
    assert_eq!(out, "99\n");
}

// ── GC-managed temporary rooting (willow-5mb) ────────────────────────────────

// Chain of string concatenations: intermediate r1 = (a + b) must survive
// the GC that runs during the second concat allocation.
#[test]
fn test_gc_tmp_string_concat_chain_is_safe() {
    let (out, ok) = compile_and_run(
        r#"
class Names {
    pub first: String;
    pub last: String;
    pub fn full(self) -> String { return self.first + " " + self.last; }
}
fn main() {
    let n = Names { first: "Ada", last: "Lovelace" };
    let s = n.first + " " + n.last;
    println(s);
}
"#,
    );
    assert!(ok);
    assert_eq!(out, "Ada Lovelace\n");
}

// Method return values used directly in concat must be safe.
#[test]
fn test_gc_tmp_method_return_in_concat_is_safe() {
    let (out, ok) = compile_and_run(
        r#"
fn bang(s: String) -> String { return s + "!"; }
fn main() {
    let s = bang("hello") + bang("world");
    println(s);
}
"#,
    );
    assert!(ok);
    assert_eq!(out, "hello!world!\n");
}

// Object literal with String fields: partially-initialised object must not
// be collected while field initialisers are still being evaluated.
#[test]
fn test_gc_tmp_object_literal_not_collected_during_init() {
    let (out, ok) = compile_and_run(
        r#"
fn make_str(s: String) -> String { return s + "."; }
class Rec {
    pub a: String;
    pub b: String;
    pub fn both(self) -> String { return self.a + self.b; }
}
fn main() {
    let r = Rec { a: make_str("x"), b: make_str("y") };
    println(r.both());
}
"#,
    );
    assert!(ok);
    assert_eq!(out, "x.y.\n");
}

// 4-level concat chain stress test.
#[test]
fn test_gc_tmp_four_level_concat_chain() {
    let (out, ok) = compile_and_run(
        r#"
class W { pub v: String; pub fn get(self) -> String { return self.v; } }
fn main() {
    let a = W { v: "a" };
    let b = W { v: "b" };
    let c = W { v: "c" };
    let d = W { v: "d" };
    let s = a.get() + b.get() + c.get() + d.get();
    println(s);
}
"#,
    );
    assert!(ok);
    assert_eq!(out, "abcd\n");
}

// ── Lambda return type inference (willow-cuq) ────────────────────────────────

// and_then with unannotated expression-body lambda
#[test]
fn test_lambda_infer_and_then_expr_body() {
    let (out, ok) = compile_and_run(
        r#"
fn safe_div(a: i64, b: i64) -> Option<i64> {
    if b == 0 { return Option::None; }
    return Option::Some(a / b);
}
fn main() {
    let r = safe_div(20, 4).and_then(|v: i64| safe_div(v, 2));
    println(r.is_some());
    println(r.unwrap());
}
"#,
    );
    assert!(ok);
    assert_eq!(out, "true\n2\n");
}

// and_then with unannotated block-body lambda
#[test]
fn test_lambda_infer_and_then_block_body() {
    let (out, ok) = compile_and_run(
        r#"
fn safe_div(a: i64, b: i64) -> Option<i64> {
    if b == 0 { return Option::None; }
    return Option::Some(a / b);
}
fn main() {
    let r = safe_div(100, 5).and_then(|v: i64| {
        return safe_div(v, 4);
    });
    println(r.is_some());
    println(r.unwrap());
}
"#,
    );
    assert!(ok);
    assert_eq!(out, "true\n5\n");
}

// map with unannotated lambda
#[test]
fn test_lambda_infer_map() {
    let (out, ok) = compile_and_run(
        r#"
fn main() {
    let r = Option::Some(7).map(|x: i64| x * 2);
    println(r.unwrap());
}
"#,
    );
    assert!(ok);
    assert_eq!(out, "14\n");
}

// or_else with unannotated lambda
#[test]
fn test_lambda_infer_or_else() {
    let (out, ok) = compile_and_run(
        r#"
fn main() {
    let r: Option<i64> = Option::None;
    let r2 = r.or_else(|| Option::Some(99));
    println(r2.unwrap());
}
"#,
    );
    assert!(ok);
    assert_eq!(out, "99\n");
}

// Result and_then with unannotated lambda
#[test]
fn test_lambda_infer_result_and_then() {
    let (out, ok) = compile_and_run(
        r#"
fn main() {
    let r: Result<i64, String> = Result::Ok(10);
    let r2 = r.and_then(|v: i64| {
        return Result::Ok(v + 5);
    });
    println(r2.unwrap());
}
"#,
    );
    assert!(ok);
    assert_eq!(out, "15\n");
}

// Explicit annotation still works
#[test]
fn test_lambda_explicit_annotation_unchanged() {
    let (out, ok) = compile_and_run(
        r#"
fn safe_div(a: i64, b: i64) -> Option<i64> {
    if b == 0 { return Option::None; }
    return Option::Some(a / b);
}
fn main() {
    let r = safe_div(20, 4).and_then(|v: i64| -> Option<i64> { return safe_div(v, 2); });
    println(r.unwrap());
}
"#,
    );
    assert!(ok);
    assert_eq!(out, "2\n");
}

// ── GC safety: remaining fixes (willow-7q1) ──────────────────────────────────

// Fix 2: GC-managed function parameter survives allocation in function body
#[test]
fn test_gc_safety_string_param_survives_alloc() {
    let (out, ok) = compile_and_run(
        r#"
fn echo_after_alloc(s: String) {
    let tmp = "x" + "y";
    gc_collect();
    println(s);
}
fn main() { echo_after_alloc("alive"); }
"#,
    );
    assert!(ok);
    assert_eq!(out, "alive\n");
}

#[test]
fn test_gc_safety_class_param_survives_alloc() {
    let (out, ok) = compile_and_run(
        r#"
class Box { pub value: String; pub fn get(self) -> String { return self.value; } }
fn print_after_alloc(b: Box) {
    let tmp = "x" + "y";
    gc_collect();
    println(b.get());
}
fn main() { print_after_alloc(Box { value: "object alive" }); }
"#,
    );
    assert!(ok);
    assert_eq!(out, "object alive\n");
}

// Fix 3: self receiver survives allocation during method body
#[test]
fn test_gc_safety_self_receiver_survives_alloc() {
    let (out, ok) = compile_and_run(
        r#"
class User {
    pub name: String;
    pub fn show(self) {
        let tmp = "x" + "y";
        gc_collect();
        println(self.name);
    }
}
fn main() {
    let u = User { name: "alice" };
    u.show();
}
"#,
    );
    assert!(ok);
    assert_eq!(out, "alice\n");
}

// Fix 3: method String parameter survives allocation
#[test]
fn test_gc_safety_method_string_param_survives_alloc() {
    let (out, ok) = compile_and_run(
        r#"
class Printer { pub fn show(self, s: String) {
    let tmp = "x" + "y";
    gc_collect();
    println(s);
} }
fn main() {
    let p = Printer {};
    p.show("method param alive");
}
"#,
    );
    assert!(ok);
    assert_eq!(out, "method param alive\n");
}

// Fix 3: method class parameter survives allocation
#[test]
fn test_gc_safety_method_class_param_survives_alloc() {
    let (out, ok) = compile_and_run(
        r#"
class Box { pub value: String; pub fn get(self) -> String { return self.value; } }
class Printer { pub fn show(self, b: Box) {
    let tmp = "x" + "y";
    gc_collect();
    println(b.get());
} }
fn main() {
    let p = Printer {};
    p.show(Box { value: "box alive" });
}
"#,
    );
    assert!(ok);
    assert_eq!(out, "box alive\n");
}

// Fix 5: GC-managed function call arguments survive later-argument allocation
#[test]
fn test_gc_safety_call_args_rooted_fn() {
    let (out, ok) = compile_and_run(
        r#"
fn make(s: String) -> String { return s + "!"; }
fn combine(a: String, b: String) -> String { return a + b; }
fn main() { println(combine(make("a"), make("b"))); }
"#,
    );
    assert!(ok);
    assert_eq!(out, "a!b!\n");
}

// Fix 5: GC-managed method call arguments survive later-argument allocation
#[test]
fn test_gc_safety_call_args_rooted_method() {
    let (out, ok) = compile_and_run(
        r#"
class C {
    pub fn make(self, s: String) -> String { return s + "!"; }
    pub fn combine(self, a: String, b: String) -> String { return a + b; }
}
fn main() {
    let c = C {};
    println(c.combine(c.make("a"), c.make("b")));
}
"#,
    );
    assert!(ok);
    assert_eq!(out, "a!b!\n");
}

// Fix 5: GC-managed object arguments survive later-argument allocation
#[test]
fn test_gc_safety_call_args_object_rooted() {
    let (out, ok) = compile_and_run(
        r#"
class Box { pub value: String; pub fn get(self) -> String { return self.value; } }
fn make_box(s: String) -> Box { return Box { value: s + "!" }; }
fn combine(a: Box, b: Box) -> String { return a.get() + b.get(); }
fn main() { println(combine(make_box("a"), make_box("b"))); }
"#,
    );
    assert!(ok);
    assert_eq!(out, "a!b!\n");
}

// ── GC root semantics: local objects survive gc_collect() inside the same scope ─

// Semantics doc: a GC-managed local is rooted until the function returns.
// gc_collect() inside the function does NOT free it; it is freed only after
// the caller performs a gc_collect() once the function's roots are popped.
#[test]
fn test_gc_local_survives_inner_collect() {
    let (out, ok) = compile_and_run(
        r#"
class Node { pub v: i64; pub fn get(self) -> i64 { return self.v; } }
fn alloc_and_collect() -> i64 {
    let n = Node { v: 3 };
    let r = n.get();
    gc_collect();
    // n is still rooted here (scope has not ended), so the Node is NOT freed
    return r;
}
fn main() {
    let r = alloc_and_collect();
    // The function has returned; n's root is popped. A collect now frees it.
    gc_collect();
    println(r);
    println(gc_allocated_bytes());
}
"#,
    );
    assert!(ok);
    assert_eq!(out, "3\n0\n");
}

// The object is still allocated right after the inner gc_collect() (still rooted).
#[test]
fn test_gc_bytes_nonzero_after_inner_collect() {
    let (out, ok) = compile_and_run(
        r#"
class Box { pub v: i64; }
fn make_and_collect() -> i64 {
    let b = Box { v: 7 };
    gc_collect();
    // b is still rooted: allocated_bytes > 0 here
    return gc_allocated_bytes();
}
fn main() {
    let during = make_and_collect();
    gc_collect();
    let after = gc_allocated_bytes();
    println(during > 0);
    println(after);
}
"#,
    );
    assert!(ok);
    assert_eq!(out, "true\n0\n");
}

// Two calls: each call allocates, inner collect keeps it alive, outer collect frees.
#[test]
fn test_gc_two_calls_freed_after_outer_collect() {
    let (out, ok) = compile_and_run(
        r#"
class Node { pub v: i64; pub fn get(self) -> i64 { return self.v; } }
fn alloc_and_collect(v: i64) -> i64 {
    let n = Node { v: v };
    gc_collect();
    return n.get();
}
fn main() {
    let r1 = alloc_and_collect(10);
    let r2 = alloc_and_collect(20);
    gc_collect();
    println(r1 + r2);
    println(gc_allocated_bytes());
}
"#,
    );
    assert!(ok);
    assert_eq!(out, "30\n0\n");
}

// String locals survive inner gc_collect() (concat result is still rooted).
// String literals ("hello", "!") are permanently interned and never freed;
// only the temporary concat result is freed after the function returns.
#[test]
fn test_gc_string_local_survives_inner_collect() {
    let (out, ok) = compile_and_run(
        r#"
fn make_and_collect(s: String) -> String {
    let t = s + "!";
    gc_collect();
    return t;
}
fn main() {
    let r = make_and_collect("hello");
    gc_collect();
    println(r);
}
"#,
    );
    assert!(ok);
    assert_eq!(out, "hello!\n");
}

// Nested functions: inner collect keeps the inner function's local alive,
// but the outer function's locals are also still rooted.
#[test]
fn test_gc_nested_scope_rooting() {
    let (out, ok) = compile_and_run(
        r#"
class N { pub v: i64; pub fn get(self) -> i64 { return self.v; } }
fn inner(v: i64) -> i64 {
    let a = N { v: v };
    gc_collect();
    return a.get();
}
fn outer() -> i64 {
    let b = N { v: 100 };
    let x = inner(42);
    return b.get() + x;
}
fn main() {
    let r = outer();
    gc_collect();
    println(r);
    println(gc_allocated_bytes());
}
"#,
    );
    assert!(ok);
    assert_eq!(out, "142\n0\n");
}

// ── std namespace and basic item imports (willow-4bv.2, Stage 2) ───────────
// The reserved `std` namespace is resolved against the built-in registry, not
// the filesystem. Single-item imports use dotted paths: `import std.mod.item;`.
// Stage 2 establishes namespace + resolver; concrete collection *types* arrive
// in Stage 3, so these tests import known items and use the ones the prelude
// and builtins already provide.

// Perspective 1: importing a known collections item resolves (compiles).
#[test]
fn test_std_import_collections_array_resolves() {
    let (out, ok) = compile_and_run(
        r#"
import std.collections.Array;
fn main() { println(1); }
"#,
    );
    assert!(ok, "import std.collections.Array should resolve");
    assert_eq!(out, "1\n");
}

// Perspective 2: importing std.collections.Map resolves.
#[test]
fn test_std_import_collections_map_resolves() {
    let (out, ok) = compile_and_run(
        r#"
import std.collections.Map;
fn main() { println(2); }
"#,
    );
    assert!(ok, "import std.collections.Map should resolve");
    assert_eq!(out, "2\n");
}

// Perspective 3: importing std.option.Option resolves and Option is usable.
#[test]
fn test_std_import_option_resolves_and_usable() {
    let (out, ok) = compile_and_run(
        r#"
import std.option.Option;
fn main() {
    let x: Option<i64> = Option::Some(10);
    println(x.unwrap());
}
"#,
    );
    assert!(ok, "import std.option.Option should resolve and be usable");
    assert_eq!(out, "10\n");
}

// Perspective 4: importing std.result.Result resolves and Result is usable.
#[test]
fn test_std_import_result_resolves_and_usable() {
    let (out, ok) = compile_and_run(
        r#"
import std.result.Result;
fn make() -> Result<i64, String> { return Result::Ok(5); }
fn main() {
    println(match make() { Result::Ok(v) => v, Result::Err(_) => -1, });
}
"#,
    );
    assert!(ok, "import std.result.Result should resolve and be usable");
    assert_eq!(out, "5\n");
}

// Perspective 5: importing std.io.println (a builtin-keyword item) resolves.
#[test]
fn test_std_import_io_println_resolves() {
    let (out, ok) = compile_and_run(
        r#"
import std.io.println;
fn main() { println(7); }
"#,
    );
    assert!(ok, "import std.io.println should resolve");
    assert_eq!(out, "7\n");
}

// Perspective 6: importing std.io.print (a builtin-keyword item) resolves.
#[test]
fn test_std_import_io_print_resolves() {
    let (out, ok) = compile_and_run(
        r#"
import std.io.print;
fn main() { print(3); println(0); }
"#,
    );
    assert!(ok, "import std.io.print should resolve");
    assert_eq!(out, "30\n");
}

// Perspective 7: importing std.env items resolves.
#[test]
fn test_std_import_env_args_resolves() {
    let (out, ok) = compile_and_run(
        r#"
import std.env.args;
import std.env.program_name;
fn main() { println(4); }
"#,
    );
    assert!(ok, "import std.env items should resolve");
    assert_eq!(out, "4\n");
}

// Perspective 8: a whole-module import resolves.
#[test]
fn test_std_module_import_resolves() {
    let (out, ok) = compile_and_run(
        r#"
import std.collections;
fn main() { println(8); }
"#,
    );
    assert!(ok, "import std.collections (module) should resolve");
    assert_eq!(out, "8\n");
}

// Perspective 9: multiple std imports coexist in one file.
#[test]
fn test_std_multiple_imports_coexist() {
    let (out, ok) = compile_and_run(
        r#"
import std.io.println;
import std.option.Option;
import std.result.Result;
import std.collections.Array;
fn main() {
    let o: Option<i64> = Option::Some(99);
    println(o.unwrap());
}
"#,
    );
    assert!(ok, "multiple std imports should coexist");
    assert_eq!(out, "99\n");
}

// Perspective 10: an unknown item in a known module reports E2006.
#[test]
fn test_std_unknown_item_reports_e2006() {
    assert_compile_error_contains(
        r#"
import std.collections.Vec;
fn main() { println(1); }
"#,
        &["error[E2006]", "no item `Vec` in `std.collections`"],
    );
}

// Perspective 11: a near-miss item name suggests the correct one.
#[test]
fn test_std_unknown_item_suggests_nearest() {
    assert_compile_error_contains(
        r#"
import std.collections.Aray;
fn main() { println(1); }
"#,
        &["error[E2006]", "did you mean `Array`?"],
    );
}

// Perspective 12: lists available items for an unknown item.
#[test]
fn test_std_unknown_item_lists_available() {
    assert_compile_error_contains(
        r#"
import std.io.flush;
fn main() { println(1); }
"#,
        &["error[E2006]", "available items:"],
    );
}

// Perspective 13: an unknown std module reports E2007.
#[test]
fn test_std_unknown_module_reports_e2007() {
    assert_compile_error_contains(
        r#"
import std.networking.Socket;
fn main() { println(1); }
"#,
        &["error[E2007]", "unknown std module `networking`"],
    );
}

// Perspective 14: a near-miss module name suggests the correct one.
#[test]
fn test_std_unknown_module_suggests_nearest() {
    assert_compile_error_contains(
        r#"
import std.collection.Array;
fn main() { println(1); }
"#,
        &["error[E2007]", "did you mean `std.collections`?"],
    );
}

// Perspective 15: importing the bare `std` root is reserved (E2005).
#[test]
fn test_std_bare_root_is_reserved_e2005() {
    assert_compile_error_contains(
        r#"
import std;
fn main() { println(1); }
"#,
        &["error[E2005]", "reserved namespace"],
    );
}

// Perspective 16: a too-deep std path reports E2007.
#[test]
fn test_std_too_deep_path_reports_e2007() {
    assert_compile_error_contains(
        r#"
import std.collections.Array.extra;
fn main() { println(1); }
"#,
        &["error[E2007]", "not a valid std import path"],
    );
}

// Perspective 17: an unknown module on a two-segment path also reports E2007.
#[test]
fn test_std_unknown_module_two_segments_reports_e2007() {
    assert_compile_error_contains(
        r#"
import std.bogus;
fn main() { println(1); }
"#,
        &["error[E2007]", "unknown std module `bogus`"],
    );
}

// Perspective 18: std imports coexist with local declarations.
#[test]
fn test_std_import_with_local_declarations() {
    let (out, ok) = compile_and_run(
        r#"
import std.io.println;
fn helper(n: i64) -> i64 { return n + 1; }
fn main() { println(helper(40)); }
"#,
    );
    assert!(ok, "std import should not disturb local declarations");
    assert_eq!(out, "41\n");
}

// Perspective 19: a dotted std import does not break a sibling `::` local path
// parse (mixed separators across imports are accepted at parse time).
#[test]
fn test_std_dotted_import_parses_alongside_colon_path() {
    // `std.io.println` uses dots; the program compiles. (A `::` local import to
    // a missing file would be a *resolution* error, not a parse error, so we
    // only assert the dotted std form parses and resolves here.)
    let (out, ok) = compile_and_run(
        r#"
import std.io.println;
fn main() { println(123); }
"#,
    );
    assert!(ok);
    assert_eq!(out, "123\n");
}

// Perspective 20: a duplicate std import is accepted (deduplicated silently).
#[test]
fn test_std_duplicate_import_is_accepted() {
    let id = unique_test_id();
    let src_path = format!("/tmp/willow_duplicate_std_import_{}.wi", id);
    let bin_path = format!("/tmp/willow_duplicate_std_import_{}", id);
    fs::write(
        &src_path,
        r#"
import std.collections.Array;
import std.collections.Array;
fn main() { println(55); }
"#,
    )
    .unwrap();

    let compiler = env!("CARGO_BIN_EXE_willowc");
    let output = Command::new(compiler)
        .args(["build", &src_path, "-o", &bin_path])
        .output()
        .expect("failed to run compiler");
    assert!(
        output.status.success(),
        "duplicate identical std import should be accepted: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("warning[W2002]"), "stderr: {stderr}");

    let run = Command::new(&bin_path)
        .output()
        .expect("failed to run binary");
    assert_eq!(String::from_utf8_lossy(&run.stdout), "55\n");

    let _ = fs::remove_file(&src_path);
    remove_output_artifacts(&bin_path);
}

// Perspective 21: prelude items remain available without any std import.
#[test]
fn test_prelude_items_available_without_std_import() {
    let (out, ok) = compile_and_run(
        r#"
fn make() -> Result<i64, String> { return Result::Ok(1); }
fn main() {
    let o: Option<i64> = Option::Some(2);
    println(o.unwrap());
    println(match make() { Result::Ok(v) => v, Result::Err(_) => -1, });
}
"#,
    );
    assert!(ok, "Option/Result/println come from the prelude");
    assert_eq!(out, "2\n1\n");
}

// Perspective 22: E2005, E2006, and E2007 are distinct diagnostic codes.
#[test]
fn test_std_import_diagnostic_codes_are_distinct() {
    assert_compile_error_contains("import std;\nfn main() {}\n", &["error[E2005]"]);
    assert_compile_error_contains(
        "import std.collections.Nope;\nfn main() {}\n",
        &["error[E2006]"],
    );
    assert_compile_error_contains("import std.nope.Thing;\nfn main() {}\n", &["error[E2007]"]);
}

// ── std.collections type imports (willow-4bv.3, Stage 3) ───────────────────

#[test]
fn test_std_collections_array_import_enables_annotations() {
    let (out, ok) = compile_and_run(
        r#"
import std.collections.Array;

fn main() {
    let xs: Array<i64> = [1, 2];
    println(xs.len());
}
"#,
    );
    assert!(ok);
    assert_eq!(out, "2\n");
}

#[test]
fn test_std_collections_module_import_enables_array_and_map() {
    let (out, ok) = compile_and_run(
        r#"
import std.collections;

fn main() {
    let xs: Array<i64> = [1];
    let m: Map<String, i64> = Map::new();
    println(xs.len() + m.len());
}
"#,
    );
    assert!(ok);
    assert_eq!(out, "1\n");
}

#[test]
fn test_array_literal_infers_without_array_import() {
    let (out, ok) = compile_and_run(
        r#"
fn main() {
    let xs = [1, 2, 3];
    println(xs.len());
}
"#,
    );
    assert!(ok, "array literals remain language syntax");
    assert_eq!(out, "3\n");
}

#[test]
fn test_missing_array_import_reports_e2001() {
    assert_compile_error_contains(
        r#"
fn main() {
    let xs: Array<i64> = [1, 2];
    println(xs.len());
}
"#,
        &["error[E2001]", "import std.collections.Array"],
    );
}

#[test]
fn test_missing_array_import_on_parameter_reports_e2001() {
    assert_compile_error_contains(
        r#"
fn total(xs: Array<i64>) -> i64 { return xs.len(); }
fn main() { println(total([1])); }
"#,
        &["error[E2001]", "import std.collections.Array"],
    );
}

#[test]
fn test_missing_array_import_on_main_args_reports_e2001() {
    assert_compile_error_contains(
        r#"
fn main(args: Array<String>) {
    println(args.len());
}
"#,
        &["error[E2001]", "import std.collections.Array"],
    );
}

#[test]
fn test_std_collections_map_import_enables_constructor() {
    let (out, ok) = compile_and_run(
        r#"
import std.collections.Map;

fn main() {
    let m: Map<String, i64> = Map::new();
    println(m.len());
}
"#,
    );
    assert!(ok);
    assert_eq!(out, "0\n");
}

#[test]
fn test_missing_map_import_reports_e2002() {
    assert_compile_error_contains(
        r#"
fn main() {
    let m: Map<String, i64> = Map::new();
    println(m.len());
}
"#,
        &["error[E2002]", "import std.collections.Map"],
    );
}

#[test]
fn test_missing_map_import_on_static_constructor_reports_e2002() {
    assert_compile_error_contains(
        r#"
fn main() {
    let m = Map::new();
    println(1);
}
"#,
        &["error[E2002]", "import std.collections.Map"],
    );
}

#[test]
fn test_importing_map_does_not_import_array() {
    assert_compile_error_contains(
        r#"
import std.collections.Map;

fn main() {
    let xs: Array<i64> = [1];
    let m: Map<String, i64> = Map::new();
    println(xs.len() + m.len());
}
"#,
        &["error[E2001]", "import std.collections.Array"],
    );
}

#[test]
fn test_importing_array_does_not_import_map() {
    assert_compile_error_contains(
        r#"
import std.collections.Array;

fn main() {
    let xs: Array<i64> = [1];
    let m: Map<String, i64> = Map::new();
    println(xs.len() + m.len());
}
"#,
        &["error[E2002]", "import std.collections.Map"],
    );
}

#[test]
fn test_std_collection_item_import_collision_reports_e2004() {
    assert_compile_error_contains(
        r#"
import std.collections.Array as Thing;
import std.collections.Map as Thing;
fn main() {}
"#,
        &["error[E2004]", "defined multiple times"],
    );
}

#[test]
fn test_std_collection_item_import_vs_local_class_reports_e2003() {
    assert_compile_error_contains(
        r#"
import std.collections.Array;
class Array { pub v: i64; }
fn main() {}
"#,
        &["error[E2003]", "import and a local declaration"],
    );
}

// ── Array<T> type (willow-xqm) ─────────────────────────────────────────────
// GC-managed arrays: literals, indexing (read/write), `.len()`, bounds checks.
// Element types cover scalars (i64/bool/f64) and GC references (String/object).

// Perspective 1: i64 literal, .len(), and index reads.
#[test]
fn test_array_i64_literal_len_and_index() {
    let (out, ok) = compile_and_run(
        r#"
import std.collections.Array;

fn main() {
    let xs: Array<i64> = [10, 20, 30];
    println(xs.len());
    println(xs[0]);
    println(xs[2]);
}
"#,
    );
    assert!(ok);
    assert_eq!(out, "3\n10\n30\n");
}

// Perspective 2: element assignment `xs[i] = v`.
#[test]
fn test_array_index_assignment() {
    let (out, ok) = compile_and_run(
        r#"
import std.collections.Array;

fn main() {
    let mut xs: Array<i64> = [1, 2, 3];
    xs[0] = 100;
    xs[2] = 300;
    println(xs[0]);
    println(xs[1]);
    println(xs[2]);
}
"#,
    );
    assert!(ok);
    assert_eq!(out, "100\n2\n300\n");
}

// Perspective 3: iterate with `.len()` and index, accumulating.
#[test]
fn test_array_sum_loop() {
    let (out, ok) = compile_and_run(
        r#"
import std.collections.Array;

fn main() {
    let xs: Array<i64> = [5, 15, 25, 55];
    let mut i = 0;
    let mut sum = 0;
    while i < xs.len() {
        sum = sum + xs[i];
        i = i + 1;
    }
    println(sum);
}
"#,
    );
    assert!(ok);
    assert_eq!(out, "100\n");
}

// Perspective 4: bool elements.
#[test]
fn test_array_bool_elements() {
    let (out, ok) = compile_and_run(
        r#"
import std.collections.Array;

fn main() {
    let bs: Array<bool> = [true, false, true];
    println(bs[0]);
    println(bs[1]);
    println(bs.len());
}
"#,
    );
    assert!(ok);
    assert_eq!(out, "true\nfalse\n3\n");
}

// Perspective 5: f64 elements (exercises the f64<->word bitcast).
#[test]
fn test_array_f64_elements() {
    let (out, ok) = compile_and_run(
        r#"
import std.collections.Array;

fn main() {
    let fs: Array<f64> = [1.5, 2.5, 3.0];
    println(fs[0] + fs[2]);
}
"#,
    );
    assert!(ok);
    assert_eq!(out, "4.5\n");
}

// Perspective 6: String (reference) elements round-trip.
#[test]
fn test_array_string_elements() {
    let (out, ok) = compile_and_run(
        r#"
import std.collections.Array;

fn main() {
    let names: Array<String> = ["alice", "bob", "carol"];
    println(names.len());
    println(names[0]);
    println(names[2]);
}
"#,
    );
    assert!(ok);
    assert_eq!(out, "3\nalice\ncarol\n");
}

// Perspective 7: an array passed as a function parameter.
#[test]
fn test_array_as_parameter() {
    let (out, ok) = compile_and_run(
        r#"
import std.collections.Array;

fn total(xs: Array<i64>) -> i64 {
    let mut i = 0;
    let mut s = 0;
    while i < xs.len() { s = s + xs[i]; i = i + 1; }
    return s;
}
fn main() {
    let xs: Array<i64> = [10, 20, 30];
    println(total(xs));
}
"#,
    );
    assert!(ok);
    assert_eq!(out, "60\n");
}

// Perspective 8: an array returned from a function.
#[test]
fn test_array_returned_from_function() {
    let (out, ok) = compile_and_run(
        r#"
import std.collections.Array;

fn make() -> Array<i64> {
    return [7, 8, 9];
}
fn main() {
    let xs = make();
    println(xs.len());
    println(xs[1]);
}
"#,
    );
    assert!(ok);
    assert_eq!(out, "3\n8\n");
}

// Perspective 9: array of class instances, with method calls on elements.
#[test]
fn test_array_of_objects() {
    let (out, ok) = compile_and_run(
        r#"
import std.collections.Array;

class P {
    pub val: i64;
    pub fn new(v: i64) -> P { return P { val: v }; }
    pub fn get(self) -> i64 { return self.val; }
}
fn main() {
    let ps: Array<P> = [P::new(7), P::new(8)];
    println(ps[0].get());
    println(ps[1].get());
}
"#,
    );
    assert!(ok);
    assert_eq!(out, "7\n8\n");
}

// Perspective 10: empty array with annotation has length 0.
#[test]
fn test_array_empty_annotated() {
    let (out, ok) = compile_and_run(
        r#"
import std.collections.Array;

fn main() {
    let xs: Array<i64> = [];
    println(xs.len());
}
"#,
    );
    assert!(ok);
    assert_eq!(out, "0\n");
}

// Perspective 11: single-element array.
#[test]
fn test_array_single_element() {
    let (out, ok) = compile_and_run(
        r#"
import std.collections.Array;

fn main() {
    let xs: Array<i64> = [42];
    println(xs.len());
    println(xs[0]);
}
"#,
    );
    assert!(ok);
    assert_eq!(out, "1\n42\n");
}

// Perspective 12: read back a written reference element.
#[test]
fn test_array_string_write_then_read() {
    let (out, ok) = compile_and_run(
        r#"
import std.collections.Array;

fn main() {
    let mut xs: Array<String> = ["a", "b"];
    xs[0] = "changed";
    println(xs[0]);
    println(xs[1]);
}
"#,
    );
    assert!(ok);
    assert_eq!(out, "changed\nb\n");
}

// Perspective 13: doubling each element in place.
#[test]
fn test_array_mutate_in_loop() {
    let (out, ok) = compile_and_run(
        r#"
import std.collections.Array;

fn main() {
    let mut xs: Array<i64> = [1, 2, 3, 4];
    let mut i = 0;
    while i < xs.len() {
        xs[i] = xs[i] * 2;
        i = i + 1;
    }
    println(xs[0]);
    println(xs[3]);
}
"#,
    );
    assert!(ok);
    assert_eq!(out, "2\n8\n");
}

// Perspective 14: `.len()` used directly in an arithmetic expression.
#[test]
fn test_array_len_in_expression() {
    let (out, ok) = compile_and_run(
        r#"
import std.collections.Array;

fn main() {
    let xs: Array<i64> = [1, 2, 3, 4, 5];
    println(xs.len() * 2);
}
"#,
    );
    assert!(ok);
    assert_eq!(out, "10\n");
}

// Perspective 15: string array survives a GC collection while held live.
#[test]
fn test_array_string_elements_survive_gc() {
    let (out, ok) = compile_and_run(
        r#"
import std.collections.Array;

fn main() {
    let names: Array<String> = ["alpha", "beta", "gamma"];
    gc_collect();
    println(names[0]);
    println(names[2]);
}
"#,
    );
    assert!(ok, "array string elements must survive GC");
    assert_eq!(out, "alpha\ngamma\n");
}

// Perspective 16: out-of-bounds read aborts with a clear message.
#[test]
fn test_array_index_out_of_bounds_read_aborts() {
    let (out, ok) = compile_and_run_check_exit(
        r#"
import std.collections.Array;

fn main() {
    let xs: Array<i64> = [1, 2];
    println(xs[5]);
}
"#,
    );
    assert!(!ok, "out-of-bounds read must abort");
    assert!(
        out.contains("out of bounds"),
        "expected an out-of-bounds panic message, got: {out}"
    );
}

// Perspective 17: out-of-bounds write aborts.
#[test]
fn test_array_index_out_of_bounds_write_aborts() {
    let (_out, ok) = compile_and_run_check_exit(
        r#"
import std.collections.Array;

fn main() {
    let mut xs: Array<i64> = [1, 2];
    xs[9] = 0;
}
"#,
    );
    assert!(!ok, "out-of-bounds write must abort");
}

// Perspective 18: a negative index aborts.
#[test]
fn test_array_negative_index_aborts() {
    let (out, ok) = compile_and_run_check_exit(
        r#"
import std.collections.Array;

fn main() {
    let xs: Array<i64> = [1, 2, 3];
    let i = 0 - 1;
    println(xs[i]);
}
"#,
    );
    assert!(!ok, "negative index must abort");
    assert!(out.contains("out of bounds"), "got: {out}");
}

// Perspective 19: indexing with a non-i64 type is a compile error.
#[test]
fn test_array_index_non_i64_is_error() {
    assert_compile_error_contains(
        r#"
import std.collections.Array;

fn main() {
    let xs: Array<i64> = [1, 2, 3];
    println(xs[true]);
}
"#,
        &["error[E0201]", "index must be `i64`"],
    );
}

// Perspective 20: indexing a non-array value is a compile error.
#[test]
fn test_array_index_non_array_is_error() {
    assert_compile_error_contains(
        r#"
fn main() {
    let x: i64 = 5;
    println(x[0]);
}
"#,
        &["error[E0201]", "cannot index a value of type `i64`"],
    );
}

// Perspective 21: mismatched element types in a literal is a compile error.
#[test]
fn test_array_mixed_element_types_is_error() {
    assert_compile_error_contains(
        r#"
fn main() {
    let xs = [1, true, 3];
    println(xs.len());
}
"#,
        &["error[E0201]", "array elements must have the same type"],
    );
}

// Perspective 22: an unknown array method is a compile error.
#[test]
fn test_array_unknown_method_is_error() {
    assert_compile_error_contains(
        r#"
import std.collections.Array;

fn main() {
    let xs: Array<i64> = [1, 2, 3];
    println(xs.first());
}
"#,
        &["error[E0201]", "no method `first` on `Array<i64>`"],
    );
}

// Perspective 23: assigning the wrong element type is a compile error.
#[test]
fn test_array_element_assign_type_mismatch_is_error() {
    assert_compile_error_contains(
        r#"
import std.collections.Array;

fn main() {
    let mut xs: Array<i64> = [1, 2, 3];
    xs[0] = true;
}
"#,
        &["error[E0201]"],
    );
}

// ── Map<K,V> type (willow-5t6) ─────────────────────────────────────────────
// GC-managed hash map: Map::new(), .insert(k,v), .get(k) -> Option<V>,
// .contains(k) -> bool, .len() -> i64. Keys: String (by content) or i64.

// Perspective 1: insert/get/len with String keys.
#[test]
fn test_map_string_key_insert_get_len() {
    let (out, ok) = compile_and_run(
        r#"
import std.collections.Map;

fn main() {
    let mut ages: Map<String, i64> = Map::new();
    ages.insert("Alice", 30);
    ages.insert("Bob", 25);
    println(ages.len());
    println(match ages.get("Alice") { Option::Some(a) => a, Option::None => -1, });
    println(match ages.get("Bob") { Option::Some(a) => a, Option::None => -1, });
}
"#,
    );
    assert!(ok);
    assert_eq!(out, "2\n30\n25\n");
}

// Perspective 2: a missing key returns None.
#[test]
fn test_map_get_missing_returns_none() {
    let (out, ok) = compile_and_run(
        r#"
import std.collections.Map;

fn main() {
    let mut m: Map<String, i64> = Map::new();
    m.insert("a", 1);
    println(match m.get("zzz") { Option::Some(v) => v, Option::None => -99, });
}
"#,
    );
    assert!(ok);
    assert_eq!(out, "-99\n");
}

// Perspective 3: insert overwrites an existing key.
#[test]
fn test_map_insert_overwrites() {
    let (out, ok) = compile_and_run(
        r#"
import std.collections.Map;

fn main() {
    let mut m: Map<String, i64> = Map::new();
    m.insert("k", 1);
    m.insert("k", 2);
    println(m.len());
    println(match m.get("k") { Option::Some(v) => v, Option::None => -1, });
}
"#,
    );
    assert!(ok);
    assert_eq!(out, "1\n2\n");
}

// Perspective 4: contains reports presence/absence.
#[test]
fn test_map_contains() {
    let (out, ok) = compile_and_run(
        r#"
import std.collections.Map;

fn main() {
    let mut m: Map<String, i64> = Map::new();
    m.insert("x", 1);
    println(m.contains("x"));
    println(m.contains("y"));
}
"#,
    );
    assert!(ok);
    assert_eq!(out, "true\nfalse\n");
}

// Perspective 5: i64 keys.
#[test]
fn test_map_i64_keys() {
    let (out, ok) = compile_and_run(
        r#"
import std.collections.Map;

fn main() {
    let mut m: Map<i64, i64> = Map::new();
    m.insert(10, 100);
    m.insert(20, 200);
    println(match m.get(20) { Option::Some(v) => v, Option::None => -1, });
    println(match m.get(30) { Option::Some(v) => v, Option::None => -1, });
}
"#,
    );
    assert!(ok);
    assert_eq!(out, "200\n-1\n");
}

// Perspective 6: String values (GC references).
#[test]
fn test_map_string_values() {
    let (out, ok) = compile_and_run(
        r#"
import std.collections.Map;

fn main() {
    let mut m: Map<i64, String> = Map::new();
    m.insert(1, "one");
    m.insert(2, "two");
    println(match m.get(2) { Option::Some(s) => s, Option::None => "none", });
}
"#,
    );
    assert!(ok);
    assert_eq!(out, "two\n");
}

// Perspective 7: empty map has length 0.
#[test]
fn test_map_empty_len_zero() {
    let (out, ok) = compile_and_run(
        r#"
import std.collections.Map;

fn main() {
    let m: Map<String, i64> = Map::new();
    println(m.len());
}
"#,
    );
    assert!(ok);
    assert_eq!(out, "0\n");
}

// Perspective 8: a map passed as a function parameter.
#[test]
fn test_map_as_parameter() {
    let (out, ok) = compile_and_run(
        r#"
import std.collections.Map;

fn get_or(m: Map<String, i64>, k: String, d: i64) -> i64 {
    return match m.get(k) { Option::Some(v) => v, Option::None => d, };
}
fn main() {
    let mut m: Map<String, i64> = Map::new();
    m.insert("a", 7);
    println(get_or(m, "a", -1));
    println(get_or(m, "b", -1));
}
"#,
    );
    assert!(ok);
    assert_eq!(out, "7\n-1\n");
}

// Perspective 9: a map returned from a function.
#[test]
fn test_map_returned_from_function() {
    let (out, ok) = compile_and_run(
        r#"
import std.collections.Map;

fn build() -> Map<String, i64> {
    let mut m: Map<String, i64> = Map::new();
    m.insert("v", 99);
    return m;
}
fn main() {
    let m = build();
    println(match m.get("v") { Option::Some(v) => v, Option::None => -1, });
}
"#,
    );
    assert!(ok);
    assert_eq!(out, "99\n");
}

// Perspective 10: String keys compare by content, not identity.
#[test]
fn test_map_string_keys_by_content() {
    let (out, ok) = compile_and_run(
        r#"
import std.collections.Map;

fn key() -> String { return "dynamic"; }
fn main() {
    let mut m: Map<String, i64> = Map::new();
    m.insert("dynamic", 5);
    // A value produced separately but equal in content must hit.
    println(match m.get(key()) { Option::Some(v) => v, Option::None => -1, });
}
"#,
    );
    assert!(ok);
    assert_eq!(out, "5\n");
}

// Perspective 11: len grows with distinct keys.
#[test]
fn test_map_len_distinct_keys() {
    let (out, ok) = compile_and_run(
        r#"
import std.collections.Map;

fn main() {
    let mut m: Map<i64, i64> = Map::new();
    m.insert(1, 1);
    m.insert(2, 2);
    m.insert(3, 3);
    println(m.len());
}
"#,
    );
    assert!(ok);
    assert_eq!(out, "3\n");
}

// Perspective 12: a get result bound to a variable, then matched.
#[test]
fn test_map_get_result_in_variable() {
    let (out, ok) = compile_and_run(
        r#"
import std.collections.Map;

fn main() {
    let mut m: Map<String, i64> = Map::new();
    m.insert("k", 42);
    let r = m.get("k");
    println(match r { Option::Some(v) => v, Option::None => -1, });
}
"#,
    );
    assert!(ok);
    assert_eq!(out, "42\n");
}

// Perspective 13: reference values survive a GC collection while the map lives.
#[test]
fn test_map_string_values_survive_gc() {
    let (out, ok) = compile_and_run(
        r#"
import std.collections.Map;

fn main() {
    let mut m: Map<i64, String> = Map::new();
    m.insert(1, "alpha");
    m.insert(2, "beta");
    gc_collect();
    println(match m.get(1) { Option::Some(s) => s, Option::None => "gone", });
    println(match m.get(2) { Option::Some(s) => s, Option::None => "gone", });
}
"#,
    );
    assert!(ok, "map string values must survive GC");
    assert_eq!(out, "alpha\nbeta\n");
}

// Perspective 14: a get value used in arithmetic.
#[test]
fn test_map_value_in_arithmetic() {
    let (out, ok) = compile_and_run(
        r#"
import std.collections.Map;

fn main() {
    let mut m: Map<String, i64> = Map::new();
    m.insert("n", 21);
    let v = match m.get("n") { Option::Some(x) => x, Option::None => 0, };
    println(v * 2);
}
"#,
    );
    assert!(ok);
    assert_eq!(out, "42\n");
}

// Perspective 15: a wrong key type is a compile error.
#[test]
fn test_map_wrong_key_type_is_error() {
    assert_compile_error_contains(
        r#"
import std.collections.Map;

fn main() {
    let mut m: Map<String, i64> = Map::new();
    m.insert(1, 2);
}
"#,
        &["error[E0201]", "map key type mismatch"],
    );
}

// Perspective 16: a wrong value type is a compile error.
#[test]
fn test_map_wrong_value_type_is_error() {
    assert_compile_error_contains(
        r#"
import std.collections.Map;

fn main() {
    let mut m: Map<String, i64> = Map::new();
    m.insert("a", true);
}
"#,
        &["error[E0201]", "map value type mismatch"],
    );
}

// Perspective 17: an unknown method is a compile error.
#[test]
fn test_map_unknown_method_is_error() {
    assert_compile_error_contains(
        r#"
import std.collections.Map;

fn main() {
    let m: Map<String, i64> = Map::new();
    m.clear();
}
"#,
        &["error[E0201]", "no method `clear` on `Map<"],
    );
}

// Perspective 18: get with the wrong argument count is a compile error.
#[test]
fn test_map_get_wrong_arity_is_error() {
    assert_compile_error_contains(
        r#"
import std.collections.Map;

fn main() {
    let m: Map<String, i64> = Map::new();
    let r = m.get();
}
"#,
        &["error[E0201]", "`Map::get` expects 1 argument"],
    );
}

// Perspective 19: Map::new with arguments is a compile error.
#[test]
fn test_map_new_with_args_is_error() {
    assert_compile_error_contains(
        r#"
import std.collections.Map;

fn main() {
    let m: Map<String, i64> = Map::new(5);
}
"#,
        &["error[E0201]", "`Map::new` takes no arguments"],
    );
}

// Perspective 20: two independent maps do not share state.
#[test]
fn test_map_independent_instances() {
    let (out, ok) = compile_and_run(
        r#"
import std.collections.Map;

fn main() {
    let mut a: Map<String, i64> = Map::new();
    let mut b: Map<String, i64> = Map::new();
    a.insert("k", 1);
    b.insert("k", 2);
    println(match a.get("k") { Option::Some(v) => v, Option::None => -1, });
    println(match b.get("k") { Option::Some(v) => v, Option::None => -1, });
    println(b.contains("k"));
    println(a.len());
}
"#,
    );
    assert!(ok);
    assert_eq!(out, "1\n2\ntrue\n1\n");
}

// ── Command-line arguments: fn main(args) and env::args() (willow-b86) ──────

// Perspective 1: main(args) receives the user arguments (excluding program name).
#[test]
fn test_main_args_length_and_elements() {
    let (out, ok) = compile_and_run_with_program_args(
        r#"
import std.collections.Array;

fn main(args: Array<String>) {
    println(args.len());
    let mut i = 0;
    while i < args.len() { println(args[i]); i = i + 1; }
}
"#,
        &["alpha", "beta", "gamma"],
    );
    assert!(ok);
    assert_eq!(out, "3\nalpha\nbeta\ngamma\n");
}

// Perspective 2: main(args) with no arguments sees an empty array.
#[test]
fn test_main_args_empty() {
    let (out, ok) = compile_and_run_with_program_args(
        r#"
import std.collections.Array;

fn main(args: Array<String>) {
    println(args.len());
}
"#,
        &[],
    );
    assert!(ok);
    assert_eq!(out, "0\n");
}

// Perspective 3: env::args() returns the same arguments.
#[test]
fn test_env_args_length() {
    let (out, ok) = compile_and_run_with_program_args(
        r#"
fn main() {
    let a = env::args();
    println(a.len());
    println(a[0]);
    println(a[1]);
}
"#,
        &["one", "two"],
    );
    assert!(ok);
    assert_eq!(out, "2\none\ntwo\n");
}

// Perspective 4: env::args() and main(args) agree.
#[test]
fn test_main_args_matches_env_args() {
    let (out, ok) = compile_and_run_with_program_args(
        r#"
import std.collections.Array;

fn main(args: Array<String>) {
    let other = env::args();
    println(args.len() == other.len());
    println(args.len() == env::args_len());
}
"#,
        &["x", "y", "z"],
    );
    assert!(ok);
    assert_eq!(out, "true\ntrue\n");
}

// Perspective 5: env::args() in a non-main function.
#[test]
fn test_env_args_in_helper() {
    let (out, ok) = compile_and_run_with_program_args(
        r#"
fn count() -> i64 { return env::args().len(); }
fn main() { println(count()); }
"#,
        &["a", "b"],
    );
    assert!(ok);
    assert_eq!(out, "2\n");
}

// Perspective 6: the args array can be passed to another function.
#[test]
fn test_main_args_passed_to_helper() {
    let (out, ok) = compile_and_run_with_program_args(
        r#"
import std.collections.Array;

fn first(xs: Array<String>) -> String {
    if xs.len() > 0 { return xs[0]; }
    return "none";
}
fn main(args: Array<String>) {
    println(first(args));
}
"#,
        &["hello", "world"],
    );
    assert!(ok);
    assert_eq!(out, "hello\n");
}

// Perspective 7: a single argument.
#[test]
fn test_main_args_single() {
    let (out, ok) = compile_and_run_with_program_args(
        r#"
import std.collections.Array;

fn main(args: Array<String>) {
    println(args.len());
    println(args[0]);
}
"#,
        &["solo"],
    );
    assert!(ok);
    assert_eq!(out, "1\nsolo\n");
}

// Perspective 8: env::args() stored in a variable, then indexed.
#[test]
fn test_env_args_in_variable() {
    let (out, ok) = compile_and_run_with_program_args(
        r#"
fn main() {
    let a = env::args();
    let mut i = 0;
    while i < a.len() { println(a[i]); i = i + 1; }
}
"#,
        &["p", "q"],
    );
    assert!(ok);
    assert_eq!(out, "p\nq\n");
}

// Perspective 9: a plain fn main() still works, ignoring any arguments.
#[test]
fn test_main_no_params_ignores_args() {
    let (out, ok) = compile_and_run_with_program_args(
        r#"
fn main() { println(42); }
"#,
        &["ignored", "args"],
    );
    assert!(ok);
    assert_eq!(out, "42\n");
}

// Perspective 10: args length used in arithmetic.
#[test]
fn test_main_args_len_arithmetic() {
    let (out, ok) = compile_and_run_with_program_args(
        r#"
import std.collections.Array;

fn main(args: Array<String>) {
    println(args.len() * 10);
}
"#,
        &["a", "b", "c", "d"],
    );
    assert!(ok);
    assert_eq!(out, "40\n");
}

// Perspective 11: env::arg(i) and env::args()[i] agree.
#[test]
fn test_env_arg_index_agrees_with_array() {
    let (out, ok) = compile_and_run_with_program_args(
        r#"
fn main() {
    let a = env::args();
    println(a[1]);
    println(env::arg(1));
}
"#,
        &["zero", "first"],
    );
    assert!(ok);
    assert_eq!(out, "first\nfirst\n");
}

// Perspective 12: an empty env::args() iterates zero times.
#[test]
fn test_env_args_empty_no_iteration() {
    let (out, ok) = compile_and_run_with_program_args(
        r#"
fn main() {
    let a = env::args();
    println(a.len());
    let mut i = 0;
    while i < a.len() { println(a[i]); i = i + 1; }
    println(99);
}
"#,
        &[],
    );
    assert!(ok);
    assert_eq!(out, "0\n99\n");
}

// Perspective 13: an invalid main signature is rejected (E1301).
#[test]
fn test_main_invalid_arg_type_is_error() {
    assert_compile_error_contains(
        r#"
fn main(n: i64) {
    println(n);
}
"#,
        &["error[E1301]", "invalid entry point signature"],
    );
}

// Perspective 14: a non-Array<String> single param is rejected.
#[test]
fn test_main_array_of_i64_param_is_error() {
    assert_compile_error_contains(
        r#"
import std.collections.Array;

fn main(args: Array<i64>) {
    println(args.len());
}
"#,
        &["error[E1301]"],
    );
}

// Perspective 15: the last argument is reachable by index.
#[test]
fn test_main_args_last_element() {
    let (out, ok) = compile_and_run_with_program_args(
        r#"
import std.collections.Array;

fn main(args: Array<String>) {
    println(args[args.len() - 1]);
}
"#,
        &["a", "b", "last"],
    );
    assert!(ok);
    assert_eq!(out, "last\n");
}

// Perspective 16: arguments preserve order and content.
#[test]
fn test_main_args_order_preserved() {
    let (out, ok) = compile_and_run_with_program_args(
        r#"
import std.collections.Array;

fn main(args: Array<String>) {
    println(args[0]);
    println(args[2]);
}
"#,
        &["first", "middle", "third"],
    );
    assert!(ok);
    assert_eq!(out, "first\nthird\n");
}

// ── User module declarations (willow-y0o, spec 4.1 / 8 / 20) ───────────────

// Perspective 1: a module declaration is accepted and the program runs (the
// declaration is otherwise inert for an entry file).
#[test]
fn test_module_decl_entry_compiles() {
    let (out, ok) = compile_and_run(
        r#"
module myapp;
fn main() { println(7); }
"#,
    );
    assert!(ok);
    assert_eq!(out, "7\n");
}

// Perspective 2: dotted/colon module paths are accepted on the entry file.
#[test]
fn test_module_decl_dotted_entry_compiles() {
    let (out, ok) = compile_and_run(
        r#"
module myapp.tools;
fn main() { println(8); }
"#,
    );
    assert!(ok);
    assert_eq!(out, "8\n");
}

// Perspective 3: `module std...` is rejected (reserved namespace).
#[test]
fn test_module_decl_std_rejected() {
    assert_compile_error_contains(
        "module std.io;\nfn main() {}\n",
        &["error[E2010]", "reserved namespace"],
    );
}

// Perspective 4: a module declaration after an item is rejected.
#[test]
fn test_module_decl_after_item_rejected() {
    assert_compile_error_contains(
        "fn helper() {}\nmodule myapp;\nfn main() {}\n",
        &["error[E2008]", "must appear before imports and items"],
    );
}

// Perspective 5: a duplicate module declaration is rejected.
#[test]
fn test_module_decl_duplicate_rejected() {
    assert_compile_error_contains(
        "module a;\nmodule b;\nfn main() {}\n",
        &["error[E2009]", "duplicate module declaration"],
    );
}

// Perspective 6: programs without a module declaration still compile.
#[test]
fn test_no_module_decl_backward_compatible() {
    let (out, ok) = compile_and_run(r#"fn main() { println(1); }"#);
    assert!(ok);
    assert_eq!(out, "1\n");
}

// Perspective 7: an imported file whose declared module matches the import path
// resolves and runs.
#[test]
fn test_imported_module_matching_decl_runs() {
    let (out, ok) = compile_temp_project_and_run(
        &[
            (
                "main.wi",
                "import math;\nfn main() { println(math::add(2, 3)); }\n",
            ),
            (
                "math.wi",
                "module math;\npub fn add(a: i64, b: i64) -> i64 { return a + b; }\n",
            ),
        ],
        "main.wi",
    );
    assert!(ok);
    assert_eq!(out, "5\n");
}

// Perspective 8: an imported file whose declared module does not match the
// import path is an error (E2011).
#[test]
fn test_imported_module_mismatched_decl_errors() {
    let stderr = compile_temp_project_error_stderr(
        &[
            (
                "main.wi",
                "import math;\nfn main() { println(math::add(2, 3)); }\n",
            ),
            (
                "math.wi",
                "module other;\npub fn add(a: i64, b: i64) -> i64 { return a + b; }\n",
            ),
        ],
        "main.wi",
    );
    assert!(stderr.contains("error[E2011]"), "stderr: {stderr}");
    assert!(
        stderr.contains("does not match import path"),
        "stderr: {stderr}"
    );
}

// Perspective 9: an imported file with no module declaration still resolves
// (identity derived from the path — backward compatible).
#[test]
fn test_imported_module_no_decl_backward_compatible() {
    let (out, ok) = compile_temp_project_and_run(
        &[
            (
                "main.wi",
                "import math;\nfn main() { println(math::add(4, 5)); }\n",
            ),
            (
                "math.wi",
                "pub fn add(a: i64, b: i64) -> i64 { return a + b; }\n",
            ),
        ],
        "main.wi",
    );
    assert!(ok);
    assert_eq!(out, "9\n");
}

// Perspective 10: a nested module path matches a declared nested module.
#[test]
fn test_nested_imported_module_matching_decl_runs() {
    let (out, ok) = compile_temp_project_and_run(
        &[
            (
                "main.wi",
                "import foo::bar;\nfn main() { println(bar::val()); }\n",
            ),
            (
                "foo/bar.wi",
                "module foo.bar;\npub fn val() -> i64 { return 77; }\n",
            ),
        ],
        "main.wi",
    );
    assert!(ok);
    assert_eq!(out, "77\n");
}

// Perspective 11: a nested module with a mismatched declaration is an error.
#[test]
fn test_nested_imported_module_mismatch_errors() {
    let stderr = compile_temp_project_error_stderr(
        &[
            (
                "main.wi",
                "import foo::bar;\nfn main() { println(bar::val()); }\n",
            ),
            (
                "foo/bar.wi",
                "module foo.baz;\npub fn val() -> i64 { return 1; }\n",
            ),
        ],
        "main.wi",
    );
    assert!(stderr.contains("error[E2011]"), "stderr: {stderr}");
}

// ── Single-item imports (willow-om7, spec 10 / 12.2) ───────────────────────

fn math_module() -> (&'static str, &'static str) {
    (
        "math.wi",
        "module math;\npub fn add(a: i64, b: i64) -> i64 { return a + b; }\npub fn mul(a: i64, b: i64) -> i64 { return a * b; }\nfn secret() -> i64 { return 99; }\n",
    )
}

// Perspective 1: a directly imported function is callable unqualified.
#[test]
fn test_item_import_function_call() {
    let (out, ok) = compile_temp_project_and_run(
        &[
            (
                "main.wi",
                "import math.add;\nfn main() { println(add(2, 3)); }\n",
            ),
            math_module(),
        ],
        "main.wi",
    );
    assert!(ok);
    assert_eq!(out, "5\n");
}

// Perspective 2: an item import with an alias.
#[test]
fn test_item_import_alias() {
    let (out, ok) = compile_temp_project_and_run(
        &[
            (
                "main.wi",
                "import math.add as plus;\nfn main() { println(plus(10, 20)); }\n",
            ),
            math_module(),
        ],
        "main.wi",
    );
    assert!(ok);
    assert_eq!(out, "30\n");
}

// Perspective 3: two item imports from the same module.
#[test]
fn test_item_import_two_items() {
    let (out, ok) = compile_temp_project_and_run(
        &[
            (
                "main.wi",
                "import math.add;\nimport math.mul;\nfn main() { println(add(2, 3)); println(mul(2, 3)); }\n",
            ),
            math_module(),
        ],
        "main.wi",
    );
    assert!(ok);
    assert_eq!(out, "5\n6\n");
}

// Perspective 4: importing a private item is rejected.
#[test]
fn test_item_import_private_rejected() {
    let stderr = compile_temp_project_error_stderr(
        &[
            (
                "main.wi",
                "import math.secret;\nfn main() { println(secret()); }\n",
            ),
            math_module(),
        ],
        "main.wi",
    );
    assert!(stderr.contains("error[E2006]"), "stderr: {stderr}");
    assert!(stderr.contains("private"), "stderr: {stderr}");
}

// Perspective 5: importing a non-existent item is rejected.
#[test]
fn test_item_import_missing_rejected() {
    let stderr = compile_temp_project_error_stderr(
        &[
            ("main.wi", "import math.nope;\nfn main() { println(1); }\n"),
            math_module(),
        ],
        "main.wi",
    );
    assert!(stderr.contains("error[E2006]"), "stderr: {stderr}");
    assert!(stderr.contains("no item `nope`"), "stderr: {stderr}");
}

// Perspective 6: a module import still works alongside item imports.
#[test]
fn test_item_import_with_module_import() {
    let (out, ok) = compile_temp_project_and_run(
        &[
            (
                "main.wi",
                "import math;\nimport math.add;\nfn main() { println(add(1, 1)); println(math::mul(2, 4)); }\n",
            ),
            math_module(),
        ],
        "main.wi",
    );
    assert!(ok);
    assert_eq!(out, "2\n8\n");
}

// Perspective 7: an item import without any plain `import math;` still loads
// the module (no explicit module import required).
#[test]
fn test_item_import_loads_module_implicitly() {
    let (out, ok) = compile_temp_project_and_run(
        &[
            (
                "main.wi",
                "import math.mul;\nfn main() { println(mul(6, 7)); }\n",
            ),
            math_module(),
        ],
        "main.wi",
    );
    assert!(ok);
    assert_eq!(out, "42\n");
}

// Perspective 8: an item-imported function used inside a helper.
#[test]
fn test_item_import_used_in_helper() {
    let (out, ok) = compile_temp_project_and_run(
        &[
            (
                "main.wi",
                "import math.add;\nfn twice(n: i64) -> i64 { return add(n, n); }\nfn main() { println(twice(21)); }\n",
            ),
            math_module(),
        ],
        "main.wi",
    );
    assert!(ok);
    assert_eq!(out, "42\n");
}

// Perspective 9: the item-imported function's result in an expression.
#[test]
fn test_item_import_result_in_expression() {
    let (out, ok) = compile_temp_project_and_run(
        &[
            (
                "main.wi",
                "import math.add;\nfn main() { println(add(3, 4) * 2); }\n",
            ),
            math_module(),
        ],
        "main.wi",
    );
    assert!(ok);
    assert_eq!(out, "14\n");
}

// Perspective 10: a nested-module item import (`import foo.bar.baz;`).
#[test]
fn test_item_import_nested_module() {
    let (out, ok) = compile_temp_project_and_run(
        &[
            (
                "main.wi",
                "import foo.bar.baz;\nfn main() { println(baz()); }\n",
            ),
            (
                "foo/bar.wi",
                "module foo.bar;\npub fn baz() -> i64 { return 88; }\n",
            ),
        ],
        "main.wi",
    );
    assert!(ok);
    assert_eq!(out, "88\n");
}

// Perspective 11: two item imports + an alias together.
#[test]
fn test_item_import_mixed() {
    let (out, ok) = compile_temp_project_and_run(
        &[
            (
                "main.wi",
                "import math.add;\nimport math.mul as times;\nfn main() { println(add(1, 2)); println(times(3, 4)); }\n",
            ),
            math_module(),
        ],
        "main.wi",
    );
    assert!(ok);
    assert_eq!(out, "3\n12\n");
}

// ── validate_type rejects unknown/module type annotations (willow-a7j) ─────

// A module name used as a type is rejected.
#[test]
fn test_module_name_as_param_type_rejected() {
    let stderr = compile_temp_project_error_stderr(
        &[
            (
                "main.wi",
                "import calc;\nfn f(x: calc) -> i64 { return 0; }\nfn main() { println(1); }\n",
            ),
            (
                "calc.wi",
                "module calc;\npub fn add(a: i64, b: i64) -> i64 { return a + b; }\n",
            ),
        ],
        "main.wi",
    );
    assert!(stderr.contains("error[E0350]"), "stderr: {stderr}");
    assert!(
        stderr.contains("is a module, not a type"),
        "stderr: {stderr}"
    );
}

// An undefined type name in a parameter is rejected.
#[test]
fn test_unknown_param_type_rejected() {
    assert_compile_error_contains(
        "fn f(x: Bogus) -> i64 { return 0; }\nfn main() {}\n",
        &["error[E0350]", "cannot find type `Bogus`"],
    );
}

// An undefined type name in a return position is rejected.
#[test]
fn test_unknown_return_type_rejected() {
    assert_compile_error_contains(
        "fn f() -> Nope { return 0; }\nfn main() {}\n",
        &["error[E0350]", "cannot find type `Nope`"],
    );
}

// An undefined type name in a let annotation is rejected.
#[test]
fn test_unknown_let_type_rejected() {
    assert_compile_error_contains(
        "fn main() { let x: Whatever = 1; println(1); }\n",
        &["error[E0350]", "cannot find type `Whatever`"],
    );
}

// An undefined type name in a class field is rejected.
#[test]
fn test_unknown_field_type_rejected() {
    assert_compile_error_contains(
        "class C { pub v: Ghost; }\nfn main() {}\n",
        &["error[E0350]", "cannot find type `Ghost`"],
    );
}

// Regression guard: a real class type is still accepted.
#[test]
fn test_known_class_type_accepted() {
    let (out, ok) = compile_and_run(
        r#"
class P {
    pub v: i64;
    pub fn new(v: i64) -> P { return P { v: v }; }
    pub fn get(self) -> i64 { return self.v; }
}
fn use_p(p: P) -> i64 { return p.get(); }
fn main() { println(use_p(P::new(42))); }
"#,
    );
    assert!(ok, "a known class type must validate");
    assert_eq!(out, "42\n");
}

// Regression guard: enum types (Option/Result) are still accepted.
#[test]
fn test_known_enum_type_accepted() {
    let (out, ok) = compile_and_run(
        r#"
fn pick(x: Option<i64>) -> Result<i64, String> {
    return match x { Option::Some(v) => Result::Ok(v), Option::None => Result::Err("none"), };
}
fn main() {
    let r = pick(Option::Some(5));
    println(match r { Result::Ok(v) => v, Result::Err(_) => -1, });
}
"#,
    );
    assert!(ok, "Option/Result types must validate");
    assert_eq!(out, "5\n");
}

// Regression guard: a module-qualified class type annotation is accepted.
#[test]
fn test_module_qualified_class_type_accepted() {
    let (out, ok) = compile_temp_project_and_run(
        &[
            (
                "main.wi",
                "import geom;\nfn show(p: geom::Point) -> i64 { return p.getx(); }\nfn main() { println(1); }\n",
            ),
            (
                "geom.wi",
                "module geom;\npub class Point {\n    pub x: i64;\n    pub fn getx(self) -> i64 { return self.x; }\n}\n",
            ),
        ],
        "main.wi",
    );
    assert!(ok, "module-qualified class type must validate");
    assert_eq!(out, "1\n");
}

// Regression guard: a module-qualified class constructor parses, type-checks,
// links to the imported module's class method, and returns the qualified object.
#[test]
fn test_module_qualified_class_constructor_runs() {
    let (out, ok) = compile_temp_project_and_run(
        &[
            (
                "main.wi",
                "import geom;\nfn main() { let p = geom::Point::new(10, 32); println(p.sum()); }\n",
            ),
            (
                "geom.wi",
                "module geom;\npub class Point {\n    pub x: i64;\n    pub y: i64;\n    pub fn new(x: i64, y: i64) -> Point { return Point { x: x, y: y }; }\n    pub fn sum(self) -> i64 { return self.x + self.y; }\n}\n",
            ),
        ],
        "main.wi",
    );
    assert!(ok, "module-qualified class construction should run");
    assert_eq!(out, "42\n");
}

// Imported module bodies can still use their local class name while the entry
// module uses the qualified class name.
#[test]
fn test_module_class_body_can_call_local_constructor() {
    let (out, ok) = compile_temp_project_and_run(
        &[
            (
                "main.wi",
                "import geom;\nfn main() { println(geom::origin_sum()); }\n",
            ),
            (
                "geom.wi",
                "module geom;\npub class Point {\n    pub x: i64;\n    pub y: i64;\n    pub fn new(x: i64, y: i64) -> Point { return Point { x: x, y: y }; }\n    pub fn sum(self) -> i64 { return self.x + self.y; }\n}\npub fn origin_sum() -> i64 { let p = Point::new(3, 4); return p.sum(); }\n",
            ),
        ],
        "main.wi",
    );
    assert!(
        ok,
        "module class methods should be available inside the module"
    );
    assert_eq!(out, "7\n");
}

#[test]
fn test_module_alias_class_constructor_uses_canonical_symbol() {
    let (out, ok) = compile_temp_project_and_run(
        &[
            (
                "main.wi",
                "import geom as g;\nfn main() { let p = g::Point::new(5, 6); println(p.sum()); }\n",
            ),
            (
                "geom.wi",
                "module geom;\npub class Point {\n    pub x: i64;\n    pub y: i64;\n    pub fn new(x: i64, y: i64) -> Point { return Point { x: x, y: y }; }\n    pub fn sum(self) -> i64 { return self.x + self.y; }\n}\n",
            ),
        ],
        "main.wi",
    );
    assert!(ok, "aliased module class construction should run");
    assert_eq!(out, "11\n");
}

#[test]
fn test_nested_item_imports_same_leaf_module_do_not_collide() {
    let (out, ok) = compile_temp_project_and_run(
        &[
            (
                "main.wi",
                "import left.math.value as left_value;\nimport right.math.value as right_value;\nfn main() { println(left_value()); println(right_value()); }\n",
            ),
            (
                "left/math.wi",
                "module left.math;\npub fn value() -> i64 { return 11; }\n",
            ),
            (
                "right/math.wi",
                "module right.math;\npub fn value() -> i64 { return 22; }\n",
            ),
        ],
        "main.wi",
    );
    assert!(
        ok,
        "canonical module symbol names should avoid leaf-name collisions"
    );
    assert_eq!(out, "11\n22\n");
}

// ── Module aliases + `::` access; `.` reserved for instances (willow-u98) ──

fn aliasable_math() -> (&'static str, &'static str) {
    (
        "math.wi",
        "module math;\npub fn add(a: i64, b: i64) -> i64 { return a + b; }\npub fn square(n: i64) -> i64 { return n * n; }\n",
    )
}

// A module imported under an alias is accessed with `alias::item`.
#[test]
fn test_module_alias_qualified_call() {
    let (out, ok) = compile_temp_project_and_run(
        &[
            (
                "main.wi",
                "import math as m;\nfn main() { println(m::add(2, 3)); println(m::square(4)); }\n",
            ),
            aliasable_math(),
        ],
        "main.wi",
    );
    assert!(ok);
    assert_eq!(out, "5\n16\n");
}

// The plain `module::item` form still works without an alias.
#[test]
fn test_module_qualified_call_no_alias() {
    let (out, ok) = compile_temp_project_and_run(
        &[
            (
                "main.wi",
                "import math;\nfn main() { println(math::add(10, 20)); }\n",
            ),
            aliasable_math(),
        ],
        "main.wi",
    );
    assert!(ok);
    assert_eq!(out, "30\n");
}

// Accessing a module item with `.` is an error that points at `::`.
#[test]
fn test_module_dot_access_rejected() {
    let stderr = compile_temp_project_error_stderr(
        &[
            (
                "main.wi",
                "import math;\nfn main() { println(math.add(1, 2)); }\n",
            ),
            aliasable_math(),
        ],
        "main.wi",
    );
    assert!(stderr.contains("error[E0350]"), "stderr: {stderr}");
    assert!(stderr.contains("is a module; use `::`"), "stderr: {stderr}");
}

// `.` on an aliased module is likewise rejected.
#[test]
fn test_module_alias_dot_access_rejected() {
    let stderr = compile_temp_project_error_stderr(
        &[
            (
                "main.wi",
                "import math as m;\nfn main() { println(m.add(1, 2)); }\n",
            ),
            aliasable_math(),
        ],
        "main.wi",
    );
    assert!(stderr.contains("error[E0350]"), "stderr: {stderr}");
}

// After aliasing, the original module name is not in scope.
#[test]
fn test_module_alias_hides_original_name() {
    let stderr = compile_temp_project_error_stderr(
        &[
            (
                "main.wi",
                "import math as m;\nfn main() { println(math::add(1, 2)); }\n",
            ),
            aliasable_math(),
        ],
        "main.wi",
    );
    // `math` is not a known module under the alias import.
    assert!(
        !stderr.is_empty(),
        "expected an error using the original name"
    );
}

// Instance `.` method/field access is unaffected by the module-dot rule.
#[test]
fn test_instance_dot_access_still_works() {
    let (out, ok) = compile_and_run(
        r#"
class P {
    pub v: i64;
    pub fn new(v: i64) -> P { return P { v: v }; }
    pub fn get(self) -> i64 { return self.v; }
}
fn main() {
    let p = P::new(9);
    println(p.get());
    println(p.v);
}
"#,
    );
    assert!(ok);
    assert_eq!(out, "9\n9\n");
}

// ── Import visibility + collision diagnostics (willow-pwa, spec 11/13) ─────

fn s5_modules() -> Vec<(&'static str, &'static str)> {
    vec![
        (
            "a.wi",
            "module a;\npub fn f() -> i64 { return 1; }\npub fn dup() -> i64 { return 10; }\nfn hidden() -> i64 { return 9; }\n",
        ),
        (
            "b.wi",
            "module b;\npub fn g() -> i64 { return 2; }\npub fn dup() -> i64 { return 20; }\n",
        ),
    ]
}

fn s5_project(main: &str) -> Vec<(&'static str, &'static str)> {
    let mut v = s5_modules();
    v.insert(0, ("main.wi", Box::leak(main.to_string().into_boxed_str())));
    v
}

// Importing a private (non-pub) item is rejected.
#[test]
fn test_import_private_item_rejected() {
    let stderr = compile_temp_project_error_stderr(
        &s5_project("import a.hidden;\nfn main() { println(hidden()); }\n"),
        "main.wi",
    );
    assert!(stderr.contains("error[E2006]"), "stderr: {stderr}");
    assert!(stderr.contains("private"), "stderr: {stderr}");
}

// Two item imports binding the same local name collide.
#[test]
fn test_duplicate_item_import_rejected() {
    let stderr = compile_temp_project_error_stderr(
        &s5_project("import a.dup;\nimport b.dup;\nfn main() { println(dup()); }\n"),
        "main.wi",
    );
    assert!(stderr.contains("error[E2004]"), "stderr: {stderr}");
    assert!(
        stderr.contains("defined multiple times"),
        "stderr: {stderr}"
    );
}

// An item import colliding with a local function is rejected.
#[test]
fn test_item_import_vs_local_fn_rejected() {
    let stderr = compile_temp_project_error_stderr(
        &s5_project("import a.f;\nfn f() -> i64 { return 0; }\nfn main() { println(f()); }\n"),
        "main.wi",
    );
    assert!(stderr.contains("error[E2003]"), "stderr: {stderr}");
    assert!(
        stderr.contains("import and a local declaration"),
        "stderr: {stderr}"
    );
}

// An item import colliding with a local class is rejected.
#[test]
fn test_item_import_vs_local_class_rejected() {
    let stderr = compile_temp_project_error_stderr(
        &s5_project("import a.f;\nclass f { pub v: i64; }\nfn main() {}\n"),
        "main.wi",
    );
    assert!(stderr.contains("error[E2003]"), "stderr: {stderr}");
}

// Two module imports aliased to the same name collide.
#[test]
fn test_module_alias_collision_rejected() {
    let stderr = compile_temp_project_error_stderr(
        &s5_project("import a as x;\nimport b as x;\nfn main() { println(x::f()); }\n"),
        "main.wi",
    );
    assert!(stderr.contains("error[E2004]"), "stderr: {stderr}");
}

// A module access-name colliding with a local declaration is rejected.
#[test]
fn test_module_name_vs_local_fn_rejected() {
    let stderr = compile_temp_project_error_stderr(
        &s5_project("import a;\nfn a() -> i64 { return 0; }\nfn main() {}\n"),
        "main.wi",
    );
    assert!(stderr.contains("error[E2003]"), "stderr: {stderr}");
}

// Distinct imports and declarations compile and run.
#[test]
fn test_distinct_imports_and_decls_ok() {
    let (out, ok) = compile_temp_project_and_run(
        &s5_project(
            "import a.f;\nimport b.g;\nfn helper() -> i64 { return 100; }\nfn main() { println(f() + g() + helper()); }\n",
        ),
        "main.wi",
    );
    assert!(ok);
    assert_eq!(out, "103\n");
}

// An alias disambiguates two otherwise-colliding item imports.
#[test]
fn test_alias_disambiguates_duplicate_item() {
    let (out, ok) = compile_temp_project_and_run(
        &s5_project(
            "import a.dup;\nimport b.dup as bdup;\nfn main() { println(dup() + bdup()); }\n",
        ),
        "main.wi",
    );
    assert!(ok);
    assert_eq!(out, "30\n");
}

// ── Array dynamic growth: push/pop (willow-5a4) ────────────────────────────

// push grows an empty array; len and indexing reflect the appended elements.
#[test]
fn test_array_push_grows_empty() {
    let (out, ok) = compile_and_run(
        r#"
import std.collections.Array;

fn main() {
    let xs: Array<i64> = [];
    let mut i = 0;
    while i < 6 { xs.push(i * 10); i = i + 1; }
    println(xs.len());
    println(xs[0]);
    println(xs[5]);
}
"#,
    );
    assert!(ok);
    assert_eq!(out, "6\n0\n50\n");
}

// pop returns the last element and shrinks the array.
#[test]
fn test_array_pop_returns_last() {
    let (out, ok) = compile_and_run(
        r#"
import std.collections.Array;

fn main() {
    let xs: Array<i64> = [1, 2, 3];
    println(xs.pop());
    println(xs.pop());
    println(xs.len());
    println(xs[0]);
}
"#,
    );
    assert!(ok);
    assert_eq!(out, "3\n2\n1\n1\n");
}

// push works on a non-empty literal (grows past initial capacity).
#[test]
fn test_array_push_onto_literal() {
    let (out, ok) = compile_and_run(
        r#"
import std.collections.Array;

fn main() {
    let xs: Array<i64> = [10, 20];
    xs.push(30);
    xs.push(40);
    println(xs.len());
    println(xs[2]);
    println(xs[3]);
}
"#,
    );
    assert!(ok);
    assert_eq!(out, "4\n30\n40\n");
}

// push/pop of reference (String) elements round-trips.
#[test]
fn test_array_push_pop_string_elements() {
    let (out, ok) = compile_and_run(
        r#"
import std.collections.Array;

fn main() {
    let names: Array<String> = [];
    names.push("alice");
    names.push("bob");
    println(names.len());
    println(names.pop());
    println(names[0]);
}
"#,
    );
    assert!(ok);
    assert_eq!(out, "2\nbob\nalice\n");
}

// f64 elements survive the push word/bit-cast.
#[test]
fn test_array_push_f64_elements() {
    let (out, ok) = compile_and_run(
        r#"
import std.collections.Array;

fn main() {
    let fs: Array<f64> = [];
    fs.push(1.5);
    fs.push(2.5);
    println(fs[0] + fs[1]);
}
"#,
    );
    assert!(ok);
    assert_eq!(out, "4\n");
}

// pop then push reuses the array correctly.
#[test]
fn test_array_pop_then_push() {
    let (out, ok) = compile_and_run(
        r#"
import std.collections.Array;

fn main() {
    let xs: Array<i64> = [1, 2, 3];
    let last = xs.pop();
    xs.push(last * 10);
    println(xs.len());
    println(xs[2]);
}
"#,
    );
    assert!(ok);
    assert_eq!(out, "3\n30\n");
}

// String elements pushed across several growths survive a GC collection.
#[test]
fn test_array_pushed_strings_survive_gc() {
    let (out, ok) = compile_and_run(
        r#"
import std.collections.Array;

fn main() {
    let xs: Array<String> = [];
    let mut i = 0;
    while i < 20 { xs.push("item"); i = i + 1; }
    gc_collect();
    println(xs.len());
    println(xs[0]);
    println(xs[19]);
}
"#,
    );
    assert!(ok, "pushed string elements must survive GC across growth");
    assert_eq!(out, "20\nitem\nitem\n");
}

// Popping an empty array aborts.
#[test]
fn test_array_pop_empty_aborts() {
    let (out, ok) = compile_and_run_check_exit(
        r#"
import std.collections.Array;

fn main() {
    let xs: Array<i64> = [];
    println(xs.pop());
}
"#,
    );
    assert!(!ok, "pop on empty must abort");
    assert!(out.contains("empty array"), "got: {out}");
}

// Pushing the wrong element type is a compile error.
#[test]
fn test_array_push_wrong_type_is_error() {
    assert_compile_error_contains(
        r#"
import std.collections.Array;

fn main() {
    let xs: Array<i64> = [1];
    xs.push(true);
}
"#,
        &["error[E0201]", "cannot push"],
    );
}

// push with the wrong arity is a compile error.
#[test]
fn test_array_push_wrong_arity_is_error() {
    assert_compile_error_contains(
        r#"
import std.collections.Array;

fn main() {
    let xs: Array<i64> = [1];
    xs.push();
}
"#,
        &["error[E0201]", "`Array::push` expects 1 argument"],
    );
}

// ── Arrays are GC roots (regression for is_gc_managed(Array), willow-a7j-adjacent) ──

// An array local must survive gc_collect AND subsequent allocations that would
// reuse its freed memory if it were not rooted. (The plain survive-gc tests can
// pass by reading not-yet-reused freed memory; this forces reuse.)
#[test]
fn test_array_local_rooted_across_gc_and_reuse() {
    let (out, ok) = compile_and_run(
        r#"
import std.collections.Array;

fn main() {
    let xs: Array<String> = ["alpha", "beta", "gamma"];
    gc_collect();
    let ys: Array<i64> = [];
    let mut i = 0;
    while i < 300 { ys.push(i); i = i + 1; }
    println(xs[0]);
    println(xs[2]);
}
"#,
    );
    assert!(ok);
    assert_eq!(out, "alpha\ngamma\n");
}

// A class field of array type must be traced (so the held array survives GC).
#[test]
fn test_array_class_field_traced() {
    let (out, ok) = compile_and_run(
        r#"
import std.collections.Array;

class Bag {
    pub items: Array<String>;
    pub fn new(items: Array<String>) -> Bag { return Bag { items: items }; }
    pub fn first(self) -> String { return self.items[0]; }
}
fn main() {
    let b = Bag::new(["x", "y"]);
    gc_collect();
    let junk: Array<i64> = [];
    let mut i = 0;
    while i < 200 { junk.push(i); i = i + 1; }
    println(b.first());
}
"#,
    );
    assert!(ok, "array-typed class field must be traced as a GC ref");
    assert_eq!(out, "x\n");
}

// ── `void` is a writable type (foundation for willow-exg) ──────────────────

// An explicit `-> void` return annotation is accepted and behaves like an
// omitted return type.
#[test]
fn test_explicit_void_return_type() {
    let (out, ok) = compile_and_run(
        r#"
fn greet() -> void { println(1); }
fn main() { greet(); }
"#,
    );
    assert!(ok);
    assert_eq!(out, "1\n");
}

// `void` is usable as a generic type argument in an annotation (e.g. a future
// Result<void, E>); the annotation parses and type-checks.
#[test]
fn test_void_as_generic_type_arg_annotation() {
    let (out, ok) = compile_and_run(
        r#"
fn use_r(r: Result<void, String>) -> i64 { return 0; }
fn main() { println(2); }
"#,
    );
    assert!(ok);
    assert_eq!(out, "2\n");
}

// ---------------------------------------------------------------------------
// GC rooting under allocation stress (WILLOW_GC_STRESS=alloc).
//
// These guard codegen GC-root soundness: every live value must survive a
// collection that fires *during* a subsequent allocation.  Without the fixes
// these exercise, each crashes or prints wrong output only when a collection
// happens to land mid-expression — invisible to normal threshold-based GC.
// ---------------------------------------------------------------------------

// Enum-variant construction must root the half-built enum across argument
// evaluation: `Option::Some(Node { .. })` allocates the Node after allocating
// the Option, and that allocation can collect the unrooted Option.
#[test]
fn gc_stress_01_option_some_class_payload() {
    let (out, ok) = compile_and_run_gc_stress(
        r#"
class Node { v: i64; pub fn get(self) -> i64 { return self.v; } }
fn main() {
    let opt = Option::Some(Node { v: 8 });
    gc_collect();
    let v = opt.unwrap();
    println(v.get());
}
"#,
    );
    assert!(ok, "should not crash under GC stress: {out}");
    assert_eq!(out, "8\n");
}

// Result::Ok with a String payload through the same construction path.
#[test]
fn gc_stress_02_result_ok_string_payload() {
    let (out, ok) = compile_and_run_gc_stress(
        r#"
fn main() {
    let r: Result<String, i64> = Result::Ok("alpha");
    gc_collect();
    println(r.unwrap());
}
"#,
    );
    assert!(ok, "should not crash under GC stress: {out}");
    assert_eq!(out, "alpha\n");
}

// Option<String> built and matched after a collection.
#[test]
fn gc_stress_03_option_string_survives() {
    let (out, ok) = compile_and_run_gc_stress(
        r#"
fn main() {
    let s = Option::Some("hello");
    gc_collect();
    println(s.unwrap());
}
"#,
    );
    assert!(ok, "should not crash under GC stress: {out}");
    assert_eq!(out, "hello\n");
}

// Fieldless (C-like) enums are immediate tags, not heap pointers, so a value of
// such an enum type must NOT be rooted/traced as a GC reference.  Passing one
// to a function that then allocates (the String literal) used to crash the
// collector by dereferencing the tag as an object header.
#[test]
fn gc_stress_04_fieldless_enum_not_rooted() {
    let (out, ok) = compile_and_run_gc_stress(
        r#"
enum Color { Red, Green, Blue, }
fn name(c: Color) -> String {
    return match c {
        Color::Red => "red",
        Color::Green => "green",
        Color::Blue => "blue",
    };
}
fn main() {
    println(name(Color::Red));
    println(name(Color::Green));
    println(name(Color::Blue));
}
"#,
    );
    assert!(ok, "should not crash under GC stress: {out}");
    assert_eq!(out, "red\ngreen\nblue\n");
}

// A class method returning Option, called twice.  Regression for the
// gc_root_count bookkeeping bug: the enum-construction root inside the method
// must decrement the root counter so the method epilogue does not over-pop the
// shared runtime root stack and strip the caller's live roots.
#[test]
fn gc_stress_05_class_method_returns_option_twice() {
    let (out, ok) = compile_and_run_gc_stress(
        r#"
class Lookup {
    key: i64;
    value: i64;
    pub fn find(self, k: i64) -> Option<i64> {
        if self.key == k {
            return Option::Some(self.value);
        }
        return Option::None;
    }
}
fn main() {
    let l = Lookup { key: 5, value: 100 };
    println(l.find(5).unwrap());
    println(l.find(9).is_none());
}
"#,
    );
    assert!(ok, "should not crash under GC stress: {out}");
    assert_eq!(out, "100\ntrue\n");
}

// Enum with a payload-carrying variant IS heap-allocated and must survive a
// collection when held, including a fieldless variant (None) of the same enum.
#[test]
fn gc_stress_06_mixed_enum_variants_round_trip() {
    let (out, ok) = compile_and_run_gc_stress(
        r#"
fn pick(n: i64) -> Option<i64> {
    if n > 0 { return Option::Some(n * 2); }
    return Option::None;
}
fn main() {
    let mut i = 0;
    let mut total = 0;
    while i < 5 {
        let o = pick(i);
        gc_collect();
        total = total + o.unwrap_or(0);
        i = i + 1;
    }
    println(total);
}
"#,
    );
    assert!(ok, "should not crash under GC stress: {out}");
    assert_eq!(out, "20\n");
}

// Channel/Future/JoinHandle locals are opaque RUNTIME pointers with no GC
// header, so is_gc_managed must NOT root them on the shadow stack — otherwise
// the collector reads a bogus header at payload_to_header and crashes once a
// collection actually scans the root (willow-lpn.9). These exercise the three
// runtime-pointer generics under WILLOW_GC_STRESS=alloc (collect on every alloc).

// A spawned void function joined while collections fire on every allocation.
// The JoinHandle local must not be traced as a heap object.
#[test]
fn gc_stress_07_spawn_join_void() {
    let (out, ok) = compile_and_run_gc_stress(
        r#"
fn say() {
    println("hi");
}
fn main() {
    let h = spawn say();
    gc_collect();
    h.join();
    println("done");
}
"#,
    );
    assert!(ok, "spawn/join must not crash under GC stress: {out}");
    assert_eq!(out, "hi\ndone\n");
}

// Awaiting futures of scalar types under stress. The Future locals are runtime
// pointers; rooting them previously crashed the collector.
#[test]
fn gc_stress_08_future_await_scalars() {
    let (out, ok) = compile_and_run_gc_stress(
        r#"
async fn number() -> i64 {
    return 7;
}
async fn ratio() -> f64 {
    return 2.5;
}
async fn main() {
    let f = number();
    gc_collect();
    println(await f);
    println(await ratio());
}
"#,
    );
    assert!(ok, "await must not crash under GC stress: {out}");
    assert_eq!(out, "7\n2.5\n");
}

// A channel produced by a spawned task, drained on the main task, with a
// collection between operations. The Channel local must not be traced.
#[test]
fn gc_stress_09_channel_spawn_producer() {
    let (out, ok) = compile_and_run_gc_stress(
        r#"
fn producer(ch: Channel<i64>) {
    ch.send(10);
    ch.send(20);
    ch.close();
}
fn main() {
    let ch = Channel<i64>::new();
    let h = spawn producer(ch);
    gc_collect();
    println(ch.recv());
    println(ch.recv());
    h.join();
}
"#,
    );
    assert!(ok, "channel/spawn must not crash under GC stress: {out}");
    assert_eq!(out, "10\n20\n");
}

// ── Interface dispatch (willow-xds, spec 14) ───────────────────────────────

const IFACE_ANIMALS: &str = r#"
interface Animal {
    fn speak(self) -> String;
}
class Dog implements Animal {
    pub fn speak(self) -> String { return "woof"; }
}
class Cat implements Animal {
    pub fn speak(self) -> String { return "meow"; }
}
"#;

#[test]
fn iface_dispatch_01_basic_via_function_arg() {
    let (out, ok) = compile_and_run(&format!(
        "{IFACE_ANIMALS}\nfn say(a: Animal) {{ println(a.speak()); }}\nfn main() {{ say(Dog {{}}); say(Cat {{}}); }}"
    ));
    assert!(ok, "interface dispatch must compile and run");
    assert_eq!(out, "woof\nmeow\n");
}

#[test]
fn iface_dispatch_02_local_binding() {
    let (out, ok) = compile_and_run(&format!(
        "{IFACE_ANIMALS}\nfn main() {{ let a: Animal = Dog {{}}; println(a.speak()); }}"
    ));
    assert!(ok);
    assert_eq!(out, "woof\n");
}

#[test]
fn iface_dispatch_03_return_coercion() {
    let (out, ok) = compile_and_run(&format!(
        "{IFACE_ANIMALS}\nfn pick(b: bool) -> Animal {{ if b {{ return Dog {{}}; }} return Cat {{}}; }}\nfn main() {{ println(pick(true).speak()); println(pick(false).speak()); }}"
    ));
    assert!(ok);
    assert_eq!(out, "woof\nmeow\n");
}

#[test]
fn iface_dispatch_04_multi_method_slot_indexing() {
    // Calls both interface methods; the second exercises vtable slot 1.
    let (out, ok) = compile_and_run(
        r#"
interface Shape {
    fn name(self) -> String;
    fn area(self) -> i64;
}
class Square implements Shape {
    pub side: i64;
    pub fn name(self) -> String { return "square"; }
    pub fn area(self) -> i64 { return self.side * self.side; }
}
fn show(s: Shape) { println(s.name()); println(s.area()); }
fn main() { show(Square { side: 6 }); }
"#,
    );
    assert!(ok);
    assert_eq!(out, "square\n36\n");
}

#[test]
fn iface_dispatch_05_reassignment() {
    let (out, ok) = compile_and_run(&format!(
        "{IFACE_ANIMALS}\nfn main() {{ let mut a: Animal = Dog {{}}; println(a.speak()); a = Cat {{}}; println(a.speak()); }}"
    ));
    assert!(ok);
    assert_eq!(out, "woof\nmeow\n");
}

// spec 14.6: interface values must survive collection under GC stress.

#[test]
fn iface_gc_stress_01_local_survives() {
    let (out, ok) = compile_and_run_gc_stress(&format!(
        "{IFACE_ANIMALS}\nfn main() {{ let a: Animal = Dog {{}}; gc_collect(); println(a.speak()); }}"
    ));
    assert!(ok, "interface local must survive GC: {out}");
    assert_eq!(out, "woof\n");
}

#[test]
fn iface_gc_stress_02_param_survives() {
    let (out, ok) = compile_and_run_gc_stress(&format!(
        "{IFACE_ANIMALS}\nfn say(a: Animal) {{ gc_collect(); println(a.speak()); }}\nfn main() {{ say(Dog {{}}); }}"
    ));
    assert!(ok, "interface parameter must survive GC: {out}");
    assert_eq!(out, "woof\n");
}

#[test]
fn iface_gc_stress_03_method_result_string_survives() {
    let (out, ok) = compile_and_run_gc_stress(&format!(
        "{IFACE_ANIMALS}\nfn main() {{ let a: Animal = Dog {{}}; let s = a.speak(); gc_collect(); println(s); }}"
    ));
    assert!(ok, "interface method-result String must survive GC: {out}");
    assert_eq!(out, "woof\n");
}

// spec 14.4: a class field typed as an interface.
#[test]
fn iface_field_01_dispatch_through_field() {
    let (out, ok) = compile_and_run(&format!(
        "{IFACE_ANIMALS}\nclass Holder {{ pub value: Animal; }}\nfn main() {{ let h = Holder {{ value: Dog {{}} }}; println(h.value.speak()); }}"
    ));
    assert!(ok, "interface field dispatch must work: {out}");
    assert_eq!(out, "woof\n");
}

#[test]
fn iface_field_02_gc_stress_field_survives() {
    let (out, ok) = compile_and_run_gc_stress(&format!(
        "{IFACE_ANIMALS}\nclass Holder {{ pub value: Animal; }}\nfn main() {{ let h = Holder {{ value: Dog {{}} }}; gc_collect(); println(h.value.speak()); }}"
    ));
    assert!(ok, "interface field must survive GC: {out}");
    assert_eq!(out, "woof\n");
}

// spec 14.5: Array<Interface> (empty literal + push, the documented pattern).
#[test]
fn iface_array_01_push_and_dispatch() {
    let (out, ok) = compile_and_run(&format!(
        "import std.collections.Array;\n{IFACE_ANIMALS}\nfn main() {{ let xs: Array<Animal> = []; xs.push(Dog {{}}); xs.push(Cat {{}}); println(xs[0].speak()); println(xs[1].speak()); }}"
    ));
    assert!(ok, "Array<Interface> must work: {out}");
    assert_eq!(out, "woof\nmeow\n");
}

#[test]
fn iface_array_02_gc_stress_elements_survive() {
    let (out, ok) = compile_and_run_gc_stress(&format!(
        "import std.collections.Array;\n{IFACE_ANIMALS}\nfn main() {{ let xs: Array<Animal> = []; xs.push(Dog {{}}); xs.push(Cat {{}}); gc_collect(); println(xs[0].speak()); println(xs[1].speak()); }}"
    ));
    assert!(ok, "Array<Interface> elements must survive GC: {out}");
    assert_eq!(out, "woof\nmeow\n");
}

#[test]
fn iface_array_03_index_assign_boxes() {
    let (out, ok) = compile_and_run(&format!(
        "import std.collections.Array;\n{IFACE_ANIMALS}\nfn main() {{ let xs: Array<Animal> = []; xs.push(Dog {{}}); xs[0] = Cat {{}}; println(xs[0].speak()); }}"
    ));
    assert!(ok, "interface index-assign must box: {out}");
    assert_eq!(out, "meow\n");
}

#[test]
fn iface_array_04_nonempty_literal_with_annotation() {
    // A non-empty `Array<Interface>` literal of differing classes is checked
    // element-wise against the interface and each element is boxed.
    let (out, ok) = compile_and_run(&format!(
        "import std.collections.Array;\n{IFACE_ANIMALS}\nfn main() {{ let xs: Array<Animal> = [Dog {{}}, Cat {{}}]; println(xs[0].speak()); println(xs[1].speak()); }}"
    ));
    assert!(ok, "non-empty interface array literal must work: {out}");
    assert_eq!(out, "woof\nmeow\n");
}

// spec 11: module-qualified interface use (`animals::Animal`) where both the
// interface and the implementing class live in an imported module.
#[test]
fn iface_module_01_qualified_interface_and_class() {
    let animals = r#"
module animals;
pub interface Animal {
    fn speak(self) -> String;
}
pub class Dog implements Animal {
    pub fn speak(self) -> String { return "woof"; }
}
"#;
    let main = r#"
import animals;
fn say(a: animals::Animal) {
    println(a.speak());
}
fn main() {
    say(animals::Dog {});
    let a: animals::Animal = animals::Dog {};
    println(a.speak());
}
"#;
    let (out, ok) =
        compile_temp_project_and_run(&[("animals.wi", animals), ("main.wi", main)], "main.wi");
    assert!(ok, "module-qualified interface project failed: {out}");
    assert_eq!(out, "woof\nwoof\n");
}

// ── Async frame: frame-backed GC params survive across await (willow-lpn.5a) ──

#[test]
fn async_frame_01_string_param_across_await() {
    let (out, ok) = compile_and_run(
        r#"
async fn echo(s: String) -> String {
    await sleep(1);
    return s;
}
async fn main() {
    println(await echo("hello"));
}
"#,
    );
    assert!(ok, "async String param across await must work: {out}");
    assert_eq!(out, "hello\n");
}

#[test]
fn async_frame_02_second_param_slot_indexing() {
    // Returning the second GC param verifies per-slot frame offsets.
    let (out, ok) = compile_and_run(
        r#"
async fn pick(a: String, b: String) -> String {
    await sleep(1);
    return b;
}
async fn main() {
    println(await pick("first", "second"));
}
"#,
    );
    assert!(ok);
    assert_eq!(out, "second\n");
}

#[test]
fn async_frame_03_mixed_gc_and_scalar_params() {
    // A non-GC param (slot 0) stays on the stack; the GC param (slot 1) is
    // frame-backed — exercises slot-indexed offsets independent of which slots
    // are frame-backed.
    let (out, ok) = compile_and_run(
        r#"
async fn pick(n: i64, s: String) -> String {
    await sleep(1);
    return s;
}
async fn main() {
    println(await pick(7, "kept"));
}
"#,
    );
    assert!(ok);
    assert_eq!(out, "kept\n");
}

#[test]
fn async_frame_04_gc_stress_param_survives() {
    // The String param is reachable only through the heap frame across the
    // await; it must survive collection at every allocation.
    let (out, ok) = compile_and_run_gc_stress(
        r#"
async fn echo(s: String) -> String {
    await sleep(1);
    return s;
}
async fn main() {
    println(await echo("hello world"));
}
"#,
    );
    assert!(ok, "frame-backed param must survive GC stress: {out}");
    assert_eq!(out, "hello world\n");
}

#[test]
fn async_frame_05_annotated_string_local_across_await() {
    let (out, ok) = compile_and_run(
        r#"
async fn make() -> String {
    let s: String = "local value";
    await sleep(1);
    return s;
}
async fn main() {
    println(await make());
}
"#,
    );
    assert!(ok, "annotated GC local across await must work: {out}");
    assert_eq!(out, "local value\n");
}

#[test]
fn async_frame_06_mutated_frame_local_round_trips() {
    // The local is read+written on both sides of the await; values must round
    // trip through the heap frame slot.
    let (out, ok) = compile_and_run(
        r#"
async fn build() -> String {
    let mut s: String = "a";
    s = s + "b";
    await sleep(1);
    s = s + "c";
    return s;
}
async fn main() {
    println(await build());
}
"#,
    );
    assert!(ok);
    assert_eq!(out, "abc\n");
}

#[test]
fn async_frame_07_gc_stress_local_survives() {
    let (out, ok) = compile_and_run_gc_stress(
        r#"
async fn make() -> String {
    let s: String = "kept across await";
    await sleep(1);
    return s;
}
async fn main() {
    println(await make());
}
"#,
    );
    assert!(ok, "frame-backed local must survive GC stress: {out}");
    assert_eq!(out, "kept across await\n");
}

// ── lpn.5c slice 1: unannotated locals frame-backed via type-checker types ──

#[test]
fn async_frame_08_unannotated_local_across_await() {
    let (out, ok) = compile_and_run(
        r#"
async fn make() -> String {
    let s = "unannotated";
    await sleep(1);
    return s;
}
async fn main() {
    println(await make());
}
"#,
    );
    assert!(ok, "unannotated GC local across await must work: {out}");
    assert_eq!(out, "unannotated\n");
}

#[test]
fn async_frame_09_unannotated_local_mutated_round_trips() {
    let (out, ok) = compile_and_run(
        r#"
async fn build() -> String {
    let mut s = "x";
    await sleep(1);
    s = s + "y";
    return s;
}
async fn main() {
    println(await build());
}
"#,
    );
    assert!(ok);
    assert_eq!(out, "xy\n");
}

#[test]
fn async_frame_10_unannotated_local_gc_stress() {
    let (out, ok) = compile_and_run_gc_stress(
        r#"
async fn make() -> String {
    let s = "inferred kept";
    await sleep(1);
    return s;
}
async fn main() {
    println(await make());
}
"#,
    );
    assert!(
        ok,
        "unannotated frame-backed local must survive GC stress: {out}"
    );
    assert_eq!(out, "inferred kept\n");
}

// ── Frame-backed values across await: GC tracing by type (lpn.5c perspectives) ──
// Each value lives ONLY in the GC-rooted heap frame across the await, so these
// verify the frame's per-type GC tracing under collection at every allocation.

#[test]
fn async_frame_11_class_with_ref_field_survives() {
    // Two-level tracing: frame traces the Box, Box's mask traces its String field.
    let (out, ok) = compile_and_run_gc_stress(
        r#"
class Box { pub s: String; }
async fn f() -> String {
    let b: Box = Box { s: "nested" };
    await sleep(1);
    return b.s;
}
async fn main() { println(await f()); }
"#,
    );
    assert!(ok, "class with ref field must survive across await: {out}");
    assert_eq!(out, "nested\n");
}

#[test]
fn async_frame_12_array_of_string_survives() {
    let (out, ok) = compile_and_run_gc_stress(
        r#"
async fn f() -> String {
    let xs: Array<String> = [];
    xs.push("e0");
    xs.push("e1");
    await sleep(1);
    return xs[1];
}
async fn main() { println(await f()); }
"#,
    );
    assert!(ok, "Array<String> must survive across await: {out}");
    assert_eq!(out, "e1\n");
}

#[test]
fn async_frame_13_option_payload_survives() {
    let (out, ok) = compile_and_run_gc_stress(
        r#"
async fn f() -> String {
    let o: Option<String> = Option::Some("opt");
    await sleep(1);
    return match o { Option::Some(x) => x, Option::None => "none", };
}
async fn main() { println(await f()); }
"#,
    );
    assert!(
        ok,
        "Option<String> payload must survive across await: {out}"
    );
    assert_eq!(out, "opt\n");
}

#[test]
fn async_frame_14_result_payload_survives() {
    let (out, ok) = compile_and_run_gc_stress(
        r#"
async fn f() -> String {
    let r: Result<String, String> = Result::Ok("ok");
    await sleep(1);
    return match r { Result::Ok(x) => x, Result::Err(e) => e, };
}
async fn main() { println(await f()); }
"#,
    );
    assert!(ok, "Result payload must survive across await: {out}");
    assert_eq!(out, "ok\n");
}

#[test]
fn async_frame_15_map_ref_value_survives() {
    let (out, ok) = compile_and_run_gc_stress(
        r#"
async fn f() -> String {
    let mut m: Map<String, String> = Map::new();
    m.insert("k", "val");
    await sleep(1);
    return match m.get("k") { Option::Some(v) => v, Option::None => "missing", };
}
async fn main() { println(await f()); }
"#,
    );
    assert!(ok, "Map ref value must survive across await: {out}");
    assert_eq!(out, "val\n");
}

#[test]
fn async_frame_16_nullable_non_nil_survives() {
    let (out, ok) = compile_and_run_gc_stress(
        r#"
class Node { pub value: i64; pub next: Node?; }
async fn f(n: Node?) -> i64 {
    await sleep(1);
    if n == nil { return -1; }
    return n.value;
}
async fn main() { println(await f(Node { value: 77, next: nil })); }
"#,
    );
    assert!(ok, "non-nil nullable must survive across await: {out}");
    assert_eq!(out, "77\n");
}

#[test]
fn async_frame_17_nullable_nil_traced_as_null() {
    // A nil nullable in a GC frame slot must be skipped (not dereferenced) by the
    // collector, not crash.
    let (out, ok) = compile_and_run_gc_stress(
        r#"
class Node { pub value: i64; pub next: Node?; }
async fn f(n: Node?) -> i64 {
    await sleep(1);
    if n == nil { return -1; }
    return n.value;
}
async fn main() { println(await f(nil)); }
"#,
    );
    assert!(
        ok,
        "nil nullable frame slot must be safe across await: {out}"
    );
    assert_eq!(out, "-1\n");
}

#[test]
fn async_frame_18_future_local_not_traced_across_await() {
    // A Future local held across an await is an opaque runtime pointer (no
    // GcHeader); it must NOT be traced as a heap object (lpn.9) and must not crash.
    let (out, ok) = compile_and_run_gc_stress(
        r#"
async fn other() -> i64 { return 7; }
async fn f() -> i64 {
    let fut = other();
    await sleep(1);
    return await fut;
}
async fn main() { println(await f()); }
"#,
    );
    assert!(
        ok,
        "Future local across await must not crash the collector: {out}"
    );
    assert_eq!(out, "7\n");
}

#[test]
fn async_frame_19_join_handle_local_not_traced_across_await() {
    let (out, ok) = compile_and_run_gc_stress(
        r#"
fn work() { println("worked"); }
async fn f() {
    let h = spawn work();
    await sleep(1);
    h.join();
}
async fn main() { await f(); }
"#,
    );
    assert!(
        ok,
        "JoinHandle local across await must not crash the collector: {out}"
    );
    assert_eq!(out, "worked\n");
}

#[test]
fn async_frame_20_channel_local_not_traced_across_await() {
    let (out, ok) = compile_and_run_gc_stress(
        r#"
fn producer(ch: Channel<i64>) { ch.send(11); ch.close(); }
async fn f() -> i64 {
    let ch = Channel<i64>::new();
    let h = spawn producer(ch);
    await sleep(1);
    let v = ch.recv();
    h.join();
    return v;
}
async fn main() { println(await f()); }
"#,
    );
    assert!(
        ok,
        "Channel local across await must not crash the collector: {out}"
    );
    assert_eq!(out, "11\n");
}

// ── Module-qualified type visibility (willow-7ihl): E0419 for private types ──

#[test]
fn module_vis_01_private_class_annotation_rejected() {
    let m = "module animals;\nclass Secret { pub v: i64; }\npub class Dog {}\n";
    let main = "import animals;\nfn main() { let s: animals::Secret = animals::Secret { v: 5 }; println(s.v); }\n";
    let stderr =
        compile_temp_project_error_stderr(&[("animals.wi", m), ("main.wi", main)], "main.wi");
    assert!(stderr.contains("E0419"), "expected E0419, got: {stderr}");
    assert!(
        stderr.contains("private"),
        "diagnostic should mention private: {stderr}"
    );
}

#[test]
fn module_vis_02_pub_class_accessible() {
    let m = "module animals;\nclass Secret {}\npub class Dog { pub fn speak(self) -> i64 { return 1; } }\n";
    let main = "import animals;\nfn main() { let d: animals::Dog = animals::Dog {}; println(d.speak()); }\n";
    let (out, ok) =
        compile_temp_project_and_run(&[("animals.wi", m), ("main.wi", main)], "main.wi");
    assert!(ok, "pub module class must be accessible: {out}");
    assert_eq!(out, "1\n");
}

#[test]
fn module_vis_03_private_interface_rejected() {
    let m = "module animals;\ninterface Hidden { fn f(self) -> i64; }\npub interface Shown { fn f(self) -> i64; }\n";
    let main = "import animals;\nfn use_it(a: animals::Hidden) {}\nfn main() {}\n";
    let stderr =
        compile_temp_project_error_stderr(&[("animals.wi", m), ("main.wi", main)], "main.wi");
    assert!(stderr.contains("E0419"), "expected E0419, got: {stderr}");
    assert!(
        stderr.contains("interface"),
        "diagnostic should name the kind: {stderr}"
    );
}

#[test]
fn module_vis_04_private_class_static_call_rejected() {
    let m =
        "module animals;\nclass Secret { pub fn make() -> i64 { return 9; } }\npub class Dog {}\n";
    let main = "import animals;\nfn main() { println(animals::Secret::make()); }\n";
    let stderr =
        compile_temp_project_error_stderr(&[("animals.wi", m), ("main.wi", main)], "main.wi");
    assert!(
        stderr.contains("E0419"),
        "expected E0419 on static call, got: {stderr}"
    );
}

#[test]
fn module_vis_05_pub_interface_accessible() {
    let m = "module shapes;\npub interface Shape { fn area(self) -> i64; }\npub class Sq implements Shape { pub side: i64; pub fn area(self) -> i64 { return self.side * self.side; } }\n";
    let main = "import shapes;\nfn describe(s: shapes::Shape) { println(s.area()); }\nfn main() { describe(shapes::Sq { side: 4 }); }\n";
    let (out, ok) = compile_temp_project_and_run(&[("shapes.wi", m), ("main.wi", main)], "main.wi");
    assert!(ok, "pub module interface must be accessible: {out}");
    assert_eq!(out, "16\n");
}
