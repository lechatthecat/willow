//! Module-level `const` declarations (willow-jz15.10).
//!
//! `const NAME: T = literal;` declares a named scalar or `String` value at
//! module level. It is registered like a zero-parameter function, so it shares
//! visibility, item imports and module-qualified access with functions, but a
//! use reads the declared type and lowering inlines the literal instead of
//! emitting a call. These perspectives cover each supported type, every
//! spelling a use can take, visibility across modules and the rejected forms.

use super::support::*;

#[track_caller]
fn assert_output(source: &str, expected: &str) {
    let (out, ok) = compile_and_run(source);
    assert!(ok, "build or run failed: {out}");
    assert_eq!(out, expected, "wrong output");
}

const CONFIG: &str = r#"
module config;

pub const LIMIT: i64 = 7;
pub const LABEL: String = "cfg";
pub const ratio: f64 = 0.5;
const SECRET: i64 = 1;

pub fn total() -> i64 {
    return SECRET + LIMIT;
}
"#;

#[track_caller]
fn assert_project_output(entry: &str, expected: &str) {
    let files = [("config.wi", CONFIG), ("main.wi", entry)];
    let (out, ok) = compile_temp_project_and_run(&files, "main.wi");
    assert!(ok, "build or run failed: {out}");
    assert_eq!(out, expected, "wrong output");
}

#[track_caller]
fn assert_project_error(entry: &str, expected_parts: &[&str]) {
    let files = [("config.wi", CONFIG), ("main.wi", entry)];
    let stderr = compile_temp_project_error_stderr(&files, "main.wi");
    for part in expected_parts {
        assert!(
            stderr.contains(part),
            "stderr did not contain `{part}`:\n{stderr}"
        );
    }
}

// ---------------------------------------------------------------------------
// Values and types.
// ---------------------------------------------------------------------------

// 1. The ticket's reproducer: an i64 constant read in `main`.
#[test]
fn c01_i64_constant_is_readable() {
    assert_output("const N: i64 = 5;\nfn main() { println(N); }\n", "5\n");
}

// 2. Negative literals for both numeric types, and the i64 extremes.
#[test]
fn c02_negative_and_extreme_numeric_literals() {
    assert_output(
        r#"
const NEG: i64 = -3;
const MAX: i64 = 9223372036854775807;
const MIN: i64 = -9223372036854775807;
const HALF: f64 = -0.5;
fn main() {
    println(NEG);
    println(MAX);
    println(MIN);
    println(HALF);
}
"#,
        "-3\n9223372036854775807\n-9223372036854775807\n-0.5\n",
    );
}

// 3. bool and String constants.
#[test]
fn c03_bool_and_string_constants() {
    assert_output(
        r#"
const DEBUG: bool = true;
pub const NAME: String = "willow";
fn main() {
    if DEBUG {
        println(NAME + "!");
    }
    println(!DEBUG);
}
"#,
        "willow!\nfalse\n",
    );
}

// 4. A constant is an ordinary operand: arithmetic, comparison, arguments.
#[test]
fn c04_constant_in_expressions_and_arguments() {
    assert_output(
        r#"
const N: i64 = 4;
fn twice(x: i64) -> i64 { return x * 2; }
fn main() {
    println(twice(N) + N);
    println(N > 3 && N < 5);
    let xs = [N, N + 1];
    println(xs[1]);
}
"#,
        "12\ntrue\n5\n",
    );
}

// 5. A constant declared after its first use resolves (items are hoisted).
#[test]
fn c05_constant_declared_after_use() {
    assert_output(
        "fn main() { println(LATE * 2); }\nconst LATE: i64 = 21;\n",
        "42\n",
    );
}

// 6. A constant as a loop bound.
#[test]
fn c06_constant_as_range_bound() {
    assert_output(
        r#"
const COUNT: i64 = 3;
fn main() {
    let mut sum = 0;
    for i in 0..COUNT {
        sum = sum + i;
    }
    println(sum);
}
"#,
        "3\n",
    );
}

// 7. A lambda reads a constant without capturing a local.
#[test]
fn c07_constant_inside_lambda() {
    assert_output(
        r#"
const STEP: i64 = 10;
fn main() {
    let add = |x: i64| -> i64 { return x + STEP; };
    println(add(1));
}
"#,
        "11\n",
    );
}

// 8. A local binding of the same name shadows the constant.
#[test]
fn c08_local_binding_shadows_constant() {
    assert_output(
        r#"
const N: i64 = 1;
fn main() {
    println(N);
    let N = 100;
    println(N);
}
"#,
        "1\n100\n",
    );
}

// 9. Constants are usable inside methods, static initializers and async code.
#[test]
fn c09_constant_in_methods_static_and_async() {
    assert_output(
        r#"
const BASE: i64 = 4;
class Box {
    pub static start: i64 = BASE * 10;
    pub v: i64;
    pub fn bump(self) -> i64 { return self.v + BASE; }
}
async fn later() -> i64 { return BASE + 1; }
async fn main() {
    let b = new Box(1);
    println(b.bump());
    println(Box::start);
    println(await later());
}
"#,
        "5\n40\n5\n",
    );
}

// ---------------------------------------------------------------------------
// Modules: qualified access, item imports and visibility.
// ---------------------------------------------------------------------------

// 10. `module::NAME` (an upper-case member) and `module::name` (lower-case).
#[test]
fn c10_module_qualified_constants() {
    assert_project_output(
        r#"
import config;
fn main() {
    println(config::LIMIT);
    println(config::ratio);
    println(config::LABEL + "!");
}
"#,
        "7\n0.5\ncfg!\n",
    );
}

// 11. An item import binds the bare name, with or without an alias.
#[test]
fn c11_item_import_with_and_without_alias() {
    assert_project_output(
        r#"
import config::LIMIT;
import config::LABEL as Tag;
fn main() {
    println(LIMIT + 1);
    println(Tag);
}
"#,
        "8\ncfg\n",
    );
}

// 12. A private constant is usable inside its own module.
#[test]
fn c12_private_constant_used_by_its_module() {
    assert_project_output(
        "import config;\nfn main() { println(config::total()); }\n",
        "8\n",
    );
}

// 13. A private constant cannot be imported.
#[test]
fn c13_private_constant_import_is_rejected() {
    assert_project_error(
        "import config::SECRET;\nfn main() { println(SECRET); }\n",
        &["SECRET", "private"],
    );
}

// 14. A private constant cannot be read through its module.
#[test]
fn c14_private_constant_qualified_read_is_rejected() {
    assert_project_error(
        "import config;\nfn main() { println(config::SECRET); }\n",
        &["E0402", "constant `config::SECRET` is private"],
    );
}

// 15. A qualified constant has its declared type, not a function type.
#[test]
fn c15_qualified_constant_has_declared_type() {
    assert_project_error(
        "import config;\nfn main() { let s: String = config::LIMIT; println(s); }\n",
        &["mismatched types"],
    );
}

// ---------------------------------------------------------------------------
// Rejected uses.
// ---------------------------------------------------------------------------

// 16. Calling a constant is an error with a fix-it.
#[test]
fn c16_calling_a_constant_is_rejected() {
    assert_compile_error_contains(
        "const A: i64 = 1;\nfn main() { println(A()); }\n",
        &[
            "E0201",
            "cannot call constant `A` of type `i64`",
            "without parentheses",
        ],
    );
}

// 17. Calling a module-qualified constant is an error too.
#[test]
fn c17_calling_a_qualified_constant_is_rejected() {
    assert_project_error(
        "import config;\nfn main() { println(config::LIMIT()); }\n",
        &["E0201", "cannot call constant `config::LIMIT`"],
    );
}

// 18. Assigning to a constant is rejected, plain or compound.
#[test]
fn c18_assignment_to_constant_is_rejected() {
    assert_compile_error_contains(
        "const A: i64 = 1;\nfn main() { A = 3; println(A); }\n",
        &["E0301", "cannot assign to constant `A`"],
    );
    assert_compile_error_contains(
        "const A: i64 = 1;\nfn main() { A += 3; println(A); }\n",
        &["E0301", "cannot assign to constant `A`"],
    );
}

// 19. A constant is a value, not a function: it does not coerce to `fn() -> T`.
#[test]
fn c19_constant_is_not_a_function_value() {
    assert_compile_error_contains(
        r#"
const A: i64 = 1;
fn call(f: fn() -> i64) -> i64 { return f(); }
fn main() { println(call(A)); }
"#,
        &["mismatched types"],
    );
}

// ---------------------------------------------------------------------------
// Rejected declarations.
// ---------------------------------------------------------------------------

// 20. The initializer must be a literal.
#[test]
fn c20_non_literal_initializer_is_rejected() {
    assert_compile_error_contains(
        "const A: i64 = 1 + 2;\nfn main() { println(A); }\n",
        &["E0109"],
    );
}

// 21. The literal must match the declared type (no implicit int -> float).
#[test]
fn c21_mismatched_literal_is_rejected() {
    assert_compile_error_contains(
        "const A: f64 = 1;\nfn main() { println(A); }\n",
        &[
            "E0110",
            "`const A` is declared `f64`, but its value is `i64`",
        ],
    );
}

// 22. Only i64, f64, bool and String are supported types.
#[test]
fn c22_unsupported_type_is_rejected() {
    assert_compile_error_contains("const A: i32 = 1;\nfn main() { println(A); }\n", &["E0110"]);
}

// 23. `const` is module-level only.
#[test]
fn c23_local_const_is_rejected() {
    assert_compile_error_contains(
        "fn main() { const A: i64 = 1; println(A); }\n",
        &["E0105", "only allowed at module level"],
    );
}

// 24. `async` and `open` do not apply to a constant.
#[test]
fn c24_async_and_open_const_are_rejected() {
    assert_compile_error_contains(
        "async const A: i64 = 1;\nfn main() { println(A); }\n",
        &["E0105"],
    );
    assert_compile_error_contains(
        "open const A: i64 = 1;\nfn main() { println(A); }\n",
        &["E0105"],
    );
}

// 25. A constant shares the item namespace: a same-named function collides,
//     in either order, and is reported before any use is checked against it.
#[test]
fn c25_constant_and_function_name_collision_is_rejected() {
    assert_compile_error_contains(
        "const A: i64 = 1;\nfn A() -> i64 { return 2; }\nfn main() { println(A); }\n",
        &["E0706", "function `A` is already declared as a constant"],
    );
    assert_compile_error_contains(
        "fn A() -> i64 { return 2; }\nconst A: i64 = 1;\nfn main() { println(A()); }\n",
        &["E0706", "constant `A` is already declared as a function"],
    );
}

// 25b. The same clash inside an imported module is reported in source order.
#[test]
fn c25b_module_constant_collision_is_rejected() {
    let files = [
        (
            "dup.wi",
            "module dup;\npub const A: i64 = 1;\npub fn A() -> i64 { return 2; }\n",
        ),
        ("main.wi", "import dup;\nfn main() { println(dup::A); }\n"),
    ];
    let stderr = compile_temp_project_error_stderr(&files, "main.wi");
    assert!(
        stderr.contains("function `A` is already declared as a constant"),
        "{stderr}"
    );
}

// 26. `const` is a keyword: it cannot name a binding.
#[test]
fn c26_const_is_a_reserved_word() {
    assert!(expect_compile_error(
        "fn main() { let const = 1; println(const); }\n"
    ));
}

// 27. A constant cannot be a match pattern: it would silently bind instead of
//     compare. A local of the same name shadows it and binds as before.
#[test]
fn c27_constant_as_match_pattern_is_rejected() {
    assert_compile_error_contains(
        r#"
const LIMIT: i64 = 3;
fn main() {
    let v = 3;
    let s = match v {
        LIMIT => "limit",
    };
    println(s);
}
"#,
        &[
            "E1205",
            "constant `LIMIT` cannot be used as a match pattern",
        ],
    );
}
