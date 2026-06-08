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

fn temp_path(path: impl AsRef<Path>) -> String {
    std::env::temp_dir()
        .join(path)
        .to_string_lossy()
        .into_owned()
}

fn remove_output_artifacts(bin_path: &str) {
    let _ = fs::remove_file(bin_path);
    let _ = fs::remove_file(format!("{bin_path}.wsmap"));
}

fn contains_path_fragment(haystack: &str, slash_fragment: &str) -> bool {
    haystack.contains(slash_fragment) || haystack.contains(&slash_fragment.replace('/', "\\"))
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
        .join(if cfg!(target_env = "msvc") {
            "willow_runtime.lib"
        } else {
            "libwillow_runtime.a"
        })
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
    let src_path = temp_path(format!("willow_test_{}.wi", id));
    let bin_path = temp_path(format!("willow_test_{}", id));

    fs::write(&src_path, source).unwrap();

    let compiler = env!("CARGO_BIN_EXE_willowc");
    let output = Command::new(compiler)
        .args(["build", &src_path, "-o", &bin_path])
        .output()
        .expect("failed to run compiler");

    if !output.status.success() {
        eprintln!(
            "compiler stdout:\n{}",
            String::from_utf8_lossy(&output.stdout)
        );
        eprintln!(
            "compiler stderr:\n{}",
            String::from_utf8_lossy(&output.stderr)
        );
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
    let src_path = temp_path(format!("willow_exit_test_{}.wi", id));
    let bin_path = temp_path(format!("willow_exit_test_{}", id));

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
    compile_and_run_gc_stress_mode(source, "alloc")
}

fn compile_and_run_gc_stress_all(source: &str) -> (String, bool) {
    compile_and_run_gc_stress_mode(source, "all")
}

fn compile_and_run_gc_stress_mode(source: &str, mode: &str) -> (String, bool) {
    let id = unique_test_id();
    let src_path = temp_path(format!("willow_gcstress_test_{}.wi", id));
    let bin_path = temp_path(format!("willow_gcstress_test_{}", id));

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
        .env("WILLOW_GC_STRESS", mode)
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

/// Like `compile_and_run` but runs the binary with extra environment variables.
/// Returns `(stdout, binary_exit_ok)`.
fn compile_and_run_with_env(source: &str, env: &[(&str, &str)]) -> (String, bool) {
    let id = unique_test_id();
    let src_path = temp_path(format!("willow_env_test_{}.wi", id));
    let bin_path = temp_path(format!("willow_env_test_{}", id));

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

    let mut cmd = Command::new(&bin_path);
    for (key, value) in env {
        cmd.env(key, value);
    }
    let out = cmd.output().expect("failed to run binary");

    let _ = fs::remove_file(&src_path);
    remove_output_artifacts(&bin_path);

    (
        String::from_utf8_lossy(&out.stdout).into_owned(),
        out.status.success(),
    )
}

fn compile_and_run_with_program_args(source: &str, program_args: &[&str]) -> (String, bool) {
    let id = unique_test_id();
    let src_path = temp_path(format!("willow_args_test_{}.wi", id));
    let bin_path = temp_path(format!("willow_args_test_{}", id));

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
    let src_path = temp_path(format!("willow_run_args_test_{}.wi", id));

    fs::write(&src_path, source).unwrap();

    let compiler = env!("CARGO_BIN_EXE_willowc");
    let mut command = Command::new(compiler);
    command.args(["run", &src_path, "--"]);
    command.args(program_args);
    let out = command.output().expect("failed to run compiler");

    let _ = fs::remove_file(&src_path);
    let bin_path = temp_path(format!("willow_run_{}", stem_for_test(&src_path)));
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
    let bin_path = temp_path(format!("willow_example_test_{}", id));

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
    let dir_path = temp_path(format!("willow_project_test_{}", id));
    let bin_path = temp_path(format!("willow_project_test_{}_bin", id));

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
    let dir_path = temp_path(format!("willow_project_error_test_{}", id));
    let bin_path = temp_path(format!("willow_project_error_test_{}_bin", id));

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
    let src_path = temp_path(format!("willow_err_{}.wi", id));
    let bin_path = temp_path(format!("willow_err_{}", id));

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
    let src_path = temp_path(format!("willow_diag_{}.wi", id));
    let bin_path = temp_path(format!("willow_diag_{}", id));

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
            "cannot concatenate `String` with `i64`",
            ".toString()",
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
    let p = new Point(10, 20);
    println(p.get_x());
    println(p.get_y());
}
"#;
    let (out, ok) = compile_and_run(src);
    assert!(ok, "compilation failed");
    assert_eq!(out, "10\n20\n");
}

#[test]
fn test_class_gc_ref_mask_rejects_gc_field_beyond_coverage() {
    let mut fields = String::new();
    let mut args = Vec::new();
    for i in 0..63 {
        fields.push_str(&format!("    n{i}: i64;\n"));
        args.push(i.to_string());
    }
    fields.push_str("    late: String;\n");
    args.push("\"late\"".to_string());

    let src = format!(
        r#"
class TooWide {{
{fields}}}

fn main() {{
    let value = new TooWide({});
    println(1);
}}
"#,
        args.join(", ")
    );
    assert_compile_error_contains(
        &src,
        &[
            "TooWide",
            "late",
            "outside gc_ref_mask coverage",
            "class_type_id",
        ],
    );
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
    let c = new Counter(5);
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
    let b = new Box(99);
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
    let b = new Box(42);
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
    let n = new Node(7);
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
    let n = new Node(42);
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
    let tail = new Node(2, nil);
    return new Node(1, tail);
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
    let head = new Node(1, nil);
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
        ("example/async_yield.wi", "1\n2\n11\n12\n3\n"),
        ("example/async_concurrent.wi", "102\n203\n"),
        ("example/async_cooperative.wi", "1\n2\n3\n"),
        ("example/async_string_param.wi", "hello, willow\n"),
        ("example/booleans.wi", "true\nfalse\ntrue\ntrue\n"),
        ("example/class_hierarchy.wi", "3\n"),
        ("example/class.wi", "42\n"),
        ("example/command_line_args.wi", "0\n0\ntrue\ntrue\n"),
        ("example/constructor_visibility.wi", "pub\n42\n7\n"),
        ("example/constructors.wi", "John\n20\n7\n"),
        ("example/control_flow.wi", "120\n"),
        ("example/debug_source_map.wi", "12\n"),
        ("example/early_return.wi", "7\n0\n12\n"),
        ("example/example.wi", "50\ntrue\n"),
        ("example/fib.wi", "6765\n"),
        ("example/fib_bench.wi", "6765\n"),
        ("example/f64_parse.wi", "3.5\ntrue\nNaN\nparse failed\n"),
        ("example/floats.wi", "4\ntrue\n-4\n"),
        ("example/fn_values.wi", "20\n25\n30\n107\n104\n"),
        ("example/for_loops.wi", "6\n1\n2\n3\n5050\n9\n"),
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
        ("example/generic_interfaces.wi", "10\nhello\nhello\nworld\n"),
        (
            "example/generic_interface_multi_instantiation.wi",
            "file\nfile\n",
        ),
        ("example/default_methods.wi", "Hello, Rex!\nBEEP Unit-7!\n"),
        (
            "example/interface_inheritance.wi",
            "Rex / Sam\n<Rex>\n<Rex>\n",
        ),
        (
            "example/interface_downcast.wi",
            "woof\nmeow\nNemo is quiet\n",
        ),
        ("example/subclass_interface.wi", "dog\n4\npuppy\n4\n"),
        ("example/virtual_dispatch.wi", "19\n"),
        ("example/error_conversion.wi", "14\n1042\n"),
        ("example/main_result.wi", "42\n"),
        (
            "example/to_string.wi",
            "answer = 42\nok = true\npi = 3.5\np = (3, 4)\n",
        ),
        ("example/many_tasks.wi", "55\n"),
        ("example/maps.wi", "2\n31\n25\n-1\ntrue\nfalse\ntwo\n"),
        ("example/module_alias_demo/main.wi", "5\n16\n"),
        ("example/module_class_demo/main.wi", "42\n12\n"),
        ("example/module_demo/main.wi", "12\n14\n"),
        ("example/module_enum_demo/main.wi", "1\n2\n42\n"),
        ("example/direct_import_demo/main.wi", "7\n1\n99\n"),
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
        ("example/range_value.wi", "2\n6\n4\n14\n0\n1\n2\n"),
        (
            "example/references.wi",
            "11\n22\ntrue\nhi!\nhi?\nold box\nold box!\nnew box\n3\n",
        ),
        (
            "example/rust_runtime_smoke.wi",
            "rust runtime\n42\n10\n21\n0\n",
        ),
        ("example/channel_producer.wi", "10\n20\n30\n"),
        (
            "example/concurrent_counts.wi",
            "101\n201\n301\n102\n202\n302\n103\n203\n303\n104\n204\n304\n105\n205\n305\n106\n206\n306\n107\n207\n307\n108\n208\n308\n109\n209\n309\n110\n210\n310\n",
        ),
        ("example/coop_select.wi", "100\n200\n300\n"),
        ("example/parallel_tasks.wi", "55\n144\n610\n42\nfalse\n"),
        ("example/select.wi", "0\n42\n7\n"),
        ("example/self_demo.wi", "10\n10\n10\n"),
        ("example/spawn_join.wi", "9\n16\n25\n42\n"),
        ("example/static_inheritance.wi", "base\nbase\n3\nok\n"),
        ("example/static_members.wi", "3\n25\n40\n42\n"),
        ("example/static_mut.wi", "0\n10\n42\nstart\ndone\n"),
        (
            "example/static_properties.wi",
            "1\nwillow\ntrue\n1.5\n20\n100\n",
        ),
        ("example/std_imports.wi", "1\n42\n7\n-1\n"),
        ("example/strings.wi", "Hello, Willow\nstring concat\n"),
        ("example/ternary.wi", "1\n-1\n0\n20\n99\n15\n8\n1\n"),
        ("example/types.wi", "10\n2.5\n10\n78.53975\ntrue\n"),
        ("example/super_class.wi", "ann\njohn\nben\n"),
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
fn test_future_example_catalog_covers_constructor_init_diagnostics() {
    let source = fs::read_to_string("example/future/diagnostic_constructor_init_rules.wi")
        .expect("missing constructor init diagnostic example");
    assert!(source.contains("// status: future"));
    assert!(source.contains("// feature: constructor init diagnostics"));
    assert!(source.contains("static init(self)"));
    assert!(source.contains("fn init(self)"));
    assert!(source.contains("static fn init()"));
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
    let src_path = temp_path(format!("willow_sourcemap_{}.wi", id));
    let bin_path = temp_path(format!("willow_sourcemap_{}", id));

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
    let src_path = temp_path(format!("willow_release_sourcemap_{}.wi", id));
    let bin_path = temp_path(format!("willow_release_sourcemap_{}", id));
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
    let src_path = temp_path(format!("willow_release_debug_sourcemap_{}.wi", id));
    let bin_path = temp_path(format!("willow_release_debug_sourcemap_{}", id));
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
    let src_path = temp_path(format!("willow_runtime_metadata_{}.wi", id));
    let bin_path = temp_path(format!("willow_runtime_metadata_{}", id));

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
    let src_path = temp_path(format!("willow_async_metadata_{}.wi", id));
    let bin_path = temp_path(format!("willow_async_metadata_{}", id));

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
    let src_path = temp_path(format!("willow_ref_metadata_{}.wi", id));
    let bin_path = temp_path(format!("willow_ref_metadata_{}", id));

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
import std::collections::Array;

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
    let src_path = temp_path(format!("willow_release_runtime_metadata_{}.wi", id));
    let bin_path = temp_path(format!("willow_release_runtime_metadata_{}", id));

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
        "help: create `",
        "or check the import name",
    ] {
        assert!(
            stderr.contains(expected),
            "stderr did not contain `{expected}`:\n{stderr}"
        );
    }
    assert!(
        contains_path_fragment(&stderr, "missing_math/mod.wi"),
        "stderr did not contain missing_math/mod.wi:\n{stderr}"
    );
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

    for expected in ["error[E0401]", "unresolved import `tools::missing_math`"] {
        assert!(
            stderr.contains(expected),
            "stderr did not contain `{expected}`:\n{stderr}"
        );
    }
    for expected in ["tools/missing_math.wi", "tools/missing_math/mod.wi"] {
        assert!(
            contains_path_fragment(&stderr, expected),
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
import std::collections::Array;

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
    let output = staticlib_symbols_output(runtime_lib);
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

#[cfg(not(all(windows, target_env = "msvc")))]
fn staticlib_symbols_output(runtime_lib: &Path) -> std::process::Output {
    let output = Command::new("nm")
        .arg(runtime_lib)
        .output()
        .expect("failed to inspect runtime staticlib with nm");
    assert!(output.status.success(), "nm failed for {runtime_lib:?}");
    output
}

#[cfg(all(windows, target_env = "msvc"))]
fn staticlib_symbols_output(runtime_lib: &Path) -> std::process::Output {
    let target = if cfg!(target_arch = "x86_64") {
        "x86_64-pc-windows-msvc"
    } else if cfg!(target_arch = "aarch64") {
        "aarch64-pc-windows-msvc"
    } else if cfg!(target_arch = "x86") {
        "i686-pc-windows-msvc"
    } else {
        panic!("unsupported Windows MSVC target architecture");
    };
    let mut cmd = cc::windows_registry::find_tool(target, "dumpbin.exe")
        .expect("failed to find MSVC dumpbin.exe")
        .to_command();
    cmd.arg("/SYMBOLS").arg(runtime_lib);
    let output = cmd
        .output()
        .expect("failed to inspect runtime staticlib with dumpbin");
    assert!(
        output.status.success(),
        "dumpbin failed for {runtime_lib:?}"
    );
    output
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
    let src_path = temp_path(format!("willow_rust_runtime_no_c_{id}.wi"));
    let bin_path = temp_path(format!("willow_rust_runtime_no_c_{id}"));
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
    let src_path = temp_path(format!("willow_runtime_cli_override_{id}.wi"));
    let bin_path = temp_path(format!("willow_runtime_cli_override_{id}"));
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
    let src_path = temp_path(format!("willow_runtime_env_override_{id}.wi"));
    let bin_path = temp_path(format!("willow_runtime_env_override_{id}"));
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
    let src_path = temp_path(format!("willow_runtime_missing_{id}.wi"));
    let bin_path = temp_path(format!("willow_runtime_missing_{id}"));
    let missing = temp_path(format!("willow_runtime_missing_{id}.a"));
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
    let node: Node = new Node(7, nil);
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
    let node: Node = new Node(9, nil);
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

// ---------------------------------------------------------------------------
// Async state machines + async stack traces — willow-9lw acceptance.
// ---------------------------------------------------------------------------

// WILLOW_WORKERS contract (willow-gyaa.4): the worker count is configurable but
// the cooperative runtime currently clamps to one active worker, so concurrent
// programs produce identical, deterministic results for any value. These pin
// that contract so a future parallel-worker change must keep results correct.
const WORKERS_CONCURRENT_SRC: &str = r#"
async fn compute(n: i64) -> i64 {
    await sleep(1);
    return n * n;
}
async fn main() {
    let a = compute(1);
    let b = compute(2);
    let c = compute(3);
    println(a.join() + b.join() + c.join());
}
"#;

#[test]
fn test_workers_default_runs_concurrent_program() {
    let (out, ok) = compile_and_run(WORKERS_CONCURRENT_SRC);
    assert!(ok);
    assert_eq!(out, "14\n"); // 1 + 4 + 9
}

#[test]
fn test_workers_env_does_not_change_result() {
    // 1, 4 (>active), 0 (invalid -> default), and garbage (-> default) must all
    // yield the same correct output.
    for value in ["1", "4", "0", "not-a-number"] {
        let (out, ok) =
            compile_and_run_with_env(WORKERS_CONCURRENT_SRC, &[("WILLOW_WORKERS", value)]);
        assert!(ok, "WILLOW_WORKERS={value} should run");
        assert_eq!(out, "14\n", "WILLOW_WORKERS={value} changed the result");
    }
}

#[test]
fn test_workers_high_count_still_correct_under_gc_stress() {
    let (out, ok) = compile_and_run_with_env(
        WORKERS_CONCURRENT_SRC,
        &[("WILLOW_WORKERS", "8"), ("WILLOW_GC_STRESS", "alloc")],
    );
    assert!(ok, "high worker count under GC stress should run");
    assert_eq!(out, "14\n");
}

// Concurrency unification (willow-h2vf Stage 1): an async fn call returns an
// eager Task that is joinable directly — no `spawn` needed.
// Case A (willow-h2vf.5): an async fn already returns Task<ReturnType>, so its
// declared return type must be the awaited value, not a task handle (E0809).
#[test]
fn test_async_return_task_handle_rejected_task() {
    assert_compile_error_contains(
        "async fn f() -> Task<i64> { return 1; }\nfn main() {}\n",
        &[
            "error[E0809]",
            "async fn return type must be the awaited value",
        ],
    );
}

#[test]
fn test_async_return_task_handle_rejected_future() {
    assert_compile_error_contains(
        "async fn f() -> Future<i64> { return 1; }\nfn main() {}\n",
        &["error[E0809]"],
    );
}

#[test]
fn test_async_return_task_handle_rejected_join_handle() {
    assert_compile_error_contains(
        "async fn f() -> JoinHandle<i64> { return 1; }\nfn main() {}\n",
        &["error[E0809]"],
    );
}

#[test]
fn test_async_return_plain_value_allowed() {
    // The awaited-value annotation (`-> i64`) is fine and yields a joinable task.
    let (out, ok) = compile_and_run(
        r#"
async fn f() -> i64 { await sleep(1); return 7; }
async fn main() { println(f().join()); }
"#,
    );
    assert!(ok);
    assert_eq!(out, "7\n");
}

#[test]
fn test_async_call_is_joinable_without_spawn() {
    let (out, ok) = compile_and_run(
        r#"
async fn work(x: i64) -> i64 { await sleep(1); return x * 2; }
async fn main() {
    let t = work(21);
    println(t.join());
}
"#,
    );
    assert!(ok);
    assert_eq!(out, "42\n");
}

#[test]
fn test_async_call_concurrent_joins_without_spawn() {
    let (out, ok) = compile_and_run(
        r#"
async fn work(id: i64, ticks: i64) -> i64 {
    let mut i = 0;
    while i < ticks { await sleep(1); i = i + 1; }
    return id * 100 + i;
}
async fn main() {
    let a = work(1, 2);
    let b = work(2, 3);
    println(a.join());
    println(b.join());
}
"#,
    );
    assert!(ok);
    assert_eq!(out, "102\n203\n");
}

#[test]
fn test_async_call_join_inline_without_spawn() {
    let (out, ok) = compile_and_run(
        r#"
async fn square(x: i64) -> i64 { await sleep(1); return x * x; }
async fn main() {
    println(square(5).join());
}
"#,
    );
    assert!(ok);
    assert_eq!(out, "25\n");
}

#[test]
fn test_async_9lw_two_concurrent_timers() {
    // Two spawned async workers each loop awaiting sleep; the single-threaded
    // executor drives both concurrently to completion.
    let (stdout, ok) = compile_and_run(
        r#"
async fn worker(id: i64, ticks: i64) -> i64 {
    let mut i = 0;
    while i < ticks {
        await sleep(1);
        i = i + 1;
    }
    return id * 100 + i;
}
async fn main() {
    let a = worker(1, 2);
    let b = worker(2, 3);
    println(a.join());
    println(b.join());
}
"#,
    );
    assert!(ok);
    assert_eq!(stdout, "102\n203\n");
}

#[test]
fn test_async_9lw_locals_live_across_await() {
    let (stdout, ok) = compile_and_run(
        r#"
async fn main() {
    let mut sum = 0;
    let mut i = 1;
    while i <= 3 {
        await sleep(1);
        sum = sum + i;
        i = i + 1;
    }
    println(sum);
}
"#,
    );
    assert!(ok);
    assert_eq!(stdout, "6\n");
}

#[test]
fn test_async_9lw_nested_await_passes_values() {
    let (stdout, ok) = compile_and_run(
        r#"
async fn inner(x: i64) -> i64 {
    await sleep(1);
    return x + 1;
}
async fn outer(x: i64) -> i64 {
    let a = await inner(x);
    let b = await inner(a);
    return b;
}
async fn main() {
    println(await outer(10));
}
"#,
    );
    assert!(ok);
    assert_eq!(stdout, "12\n");
}

#[test]
fn test_async_9lw_panic_renders_async_chain() {
    // A panic inside a suspended async fn renders the async future chain
    // (current task first), not just the immediate location — the cooperative
    // scheduler flattens the OS call stack, so this comes from runtime state.
    let (out, ok) = compile_and_run_check_exit(
        r#"
async fn inner(x: i64) -> i64 {
    await sleep(1);
    panic("boom in inner");
    return x;
}
async fn main() {
    let r = await inner(5);
    println(r);
}
"#,
    );
    assert!(!ok, "panic must make the program exit non-zero");
    assert!(out.contains("boom in inner"), "panic message: {out}");
    assert!(
        out.contains("async stack"),
        "expected an async stack trace: {out}"
    );
    assert!(out.contains("inner"), "chain should name `inner`: {out}");
    assert!(out.contains("main"), "chain should name `main`: {out}");
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
fn test_async_task_values_are_awaitable() {
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
    let number_task = number();
    let value = await number_task;
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
async fn work(x: i64) -> i64 {
    return x * 2;
}

fn main() {
    let h = work(21);
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
async fn square(x: i64) -> i64 {
    return x * x;
}

fn main() {
    let a = square(3);
    let b = square(4);
    let c = square(5);
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
fn test_select_block_is_supported() {
    // `select` is implemented (willow-7aj): a ready recv case runs its body.
    let (out, ok) = compile_and_run(
        r#"
fn main() {
    let ch = Channel<i64>::new();
    ch.send(5);
    select {
        let v = ch.recv() => { println(v); }
        default => { println(0); }
    }
}
"#,
    );
    assert!(ok, "{out}");
    assert_eq!(out, "5\n");
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
            "expected an awaitable",
        ],
    );
}

#[test]
fn test_async_infinite_loop_without_await_reports_e0808() {
    assert_compile_error_contains(
        r#"
async fn bad() {
    while true {
    }
}

fn main() {
    println(1);
}
"#,
        &[
            "error[E0808]",
            "async infinite loop has no suspension point",
            "`while true` in async code can monopolize the executor",
            "help: add an `await` in the loop body or make the loop terminate",
        ],
    );
}

// ---------------------------------------------------------------------------
// Function-pointer spawn (willow-spawn-fptr).
//
// `spawn f(args)` where `f` is a function VALUE (a `fn(...)` local — a named
// function reference or a lambda) used to run the call INLINE at the spawn site
// and merely wrap the result in a frame. It now compiles a `call_indirect` poll
// trampoline and schedules the task on the cooperative scheduler, exactly like
// `spawn named_fn(args)`. The 20 perspectives below cover that behavior.
//
//  1. named fn in a `fn` local, single i64 arg → join returns the result
//  2. lambda value spawned → join returns the result
//  3. two-arg fptr spawn → correct combined result
//  4. zero-arg fptr spawn
//  5. bool-returning fptr spawn
//  6. f64-returning fptr spawn
//  7. String-returning fptr spawn (GC-managed result slot in the frame mask)
//  8. String args through the indirect trampoline (GC-managed arg slots)
//  9. result usable in arithmetic after join
// 10. multiple fptr spawns joined in spawn order
// 11. multiple fptr spawns joined OUT of spawn order
// 12. fptr spawn is DEFERRED, not inline: a print after spawn precedes the
//     task's print (the observable behavior change vs. the old inline fallback)
// 13. fptr spawn matches named-fn spawn ordering (same scheduled semantics)
// 14. fptr passed in as a `fn` PARAMETER, then spawned
// 15. the same fptr local spawned twice → two independent tasks
// 16. two DIFFERENT fptr signatures in one program → distinct trampolines
// 17. fptr spawn result equals the equivalent direct call
// 18. four-arg fptr spawn → arg slot offsets stay correct
// 19. mixed arg types (i64 + bool) through one indirect trampoline
// 20. GC stress: String-returning + String-arg fptr spawn survives collection
//     during scheduling/join (frame + arg rooting correctness)
// ---------------------------------------------------------------------------

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
            "expected a task",
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
fn test_channel_recv_empty_open_panics_instead_of_defaulting() {
    let (out, ok) = compile_and_run_check_exit(
        r#"
fn main() {
    let ch: Channel<i64> = Channel::new();
    println(ch.recv());
}
"#,
    );
    assert!(!ok, "empty open recv must fail instead of returning 0");
    assert!(
        out.contains("runtime panic: recv on empty open channel would block"),
        "{out}"
    );
}

#[test]
fn test_channel_recv_closed_empty_still_returns_default() {
    let (out, ok) = compile_and_run(
        r#"
fn main() {
    let ch: Channel<i64> = Channel::new();
    ch.close();
    println(ch.recv());
}
"#,
    );
    assert!(ok, "closed empty recv keeps the existing default behavior");
    assert_eq!(out, "0\n");
}

#[test]
fn test_channel_target_producer_spawn_example_compiles_and_runs() {
    let (out, ok) = compile_and_run(
        r#"
async fn producer(ch: Channel<i64>) {
    ch.send(10);
    ch.send(20);
    ch.close();
}

fn main() {
    let ch = Channel<i64>::new();
    let h = producer(ch);
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

// ── Spawn / task: additional type and behaviour coverage ────────────────────

/// Void-return function can be spawned and joined; join completes without a value.
#[test]
fn test_spawn_void_function_join_completes() {
    let (out, ok) = compile_and_run(
        r#"
async fn say() {
    println("hi");
}

fn main() {
    let h = say();
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
async fn is_even(x: i64) -> bool {
    return x % 2 == 0;
}

fn main() {
    let h1 = is_even(4);
    let h2 = is_even(7);
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
async fn half(x: f64) -> f64 {
    return x / 2.0;
}

fn main() {
    let h = half(10.0);
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
async fn sum3(a: i64, b: i64, c: i64) -> i64 {
    return a + b + c;
}

fn main() {
    let h = sum3(10, 20, 30);
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
async fn square(x: i64) -> i64 {
    return x * x;
}

fn main() {
    let a = square(3);
    let b = square(4);
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
async fn double(x: i64) -> i64 {
    return x * 2;
}

fn main() {
    let h1 = double(5);
    let h2 = double(6);
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
    let src_path = temp_path(format!("willow_spawn_rel_{}.wi", id));
    let bin_path = temp_path(format!("willow_spawn_rel_{}", id));

    let source = r#"
async fn square(x: i64) -> i64 { return x * x; }
fn main() {
    let h = square(7);
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
            "expected a task",
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
    let box = new Box("ke" + "pt");
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
    box = new Box("after" + "!");
    gc_collect();
}

fn main() {
    let mut box = new Box("before");
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
    let counter = new Counter(10);
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
    let counter = new Counter(21);
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
    let user = new User("sh" + "u");
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

    pub static fn new(v: i64) -> User {
        return new User(v);
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
import std::collections::Array;

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
import std::collections::Array;

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
import std::collections::Array;

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
import std::collections::Array;

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
fn test_class_new_replaces_object_literal_construction() {
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
    return new A(value, new AA(value + 1));
}

fn main() {
    let a = make_a(40);
    println(consume(a));
}
"#;
    let (out, ok) = compile_and_run(src);
    assert!(ok, "new constructor program failed to compile or run");
    assert_eq!(out.trim(), "7");
}

#[test]
fn test_class_object_literal_rejected_with_new_guidance() {
    assert_compile_error_contains(
        r#"
class Point {
    x: i64;
    y: i64;
}

fn make_point() -> Point {
    return Point { x: 1, y: 2 };
}

fn main() {
    println(1);
}
"#,
        &[
            "error[E0847]",
            "object literal construction for `Point` is no longer supported",
            "use `new Point(...)`",
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

    pub static fn new(balance: i64) -> Account {
        return new Account(balance);
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
    let stderr = compile_error_stderr(
        r#"
fn main() {
    let @x = 1;
}
"#,
    );
    let temp_diag_prefix = format!(" --> {}", temp_path("willow_diag_"));
    for part in [
        "error[E0050]",
        "invalid character `@`",
        temp_diag_prefix.as_str(),
        ":3:9",
        "3 |     let @x = 1;",
        "        ^ invalid character",
    ] {
        assert!(
            stderr.contains(part),
            "stderr did not contain `{part}`:\n{stderr}"
        );
    }
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
    let b = new Box(42);
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
    let c = new Counter(7);
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
    let b = new Node(20, nil);
    let a = new Node(10, b);
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
    let src_path = temp_path(format!("willow_nil_rel_{}.wi", id));
    let bin_path = temp_path(format!("willow_nil_rel_{}", id));

    let source = r#"
class Box { pub value: i64; }
fn read(b: Box) -> i64 { return b.value; }
fn main() { println(read(new Box(99))); }
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
fn main() { println(new Box(1).value); }
"#;
    let id = unique_test_id();
    let src_path = temp_path(format!("willow_nil_msg_{}.wi", id));
    let bin_path = temp_path(format!("willow_nil_msg_{}", id));

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
    let bin_path = temp_path(format!("willow_leibniz_perf_{}", id));

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
    let max_ms = if cfg!(windows) { 1000 } else { 150 };
    assert!(
        elapsed.as_millis() < max_ms,
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
    let n = new Node(v);
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
    let n = new Node(v);
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
    let n = new Node(7);
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
    let b = new Bag(7);
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
    let c = new Calc(4);
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
    let c = new Child(5);
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
    let t = new Turbo(50);
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
    let c = new C(4);
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
    let b = new Box(1);
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
    let m = new Mix(10, 20);
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
    let d = new Dog(5);
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
    let s = new Shape(4);
    let t = new Triangle(2);
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
    let h = new Hidden(1);
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
    let c = new C(1);
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
    let w = new Wrapper(42);
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
    let w = new Worker(9);
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
    let v = new Vehicle(30);
    let c = new Car(50);
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
    let c = new BoundedCounter(8);
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
    let x = new Num(7);
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
    let r = new Rect(6, 4);
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
    let a = new Adder(10);
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
    let c = new Circle(5);
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
    let x = new Val(21);
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
fn make(x: i64, y: i64) -> Point { return new Point(x, y); }
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
    let f = new Flag(true);
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
    let m = new Msg("hello");
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
    let t = new Temp(36.6);
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
    let o = new Outer(new Inner(99));
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
    let a = new Box(1);
    let b = new Box(2);
    let c = new Box(3);
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
fn make_counter(start: i64) -> Counter { return new Counter(start); }
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
    let s1 = new Score(80);
    let s2 = new Score(40);
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
    let a = new Abs(-5);
    let b = new Abs(3);
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
    let p = new Pow(2);
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
    let p = new Point(10, 20);
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
    let p = new Pair(1, 2);
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
    let p = new Pair(1);
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
    let c = new Child();
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
    let b = new Base();
    let d = new Derived();
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
    let a = new A();
    let b = new B();
    let c = new C();
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
    let e = new Employee("Alice", "Eng");
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
    let base = new Animal(5);
    let cat = new Cat(3);
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
    let t = new Token(77);
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
    let s = new Stats(90, 3);
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
    pub fn make_inner(self, v: i64) -> Inner { return new Inner(v); }
}
fn main() {
    let o = new Outer();
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
    let a = new MaybeNum(Option::Some(10));
    let b = new MaybeNum(Option::None);
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
    let a = new Op(Result::Ok(7));
    let b = new Op(Result::Err("fail"));
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
    let l = new Lookup(5, 100);
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
    let d = new Divider(4);
    let z = new Divider(0);
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
    let a = new Acc(0);
    let b = new Acc(100);
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
    let t = new Tmp(42);
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
    let b = new Box(123);
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
    let tmp = new Obj(999);
}
fn main() {
    let live = new Obj(7);
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
    let a: Node? = new Node(5);
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
    return new Vec2(a.x() + b.x(), a.y() + b.y());
}
fn main() {
    let u = new Vec2(1, 2);
    let v = new Vec2(3, 4);
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
    let p = new Person("Jane", "Doe");
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
    let r = new Range(10, 20);
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
    let c = new Counter(3);
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
    let w = new Width(7);
    let h = new Height(3);
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
    let c = new Circle(2.0);
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
    let v = new Vehicle(30);
    let car = new Car(60);
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
    println(describe(new Tag(1)));
    println(describe(new Tag(2)));
    println(describe(new Tag(9)));
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
        return new Signed(n * -1, true);
    }
    return new Signed(n, false);
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
    let p = new Point(1);
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
    let p = new Point(1, 2);
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
    let d = new Dog();
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
    let a = new Animal();
    let d = new Dog();
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
fn make() -> Animal { return new Cat(); }
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
    let v: Vehicle = new Bike();
    let b = new Bike();
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
    let sq = new Square();
    let tr = new Triangle();
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
    println(outer(new Child()));
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
    let c = new C();
    accept_a(c);
    println(new A().tag());
    println(new B().tag());
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
    let leaf = new Leaf(99);
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
    let a = new Animal();
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
    let c = new Cat();
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
    let d = new Dog();
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
    let d = new Dog();
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
    let a: Animal? = new Cat();
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
    if use_it { return new Car(); }
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
    let leaf = new Leaf();
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
    let a = new Apple();
    let o = new Orange();
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
    let d: Animal? = new Dog(1);
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
    let s: Sub? = new Sub();
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
    let b = new Box(1);
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
    let a = new Box(1);
    let b = new Box(2);
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
    let b = new Box(42);
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
    let _a = new Box(1);
    let mid = gc_allocated_bytes();
    let _b = new Box(2);
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
    let t = new Tmp(99);
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
        let item = new Item(i);
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
    let n = new Node(5);
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
    let p = new Point(3, 4);
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
    let f = new Flag(true);
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
fn drop_obj() -> i64 { let o = new Obj(1); return o.get(); }
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
    let n = new Node(7);
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
    let a = new A(10);
    let b = new B(20);
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
fn drop_dead() -> i64 { let d = new Dead(0); return d.get(); }
fn main() {
    let live = new Live(5);
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
    let val = extract(new Wrap(99));
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
fn drop_one() -> i64 { let b = new Box(1); return b.get(); }
fn main() {
    let _ = drop_one();
    gc_collect();
    let b2 = new Box(2);
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
    let m = new Msg("hello");
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
    let tail = new Node(2, nil);
    let head = new Node(1, tail);
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
    let n = new Node(3, nil);
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
    let c = new Node(3, nil);
    let b = new Node(2, c);
    let a = new Node(1, b);
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
    let c = new Node(3, nil);
    let b = new Node(2, c);
    let a = new Node(1, b);
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
fn drop_child() -> i64 { let c = new Child(5); return c.get(); }
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
    let c = new Child(11);
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
fn drop_it() -> i64 { let s = new Secret(7); return s.get(); }
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
        let t = new Tmp(3);
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
    let b = new Box(9);
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
        let t = new Tmp(i);
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
    let _s = new Small(1);
    let after_small = gc_allocated_bytes();
    let _l = new Large(1, 2, 3, 4);
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
fn make(v: i64) -> Node { return new Node(v); }
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
    let b = new Box(7);
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
fn drop_one() -> i64 { let b = new Box(1); return b.get(); }
fn main() {
    let r1 = drop_one();
    gc_collect();
    let b = new Box(2);
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
    let opt = Option::Some(new Node(42));
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
    let opt = Option::Some(new Node(13));
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
    let r: Result<Node, String> = Result::Ok(new Node(7));
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
    let r: Result<Node, String> = Result::Ok(new Node(17));
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
    let q = new Quad(1, 2, 3, 4);
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
fn f3() -> i64 { let n = new Node(3); return n.get(); }
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
    let node = new Node(n);
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
    let b = new Box(fib(5));
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
    let a = new Node(1);
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
        let a = new A(10);
        return a.get();
    }
    let b = new B(20);
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
        let o = new Obj(i);
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
fn scope1() -> i64 { let a = new A(1); return a.get(); }
fn scope2() -> i64 { let b = new B(2); return b.get(); }
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
    let _a = new Box(1);
    let b1 = gc_allocated_bytes();
    let _b = new Box(2);
    let b2 = gc_allocated_bytes();
    let _c = new Box(3);
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
    let p = new Point(3, 4);
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
    let c = new Child(1, 2);
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
    let c = new Child(10, 5);
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
fn drop_c() -> i64 { let c = new C(7); return c.get(); }
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
    let c = new C(22);
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
    let c = new Child(3);
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
    let o = new Outer(5);
    let i = new Inner(o.val() * 2);
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
fn drop_a() -> i64 { let a = new A(1); return a.get(); }
fn main() {
    let r = drop_a();
    println(r);
    gc_collect();
    let zero = gc_allocated_bytes();
    let b = new B(2);
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
fn drop_it() -> f64 { let f = new Flt(1.5); return f.get(); }
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
    let f = new Flt(2.5);
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
    let n: Node? = new Node(5);
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
    let a: Node? = new Node(1);
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
    println(consume(new Box(77)));
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
    let opt = Option::Some(new Node(8));
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
        let _ = new Obj(i);
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
    let r = extract(new Wrap(100));
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
    let c = new Child(4);
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
    let n = new Node(5);
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
fn drop_a() -> i64 { let a = new A(1); return a.get(); }
fn main() {
    let r = drop_a();
    println(r);
    gc_collect();
    println(gc_allocated_bytes());
    let b = new A(2);
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
    let d = new N(3, nil);
    let c = new N(2, d);
    let b = new N(1, c);
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
    let b = new Box(3);
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
    let t = new Tmp(10);
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
fn drop_v() -> i64 { let v = new V(1); return v.get(); }
fn main() {
    let _ = drop_v();
    gc_collect();
    let v2 = new V(99);
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
    let b = new B(2);
    let c = new C(3);
    return b.get() + c.get();
}
fn main() {
    let a = new A(1);
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
    let t = new Toggle(false);
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
    let a = new Acc(5);
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
fn drop_box(v: i64) -> i64 { let b = new Box(v); return b.get(); }
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
    let t = new Tmp(v);
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
    let c = new Child(6);
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
    let c = new Counter(0);
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
    let i = new Inner(3);
    let o = new Outer(i);
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
    let o = new Outer(nil, 7);
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
    let b = new Box(42);
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
    let n = new Node(3);
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
    let _e = new Empty();
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
    let e = new Empty();
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
fn drop_it() -> i64 { let e = new Empty(9); return e.get(); }
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
    let b = new Box(7);
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
    let q = new Quad(1, 2, 3, 4);
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
fn make_v() -> V { return new V(5); }
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
    let tmp = new Box(3);
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
fn drop_child() -> i64 { let c = new Child(2); return c.get(); }
fn main() {
    let parent = new Base(1);
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
        let t = new Tmp(i * i);
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
fn drop_mixed() -> i64 { let m = new Mixed(3, 4); return m.sum(); }
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
    let k = new Key(13);
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
fn drop_one() -> i64 { let b = new Box(1); return b.get(); }
fn main() {
    let _ = drop_one();
    gc_collect();
    let b2 = new Box(50);
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
fn drop_box() -> i64 { let b = new Box(1); return b.get(); }
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
    let c = new Counter(42);
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
fn drop_box() -> i64 { let b = new Box(1); return b.get(); }
fn main() {
    let r = drop_box();
    println(r);
    gc_collect();
    let zero = gc_allocated_bytes();
    let live = new Box(2);
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
    let c = new Counter(0);
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
    let w = new Wrap(5);
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
    let c = new Child(10);
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
    let a = new Acc(0);
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
    pub static fn pure(a: i64, b: i64) -> i64 { return a + b; }
}
fn main() {
    let a = new Adder(10);
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
    pub static fn make(value: i64) -> Counter { return new Counter(value); }
    pub fn clone_plus(self, n: i64) -> i64 {
        let next = Self::make(self.value + n);
        return next.value;
    }
}
fn main() {
    let c = new Counter(8);
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
    pub static fn pure(a: i64, b: i64) -> i64 { return a + b; }
    pub fn add_to_value(self, n: i64) -> i64 {
        return self::pure(self.value, n);
    }
}
fn main() {
    let m = new Math(20);
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
    pub static fn add(a: i64, b: i64) -> i64 { return a + b; }
}
fn main() {
    let m = new Math();
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
    pub static fn bad() -> i64 {
        return self.value;
    }
}
fn main() {}
"#,
        &["`self` is not available in static method"],
    );
}

#[test]
fn test_assign_to_self_is_error() {
    assert_compile_error_contains(
        r#"
class Box {
    v: i64;
    pub fn bad(self) {
        self = new Box(1);
    }
}
fn main() {}
"#,
        &["cannot assign to `self`"],
    );
}

// ---------------------------------------------------------------------------
// Static members + implicit self — willow-qsqf Stage 1 (static fn + implicit
// self). `static fn` is class-level (called `Type::m(...)`, no `self`); a plain
// `fn` is an instance method whose `self` is implicit (no `self` parameter).
//
//  1. static fn returns a value, called via Type::method
//  2. static fn with multiple args
//  3. static fn calls another static fn on the same class
//  4. static fn called via `Self::` inside an instance method
//  5. static factory returns a class instance
//  6. implicit self reads an instance field
//  7. implicit self method takes extra params
//  8. implicit self mutates an instance field
//  9. implicit self calls another instance method
// 10. static fn returns bool
// 11. static fn returns f64
// 12. static fn returns String (GC-managed result)
// 13. implicit-self String field roundtrips (no explicit self param)
// 14. legacy explicit `self` still compiles (migration compatibility)
// 15. static and instance methods coexist in one class
// 16. `self` in a static method is rejected (E0831)
// 17. explicit `self` on a `static fn` is a parse error (E0831)
// 18. static method called with `.` is rejected (E0834)
// 19. instance method called with `::` is rejected (E0835)
// 20. GC stress: implicit-self String field survives collection
// ---------------------------------------------------------------------------

#[test]
fn test_static_members_01_static_fn_basic() {
    let (out, ok) = compile_and_run(
        r#"
class Math {
    pub static fn add(a: i64, b: i64) -> i64 { return a + b; }
}
fn main() { println(Math::add(1, 2)); }
"#,
    );
    assert!(ok);
    assert_eq!(out, "3\n");
}

#[test]
fn test_static_members_02_static_fn_multi_args() {
    let (out, ok) = compile_and_run(
        r#"
class Math {
    pub static fn sum3(a: i64, b: i64, c: i64) -> i64 { return a + b + c; }
}
fn main() { println(Math::sum3(10, 20, 12)); }
"#,
    );
    assert!(ok);
    assert_eq!(out, "42\n");
}

#[test]
fn test_static_members_03_static_calls_static_same_class() {
    let (out, ok) = compile_and_run(
        r#"
class Math {
    pub static fn add(a: i64, b: i64) -> i64 { return a + b; }
    pub static fn square(x: i64) -> i64 { return Math::add(x * x, 0); }
}
fn main() { println(Math::square(5)); }
"#,
    );
    assert!(ok);
    assert_eq!(out, "25\n");
}

#[test]
fn test_static_members_04_self_static_call_in_instance_method() {
    let (out, ok) = compile_and_run(
        r#"
class Counter {
    value: i64;
    pub static fn make(value: i64) -> Counter { return new Counter(value); }
    pub fn clone_plus(n: i64) -> i64 {
        let next = Self::make(self.value + n);
        return next.value;
    }
}
fn main() {
    let c = new Counter(8);
    println(c.clone_plus(4));
}
"#,
    );
    assert!(ok);
    assert_eq!(out, "12\n");
}

#[test]
fn test_static_members_05_static_factory_returns_instance() {
    let (out, ok) = compile_and_run(
        r#"
class Counter {
    value: i64;
    pub static fn start(at: i64) -> Counter { return new Counter(at); }
    pub fn get() -> i64 { return self.value; }
}
fn main() {
    let c = Counter::start(40);
    println(c.get());
}
"#,
    );
    assert!(ok);
    assert_eq!(out, "40\n");
}

#[test]
fn test_static_members_06_implicit_self_reads_field() {
    let (out, ok) = compile_and_run(
        r#"
class User {
    name: String;
    pub fn getName() -> String { return self.name; }
}
fn main() {
    let u = new User("John");
    println(u.getName());
}
"#,
    );
    assert!(ok);
    assert_eq!(out, "John\n");
}

#[test]
fn test_static_members_07_implicit_self_with_params() {
    let (out, ok) = compile_and_run(
        r#"
class Counter {
    value: i64;
    pub fn plus(n: i64) -> i64 { return self.value + n; }
}
fn main() {
    let c = new Counter(40);
    println(c.plus(2));
}
"#,
    );
    assert!(ok);
    assert_eq!(out, "42\n");
}

#[test]
fn test_static_members_08_implicit_self_mutates_field() {
    let (out, ok) = compile_and_run(
        r#"
class Counter {
    value: i64;
    pub fn bump() { self.value = self.value + 1; }
    pub fn get() -> i64 { return self.value; }
}
fn main() {
    let c = new Counter(0);
    c.bump();
    c.bump();
    println(c.get());
}
"#,
    );
    assert!(ok);
    assert_eq!(out, "2\n");
}

#[test]
fn test_static_members_09_implicit_self_calls_instance_method() {
    let (out, ok) = compile_and_run(
        r#"
class Counter {
    value: i64;
    pub fn get() -> i64 { return self.value; }
    pub fn doubled() -> i64 { return self.get() + self.get(); }
}
fn main() {
    let c = new Counter(21);
    println(c.doubled());
}
"#,
    );
    assert!(ok);
    assert_eq!(out, "42\n");
}

#[test]
fn test_static_members_10_static_fn_returns_bool() {
    let (out, ok) = compile_and_run(
        r#"
class Math {
    pub static fn positive(x: i64) -> bool { return x > 0; }
}
fn main() { println(Math::positive(5)); }
"#,
    );
    assert!(ok);
    assert_eq!(out, "true\n");
}

#[test]
fn test_static_members_11_static_fn_returns_f64() {
    let (out, ok) = compile_and_run(
        r#"
class Math {
    pub static fn half(x: f64) -> f64 { return x / 2.0; }
}
fn main() { println(Math::half(5.0)); }
"#,
    );
    assert!(ok);
    assert_eq!(out, "2.5\n");
}

#[test]
fn test_static_members_12_static_fn_returns_string() {
    let (out, ok) = compile_and_run(
        r#"
class Greeter {
    pub static fn hello() -> String { return "hi"; }
}
fn main() { println(Greeter::hello()); }
"#,
    );
    assert!(ok);
    assert_eq!(out, "hi\n");
}

#[test]
fn test_static_members_13_implicit_self_string_field() {
    let (out, ok) = compile_and_run(
        r#"
class User {
    name: String;
    pub fn shout() -> String { return self.name + "!"; }
}
fn main() {
    let u = new User("Ada");
    println(u.shout());
}
"#,
    );
    assert!(ok);
    assert_eq!(out, "Ada!\n");
}

#[test]
fn test_static_members_14_legacy_explicit_self_still_compiles() {
    // Migration compatibility: an explicit `self` parameter on an instance
    // method is still accepted in Stage 1.
    let (out, ok) = compile_and_run(
        r#"
class Counter {
    value: i64;
    pub fn get(self) -> i64 { return self.value; }
}
fn main() {
    let c = new Counter(7);
    println(c.get());
}
"#,
    );
    assert!(ok);
    assert_eq!(out, "7\n");
}

#[test]
fn test_static_members_15_static_and_instance_coexist() {
    let (out, ok) = compile_and_run(
        r#"
class Adder {
    base: i64;
    pub fn add_base(n: i64) -> i64 { return self.base + n; }
    pub static fn pure(a: i64, b: i64) -> i64 { return a + b; }
}
fn main() {
    let a = new Adder(10);
    println(a.add_base(5));
    println(Adder::pure(2, 3));
}
"#,
    );
    assert!(ok);
    assert_eq!(out, "15\n5\n");
}

#[test]
fn test_static_members_16_self_in_static_method_rejected() {
    assert_compile_error_contains(
        r#"
class Math {
    value: i64;
    pub static fn bad() -> i64 { return self.value; }
}
fn main() {}
"#,
        &["error[E0831]", "`self` is not available in static method"],
    );
}

#[test]
fn test_static_members_17_explicit_self_on_static_is_parse_error() {
    assert_compile_error_contains(
        r#"
class Math {
    pub static fn bad(self) -> i64 { return 1; }
}
fn main() {}
"#,
        &["error[E0831]", "static methods cannot take `self`"],
    );
}

#[test]
fn test_static_members_18_static_called_with_dot_rejected() {
    assert_compile_error_contains(
        r#"
class Math {
    pub static fn add(a: i64, b: i64) -> i64 { return a + b; }
}
fn main() {
    let m = new Math();
    println(m.add(1, 2));
}
"#,
        &[
            "error[E0834]",
            "static method called with `.`",
            "write `Math::add` instead",
        ],
    );
}

#[test]
fn test_static_members_19_instance_called_with_colon_rejected() {
    assert_compile_error_contains(
        r#"
class Box {
    v: i64;
    pub fn get() -> i64 { return self.v; }
}
fn main() {
    println(Box::get());
}
"#,
        &["error[E0835]", "instance method called with `::`"],
    );
}

#[test]
fn test_static_members_20_implicit_self_gc_stress() {
    // Under GC-on-every-allocation, the implicit-self receiver and its String
    // field must stay rooted across the body's allocations.
    let (out, ok) = compile_and_run_gc_stress(
        r#"
class User {
    name: String;
    pub fn decorated() -> String { return "[" + self.name + "]"; }
}
fn main() {
    let u = new User("x");
    println(u.decorated());
}
"#,
    );
    assert!(ok, "implicit-self String field should survive GC stress");
    assert_eq!(out, "[x]\n");
}

// ---------------------------------------------------------------------------
// Immutable static properties — willow-qsqf Stage 2. A `static name: T = expr`
// property lives in global storage, is initialized once before `main`, and is
// read as `ClassName::property`.
//
//  1. static i64 property read
//  2. static String property read
//  3. static bool property read
//  4. static f64 property read
//  5. static property read inside a static method of the same class
//  6. static property read inside an instance method
//  7. a later static may reference an earlier one of the same class
//  8. static property used in arithmetic
//  9. multiple classes each with their own statics (no collision)
// 10. static property initialized from a static method call
// 11. missing initializer is rejected (E0830)
// 12. initializer type mismatch is rejected (E0301)
// 13. `self` in a static initializer is rejected (E0837)
// 14. forward reference to a later static is rejected (E0838)
// 15. instance field accessed via `::` is rejected (E0835)
// 16. reading an unknown static property is rejected
// 17. assigning to an immutable static is rejected (compile error)
// 18. GC stress: static String survives collection (slot rooting)
// 19. GC stress: static String read repeatedly stays valid
// 20. private static property is not accessible from outside the class
// ---------------------------------------------------------------------------

#[test]
fn test_static_prop_01_i64() {
    let (out, ok) = compile_and_run(
        r#"
class Config { pub static version: i64 = 7; }
fn main() { println(Config::version); }
"#,
    );
    assert!(ok);
    assert_eq!(out, "7\n");
}

#[test]
fn test_static_prop_02_string() {
    let (out, ok) = compile_and_run(
        r#"
class Config { pub static name: String = "willow"; }
fn main() { println(Config::name); }
"#,
    );
    assert!(ok);
    assert_eq!(out, "willow\n");
}

#[test]
fn test_static_prop_03_bool() {
    let (out, ok) = compile_and_run(
        r#"
class Config { pub static enabled: bool = true; }
fn main() { println(Config::enabled); }
"#,
    );
    assert!(ok);
    assert_eq!(out, "true\n");
}

#[test]
fn test_static_prop_04_f64() {
    let (out, ok) = compile_and_run(
        r#"
class Config { pub static ratio: f64 = 2.5; }
fn main() { println(Config::ratio); }
"#,
    );
    assert!(ok);
    assert_eq!(out, "2.5\n");
}

#[test]
fn test_static_prop_05_read_in_static_method() {
    let (out, ok) = compile_and_run(
        r#"
class Limits {
    pub static max: i64 = 100;
    pub static fn cap() -> i64 { return Limits::max; }
}
fn main() { println(Limits::cap()); }
"#,
    );
    assert!(ok);
    assert_eq!(out, "100\n");
}

#[test]
fn test_static_prop_06_read_in_instance_method() {
    let (out, ok) = compile_and_run(
        r#"
class Widget {
    id: i64;
    pub static count: i64 = 3;
    pub fn total() -> i64 { return self.id + Widget::count; }
}
fn main() {
    let w = new Widget(39);
    println(w.total());
}
"#,
    );
    assert!(ok);
    assert_eq!(out, "42\n");
}

#[test]
fn test_static_prop_07_references_earlier_static() {
    let (out, ok) = compile_and_run(
        r#"
class C {
    pub static a: i64 = 10;
    pub static b: i64 = C::a + 1;
}
fn main() { println(C::b); }
"#,
    );
    assert!(ok);
    assert_eq!(out, "11\n");
}

#[test]
fn test_static_prop_08_in_arithmetic() {
    let (out, ok) = compile_and_run(
        r#"
class K { pub static base: i64 = 20; }
fn main() { println(K::base * 2 + 2); }
"#,
    );
    assert!(ok);
    assert_eq!(out, "42\n");
}

#[test]
fn test_static_prop_09_multiple_classes_no_collision() {
    let (out, ok) = compile_and_run(
        r#"
class A { pub static v: i64 = 1; }
class B { pub static v: i64 = 2; }
fn main() {
    println(A::v);
    println(B::v);
}
"#,
    );
    assert!(ok);
    assert_eq!(out, "1\n2\n");
}

#[test]
fn test_static_prop_10_initialized_from_static_method() {
    let (out, ok) = compile_and_run(
        r#"
class Seed {
    pub static fn make() -> i64 { return 42; }
    pub static value: i64 = Seed::make();
}
fn main() { println(Seed::value); }
"#,
    );
    assert!(ok);
    assert_eq!(out, "42\n");
}

#[test]
fn test_static_prop_11_missing_initializer_rejected() {
    assert_compile_error_contains(
        r#"
class C { static x: i64; }
fn main() {}
"#,
        &["error[E0830]", "requires an initializer"],
    );
}

#[test]
fn test_static_prop_12_initializer_type_mismatch_rejected() {
    assert_compile_error_contains(
        r#"
class C { static x: i64 = true; }
fn main() {}
"#,
        &["error[E0301]"],
    );
}

#[test]
fn test_static_prop_13_self_in_initializer_rejected() {
    assert_compile_error_contains(
        r#"
class C {
    x: i64;
    static y: i64 = self.x;
}
fn main() {}
"#,
        &["error[E0837]", "static property initializer"],
    );
}

#[test]
fn test_static_prop_14_forward_reference_rejected() {
    assert_compile_error_contains(
        r#"
class C {
    static b: i64 = C::a + 1;
    static a: i64 = 1;
}
fn main() {}
"#,
        &["error[E0838]", "used before it is initialized"],
    );
}

#[test]
fn test_static_prop_15_instance_field_via_colon_rejected() {
    assert_compile_error_contains(
        r#"
class C { v: i64; }
fn main() {
    let x = C::v;
    println(x);
}
"#,
        &["error[E0835]", "requires an object"],
    );
}

#[test]
fn test_static_prop_16_unknown_static_property_rejected() {
    assert_compile_error_contains(
        r#"
class C { pub static a: i64 = 1; }
fn main() {
    let x = C::missing;
    println(x);
}
"#,
        &["error[E0502]", "no static property"],
    );
}

#[test]
fn test_static_prop_17_assign_to_immutable_static_rejected() {
    // Immutable static properties cannot be reassigned (willow-qsqf §5.1). In
    // Stage 2 this is a compile error (static-field assignment + the dedicated
    // E0832 message arrive with `static mut` in Stage 3).
    let (_out, ok) = compile_and_run(
        r#"
class C { pub static x: i64 = 1; }
fn main() { C::x = 2; }
"#,
    );
    assert!(!ok, "assigning to an immutable static must not compile");
}

#[test]
fn test_static_prop_18_string_survives_gc_stress() {
    let (out, ok) = compile_and_run_gc_stress(
        r#"
class Config { pub static name: String = "willow"; }
fn main() { println(Config::name); }
"#,
    );
    assert!(ok, "static String must survive GC stress");
    assert_eq!(out, "willow\n");
}

#[test]
fn test_static_prop_19_string_read_repeatedly_under_gc_stress() {
    let (out, ok) = compile_and_run_gc_stress(
        r#"
class Config { pub static name: String = "ok"; }
fn main() {
    println(Config::name);
    println(Config::name);
    println(Config::name);
}
"#,
    );
    assert!(ok);
    assert_eq!(out, "ok\nok\nok\n");
}

#[test]
fn test_static_prop_20_private_static_not_accessible_outside() {
    assert_compile_error_contains(
        r#"
class C { static secret: i64 = 1; }
fn main() {
    println(C::secret);
}
"#,
        &["error[E0419]", "private"],
    );
}

// ---------------------------------------------------------------------------
// Mutable static properties + mutability enforcement — willow-qsqf Stage 3.
// `static mut name: T = expr` is reassignable via `ClassName::name = value`;
// a plain `static` rejects assignment (E0832).
//
//  1. static mut i64 reassigned and read back
//  2. static mut updated relative to its own value
//  3. static mut String reassigned
//  4. static mut bool reassigned
//  5. static mut f64 reassigned
//  6. static method mutates a static mut of its class
//  7. instance method mutates a static mut of its class
//  8. mutation persists across separate method calls (shared state)
//  9. assigning to an immutable static is rejected (E0832)
// 10. E0832 help mentions `static mut`
// 11. assigning to an unknown static is rejected
// 12. type mismatch on static mut assignment is rejected
// 13. static mut starts from its initializer value
// 14. two static mut properties are independent
// 15. static mut i64 reassigned under GC stress
// 16. static mut String reassigned under GC stress (old value collectible)
// 17. static mut String reassigned many times under GC stress
// 18. reassigned static mut readable from another class's method
// 19. static mut bool toggled in a loop
// 20. private static mut not assignable from outside (E0419)
// ---------------------------------------------------------------------------

#[test]
fn test_static_mut_01_i64_reassign() {
    let (out, ok) = compile_and_run(
        r#"
class S { pub static mut n: i64 = 1; }
fn main() {
    S::n = 42;
    println(S::n);
}
"#,
    );
    assert!(ok);
    assert_eq!(out, "42\n");
}

#[test]
fn test_static_mut_02_update_relative_to_self() {
    let (out, ok) = compile_and_run(
        r#"
class S { pub static mut n: i64 = 10; }
fn main() {
    S::n = S::n + 32;
    println(S::n);
}
"#,
    );
    assert!(ok);
    assert_eq!(out, "42\n");
}

#[test]
fn test_static_mut_03_string_reassign() {
    let (out, ok) = compile_and_run(
        r#"
class S { pub static mut s: String = "a"; }
fn main() {
    S::s = "b";
    println(S::s);
}
"#,
    );
    assert!(ok);
    assert_eq!(out, "b\n");
}

#[test]
fn test_static_mut_04_bool_reassign() {
    let (out, ok) = compile_and_run(
        r#"
class S { pub static mut flag: bool = false; }
fn main() {
    S::flag = true;
    println(S::flag);
}
"#,
    );
    assert!(ok);
    assert_eq!(out, "true\n");
}

#[test]
fn test_static_mut_05_f64_reassign() {
    let (out, ok) = compile_and_run(
        r#"
class S { pub static mut r: f64 = 1.0; }
fn main() {
    S::r = 2.5;
    println(S::r);
}
"#,
    );
    assert!(ok);
    assert_eq!(out, "2.5\n");
}

#[test]
fn test_static_mut_06_mutated_by_static_method() {
    let (out, ok) = compile_and_run(
        r#"
class S {
    pub static mut n: i64 = 0;
    pub static fn add(x: i64) { S::n = S::n + x; }
}
fn main() {
    S::add(40);
    S::add(2);
    println(S::n);
}
"#,
    );
    assert!(ok);
    assert_eq!(out, "42\n");
}

#[test]
fn test_static_mut_07_mutated_by_instance_method() {
    let (out, ok) = compile_and_run(
        r#"
class S {
    v: i64;
    pub static mut n: i64 = 0;
    pub fn record() { S::n = self.v; }
}
fn main() {
    let s = new S(7);
    s.record();
    println(S::n);
}
"#,
    );
    assert!(ok);
    assert_eq!(out, "7\n");
}

#[test]
fn test_static_mut_08_shared_across_calls() {
    let (out, ok) = compile_and_run(
        r#"
class S {
    pub static mut n: i64 = 0;
    pub static fn inc() { S::n = S::n + 1; }
}
fn main() {
    S::inc();
    S::inc();
    S::inc();
    println(S::n);
}
"#,
    );
    assert!(ok);
    assert_eq!(out, "3\n");
}

#[test]
fn test_static_mut_09_immutable_assign_rejected() {
    assert_compile_error_contains(
        r#"
class C { pub static x: i64 = 1; }
fn main() { C::x = 2; }
"#,
        &[
            "error[E0832]",
            "cannot assign to immutable static property `C::x`",
        ],
    );
}

#[test]
fn test_static_mut_10_immutable_assign_help_mentions_static_mut() {
    assert_compile_error_contains(
        r#"
class C { pub static x: i64 = 1; }
fn main() { C::x = 2; }
"#,
        &["static mut"],
    );
}

#[test]
fn test_static_mut_11_assign_unknown_static_rejected() {
    assert_compile_error_contains(
        r#"
class C { pub static mut x: i64 = 1; }
fn main() { C::missing = 2; }
"#,
        &["error[E0502]", "no static property"],
    );
}

#[test]
fn test_static_mut_12_assign_type_mismatch_rejected() {
    assert_compile_error_contains(
        r#"
class C { pub static mut x: i64 = 1; }
fn main() { C::x = true; }
"#,
        &["mismatched types"],
    );
}

#[test]
fn test_static_mut_13_starts_from_initializer() {
    let (out, ok) = compile_and_run(
        r#"
class S { pub static mut n: i64 = 99; }
fn main() { println(S::n); }
"#,
    );
    assert!(ok);
    assert_eq!(out, "99\n");
}

#[test]
fn test_static_mut_14_two_props_independent() {
    let (out, ok) = compile_and_run(
        r#"
class S {
    pub static mut a: i64 = 0;
    pub static mut b: i64 = 0;
}
fn main() {
    S::a = 1;
    S::b = 2;
    println(S::a);
    println(S::b);
}
"#,
    );
    assert!(ok);
    assert_eq!(out, "1\n2\n");
}

#[test]
fn test_static_mut_15_i64_reassign_gc_stress() {
    let (out, ok) = compile_and_run_gc_stress(
        r#"
class S { pub static mut n: i64 = 0; }
fn main() {
    S::n = 5;
    S::n = S::n + 5;
    println(S::n);
}
"#,
    );
    assert!(ok);
    assert_eq!(out, "10\n");
}

#[test]
fn test_static_mut_16_string_reassign_gc_stress() {
    // The slot is a permanent GC root, so the reassigned String stays live and
    // the old one becomes collectible — must be safe under GC stress.
    let (out, ok) = compile_and_run_gc_stress(
        r#"
class S { pub static mut s: String = "old"; }
fn main() {
    S::s = "new";
    println(S::s);
}
"#,
    );
    assert!(ok, "reassigned static mut String must survive GC stress");
    assert_eq!(out, "new\n");
}

#[test]
fn test_static_mut_17_string_many_reassigns_gc_stress() {
    let (out, ok) = compile_and_run_gc_stress(
        r#"
class S {
    pub static mut s: String = "0";
    pub static fn set(v: String) { S::s = v; }
}
fn main() {
    S::set("a");
    S::set("b");
    S::set("c");
    println(S::s);
}
"#,
    );
    assert!(ok);
    assert_eq!(out, "c\n");
}

#[test]
fn test_static_mut_18_read_from_other_class() {
    let (out, ok) = compile_and_run(
        r#"
class State { pub static mut n: i64 = 0; }
class Reader {
    pub static fn get() -> i64 { return State::n; }
}
fn main() {
    State::n = 42;
    println(Reader::get());
}
"#,
    );
    assert!(ok);
    assert_eq!(out, "42\n");
}

#[test]
fn test_static_mut_19_bool_toggled_in_loop() {
    let (out, ok) = compile_and_run(
        r#"
class S { pub static mut n: i64 = 0; }
fn main() {
    let mut i = 0;
    while i < 5 {
        S::n = S::n + i;
        i = i + 1;
    }
    println(S::n);
}
"#,
    );
    assert!(ok);
    assert_eq!(out, "10\n");
}

#[test]
fn test_static_mut_20_private_mut_not_assignable_outside() {
    assert_compile_error_contains(
        r#"
class C { static mut x: i64 = 1; }
fn main() { C::x = 2; }
"#,
        &["error[E0419]", "private"],
    );
}

// ---------------------------------------------------------------------------
// Static members: visibility, inheritance, interfaces — willow-qsqf Stage 4.
// Static members are non-virtual (resolved by type name, inherited statics
// reachable through a subclass, redefinition rejected); interfaces reject
// static members; explicit `self` keeps a migration path.
//
//  1. static fn in an interface is rejected (E0836)
//  2. static property in an interface is rejected (E0836)
//  3. static mut property in an interface is rejected (E0836)
//  4. subclass redefining an inherited static property is rejected (E0839)
//  5. subclass redefining an inherited static method is rejected (E0839)
//  6. E0839 names the hidden inherited member
//  7. distinct static names across base/child are allowed
//  8. an inherited static property is readable through the subclass
//  9. an inherited static is readable inside a subclass static method
// 10. an inherited static mut is assignable through the subclass
// 11. base and child each expose their own statics (non-virtual)
// 12. two-level inheritance: grandchild reads a grandparent static
// 13. interface instance method satisfied by an implicit-self method
// 14. interface default method (explicit self) still works
// 15. private static is not accessible from outside (E0419)
// 16. private static IS accessible from a same-class static method
// 17. protected static IS accessible from a subclass method
// 18. explicit `self` instance method still compiles (migration path)
// 19. explicit `self` on a static fn is still rejected (E0831)
// 20. GC stress: an inherited static String read through a subclass is valid
// ---------------------------------------------------------------------------

#[test]
fn test_static_s4_01_static_fn_in_interface_rejected() {
    assert_compile_error_contains(
        r#"
interface I { static fn helper() -> i64; }
fn main() {}
"#,
        &["error[E0836]", "static interface members are not supported"],
    );
}

#[test]
fn test_static_s4_02_static_prop_in_interface_rejected() {
    assert_compile_error_contains(
        r#"
interface I { static x: i64 = 1; }
fn main() {}
"#,
        &["error[E0836]"],
    );
}

#[test]
fn test_static_s4_03_static_mut_in_interface_rejected() {
    assert_compile_error_contains(
        r#"
interface I { static mut x: i64 = 1; }
fn main() {}
"#,
        &["error[E0836]"],
    );
}

#[test]
fn test_static_s4_04_subclass_hides_static_prop_rejected() {
    assert_compile_error_contains(
        r#"
open class Base { pub static x: i64 = 1; }
class Child extends Base { pub static x: i64 = 2; }
fn main() {}
"#,
        &["error[E0839]", "hides inherited static member"],
    );
}

#[test]
fn test_static_s4_05_subclass_hides_static_method_rejected() {
    assert_compile_error_contains(
        r#"
open class Base { pub static fn h() -> i64 { return 1; } }
class Child extends Base { pub static fn h() -> i64 { return 2; } }
fn main() {}
"#,
        &["error[E0839]", "hides inherited static member"],
    );
}

#[test]
fn test_static_s4_06_hiding_error_names_member() {
    assert_compile_error_contains(
        r#"
open class Base { pub static x: i64 = 1; }
class Child extends Base { pub static x: i64 = 2; }
fn main() {}
"#,
        &["Child::x", "Base::x"],
    );
}

#[test]
fn test_static_s4_07_distinct_names_allowed() {
    let (out, ok) = compile_and_run(
        r#"
open class Base { pub static x: i64 = 1; }
class Child extends Base { pub static y: i64 = 2; }
fn main() {
    println(Base::x);
    println(Child::y);
}
"#,
    );
    assert!(ok);
    assert_eq!(out, "1\n2\n");
}

#[test]
fn test_static_s4_08_inherited_static_readable_via_subclass() {
    let (out, ok) = compile_and_run(
        r#"
open class Base { pub static x: i64 = 7; }
class Child extends Base {}
fn main() { println(Child::x); }
"#,
    );
    assert!(ok);
    assert_eq!(out, "7\n");
}

#[test]
fn test_static_s4_09_inherited_static_in_subclass_static_method() {
    let (out, ok) = compile_and_run(
        r#"
open class Base { pub static base: i64 = 40; }
class Child extends Base {
    pub static fn doubled() -> i64 { return Base::base + 2; }
}
fn main() { println(Child::doubled()); }
"#,
    );
    assert!(ok);
    assert_eq!(out, "42\n");
}

#[test]
fn test_static_s4_10_inherited_static_mut_assignable_via_subclass() {
    let (out, ok) = compile_and_run(
        r#"
open class Base { pub static mut n: i64 = 0; }
class Child extends Base {}
fn main() {
    Child::n = 9;
    println(Base::n);
    println(Child::n);
}
"#,
    );
    assert!(ok);
    assert_eq!(out, "9\n9\n");
}

#[test]
fn test_static_s4_11_base_and_child_own_statics() {
    let (out, ok) = compile_and_run(
        r#"
open class Base { pub static a: i64 = 1; }
class Child extends Base { pub static b: i64 = 2; }
fn main() {
    println(Base::a);
    println(Child::a);
    println(Child::b);
}
"#,
    );
    assert!(ok);
    assert_eq!(out, "1\n1\n2\n");
}

#[test]
fn test_static_s4_12_two_level_inheritance_reads_grandparent_static() {
    let (out, ok) = compile_and_run(
        r#"
open class A { pub static v: i64 = 5; }
open class B extends A {}
class C extends B {}
fn main() { println(C::v); }
"#,
    );
    assert!(ok);
    assert_eq!(out, "5\n");
}

#[test]
fn test_static_s4_13_interface_implicit_self_conformance() {
    let (out, ok) = compile_and_run(
        r#"
interface Named { fn name(self) -> String; }
class User implements Named {
    label: String;
    pub fn name(self) -> String { return self.label; }
}
fn describe(n: Named) -> String { return n.name(); }
fn main() {
    let u = new User("ada");
    println(describe(u));
}
"#,
    );
    assert!(ok);
    assert_eq!(out, "ada\n");
}

#[test]
fn test_static_s4_14_interface_default_method_works() {
    let (out, ok) = compile_and_run(
        r#"
interface Named {
    fn name(self) -> String;
    fn greeting(self) -> String { return self.name(); }
}
class User implements Named {
    label: String;
    pub fn name(self) -> String { return self.label; }
}
fn main() {
    let u = new User("bob");
    println(u.greeting());
}
"#,
    );
    assert!(ok);
    assert_eq!(out, "bob\n");
}

#[test]
fn test_static_s4_15_private_static_inaccessible_outside() {
    assert_compile_error_contains(
        r#"
class C { static secret: i64 = 1; }
fn main() { println(C::secret); }
"#,
        &["error[E0419]", "private"],
    );
}

#[test]
fn test_static_s4_16_private_static_accessible_in_same_class() {
    let (out, ok) = compile_and_run(
        r#"
class C {
    static secret: i64 = 42;
    pub static fn reveal() -> i64 { return C::secret; }
}
fn main() { println(C::reveal()); }
"#,
    );
    assert!(ok);
    assert_eq!(out, "42\n");
}

#[test]
fn test_static_s4_17_protected_static_accessible_in_subclass() {
    let (out, ok) = compile_and_run(
        r#"
open class Base { prot static p: i64 = 5; }
class Child extends Base {
    pub static fn get() -> i64 { return Base::p; }
}
fn main() { println(Child::get()); }
"#,
    );
    assert!(ok);
    assert_eq!(out, "5\n");
}

#[test]
fn test_static_s4_18_explicit_self_still_compiles() {
    let (out, ok) = compile_and_run(
        r#"
class C {
    v: i64;
    pub fn get(self) -> i64 { return self.v; }
}
fn main() {
    let c = new C(8);
    println(c.get());
}
"#,
    );
    assert!(ok);
    assert_eq!(out, "8\n");
}

#[test]
fn test_static_s4_19_explicit_self_on_static_rejected() {
    assert_compile_error_contains(
        r#"
class C { pub static fn bad(self) -> i64 { return 1; } }
fn main() {}
"#,
        &["error[E0831]", "static methods cannot take `self`"],
    );
}

#[test]
fn test_static_s4_20_inherited_static_string_gc_stress() {
    let (out, ok) = compile_and_run_gc_stress(
        r#"
open class Base { pub static name: String = "willow"; }
class Child extends Base {}
fn main() { println(Child::name); }
"#,
    );
    assert!(
        ok,
        "inherited static String via subclass must survive GC stress"
    );
    assert_eq!(out, "willow\n");
}

// ---------------------------------------------------------------------------
// `new` object creation + `init` constructors — willow-scq2 Stage 1.
//
//  1. explicit constructor + method call
//  2. implicit memberwise constructor (no init)
//  3. implicit memberwise sums fields
//  4. constructor with a String field
//  5. constructor validation logic on the valid path
//  6. constructor runtime panic on invalid input
//  7. zero-arg explicit constructor
//  8. `new` result used inline (method call on it)
//  9. constructor assigns from a computed expression
// 10. explicit init's arity is used (not memberwise) — 1 arg, 2 fields
// 11. implicit memberwise with mixed field types
// 12. missing field initialization is rejected (E0842)
// 13. returning a value from init is rejected (E0841)
// 14. declaring a return type on init is rejected (E0840)
// 15. calling init via `Type::init(...)` is rejected (E0843)
// 16. calling init via `obj.init(...)` is rejected (E0843)
// 17. `new` on an unknown class is rejected (E0844)
// 18. wrong constructor argument count is rejected (E0845)
// 19. wrong constructor argument type is rejected
// 20. GC stress: constructed object with a String field survives collection
// 21. implicit memberwise constructor includes inherited instance fields
// 22. subclass init needing base field initialization is rejected (E0848)
// 23. subclass init needing base init logic is rejected (E0848)
// 24. subclass init is allowed when the base has no initialization requirement
// 25. super.init calls an explicit base init
// 26. super.init fills implicit base fields
// 27. protected base init is callable from a subclass
// 28. private base init is rejected from a subclass
// 29. super.init must be the first constructor statement
// 30. super.init outside a constructor is rejected
// 31. init requires an explicit self receiver
// 32. init self receiver must be bare
// 33. private init rejects external new
// 34. public init allows external new
// 35. protected init rejects external new
// 36. private init allows an owner factory
// 37. static init is rejected with a constructor-specific diagnostic
// 38. fn init method syntax is rejected
// 39. static fn init method syntax is rejected
// ---------------------------------------------------------------------------

#[test]
fn test_new_ctor_01_explicit_constructor() {
    let (out, ok) = compile_and_run(
        r#"
class User {
    name: String;
    pub init(self, name: String) { self.name = name; }
    pub fn label(self) -> String { return self.name; }
}
fn main() {
    let u = new User("John");
    println(u.label());
}
"#,
    );
    assert!(ok);
    assert_eq!(out, "John\n");
}

#[test]
fn test_new_ctor_02_implicit_memberwise() {
    let (out, ok) = compile_and_run(
        r#"
class Point { pub x: i64; pub y: i64; }
fn main() {
    let p = new Point(3, 4);
    println(p.x);
    println(p.y);
}
"#,
    );
    assert!(ok);
    assert_eq!(out, "3\n4\n");
}

#[test]
fn test_new_ctor_03_implicit_sum() {
    let (out, ok) = compile_and_run(
        r#"
class Point { pub x: i64; pub y: i64; }
fn main() {
    let p = new Point(3, 4);
    println(p.x + p.y);
}
"#,
    );
    assert!(ok);
    assert_eq!(out, "7\n");
}

#[test]
fn test_new_ctor_04_string_field() {
    let (out, ok) = compile_and_run(
        r#"
class Greeting {
    text: String;
    pub init(self, name: String) { self.text = "hi " + name; }
    pub fn get(self) -> String { return self.text; }
}
fn main() {
    let g = new Greeting("ada");
    println(g.get());
}
"#,
    );
    assert!(ok);
    assert_eq!(out, "hi ada\n");
}

#[test]
fn test_new_ctor_05_validation_valid_path() {
    let (out, ok) = compile_and_run(
        r#"
class User {
    pub age: i64;
    pub init(self, age: i64) {
        if age < 0 { panic("bad age"); }
        self.age = age;
    }
}
fn main() {
    let u = new User(20);
    println(u.age);
}
"#,
    );
    assert!(ok);
    assert_eq!(out, "20\n");
}

#[test]
fn test_new_ctor_06_validation_panics() {
    let (out, ok) = compile_and_run_check_exit(
        r#"
class User {
    pub age: i64;
    pub init(self, age: i64) {
        if age < 0 { panic("bad age"); }
        self.age = age;
    }
}
fn main() {
    let u = new User(-1);
    println(u.age);
}
"#,
    );
    assert!(
        !ok,
        "constructor panic should make the program exit non-zero"
    );
    assert!(out.contains("bad age"), "panic message expected: {out}");
}

#[test]
fn test_new_ctor_07_zero_arg_constructor() {
    let (out, ok) = compile_and_run(
        r#"
class Counter {
    pub n: i64;
    pub init(self) { self.n = 0; }
}
fn main() {
    let c = new Counter();
    println(c.n);
}
"#,
    );
    assert!(ok);
    assert_eq!(out, "0\n");
}

#[test]
fn test_new_ctor_08_used_inline() {
    let (out, ok) = compile_and_run(
        r#"
class User {
    name: String;
    pub init(self, name: String) { self.name = name; }
    pub fn label(self) -> String { return self.name; }
}
fn main() {
    println(new User("inline").label());
}
"#,
    );
    assert!(ok);
    assert_eq!(out, "inline\n");
}

#[test]
fn test_new_ctor_09_computed_field() {
    let (out, ok) = compile_and_run(
        r#"
class Square {
    pub area: i64;
    pub init(self, side: i64) { self.area = side * side; }
}
fn main() {
    let s = new Square(5);
    println(s.area);
}
"#,
    );
    assert!(ok);
    assert_eq!(out, "25\n");
}

#[test]
fn test_new_ctor_10_explicit_init_arity_used() {
    // Two fields but a 1-arg init: `new User("x")` is valid because the explicit
    // init (not the memberwise constructor) determines the signature.
    let (out, ok) = compile_and_run(
        r#"
class User {
    name: String;
    pub age: i64;
    pub init(self, name: String) {
        self.name = name;
        self.age = 99;
    }
    pub fn label(self) -> String { return self.name; }
}
fn main() {
    let u = new User("x");
    println(u.label());
    println(u.age);
}
"#,
    );
    assert!(ok);
    assert_eq!(out, "x\n99\n");
}

#[test]
fn test_new_ctor_11_implicit_mixed_types() {
    let (out, ok) = compile_and_run(
        r#"
class Mix { pub a: i64; pub b: bool; }
fn main() {
    let m = new Mix(7, true);
    println(m.a);
    println(m.b);
}
"#,
    );
    assert!(ok);
    assert_eq!(out, "7\ntrue\n");
}

#[test]
fn test_new_ctor_12_missing_field_init_rejected() {
    assert_compile_error_contains(
        r#"
class User {
    name: String;
    age: i64;
    init(self, name: String) { self.name = name; }
}
fn main() {}
"#,
        &["error[E0842]", "not initialized by constructor"],
    );
}

#[test]
fn test_new_ctor_13_return_value_rejected() {
    assert_compile_error_contains(
        r#"
class User {
    name: String;
    init(self, name: String) {
        self.name = name;
        return self;
    }
}
fn main() {}
"#,
        &["error[E0841]", "cannot return a value"],
    );
}

#[test]
fn test_new_ctor_14_return_type_rejected() {
    assert_compile_error_contains(
        r#"
class User {
    name: String;
    init(self, name: String) -> User { self.name = name; }
}
fn main() {}
"#,
        &["error[E0840]", "must not declare a return type"],
    );
}

#[test]
fn test_new_ctor_15_direct_static_call_rejected() {
    assert_compile_error_contains(
        r#"
class U { init(self) {} }
fn main() { U::init(); }
"#,
        &["error[E0843]", "can only be called with `new`"],
    );
}

#[test]
fn test_new_ctor_16_direct_instance_call_rejected() {
    assert_compile_error_contains(
        r#"
class U {
    v: i64;
    init(self) { self.v = 1; }
    pub fn f(self) { self.init(); }
}
fn main() {}
"#,
        &["error[E0843]", "can only be called with `new`"],
    );
}

#[test]
fn test_new_ctor_17_unknown_class_rejected() {
    assert_compile_error_contains(
        r#"
fn main() { let x = new Missing(); }
"#,
        &["error[E0844]", "unknown class `Missing`"],
    );
}

#[test]
fn test_new_ctor_18_wrong_arg_count_rejected() {
    assert_compile_error_contains(
        r#"
class Point { pub x: i64; pub y: i64; }
fn main() { let p = new Point(1); }
"#,
        &["error[E0845]", "expects 2 argument(s) but got 1"],
    );
}

#[test]
fn test_new_ctor_19_wrong_arg_type_rejected() {
    assert_compile_error_contains(
        r#"
class User {
    pub age: i64;
    pub init(self, age: i64) { self.age = age; }
}
fn main() { let u = new User("not an int"); }
"#,
        &["constructor argument 1"],
    );
}

#[test]
fn test_new_ctor_20_gc_stress_string_field() {
    let (out, ok) = compile_and_run_gc_stress(
        r#"
class User {
    name: String;
    pub init(self, name: String) { self.name = name + "!"; }
    pub fn get(self) -> String { return self.name; }
}
fn main() {
    let u = new User("John");
    println(u.get());
}
"#,
    );
    assert!(
        ok,
        "constructed object with String field must survive GC stress"
    );
    assert_eq!(out, "John!\n");
}

#[test]
fn test_new_ctor_21_implicit_inherited_memberwise_constructor() {
    let (out, ok) = compile_and_run(
        r#"
open class Base { pub id: i64; }
class Child extends Base { pub name: String; }
fn main() {
    let c = new Child(7, "ok");
    println(c.id);
    println(c.name);
}
"#,
    );
    assert!(
        ok,
        "implicit subclass constructor should include base fields"
    );
    assert_eq!(out, "7\nok\n");
}

#[test]
fn test_new_ctor_22_subclass_init_with_base_fields_rejected() {
    assert_compile_error_contains(
        r#"
open class Base { pub id: i64; }
class Child extends Base {
    pub name: String;
    pub init(self, name: String) { self.name = name; }
}
fn main() {}
"#,
        &["error[E0848]", "super.init"],
    );
}

#[test]
fn test_new_ctor_23_subclass_init_with_base_init_rejected() {
    assert_compile_error_contains(
        r#"
open class Base { pub init(self) {} }
class Child extends Base {
    pub value: i64;
    pub init(self, value: i64) { self.value = value; }
}
fn main() {}
"#,
        &["error[E0848]", "base class requires initialization"],
    );
}

#[test]
fn test_new_ctor_24_subclass_init_with_empty_base_allowed() {
    let (out, ok) = compile_and_run(
        r#"
open class Base {}
class Child extends Base {
    pub value: i64;
    pub init(self, value: i64) { self.value = value; }
}
fn main() {
    let c = new Child(9);
    println(c.value);
}
"#,
    );
    assert!(ok, "empty base class should not require super.init");
    assert_eq!(out, "9\n");
}

#[test]
fn test_new_ctor_25_super_init_calls_explicit_base_init() {
    let (out, ok) = compile_and_run(
        r#"
open class Base {
    pub id: i64;
    pub init(self, id: i64) { self.id = id; }
}
class Child extends Base {
    pub name: String;
    pub init(self, id: i64, name: String) {
        super.init(id);
        self.name = name;
    }
}
fn main() {
    let c = new Child(7, "ok");
    println(c.id);
    println(c.name);
}
"#,
    );
    assert!(ok, "super.init should call the explicit base constructor");
    assert_eq!(out, "7\nok\n");
}

#[test]
fn test_new_ctor_26_super_init_fills_implicit_base_fields() {
    let (out, ok) = compile_and_run(
        r#"
open class Base {
    pub id: i64;
    pub label: String;
}
class Child extends Base {
    pub bonus: i64;
    pub init(self, id: i64, label: String, bonus: i64) {
        super.init(id, label);
        self.bonus = bonus;
    }
}
fn main() {
    let c = new Child(7, "base", 3);
    println(c.id);
    println(c.label);
    println(c.bonus);
}
"#,
    );
    assert!(ok, "super.init should lower implicit base memberwise init");
    assert_eq!(out, "7\nbase\n3\n");
}

#[test]
fn test_new_ctor_27_super_init_can_call_protected_base_init() {
    let (out, ok) = compile_and_run(
        r#"
open class Base {
    pub id: i64;
    prot init(self, id: i64) { self.id = id; }
}
class Child extends Base {
    pub init(self, id: i64) { super.init(id); }
}
fn main() {
    let c = new Child(9);
    println(c.id);
}
"#,
    );
    assert!(ok, "subclass should be able to call protected base init");
    assert_eq!(out, "9\n");
}

#[test]
fn test_new_ctor_28_super_init_rejects_private_base_init() {
    assert_compile_error_contains(
        r#"
open class Base {
    pub id: i64;
    init(self, id: i64) { self.id = id; }
}
class Child extends Base {
    pub init(self, id: i64) { super.init(id); }
}
fn main() {}
"#,
        &["error[E0846]", "constructor of `Base` is not visible"],
    );
}

#[test]
fn test_new_ctor_29_super_init_must_be_first_statement() {
    assert_compile_error_contains(
        r#"
open class Base { pub id: i64; }
class Child extends Base {
    pub name: String;
    pub init(self, id: i64, name: String) {
        self.name = name;
        super.init(id);
    }
}
fn main() {}
"#,
        &["error[E0848]", "must be the first statement"],
    );
}

#[test]
fn test_new_ctor_30_super_init_outside_constructor_rejected() {
    assert_compile_error_contains(
        r#"
class Plain {
    pub fn bad(self) { super.init(); }
}
fn main() {}
"#,
        &["error[E0848]", "can only be used inside a constructor"],
    );
}

#[test]
fn test_new_ctor_31_init_requires_explicit_self() {
    assert_compile_error_contains(
        r#"
class User {
    pub init(name: String) {}
}
fn main() {}
"#,
        &[
            "error[E0849]",
            "constructor `init` must declare `self` as its first parameter",
        ],
    );
}

#[test]
fn test_new_ctor_32_init_self_must_be_bare() {
    assert_compile_error_contains(
        r#"
class User {
    pub init(self: User) {}
}
fn main() {}
"#,
        &["error[E0849]", "constructor `self` parameter must be bare"],
    );
}

#[test]
fn test_new_ctor_33_private_init_rejects_external_new() {
    assert_compile_error_contains(
        r#"
class Secret {
    value: i64;
    init(self, value: i64) { self.value = value; }
}
fn main() {
    let secret = new Secret(1);
}
"#,
        &["error[E0846]", "constructor of `Secret` is not visible"],
    );
}

#[test]
fn test_new_ctor_34_public_init_allows_external_new() {
    let (out, ok) = compile_and_run(
        r#"
class Token {
    pub value: i64;
    pub init(self, value: i64) { self.value = value; }
}
fn main() {
    let token = new Token(5);
    println(token.value);
}
"#,
    );
    assert!(ok, "public constructor should be visible to external new");
    assert_eq!(out, "5\n");
}

#[test]
fn test_new_ctor_35_protected_init_rejects_external_new() {
    assert_compile_error_contains(
        r#"
open class Base {
    prot init(self) {}
}
fn main() {
    let base = new Base();
}
"#,
        &["error[E0846]", "constructor of `Base` is not visible"],
    );
}

#[test]
fn test_new_ctor_36_private_init_allows_owner_factory() {
    let (out, ok) = compile_and_run(
        r#"
class Secret {
    value: i64;
    init(self, value: i64) { self.value = value; }
    pub static fn make(value: i64) -> Secret {
        return new Secret(value);
    }
    pub fn read(self) -> i64 { return self.value; }
}
fn main() {
    let secret = Secret::make(8);
    println(secret.read());
}
"#,
    );
    assert!(ok, "owner factory should be allowed to call private init");
    assert_eq!(out, "8\n");
}

#[test]
fn test_new_ctor_37_static_init_modifier_rejected() {
    assert_compile_error_contains(
        r#"
class User {
    static init(self) {}
}
fn main() {}
"#,
        &[
            "error[E0850]",
            "`static` is not allowed on constructor `init`",
        ],
    );
}

#[test]
fn test_new_ctor_38_fn_init_method_syntax_rejected() {
    assert_compile_error_contains(
        r#"
class User {
    fn init(self) {}
}
fn main() {}
"#,
        &[
            "error[E0850]",
            "method name `init` is reserved for constructors",
        ],
    );
}

#[test]
fn test_new_ctor_39_static_fn_init_method_syntax_rejected() {
    assert_compile_error_contains(
        r#"
class User {
    static fn init() {}
}
fn main() {}
"#,
        &[
            "error[E0850]",
            "method name `init` is reserved for constructors",
        ],
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
    let h = new Holder(55);
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
    let u = new User("alice");
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
    let u = new User("Ada", "Lovelace");
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
    let a = new Node("alpha");
    let b = new Node("beta");
    gc_collect();
    let c = new Node("gamma");
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
    if flag { return new Box(99); }
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
    let n = new Names("Ada", "Lovelace");
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
    let r = new Rec(make_str("x"), make_str("y"));
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
    let a = new W("a");
    let b = new W("b");
    let c = new W("c");
    let d = new W("d");
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
fn main() { print_after_alloc(new Box("object alive")); }
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
    let u = new User("alice");
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
    let p = new Printer();
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
    let p = new Printer();
    p.show(new Box("box alive"));
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
    let c = new C();
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
fn make_box(s: String) -> Box { return new Box(s + "!"); }
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
    let n = new Node(3);
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
    let b = new Box(7);
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
    let n = new Node(v);
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
    let a = new N(v);
    gc_collect();
    return a.get();
}
fn outer() -> i64 {
    let b = new N(100);
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
// the filesystem. Single-item imports use `::` paths: `import std::mod::item;`.
// Stage 2 establishes namespace + resolver; concrete collection *types* arrive
// in Stage 3, so these tests import known items and use the ones the prelude
// and builtins already provide.

// Perspective 1: importing a known collections item resolves (compiles).
#[test]
fn test_std_import_collections_array_resolves() {
    let (out, ok) = compile_and_run(
        r#"
import std::collections::Array;
fn main() { println(1); }
"#,
    );
    assert!(ok, "import std::collections::Array should resolve");
    assert_eq!(out, "1\n");
}

// Perspective 2: importing std::collections::Map resolves.
#[test]
fn test_std_import_collections_map_resolves() {
    let (out, ok) = compile_and_run(
        r#"
import std::collections::Map;
fn main() { println(2); }
"#,
    );
    assert!(ok, "import std::collections::Map should resolve");
    assert_eq!(out, "2\n");
}

// Perspective 3: importing std::option::Option resolves and Option is usable.
#[test]
fn test_std_import_option_resolves_and_usable() {
    let (out, ok) = compile_and_run(
        r#"
import std::option::Option;
fn main() {
    let x: Option<i64> = Option::Some(10);
    println(x.unwrap());
}
"#,
    );
    assert!(
        ok,
        "import std::option::Option should resolve and be usable"
    );
    assert_eq!(out, "10\n");
}

// Perspective 4: importing std::result::Result resolves and Result is usable.
#[test]
fn test_std_import_result_resolves_and_usable() {
    let (out, ok) = compile_and_run(
        r#"
import std::result::Result;
fn make() -> Result<i64, String> { return Result::Ok(5); }
fn main() {
    println(match make() { Result::Ok(v) => v, Result::Err(_) => -1, });
}
"#,
    );
    assert!(
        ok,
        "import std::result::Result should resolve and be usable"
    );
    assert_eq!(out, "5\n");
}

// Perspective 5: importing std::io::println (a builtin-keyword item) resolves.
#[test]
fn test_std_import_io_println_resolves() {
    let (out, ok) = compile_and_run(
        r#"
import std::io::println;
fn main() { println(7); }
"#,
    );
    assert!(ok, "import std::io::println should resolve");
    assert_eq!(out, "7\n");
}

// Perspective 6: importing std::io::print (a builtin-keyword item) resolves.
#[test]
fn test_std_import_io_print_resolves() {
    let (out, ok) = compile_and_run(
        r#"
import std::io::print;
fn main() { print(3); println(0); }
"#,
    );
    assert!(ok, "import std::io::print should resolve");
    assert_eq!(out, "30\n");
}

// Perspective 7: importing std::env items resolves.
#[test]
fn test_std_import_env_args_resolves() {
    let (out, ok) = compile_and_run(
        r#"
import std::env::args;
import std::env::program_name;
fn main() { println(4); }
"#,
    );
    assert!(ok, "import std::env items should resolve");
    assert_eq!(out, "4\n");
}

// Perspective 8: a whole-module import resolves.
#[test]
fn test_std_module_import_resolves() {
    let (out, ok) = compile_and_run(
        r#"
import std::collections;
fn main() { println(8); }
"#,
    );
    assert!(ok, "import std::collections (module) should resolve");
    assert_eq!(out, "8\n");
}

// Perspective 9: multiple std imports coexist in one file.
#[test]
fn test_std_multiple_imports_coexist() {
    let (out, ok) = compile_and_run(
        r#"
import std::io::println;
import std::option::Option;
import std::result::Result;
import std::collections::Array;
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
import std::collections::Vec;
fn main() { println(1); }
"#,
        &["error[E2006]", "no item `Vec` in `std::collections`"],
    );
}

// Perspective 11: a near-miss item name suggests the correct one.
#[test]
fn test_std_unknown_item_suggests_nearest() {
    assert_compile_error_contains(
        r#"
import std::collections::Aray;
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
import std::io::flush;
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
import std::networking::Socket;
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
import std::collection::Array;
fn main() { println(1); }
"#,
        &["error[E2007]", "did you mean `std::collections`?"],
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
import std::collections::Array::extra;
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
import std::bogus;
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
import std::io::println;
fn helper(n: i64) -> i64 { return n + 1; }
fn main() { println(helper(40)); }
"#,
    );
    assert!(ok, "std import should not disturb local declarations");
    assert_eq!(out, "41\n");
}

// Perspective 19: dotted std imports are rejected; std paths use `::`.
#[test]
fn test_std_dotted_import_is_rejected() {
    assert_compile_error_contains(
        r#"
import std.io.println;
fn main() {}
"#,
        &["error[E0101]"],
    );
}

// Perspective 20: a duplicate std import is accepted (deduplicated silently).
#[test]
fn test_std_duplicate_import_is_accepted() {
    let id = unique_test_id();
    let src_path = temp_path(format!("willow_duplicate_std_import_{}.wi", id));
    let bin_path = temp_path(format!("willow_duplicate_std_import_{}", id));
    fs::write(
        &src_path,
        r#"
import std::collections::Array;
import std::collections::Array;
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
        "import std::collections::Nope;\nfn main() {}\n",
        &["error[E2006]"],
    );
    assert_compile_error_contains(
        "import std::nope::Thing;\nfn main() {}\n",
        &["error[E2007]"],
    );
}

// ── std::collections type imports (willow-4bv.3, Stage 3) ───────────────────

#[test]
fn test_std_collections_array_import_enables_annotations() {
    let (out, ok) = compile_and_run(
        r#"
import std::collections::Array;

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
import std::collections;

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
        &["error[E2001]", "import std::collections::Array"],
    );
}

#[test]
fn test_missing_array_import_on_parameter_reports_e2001() {
    assert_compile_error_contains(
        r#"
fn total(xs: Array<i64>) -> i64 { return xs.len(); }
fn main() { println(total([1])); }
"#,
        &["error[E2001]", "import std::collections::Array"],
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
        &["error[E2001]", "import std::collections::Array"],
    );
}

#[test]
fn test_std_collections_map_import_enables_constructor() {
    let (out, ok) = compile_and_run(
        r#"
import std::collections::Map;

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
        &["error[E2002]", "import std::collections::Map"],
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
        &["error[E2002]", "import std::collections::Map"],
    );
}

#[test]
fn test_importing_map_does_not_import_array() {
    assert_compile_error_contains(
        r#"
import std::collections::Map;

fn main() {
    let xs: Array<i64> = [1];
    let m: Map<String, i64> = Map::new();
    println(xs.len() + m.len());
}
"#,
        &["error[E2001]", "import std::collections::Array"],
    );
}

#[test]
fn test_importing_array_does_not_import_map() {
    assert_compile_error_contains(
        r#"
import std::collections::Array;

fn main() {
    let xs: Array<i64> = [1];
    let m: Map<String, i64> = Map::new();
    println(xs.len() + m.len());
}
"#,
        &["error[E2002]", "import std::collections::Map"],
    );
}

#[test]
fn test_std_collection_item_import_collision_reports_e2004() {
    assert_compile_error_contains(
        r#"
import std::collections::Array as Thing;
import std::collections::Map as Thing;
fn main() {}
"#,
        &["error[E2004]", "defined multiple times"],
    );
}

#[test]
fn test_std_collection_item_import_vs_local_class_reports_e2003() {
    assert_compile_error_contains(
        r#"
import std::collections::Array;
class Array { pub v: i64; }
fn main() {}
"#,
        &["error[E2003]", "import and a local declaration"],
    );
}

// ── std::collections module imports (willow-4bv.4, Stage 4) ─────────────────

#[test]
fn test_std_collections_module_import_enables_qualified_types_and_constructor() {
    let (out, ok) = compile_and_run(
        r#"
import std::collections;

fn main() {
    let xs: collections::Array<i64> = [1, 2, 3];
    let m: collections::Map<String, i64> = collections::Map::new();
    println(xs.len() + m.len());
}
"#,
    );
    assert!(ok);
    assert_eq!(out, "3\n");
}

#[test]
fn test_std_collections_module_import_enables_qualified_main_args() {
    let (out, ok) = compile_and_run_with_program_args(
        r#"
import std::collections;

fn main(args: collections::Array<String>) {
    println(args.len());
}
"#,
        &["one", "two"],
    );
    assert!(ok);
    assert_eq!(out, "2\n");
}

#[test]
fn test_std_collections_module_import_coexists_with_item_import_and_prelude() {
    let (out, ok) = compile_and_run(
        r#"
import std::collections;
import std::collections::Array;

fn make() -> Option<i64> {
    return Option::Some(40);
}

fn main() {
    let xs: collections::Array<i64> = [make().unwrap(), 2];
    let ys: Array<i64> = [1];
    println(xs[0] + xs[1] + ys.len());
}
"#,
    );
    assert!(ok);
    assert_eq!(out, "43\n");
}

#[test]
fn test_std_collections_unknown_qualified_type_reports_e2006() {
    assert_compile_error_contains(
        r#"
import std::collections;

fn main() {
    let xs: collections::Vec<i64> = [];
    println(1);
}
"#,
        &["error[E2006]", "no item `Vec` in `std::collections`"],
    );
}

#[test]
fn test_std_collections_unknown_qualified_constructor_reports_e2006() {
    assert_compile_error_contains(
        r#"
import std::collections;

fn main() {
    collections::Vec::new();
}
"#,
        &["error[E2006]", "no item `Vec` in `std::collections`"],
    );
}

#[test]
fn test_std_collections_module_import_vs_local_decl_reports_e2003() {
    assert_compile_error_contains(
        r#"
import std::collections;
fn collections() -> i64 { return 0; }
fn main() {}
"#,
        &["error[E2003]", "import and a local declaration"],
    );
}

#[test]
fn test_std_collections_module_import_vs_item_alias_reports_e2004() {
    assert_compile_error_contains(
        r#"
import std::collections;
import std::collections::Array as collections;
fn main() {}
"#,
        &["error[E2004]", "defined multiple times"],
    );
}

// ── std::collections alias imports (willow-4bv.5, Stage 5) ──────────────────

#[test]
fn test_std_collection_array_alias_enables_type_positions() {
    let (out, ok) = compile_and_run(
        r#"
import std::collections::Array as Arr;

fn main() {
    let xs: Arr<i64> = [1, 2, 3, 4];
    println(xs.len());
}
"#,
    );
    assert!(ok);
    assert_eq!(out, "4\n");
}

#[test]
fn test_std_collection_map_alias_enables_type_and_constructor() {
    let (out, ok) = compile_and_run(
        r#"
import std::collections::Map as Dict;

fn main() {
    let m: Dict<String, i64> = Dict::new();
    println(m.len());
}
"#,
    );
    assert!(ok);
    assert_eq!(out, "0\n");
}

#[test]
fn test_std_collection_alias_can_shadow_prelude_name() {
    let (out, ok) = compile_and_run(
        r#"
import std::collections::Map as Option;

fn main() {
    let m: Option<String, i64> = Option::new();
    println(m.len());
}
"#,
    );
    assert!(ok);
    assert_eq!(out, "0\n");
}

#[test]
fn test_std_collection_alias_conflict_reports_e2004() {
    assert_compile_error_contains(
        r#"
import std::collections::Array as Bag;
import std::collections::Map as Bag;
fn main() {}
"#,
        &["error[E2004]", "defined multiple times"],
    );
}

#[test]
fn test_std_collection_duplicate_alias_warns() {
    let id = unique_test_id();
    let src_path = temp_path(format!("willow_duplicate_std_alias_{}.wi", id));
    let bin_path = temp_path(format!("willow_duplicate_std_alias_{}", id));
    fs::write(
        &src_path,
        r#"
import std::collections::Array as Arr;
import std::collections::Array as Arr;
fn main() {
    let xs: Arr<i64> = [9];
    println(xs[0]);
}
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
        "duplicate identical alias should compile with a warning: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("warning[W2002]"), "stderr: {stderr}");

    let run = Command::new(&bin_path)
        .output()
        .expect("failed to run binary");
    assert_eq!(String::from_utf8_lossy(&run.stdout), "9\n");

    let _ = fs::remove_file(&src_path);
    remove_output_artifacts(&bin_path);
}

#[test]
fn test_std_collection_alias_vs_local_decl_reports_e2003() {
    assert_compile_error_contains(
        r#"
import std::collections::Array as Bag;
class Bag { pub v: i64; }
fn main() {}
"#,
        &["error[E2003]", "import and a local declaration"],
    );
}

// ── fully qualified std paths (willow-4bv.6, Stage 6) ──────────────────────

#[test]
fn test_fully_qualified_std_collection_array_type() {
    let (out, ok) = compile_and_run(
        r#"
fn main() {
    let xs: std::collections::Array<i64> = [3, 4];
    println(xs[0] + xs[1]);
}
"#,
    );
    assert!(ok);
    assert_eq!(out, "7\n");
}

#[test]
fn test_fully_qualified_std_collection_map_type_and_constructor() {
    let (out, ok) = compile_and_run(
        r#"
fn main() {
    let m: std::collections::Map<String, i64> = std::collections::Map::new();
    println(m.len());
}
"#,
    );
    assert!(ok);
    assert_eq!(out, "0\n");
}

#[test]
fn test_fully_qualified_std_option_and_result_paths() {
    let (out, ok) = compile_and_run(
        r#"
fn make() -> std::result::Result<i64, String> {
    return std::result::Result::Ok(41);
}

fn main() {
    let value: std::option::Option<i64> = std::option::Option::Some(1);
    println(value.unwrap() + make().unwrap());
}
"#,
    );
    assert!(ok);
    assert_eq!(out, "42\n");
}

#[test]
fn test_fully_qualified_std_io_println() {
    let (out, ok) = compile_and_run(
        r#"
fn main() {
    std::io::println(123);
}
"#,
    );
    assert!(ok);
    assert_eq!(out, "123\n");
}

#[test]
fn test_fully_qualified_std_unknown_item_reports_e2006() {
    assert_compile_error_contains(
        r#"
fn main() {
    let xs: std::collections::Vec<i64> = [];
    println(1);
}
"#,
        &["error[E2006]", "no item `Vec` in `std::collections`"],
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
import std::collections::Array;

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
import std::collections::Array;

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
import std::collections::Array;

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

#[test]
fn test_array_for_loop_sum() {
    let (out, ok) = compile_and_run(
        r#"
import std::collections::Array;

fn sum(values: Array<i64>) -> i64 {
    let mut total = 0;
    for value in values {
        total = total + value;
    }
    return total;
}

fn main() {
    let values: Array<i64> = [1, 1, 2, 3, 5, 8];
    println(values[0]);
    println(values.len());
    println(sum(values));
}
"#,
    );
    assert!(ok, "{out}");
    assert_eq!(out, "1\n6\n20\n");
}

#[test]
fn test_array_for_loop_gc_elements_survive_gc_stress() {
    let (out, ok) = compile_and_run_gc_stress(
        r#"
import std::collections::Array;

fn main() {
    let names: Array<String> = ["a", "b", "c"];
    for name in names {
        let message = name + "!";
        println(message);
    }
}
"#,
    );
    assert!(ok, "{out}");
    assert_eq!(out, "a!\nb!\nc!\n");
}

#[test]
fn test_for_loop_requires_array_iterable() {
    assert_compile_error_contains(
        r#"
fn main() {
    for value in 123 {
        println(value);
    }
}
"#,
        &[
            "error[E0201]",
            "cannot iterate over `i64`",
            "for-in requires an array",
        ],
    );
}

// Perspective 4: bool elements.
#[test]
fn test_array_bool_elements() {
    let (out, ok) = compile_and_run(
        r#"
import std::collections::Array;

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
import std::collections::Array;

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
import std::collections::Array;

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
import std::collections::Array;

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
import std::collections::Array;

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
import std::collections::Array;

class P {
    pub val: i64;
    pub static fn new(v: i64) -> P { return new P(v); }
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
import std::collections::Array;

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
import std::collections::Array;

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
import std::collections::Array;

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
import std::collections::Array;

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
import std::collections::Array;

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
import std::collections::Array;

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
import std::collections::Array;

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
import std::collections::Array;

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
import std::collections::Array;

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
import std::collections::Array;

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
import std::collections::Array;

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
import std::collections::Array;

fn main() {
    let mut xs: Array<i64> = [1, 2, 3];
    xs[0] = true;
}
"#,
        &["error[E0201]"],
    );
}

// ── For loops over Array<T> (willow-for-loop) ───────────────────────────────
// 20 explicit perspectives: scalar/reference elements, control-flow nesting,
// scoping, diagnostics, evaluation order, GC, and cooperative async.

// Perspective 1: i64 elements can be accumulated.
#[test]
fn test_for_loop_perspective_01_i64_sum() {
    let (out, ok) = compile_and_run(
        r#"
import std::collections::Array;

fn main() {
    let xs: Array<i64> = [2, 4, 6, 8];
    let mut total = 0;
    for x in xs {
        total = total + x;
    }
    println(total);
}
"#,
    );
    assert!(ok, "{out}");
    assert_eq!(out, "20\n");
}

// Perspective 2: an empty array executes the body zero times.
#[test]
fn test_for_loop_perspective_02_empty_array_skips_body() {
    let (out, ok) = compile_and_run(
        r#"
import std::collections::Array;

fn main() {
    let xs: Array<i64> = [];
    let mut count = 7;
    for _ in xs {
        count = count + 100;
    }
    println(count);
}
"#,
    );
    assert!(ok, "{out}");
    assert_eq!(out, "7\n");
}

// Perspective 3: a single-element array executes the body exactly once.
#[test]
fn test_for_loop_perspective_03_single_element_runs_once() {
    let (out, ok) = compile_and_run(
        r#"
import std::collections::Array;

fn main() {
    let xs: Array<i64> = [42];
    let mut count = 0;
    for x in xs {
        println(x);
        count = count + 1;
    }
    println(count);
}
"#,
    );
    assert!(ok, "{out}");
    assert_eq!(out, "42\n1\n");
}

// Perspective 4: bool elements work with ordinary branch logic.
#[test]
fn test_for_loop_perspective_04_bool_elements_drive_if() {
    let (out, ok) = compile_and_run(
        r#"
import std::collections::Array;

fn main() {
    let flags: Array<bool> = [true, false, true];
    let mut yes = 0;
    for flag in flags {
        if flag {
            yes = yes + 1;
        }
    }
    println(yes);
}
"#,
    );
    assert!(ok, "{out}");
    assert_eq!(out, "2\n");
}

// Perspective 5: f64 elements preserve their bit representation through the loop.
#[test]
fn test_for_loop_perspective_05_f64_accumulation() {
    let (out, ok) = compile_and_run(
        r#"
import std::collections::Array;

fn main() {
    let values: Array<f64> = [0.5, 1.25];
    let mut total = 0.0;
    for value in values {
        total = total + value;
    }
    println(total);
}
"#,
    );
    assert!(ok, "{out}");
    assert_eq!(out, "1.75\n");
}

// Perspective 6: String elements are usable as GC-managed references.
#[test]
fn test_for_loop_perspective_06_string_concat() {
    let (out, ok) = compile_and_run(
        r#"
import std::collections::Array;

fn main() {
    let parts: Array<String> = ["will", "ow"];
    let mut text = "";
    for part in parts {
        text = text + part;
    }
    println(text);
}
"#,
    );
    assert!(ok, "{out}");
    assert_eq!(out, "willow\n");
}

// Perspective 7: class instances can be iterated and called through.
#[test]
fn test_for_loop_perspective_07_object_elements_methods() {
    let (out, ok) = compile_and_run(
        r#"
import std::collections::Array;

class Score {
    pub value: i64;
    pub static fn new(value: i64) -> Score {
        return new Score(value);
    }
    pub fn get(self) -> i64 {
        return self.value;
    }
}

fn main() {
    let scores: Array<Score> = [Score::new(4), Score::new(5)];
    let mut total = 0;
    for score in scores {
        total = total + score.get();
    }
    println(total);
}
"#,
    );
    assert!(ok, "{out}");
    assert_eq!(out, "9\n");
}

// Perspective 8: nested for loops compose.
#[test]
fn test_for_loop_perspective_08_nested_for_loops() {
    let (out, ok) = compile_and_run(
        r#"
import std::collections::Array;

fn main() {
    let left: Array<i64> = [1, 2];
    let right: Array<i64> = [10, 20];
    let mut total = 0;
    for a in left {
        for b in right {
            total = total + a + b;
        }
    }
    println(total);
}
"#,
    );
    assert!(ok, "{out}");
    assert_eq!(out, "66\n");
}

// Perspective 9: for loops can live inside while loops.
#[test]
fn test_for_loop_perspective_09_for_inside_while() {
    let (out, ok) = compile_and_run(
        r#"
import std::collections::Array;

fn main() {
    let xs: Array<i64> = [1, 2];
    let mut round = 0;
    let mut total = 0;
    while round < 2 {
        for x in xs {
            total = total + x;
        }
        round = round + 1;
    }
    println(total);
}
"#,
    );
    assert!(ok, "{out}");
    assert_eq!(out, "6\n");
}

// Perspective 10: while loops can live inside for loop bodies.
#[test]
fn test_for_loop_perspective_10_while_inside_for() {
    let (out, ok) = compile_and_run(
        r#"
import std::collections::Array;

fn main() {
    let limits: Array<i64> = [1, 3];
    let mut total = 0;
    for limit in limits {
        let mut i = 0;
        while i < limit {
            total = total + 1;
            i = i + 1;
        }
    }
    println(total);
}
"#,
    );
    assert!(ok, "{out}");
    assert_eq!(out, "4\n");
}

// Perspective 11: the loop variable shadows an outer binding only in the loop.
#[test]
fn test_for_loop_perspective_11_loop_var_shadows_outer_and_restores() {
    let (out, ok) = compile_and_run(
        r#"
import std::collections::Array;

fn main() {
    let value = 99;
    let xs: Array<i64> = [1, 2];
    for value in xs {
        println(value);
    }
    println(value);
}
"#,
    );
    assert!(ok, "{out}");
    assert_eq!(out, "1\n2\n99\n");
}

// Perspective 12: `_` discards the element but still counts iterations.
#[test]
fn test_for_loop_perspective_12_underscore_discards_element() {
    let (out, ok) = compile_and_run(
        r#"
import std::collections::Array;

fn main() {
    let xs: Array<i64> = [3, 4, 5];
    let mut count = 0;
    for _ in xs {
        count = count + 1;
    }
    println(count);
}
"#,
    );
    assert!(ok, "{out}");
    assert_eq!(out, "3\n");
}

// Perspective 13: the iterable expression is evaluated once before iteration.
#[test]
fn test_for_loop_perspective_13_iterable_expression_evaluated_once() {
    let (out, ok) = compile_and_run(
        r#"
import std::collections::Array;

fn make() -> Array<i64> {
    println(70);
    return [1, 2];
}

fn main() {
    for x in make() {
        println(x);
    }
}
"#,
    );
    assert!(ok, "{out}");
    assert_eq!(out, "70\n1\n2\n");
}

// Perspective 14: arrays returned from functions can be iterated directly.
#[test]
fn test_for_loop_perspective_14_iterates_returned_array() {
    let (out, ok) = compile_and_run(
        r#"
import std::collections::Array;

fn make() -> Array<i64> {
    return [7, 8, 9];
}

fn main() {
    let mut total = 0;
    for value in make() {
        total = total + value;
    }
    println(total);
}
"#,
    );
    assert!(ok, "{out}");
    assert_eq!(out, "24\n");
}

// Perspective 15: arrays passed as parameters can be iterated in callees.
#[test]
fn test_for_loop_perspective_15_iterates_array_parameter() {
    let (out, ok) = compile_and_run(
        r#"
import std::collections::Array;

fn sum(values: Array<i64>) -> i64 {
    let mut total = 0;
    for value in values {
        total = total + value;
    }
    return total;
}

fn main() {
    let values: Array<i64> = [5, 6, 7];
    println(sum(values));
}
"#,
    );
    assert!(ok, "{out}");
    assert_eq!(out, "18\n");
}

// Perspective 16: reference elements stay live across GC stress while iterating.
#[test]
fn test_for_loop_perspective_16_reference_elements_survive_gc_stress() {
    let (out, ok) = compile_and_run_gc_stress(
        r#"
import std::collections::Array;

fn main() {
    let names: Array<String> = ["a", "b", "c"];
    for name in names {
        gc_collect();
        println(name + "!");
    }
}
"#,
    );
    assert!(ok, "{out}");
    assert_eq!(out, "a!\nb!\nc!\n");
}

// Perspective 17: element reads observe array mutations made before later turns.
#[test]
fn test_for_loop_perspective_17_mutating_array_during_iteration() {
    let (out, ok) = compile_and_run(
        r#"
import std::collections::Array;

fn main() {
    let mut xs: Array<i64> = [1, 2, 3];
    let mut total = 0;
    for x in xs {
        total = total + x;
        if x == 1 {
            xs[1] = 20;
        }
    }
    println(total);
}
"#,
    );
    assert!(ok, "{out}");
    assert_eq!(out, "24\n");
}

// Perspective 18: loop variables are immutable.
#[test]
fn test_for_loop_perspective_18_loop_var_assignment_is_error() {
    assert_compile_error_contains(
        r#"
import std::collections::Array;

fn main() {
    let xs: Array<i64> = [1, 2];
    for value in xs {
        value = 9;
    }
}
"#,
        &[
            "error[E0301]",
            "cannot assign to immutable variable `value`",
        ],
    );
}

// Perspective 19: loop variables do not leak out of the loop body.
#[test]
fn test_for_loop_perspective_19_loop_var_is_scoped_to_body() {
    assert_compile_error_contains(
        r#"
import std::collections::Array;

fn main() {
    let xs: Array<i64> = [1, 2];
    for value in xs {
        println(value);
    }
    println(value);
}
"#,
        &["error[E0350]", "cannot find variable `value`"],
    );
}

// Perspective 20: await works inside for loops in both async main and leaf fns.
#[test]
fn test_for_loop_perspective_20_async_await_in_main_and_leaf() {
    let (out, ok) = compile_and_run_gc_stress(
        r#"
import std::collections::Array;

async fn sum(values: Array<i64>) -> i64 {
    let mut total = 0;
    for value in values {
        await sleep(1);
        total = total + value;
    }
    return total;
}

async fn main() {
    let visible: Array<i64> = [1, 2];
    for value in visible {
        await sleep(1);
        println(value);
    }

    let hidden: Array<i64> = [3, 4];
    let total = await sum(hidden);
    println(total);
}
"#,
    );
    assert!(ok, "{out}");
    assert_eq!(out, "1\n2\n7\n");
}

// ── For loops over i64 ranges (willow-range-for) ────────────────────────────
// 22 explicit perspectives: half-open behavior, empty ranges, bound typing,
// evaluation order, scoping, array interop, and cooperative async.

// Perspective 1: `start..end` is half-open.
#[test]
fn test_range_for_loop_perspective_01_half_open_prints_start_to_end_minus_one() {
    let (out, ok) = compile_and_run(
        r#"
fn main() {
    for n in 1..4 {
        println(n);
    }
}
"#,
    );
    assert!(ok, "{out}");
    assert_eq!(out, "1\n2\n3\n");
}

// Perspective 2: `1..101` covers 1 through 100.
#[test]
fn test_range_for_loop_perspective_02_one_to_one_hundred_sum() {
    let (out, ok) = compile_and_run(
        r#"
fn main() {
    let mut total = 0;
    for n in 1..101 {
        total = total + n;
    }
    println(total);
}
"#,
    );
    assert!(ok, "{out}");
    assert_eq!(out, "5050\n");
}

// Perspective 3: equal start/end runs zero iterations.
#[test]
fn test_range_for_loop_perspective_03_equal_bounds_are_empty() {
    let (out, ok) = compile_and_run(
        r#"
fn main() {
    let mut count = 0;
    for _ in 5..5 {
        count = count + 1;
    }
    println(count);
}
"#,
    );
    assert!(ok, "{out}");
    assert_eq!(out, "0\n");
}

// Perspective 4: descending ranges run zero iterations.
#[test]
fn test_range_for_loop_perspective_04_descending_range_is_empty() {
    let (out, ok) = compile_and_run(
        r#"
fn main() {
    let mut count = 0;
    for _ in 5..2 {
        count = count + 1;
    }
    println(count);
}
"#,
    );
    assert!(ok, "{out}");
    assert_eq!(out, "0\n");
}

// Perspective 5: negative starts work with the same +1 step.
#[test]
fn test_range_for_loop_perspective_05_negative_start() {
    let (out, ok) = compile_and_run(
        r#"
fn main() {
    for n in -2..2 {
        println(n);
    }
}
"#,
    );
    assert!(ok, "{out}");
    assert_eq!(out, "-2\n-1\n0\n1\n");
}

// Perspective 6: variable bounds are accepted.
#[test]
fn test_range_for_loop_perspective_06_variable_bounds() {
    let (out, ok) = compile_and_run(
        r#"
fn main() {
    let start = 2;
    let end = 5;
    let mut total = 0;
    for n in start..end {
        total = total + n;
    }
    println(total);
}
"#,
    );
    assert!(ok, "{out}");
    assert_eq!(out, "9\n");
}

// Perspective 7: arithmetic bound expressions are accepted.
#[test]
fn test_range_for_loop_perspective_07_arithmetic_bounds() {
    let (out, ok) = compile_and_run(
        r#"
fn main() {
    let mut total = 0;
    for n in (1 + 1)..(3 + 2) {
        total = total + n;
    }
    println(total);
}
"#,
    );
    assert!(ok, "{out}");
    assert_eq!(out, "9\n");
}

// Perspective 8: bound expressions are evaluated once, left to right.
#[test]
fn test_range_for_loop_perspective_08_bounds_evaluated_once_left_to_right() {
    let (out, ok) = compile_and_run(
        r#"
fn start() -> i64 {
    println(10);
    return 1;
}

fn stop() -> i64 {
    println(20);
    return 3;
}

fn main() {
    for n in start()..stop() {
        println(n);
    }
}
"#,
    );
    assert!(ok, "{out}");
    assert_eq!(out, "10\n20\n1\n2\n");
}

// Perspective 9: nested range loops compose.
#[test]
fn test_range_for_loop_perspective_09_nested_ranges() {
    let (out, ok) = compile_and_run(
        r#"
fn main() {
    let mut total = 0;
    for a in 1..3 {
        for b in 1..3 {
            total = total + a * 10 + b;
        }
    }
    println(total);
}
"#,
    );
    assert!(ok, "{out}");
    assert_eq!(out, "66\n");
}

// Perspective 10: range loops can live inside while loops.
#[test]
fn test_range_for_loop_perspective_10_range_inside_while() {
    let (out, ok) = compile_and_run(
        r#"
fn main() {
    let mut round = 0;
    let mut total = 0;
    while round < 2 {
        for n in 1..3 {
            total = total + n;
        }
        round = round + 1;
    }
    println(total);
}
"#,
    );
    assert!(ok, "{out}");
    assert_eq!(out, "6\n");
}

// Perspective 11: while loops can live inside range loop bodies.
#[test]
fn test_range_for_loop_perspective_11_while_inside_range() {
    let (out, ok) = compile_and_run(
        r#"
fn main() {
    let mut total = 0;
    for limit in 1..4 {
        let mut i = 0;
        while i < limit {
            total = total + 1;
            i = i + 1;
        }
    }
    println(total);
}
"#,
    );
    assert!(ok, "{out}");
    assert_eq!(out, "6\n");
}

// Perspective 12: `_` discards range elements but preserves iteration count.
#[test]
fn test_range_for_loop_perspective_12_underscore_discards_range_item() {
    let (out, ok) = compile_and_run(
        r#"
fn main() {
    let mut count = 0;
    for _ in 3..7 {
        count = count + 1;
    }
    println(count);
}
"#,
    );
    assert!(ok, "{out}");
    assert_eq!(out, "4\n");
}

// Perspective 13: the loop variable shadows only inside the range loop.
#[test]
fn test_range_for_loop_perspective_13_shadowing_restores_outer() {
    let (out, ok) = compile_and_run(
        r#"
fn main() {
    let n = 99;
    for n in 1..3 {
        println(n);
    }
    println(n);
}
"#,
    );
    assert!(ok, "{out}");
    assert_eq!(out, "1\n2\n99\n");
}

// Perspective 14: returning from inside a range loop terminates the function.
#[test]
fn test_range_for_loop_perspective_14_return_inside_range_loop() {
    let (out, ok) = compile_and_run(
        r#"
fn first() -> i64 {
    for n in 2..5 {
        return n;
    }
    return 0;
}

fn main() {
    println(first());
}
"#,
    );
    assert!(ok, "{out}");
    assert_eq!(out, "2\n");
}

// Perspective 15: range loops interoperate with Array indexing.
#[test]
fn test_range_for_loop_perspective_15_range_indexes_array() {
    let (out, ok) = compile_and_run(
        r#"
import std::collections::Array;

fn main() {
    let xs: Array<i64> = [5, 6, 7];
    let mut total = 0;
    for i in 0..xs.len() {
        total = total + xs[i];
    }
    println(total);
}
"#,
    );
    assert!(ok, "{out}");
    assert_eq!(out, "18\n");
}

// Perspective 16: the end bound is snapshotted before the loop starts.
#[test]
fn test_range_for_loop_perspective_16_end_bound_snapshot() {
    let (out, ok) = compile_and_run(
        r#"
fn main() {
    let mut end = 4;
    let mut total = 0;
    for n in 1..end {
        total = total + n;
        if n == 1 {
            end = 2;
        }
    }
    println(total);
    println(end);
}
"#,
    );
    assert!(ok, "{out}");
    assert_eq!(out, "6\n2\n");
}

// Perspective 17: range loop variables are immutable.
#[test]
fn test_range_for_loop_perspective_17_loop_var_assignment_is_error() {
    assert_compile_error_contains(
        r#"
fn main() {
    for n in 1..3 {
        n = 9;
    }
}
"#,
        &["error[E0301]", "cannot assign to immutable variable `n`"],
    );
}

// Perspective 18: range loop variables do not leak out of the body.
#[test]
fn test_range_for_loop_perspective_18_loop_var_is_scoped_to_body() {
    assert_compile_error_contains(
        r#"
fn main() {
    for n in 1..3 {
        println(n);
    }
    println(n);
}
"#,
        &["error[E0350]", "cannot find variable `n`"],
    );
}

// Perspective 19: the start bound must be i64.
#[test]
fn test_range_for_loop_perspective_19_start_bound_must_be_i64() {
    assert_compile_error_contains(
        r#"
fn main() {
    for n in true..3 {
        println(n);
    }
}
"#,
        &["error[E0201]", "range bounds must be `i64`"],
    );
}

// Perspective 20: the end bound must be i64.
#[test]
fn test_range_for_loop_perspective_20_end_bound_must_be_i64() {
    assert_compile_error_contains(
        r#"
fn main() {
    for n in 1..3.5 {
        println(n);
    }
}
"#,
        &["error[E0201]", "range bounds must be `i64`"],
    );
}

// Perspective 21: a range outside a `for` loop is now a first-class value.
#[test]
fn test_range_for_loop_perspective_21_range_value_outside_for_is_allowed() {
    let (out, ok) = compile_and_run(
        r#"
fn main() {
    let r = 1..3;
    println(r.start);
    println(r.end);
}
"#,
    );
    assert!(ok, "{out}");
    assert_eq!(out, "1\n3\n");
}

// Perspective 22: await works inside range loops in async main and leaf fns.
#[test]
fn test_range_for_loop_perspective_22_async_await_in_range_main_and_leaf() {
    let (out, ok) = compile_and_run_gc_stress(
        r#"
async fn sum() -> i64 {
    let mut total = 0;
    for n in 1..5 {
        await sleep(1);
        total = total + n;
    }
    return total;
}

async fn main() {
    for n in 1..4 {
        await sleep(1);
        println(n);
    }
    println(await sum());
}
"#,
    );
    assert!(ok, "{out}");
    assert_eq!(out, "1\n2\n3\n10\n");
}

// ── Map<K,V> type (willow-5t6) ─────────────────────────────────────────────
// GC-managed hash map: Map::new(), .insert(k,v), .get(k) -> Option<V>,
// .contains(k) -> bool, .len() -> i64. Keys: String (by content) or i64.

// Perspective 1: insert/get/len with String keys.
#[test]
fn test_map_string_key_insert_get_len() {
    let (out, ok) = compile_and_run(
        r#"
import std::collections::Map;

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
import std::collections::Map;

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
import std::collections::Map;

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
import std::collections::Map;

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
import std::collections::Map;

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
import std::collections::Map;

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
import std::collections::Map;

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
import std::collections::Map;

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
import std::collections::Map;

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
import std::collections::Map;

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
import std::collections::Map;

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
import std::collections::Map;

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
import std::collections::Map;

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
import std::collections::Map;

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
import std::collections::Map;

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
import std::collections::Map;

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
import std::collections::Map;

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
import std::collections::Map;

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
import std::collections::Map;

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
import std::collections::Map;

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
import std::collections::Array;

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
import std::collections::Array;

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
import std::collections::Array;

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
import std::collections::Array;

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
import std::collections::Array;

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
import std::collections::Array;

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
import std::collections::Array;

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
import std::collections::Array;

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
import std::collections::Array;

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

// Perspective 2: `::`-separated module paths are accepted on the entry file.
#[test]
fn test_module_decl_colon_entry_compiles() {
    let (out, ok) = compile_and_run(
        r#"
module myapp::tools;
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
        "module std::io;\nfn main() {}\n",
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
                "module foo::bar;\npub fn val() -> i64 { return 77; }\n",
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
                "module foo::baz;\npub fn val() -> i64 { return 1; }\n",
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
                "import math::add;\nfn main() { println(add(2, 3)); }\n",
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
                "import math::add as plus;\nfn main() { println(plus(10, 20)); }\n",
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
                "import math::add;\nimport math::mul;\nfn main() { println(add(2, 3)); println(mul(2, 3)); }\n",
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
                "import math::secret;\nfn main() { println(secret()); }\n",
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
            ("main.wi", "import math::nope;\nfn main() { println(1); }\n"),
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
                "import math;\nimport math::add;\nfn main() { println(add(1, 1)); println(math::mul(2, 4)); }\n",
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
                "import math::mul;\nfn main() { println(mul(6, 7)); }\n",
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
                "import math::add;\nfn twice(n: i64) -> i64 { return add(n, n); }\nfn main() { println(twice(21)); }\n",
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
                "import math::add;\nfn main() { println(add(3, 4) * 2); }\n",
            ),
            math_module(),
        ],
        "main.wi",
    );
    assert!(ok);
    assert_eq!(out, "14\n");
}

// Perspective 10: a nested-module item import (`import foo::bar::baz;`).
#[test]
fn test_item_import_nested_module() {
    let (out, ok) = compile_temp_project_and_run(
        &[
            (
                "main.wi",
                "import foo::bar::baz;\nfn main() { println(baz()); }\n",
            ),
            (
                "foo/bar.wi",
                "module foo::bar;\npub fn baz() -> i64 { return 88; }\n",
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
                "import math::add;\nimport math::mul as times;\nfn main() { println(add(1, 2)); println(times(3, 4)); }\n",
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
    pub static fn new(v: i64) -> P { return new P(v); }
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
                "module geom;\npub class Point {\n    pub x: i64;\n    pub y: i64;\n    pub static fn new(x: i64, y: i64) -> Point { return new Point(x, y); }\n    pub fn sum(self) -> i64 { return self.x + self.y; }\n}\n",
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
                "module geom;\npub class Point {\n    pub x: i64;\n    pub y: i64;\n    pub static fn new(x: i64, y: i64) -> Point { return new Point(x, y); }\n    pub fn sum(self) -> i64 { return self.x + self.y; }\n}\npub fn origin_sum() -> i64 { let p = Point::new(3, 4); return p.sum(); }\n",
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
                "module geom;\npub class Point {\n    pub x: i64;\n    pub y: i64;\n    pub static fn new(x: i64, y: i64) -> Point { return new Point(x, y); }\n    pub fn sum(self) -> i64 { return self.x + self.y; }\n}\n",
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
                "import left::math::value as left_value;\nimport right::math::value as right_value;\nfn main() { println(left_value()); println(right_value()); }\n",
            ),
            (
                "left/math.wi",
                "module left::math;\npub fn value() -> i64 { return 11; }\n",
            ),
            (
                "right/math.wi",
                "module right::math;\npub fn value() -> i64 { return 22; }\n",
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
    pub static fn new(v: i64) -> P { return new P(v); }
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
        &s5_project("import a::hidden;\nfn main() { println(hidden()); }\n"),
        "main.wi",
    );
    assert!(stderr.contains("error[E2006]"), "stderr: {stderr}");
    assert!(stderr.contains("private"), "stderr: {stderr}");
}

// Two item imports binding the same local name collide.
#[test]
fn test_duplicate_item_import_rejected() {
    let stderr = compile_temp_project_error_stderr(
        &s5_project("import a::dup;\nimport b::dup;\nfn main() { println(dup()); }\n"),
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
        &s5_project("import a::f;\nfn f() -> i64 { return 0; }\nfn main() { println(f()); }\n"),
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
        &s5_project("import a::f;\nclass f { pub v: i64; }\nfn main() {}\n"),
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
            "import a::f;\nimport b::g;\nfn helper() -> i64 { return 100; }\nfn main() { println(f() + g() + helper()); }\n",
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
            "import a::dup;\nimport b::dup as bdup;\nfn main() { println(dup() + bdup()); }\n",
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
import std::collections::Array;

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
import std::collections::Array;

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
import std::collections::Array;

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
import std::collections::Array;

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
import std::collections::Array;

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
import std::collections::Array;

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
import std::collections::Array;

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
import std::collections::Array;

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
import std::collections::Array;

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
import std::collections::Array;

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
import std::collections::Array;

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
import std::collections::Array;

class Bag {
    pub items: Array<String>;
    pub static fn new(items: Array<String>) -> Bag { return new Bag(items); }
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
    let opt = Option::Some(new Node(8));
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
    let l = new Lookup(5, 100);
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

// Channel/Future locals are opaque RUNTIME pointers with no GC header, so
// is_gc_managed must NOT root them on the shadow stack — otherwise the collector
// reads a bogus header at payload_to_header and crashes once a collection scans
// the root (willow-lpn.9). Task/JoinHandle are GC async frames in the cooperative
// scheduler path, so it is safe and necessary to trace them.

// A spawned void function joined while collections fire on every allocation.
// The JoinHandle local is a GC frame and remains valid across collection.
#[test]
fn gc_stress_07_spawn_join_void() {
    let (out, ok) = compile_and_run_gc_stress(
        r#"
async fn say() {
    println("hi");
}
fn main() {
    let h = say();
    gc_collect();
    h.join();
    println("done");
}
"#,
    );
    assert!(ok, "spawn/join must not crash under GC stress: {out}");
    assert_eq!(out, "hi\ndone\n");
}

// Awaiting task values of scalar types under stress. Task locals are async frame
// pointers and must remain traced across collection.
#[test]
fn gc_stress_08_task_await_scalars() {
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
async fn producer(ch: Channel<i64>) {
    ch.send(10);
    ch.send(20);
    ch.close();
}
fn main() {
    let ch = Channel<i64>::new();
    let h = producer(ch);
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
        "{IFACE_ANIMALS}\nfn say(a: Animal) {{ println(a.speak()); }}\nfn main() {{ say(new Dog()); say(new Cat()); }}"
    ));
    assert!(ok, "interface dispatch must compile and run");
    assert_eq!(out, "woof\nmeow\n");
}

#[test]
fn iface_dispatch_02_local_binding() {
    let (out, ok) = compile_and_run(&format!(
        "{IFACE_ANIMALS}\nfn main() {{ let a: Animal = new Dog(); println(a.speak()); }}"
    ));
    assert!(ok);
    assert_eq!(out, "woof\n");
}

#[test]
fn iface_dispatch_03_return_coercion() {
    let (out, ok) = compile_and_run(&format!(
        "{IFACE_ANIMALS}\nfn pick(b: bool) -> Animal {{ if b {{ return new Dog(); }} return new Cat(); }}\nfn main() {{ println(pick(true).speak()); println(pick(false).speak()); }}"
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
fn main() { show(new Square(6)); }
"#,
    );
    assert!(ok);
    assert_eq!(out, "square\n36\n");
}

#[test]
fn iface_dispatch_05_reassignment() {
    let (out, ok) = compile_and_run(&format!(
        "{IFACE_ANIMALS}\nfn main() {{ let mut a: Animal = new Dog(); println(a.speak()); a = new Cat(); println(a.speak()); }}"
    ));
    assert!(ok);
    assert_eq!(out, "woof\nmeow\n");
}

// spec 14.6: interface values must survive collection under GC stress.

#[test]
fn iface_gc_stress_01_local_survives() {
    let (out, ok) = compile_and_run_gc_stress(&format!(
        "{IFACE_ANIMALS}\nfn main() {{ let a: Animal = new Dog(); gc_collect(); println(a.speak()); }}"
    ));
    assert!(ok, "interface local must survive GC: {out}");
    assert_eq!(out, "woof\n");
}

#[test]
fn iface_gc_stress_02_param_survives() {
    let (out, ok) = compile_and_run_gc_stress(&format!(
        "{IFACE_ANIMALS}\nfn say(a: Animal) {{ gc_collect(); println(a.speak()); }}\nfn main() {{ say(new Dog()); }}"
    ));
    assert!(ok, "interface parameter must survive GC: {out}");
    assert_eq!(out, "woof\n");
}

#[test]
fn iface_gc_stress_03_method_result_string_survives() {
    let (out, ok) = compile_and_run_gc_stress(&format!(
        "{IFACE_ANIMALS}\nfn main() {{ let a: Animal = new Dog(); let s = a.speak(); gc_collect(); println(s); }}"
    ));
    assert!(ok, "interface method-result String must survive GC: {out}");
    assert_eq!(out, "woof\n");
}

// spec 14.4: a class field typed as an interface.
#[test]
fn iface_field_01_dispatch_through_field() {
    let (out, ok) = compile_and_run(&format!(
        "{IFACE_ANIMALS}\nclass Holder {{ pub value: Animal; }}\nfn main() {{ let h = new Holder(new Dog()); println(h.value.speak()); }}"
    ));
    assert!(ok, "interface field dispatch must work: {out}");
    assert_eq!(out, "woof\n");
}

#[test]
fn iface_field_02_gc_stress_field_survives() {
    let (out, ok) = compile_and_run_gc_stress(&format!(
        "{IFACE_ANIMALS}\nclass Holder {{ pub value: Animal; }}\nfn main() {{ let h = new Holder(new Dog()); gc_collect(); println(h.value.speak()); }}"
    ));
    assert!(ok, "interface field must survive GC: {out}");
    assert_eq!(out, "woof\n");
}

// spec 14.5: Array<Interface> (empty literal + push, the documented pattern).
#[test]
fn iface_array_01_push_and_dispatch() {
    let (out, ok) = compile_and_run(&format!(
        "import std::collections::Array;\n{IFACE_ANIMALS}\nfn main() {{ let xs: Array<Animal> = []; xs.push(new Dog()); xs.push(new Cat()); println(xs[0].speak()); println(xs[1].speak()); }}"
    ));
    assert!(ok, "Array<Interface> must work: {out}");
    assert_eq!(out, "woof\nmeow\n");
}

#[test]
fn iface_array_02_gc_stress_elements_survive() {
    let (out, ok) = compile_and_run_gc_stress(&format!(
        "import std::collections::Array;\n{IFACE_ANIMALS}\nfn main() {{ let xs: Array<Animal> = []; xs.push(new Dog()); xs.push(new Cat()); gc_collect(); println(xs[0].speak()); println(xs[1].speak()); }}"
    ));
    assert!(ok, "Array<Interface> elements must survive GC: {out}");
    assert_eq!(out, "woof\nmeow\n");
}

#[test]
fn iface_array_03_index_assign_boxes() {
    let (out, ok) = compile_and_run(&format!(
        "import std::collections::Array;\n{IFACE_ANIMALS}\nfn main() {{ let xs: Array<Animal> = []; xs.push(new Dog()); xs[0] = new Cat(); println(xs[0].speak()); }}"
    ));
    assert!(ok, "interface index-assign must box: {out}");
    assert_eq!(out, "meow\n");
}

#[test]
fn iface_array_04_nonempty_literal_with_annotation() {
    // A non-empty `Array<Interface>` literal of differing classes is checked
    // element-wise against the interface and each element is boxed.
    let (out, ok) = compile_and_run(&format!(
        "import std::collections::Array;\n{IFACE_ANIMALS}\nfn main() {{ let xs: Array<Animal> = [new Dog(), new Cat()]; println(xs[0].speak()); println(xs[1].speak()); }}"
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
    say(new animals::Dog());
    let a: animals::Animal = new animals::Dog();
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
    let b: Box = new Box("nested");
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
import std::collections::Array;

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
import std::collections::Map;

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
async fn main() { println(await f(new Node(77, nil))); }
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
fn async_frame_18_task_local_traced_across_await() {
    // A Task local held across an await is a GC async-frame pointer; it must be
    // traced as a heap object and remain awaitable after collection.
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
        "Task local across await must stay alive across collection: {out}"
    );
    assert_eq!(out, "7\n");
}

#[test]
fn async_frame_19_join_handle_local_not_traced_across_await() {
    let (out, ok) = compile_and_run_gc_stress(
        r#"
async fn work() { println("worked"); }
async fn f() {
    let h = work();
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
async fn producer(ch: Channel<i64>) { ch.send(11); ch.close(); }
async fn f() -> i64 {
    let ch = Channel<i64>::new();
    let h = producer(ch);
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
    let main = "import animals;\nfn main() { let s: animals::Secret = new animals::Secret(5); println(s.v); }\n";
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
    let m = "module animals;\nclass Secret { pub v: i64; }\npub class Dog { pub fn speak(self) -> i64 { return 1; } }\n";
    let main = "import animals;\nfn main() { let d: animals::Dog = new animals::Dog(); println(d.speak()); }\n";
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
    let m = "module animals;\nclass Secret { pub static fn make() -> i64 { return 9; } }\npub class Dog {}\n";
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
    let main = "import shapes;\nfn describe(s: shapes::Shape) { println(s.area()); }\nfn main() { describe(new shapes::Sq(4)); }\n";
    let (out, ok) = compile_temp_project_and_run(&[("shapes.wi", m), ("main.wi", main)], "main.wi");
    assert!(ok, "pub module interface must be accessible: {out}");
    assert_eq!(out, "16\n");
}

// ── fn main() -> Result<void, E> (willow-exg) ────────────────────────────────

#[test]
fn main_result_01_err_prints_and_exits_nonzero() {
    let (out, ok) = compile_and_run_check_exit(
        r#"
fn main() -> Result<void, String> {
    return Result::Err("boom");
}
"#,
    );
    assert!(!ok, "Err main must exit non-zero");
    assert!(
        out.contains("boom"),
        "Err report must include the message: {out}"
    );
}

#[test]
fn main_result_02_ok_exits_zero() {
    let (out, ok) = compile_and_run_check_exit(
        r#"
fn main() -> Result<void, String> {
    println(7);
    return Result::Ok();
}
"#,
    );
    assert!(ok, "Ok main must exit 0: {out}");
    assert_eq!(out, "7\n");
}

#[test]
fn main_result_03_implicit_end_is_success() {
    // Falling off the end of a Result<void,E> main is success (exit 0).
    let (out, ok) = compile_and_run_check_exit(
        r#"
fn main() -> Result<void, String> {
    println(99);
}
"#,
    );
    assert!(ok, "implicit-end main must exit 0: {out}");
    assert_eq!(out, "99\n");
}

#[test]
fn main_result_04_question_mark_propagates_err() {
    let (out, ok) = compile_and_run_check_exit(
        r#"
fn risky(ok: bool) -> Result<i64, String> {
    if ok { return Result::Ok(7); }
    return Result::Err("propagated");
}
fn main() -> Result<void, String> {
    let x = risky(false)?;
    println(x);
    return Result::Ok();
}
"#,
    );
    assert!(!ok, "? propagating Err must exit non-zero");
    assert!(
        out.contains("propagated"),
        "should report the propagated error: {out}"
    );
}

#[test]
fn main_result_05_question_mark_success_path() {
    let (out, ok) = compile_and_run_check_exit(
        r#"
fn risky(ok: bool) -> Result<i64, String> {
    if ok { return Result::Ok(7); }
    return Result::Err("nope");
}
fn main() -> Result<void, String> {
    let x = risky(true)?;
    println(x);
    return Result::Ok();
}
"#,
    );
    assert!(ok, "? success path must exit 0: {out}");
    assert_eq!(out, "7\n");
}

#[test]
fn main_result_06_non_string_error_exits_nonzero() {
    let (out, ok) = compile_and_run_check_exit(
        r#"
fn main() -> Result<void, i64> {
    return Result::Err(42);
}
"#,
    );
    assert!(!ok, "non-String Err main must still exit non-zero: {out}");
}

// ── Explicit toString() + String-only concatenation (willow-fvfc) ────────────

#[test]
fn tostring_01_primitives_and_class() {
    let (out, ok) = compile_and_run(
        r#"
class Point { pub x: i64; pub y: i64;
    pub fn toString(self) -> String { return "(" + self.x.toString() + ", " + self.y.toString() + ")"; }
}
fn main() {
    println(42.toString());
    println(true.toString());
    println(false.toString());
    println(3.5.toString());
    println("hi".toString());
    println("x = " + 42.toString());
    let p = new Point(3, 4);
    println(p.toString());
}
"#,
    );
    assert!(ok, "toString must compile and run: {out}");
    assert_eq!(out, "42\ntrue\nfalse\n3.5\nhi\nx = 42\n(3, 4)\n");
}

#[test]
fn tostring_02_gc_stress() {
    // toString allocates WillowStrings; concatenation chains must stay GC-safe.
    let (out, ok) = compile_and_run_gc_stress(
        r#"
fn main() {
    let n = 7;
    let s = "n=" + n.toString() + " ok=" + true.toString();
    println(s);
}
"#,
    );
    assert!(ok, "toString/concat must survive GC stress: {out}");
    assert_eq!(out, "n=7 ok=true\n");
}

#[test]
fn tostring_03_string_plus_int_rejected() {
    assert_compile_error_contains(
        r#"fn main() { println("x = " + 42); }"#,
        &["error[E0202]", "cannot concatenate", ".toString()"],
    );
}

#[test]
fn tostring_04_int_plus_string_rejected() {
    assert_compile_error_contains(
        r#"fn main() { let s: String = "y"; println(42 + s); }"#,
        &["error[E0202]", "cannot concatenate"],
    );
}

#[test]
fn tostring_05_string_plus_bool_and_f64_rejected() {
    assert_compile_error_contains(
        r#"fn main() { println("b = " + true); }"#,
        &["error[E0202]", "cannot concatenate `String` with `bool`"],
    );
    assert_compile_error_contains(
        r#"fn main() { println("f = " + 3.5); }"#,
        &["error[E0202]", "cannot concatenate `String` with `f64`"],
    );
}

// ── panic() builtin (regression: codegen no longer crashes; willow-4j6) ──────

#[test]
fn panic_01_compiles_runs_and_exits_nonzero() {
    let (out, ok) = compile_and_run_check_exit(r#"fn main() { panic("boom"); }"#);
    assert!(!ok, "panic must exit non-zero");
    assert!(
        out.contains("boom"),
        "panic should print its message: {out}"
    );
}

#[test]
fn panic_02_in_nested_function() {
    let (out, ok) = compile_and_run_check_exit(
        r#"
fn deeper() { panic("deep failure"); }
fn helper() { deeper(); }
fn main() { helper(); }
"#,
    );
    assert!(!ok, "panic in a nested call must exit non-zero");
    assert!(
        out.contains("deep failure"),
        "panic message must appear: {out}"
    );
}

#[test]
fn panic_03_debug_build_reports_source_location() {
    // Debug builds include the panic call-site location (willow-4j6).
    let (out, ok) = compile_and_run_check_exit("fn main() {\n    panic(\"located\");\n}\n");
    assert!(!ok, "panic must exit non-zero");
    assert!(out.contains("located"), "message present: {out}");
    assert!(
        out.contains(".wi:2:"),
        "debug panic should report source line: {out}"
    );
}

// ── Generic interface declarations (willow-1js.1, slice 1) ───────────────────

#[test]
fn generic_interface_01_declaration_compiles() {
    // A generic interface declares with type params; method sigs may reference
    // them. (Implementing/dispatch on generic interfaces is a later slice.)
    let (out, ok) = compile_and_run(
        r#"
interface Box<T> {
    fn get(self) -> T;
}
interface Conv<A, B> {
    fn run(self, a: A) -> B;
}
fn main() { println(7); }
"#,
    );
    assert!(ok, "generic interface declarations must type-check: {out}");
    assert_eq!(out, "7\n");
}

// ── Generic interfaces: implement, dispatch, conformance (willow-1js.1) ──────

#[test]
fn generic_interface_02_implement_and_dispatch_i64() {
    let (out, ok) = compile_and_run(
        r#"
interface Box<T> { fn get(self) -> T; }
class IntBox implements Box<i64> {
    n: i64;
    pub fn get(self) -> i64 { return self.n; }
}
fn main() {
    let b: Box<i64> = new IntBox(99);
    println(b.get());
}
"#,
    );
    assert!(ok, "{out}");
    assert_eq!(out, "99\n");
}

#[test]
fn generic_interface_03_implement_and_dispatch_string() {
    let (out, ok) = compile_and_run(
        r#"
interface Box<T> { fn get(self) -> T; }
class TextBox implements Box<String> {
    s: String;
    pub fn get(self) -> String { return self.s; }
}
fn main() {
    let b: Box<String> = new TextBox("hi");
    println(b.get());
}
"#,
    );
    assert!(ok, "{out}");
    assert_eq!(out, "hi\n");
}

#[test]
fn generic_interface_04_two_type_params() {
    let (out, ok) = compile_and_run(
        r#"
interface Pair<A, B> { fn first(self) -> A; fn second(self) -> B; }
class P implements Pair<i64, String> {
    a: i64;
    b: String;
    pub fn first(self) -> i64 { return self.a; }
    pub fn second(self) -> String { return self.b; }
}
fn main() {
    let p: Pair<i64, String> = new P(7, "x");
    println(p.first());
    println(p.second());
}
"#,
    );
    assert!(ok, "{out}");
    assert_eq!(out, "7\nx\n");
}

#[test]
fn generic_interface_05_param_typed_by_type_param() {
    // Method takes an argument whose type is the type parameter.
    let (out, ok) = compile_and_run(
        r#"
interface Sink<T> { fn put(self, v: T) -> T; }
class IntSink implements Sink<i64> {
    pub fn put(self, v: i64) -> i64 { return v + 1; }
}
fn main() {
    let s: Sink<i64> = new IntSink();
    println(s.put(41));
}
"#,
    );
    assert!(ok, "{out}");
    assert_eq!(out, "42\n");
}

#[test]
fn generic_interface_06_self_return_on_concrete() {
    let (out, ok) = compile_and_run(
        r#"
interface From<E> { fn from(self, e: E) -> Self; }
class W implements From<i64> {
    n: i64;
    pub fn from(self, e: i64) -> W { return new W(e); }
    pub fn val(self) -> i64 { return self.n; }
}
fn main() {
    let w = new W(0);
    println(w.from(42).val());
}
"#,
    );
    assert!(ok, "{out}");
    assert_eq!(out, "42\n");
}

#[test]
fn generic_interface_07_passed_to_function() {
    let (out, ok) = compile_and_run(
        r#"
interface Box<T> { fn get(self) -> T; }
class IntBox implements Box<i64> {
    n: i64;
    pub fn get(self) -> i64 { return self.n; }
}
fn show(b: Box<i64>) { println(b.get()); }
fn main() { show(new IntBox(5)); }
"#,
    );
    assert!(ok, "{out}");
    assert_eq!(out, "5\n");
}

#[test]
fn generic_interface_08_two_instantiations_distinct() {
    let (out, ok) = compile_and_run(
        r#"
interface Box<T> { fn get(self) -> T; }
class IntBox implements Box<i64> {
    n: i64;
    pub fn get(self) -> i64 { return self.n; }
}
class TextBox implements Box<String> {
    s: String;
    pub fn get(self) -> String { return self.s; }
}
fn main() {
    let a: Box<i64> = new IntBox(1);
    let b: Box<String> = new TextBox("two");
    println(a.get());
    println(b.get());
}
"#,
    );
    assert!(ok, "{out}");
    assert_eq!(out, "1\ntwo\n");
}

// Negative perspectives ------------------------------------------------------

#[test]
fn generic_interface_neg_01_too_few_type_args() {
    // `Box` requires one type argument (E0422).
    assert!(expect_compile_error(
        r#"
interface Box<T> { fn get(self) -> T; }
class C implements Box { pub fn get(self) -> i64 { return 1; } }
fn main() {}
"#,
    ));
}

#[test]
fn generic_interface_neg_02_too_many_type_args() {
    assert!(expect_compile_error(
        r#"
interface Box<T> { fn get(self) -> T; }
class C implements Box<i64, i64> { pub fn get(self) -> i64 { return 1; } }
fn main() {}
"#,
    ));
}

#[test]
fn generic_interface_neg_03_return_type_mismatch_after_subst() {
    // With T=String, `get` must return String; returning i64 is a mismatch.
    assert!(expect_compile_error(
        r#"
interface Box<T> { fn get(self) -> T; }
class C implements Box<String> { pub fn get(self) -> i64 { return 1; } }
fn main() {}
"#,
    ));
}

#[test]
fn generic_interface_neg_04_param_type_mismatch_after_subst() {
    assert!(expect_compile_error(
        r#"
interface Sink<T> { fn put(self, v: T) -> T; }
class C implements Sink<i64> { pub fn put(self, v: String) -> i64 { return 1; } }
fn main() {}
"#,
    ));
}

#[test]
fn generic_interface_neg_05_missing_method() {
    assert!(expect_compile_error(
        r#"
interface Box<T> { fn get(self) -> T; }
class C implements Box<i64> {}
fn main() {}
"#,
    ));
}

#[test]
fn generic_interface_neg_06_unknown_method_on_iface_value() {
    assert!(expect_compile_error(
        r#"
interface Box<T> { fn get(self) -> T; }
class IntBox implements Box<i64> {
    n: i64;
    pub fn get(self) -> i64 { return self.n; }
}
fn main() {
    let b: Box<i64> = new IntBox(1);
    b.missing();
}
"#,
    ));
}

#[test]
fn generic_interface_neg_07_wrong_instantiation_not_assignable() {
    // An IntBox implements Box<i64>, not Box<String>.
    assert!(expect_compile_error(
        r#"
interface Box<T> { fn get(self) -> T; }
class IntBox implements Box<i64> {
    n: i64;
    pub fn get(self) -> i64 { return self.n; }
}
fn main() {
    let b: Box<String> = new IntBox(1);
}
"#,
    ));
}

// ── `?` automatic error conversion via Into<E> (willow-1ow) ─────────────────

#[test]
fn try_convert_01_err_path_converts() {
    let (out, ok) = compile_and_run(
        r#"
class AppErr { pub code: i64; }
class LowErr implements Into<AppErr> {
    pub n: i64;
    pub fn into(self) -> AppErr { return new AppErr(900 + self.n); }
}
fn low() -> Result<i64, LowErr> { return Result::Err(new LowErr(5)); }
fn high() -> Result<i64, AppErr> { let v = low()?; return Result::Ok(v); }
fn main() {
    let out = match high() {
        Result::Ok(v) => v,
        Result::Err(e) => e.code,
    };
    println(out);
}
"#,
    );
    assert!(ok, "{out}");
    assert_eq!(out, "905\n");
}

#[test]
fn try_convert_02_ok_path_flows_through() {
    let (out, ok) = compile_and_run(
        r#"
class AppErr { pub code: i64; }
class LowErr implements Into<AppErr> {
    pub n: i64;
    pub fn into(self) -> AppErr { return new AppErr(0); }
}
fn low() -> Result<i64, LowErr> { return Result::Ok(11); }
fn high() -> Result<i64, AppErr> { let v = low()?; return Result::Ok(v + 1); }
fn main() {
    let out = match high() {
        Result::Ok(v) => v,
        Result::Err(e) => e.code,
    };
    println(out);
}
"#,
    );
    assert!(ok, "{out}");
    assert_eq!(out, "12\n");
}

#[test]
fn try_convert_03_exact_match_unaffected() {
    // E1 == E2: no conversion, original error propagates.
    let (out, ok) = compile_and_run(
        r#"
fn low() -> Result<i64, String> { return Result::Err("boom"); }
fn high() -> Result<i64, String> { let v = low()?; return Result::Ok(v); }
fn main() {
    let out = match high() {
        Result::Ok(v) => v,
        Result::Err(e) => -1,
    };
    println(out);
}
"#,
    );
    assert!(ok, "{out}");
    assert_eq!(out, "-1\n");
}

#[test]
fn try_convert_04_two_question_marks() {
    let (out, ok) = compile_and_run(
        r#"
class AppErr { pub code: i64; }
class LowErr implements Into<AppErr> {
    pub n: i64;
    pub fn into(self) -> AppErr { return new AppErr(self.n); }
}
fn a() -> Result<i64, LowErr> { return Result::Ok(2); }
fn b() -> Result<i64, LowErr> { return Result::Err(new LowErr(77)); }
fn high() -> Result<i64, AppErr> {
    let x = a()?;
    let y = b()?;
    return Result::Ok(x + y);
}
fn main() {
    let out = match high() {
        Result::Ok(v) => v,
        Result::Err(e) => e.code,
    };
    println(out);
}
"#,
    );
    assert!(ok, "{out}");
    assert_eq!(out, "77\n");
}

#[test]
fn try_convert_05_no_into_impl_is_error() {
    assert!(expect_compile_error(
        r#"
class AppErr { pub code: i64; }
class LowErr { pub n: i64; }
fn low() -> Result<i64, LowErr> { return Result::Err(new LowErr(1)); }
fn high() -> Result<i64, AppErr> { let v = low()?; return Result::Ok(v); }
fn main() {}
"#,
    ));
}

#[test]
fn try_convert_06_option_question_unaffected() {
    let (out, ok) = compile_and_run(
        r#"
fn first() -> Option<i64> { return Option::None; }
fn run() -> Option<i64> { let v = first()?; return Option::Some(v + 1); }
fn main() {
    let out = match run() {
        Option::Some(v) => v,
        Option::None => -9,
    };
    println(out);
}
"#,
    );
    assert!(ok, "{out}");
    assert_eq!(out, "-9\n");
}

#[test]
fn try_convert_07_into_wrong_target_still_errors() {
    // LowErr implements Into<Other>, not Into<AppErr>: still a mismatch.
    assert!(expect_compile_error(
        r#"
class AppErr { pub code: i64; }
class Other { pub x: i64; }
class LowErr implements Into<Other> {
    pub n: i64;
    pub fn into(self) -> Other { return new Other(0); }
}
fn low() -> Result<i64, LowErr> { return Result::Err(new LowErr(1)); }
fn high() -> Result<i64, AppErr> { let v = low()?; return Result::Ok(v); }
fn main() {}
"#,
    ));
}

#[test]
fn try_convert_08_payload_data_preserved() {
    // The converted error carries data computed from the source error.
    let (out, ok) = compile_and_run(
        r#"
class AppErr { pub code: i64; }
class LowErr implements Into<AppErr> {
    pub n: i64;
    pub fn into(self) -> AppErr { return new AppErr(self.n * 10); }
}
fn low() -> Result<i64, LowErr> { return Result::Err(new LowErr(6)); }
fn high() -> Result<i64, AppErr> { let v = low()?; return Result::Ok(v); }
fn main() {
    let out = match high() {
        Result::Ok(v) => v,
        Result::Err(e) => e.code,
    };
    println(out);
}
"#,
    );
    assert!(ok, "{out}");
    assert_eq!(out, "60\n");
}

#[test]
fn try_convert_08b_gc_managed_err_payload_rooted_during_into() {
    let (out, ok) = compile_and_run_gc_stress(
        r#"
class AppErr { pub msg: String; }
class LowErr implements Into<AppErr> {
    pub msg: String;
    pub fn into(self) -> AppErr {
        let prefix = "converted: ";
        gc_collect();
        return new AppErr(prefix + self.msg);
    }
}
fn low() -> Result<i64, LowErr> { return Result::Err(new LowErr("payload")); }
fn high() -> Result<i64, AppErr> { let v = low()?; return Result::Ok(v); }
fn main() {
    let out = match high() {
        Result::Ok(v) => "ok",
        Result::Err(e) => e.msg,
    };
    println(out);
}
"#,
    );
    assert!(ok, "{out}");
    assert_eq!(out, "converted: payload\n");
}

#[test]
fn try_convert_09_chained_three_levels() {
    // Conversion at each ? boundary up a three-level call chain.
    let (out, ok) = compile_and_run(
        r#"
class E1 implements Into<E2> {
    pub n: i64;
    pub fn into(self) -> E2 { return new E2(self.n + 1); }
}
class E2 implements Into<E3> {
    pub n: i64;
    pub fn into(self) -> E3 { return new E3(self.n + 1); }
}
class E3 { pub n: i64; }
fn a() -> Result<i64, E1> { return Result::Err(new E1(0)); }
fn b() -> Result<i64, E2> { let v = a()?; return Result::Ok(v); }
fn c() -> Result<i64, E3> { let v = b()?; return Result::Ok(v); }
fn main() {
    let out = match c() {
        Result::Ok(v) => v,
        Result::Err(e) => e.n,
    };
    println(out);
}
"#,
    );
    assert!(ok, "{out}");
    // E1{0} -> E2{1} at b's ?, then E2{1} -> E3{2} at c's ?.
    assert_eq!(out, "2\n");
}

#[test]
fn try_convert_10_two_source_types_one_target() {
    let (out, ok) = compile_and_run(
        r#"
class AppErr { pub code: i64; }
class IoErr implements Into<AppErr> {
    pub fn into(self) -> AppErr { return new AppErr(1); }
}
class FmtErr implements Into<AppErr> {
    pub fn into(self) -> AppErr { return new AppErr(2); }
}
fn io(fail: bool) -> Result<i64, IoErr> {
    if fail { return Result::Err(new IoErr()); }
    return Result::Ok(10);
}
fn fmt() -> Result<i64, FmtErr> { return Result::Err(new FmtErr()); }
fn high() -> Result<i64, AppErr> {
    let a = io(false)?;
    let b = fmt()?;
    return Result::Ok(a + b);
}
fn main() {
    let out = match high() {
        Result::Ok(v) => v,
        Result::Err(e) => e.code,
    };
    println(out);
}
"#,
    );
    assert!(ok, "{out}");
    assert_eq!(out, "2\n");
}

// ── Debug call-chain stack traces on panic (willow-992h) ─────────────────────

#[test]
fn callchain_01_nested_panic_prints_ordered_chain() {
    // deeper() <- helper() <- main(): the panic prints the active call chain,
    // most recent call first, with the call-site file:line:col of each frame.
    let (out, ok) = compile_and_run_check_exit(
        r#"
fn deeper() {
    panic("boom");
}
fn helper() {
    deeper();
}
fn main() {
    helper();
}
"#,
    );
    assert!(!ok, "program should abort on panic");
    assert!(
        out.contains("runtime panic: boom"),
        "missing panic line: {out}"
    );
    assert!(
        out.contains("call stack (most recent call first):"),
        "missing call stack header: {out}"
    );
    // Frame 0 is the innermost call (deeper), frame 1 is helper.
    let zero = out.find("0: deeper").expect(&format!("no frame 0: {out}"));
    let one = out.find("1: helper").expect(&format!("no frame 1: {out}"));
    assert!(zero < one, "frames out of order: {out}");
    // Each frame records its call site, not the callee body.
    assert!(
        out.contains("0: deeper at "),
        "frame 0 missing location: {out}"
    );
    assert!(
        out.contains("1: helper at "),
        "frame 1 missing location: {out}"
    );
}

#[test]
fn callchain_02_direct_panic_in_main_has_no_chain() {
    // main is the entry (not called via the instrumented path), so a panic
    // directly in main prints no call-stack section.
    let (out, ok) = compile_and_run_check_exit(
        r#"
fn main() {
    panic("top");
}
"#,
    );
    assert!(!ok);
    assert!(out.contains("runtime panic: top"), "{out}");
    assert!(
        !out.contains("call stack"),
        "main-only panic should have no chain: {out}"
    );
}

#[test]
fn callchain_03_release_build_omits_chain() {
    let id = unique_test_id();
    let src_path = temp_path(format!("willow_callchain_rel_{}.wi", id));
    let bin_path = temp_path(format!("willow_callchain_rel_{}", id));
    fs::write(
        &src_path,
        "fn inner() { panic(\"x\"); }\nfn main() { inner(); }\n",
    )
    .unwrap();

    let compiler = env!("CARGO_BIN_EXE_willowc");
    let status = Command::new(compiler)
        .args(["build", &src_path, "-o", &bin_path, "--release"])
        .stderr(Stdio::null())
        .status()
        .expect("failed to run compiler");
    assert!(status.success(), "release build failed");

    let out = Command::new(&bin_path).output().expect("run failed");
    let combined = format!(
        "{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    let _ = fs::remove_file(&src_path);
    remove_output_artifacts(&bin_path);

    assert!(combined.contains("runtime panic: x"), "{combined}");
    assert!(
        !combined.contains("call stack"),
        "release should omit call chain: {combined}"
    );
}

#[test]
fn callchain_04_three_levels() {
    let (out, ok) = compile_and_run_check_exit(
        r#"
fn c() { panic("deep"); }
fn b() { c(); }
fn a() { b(); }
fn main() { a(); }
"#,
    );
    assert!(!ok);
    let f0 = out.find("0: c").expect(&format!("{out}"));
    let f1 = out.find("1: b").expect(&format!("{out}"));
    let f2 = out.find("2: a").expect(&format!("{out}"));
    assert!(f0 < f1 && f1 < f2, "chain order wrong: {out}");
}

#[test]
fn callchain_05_method_call_in_chain() {
    // A panic inside a class method shows the method frame above its caller
    // (willow-phx3).
    let (out, ok) = compile_and_run_check_exit(
        r#"
class Worker {
    pub fn run(self) {
        panic("worker failed");
    }
}
fn helper(w: Worker) {
    w.run();
}
fn main() {
    let w = new Worker();
    helper(w);
}
"#,
    );
    assert!(!ok);
    assert!(out.contains("runtime panic: worker failed"), "{out}");
    let run = out
        .find("0: run")
        .expect(&format!("no method frame: {out}"));
    let helper = out
        .find("1: helper")
        .expect(&format!("no caller frame: {out}"));
    assert!(run < helper, "method frame must be innermost: {out}");
}

#[test]
fn async_frame_shadowed_locals_get_distinct_slots() {
    // An outer GC-managed local and a nested shadowed local of the SAME name,
    // both live across awaits, must occupy distinct async-frame slots — the
    // inner write must not clobber the outer (willow-lpn.11). Run under GC
    // stress so a mis-traced/aliased slot is caught.
    let src = r#"
async fn work() -> String {
    let s = "outer";
    await sleep(1);
    if s == "outer" {
        let s = "inner";
        await sleep(1);
        println(s);
    }
    await sleep(1);
    return s;
}

async fn main() {
    let r = await work();
    println(r);
}
"#;
    let (out, ok) = compile_and_run_gc_stress(src);
    assert!(ok, "async shadowing program must run: {out}");
    assert_eq!(
        out, "inner\nouter\n",
        "outer local was clobbered by inner: {out}"
    );
}

#[test]
fn generic_interface_neg_08_two_instantiations_unsatisfiable_rejected() {
    // A class MAY implement two instantiations of the same generic interface
    // (willow-1js.6), but only when one method body can satisfy every
    // instantiation. Here `get(self) -> T` cannot return both `i64` and
    // `String`, so conformance rejects it (E0417), not the duplicate check.
    assert!(expect_compile_error(
        r#"
interface Container<T> { fn get(self) -> T; }
class C implements Container<i64>, Container<String> {
    pub fn get(self) -> i64 { return 1; }
}
fn main() {}
"#,
    ));
}

#[test]
fn generic_interface_09_phantom_two_instantiations_allowed() {
    // When the interface's type parameter appears in no method signature
    // (a phantom/marker parameter), a class can implement several
    // instantiations at once; they share one identical vtable (willow-1js.6).
    let (out, ok) = compile_and_run(
        r#"
interface Tagged<T> { fn tag_name(self) -> String; }
class Item implements Tagged<i64>, Tagged<String> {
    pub fn tag_name(self) -> String { return "item"; }
}
fn use_int(t: Tagged<i64>) -> String { return t.tag_name(); }
fn use_str(t: Tagged<String>) -> String { return t.tag_name(); }
fn main() {
    let it = new Item();
    let a: Tagged<i64> = it;
    let b: Tagged<String> = it;
    println(use_int(a));
    println(use_str(b));
}
"#,
    );
    assert!(ok, "{out}");
    assert_eq!(out, "item\nitem\n");
}

#[test]
fn generic_interface_10_exact_duplicate_instantiation_rejected() {
    // The same instantiation listed twice is still a duplicate (E0414).
    assert!(expect_compile_error(
        r#"
interface Tagged<T> { fn tag_name(self) -> String; }
class Item implements Tagged<i64>, Tagged<i64> {
    pub fn tag_name(self) -> String { return "item"; }
}
fn main() {}
"#,
    ));
}

#[test]
fn generic_interface_11_phantom_three_instantiations_allowed() {
    // More than two instantiations of a phantom-parameter interface.
    let (out, ok) = compile_and_run(
        r#"
interface Marker<T> { fn kind(self) -> i64; }
class Node implements Marker<i64>, Marker<String>, Marker<bool> {
    pub fn kind(self) -> i64 { return 7; }
}
fn main() {
    let n: Marker<bool> = new Node();
    println(n.kind());
}
"#,
    );
    assert!(ok, "{out}");
    assert_eq!(out, "7\n");
}

// ── Default interface methods (willow-1js.3) ─────────────────────────────────

#[test]
fn default_method_01_used_when_not_overridden() {
    let (out, ok) = compile_and_run(
        r#"
interface Greeter {
    fn name(self) -> String;
    fn greet(self) -> String { return "Hi " + self.name(); }
}
class Dog implements Greeter {
    pub fn name(self) -> String { return "Rex"; }
}
fn main() { println(new Dog().greet()); }
"#,
    );
    assert!(ok, "{out}");
    assert_eq!(out, "Hi Rex\n");
}

#[test]
fn default_method_02_override_wins() {
    let (out, ok) = compile_and_run(
        r#"
interface Greeter {
    fn name(self) -> String;
    fn greet(self) -> String { return "Hi " + self.name(); }
}
class Cat implements Greeter {
    pub fn name(self) -> String { return "Tom"; }
    pub fn greet(self) -> String { return "Meow " + self.name(); }
}
fn main() { println(new Cat().greet()); }
"#,
    );
    assert!(ok, "{out}");
    assert_eq!(out, "Meow Tom\n");
}

#[test]
fn default_method_03_dispatch_through_interface() {
    let (out, ok) = compile_and_run(
        r#"
interface Greeter {
    fn name(self) -> String;
    fn greet(self) -> String { return "Hi " + self.name(); }
}
class Dog implements Greeter { pub fn name(self) -> String { return "Rex"; } }
fn run(g: Greeter) { println(g.greet()); }
fn main() { run(new Dog()); }
"#,
    );
    assert!(ok, "{out}");
    assert_eq!(out, "Hi Rex\n");
}

#[test]
fn default_method_04_default_calls_default() {
    let (out, ok) = compile_and_run(
        r#"
interface Calc {
    fn base(self) -> i64;
    fn doubled(self) -> i64 { return self.base() * 2; }
    fn plus(self, n: i64) -> i64 { return self.doubled() + n; }
}
class Num implements Calc { pub fn base(self) -> i64 { return 5; } }
fn main() {
    let n = new Num();
    println(n.doubled());
    println(n.plus(3));
}
"#,
    );
    assert!(ok, "{out}");
    assert_eq!(out, "10\n13\n");
}

#[test]
fn default_method_05_override_seen_by_other_default() {
    // shout() is a default that calls greet(); when greet() is overridden, the
    // default shout() must call the override (dynamic self-dispatch).
    let (out, ok) = compile_and_run(
        r#"
interface Greeter {
    fn name(self) -> String;
    fn greet(self) -> String { return "Hi " + self.name(); }
    fn shout(self) -> String { return self.greet() + "!"; }
}
class Robot implements Greeter {
    pub fn name(self) -> String { return "R2"; }
    pub fn greet(self) -> String { return "BEEP " + self.name(); }
}
fn main() { println(new Robot().shout()); }
"#,
    );
    assert!(ok, "{out}");
    assert_eq!(out, "BEEP R2!\n");
}

#[test]
fn default_method_06_required_method_still_enforced() {
    // A non-default (required) method must still be implemented.
    assert!(expect_compile_error(
        r#"
interface I {
    fn req(self) -> i64;
    fn opt(self) -> i64 { return 1; }
}
class C implements I {}
fn main() {}
"#,
    ));
}

#[test]
fn default_method_07_no_self_default_rejected() {
    // A default body requires a `self` receiver (E0420).
    assert!(expect_compile_error(
        r#"
interface I { fn f() { return; } }
fn main() {}
"#,
    ));
}

// ── Interface inheritance (willow-1js.2) ─────────────────────────────────────

#[test]
fn iface_inherit_01_class_usable_as_sub_and_super() {
    let (out, ok) = compile_and_run(
        r#"
interface Animal { fn name(self) -> String; }
interface Pet extends Animal { fn owner(self) -> String; }
class Dog implements Pet {
    pub fn name(self) -> String { return "Rex"; }
    pub fn owner(self) -> String { return "Sam"; }
}
fn as_animal(a: Animal) { println(a.name()); }
fn as_pet(p: Pet) { println(p.name() + "/" + p.owner()); }
fn main() {
    let d = new Dog();
    as_pet(d);
    as_animal(d);
}
"#,
    );
    assert!(ok, "{out}");
    assert_eq!(out, "Rex/Sam\nRex\n");
}

#[test]
fn iface_inherit_02_sub_interface_value_as_super() {
    let (out, ok) = compile_and_run(
        r#"
interface Animal { fn name(self) -> String; }
interface Pet extends Animal { fn owner(self) -> String; }
class Dog implements Pet {
    pub fn name(self) -> String { return "Rex"; }
    pub fn owner(self) -> String { return "Sam"; }
}
fn as_animal(a: Animal) { println(a.name()); }
fn main() {
    let p: Pet = new Dog();
    as_animal(p);
}
"#,
    );
    assert!(ok, "{out}");
    assert_eq!(out, "Rex\n");
}

#[test]
fn iface_inherit_03_missing_inherited_required_method_errors() {
    assert!(expect_compile_error(
        r#"
interface Animal { fn name(self) -> String; }
interface Pet extends Animal { fn owner(self) -> String; }
class Bad implements Pet { pub fn owner(self) -> String { return "x"; } }
fn main() {}
"#,
    ));
}

#[test]
fn iface_inherit_04_inherited_default_method() {
    let (out, ok) = compile_and_run(
        r#"
interface Named {
    fn name(self) -> String;
    fn label(self) -> String { return "name=" + self.name(); }
}
interface Pet extends Named { fn owner(self) -> String; }
class Dog implements Pet {
    pub fn name(self) -> String { return "Rex"; }
    pub fn owner(self) -> String { return "Sam"; }
}
fn main() {
    let p: Pet = new Dog();
    println(p.label());
}
"#,
    );
    assert!(ok, "{out}");
    assert_eq!(out, "name=Rex\n");
}

#[test]
fn iface_inherit_05_transitive_three_levels() {
    let (out, ok) = compile_and_run(
        r#"
interface A { fn a(self) -> i64; }
interface B extends A { fn b(self) -> i64; }
interface C extends B { fn c(self) -> i64; }
class Impl implements C {
    pub fn a(self) -> i64 { return 1; }
    pub fn b(self) -> i64 { return 2; }
    pub fn c(self) -> i64 { return 3; }
}
fn sum_a(x: A) -> i64 { return x.a(); }
fn main() {
    let v: C = new Impl();
    println(sum_a(v) + v.b() + v.c());
}
"#,
    );
    assert!(ok, "{out}");
    assert_eq!(out, "6\n");
}

// ── Interface -> concrete downcast via match (willow-1js.4) ──────────────────

#[test]
fn downcast_01_matches_concrete_and_calls_method() {
    let (out, ok) = compile_and_run(
        r#"
interface Animal { fn name(self) -> String; }
class Dog implements Animal {
    pub fn name(self) -> String { return "Rex"; }
    pub fn bark(self) -> String { return "woof"; }
}
class Cat implements Animal {
    pub fn name(self) -> String { return "Tom"; }
    pub fn meow(self) -> String { return "meow"; }
}
fn sound(a: Animal) -> String {
    return match a {
        Dog(d) => d.bark(),
        Cat(c) => c.meow(),
        _ => "?",
    };
}
fn main() {
    println(sound(new Dog()));
    println(sound(new Cat()));
}
"#,
    );
    assert!(ok, "{out}");
    assert_eq!(out, "woof\nmeow\n");
}

#[test]
fn downcast_02_wildcard_handles_other_classes() {
    let (out, ok) = compile_and_run(
        r#"
interface Animal { fn name(self) -> String; }
class Dog implements Animal { pub fn name(self) -> String { return "Rex"; } pub fn bark(self) -> String { return "woof"; } }
class Fish implements Animal { pub fn name(self) -> String { return "Nemo"; } }
fn sound(a: Animal) -> String {
    return match a {
        Dog(d) => d.bark(),
        _ => a.name(),
    };
}
fn main() {
    println(sound(new Dog()));
    println(sound(new Fish()));
}
"#,
    );
    assert!(ok, "{out}");
    assert_eq!(out, "woof\nNemo\n");
}

#[test]
fn downcast_03_underscore_binding_no_bind() {
    let (out, ok) = compile_and_run(
        r#"
interface Animal { fn name(self) -> String; }
class Dog implements Animal { pub fn name(self) -> String { return "Rex"; } }
class Cat implements Animal { pub fn name(self) -> String { return "Tom"; } }
fn kind(a: Animal) -> String {
    return match a {
        Dog(_) => "dog",
        Cat(_) => "cat",
        _ => "other",
    };
}
fn main() {
    println(kind(new Dog()));
    println(kind(new Cat()));
}
"#,
    );
    assert!(ok, "{out}");
    assert_eq!(out, "dog\ncat\n");
}

#[test]
fn downcast_04_gc_stress() {
    let (out, ok) = compile_and_run_gc_stress(
        r#"
interface Animal { fn name(self) -> String; }
class Dog implements Animal { pub fn name(self) -> String { return "Rex"; } pub fn bark(self) -> String { return "woof " + self.name(); } }
class Cat implements Animal { pub fn name(self) -> String { return "Tom"; } }
fn sound(a: Animal) -> String {
    return match a {
        Dog(d) => d.bark(),
        _ => a.name(),
    };
}
fn main() {
    println(sound(new Dog()));
    println(sound(new Cat()));
}
"#,
    );
    assert!(ok, "{out}");
    assert_eq!(out, "woof Rex\nTom\n");
}

#[test]
fn downcast_04b_debug_build_embeds_nil_guard_contexts() {
    let id = unique_test_id();
    let src_path = temp_path(format!("willow_downcast_guard_{}.wi", id));
    let bin_path = temp_path(format!("willow_downcast_guard_{}", id));
    let source = r#"
interface Animal { fn name(self) -> String; }
class Dog implements Animal { pub fn name(self) -> String { return "Rex"; } }
class Cat implements Animal { pub fn name(self) -> String { return "Tom"; } }
fn kind(a: Animal) -> String {
    return match a {
        Dog(_) => "dog",
        _ => "other",
    };
}
fn main() { println(kind(new Cat())); }
"#;
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

    assert!(content.contains("interface downcast box"));
    assert!(content.contains("interface downcast object"));
}

#[test]
fn downcast_neg_01_non_interface_scrutinee() {
    assert!(expect_compile_error(
        r#"
class Dog { pub fn bark(self) -> String { return "w"; } }
fn main() {
    let d = new Dog();
    let s = match d { Dog(x) => x.bark(), _ => "no" };
    println(s);
}
"#,
    ));
}

#[test]
fn downcast_neg_02_class_not_implementing_interface() {
    assert!(expect_compile_error(
        r#"
interface Animal { fn name(self) -> String; }
class Dog implements Animal { pub fn name(self) -> String { return "R"; } }
class Tree { pub fn h(self) -> i64 { return 1; } }
fn f(a: Animal) -> i64 { return match a { Tree(t) => t.h(), _ => 0 }; }
fn main() {}
"#,
    ));
}

// ── Interface inheritance validation (willow-1js.8) ──────────────────────────

#[test]
fn iface_inherit_neg_01_cycle_rejected() {
    assert!(expect_compile_error(
        r#"
interface A extends B { fn a(self) -> i64; }
interface B extends A { fn b(self) -> i64; }
fn main() {}
"#,
    ));
}

#[test]
fn iface_inherit_neg_02_multiple_supers_rejected() {
    assert!(expect_compile_error(
        r#"
interface A { fn a(self) -> i64; }
interface B { fn b(self) -> i64; }
interface C extends A, B { fn c(self) -> i64; }
fn main() {}
"#,
    ));
}

#[test]
fn iface_inherit_neg_03_extends_class_rejected() {
    assert!(expect_compile_error(
        r#"
class Foo { pub fn f(self) -> i64 { return 1; } }
interface Bad extends Foo { fn g(self) -> i64; }
fn main() {}
"#,
    ));
}

#[test]
fn iface_inherit_neg_04_extends_unknown_rejected() {
    assert!(expect_compile_error(
        r#"
interface Bad extends Nope { fn g(self) -> i64; }
fn main() {}
"#,
    ));
}

#[test]
fn downcast_05_generic_interface_scrutinee() {
    // Downcast works when the scrutinee is a generic interface instantiation
    // (willow-1js.9).
    let (out, ok) = compile_and_run(
        r#"
interface Box<T> { fn get(self) -> T; }
class IntBox implements Box<i64> {
    pub fn get(self) -> i64 { return 7; }
    pub fn extra(self) -> i64 { return 99; }
}
class OtherBox implements Box<i64> { pub fn get(self) -> i64 { return 1; } }
fn probe(b: Box<i64>) -> i64 {
    return match b {
        IntBox(x) => x.extra(),
        _ => b.get(),
    };
}
fn main() {
    println(probe(new IntBox()));
    println(probe(new OtherBox()));
}
"#,
    );
    assert!(ok, "{out}");
    assert_eq!(out, "99\n1\n");
}

// ── Subclass usable as a base-declared interface (willow-2s4i) ───────────────

#[test]
fn subclass_iface_01_used_as_base_interface() {
    // Puppy extends Dog (which implements Animal); a Puppy is an Animal even
    // though Puppy does not re-declare `implements Animal`.
    let (out, ok) = compile_and_run_gc_stress(
        r#"
interface Animal { fn name(self) -> String; }
open class Dog implements Animal { pub open fn name(self) -> String { return "dog"; } }
class Puppy extends Dog { pub override fn name(self) -> String { return "puppy"; } }
fn describe(a: Animal) { println(a.name()); }
fn main() {
    describe(new Dog());
    describe(new Puppy());
}
"#,
    );
    assert!(ok, "subclass must be usable as the base's interface: {out}");
    assert_eq!(out, "dog\npuppy\n");
}

#[test]
fn subclass_iface_02_inherits_method_no_override() {
    // The subclass inherits the base's interface method (no override).
    let (out, ok) = compile_and_run(
        r#"
interface Animal { fn legs(self) -> i64; }
open class Dog implements Animal { pub fn legs(self) -> i64 { return 4; } }
class Puppy extends Dog {}
fn count(a: Animal) -> i64 { return a.legs(); }
fn main() { println(count(new Puppy())); }
"#,
    );
    assert!(ok, "{out}");
    assert_eq!(out, "4\n");
}

#[test]
fn subclass_iface_03_two_levels() {
    // Grandchild is usable as the interface declared two levels up.
    let (out, ok) = compile_and_run(
        r#"
interface Animal { fn name(self) -> String; }
open class Dog implements Animal { pub open fn name(self) -> String { return "dog"; } }
open class Puppy extends Dog { pub open override fn name(self) -> String { return "puppy"; } }
class Teacup extends Puppy { pub override fn name(self) -> String { return "teacup"; } }
fn describe(a: Animal) { println(a.name()); }
fn main() { describe(new Teacup()); }
"#,
    );
    assert!(ok, "{out}");
    assert_eq!(out, "teacup\n");
}

// ── Virtual dispatch for overridden methods (willow-ftk) ─────────────────────

#[test]
fn virtual_dispatch_01_override_via_base_ref() {
    let (out, ok) = compile_and_run(
        r#"
open class Animal { pub open fn sound(self) -> String { return "..."; } }
class Dog extends Animal { pub override fn sound(self) -> String { return "woof"; } }
class Cat extends Animal { pub override fn sound(self) -> String { return "meow"; } }
fn speak(a: Animal) { println(a.sound()); }
fn main() { speak(new Dog()); speak(new Cat()); }
"#,
    );
    assert!(ok, "{out}");
    assert_eq!(out, "woof\nmeow\n");
}

#[test]
fn virtual_dispatch_02_base_method_calls_overridden_self() {
    // An inherited base method that calls self.m() dispatches to the override.
    let (out, ok) = compile_and_run(
        r#"
open class Animal {
    pub open fn sound(self) -> String { return "..."; }
    pub fn describe(self) -> String { return "I say " + self.sound(); }
}
class Dog extends Animal { pub override fn sound(self) -> String { return "woof"; } }
fn main() { println(new Dog().describe()); }
"#,
    );
    assert!(ok, "{out}");
    assert_eq!(out, "I say woof\n");
}

#[test]
fn virtual_dispatch_03_inherited_non_override_dispatches_to_base() {
    // A subclass that does NOT override must dispatch to the inherited base
    // implementation (regression for the fall-through bug, willow-ftk).
    let (out, ok) = compile_and_run(
        r#"
open class Animal { pub open fn sound(self) -> String { return "base"; } }
class Mute extends Animal {}
fn speak(a: Animal) { println(a.sound()); }
fn main() { speak(new Mute()); }
"#,
    );
    assert!(ok, "{out}");
    assert_eq!(out, "base\n");
}

#[test]
fn virtual_dispatch_04_three_levels_mixed_gc_stress() {
    let (out, ok) = compile_and_run_gc_stress(
        r#"
import std::collections::Array;
open class A {
    pub open fn kind(self) -> String { return "A"; }
    pub fn tag(self) -> String { return "[" + self.kind() + "]"; }
}
open class B extends A { pub open override fn kind(self) -> String { return "B"; } }
class C extends B { pub override fn kind(self) -> String { return "C"; } }
class D extends A {}
fn main() {
    let xs: Array<A> = [new A(), new B(), new C(), new D()];
    let mut i = 0;
    while i < xs.len() {
        println(xs[i].tag());
        i = i + 1;
    }
}
"#,
    );
    assert!(ok, "{out}");
    assert_eq!(out, "[A]\n[B]\n[C]\n[A]\n");
}

// ── ? error conversion: virtual Into dispatch on subclassed errors (bpk6) ────

#[test]
fn try_convert_11_subclassed_error_uses_override() {
    // A Result<_, BaseErr> holding a SpecificErr (override of into) must convert
    // via the override when propagated with `?` (willow-bpk6).
    let (out, ok) = compile_and_run(
        r#"
class AppErr { pub code: i64; }
open class BaseErr implements Into<AppErr> {
    pub open fn into(self) -> AppErr { return new AppErr(1); }
}
class SpecificErr extends BaseErr {
    pub override fn into(self) -> AppErr { return new AppErr(99); }
}
fn fails() -> Result<i64, BaseErr> {
    let e: BaseErr = new SpecificErr();
    return Result::Err(e);
}
fn run() -> Result<i64, AppErr> { let v = fails()?; return Result::Ok(v); }
fn main() {
    let out = match run() {
        Result::Ok(v) => v,
        Result::Err(e) => e.code,
    };
    println(out);
}
"#,
    );
    assert!(ok, "{out}");
    assert_eq!(out, "99\n");
}

#[test]
fn try_convert_12_base_error_uses_base_into() {
    // The same hierarchy: a plain BaseErr converts via BaseErr::into.
    let (out, ok) = compile_and_run(
        r#"
class AppErr { pub code: i64; }
open class BaseErr implements Into<AppErr> {
    pub open fn into(self) -> AppErr { return new AppErr(1); }
}
class SpecificErr extends BaseErr {
    pub override fn into(self) -> AppErr { return new AppErr(99); }
}
fn fails() -> Result<i64, BaseErr> { return Result::Err(new BaseErr()); }
fn run() -> Result<i64, AppErr> { let v = fails()?; return Result::Ok(v); }
fn main() {
    let out = match run() {
        Result::Ok(v) => v,
        Result::Err(e) => e.code,
    };
    println(out);
}
"#,
    );
    assert!(ok, "{out}");
    assert_eq!(out, "1\n");
}

#[test]
fn subclass_iface_04_inherits_generic_interface_with_args() {
    // A subclass inherits a generic interface (Into<AppErr>) from its base with
    // type args preserved (regression for the name-only propagation bug).
    let (out, ok) = compile_and_run(
        r#"
class AppErr { pub code: i64; }
open class BaseErr implements Into<AppErr> {
    pub open fn into(self) -> AppErr { return new AppErr(7); }
}
class SubErr extends BaseErr {}
fn convert(e: Into<AppErr>) -> i64 { return e.into().code; }
fn main() { println(convert(new SubErr())); }
"#,
    );
    assert!(ok, "{out}");
    assert_eq!(out, "7\n");
}

// ── Cooperative async suspension (willow-lpn.5.3 Stage 2) ────────────────────

#[test]
fn coop_async_01_main_suspends_at_sleep() {
    // An eligible `async fn main` lowers to a suspending poll-fn state machine
    // driven by the scheduler; output is produced across the await points.
    let (out, ok) = compile_and_run(
        r#"
async fn main() {
    println(1);
    await sleep(1);
    println(2);
    await sleep(1);
    println(3);
}
"#,
    );
    assert!(ok, "{out}");
    assert_eq!(out, "1\n2\n3\n");
}

#[test]
fn coop_async_02_no_await_before_first_output() {
    let (out, ok) = compile_and_run(
        r#"
async fn main() {
    await sleep(1);
    println(42);
}
"#,
    );
    assert!(ok, "{out}");
    assert_eq!(out, "42\n");
}

#[test]
fn coop_async_03_gc_stress() {
    let (out, ok) = compile_and_run_gc_stress(
        r#"
async fn main() {
    println(1);
    await sleep(1);
    println(2);
}
"#,
    );
    assert!(ok, "{out}");
    assert_eq!(out, "1\n2\n");
}

#[test]
fn coop_async_04_gc_locals_across_awaits() {
    // GC-managed locals declared before an await and used after must survive
    // suspension (frame-backed). Run under GC stress (willow-lpn.5.3 slice 3).
    let (out, ok) = compile_and_run_gc_stress(
        r#"
async fn main() {
    let s = "hello";
    await sleep(1);
    println(s);
    let t = s + " world";
    await sleep(1);
    println(t);
}
"#,
    );
    assert!(ok, "{out}");
    assert_eq!(out, "hello\nhello world\n");
}

#[test]
fn coop_async_05_non_gc_locals_across_awaits() {
    // i64/scalar locals across awaits are frame-backed too (not just GC), and
    // are not GC-traced (willow-lpn.5.3 slice 3b). GC-stress.
    let (out, ok) = compile_and_run_gc_stress(
        r#"
async fn main() {
    let n = 10;
    let s = "v=";
    await sleep(1);
    let m = n + 5;
    println(s);
    println(m);
    await sleep(1);
    println(n);
}
"#,
    );
    assert!(ok, "{out}");
    assert_eq!(out, "v=\n15\n10\n");
}

#[test]
fn coop_async_06_await_cooperative_leaf() {
    // A no-param leaf async fn (sleep + return) compiles to a cooperative
    // constructor + poll fn; `await f()` block-runs the scheduler and reads the
    // result (willow-lpn.5.3 slice 4). GC-stress.
    let (out, ok) = compile_and_run_gc_stress(
        r#"
async fn wait_value() -> i64 {
    await sleep(1);
    return 42;
}
async fn compute() -> i64 {
    await sleep(1);
    return 7;
}
async fn main() {
    let x = await wait_value();
    println(x);
    let y = await compute();
    println(y + 1);
}
"#,
    );
    assert!(ok, "{out}");
    assert_eq!(out, "42\n8\n");
}

#[test]
fn coop_async_06b_eager_await_roots_leaf_frame_until_result_load() {
    // `println(await f())` is intentionally not eligible for the cooperative
    // awaiter lowering, so it exercises the eager emit_await() path. That path
    // must keep the completed leaf frame rooted while willow_sched_run() removes
    // the task runtime root and before frame[RESULT] is loaded.
    let (out, ok) = compile_and_run_gc_stress(
        r#"
async fn make_text() -> String {
    await sleep(1);
    return "root" + "ed";
}
async fn make_number() -> i64 {
    await sleep(1);
    return 42;
}
async fn main() {
    println(await make_text());
    println(await make_number());
}
"#,
    );
    assert!(ok, "{out}");
    assert_eq!(out, "rooted\n42\n");
}

#[test]
fn coop_async_06c_eager_await_survives_await_stress() {
    let (out, ok) = compile_and_run_gc_stress_mode(
        r#"
async fn make_text() -> String {
    await sleep(1);
    return "await" + "-stress";
}
async fn main() {
    println(await make_text());
}
"#,
        "await",
    );
    assert!(ok, "{out}");
    assert_eq!(out, "await-stress\n");
}

#[test]
fn coop_async_07_cooperative_leaf_with_params() {
    // A leaf async fn with by-value params (GC + scalar) compiles to a
    // cooperative constructor that stores args into frame slots; the poll fn
    // reads them back across the suspension (willow-lpn.5.3 slice 4b). GC-stress.
    let (out, ok) = compile_and_run_gc_stress(
        r#"
async fn greet(name: String, n: i64) -> String {
    await sleep(1);
    return "hi " + name;
}
async fn add(a: i64, b: i64) -> i64 {
    await sleep(1);
    return a + b;
}
async fn main() {
    let g = await greet("willow", 3);
    println(g);
    let s = await add(40, 2);
    println(s);
}
"#,
    );
    assert!(ok, "{out}");
    assert_eq!(out, "hi willow\n42\n");
}

#[test]
fn coop_async_08_cooperative_leaf_with_locals() {
    // A cooperative leaf may declare locals (GC + scalar) that survive its own
    // suspensions, frame-backed after the param slots (willow-lpn.5.3 4c).
    let (out, ok) = compile_and_run_gc_stress(
        r#"
async fn calc(base: i64) -> i64 {
    let a = base + 1;
    let label = "result";
    await sleep(1);
    let b = a * 2;
    await sleep(1);
    println(label);
    return b + base;
}
async fn main() {
    let r = await calc(10);
    println(r);
}
"#,
    );
    assert!(ok, "{out}");
    assert_eq!(out, "result\n32\n");
}

#[test]
fn coop_async_09_await_inside_if_and_while_in_main() {
    // Slice 5: structured control flow in the cooperative main poll fn, including
    // a loop back-edge and branch-local suspend points.
    let (out, ok) = compile_and_run_gc_stress(
        r#"
async fn main() {
    let mut i = 0;
    while i < 3 {
        if i == 1 {
            await sleep(1);
            println(10);
        } else {
            await sleep(1);
            println(i);
        }
        i = i + 1;
    }
    println(99);
}
"#,
    );
    assert!(ok, "{out}");
    assert_eq!(out, "0\n10\n2\n99\n");
}

#[test]
fn coop_async_10_await_inside_leaf_if_else_returns() {
    // Slice 5 regression: both branches can suspend and then return from a
    // cooperative leaf poll fn.
    let (out, ok) = compile_and_run_gc_stress(
        r#"
async fn pick(flag: bool) -> i64 {
    if flag {
        await sleep(1);
        return 10;
    } else {
        await sleep(1);
        await sleep(1);
        return 20;
    }
}
async fn main() {
    let a = await pick(true);
    println(a);
    let b = await pick(false);
    println(b);
}
"#,
    );
    assert!(ok, "{out}");
    assert_eq!(out, "10\n20\n");
}

#[test]
fn coop_async_11_await_inside_for_loop_in_main() {
    let (out, ok) = compile_and_run_gc_stress(
        r#"
import std::collections::Array;

async fn main() {
    let xs: Array<i64> = [1, 2, 3];
    for x in xs {
        await sleep(1);
        println(x);
    }
    println(99);
}
"#,
    );
    assert!(ok, "{out}");
    assert_eq!(out, "1\n2\n3\n99\n");
}

#[test]
fn coop_async_12_await_inside_for_loop_in_leaf() {
    let (out, ok) = compile_and_run_gc_stress(
        r#"
import std::collections::Array;

async fn sum(values: Array<i64>) -> i64 {
    let mut total = 0;
    for value in values {
        await sleep(1);
        total = total + value;
    }
    return total;
}

async fn main() {
    let values: Array<i64> = [4, 5, 6];
    let total = await sum(values);
    println(total);
}
"#,
    );
    assert!(ok, "{out}");
    assert_eq!(out, "15\n");
}

// ----------------------------------------------------------------------------
// Range<i64> as a first-class value (willow: range-value feature).
// 20 perspectives on materializing, reading, passing, returning, and iterating
// a `Range<i64>` held as a value rather than only as an inline `for` iterable.
// ----------------------------------------------------------------------------

// P1: `let r = a..b` materializes a value; P2: `.start`; P3: `.end`.
#[test]
fn range_value_p01_let_and_fields() {
    let (out, ok) =
        compile_and_run("fn main() { let r = 4..9; println(r.start); println(r.end); }");
    assert!(ok, "{out}");
    assert_eq!(out, "4\n9\n");
}

// P4: a function may return `Range<i64>`; P5: and accept it as a parameter.
#[test]
fn range_value_p02_return_and_param() {
    let (out, ok) = compile_and_run(
        r#"
fn make() -> Range<i64> { return 3..8; }
fn width(r: Range<i64>) -> i64 { return r.end - r.start; }
fn main() {
    let r = make();
    println(r.start);
    println(width(r));
}
"#,
    );
    assert!(ok, "{out}");
    assert_eq!(out, "3\n5\n");
}

// P6: `for x in <range variable>` iterates the stored bounds.
#[test]
fn range_value_p03_for_over_variable() {
    let (out, ok) = compile_and_run("fn main() { let r = 1..4; for x in r { println(x); } }");
    assert!(ok, "{out}");
    assert_eq!(out, "1\n2\n3\n");
}

// P7: bounds may be arbitrary i64 expressions (not just literals).
#[test]
fn range_value_p04_expression_bounds() {
    let (out, ok) = compile_and_run(
        r#"
fn main() {
    let a = 2;
    let b = a + 3;
    let r = (a - 1)..(b * 2);
    println(r.start);
    println(r.end);
}
"#,
    );
    assert!(ok, "{out}");
    assert_eq!(out, "1\n10\n");
}

// P8: an empty range (start == end) yields no iterations; fields still correct.
#[test]
fn range_value_p05_empty_range() {
    let (out, ok) = compile_and_run(
        r#"
fn main() {
    let r = 5..5;
    let mut n = 0;
    for _ in r { n = n + 1; }
    println(n);
    println(r.end - r.start);
}
"#,
    );
    assert!(ok, "{out}");
    assert_eq!(out, "0\n0\n");
}

// P9: a reversed range (start > end) yields no iterations.
#[test]
fn range_value_p06_reversed_range_no_iterations() {
    let (out, ok) = compile_and_run(
        r#"
fn main() {
    let r = 7..3;
    let mut n = 0;
    for _ in r { n = n + 1; }
    println(n);
}
"#,
    );
    assert!(ok, "{out}");
    assert_eq!(out, "0\n");
}

// P10: negative bounds; P11: summing a range variable.
#[test]
fn range_value_p07_negative_bounds_sum() {
    let (out, ok) = compile_and_run(
        r#"
fn main() {
    let r = -2..3;
    let mut total = 0;
    for x in r { total = total + x; }
    println(total);
    println(r.start);
}
"#,
    );
    assert!(ok, "{out}");
    // -2 + -1 + 0 + 1 + 2 = 0
    assert_eq!(out, "0\n-2\n");
}

// P12: multiple range values coexist independently.
#[test]
fn range_value_p08_multiple_ranges() {
    let (out, ok) = compile_and_run(
        r#"
fn main() {
    let a = 0..2;
    let b = 10..13;
    println(a.end);
    println(b.start);
    for x in a { println(x); }
    for y in b { println(y); }
}
"#,
    );
    assert!(ok, "{out}");
    assert_eq!(out, "2\n10\n0\n1\n10\n11\n12\n");
}

// P13: range value survives GC stress (heap object is rooted).
#[test]
fn range_value_p09_gc_stress() {
    let (out, ok) = compile_and_run_gc_stress(
        r#"
fn main() {
    let r = 2..6;
    let s = "keepalive";
    let mut total = 0;
    for x in r { total = total + x; }
    println(s);
    println(total);
    println(r.start);
}
"#,
    );
    assert!(ok, "{out}");
    // 2+3+4+5 = 14
    assert_eq!(out, "keepalive\n14\n2\n");
}

// P14: iterate directly over a range returned by a call.
#[test]
fn range_value_p10_for_over_call_result() {
    let (out, ok) = compile_and_run(
        r#"
fn upto(n: i64) -> Range<i64> { return 0..n; }
fn main() { for x in upto(3) { println(x); } }
"#,
    );
    assert!(ok, "{out}");
    assert_eq!(out, "0\n1\n2\n");
}

// P15: a `mut` range may be reassigned to another range value.
#[test]
fn range_value_p11_mut_reassign() {
    let (out, ok) = compile_and_run(
        r#"
fn main() {
    let mut r = 0..1;
    r = 5..8;
    println(r.start);
    println(r.end);
    for x in r { println(x); }
}
"#,
    );
    assert!(ok, "{out}");
    assert_eq!(out, "5\n8\n5\n6\n7\n");
}

// P16: range fields participate in conditions/arithmetic.
#[test]
fn range_value_p12_field_in_condition() {
    let (out, ok) = compile_and_run(
        r#"
fn main() {
    let r = 4..10;
    if r.end > r.start {
        println(r.end - r.start);
    } else {
        println(0);
    }
}
"#,
    );
    assert!(ok, "{out}");
    assert_eq!(out, "6\n");
}

// P17: a range literal `for` loop still works (no regression).
#[test]
fn range_value_p13_literal_for_loop_regression() {
    let (out, ok) =
        compile_and_run("fn main() { let mut t = 0; for x in 1..5 { t = t + x; } println(t); }");
    assert!(ok, "{out}");
    assert_eq!(out, "10\n");
}

// P18: range value lives in an async frame across an await; fields read after.
#[test]
fn range_value_p14_async_frame_across_await() {
    let (out, ok) = compile_and_run_gc_stress(
        r#"
async fn compute() -> i64 {
    let r = 3..7;
    await sleep(1);
    return r.start + r.end;
}
async fn main() {
    let v = await compute();
    println(v);
}
"#,
    );
    assert!(ok, "{out}");
    assert_eq!(out, "10\n");
}

// P19: cooperative `for` over a range variable with an await in the body.
#[test]
fn range_value_p15_cooperative_for_over_variable() {
    let (out, ok) = compile_and_run_gc_stress(
        r#"
async fn run() -> i64 {
    let r = 1..4;
    let mut total = 0;
    for x in r {
        await sleep(1);
        total = total + x;
    }
    return total;
}
async fn main() {
    let t = await run();
    println(t);
}
"#,
    );
    assert!(ok, "{out}");
    assert_eq!(out, "6\n");
}

// P20: range bounds must be `i64` (float bound is a diagnostic).
#[test]
fn range_value_p16_non_i64_bound_is_error() {
    assert_compile_error_contains(
        "fn main() { let r = 0.0..5; println(r.start); }",
        &["error[E0201]", "range bounds must be `i64`"],
    );
}

// P21: accessing an unknown range field is a diagnostic.
#[test]
fn range_value_p17_unknown_field_is_error() {
    assert_compile_error_contains(
        "fn main() { let r = 0..5; println(r.middle); }",
        &["error[E0201]", "has no field `middle`"],
    );
}

// ----------------------------------------------------------------------------
// Cooperative spawn/join (willow: spawn migrated off OS threads onto the
// single-threaded cooperative scheduler). `spawn` queues a lightweight task;
// `join()` (and channel `recv()`) drive the scheduler until it completes.
// ----------------------------------------------------------------------------

// Spawn/join returns each task's result, regardless of join order.
#[test]
fn coop_spawn_01_join_order_independent() {
    let (out, ok) = compile_and_run(
        r#"
async fn sq(x: i64) -> i64 { return x * x; }
fn main() {
    let a = sq(2);
    let b = sq(3);
    let c = sq(4);
    println(c.join());
    println(a.join());
    println(b.join());
}
"#,
    );
    assert!(ok, "{out}");
    assert_eq!(out, "16\n4\n9\n");
}

// Many lightweight tasks: spawning a lot is cheap (no OS thread per spawn).
#[test]
fn coop_spawn_02_many_tasks() {
    let (out, ok) = compile_and_run(
        r#"
async fn id(x: i64) -> i64 { return x; }
fn main() {
    let a = id(1);
    let b = id(2);
    let c = id(3);
    let d = id(4);
    let e = id(5);
    let f = id(6);
    let g = id(7);
    let h = id(8);
    let total = a.join() + b.join() + c.join() + d.join()
        + e.join() + f.join() + g.join() + h.join();
    println(total);
}
"#,
    );
    assert!(ok, "{out}");
    assert_eq!(out, "36\n");
}

// A spawned producer is driven by the consumer's `recv()` (cooperative, no
// cross-thread deadlock).
#[test]
fn coop_spawn_03_channel_producer_consumer() {
    let (out, ok) = compile_and_run(
        r#"
async fn producer(ch: Channel<i64>) {
    ch.send(1);
    ch.send(2);
    ch.send(3);
    ch.close();
}
fn main() {
    let ch = Channel<i64>::new();
    let h = producer(ch);
    println(ch.recv());
    println(ch.recv());
    println(ch.recv());
    h.join();
}
"#,
    );
    assert!(ok, "{out}");
    assert_eq!(out, "1\n2\n3\n");
}

// Spawn with GC-managed args (object + string), result read via join, under
// GC stress: the frame roots the args and traces the result slot.
#[test]
fn coop_spawn_04_gc_args_and_result() {
    let (out, ok) = compile_and_run_gc_stress(
        r#"
class Box { v: i64; pub static fn new(v: i64) -> Box { return new Box(v); } pub fn get(self) -> i64 { return self.v; } }
async fn label(b: Box, name: String) -> String {
    return name;
}
async fn value(b: Box) -> i64 {
    return b.get();
}
fn main() {
    let b = Box::new(7);
    let h1 = label(b, "tag");
    let h2 = value(b);
    println(h1.join());
    println(h2.join());
}
"#,
    );
    assert!(ok, "{out}");
    assert_eq!(out, "tag\n7\n");
}

// A non-i64 (bool) spawn result round-trips through the frame result slot.
#[test]
fn coop_spawn_05_bool_result() {
    let (out, ok) = compile_and_run(
        r#"
async fn positive(x: i64) -> bool { return x > 0; }
fn main() {
    let a = positive(5);
    let b = positive(-5);
    println(a.join());
    println(b.join());
}
"#,
    );
    assert!(ok, "{out}");
    assert_eq!(out, "true\nfalse\n");
}

// Slice 5: awaits inside if/else and while are lowered by the CFG-based
// cooperative state machine (willow-lpn.5.3 / willow-8fh3 regression).
#[test]
fn coop_async_09_await_in_if_else_both_return() {
    let (out, ok) = compile_and_run_gc_stress(
        r#"
async fn pick(flag: bool) -> i64 {
    if flag {
        await sleep(1);
        return 10;
    } else {
        await sleep(1);
        await sleep(1);
        return 20;
    }
}
async fn main() {
    println(await pick(true));
    println(await pick(false));
}
"#,
    );
    assert!(ok, "{out}");
    assert_eq!(out, "10\n20\n");
}

#[test]
fn coop_async_10_await_in_if_else_join() {
    // Both arms fall through to a shared join, carrying a frame-backed local.
    let (out, ok) = compile_and_run_gc_stress(
        r#"
async fn run(flag: bool) -> i64 {
    let mut r = 0;
    if flag {
        await sleep(1);
        r = 10;
    } else {
        await sleep(1);
        r = 20;
    }
    await sleep(1);
    return r + 1;
}
async fn main() {
    println(await run(true));
    println(await run(false));
}
"#,
    );
    assert!(ok, "{out}");
    assert_eq!(out, "11\n21\n");
}

#[test]
fn coop_async_11_await_in_while() {
    let (out, ok) = compile_and_run_gc_stress(
        r#"
async fn sum(n: i64) -> i64 {
    let mut total = 0;
    let mut i = 0;
    while i < n {
        await sleep(1);
        total = total + i;
        i = i + 1;
    }
    return total;
}
async fn main() { println(await sum(4)); }
"#,
    );
    assert!(ok, "{out}");
    assert_eq!(out, "6\n");
}

#[test]
fn coop_async_12_await_in_if_inside_while() {
    let (out, ok) = compile_and_run_gc_stress(
        r#"
async fn run(n: i64) -> i64 {
    let mut total = 0;
    let mut i = 0;
    while i < n {
        if i == 1 {
            await sleep(1);
            total = total + 100;
        } else {
            await sleep(1);
            total = total + i;
        }
        i = i + 1;
    }
    return total;
}
async fn main() { println(await run(3)); }
"#,
    );
    assert!(ok, "{out}");
    // i=0: +0, i=1: +100, i=2: +2 => 102
    assert_eq!(out, "102\n");
}

#[test]
fn coop_async_13_gc_string_built_across_while_awaits() {
    let (out, ok) = compile_and_run_gc_stress(
        r#"
async fn build(n: i64) -> String {
    let mut s = "";
    let mut i = 0;
    while i < n {
        await sleep(1);
        s = s + "x";
        i = i + 1;
    }
    return s;
}
async fn main() { println(await build(3)); }
"#,
    );
    assert!(ok, "{out}");
    assert_eq!(out, "xxx\n");
}

// ----------------------------------------------------------------------------
// Async-GC stress suite (willow-lpn.5.5): GC-safety of the cooperative state
// machine — collection before await, after await, GC objects/strings carried
// across awaits, and JoinHandle keeping a GC result alive. All under
// WILLOW_GC_STRESS=alloc (collect at every allocation) plus explicit gc_collect.
// ----------------------------------------------------------------------------

// 16.1: collection BEFORE an await — a frame-backed GC local survives.
#[test]
fn coop_gc_01_collect_before_await() {
    let (out, ok) = compile_and_run_gc_stress(
        r#"
async fn run() -> String {
    let s = "kept";
    gc_collect();
    await sleep(1);
    return s;
}
async fn main() { println(await run()); }
"#,
    );
    assert!(ok, "{out}");
    assert_eq!(out, "kept\n");
}

// 16.2: collection AFTER an await — the local declared before the await survives.
#[test]
fn coop_gc_02_collect_after_await() {
    let (out, ok) = compile_and_run_gc_stress(
        r#"
async fn run() -> String {
    let s = "kept";
    await sleep(1);
    gc_collect();
    return s;
}
async fn main() { println(await run()); }
"#,
    );
    assert!(ok, "{out}");
    assert_eq!(out, "kept\n");
}

// GC object (class instance) carried across an await with collections on both
// sides; field access after the await reads the live object.
#[test]
fn coop_gc_03_object_across_await() {
    let (out, ok) = compile_and_run_gc_stress(
        r#"
class Box { v: i64; pub static fn new(v: i64) -> Box { return new Box(v); } pub fn get(self) -> i64 { return self.v; } }
async fn run() -> i64 {
    let b = Box::new(42);
    gc_collect();
    await sleep(1);
    gc_collect();
    return b.get();
}
async fn main() { println(await run()); }
"#,
    );
    assert!(ok, "{out}");
    assert_eq!(out, "42\n");
}

// 16.9: a JoinHandle keeps the spawned task's GC result alive across a collection
// performed before join().
#[test]
fn coop_gc_04_joinhandle_keeps_result_alive() {
    let (out, ok) = compile_and_run_gc_stress(
        r#"
async fn tag(n: i64) -> String { return "tag"; }
async fn main() {
    let h = tag(7);
    gc_collect();
    gc_collect();
    println(h.join());
}
"#,
    );
    assert!(ok, "{out}");
    assert_eq!(out, "tag\n");
}

// Combined stress: many awaits in a loop, each iteration allocates (string
// concat) and collects, while the accumulator local survives every collection.
#[test]
fn coop_gc_05_combined_stress_loop() {
    let (out, ok) = compile_and_run_gc_stress(
        r#"
async fn build(n: i64) -> String {
    let mut s = "";
    let mut i = 0;
    while i < n {
        await sleep(1);
        s = s + "ab";
        gc_collect();
        i = i + 1;
    }
    return s;
}
async fn main() { println(await build(4)); }
"#,
    );
    assert!(ok, "{out}");
    assert_eq!(out, "abababab\n");
}

// spawn of a cooperative-leaf ASYNC fn: join() must return the async function's
// REAL result, not the constructor's frame pointer (willow-lpn.5.4 fix).
#[test]
fn coop_spawn_06_spawn_async_leaf_sync_main() {
    let (out, ok) = compile_and_run(
        r#"
async fn work(x: i64) -> i64 {
    await sleep(1);
    return x + 1;
}
fn main() {
    let h = work(41);
    println(h.join());
}
"#,
    );
    assert!(ok, "{out}");
    assert_eq!(out, "42\n");
}

#[test]
fn coop_spawn_07_spawn_async_leaf_multiple_gc() {
    // Multiple spawned async leaves (i64 + String results) joined; under GC
    // stress to exercise frame/result tracing.
    let (out, ok) = compile_and_run_gc_stress(
        r#"
async fn add(a: i64, b: i64) -> i64 {
    await sleep(1);
    return a + b;
}
async fn tag(name: String) -> String {
    await sleep(1);
    return "hi " + name;
}
async fn main() {
    let h1 = add(40, 2);
    let h2 = add(10, 5);
    let h3 = tag("willow");
    println(h1.join());
    println(h2.join());
    println(h3.join());
}
"#,
    );
    assert!(ok, "{out}");
    assert_eq!(out, "42\n15\nhi willow\n");
}

#[test]
fn coop_spawn_08_spawn_async_leaf_runs_to_completion() {
    // The spawned leaf actually runs (side effects observed) and join returns
    // its real result; spawn does not block (the println(2) happens first).
    let (out, ok) = compile_and_run(
        r#"
async fn work(x: i64) -> i64 {
    println(100);
    await sleep(1);
    println(200);
    return x;
}
fn main() {
    println(1);
    let h = work(42);
    println(2);
    let r = h.join();
    println(3);
    println(r);
}
"#,
    );
    assert!(ok, "{out}");
    assert_eq!(out, "1\n2\n100\n200\n3\n42\n");
}

// Cooperative concurrency: spawned async-leaf tasks suspend independently at
// their awaits and the single-threaded scheduler interleaves them — observably
// distinct from sequential execution (willow-lpn.5.4).
#[test]
fn coop_concurrent_01_two_workers_interleave() {
    let (out, ok) = compile_and_run(
        r#"
async fn worker(id: i64) -> i64 {
    println(id);
    await sleep(1);
    println(id + 100);
    return id;
}
fn main() {
    let a = worker(1);
    let b = worker(2);
    println(a.join() + b.join());
}
"#,
    );
    assert!(ok, "{out}");
    // Interleaved: both print id, both sleep, both resume, then the sum.
    assert_eq!(out, "1\n2\n101\n102\n3\n");
}

#[test]
fn coop_yield_01_main_resumes_without_timer() {
    let (out, ok) = compile_and_run(
        r#"
async fn main() {
    println(1);
    await yield();
    println(2);
}
"#,
    );
    assert!(ok, "{out}");
    assert_eq!(out, "1\n2\n");
}

#[test]
fn coop_yield_02_spawned_workers_interleave() {
    let (out, ok) = compile_and_run(
        r#"
async fn worker(id: i64) -> i64 {
    println(id);
    await yield();
    println(id + 10);
    return id;
}
fn main() {
    let a = worker(1);
    let b = worker(2);
    println(a.join() + b.join());
}
"#,
    );
    assert!(ok, "{out}");
    assert_eq!(out, "1\n2\n11\n12\n3\n");
}

#[test]
fn coop_yield_03_gc_string_survives_yield() {
    let (out, ok) = compile_and_run_gc_stress_all(
        r#"
async fn keep(text: String) -> String {
    let held = text + "!";
    gc_collect();
    await yield();
    gc_collect();
    return held + "?";
}
fn main() {
    let task = keep("yield");
    gc_collect();
    println(task.join());
}
"#,
    );
    assert!(ok, "{out}");
    assert_eq!(out, "yield!?\n");
}

#[test]
fn coop_concurrent_02_three_workers_interleave_gc() {
    let (out, ok) = compile_and_run_gc_stress(
        r#"
async fn worker(id: i64) -> i64 {
    println(id);
    await sleep(1);
    println(id * 10);
    return id;
}
async fn main() {
    let a = worker(1);
    let b = worker(2);
    let c = worker(3);
    println(a.join() + b.join() + c.join());
}
"#,
    );
    assert!(ok, "{out}");
    assert_eq!(out, "1\n2\n3\n10\n20\n30\n6\n");
}

#[test]
fn coop_concurrent_03_spawn_then_await_in_main() {
    // An eager main spawns a background worker, then `await f()` block-drives the
    // scheduler — the background worker interleaves during that await.
    let (out, ok) = compile_and_run(
        r#"
async fn bg() -> i64 {
    println(7);
    await sleep(1);
    println(8);
    return 0;
}
async fn f() -> i64 {
    await sleep(1);
    return 42;
}
async fn main() {
    let h = bg();
    let x = await f();
    println(x);
    h.join();
}
"#,
    );
    assert!(ok, "{out}");
    assert_eq!(out, "7\n8\n42\n");
}

// ----------------------------------------------------------------------------
// Cooperative awaiter-suspend model (willow-lpn.5.3.1): a `let x = await f()` /
// `await f()` of a cooperative leaf SUSPENDS the awaiter via willow_sched_await
// (dependency-wake) rather than block-on, so a fn that MIXES call-awaits and
// sleep-awaits is itself a cooperative task. The callee frame is held in a
// GC-traced awaiter slot across suspension.
// ----------------------------------------------------------------------------

// A spawned worker that mixes a call-await and a sleep-await joins its REAL
// result (previously returned a frame ptr / garbage).
#[test]
fn coop_await_01_mixed_call_and_sleep_await_spawned() {
    let (out, ok) = compile_and_run(
        r#"
async fn helper(x: i64) -> i64 {
    await sleep(1);
    return x * 10;
}
async fn worker(id: i64) -> i64 {
    println(id);
    let h = await helper(id);
    await sleep(1);
    println(h);
    return h + id;
}
fn main() {
    let a = worker(1);
    println(a.join());
}
"#,
    );
    assert!(ok, "{out}");
    assert_eq!(out, "1\n10\n11\n");
}

// Two mixed-await workers interleave (true concurrency WITH composition), GC.
#[test]
fn coop_await_02_mixed_workers_interleave_gc() {
    let (out, ok) = compile_and_run_gc_stress(
        r#"
async fn helper(x: i64) -> i64 {
    await sleep(1);
    return x * 10;
}
async fn worker(id: i64) -> i64 {
    println(id);
    let h = await helper(id);
    println(h);
    return h + id;
}
async fn main() {
    let a = worker(1);
    let b = worker(2);
    println(a.join() + b.join());
}
"#,
    );
    assert!(ok, "{out}");
    // Both print id (interleave at the call-await), both resume + print h, then sum.
    // Timer wake order can resume the two helpers in either order.
    assert!(
        matches!(out.as_str(), "1\n2\n10\n20\n33\n" | "1\n2\n20\n10\n33\n"),
        "{out}"
    );
}

// Sequential call-awaits chaining a GC (String) result through the awaiter
// frame, under GC stress.
#[test]
fn coop_await_03_sequential_string_call_awaits_gc() {
    let (out, ok) = compile_and_run_gc_stress(
        r#"
async fn step(s: String) -> String {
    await sleep(1);
    return s + "!";
}
async fn main() {
    let a = await step("a");
    let b = await step(a);
    let c = await step(b);
    println(c);
}
"#,
    );
    assert!(ok, "{out}");
    assert_eq!(out, "a!!!\n");
}

// A call-await result drives later control flow + arithmetic in the awaiter.
#[test]
fn coop_await_04_call_await_result_in_control_flow() {
    let (out, ok) = compile_and_run_gc_stress(
        r#"
async fn compute(x: i64) -> i64 {
    await sleep(1);
    return x + 5;
}
async fn main() {
    let v = await compute(10);
    if v > 12 {
        await sleep(1);
        println(v * 2);
    } else {
        println(0);
    }
}
"#,
    );
    assert!(ok, "{out}");
    assert_eq!(out, "30\n");
}

// A discarded call-await (`await f();` with no binding) still suspends + runs.
#[test]
fn coop_await_05_discarded_call_await() {
    let (out, ok) = compile_and_run(
        r#"
async fn tick(n: i64) -> i64 {
    await sleep(1);
    println(n);
    return n;
}
async fn main() {
    await tick(1);
    await tick(2);
    println(3);
}
"#,
    );
    assert!(ok, "{out}");
    assert_eq!(out, "1\n2\n3\n");
}

// A call-await can assign into an existing frame-backed local and then keep
// running after another suspension.
#[test]
fn coop_await_06_assignment_call_await() {
    let (out, ok) = compile_and_run_gc_stress(
        r#"
async fn next(n: i64) -> i64 {
    await sleep(1);
    return n + 1;
}
async fn worker() -> i64 {
    let mut total = 0;
    total = await next(10);
    await sleep(1);
    return total + 5;
}
async fn main() {
    println(await worker());
}
"#,
    );
    assert!(ok, "{out}");
    assert_eq!(out, "16\n");
}

// A cooperative leaf can return the result of a call-await directly.
#[test]
fn coop_await_07_return_call_await_chain_gc() {
    let (out, ok) = compile_and_run_gc_stress(
        r#"
async fn mark(s: String) -> String {
    await sleep(1);
    return s + "!";
}
async fn wrap(s: String) -> String {
    return await mark(s);
}
async fn main() {
    println(await wrap("ok"));
}
"#,
    );
    assert!(ok, "{out}");
    assert_eq!(out, "ok!\n");
}

// A call-await can assign a GC result into an object field, then survive another
// suspension before the field is read.
#[test]
fn coop_await_08_field_assignment_call_await_gc() {
    let (out, ok) = compile_and_run_gc_stress(
        r#"
class Holder {
    pub text: String;
}
async fn mark(s: String) -> String {
    await sleep(1);
    return s + "!";
}
async fn main() {
    let h = new Holder("seed");
    h.text = await mark("field");
    await sleep(1);
    println(h.text);
}
"#,
    );
    assert!(ok, "{out}");
    assert_eq!(out, "field!\n");
}

// A call-await can assign a GC result into an array element through the
// cooperative awaiter path.
#[test]
fn coop_await_09_index_assignment_call_await_gc() {
    let (out, ok) = compile_and_run_gc_stress(
        r#"
import std::collections::Array;

async fn mark(s: String) -> String {
    await sleep(1);
    return s + "!";
}
async fn main() {
    let mut xs: Array<String> = ["seed"];
    xs[0] = await mark("index");
    await sleep(1);
    println(xs[0]);
}
"#,
    );
    assert!(ok, "{out}");
    assert_eq!(out, "index!\n");
}

// ----------------------------------------------------------------------------
// Cooperative channels (willow-dsw): channel `recv` is a cooperative suspend
// point — an empty `recv` parks the consuming task as a channel waiter, and
// `send`/`close` wake it. This makes a recv-consumer a real cooperative task
// (spawn/join works) and lets producer/consumer tasks interleave correctly.
// ----------------------------------------------------------------------------

// Spawned producer + spawned consumer task; consumer's join returns its result.
#[test]
fn coop_chan_01_task_producer_consumer() {
    let (out, ok) = compile_and_run(
        r#"
async fn producer(ch: Channel<i64>) -> i64 {
    let mut i = 1;
    while i <= 3 {
        await sleep(1);
        ch.send(i * 10);
        i = i + 1;
    }
    ch.close();
    return 0;
}
async fn consumer(ch: Channel<i64>) -> i64 {
    let mut total = 0;
    let mut v = ch.recv();
    while v != 0 {
        println(v);
        total = total + v;
        v = ch.recv();
    }
    return total;
}
async fn main() {
    let ch = Channel<i64>::new();
    let p = producer(ch);
    let c = consumer(ch);
    println(c.join());
    p.join();
}
"#,
    );
    assert!(ok, "{out}");
    assert_eq!(out, "10\n20\n30\n60\n");
}

// Same, under GC stress (the channel value queue + frame slots survive).
#[test]
fn coop_chan_02_task_producer_consumer_gc() {
    let (out, ok) = compile_and_run_gc_stress(
        r#"
async fn producer(ch: Channel<i64>) -> i64 {
    let mut i = 1;
    while i <= 3 {
        await sleep(1);
        ch.send(i);
        i = i + 1;
    }
    ch.close();
    return 0;
}
async fn consumer(ch: Channel<i64>) -> i64 {
    let mut total = 0;
    let mut v = ch.recv();
    while v != 0 {
        total = total + v;
        v = ch.recv();
    }
    return total;
}
async fn main() {
    let ch = Channel<i64>::new();
    let p = producer(ch);
    let c = consumer(ch);
    println(c.join());
    p.join();
}
"#,
    );
    assert!(ok, "{out}");
    assert_eq!(out, "6\n");
}

// A consumer that recvs in a `let` binding (first value) then loops with assign.
#[test]
fn coop_chan_03_recv_let_and_assign() {
    let (out, ok) = compile_and_run(
        r#"
async fn producer(ch: Channel<i64>) -> i64 {
    await sleep(1);
    ch.send(7);
    ch.send(8);
    ch.close();
    return 0;
}
async fn main() {
    let ch = Channel<i64>::new();
    let p = producer(ch);
    let a = await consume_first(ch);
    println(a);
    p.join();
}
async fn consume_first(ch: Channel<i64>) -> i64 {
    let x = ch.recv();
    let y = ch.recv();
    return x + y;
}
"#,
    );
    assert!(ok, "{out}");
    assert_eq!(out, "15\n");
}

// Channel<GC-type> buffers are GC-traced: computed (non-literal) string values
// queued in a channel survive collection until received (willow-dsw GC tracing).
#[test]
fn coop_chan_04_gc_element_channel_traced() {
    let (out, ok) = compile_and_run_gc_stress(
        r#"
async fn producer(ch: Channel<String>, tag: String) -> i64 {
    await sleep(1);
    ch.send(tag + "-1");
    ch.send(tag + "-2");
    ch.close();
    return 0;
}
async fn consumer(ch: Channel<String>) -> i64 {
    let a = ch.recv();
    let b = ch.recv();
    println(a);
    println(b);
    return 0;
}
async fn main() {
    let ch = Channel<String>::new();
    let p = producer(ch, "x");
    let c = consumer(ch);
    c.join();
    p.join();
}
"#,
    );
    assert!(ok, "{out}");
    assert_eq!(out, "x-1\nx-2\n");
}

#[test]
fn coop_chan_05_parked_receiver_frame_survives_gc_before_send() {
    let (out, ok) = compile_and_run_gc_stress(
        r#"
async fn producer(ch: Channel<String>) -> i64 {
    await sleep(1);
    gc_collect();
    ch.send("done");
    ch.close();
    return 0;
}
async fn consumer(ch: Channel<String>, prefix: String) -> String {
    let kept = prefix + "-keep";
    let v = ch.recv();
    gc_collect();
    return kept + ":" + v;
}
async fn main() {
    let ch = Channel<String>::new();
    let p = producer(ch);
    let c = consumer(ch, "rx");
    gc_collect();
    println(c.join());
    p.join();
}
"#,
    );
    assert!(ok, "{out}");
    assert_eq!(out, "rx-keep:done\n");
}

#[test]
fn coop_chan_06_gc_stress_all_scheduler_boundaries() {
    let (out, ok) = compile_and_run_gc_stress_all(
        r#"
class Box { pub text: String; }
async fn producer(ch: Channel<Box>) -> i64 {
    await sleep(1);
    ch.send(new Box("v" + "1"));
    ch.close();
    return 0;
}
async fn consumer(ch: Channel<Box>, prefix: String) -> String {
    let kept = prefix + "-keep";
    let b = ch.recv();
    return kept + ":" + b.text;
}
async fn main() {
    let ch = Channel<Box>::new();
    let p = producer(ch);
    let c = consumer(ch, "rx");
    println(c.join());
    p.join();
}
"#,
    );
    assert!(ok, "{out}");
    assert_eq!(out, "rx-keep:v1\n");
}

fn assert_catalog_lines(out: &str, cases: &[(&str, &str)]) {
    let actual = out.lines().collect::<Vec<_>>();
    assert_eq!(
        actual.len(),
        cases.len(),
        "catalog output line count mismatch:\n{out}"
    );
    for (index, ((name, expected), actual)) in cases.iter().zip(actual.iter()).enumerate() {
        assert_eq!(
            *actual,
            *expected,
            "catalog case {} ({name}) failed",
            index + 1
        );
    }
}

#[test]
fn async_catalog_50_cases() {
    let (out, ok) = compile_and_run_gc_stress(
        r#"
import std::collections::Array;

async fn id_i64(x: i64) -> i64 { await sleep(1); return x; }
async fn plus(a: i64, b: i64) -> i64 { await sleep(1); return a + b; }
async fn flag(value: bool) -> bool { await sleep(1); return value; }
async fn half(value: f64) -> f64 { await sleep(1); return value / 2.0; }
async fn mark(value: String) -> String { await sleep(1); return value + "!"; }
async fn wrap(value: String) -> String { return await mark(value); }
async fn delayed_sum(values: Array<i64>) -> i64 {
    let mut total = 0;
    for value in values { await sleep(1); total = total + value; }
    return total;
}
async fn range_sum(end: i64) -> i64 {
    let mut total = 0;
    for value in 1..end { await sleep(1); total = total + value; }
    return total;
}
async fn while_sum(end: i64) -> i64 {
    let mut total = 0;
    let mut value = 1;
    while value <= end { await sleep(1); total = total + value; value = value + 1; }
    return total;
}
async fn choose(cond: bool, a: i64, b: i64) -> i64 { await sleep(1); return cond ? a : b; }
async fn mutate_local(seed: i64) -> i64 {
    let mut value = seed;
    value = await plus(value, 2);
    await sleep(1);
    return value;
}
async fn producer(ch: Channel<i64>) -> i64 {
    await sleep(1);
    ch.send(10);
    ch.send(20);
    ch.close();
    return 0;
}
async fn consumer(ch: Channel<i64>) -> i64 {
    let a = ch.recv();
    let b = ch.recv();
    return a + b;
}
async fn string_producer(ch: Channel<String>, prefix: String) -> i64 {
    await sleep(1);
    ch.send(prefix + "-a");
    ch.send(prefix + "-b");
    ch.close();
    return 0;
}
async fn string_consumer(ch: Channel<String>) -> String {
    let a = ch.recv();
    let b = ch.recv();
    return a + b;
}
async fn square(x: i64) -> i64 { return x * x; }
async fn async_square(x: i64) -> i64 { await sleep(1); return x * x; }
async fn async_bool(value: i64) -> bool { await sleep(1); return value > 0; }
async fn async_text(value: String) -> String { await sleep(1); return value + "?"; }
async fn nested_left(x: i64) -> i64 {
    let y = await plus(x, 1);
    await sleep(1);
    return y + 1;
}
async fn nested_right(x: i64) -> i64 {
    let y = await nested_left(x);
    await sleep(1);
    return y + 1;
}
async fn count_down(seed: i64) -> i64 {
    let mut value = seed;
    while value > 0 { await sleep(1); value = value - 1; }
    return value;
}
async fn maybe_sleep(flag_value: bool) -> i64 {
    if flag_value { await sleep(1); return 31; } else { await sleep(1); return 32; }
}
async fn array_pick(values: Array<i64>, index: i64) -> i64 { await sleep(1); return values[index]; }
async fn array_update() -> i64 {
    let mut values: Array<i64> = [1, 2, 3];
    values[1] = await plus(values[0], values[2]);
    await sleep(1);
    return values[1];
}
async fn gc_string(value: String) -> String {
    gc_collect();
    await sleep(1);
    gc_collect();
    return value + "*";
}
async fn return_array() -> Array<i64> { await sleep(1); return [4, 5, 6]; }
async fn join_after_sleep(value: i64) -> i64 { await sleep(1); return value; }

async fn main() {
    println(await id_i64(1));
    println(await plus(1, 1));
    println(await flag(true));
    println(await flag(false));
    println(await half(5.0));
    println(await mark("hello"));
    println(await wrap("wrap"));
    let s1 = await id_i64(3);
    let s2 = await id_i64(4);
    println(s1 + s2);
    let mut assigned = 0;
    assigned = await plus(5, 5);
    println(assigned);
    await id_i64(10);
    println(11);
    if true { await sleep(1); println(12); }
    if false { println(0); } else { await sleep(1); println(13); }
    println(await while_sum(3));
    println(await delayed_sum([1, 2, 3]));
    println(await range_sum(4));
    let h1 = square(4);
    println(h1.join());
    let h2 = async_square(5);
    println(h2.join());
    let ha = async_square(2);
    let hb = async_square(3);
    println(ha.join() + hb.join());
    let hc = join_after_sleep(21);
    await sleep(1);
    println(hc.join());
    let ch = Channel<i64>::new();
    let p = producer(ch);
    let c = consumer(ch);
    println(c.join());
    p.join();
    let sch = Channel<String>::new();
    let sp = string_producer(sch, "m");
    let sc = string_consumer(sch);
    println(sc.join());
    sp.join();
    ch.close();
    println(ch.recv());
    println(await gc_string("live"));
    let array_value: Array<i64> = [4, 5];
    println(await delayed_sum(array_value));
    println(await choose(true, 27, 0));
    println(await choose(false, 0, 28));
    println(await plus(14, 15));
    println(await plus(15, 16));
    println(await maybe_sleep(true));
    println(await maybe_sleep(false));
    println(await nested_right(30));
    println(await count_down(3));
    println(await array_pick([40, 41, 42], 1));
    println(await array_update());
    let returned = await return_array();
    println(returned[2]);
    println(await async_bool(1));
    println(await async_bool(-1));
    println(await async_text("text"));
    let j1 = async_bool(2);
    println(j1.join());
    let j2 = async_text("join");
    println(j2.join());
    let j3 = half(3.0);
    println(j3.join());
    let mut loop_total = 0;
    for n in 1..5 { await sleep(1); loop_total = loop_total + n; }
    println(loop_total);
    let mut while_total = 0;
    let mut wi = 0;
    while wi < 3 { await sleep(1); while_total = while_total + wi; wi = wi + 1; }
    println(while_total);
    await sleep(0);
    println(48);
    await sleep(-1);
    println(49);
    println(await mutate_local(40));
    let j4 = async_square(6);
    println(j4.join());
    println(await delayed_sum([7, 8]));
    println(await mark("last"));
    println(await plus(25, 25));
}
"#,
    );
    assert!(ok, "{out}");
    assert_catalog_lines(
        &out,
        &[
            ("await_i64", "1"),
            ("await_add", "2"),
            ("await_bool_true", "true"),
            ("await_bool_false", "false"),
            ("await_f64", "2.5"),
            ("await_string", "hello!"),
            ("return_call_await", "wrap!"),
            ("sequential_awaits", "7"),
            ("assign_await", "10"),
            ("discard_await", "11"),
            ("await_in_if", "12"),
            ("await_in_else", "13"),
            ("await_in_while", "6"),
            ("await_in_array_for", "6"),
            ("await_in_range_for", "6"),
            ("spawn_sync_join", "16"),
            ("spawn_async_join", "25"),
            ("multiple_async_joins", "13"),
            ("await_before_join", "21"),
            ("channel_i64", "30"),
            ("channel_string", "m-am-b"),
            ("closed_channel_default", "0"),
            ("gc_string_across_await", "live*"),
            ("array_param_across_await", "9"),
            ("ternary_true_after_await", "27"),
            ("ternary_false_after_await", "28"),
            ("await_add_again", "29"),
            ("await_add_second", "31"),
            ("if_true_return", "31"),
            ("if_false_return", "32"),
            ("nested_call_await", "33"),
            ("countdown_loop", "0"),
            ("array_index_after_await", "41"),
            ("array_assignment_await", "4"),
            ("async_return_array", "6"),
            ("spawn_bool_true", "true"),
            ("spawn_bool_false", "false"),
            ("async_text", "text?"),
            ("join_bool", "true"),
            ("join_string", "join?"),
            ("join_f64", "1.5"),
            ("main_range_loop", "10"),
            ("main_while_loop", "3"),
            ("zero_sleep", "48"),
            ("negative_sleep", "49"),
            ("mutate_local_after_await", "42"),
            ("spawn_square_again", "36"),
            ("array_sum_again", "15"),
            ("string_mark_again", "last!"),
            ("final_add", "50"),
        ],
    );
}

#[test]
fn async_object_catalog_50_cases() {
    let (out, ok) = compile_and_run_gc_stress(
        r#"
import std::collections::Array;

class Box {
    pub v: i64;
    pub fn get(self) -> i64 { return self.v; }
    pub fn add(self, n: i64) { self.v = self.v + n; }
    pub fn set(self, n: i64) { self.v = n; }
    pub fn copy(self) -> Box { return new Box(self.v); }
    pub static fn new(v: i64) -> Box { return new Box(v); }
}
class Holder { pub text: String; pub child: Box?; }
class Pair { pub left: Box; pub right: Box; }
class FlagBox { pub ok: bool; }
class FloatBox { pub v: f64; }
class Node { pub v: i64; pub next: Node?; }
interface Named { fn name(self) -> String; }
interface Greeter { fn name(self) -> String; fn greet(self) -> String { return "hi " + self.name(); } }
class User implements Named, Greeter { pub label: String; pub fn name(self) -> String { return self.label; } }
open class Animal { pub open fn score(self) -> i64 { return 1; } }
class Dog extends Animal { pub bonus: i64; pub override fn score(self) -> i64 { return self.bonus + 2; } }

async fn read_value(b: Box) -> i64 { await sleep(1); return b.v; }
async fn read_method(b: Box) -> i64 { await sleep(1); return b.get(); }
async fn add_after(b: Box, n: i64) -> i64 { await sleep(1); b.add(n); return b.v; }
async fn set_after(b: Box, n: i64) -> i64 { await sleep(1); b.set(n); return b.v; }
async fn make_box(v: i64) -> Box { await sleep(1); return new Box(v); }
async fn same_box(b: Box) -> Box { await sleep(1); return b; }
async fn copy_after(b: Box) -> Box { await sleep(1); return b.copy(); }
async fn plus_i64(a: i64, b: i64) -> i64 { await sleep(1); return a + b; }
async fn holder_text(h: Holder) -> String { await sleep(1); return h.text; }
async fn update_holder(h: Holder, suffix: String) -> String { await sleep(1); h.text = h.text + suffix; return h.text; }
async fn child_value(h: Holder) -> i64 { await sleep(1); let child = h.child; if child == nil { return 0; } return child.v; }
async fn pair_sum(p: Pair) -> i64 { await sleep(1); return p.left.v + p.right.v; }
async fn array_sum(xs: Array<Box>) -> i64 { let mut total = 0; for x in xs { await sleep(1); total = total + x.v; } return total; }
async fn array_sum_gc(xs: Array<Box>) -> i64 { gc_collect(); let mut total = 0; for x in xs { await sleep(1); gc_collect(); total = total + x.v; } return total; }
async fn box_producer(ch: Channel<Box>) -> i64 { await sleep(1); ch.send(new Box(9)); ch.send(new Box(10)); ch.close(); return 0; }
async fn box_consumer(ch: Channel<Box>) -> i64 { let a = ch.recv(); let b = ch.recv(); return a.v + b.v; }
async fn return_boxes() -> Array<Box> { await sleep(1); return [new Box(9), new Box(11)]; }
async fn gc_box_value(b: Box) -> i64 { gc_collect(); await sleep(1); gc_collect(); return b.v; }
async fn gc_holder_text(h: Holder) -> String { gc_collect(); await sleep(1); gc_collect(); return h.text; }
async fn named_name(n: Named) -> String { await sleep(1); return n.name(); }
async fn greet_text(g: Greeter) -> String { await sleep(1); return g.greet(); }
async fn animal_score(a: Animal) -> i64 { await sleep(1); return a.score(); }
async fn option_box(opt: Option<Box>) -> i64 { await sleep(1); return match opt { Option::Some(b) => b.v, Option::None => 0 }; }
async fn result_box(r: Result<Box, String>) -> i64 { await sleep(1); return match r { Result::Ok(b) => b.v, Result::Err(e) => 0 }; }
fn sound(n: Named) -> String { return match n { User(u) => u.name() + "!", _ => "?" }; }
async fn named_sound(n: Named) -> String { await sleep(1); return sound(n); }
fn sum_nodes(node: Node?) -> i64 { if node == nil { return 0; } return node.v + sum_nodes(node.next); }
async fn async_sum_nodes(node: Node?) -> i64 { await sleep(1); return sum_nodes(node); }
async fn choose_box(cond: bool, a: Box, b: Box) -> Box { await sleep(1); return cond ? a : b; }
async fn make_from_static(v: i64) -> Box { await sleep(1); return Box::new(v); }
async fn flag_value(f: FlagBox) -> bool { await sleep(1); return f.ok; }
async fn float_half(f: FloatBox) -> f64 { await sleep(1); return f.v / 2.0; }
async fn make_holder(text: String, value: i64) -> Holder { await sleep(1); return new Holder(text, new Box(value)); }
async fn holder_child_copy_value(h: Holder) -> i64 { await sleep(1); let child = h.child; if child == nil { return 0; } let copied = child.copy(); return copied.v; }
async fn user_producer(ch: Channel<User>) -> i64 { await sleep(1); ch.send(new User("chan")); ch.close(); return 0; }
async fn user_consumer(ch: Channel<User>) -> String { let u = ch.recv(); return u.name(); }
async fn nested_box(v: i64) -> Box { return await make_box(v); }

async fn main() {
    println(await read_value(new Box(1)));
    println(await read_method(new Box(2)));
    let b3 = new Box(3);
    println(await add_after(b3, 1));
    println(b3.v);
    let b5 = await make_box(5);
    println(b5.v);
    let b6 = await same_box(b5);
    println(b6.v);
    let alias = b3;
    println(await add_after(alias, 3));
    println(b3.v);
    println(await set_after(b3, 9));
    println(b3.v);
    let h = new Holder("a", b3);
    println(await holder_text(h));
    println(await update_holder(h, "b"));
    println(h.text);
    println(await child_value(h));
    let empty = new Holder("empty", nil);
    println(await child_value(empty));
    let pair = new Pair(new Box(7), new Box(8));
    println(await pair_sum(pair));
    println(await array_sum([new Box(1), new Box(2), new Box(3)]));
    let mut arr: Array<Box> = [new Box(4), new Box(5)];
    arr[1] = await make_box(18);
    println(arr[1].v);
    let ch = Channel<Box>::new();
    let p = box_producer(ch);
    let c = box_consumer(ch);
    println(c.join());
    p.join();
    let boxes = await return_boxes();
    println(boxes[0].v + boxes[1].v);
    let j = make_box(21);
    println(j.join().v);
    let jr = read_value(new Box(22));
    println(jr.join());
    let shared = new Box(20);
    let r1 = read_value(shared);
    let r2 = read_method(shared);
    println(r1.join() + r2.join());
    println(await gc_box_value(new Box(24)));
    println(await gc_holder_text(new Holder("gc", new Box(1))));
    let u = new User("Ada");
    println(await named_name(u));
    println(await greet_text(u));
    println(await animal_score(new Dog(26)));
    println(await option_box(Option::Some(new Box(29))));
    println(await option_box(Option::None));
    println(await result_box(Result::Ok(new Box(31))));
    println(await result_box(Result::Err("bad")));
    println(await named_sound(new User("Rex")));
    let n3 = new Node(3, nil);
    let n2 = new Node(2, n3);
    let n1 = new Node(1, n2);
    println(await async_sum_nodes(n1));
    println((await choose_box(true, new Box(35), new Box(0))).v);
    println((await choose_box(false, new Box(0), new Box(36))).v);
    let copied = await copy_after(new Box(37));
    println(copied.v);
    let b38 = await make_from_static(38);
    println(b38.get());
    let h39 = new Holder("h", nil);
    h39.child = await make_box(39);
    println(await child_value(h39));
    let b40 = new Box(0);
    b40.v = await plus_i64(20, 20);
    println(b40.v);
    println(await flag_value(new FlagBox(true)));
    println(await float_half(new FloatBox(84.0)));
    println(await array_sum_gc([new Box(20), new Box(23)]));
    let h44 = await make_holder("n", 44);
    println(await child_value(h44));
    println(await holder_child_copy_value(h44));
    let user_ch = Channel<User>::new();
    let up = user_producer(user_ch);
    let uc = user_consumer(user_ch);
    println(uc.join());
    up.join();
    let jh = make_holder("j", 47);
    println(await child_value(jh.join()));
    println((await nested_box(48)).v);
    println(await named_name(new User("last")));
    println(await read_value(new Box(50)));
}
"#,
    );
    assert!(ok, "{out}");
    assert_catalog_lines(
        &out,
        &[
            ("object_param_field", "1"),
            ("object_method_after_await", "2"),
            ("object_mutation_return", "4"),
            ("object_mutation_visible", "4"),
            ("async_returns_object", "5"),
            ("same_object_return", "5"),
            ("alias_mutation_return", "7"),
            ("alias_mutation_visible", "7"),
            ("set_after_await_return", "9"),
            ("set_after_await_visible", "9"),
            ("string_field_read", "a"),
            ("string_field_update", "ab"),
            ("string_field_visible", "ab"),
            ("nullable_child_present", "9"),
            ("nullable_child_nil", "0"),
            ("nested_pair_sum", "15"),
            ("object_array_sum", "6"),
            ("object_array_assignment", "18"),
            ("object_channel_sum", "19"),
            ("async_returns_object_array", "20"),
            ("spawn_returns_object", "21"),
            ("spawn_reads_object", "22"),
            ("two_tasks_read_same_object", "40"),
            ("gc_object_across_await", "24"),
            ("gc_string_field_across_await", "gc"),
            ("interface_dispatch_after_await", "Ada"),
            ("interface_default_after_await", "hi Ada"),
            ("virtual_dispatch_after_await", "28"),
            ("option_some_object", "29"),
            ("option_none_object", "0"),
            ("result_ok_object", "31"),
            ("result_err_object", "0"),
            ("interface_downcast_after_await", "Rex!"),
            ("nullable_chain_sum", "6"),
            ("ternary_object_true", "35"),
            ("ternary_object_false", "36"),
            ("copy_method_after_await", "37"),
            ("static_constructor_after_await", "38"),
            ("nullable_field_assignment_await", "39"),
            ("field_assignment_await_scalar", "40"),
            ("bool_field_after_await", "true"),
            ("f64_field_after_await", "42"),
            ("gc_object_array_after_await", "43"),
            ("async_returns_nested_holder", "44"),
            ("copy_nullable_child", "44"),
            ("channel_user_object", "chan"),
            ("join_holder_then_await", "47"),
            ("nested_async_object_return", "48"),
            ("interface_gc_final", "last"),
            ("final_object_read", "50"),
        ],
    );
}

// ----------------------------------------------------------------------------
// select (willow-7aj): wait on multiple channel ops. A recv case is ready when
// its channel has a value or is closed; a send case (unbounded) is always
// ready; the first ready case runs; `default` runs when nothing is ready.
// ----------------------------------------------------------------------------

#[test]
fn select_01_default_on_empty() {
    let (out, ok) = compile_and_run(
        r#"
async fn main() {
    let ch = Channel<i64>::new();
    select {
        let v = ch.recv() => { println(v); }
        default => { println(-1); }
    }
}
"#,
    );
    assert!(ok, "{out}");
    assert_eq!(out, "-1\n");
}

#[test]
fn select_02_recv_ready_value() {
    let (out, ok) = compile_and_run(
        r#"
async fn main() {
    let ch = Channel<i64>::new();
    ch.send(42);
    select {
        let v = ch.recv() => { println(v); }
        default => { println(-1); }
    }
}
"#,
    );
    assert!(ok, "{out}");
    assert_eq!(out, "42\n");
}

#[test]
fn select_03_recv_drives_scheduler_until_producer() {
    // No default: select drives the scheduler until a spawned producer sends.
    let (out, ok) = compile_and_run(
        r#"
async fn producer(ch: Channel<i64>) -> i64 {
    await sleep(1);
    ch.send(99);
    return 0;
}
async fn main() {
    let ch = Channel<i64>::new();
    let p = producer(ch);
    select {
        let v = ch.recv() => { println(v); }
    }
    p.join();
}
"#,
    );
    assert!(ok, "{out}");
    assert_eq!(out, "99\n");
}

#[test]
fn select_04_first_ready_of_multiple_recv() {
    let (out, ok) = compile_and_run(
        r#"
async fn main() {
    let a = Channel<i64>::new();
    let b = Channel<i64>::new();
    b.send(7);
    select {
        let x = a.recv() => { println(x + 1000); }
        let y = b.recv() => { println(y); }
        default => { println(-1); }
    }
}
"#,
    );
    assert!(ok, "{out}");
    assert_eq!(out, "7\n");
}

#[test]
fn select_05_send_case() {
    let (out, ok) = compile_and_run(
        r#"
async fn main() {
    let out = Channel<i64>::new();
    select {
        out.send(55) => { println(1); }
        default => { println(-1); }
    }
    println(out.recv());
}
"#,
    );
    assert!(ok, "{out}");
    assert_eq!(out, "1\n55\n");
}

#[test]
fn select_06_string_channel_literal_gc() {
    // A String channel select-send of a literal queues correctly (literal must
    // be collected from the select case), and survives GC stress.
    let (out, ok) = compile_and_run_gc_stress(
        r#"
async fn main() {
    let ch = Channel<String>::new();
    select {
        ch.send("hello") => { println(1); }
    }
    println(ch.recv());
}
"#,
    );
    assert!(ok, "{out}");
    assert_eq!(out, "1\nhello\n");
}

#[test]
fn select_07_non_channel_is_error() {
    assert_compile_error_contains(
        r#"
async fn main() {
    let x = 5;
    select {
        let v = x.recv() => { println(v); }
    }
}
"#,
        &["error[E0807]", "Channel"],
    );
}

// willow-lpn.7: a task parked on a TIMER keeps its async-frame GC roots alive
// while a CONCURRENT task triggers collection. The sleeper's frame is a runtime
// root while parked, so its live String survives.
#[test]
fn coop_gc_06_timer_parked_frame_survives_concurrent_gc() {
    let (out, ok) = compile_and_run_gc_stress(
        r#"
async fn sleeper() -> i64 {
    let s = "kept-across-timer-park";
    await sleep(5);
    println(s);
    return 0;
}
async fn collector() -> i64 {
    await sleep(1);
    gc_collect();
    let junk = "x" + "y";
    gc_collect();
    return 0;
}
async fn main() {
    let a = sleeper();
    let b = collector();
    a.join();
    b.join();
}
"#,
    );
    assert!(ok, "{out}");
    assert_eq!(out, "kept-across-timer-park\n");
}

// ── willow-7aj: cooperative-suspend `select` (a select INSIDE a task PARKS on
// its channels instead of block-driving). 20 test perspectives:
//  1. single recv parks when empty, woken by a later send -> receives value
//  2. repeated select in a while loop (park/wake each iteration)
//  3. multi-channel select: parks on all, woken by whichever is ready first
//  4. multi-channel across iterations (channel a then channel b)
//  5. default present + channel empty -> default branch runs (no park)
//  6. default present + channel ready -> ready branch runs (default skipped)
//  7. send case is always ready and fires
//  8. Channel<String> recv binding is GC-traced (survives gc_collect after recv)
//  9. recv binding is usable inside the case body
// 10. case body with its OWN suspend (await sleep) after the binding -> binding survives
// 11. select woken by close() -> recv returns the element default (0)
// 12. unregister: after picking channel a, a later send on the OTHER channel b
//     does not corrupt the next select iteration
// 13. `_` discard binding recv
// 14. select nested in a while loop summing values (canonical consumer)
// 15. source-order priority when multiple recv cases are ready
// 16. send-case value matches the channel element type
// 17. a select-only task is a cooperative leaf (spawn/join works)
// 18. whole thing under WILLOW_GC_STRESS=all
// 19. select runs in a spawned task joined by main
// 20. case body contains a second recv (nested suspend points)

#[test]
fn coop_select_01_single_recv_parks_and_wakes() {
    let (out, ok) = compile_and_run(
        r#"
async fn producer(ch: Channel<i64>) -> i64 { await sleep(1); ch.send(42); return 0; }
async fn consumer(ch: Channel<i64>) -> i64 {
    let mut total = 0;
    select { let v = ch.recv() => { total = v; } }
    return total;
}
async fn main() {
    let ch = Channel<i64>::new();
    let p = producer(ch);
    let c = consumer(ch);
    println(c.join()); p.join();
}
"#,
    );
    assert!(ok, "{out}");
    assert_eq!(out, "42\n");
}

#[test]
fn coop_select_02_while_loop_sum() {
    let (out, ok) = compile_and_run(
        r#"
async fn producer(ch: Channel<i64>) -> i64 {
    await sleep(1); ch.send(10);
    await sleep(1); ch.send(20);
    await sleep(1); ch.send(30);
    return 0;
}
async fn consumer(ch: Channel<i64>) -> i64 {
    let mut total = 0;
    let mut i = 0;
    while i < 3 {
        select { let v = ch.recv() => { total = total + v; } }
        i = i + 1;
    }
    return total;
}
async fn main() {
    let ch = Channel<i64>::new();
    let p = producer(ch);
    let c = consumer(ch);
    println(c.join()); p.join();
}
"#,
    );
    assert!(ok, "{out}");
    assert_eq!(out, "60\n");
}

#[test]
fn coop_select_03_multi_channel_parks_on_both() {
    // Perspectives 3, 4, 12: parks on both channels; after a wakes it, the next
    // iteration parks again and b wakes it; unregistering from the non-chosen
    // channel keeps the second iteration correct.
    let (out, ok) = compile_and_run(
        r#"
async fn p1(ch: Channel<i64>) -> i64 { await sleep(1); ch.send(100); return 0; }
async fn p2(ch: Channel<i64>) -> i64 { await sleep(2); ch.send(200); return 0; }
async fn consumer(a: Channel<i64>, b: Channel<i64>) -> i64 {
    let mut total = 0;
    let mut n = 0;
    while n < 2 {
        select {
            let v = a.recv() => { total = total + v; }
            let v = b.recv() => { total = total + v; }
        }
        n = n + 1;
    }
    return total;
}
async fn main() {
    let a = Channel<i64>::new();
    let b = Channel<i64>::new();
    let x = p1(a);
    let y = p2(b);
    let c = consumer(a, b);
    println(c.join()); x.join(); y.join();
}
"#,
    );
    assert!(ok, "{out}");
    assert_eq!(out, "300\n");
}

#[test]
fn coop_select_04_default_when_empty() {
    let (out, ok) = compile_and_run(
        r#"
async fn worker(ch: Channel<i64>) -> i64 {
    await sleep(1);
    let mut hit = 0;
    select {
        let v = ch.recv() => { hit = v; }
        default => { hit = -1; }
    }
    return hit;
}
async fn main() {
    let ch = Channel<i64>::new();
    let w = worker(ch);
    println(w.join());
}
"#,
    );
    assert!(ok, "{out}");
    assert_eq!(out, "-1\n");
}

#[test]
fn coop_select_05_default_skipped_when_ready() {
    let (out, ok) = compile_and_run(
        r#"
async fn worker(ch: Channel<i64>) -> i64 {
    ch.send(5);
    await sleep(1);
    let mut hit = 0;
    select {
        let v = ch.recv() => { hit = v; }
        default => { hit = -1; }
    }
    return hit;
}
async fn main() {
    let ch = Channel<i64>::new();
    let w = worker(ch);
    println(w.join());
}
"#,
    );
    assert!(ok, "{out}");
    assert_eq!(out, "5\n");
}

#[test]
fn coop_select_06_send_case() {
    let (out, ok) = compile_and_run(
        r#"
async fn sender(ch: Channel<i64>) -> i64 {
    await sleep(1);
    select { ch.send(7) => { } }
    return 0;
}
async fn consumer(ch: Channel<i64>) -> i64 { let v = ch.recv(); return v; }
async fn main() {
    let ch = Channel<i64>::new();
    let s = sender(ch);
    let c = consumer(ch);
    println(c.join()); s.join();
}
"#,
    );
    assert!(ok, "{out}");
    assert_eq!(out, "7\n");
}

#[test]
fn coop_select_07_string_binding_gc_safe() {
    // Perspectives 8, 18: the recv binding's frame slot is GC-traced.
    let (out, ok) = compile_and_run_gc_stress(
        r#"
async fn producer(ch: Channel<String>) -> i64 {
    await sleep(1);
    let s = "hello-" + "world";
    ch.send(s);
    return 0;
}
async fn consumer(ch: Channel<String>) -> i64 {
    let mut out = "empty";
    select { let v = ch.recv() => { out = v; } }
    gc_collect();
    println(out);
    return 0;
}
async fn main() {
    let ch = Channel<String>::new();
    let p = producer(ch);
    let c = consumer(ch);
    c.join(); p.join();
}
"#,
    );
    assert!(ok, "{out}");
    assert_eq!(out, "hello-world\n");
}

#[test]
fn coop_select_08_woken_by_close() {
    // Perspective 11: close() wakes a parked select; recv returns the default (0).
    let (out, ok) = compile_and_run(
        r#"
async fn producer(ch: Channel<i64>) -> i64 { await sleep(1); ch.close(); return 0; }
async fn consumer(ch: Channel<i64>) -> i64 {
    let mut got = 99;
    select { let v = ch.recv() => { got = v; } }
    return got;
}
async fn main() {
    let ch = Channel<i64>::new();
    let p = producer(ch);
    let c = consumer(ch);
    println(c.join()); p.join();
}
"#,
    );
    assert!(ok, "{out}");
    assert_eq!(out, "0\n");
}

#[test]
fn coop_select_09_case_body_nested_suspend() {
    // Perspectives 10, 20: the case body itself suspends (await sleep, then a
    // second recv) after binding; the binding and locals survive those suspends.
    let (out, ok) = compile_and_run_gc_stress(
        r#"
async fn producer(ch: Channel<i64>) -> i64 {
    await sleep(1); ch.send(11);
    await sleep(1); ch.send(22);
    return 0;
}
async fn consumer(ch: Channel<i64>) -> i64 {
    let mut total = 0;
    select {
        let v = ch.recv() => {
            await sleep(1);
            let w = ch.recv();
            total = v + w;
        }
    }
    return total;
}
async fn main() {
    let ch = Channel<i64>::new();
    let p = producer(ch);
    let c = consumer(ch);
    println(c.join()); p.join();
}
"#,
    );
    assert!(ok, "{out}");
    assert_eq!(out, "33\n");
}

#[test]
fn coop_select_10_source_order_priority() {
    // Perspectives 13, 15: when several recv cases are ready, the first in source
    // order wins; `_` discard binding is allowed.
    let (out, ok) = compile_and_run(
        r#"
async fn worker(a: Channel<i64>, b: Channel<i64>) -> i64 {
    a.send(1);
    b.send(2);
    await sleep(1);
    let mut picked = 0;
    select {
        let _ = a.recv() => { picked = 10; }
        let v = b.recv() => { picked = v; }
    }
    return picked;
}
async fn main() {
    let a = Channel<i64>::new();
    let b = Channel<i64>::new();
    let w = worker(a, b);
    println(w.join());
}
"#,
    );
    assert!(ok, "{out}");
    assert_eq!(out, "10\n");
}
