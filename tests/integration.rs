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
        ("example/booleans.wi", "true\nfalse\ntrue\ntrue\n"),
        ("example/class_hierarchy.wi", "3\n"),
        ("example/class.wi", "42\n"),
        ("example/control_flow.wi", "120\n"),
        ("example/debug_source_map.wi", "12\n"),
        ("example/early_return.wi", "7\n0\n12\n"),
        ("example/example.wi", "50\ntrue\n"),
        ("example/fib.wi", "63245986\n"),
        ("example/fib_bench.wi", "63245986\n"),
        ("example/floats.wi", "4\ntrue\n-4\n"),
        ("example/fn_values.wi", "20\n25\n30\n107\n104\n"),
        ("example/leibniz_pi.wi", "3.141592663589326\n"),
        ("example/functions.wi", "25\ntrue\n"),
        ("example/hello.wi", "50"),
        ("example/hello_world.wi", "Hello, world!\n"),
        ("example/import_demo/main.wi", "30\n42\n42\n99\n3\n42\n"),
        ("example/mutability.wi", "6\n15\ntrue\n"),
        ("example/nested_loops.wi", "30\n"),
        ("example/nil_guard_demo.wi", "42\n-7\n0\ntrue\nfalse\nfalse\n126\n99\n"),
        ("example/nil_nullable.wi", "0\n10\n20\ntrue\n10\n"),
        ("example/nil_safe_chain.wi", "60\n3\n30\n-1\n120\n"),
        ("example/print_test.wi", "1230\n42\ntrue\nfalsetrue\n"),
        ("example/recursion.wi", "3628800\n1024\n6\n"),
        ("example/references.wi", "11\n22\ntrue\n"),
        ("example/channel_producer.wi", "10\n20\n30\n"),
        ("example/parallel_tasks.wi", "55\n144\n610\n42\nfalse\n"),
        ("example/spawn_join.wi", "9\n16\n25\n42\n"),
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
    assert!(ok, "runtime C main should return success");
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
    assert!(ok, "target Channel producer/spawn example should compile and run");
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

    assert!(output.status.success(), "release spawn build should succeed");

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

    assert_eq!(stdout.trim(), "99", "release binary should print correct output");
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
