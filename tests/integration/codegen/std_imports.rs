use super::*;

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

    let compiler = env!("CARGO_BIN_EXE_willow");
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

    let compiler = env!("CARGO_BIN_EXE_willow");
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

// ── grouped imports (willow-4bv.7, Stage 7) ────────────────────────────────

#[test]
fn test_grouped_std_collection_imports_are_usable() {
    let (out, ok) = compile_and_run(
        r#"
import std::collections::{Array, Map};

fn main() {
    let xs: Array<i64> = [10, 20];
    let values: Map<String, i64> = Map::new();
    values.insert("answer", xs[0] + xs[1] + 12);
    println(values.get("answer").unwrap());
}
"#,
    );
    assert!(ok);
    assert_eq!(out, "42\n");
}

#[test]
fn test_grouped_import_per_item_aliases_are_usable() {
    let (out, ok) = compile_and_run(
        r#"
import std::collections::{Array as List, Map as Dict,};

fn main() {
    let xs: List<i64> = [1, 2, 3];
    let values: Dict<String, i64> = Dict::new();
    values.insert("size", xs.len());
    println(values.get("size").unwrap());
}
"#,
    );
    assert!(ok);
    assert_eq!(out, "3\n");
}

#[test]
fn test_grouped_import_unknown_item_reuses_e2006() {
    assert_compile_error_contains(
        r#"
import std::collections::{Array, Missing};
fn main() {}
"#,
        &["error[E2006]", "no item `Missing` in `std::collections`"],
    );
}

#[test]
fn test_grouped_import_local_declaration_conflict_reuses_e2003() {
    assert_compile_error_contains(
        r#"
import std::collections::{Array, Map};
class Map {}
fn main() {}
"#,
        &["error[E2003]", "import and a local declaration"],
    );
}

#[test]
fn test_grouped_user_module_items_are_callable() {
    let (out, ok) = compile_temp_project_and_run(
        &[
            (
                "math.wi",
                "module math;\n\
                 pub fn add(a: i64, b: i64) -> i64 { return a + b; }\n\
                 pub fn mul(a: i64, b: i64) -> i64 { return a * b; }\n",
            ),
            (
                "main.wi",
                "import math::{add, mul as times};\n\
                 fn main() { println(add(20, 22)); println(times(6, 7)); }\n",
            ),
        ],
        "main.wi",
    );
    assert!(ok);
    assert_eq!(out, "42\n42\n");
}

#[test]
fn test_grouped_user_module_private_item_reuses_visibility_diagnostic() {
    let stderr = compile_temp_project_error_stderr(
        &[
            (
                "helpers.wi",
                "module helpers;\n\
                 pub fn visible() -> i64 { return 1; }\n\
                 fn hidden() -> i64 { return 2; }\n",
            ),
            (
                "main.wi",
                "import helpers::{visible, hidden};\n\
                 fn main() { println(visible()); println(hidden()); }\n",
            ),
        ],
        "main.wi",
    );
    assert!(stderr.contains("error[E2006]"), "stderr: {stderr}");
    assert!(stderr.contains("private"), "stderr: {stderr}");
}

#[test]
fn test_glob_import_reports_clear_unsupported_diagnostic() {
    assert_compile_error_contains(
        "import std::collections::*;\nfn main() {}\n",
        &["error[E0102]", "glob imports are not supported"],
    );
}
