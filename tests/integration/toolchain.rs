use super::support::*;

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
    pub init(self, value: i64, next: Node?) {
        self.value = value;
        self.next = next;
    }
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
    pub init(self, value: i64, next: Node?) {
        self.value = value;
        self.next = next;
    }
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
