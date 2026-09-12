//! Type-argument arity of written types, end to end (willow-rlq9).
//!
//! The checker-level perspectives live in
//! `src/semantic/type_checker/type_arity_tests.rs`. What these add is the
//! full pipeline: the shapes that used to pass the checker and die in code
//! generation (`Option<i64, i64>`, `Foo<i64>`, `Bar<i64>`) now stop at the
//! checker with a diagnostic and no "internal compiler error"; the bare
//! spelling from the original report (willow-53hg) is rejected across a module
//! boundary with a message that names the missing argument; and the
//! module-qualified spellings resolve to the same rule.
//!
//! Perspectives:
//!  1 `Option<i64, i64>` return: no ICE     2 `Result<i64>` return: no ICE
//!  3 `Foo<i64>` on a plain class: no ICE   4 `Bar<i64>` unknown head: no ICE
//!  5 willow-53hg: bare `Option` in a module fn
//!  6 bare `m::Wrap` from the importer     7 `m::Wrap<i64, i64>` from the importer
//!  8 bare `m::Conv` (generic interface)   9 `m::Color<i64>` on a plain module enum
//! 10 private generic `m::Hidden<i64>` is E0419
//! 11 the corrected program builds and runs (the example)
//! 12 the diagnostic carries code, label and help in the rendered output
//!
//! Declarations are judged once every declaration is registered, so a
//! signature or payload may name a type declared later, and a malformed
//! payload stops at the checker instead of in code generation:
//! 13 `enum Box { Value(Option<i64, i64>) }` + `Box::Empty`: no ICE
//! 14 interface before the enum it names builds and runs
//! 15 module interface naming `Self` and a later module enum, used from the importer
//! 16 module enum payload error is reported in the module
//! 17 entry payload naming a module generic with the wrong count
//! 18 payload forward reference builds and runs
//! 19 module class field naming a later generic enum: one identity
//! 20 module interface naming a later generic enum: conformance holds

use super::support::*;

const NO_ICE: &str = "internal compiler error";

#[track_caller]
fn assert_rejected_without_ice(source: &str, expected_parts: &[&str]) {
    let stderr = compile_error_stderr(source);
    assert!(!stderr.contains(NO_ICE), "should not ICE:\n{stderr}");
    for part in expected_parts {
        assert!(
            stderr.contains(part),
            "stderr did not contain `{part}`:\n{stderr}"
        );
    }
}

// 1
#[test]
fn p01_option_with_two_arguments_is_a_checker_error() {
    assert_rejected_without_ice(
        "fn f() -> Option<i64, i64> { return None; }\nfn main() { f(); }\n",
        &[
            "error[E0201]",
            "enum `Option` expects 1 type argument, but 2 were given",
        ],
    );
}

// 2
#[test]
fn p02_result_with_one_argument_is_a_checker_error() {
    assert_rejected_without_ice(
        "fn f() -> Result<i64> { return Ok(1); }\nfn main() { f(); }\n",
        &[
            "error[E0201]",
            "enum `Result` expects 2 type arguments, but 1 was given",
        ],
    );
}

// 3
#[test]
fn p03_arguments_on_a_plain_class_are_a_checker_error() {
    assert_rejected_without_ice(
        "class Foo { x: i64; pub init(self, x: i64) { self.x = x; } }\n\
         fn f(a: Foo<i64>) -> i64 { return 1; }\nfn main() {}\n",
        &[
            "error[E0201]",
            "class `Foo` is not generic, but 1 type argument was given",
        ],
    );
}

// 4
#[test]
fn p04_unknown_generic_head_is_a_checker_error() {
    assert_rejected_without_ice(
        "fn f(a: Bar<i64>) -> i64 { return 1; }\nfn main() {}\n",
        &["error[E0350]", "cannot find type `Bar`"],
    );
}

const WRAP_MODULE: &str = r#"
module m;

pub enum Wrap<T> {
    Val(T),
    Empty,
}

pub enum Color {
    Red,
    Blue,
}

enum Hidden<T> {
    Val(T),
}

pub interface Conv<T> {
    fn conv(self) -> T;
}

pub fn only_some(n: i64) -> Option { return Some(n); }
"#;

fn module_error(entry: &str) -> String {
    let files = [("m.wi", WRAP_MODULE), ("main.wi", entry)];
    let stderr = compile_temp_project_error_stderr(&files, "main.wi");
    assert!(!stderr.contains(NO_ICE), "should not ICE:\n{stderr}");
    stderr
}

// 5. The original report: a module function returning bare `Option`.
#[test]
fn p05_bare_option_in_a_module_function_names_the_missing_argument() {
    let stderr = module_error(
        "import m;\nfn main() { match m::only_some(5) { Some(v) => println(v), None => println(0) } }\n",
    );
    assert!(
        stderr.contains("enum `Option` expects 1 type argument, but none were given"),
        "{stderr}"
    );
    assert!(
        stderr.contains("help: write `Option<T>` with concrete types, e.g. `Option<i64>`"),
        "{stderr}"
    );
}

// 6
#[test]
fn p06_bare_module_generic_enum_from_the_importer() {
    let stderr = module_error("import m;\nfn f(w: m::Wrap) -> i64 { return 1; }\nfn main() {}\n");
    assert!(
        stderr.contains("enum `m::Wrap` expects 1 type argument, but none were given"),
        "{stderr}"
    );
}

// 7
#[test]
fn p07_module_generic_enum_with_too_many_arguments() {
    let stderr =
        module_error("import m;\nfn f(w: m::Wrap<i64, i64>) -> i64 { return 1; }\nfn main() {}\n");
    assert!(
        stderr.contains("enum `m::Wrap` expects 1 type argument, but 2 were given"),
        "{stderr}"
    );
    assert!(stderr.contains("help: write `m::Wrap<T>`"), "{stderr}");
}

// 8
#[test]
fn p08_bare_module_generic_interface() {
    let stderr = module_error("import m;\nfn f(c: m::Conv) -> i64 { return 1; }\nfn main() {}\n");
    assert!(
        stderr.contains("interface `m::Conv` expects 1 type argument, but none were given"),
        "{stderr}"
    );
}

// 9
#[test]
fn p09_arguments_on_a_plain_module_enum() {
    let stderr =
        module_error("import m;\nfn f(c: m::Color<i64>) -> i64 { return 1; }\nfn main() {}\n");
    assert!(
        stderr.contains("enum `m::Color` is not generic, but 1 type argument was given"),
        "{stderr}"
    );
}

// 10. A private generic is as private as a private plain type.
#[test]
fn p10_private_generic_enum_is_rejected_with_arguments_too() {
    let stderr =
        module_error("import m;\nfn f(h: m::Hidden<i64>) -> i64 { return 1; }\nfn main() {}\n");
    assert!(stderr.contains("error[E0419]"), "{stderr}");
    assert!(
        stderr.contains("enum `m::Hidden` is private to its module"),
        "{stderr}"
    );
}

// 11. The corrected program: every type argument in place, both backends.
#[test]
fn p11_the_example_builds_and_runs() {
    let (out, ok) = compile_file_and_run("example/type_argument_arity/main.wi");
    assert!(ok, "example failed to build or run: {out}");
    assert_eq!(out, "5\n0\n7\nempty\nnone\n2\n42\nmaybe\n1\n12\n");
}

// 12. The rendered diagnostic carries its code, label and help.
#[test]
fn p12_rendered_diagnostic_has_code_label_and_help() {
    assert_rejected_without_ice(
        "fn f(o: Option) -> i64 { return 1; }\nfn main() {}\n",
        &[
            "error[E0201]: enum `Option` expects 1 type argument, but none were given",
            "missing type arguments",
            "help: write `Option<T>` with concrete types, e.g. `Option<i64>`",
        ],
    );
}

// 13. The reviewer's reproduction: a malformed payload used to reach the
// walker, which refused `Box` as outside its subset.
#[test]
fn p13_malformed_enum_payload_is_a_checker_error() {
    assert_rejected_without_ice(
        "enum Box { Value(Option<i64, i64>), Empty }\nfn main() { let b = Box::Empty; }\n",
        &[
            "error[E0201]",
            "enum `Option` expects 1 type argument, but 2 were given",
            "help: write `Option<T>`",
        ],
    );
}

// 14. An interface may name an enum declared after it.
#[test]
fn p14_interface_declared_before_the_enum_it_names() {
    let (out, ok) = compile_and_run(
        "interface I { fn f(self, x: Wrap<i64>) -> i64; }\n\
         enum Wrap<T> { Value(T) }\n\
         class C implements I {\n\
             pub init(self) {}\n\
             pub fn f(self, x: Wrap<i64>) -> i64 { return match x { Wrap::Value(n) => n + 1 }; }\n\
         }\n\
         fn main() { let c = new C(); println(c.f(Wrap::Value(41))); }\n",
    );
    assert!(ok, "{out}");
    assert_eq!(out, "42\n");
}

const ORDERED_MODULE: &str = r#"
module m;

pub interface I {
    fn f(self, x: Wrap) -> i64;
    fn me(self) -> Self;
}

pub class C implements I {
    pub init(self) {}
    pub fn f(self, x: Wrap) -> i64 { return match x { Wrap::Value(n) => n }; }
    pub fn me(self) -> C { return new C(); }
}

pub enum Wrap { Value(i64) }

pub enum Holder<T> { Some(Wrap), Pair(T, Option<T>), Empty }

pub fn holder() -> Holder<i64> { return Holder::Pair(1, Some(2)); }
"#;

// 15. A module's interface may name `Self` and a later module enum, and the
// importer registers it without judging the module's spellings again.
#[test]
fn p15_module_interface_with_self_and_forward_reference_from_the_importer() {
    let files = [
        ("m.wi", ORDERED_MODULE),
        (
            "main.wi",
            "import m;\n\
             fn main() {\n\
                 let c = new m::C();\n\
                 println(c.me().f(m::Wrap::Value(3)));\n\
                 println(match m::holder() { m::Holder::Pair(a, _) => a, m::Holder::Some(_) => 0, m::Holder::Empty => 0 });\n\
             }\n",
        ),
    ];
    let (out, ok) = compile_temp_project_and_run(&files, "main.wi");
    assert!(ok, "{out}");
    assert_eq!(out, "3\n1\n");
}

// 16. A module's malformed payload is the module's error.
#[test]
fn p16_module_enum_payload_error_is_reported_in_the_module() {
    let files = [
        (
            "m.wi",
            "module m;\npub enum Holder<T> { Pair(T, Option<T, T>), Empty }\n",
        ),
        ("main.wi", "import m;\nfn main() {}\n"),
    ];
    let stderr = compile_temp_project_error_stderr(&files, "main.wi");
    assert!(!stderr.contains(NO_ICE), "should not ICE:\n{stderr}");
    assert!(stderr.contains("error[E0201]"), "{stderr}");
    assert!(
        stderr.contains("enum `Option` expects 1 type argument, but 2 were given"),
        "{stderr}"
    );
    assert!(stderr.contains("m.wi:2:"), "{stderr}");
}

// 17. An entry payload naming a module generic answers to the same rule, by
// the module-qualified name.
#[test]
fn p17_entry_payload_naming_a_module_generic_with_the_wrong_count() {
    let stderr = module_error(
        "import m;\nenum Box { Value(m::Wrap<i64, i64>), Empty }\nfn main() { let b = Box::Empty; }\n",
    );
    assert!(
        stderr.contains("enum `m::Wrap` expects 1 type argument, but 2 were given"),
        "{stderr}"
    );
}

// 18. A payload may name an enum declared after it.
#[test]
fn p18_enum_payload_forward_reference_builds_and_runs() {
    let (out, ok) = compile_and_run(
        "enum Later { A(Early), B(Option<Early>) }\n\
         enum Early { X, Y }\n\
         fn code(e: Early) -> i64 { return match e { Early::X => 1, Early::Y => 2 }; }\n\
         fn main() {\n\
             let o: Option<Early> = Some(Early::Y);\n\
             let l = Later::B(o);\n\
             let n = match l {\n\
                 Later::A(e) => code(e),\n\
                 Later::B(o) => match o { Some(e) => 10 + code(e), None => 0 },\n\
             };\n\
             println(n);\n\
         }\n",
    );
    assert!(ok, "{out}");
    assert_eq!(out, "12\n");
}

// 19. Inside a module, a class field written before the generic enum it
// names used to keep the bare spelling (`Wrap<i64>`) and mismatch every
// `m::Wrap<i64>` value; enum identities are now known before any
// declaration's types are normalized.
#[test]
fn p19_module_class_field_naming_a_later_generic_enum() {
    let files = [
        (
            "m.wi",
            "module m;\n\
             pub class Holder {\n\
                 w: Wrap<i64>;\n\
                 pub init(self) { self.w = Wrap::Value(3); }\n\
                 pub fn get(self) -> Wrap<i64> { return self.w; }\n\
                 pub fn set(self, w: Wrap<i64>) { self.w = w; }\n\
             }\n\
             pub enum Wrap<T> { Value(T), Empty }\n\
             pub enum Later { Holds(Wrap<i64>), Nothing }\n\
             pub fn unwrap(w: Wrap<i64>) -> i64 { return match w { Wrap::Value(n) => n, Wrap::Empty => 0 }; }\n\
             pub fn later(w: Wrap<i64>) -> Later { return Later::Holds(w); }\n\
             pub fn from_later(l: Later) -> i64 { return match l { Later::Holds(w) => unwrap(w), Later::Nothing => -1 }; }\n",
        ),
        (
            "main.wi",
            "import m;\n\
             fn main() {\n\
                 let h = new m::Holder();\n\
                 println(m::unwrap(h.get()));\n\
                 h.set(m::Wrap::Value(5));\n\
                 println(m::unwrap(h.get()));\n\
                 println(m::from_later(m::later(h.get())));\n\
             }\n",
        ),
    ];
    let (out, ok) = compile_temp_project_and_run(&files, "main.wi");
    assert!(ok, "{out}");
    assert_eq!(out, "3\n5\n5\n");
}

// 20. The same for an interface signature: the implementing class's
// `Wrap<i64>` and the interface's are the same type.
#[test]
fn p20_module_interface_naming_a_later_generic_enum() {
    let files = [
        (
            "m.wi",
            "module m;\n\
             pub interface I { fn f(self, x: Wrap<i64>) -> i64; }\n\
             pub class C implements I {\n\
                 pub init(self) {}\n\
                 pub fn f(self, x: Wrap<i64>) -> i64 { return match x { Wrap::Value(n) => n, Wrap::Empty => 0 }; }\n\
             }\n\
             pub enum Wrap<T> { Value(T), Empty }\n",
        ),
        (
            "main.wi",
            "import m;\n\
             fn main() { let c = new m::C(); println(c.f(m::Wrap::Value(3))); }\n",
        ),
    ];
    let (out, ok) = compile_temp_project_and_run(&files, "main.wi");
    assert!(ok, "{out}");
    assert_eq!(out, "3\n");
}

#[test]
fn module_type_parameters_shadow_enum_names_at_runtime() {
    let files = [
        (
            "m.wi",
            r#"
            module m;
            pub enum Wrap<T> { Val(T) }
            pub interface Identity<T> { fn get(self, x: T) -> T; }
            pub enum T { X }
            pub class C implements Identity<i64> {
                pub init(self) {}
                pub fn get(self, x: i64) -> i64 { return x; }
            }
            pub fn wrapped() -> Wrap<i64> { return Wrap::Val(42); }
            pub fn unwrap(w: Wrap<i64>) -> i64 {
                return match w { Wrap::Val(n) => n };
            }
            pub fn identity(c: Identity<i64>) -> i64 { return c.get(7); }
        "#,
        ),
        (
            "main.wi",
            r#"
            import m;
            fn main() {
                println(m::unwrap(m::wrapped()));
                println(m::identity(new m::C()));
            }
        "#,
        ),
    ];
    let (out, ok) = compile_temp_project_and_run(&files, "main.wi");
    assert!(ok, "{out}");
    assert_eq!(out, "42\n7\n");
}
