//! Imported module bodies are type-checked, in their own scope (willow-3eo1).
//!
//! `typecheck_phase` used to call `check_program` on the ENTRY program only.
//! Imported modules went through `register_module_with_id`, which records
//! SIGNATURES so the entry file can call into them, and nothing ever walked a
//! module's statements. The backend, meanwhile, compiled those bodies and ran
//! them. So the entire type checker — not one rule, all of them — stopped at
//! the entry file: a module could annotate a `let` with a blatantly wrong type,
//! fall off the end of a non-void function, or call a function that does not
//! exist, and the program compiled clean and ran, with codegen quietly handing
//! back the zero of the return type.
//!
//! The fix gives every module a checker of its own, the way `concurrency_phase`
//! already gives every module its own `ConcurrencyAnalyzer`. "Of its own" is the
//! load-bearing part: a module sees the prelude plus the modules IT imports
//! under the names IT gave them, never the entry file's imports. Sharing one
//! checker would accept a module that calls a function only the entry file
//! imported — which is exactly what the backend then fails to resolve.
//!
//! 27 perspectives:
//!   1 a wrong `let` annotation in a module is rejected
//!   2 a call to a function that does not exist is rejected
//!   3 a module function that runs off the end is rejected
//!   4 the diagnostic points at the module's file, not the entry file
//!   5 a module method body is checked
//!   6 a module static method body is checked
//!   7 an arity mismatch in a module call is rejected
//!   8 a module calling its own functions is accepted
//!   9 a module's private function is callable from the same module
//!  10 a name only the ENTRY file imported is not in a module's scope
//!  11 a module using a module it imported itself is accepted
//!  12 an import alias inside a module is honoured
//!  13 a single-item import inside a module is honoured (#[ignore], willow-kxy8)
//!  14 a module's `std` import works in its own body
//!  15 a transitively imported module is checked too
//!  16 errors in two different modules are all reported
//!  17 a module is checked once even when two modules import it
//!  18 an error in a module aborts the build before codegen
//!  19 entry and module diagnostics are reported together
//!  20 a correct multi-module program still compiles and runs
//!  21 a module class implementing an interface is checked
//!  22 a module `return` type mismatch is rejected
//!  23 an undefined variable in a module body is rejected
//!  24 a module body reaching a private item of another module is rejected
//!  25 a module async body sees imported non-preemptible typed methods
//!  26 an E1702 fix in a module keeps the module file ID
//!  27 an E1701 fix in a module keeps the module file ID

use super::support::{compile_temp_project_and_run, compile_temp_project_error_stderr};

/// Compile a project expected to fail and return the compiler's stderr.
fn module_error(files: &[(&str, &str)]) -> String {
    compile_temp_project_error_stderr(files, "app.wi")
}

/// Compile and run a project expected to succeed, asserting its stdout.
fn assert_module_output(files: &[(&str, &str)], expected: &str) {
    let (output, ok) = compile_temp_project_and_run(files, "app.wi");
    assert!(ok, "expected the project to compile:\n{output}");
    assert_eq!(output, expected);
}

/// The entry file used by most perspectives: it only calls into the module, so
/// every diagnostic below belongs to the module's own body.
const CALL_INTO_LIB: &str = "import lib;

fn main() {
    println(lib::run());
}
";

// 1. The shape from the bug report. A `let` whose annotation cannot possibly
//    hold its initializer used to compile inside a module.
#[test]
fn module_typecheck_01_a_wrong_let_annotation_is_rejected() {
    let stderr = module_error(&[
        (
            "lib.wi",
            "pub fn run() -> i64 {
    let s: String = 42;
    return 1;
}
",
        ),
        ("app.wi", CALL_INTO_LIB),
    ]);
    assert!(
        stderr.contains("error[E0201]") && stderr.contains("expected `String`"),
        "{stderr}"
    );
}

// 2. A call to a function that exists nowhere. Codegen used to emit the zero of
//    the return type for it, so the program ran and printed a plausible 0.
#[test]
fn module_typecheck_02_a_call_to_a_missing_function_is_rejected() {
    let stderr = module_error(&[
        (
            "lib.wi",
            "pub fn run() -> i64 {
    return nonexistent_function(1);
}
",
        ),
        ("app.wi", CALL_INTO_LIB),
    ]);
    assert!(
        stderr.contains("error[E0350]") && stderr.contains("nonexistent_function"),
        "{stderr}"
    );
}

// 3. E0205 in a module. The rule existed; nothing reached the body to apply it.
#[test]
fn module_typecheck_03_a_body_that_runs_off_the_end_is_rejected() {
    let stderr = module_error(&[
        (
            "lib.wi",
            "pub fn run() -> i64 {
    println(\"no return\");
}
",
        ),
        ("app.wi", CALL_INTO_LIB),
    ]);
    assert!(
        stderr.contains("error[E0205]") && stderr.contains("not all paths through `run`"),
        "{stderr}"
    );
}

// 4. A module diagnostic is useless if it renders against the entry file's
//    source. `source_maps` registers every module by `file_id`, so the span
//    resolves in the file that actually holds the code.
#[test]
fn module_typecheck_04_the_diagnostic_points_at_the_module_file() {
    let stderr = module_error(&[
        (
            "lib.wi",
            "pub fn run() -> i64 {
    let s: String = 42;
    return 1;
}
",
        ),
        ("app.wi", CALL_INTO_LIB),
    ]);
    assert!(stderr.contains("lib.wi"), "{stderr}");
    assert!(
        stderr.contains("let s: String = 42;"),
        "the module's own source line must be quoted:\n{stderr}"
    );
}

// 5. Methods are bodies too. Registration records a class's shape, never its
//    method statements.
#[test]
fn module_typecheck_05_a_method_body_is_checked() {
    let stderr = module_error(&[
        (
            "lib.wi",
            "pub class Counter {
    pub n: i64;

    pub static fn new() -> Counter {
        return new Counter(0);
    }

    pub fn value(self) -> i64 {
        let wrong: String = self.n;
        return self.n;
    }
}

pub fn run() -> i64 {
    return Counter::new().value();
}
",
        ),
        ("app.wi", CALL_INTO_LIB),
    ]);
    assert!(
        stderr.contains("error[E0201]") && stderr.contains("expected `String`"),
        "{stderr}"
    );
}

// 6. A static method has no receiver and takes a different path through the
//    checker; it is walked all the same.
#[test]
fn module_typecheck_06_a_static_method_body_is_checked() {
    let stderr = module_error(&[
        (
            "lib.wi",
            "pub class Counter {
    pub n: i64;

    pub static fn make() -> Counter {
        return missing_helper();
    }
}

pub fn run() -> i64 {
    return Counter::make().n;
}
",
        ),
        ("app.wi", CALL_INTO_LIB),
    ]);
    assert!(
        stderr.contains("error[E0350]") && stderr.contains("missing_helper"),
        "{stderr}"
    );
}

// 7. Arity is checked against the module's own signature table, which the
//    module's checker builds from its own items.
#[test]
fn module_typecheck_07_an_arity_mismatch_is_rejected() {
    let stderr = module_error(&[
        (
            "lib.wi",
            "fn takes_two(a: i64, b: i64) -> i64 {
    return a + b;
}

pub fn run() -> i64 {
    return takes_two(1);
}
",
        ),
        ("app.wi", CALL_INTO_LIB),
    ]);
    assert!(
        stderr.contains("expects 2") || stderr.contains("argument"),
        "{stderr}"
    );
}

// 8. The accept side carries the weight: a module that calls its own functions
//    must not be broken by the new scope.
#[test]
fn module_typecheck_08_a_module_calling_its_own_functions_is_accepted() {
    assert_module_output(
        &[
            (
                "lib.wi",
                "fn double(n: i64) -> i64 {
    return n * 2;
}

pub fn run() -> i64 {
    return double(21);
}
",
            ),
            ("app.wi", CALL_INTO_LIB),
        ],
        "42\n",
    );
}

// 9. Visibility is about what LEAVES a module. Its own private helper is in
//    scope for its own bodies.
#[test]
fn module_typecheck_09_a_private_helper_is_visible_inside_its_module() {
    assert_module_output(
        &[
            (
                "lib.wi",
                "fn secret() -> i64 {
    return 5;
}

pub fn run() -> i64 {
    return secret() + 1;
}
",
            ),
            ("app.wi", CALL_INTO_LIB),
        ],
        "6\n",
    );
}

// 10. The reason each module gets its own checker. `helper` is imported by the
//     ENTRY file; `lib` never imported it, so `helper_fn` is not a name `lib`
//     can write — and the backend, which resolves per module, agrees.
#[test]
fn module_typecheck_10_the_entry_files_imports_do_not_leak_into_a_module() {
    let stderr = module_error(&[
        ("helper.wi", "pub fn helper_fn() -> i64 { return 7; }\n"),
        (
            "lib.wi",
            "pub fn run() -> i64 {
    return helper_fn();
}
",
        ),
        (
            "app.wi",
            "import helper;
import lib;

fn main() {
    println(lib::run());
}
",
        ),
    ]);
    assert!(
        stderr.contains("error[E0350]") && stderr.contains("helper_fn"),
        "a module sees only its own imports:\n{stderr}"
    );
}

// 11. The other direction: what a module DOES import is in scope, under the
//     name that module chose.
#[test]
fn module_typecheck_11_a_module_can_use_the_module_it_imported() {
    assert_module_output(
        &[
            ("leaf.wi", "pub fn value() -> i64 { return 20; }\n"),
            (
                "lib.wi",
                "import leaf;

pub fn run() -> i64 {
    return leaf::value() + 2;
}
",
            ),
            ("app.wi", CALL_INTO_LIB),
        ],
        "22\n",
    );
}

// 12. An alias renames the module for the importing file only, so the module's
//     scope has to be built from its own import line, not from the entry's.
#[test]
fn module_typecheck_12_an_import_alias_inside_a_module_is_honoured() {
    assert_module_output(
        &[
            ("leaf.wi", "pub fn value() -> i64 { return 30; }\n"),
            (
                "lib.wi",
                "import leaf as source;

pub fn run() -> i64 {
    return source::value() + 3;
}
",
            ),
            ("app.wi", CALL_INTO_LIB),
        ],
        "33\n",
    );
}

// 13. A single-item import binds one name directly. The resolver records these
//     for the entry file only, so a module's are derived from its import lines.
#[test]
#[ignore = "blocked on willow-kxy8: a module's own single-item import is never registered with the backend, so the unqualified call lowers to a zero-returning stub. The TYPE side (what this perspective checks) already works; the value is wrong at runtime."]
fn module_typecheck_13_a_single_item_import_inside_a_module_is_honoured() {
    assert_module_output(
        &[
            ("leaf.wi", "pub fn value() -> i64 { return 40; }\n"),
            (
                "lib.wi",
                "import leaf::value;

pub fn run() -> i64 {
    return value() + 4;
}
",
            ),
            ("app.wi", CALL_INTO_LIB),
        ],
        "44\n",
    );
}

// 14. `register_std_imports` runs off the program being checked, so a module's
//     own `std` import is what its body sees.
#[test]
fn module_typecheck_14_a_modules_own_std_import_works() {
    assert_module_output(
        &[
            (
                "lib.wi",
                "import std::collections::Array;

pub fn run() -> i64 {
    let a: Array<i64> = [1, 2, 3];
    return a[0] + a[1] + a[2];
}
",
            ),
            ("app.wi", CALL_INTO_LIB),
        ],
        "6\n",
    );
}

// 15. The pass walks every module in the graph, not just the ones the entry
//     file names, because the backend compiles all of them.
#[test]
fn module_typecheck_15_a_transitively_imported_module_is_checked() {
    let stderr = module_error(&[
        (
            "deep.wi",
            "pub fn deep_value() -> i64 {
    let s: String = 1;
    return 2;
}
",
        ),
        (
            "lib.wi",
            "import deep;

pub fn run() -> i64 {
    return deep::deep_value();
}
",
        ),
        ("app.wi", CALL_INTO_LIB),
    ]);
    assert!(
        stderr.contains("deep.wi") && stderr.contains("error[E0201]"),
        "the entry file never mentions `deep`:\n{stderr}"
    );
}

// 16. Each module gets its own checker, and every checker's errors are merged
//     — one bad module does not hide another.
#[test]
fn module_typecheck_16_errors_in_two_modules_are_both_reported() {
    let stderr = module_error(&[
        (
            "one.wi",
            "pub fn a() -> i64 {
    let x: String = 1;
    return 1;
}
",
        ),
        (
            "two.wi",
            "pub fn b() -> i64 {
    let y: bool = \"nope\";
    return 2;
}
",
        ),
        (
            "app.wi",
            "import one;
import two;

fn main() {
    println(one::a() + two::b());
}
",
        ),
    ]);
    assert!(stderr.contains("one.wi"), "{stderr}");
    assert!(stderr.contains("two.wi"), "{stderr}");
}

// 17. The module graph holds one entry per file however many times it is
//     imported, so a shared dependency is not reported twice.
#[test]
fn module_typecheck_17_a_shared_module_is_checked_exactly_once() {
    let stderr = module_error(&[
        (
            "shared.wi",
            "pub fn shared_value() -> i64 {
    let s: String = 9;
    return 9;
}
",
        ),
        (
            "one.wi",
            "import shared;

pub fn a() -> i64 { return shared::shared_value(); }
",
        ),
        (
            "two.wi",
            "import shared;

pub fn b() -> i64 { return shared::shared_value(); }
",
        ),
        (
            "app.wi",
            "import one;
import two;

fn main() {
    println(one::a() + two::b());
}
",
        ),
    ]);
    assert_eq!(
        stderr.matches("error[E0201]").count(),
        1,
        "one diagnostic per bad line, not one per importer:\n{stderr}"
    );
}

// 18. A module error must stop the build. Reporting it and then compiling the
//     body anyway would be the old bug with extra output.
#[test]
fn module_typecheck_18_a_module_error_aborts_before_codegen() {
    let stderr = module_error(&[
        (
            "lib.wi",
            "pub fn run() -> i64 {
    let s: String = 42;
    return 1;
}
",
        ),
        ("app.wi", CALL_INTO_LIB),
    ]);
    assert!(
        stderr.contains("aborting due to"),
        "the module error has to be counted, not merely printed:\n{stderr}"
    );
}

// 19. The entry checker and the module checkers are separate; their diagnostics
//     land in one report.
#[test]
fn module_typecheck_19_entry_and_module_errors_appear_together() {
    let stderr = module_error(&[
        (
            "lib.wi",
            "pub fn run() -> i64 {
    let s: String = 42;
    return 1;
}
",
        ),
        (
            "app.wi",
            "import lib;

fn main() {
    let bad: bool = 7;
    println(lib::run());
}
",
        ),
    ]);
    assert!(stderr.contains("lib.wi"), "{stderr}");
    assert!(stderr.contains("app.wi"), "{stderr}");
}

// 20. The whole point of the accept cases: a correct multi-module program is
//     unaffected. Classes, methods, a module's own imports and std, all in a
//     module body.
#[test]
fn module_typecheck_20_a_correct_multi_module_program_still_runs() {
    assert_module_output(
        &[
            (
                "leaf.wi",
                "pub fn seed() -> i64 { return 7; }

pub class Point {
    pub x: i64;

    pub static fn new(x: i64) -> Point {
        return new Point(x);
    }

    pub fn get(self) -> i64 {
        return self.x;
    }
}
",
            ),
            (
                "lib.wi",
                "import leaf;
import std::collections::Array;

pub fn run() -> i64 {
    let p = leaf::Point::new(leaf::seed());
    let a: Array<i64> = [p.get(), 2];
    return a[0] * a[1];
}
",
            ),
            ("app.wi", CALL_INTO_LIB),
        ],
        "14\n",
    );
}

// 21. Conformance is a checker rule like any other, and it needs the module's
//     own interface registrations to run at all.
#[test]
fn module_typecheck_21_interface_conformance_is_checked_in_a_module() {
    let stderr = module_error(&[
        (
            "lib.wi",
            "pub interface Shape {
    fn area(self) -> i64;
}

pub class Square implements Shape {
    pub side: i64;

    pub static fn new(side: i64) -> Square {
        return new Square(side);
    }
}

pub fn run() -> i64 {
    return Square::new(3).side;
}
",
        ),
        ("app.wi", CALL_INTO_LIB),
    ]);
    assert!(
        stderr.contains("area") && stderr.contains("lib.wi"),
        "the unimplemented interface method belongs to the module:\n{stderr}"
    );
}

// 22. A `return` whose value does not match the declared type — the mismatch
//     the backend would otherwise paper over with a zero.
#[test]
fn module_typecheck_22_a_return_type_mismatch_is_rejected() {
    let stderr = module_error(&[
        (
            "lib.wi",
            "pub fn run() -> i64 {
    return \"not an integer\";
}
",
        ),
        ("app.wi", CALL_INTO_LIB),
    ]);
    assert!(
        stderr.contains("error[E0201]") && stderr.contains("lib.wi"),
        "{stderr}"
    );
}

// 23. Name resolution in a module body, not just types.
#[test]
fn module_typecheck_23_an_undefined_variable_is_rejected() {
    let stderr = module_error(&[
        (
            "lib.wi",
            "pub fn run() -> i64 {
    return undefined_local + 1;
}
",
        ),
        ("app.wi", CALL_INTO_LIB),
    ]);
    assert!(stderr.contains("undefined_local"), "{stderr}");
}

// 24. Visibility applies between modules exactly as it does from the entry
//     file: a module body cannot reach another module's private function.
#[test]
fn module_typecheck_24_a_module_cannot_call_another_modules_private_item() {
    let stderr = module_error(&[
        ("leaf.wi", "fn hidden() -> i64 { return 1; }\n"),
        (
            "lib.wi",
            "import leaf;

pub fn run() -> i64 {
    return leaf::hidden();
}
",
        ),
        ("app.wi", CALL_INTO_LIB),
    ]);
    assert!(
        stderr.contains("lib.wi") && (stderr.contains("private") || stderr.contains("E2006")),
        "{stderr}"
    );
}

// 25. Each module checker needs the same imported typed-receiver index as the
//     entry checker. Otherwise `w.heavy()` is invisible to the AST-only
//     concurrency analysis and bypasses E0810 inside an async module body.
#[test]
fn module_typecheck_25_imported_typed_receiver_reports_e0810() {
    let stderr = module_error(&[
        (
            "b.wi",
            "pub class Work {
    pub fn heavy(self) {
        while true {
        }
    }
}
",
        ),
        (
            "a.wi",
            "import b;

pub async fn run() {
    let w: b::Work = new b::Work();
    w.heavy();
}
",
        ),
        (
            "app.wi",
            "import a;

async fn main() {
    await a::run();
}
",
        ),
    ]);
    assert!(
        stderr.contains("error[E0810]")
            && stderr.contains("a.wi")
            && stderr.contains("b::Work::heavy"),
        "{stderr}"
    );
}

// 26. The missing-`&` insertion belongs to lib.wi, not FileId::ENTRY.
#[test]
fn module_typecheck_26_e1702_fix_points_into_the_module() {
    let stderr = module_error(&[
        (
            "lib.wi",
            "fn take(value: & i64) {
}

pub fn run() {
    let n = 1;
    take(n);
}
",
        ),
        ("app.wi", "import lib;\nfn main() { lib::run(); }\n"),
    ]);
    assert!(
        stderr.contains("error[E1702]")
            && stderr.contains("lib.wi")
            && stderr.contains("take(&n);"),
        "{stderr}"
    );
}

// 27. The `mut` insertion likewise belongs to the declaration's module file.
#[test]
fn module_typecheck_27_e1701_fix_points_into_the_module() {
    let stderr = module_error(&[
        (
            "lib.wi",
            "fn increment(value: &mut i64) {
}

pub fn run() {
    let n = 1;
    increment(&n);
}
",
        ),
        ("app.wi", "import lib;\nfn main() { lib::run(); }\n"),
    ]);
    assert!(
        stderr.contains("error[E1701]")
            && stderr.contains("lib.wi")
            && stderr.contains("let mut n = 1;"),
        "{stderr}"
    );
}
