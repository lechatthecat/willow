//! Type-argument arity of written types (willow-rlq9).
//!
//! `validate_type` used to accept any head-plus-arguments shape as long as the
//! head named something. A bare generic (`Option`, `Wrap`, `Conv`) passed as an
//! uninstantiated `Type::Named`, so `Some(..)` failed to resolve against it and
//! the user read "cannot find function `Some`"; a wrong count
//! (`Option<i64, i64>`, `Result<i64>`) or arguments on a non-generic
//! (`Foo<i64>`) reached codegen, where the walker refused them as an internal
//! compiler error. Every annotation position now checks the count against the
//! declaration and reports one E0201 (E0350 for an unknown generic head).
//!
//! Perspectives:
//!  1 bare `Option` return type            2 bare `Result` return type
//!  3 `Option<i64, i64>` too many          4 `Result<i64>` too few
//!  5 bare `Option` parameter              6 bare `Option` let annotation
//!  7 let: the annotation error is first   8 bare `Option` class field
//!  9 nested in `Array<..>`               10 nested in `Map<..>`
//! 11 user generic enum bare              12 user generic enum too many
//! 13 non-generic enum given arguments    14 non-generic class given arguments
//! 15 unknown generic head                16 generic interface bare
//! 17 generic interface too many          18 non-generic interface given args
//! 19 every correct spelling is clean     20 help names the declared params
//! 21 labels name the defect              22 compiler-known generics untouched
//! 23 method signature positions          24 lambda parameter annotation
//! 25 constructor parameter               26 inside a `fn(..) -> ..` type
//! 27 exactly one diagnostic per site     28 two sites, two diagnostics
//! 29 `Option<Option>`: only the inner   30 label span is the annotation's
//! 31 `Result<T, E>` help lists both      32 class field: no Some/None cascade
//!
//! Declarations are judged after every declaration is registered, with their
//! own type parameters in scope, so a forward reference resolves and `T`
//! inside `Wrap<T>` is a parameter rather than an unknown type:
//! 33 interface names a later enum        34 interface names a later generic
//! 35 interface names a later class       36 enum payload names a later enum
//! 37 enum payload: too many arguments    38 enum payload: too few arguments
//! 39 enum payload: bare generic          40 enum payload: non-generic + args
//! 41 enum payload: unknown head          42 enum payload: unknown bare name
//! 43 payload uses own parameter          44 payload nests own parameter
//! 45 recursive payload `Wrap<T>`         46 parameter unknown outside decl
//! 47 non-generic enum cannot use `T`     48 generic interface: `T` is bound
//! 49 generic interface: wrong count      50 generic interface: unknown `U`
//! 51 `Self` in interface signature       52 payload label is the variant's
//! 53 one diagnostic per payload          54 module: `Self` + forward ref
//! 55 module: field before generic enum   56 module: payload before generic enum
//! 57 module: interface before generic enum, conformance holds

use crate::diagnostics::{Diagnostic, ErrorCode};
use crate::parser::ast::Type;

fn check(src: &str) -> Vec<Diagnostic> {
    let tokens = crate::lexer::Lexer::new(src).tokenize().expect("lex");
    let (program, parse_errors) = crate::parser::Parser::new(tokens).parse();
    assert!(parse_errors.is_empty(), "parse errors: {parse_errors:?}");
    let mut checker = crate::semantic::TypeChecker::new();
    crate::register_prelude(&mut checker).expect("prelude");
    checker.check_program(&program);
    checker.errors
}

fn ok(src: &str) {
    let d = check(src);
    assert!(d.is_empty(), "expected clean, got {d:?}");
}

/// The diagnostics whose message mentions type arguments, in report order.
fn arity_errors(src: &str) -> Vec<Diagnostic> {
    check(src)
        .into_iter()
        .filter(|d| d.message.contains("type argument"))
        .collect()
}

#[track_caller]
fn expect_one(src: &str, code: ErrorCode, message: &str) -> Diagnostic {
    let found = arity_errors(src);
    assert_eq!(found.len(), 1, "expected one arity error, got {found:?}");
    let d = found.into_iter().next().unwrap();
    assert_eq!(d.code, code, "{d:?}");
    assert_eq!(d.message, message, "{d:?}");
    d
}

const USER_TYPES: &str = "enum Wrap<T> { Val(T), Empty } \
    enum Color { Red, Blue } \
    class Foo { x: i64; pub init(self, x: i64) { self.x = x; } } \
    interface Conv<T> { fn conv(self) -> T; } \
    interface Shape { fn area(self) -> i64; } ";

// 1
#[test]
fn a01_bare_option_return_type() {
    expect_one(
        "fn f(n: i64) -> Option { return Some(n); } fn main() {}",
        ErrorCode::E0201,
        "enum `Option` expects 1 type argument, but none were given",
    );
}

// 2
#[test]
fn a02_bare_result_return_type() {
    expect_one(
        "fn f() -> Result { return Ok(1); } fn main() {}",
        ErrorCode::E0201,
        "enum `Result` expects 2 type arguments, but none were given",
    );
}

// 3
#[test]
fn a03_option_with_two_arguments() {
    expect_one(
        "fn f() -> Option<i64, i64> { return None; } fn main() {}",
        ErrorCode::E0201,
        "enum `Option` expects 1 type argument, but 2 were given",
    );
}

// 4
#[test]
fn a04_result_with_one_argument() {
    expect_one(
        "fn f() -> Result<i64> { return Ok(1); } fn main() {}",
        ErrorCode::E0201,
        "enum `Result` expects 2 type arguments, but 1 was given",
    );
}

// 5
#[test]
fn a05_bare_option_parameter() {
    expect_one(
        "fn f(o: Option) -> i64 { return 1; } fn main() {}",
        ErrorCode::E0201,
        "enum `Option` expects 1 type argument, but none were given",
    );
}

// 6
#[test]
fn a06_bare_option_let_annotation() {
    expect_one(
        "fn main() { let x: Option = None; }",
        ErrorCode::E0201,
        "enum `Option` expects 1 type argument, but none were given",
    );
}

// 7. The annotation is judged before the initializer, so the report that
// explains the cascade comes first.
#[test]
fn a07_let_annotation_error_precedes_its_cascade() {
    let all = check("fn main() { let x: Option = Some(1); }");
    assert!(all.len() >= 2, "{all:?}");
    assert!(
        all[0].message.contains("expects 1 type argument"),
        "first diagnostic should be the arity error: {all:?}"
    );
}

// 8. A class field used to accept `Option` silently.
#[test]
fn a08_bare_option_class_field() {
    expect_one(
        "class Slot { x: Option; pub init(self) { self.x = None; } } fn main() {}",
        ErrorCode::E0201,
        "enum `Option` expects 1 type argument, but none were given",
    );
}

// 9
#[test]
fn a09_bare_option_inside_array() {
    expect_one(
        "import std::collections::Array; fn f(a: Array<Option>) -> i64 { return 1; } fn main() {}",
        ErrorCode::E0201,
        "enum `Option` expects 1 type argument, but none were given",
    );
}

// 10
#[test]
fn a10_bare_option_inside_map() {
    expect_one(
        "import std::collections::Map; fn f(m: Map<i64, Option>) -> i64 { return 1; } fn main() {}",
        ErrorCode::E0201,
        "enum `Option` expects 1 type argument, but none were given",
    );
}

// 11
#[test]
fn a11_user_generic_enum_bare() {
    expect_one(
        &format!("{USER_TYPES} fn f(w: Wrap) -> i64 {{ return 1; }} fn main() {{}}"),
        ErrorCode::E0201,
        "enum `Wrap` expects 1 type argument, but none were given",
    );
}

// 12
#[test]
fn a12_user_generic_enum_too_many() {
    expect_one(
        &format!("{USER_TYPES} fn f(w: Wrap<i64, i64>) -> i64 {{ return 1; }} fn main() {{}}"),
        ErrorCode::E0201,
        "enum `Wrap` expects 1 type argument, but 2 were given",
    );
}

// 13
#[test]
fn a13_non_generic_enum_given_arguments() {
    expect_one(
        &format!("{USER_TYPES} fn f(c: Color<i64>) -> i64 {{ return 1; }} fn main() {{}}"),
        ErrorCode::E0201,
        "enum `Color` is not generic, but 1 type argument was given",
    );
}

// 14
#[test]
fn a14_non_generic_class_given_arguments() {
    expect_one(
        &format!("{USER_TYPES} fn f(x: Foo<i64, String>) -> i64 {{ return 1; }} fn main() {{}}"),
        ErrorCode::E0201,
        "class `Foo` is not generic, but 2 type arguments were given",
    );
}

// 15. An unknown head with arguments is the unknown-type error, not an ICE.
#[test]
fn a15_unknown_generic_head() {
    let all = check("fn f(x: Bar<i64>) -> i64 { return 1; } fn main() {}");
    assert_eq!(all.len(), 1, "{all:?}");
    assert_eq!(all[0].code, ErrorCode::E0350);
    assert_eq!(all[0].message, "cannot find type `Bar`");
}

// 16
#[test]
fn a16_generic_interface_bare() {
    expect_one(
        &format!("{USER_TYPES} fn f(c: Conv) -> i64 {{ return 1; }} fn main() {{}}"),
        ErrorCode::E0201,
        "interface `Conv` expects 1 type argument, but none were given",
    );
}

// 17
#[test]
fn a17_generic_interface_too_many() {
    expect_one(
        &format!("{USER_TYPES} fn f(c: Conv<i64, i64>) -> i64 {{ return 1; }} fn main() {{}}"),
        ErrorCode::E0201,
        "interface `Conv` expects 1 type argument, but 2 were given",
    );
}

// 18
#[test]
fn a18_non_generic_interface_given_arguments() {
    expect_one(
        &format!("{USER_TYPES} fn f(s: Shape<i64>) -> i64 {{ return 1; }} fn main() {{}}"),
        ErrorCode::E0201,
        "interface `Shape` is not generic, but 1 type argument was given",
    );
}

// 19. The correctly spelled counterparts of every rejected form stay clean.
#[test]
fn a19_correct_spellings_are_clean() {
    ok(&format!(
        "{USER_TYPES} \
         fn a(o: Option<i64>) -> Option<i64> {{ return o; }} \
         fn b(r: Result<i64, String>) -> Result<i64, String> {{ return r; }} \
         fn c(w: Wrap<i64>) -> Wrap<i64> {{ return w; }} \
         fn d(c: Color) -> Color {{ return c; }} \
         fn e(f: Foo) -> Foo {{ return f; }} \
         fn g(c: Conv<i64>) -> i64 {{ return c.conv(); }} \
         fn h(s: Shape) -> i64 {{ return s.area(); }} \
         fn main() {{ let x: Option<i64> = None; let y: Wrap<String> = Wrap::Empty; }}"
    ));
}

// 20. The help spells the declaration's own parameter names.
#[test]
fn a20_help_names_the_declared_parameters() {
    let d = expect_one(
        &format!("{USER_TYPES} fn f(w: Wrap) -> i64 {{ return 1; }} fn main() {{}}"),
        ErrorCode::E0201,
        "enum `Wrap` expects 1 type argument, but none were given",
    );
    assert_eq!(
        d.helps,
        vec!["write `Wrap<T>` with concrete types, e.g. `Wrap<i64>`".to_string()]
    );
    let d = expect_one(
        &format!("{USER_TYPES} fn f(w: Wrap<i64, i64>) -> i64 {{ return 1; }} fn main() {{}}"),
        ErrorCode::E0201,
        "enum `Wrap` expects 1 type argument, but 2 were given",
    );
    assert_eq!(d.helps, vec!["write `Wrap<T>`".to_string()]);
    let d = expect_one(
        &format!("{USER_TYPES} fn f(c: Color<i64>) -> i64 {{ return 1; }} fn main() {{}}"),
        ErrorCode::E0201,
        "enum `Color` is not generic, but 1 type argument was given",
    );
    assert_eq!(
        d.helps,
        vec!["write `Color` without type arguments".to_string()]
    );
}

// 21. Each shape has its own label.
#[test]
fn a21_labels_name_the_defect() {
    let label = |src: &str| {
        let d = arity_errors(src);
        assert_eq!(d.len(), 1, "{d:?}");
        d[0].labels[0].message.clone()
    };
    assert_eq!(
        label("fn f(o: Option) -> i64 { return 1; } fn main() {}"),
        "missing type arguments"
    );
    assert_eq!(
        label("fn f(o: Option<i64, i64>) -> i64 { return 1; } fn main() {}"),
        "wrong number of type arguments"
    );
    assert_eq!(
        label(&format!(
            "{USER_TYPES} fn f(c: Color<i64>) -> i64 {{ return 1; }} fn main() {{}}"
        )),
        "unexpected type arguments"
    );
}

// 22. Generic heads the compiler knows without a symbol entry are not judged
// here (their arity is the normalizer's or the backend's business).
#[test]
fn a22_compiler_known_generics_are_untouched() {
    ok(
        "fn f(t: Task<i64>, c: Channel<i64>, m: Mutex<i64>, r: RwLock<i64>, \
            b: BlockingCell<i64>, u: Future<void>) -> i64 { return 1; } \
        fn main() {}",
    );
}

// 23. Method return and parameter types.
#[test]
fn a23_method_signature_positions() {
    let d = arity_errors(
        "class K { pub fn m(self, o: Option) -> Result { return Ok(1); } } fn main() {}",
    );
    let messages: Vec<&str> = d.iter().map(|d| d.message.as_str()).collect();
    assert!(
        messages.contains(&"enum `Option` expects 1 type argument, but none were given"),
        "{messages:?}"
    );
    assert!(
        messages.contains(&"enum `Result` expects 2 type arguments, but none were given"),
        "{messages:?}"
    );
}

// 24
#[test]
fn a24_lambda_parameter_annotation() {
    let d = arity_errors("fn main() { let f = |o: Option| -> i64 { return 1; }; }");
    assert_eq!(d.len(), 1, "{d:?}");
    assert_eq!(
        d[0].message,
        "enum `Option` expects 1 type argument, but none were given"
    );
}

// 25
#[test]
fn a25_constructor_parameter() {
    expect_one(
        "class K { x: i64; pub init(self, o: Option) { self.x = 1; } } fn main() {}",
        ErrorCode::E0201,
        "enum `Option` expects 1 type argument, but none were given",
    );
}

// 26. A function type's parameter and return positions are walked too.
#[test]
fn a26_inside_a_function_type() {
    let d = arity_errors("fn f(g: fn(Option) -> Result<i64>) -> i64 { return 1; } fn main() {}");
    let messages: Vec<&str> = d.iter().map(|d| d.message.as_str()).collect();
    assert_eq!(
        messages,
        vec![
            "enum `Option` expects 1 type argument, but none were given",
            "enum `Result` expects 2 type arguments, but 1 was given",
        ]
    );
}

// 27. One site, one diagnostic: the return type is validated once.
#[test]
fn a27_one_diagnostic_per_site() {
    let d = arity_errors("fn f() -> Option<i64, i64> { return None; } fn main() {}");
    assert_eq!(d.len(), 1, "{d:?}");
}

// 28. Two sites, two diagnostics, in signature order.
#[test]
fn a28_two_sites_two_diagnostics() {
    let d = arity_errors("fn f(a: Option, b: Result<i64>) -> i64 { return 1; } fn main() {}");
    let messages: Vec<&str> = d.iter().map(|d| d.message.as_str()).collect();
    assert_eq!(
        messages,
        vec![
            "enum `Option` expects 1 type argument, but none were given",
            "enum `Result` expects 2 type arguments, but 1 was given",
        ]
    );
}

// 29. `Option<Option>`: the outer count is right, only the inner is reported.
#[test]
fn a29_nested_bare_generic_reports_only_the_inner() {
    let d = arity_errors("fn f(o: Option<Option>) -> i64 { return 1; } fn main() {}");
    assert_eq!(d.len(), 1, "{d:?}");
    assert_eq!(
        d[0].message,
        "enum `Option` expects 1 type argument, but none were given"
    );
}

// 30. The primary label sits on the annotated parameter.
#[test]
fn a30_label_span_is_the_annotation() {
    let src = "fn main() {}\nfn f(o: Option) -> i64 { return 1; }";
    let d = arity_errors(src);
    assert_eq!(d.len(), 1, "{d:?}");
    let span = d[0].labels[0].span;
    assert_eq!(span.line, 2, "{d:?}");
    assert_eq!(span.col, 6, "{d:?}");
}

// 31. `Result` lists both of its parameters.
#[test]
fn a31_result_help_lists_both_parameters() {
    let d = expect_one(
        "fn f() -> Result { return Ok(1); } fn main() {}",
        ErrorCode::E0201,
        "enum `Result` expects 2 type arguments, but none were given",
    );
    assert_eq!(
        d.helps,
        vec!["write `Result<T, E>` with concrete types, e.g. `Result<i64, i64>`".to_string()]
    );
    let d = expect_one(
        "fn f() -> Result<i64> { return Ok(1); } fn main() {}",
        ErrorCode::E0201,
        "enum `Result` expects 2 type arguments, but 1 was given",
    );
    assert_eq!(d.helps, vec!["write `Result<T, E>`".to_string()]);
}

// 32. A field with a bare generic type reports the field, and only the field:
// nothing constructs from it, so no `Some`/`None` cascade follows.
#[test]
fn a32_class_field_reports_exactly_once() {
    let all = check(
        "class Slot { x: Option; y: i64; pub init(self) { self.y = 1; self.x = None; } } fn main() {}",
    );
    let arity: Vec<&Diagnostic> = all
        .iter()
        .filter(|d| d.message.contains("type argument"))
        .collect();
    assert_eq!(arity.len(), 1, "{all:?}");
}

// 33. A non-generic interface may name an enum declared after it: signature
// types are judged once every declaration is registered.
#[test]
fn a33_interface_forward_reference_to_enum() {
    ok("interface I { fn f(self, x: Wrap) -> i64; } enum Wrap { Value(i64) } fn main() {}");
}

// 34. The same for a generic enum with arguments; the arity check needs the
// declaration, which is only there once registration has finished.
#[test]
fn a34_interface_forward_reference_to_generic_enum() {
    ok("interface I { fn f(self, x: Wrap<i64>) -> i64; } enum Wrap<T> { Value(T) } fn main() {}");
}

// 35. And for a class declared later.
#[test]
fn a35_interface_forward_reference_to_class() {
    ok("interface I { fn f(self, x: C) -> i64; } class C { pub init(self) {} } fn main() {}");
}

// 36. An enum payload may name an enum declared after it.
#[test]
fn a36_enum_payload_forward_reference() {
    ok("enum Later { A(Early), B(Option<Early>) } enum Early { X } fn main() {}");
}

// 37. A payload with too many arguments is the enum's defect, reported at the
// declaration -- not an internal compiler error once `Box::Empty` is lowered.
#[test]
fn a37_enum_payload_too_many_arguments() {
    let d = expect_one(
        "enum Box { Value(Option<i64, i64>), Empty } fn main() { let b = Box::Empty; }",
        ErrorCode::E0201,
        "enum `Option` expects 1 type argument, but 2 were given",
    );
    assert_eq!(d.helps, vec!["write `Option<T>`".to_string()]);
}

// 38
#[test]
fn a38_enum_payload_too_few_arguments() {
    expect_one(
        "enum Box { Value(Result<i64>), Empty } fn main() {}",
        ErrorCode::E0201,
        "enum `Result` expects 2 type arguments, but 1 was given",
    );
}

// 39
#[test]
fn a39_enum_payload_bare_generic() {
    expect_one(
        &format!("{USER_TYPES} enum Box {{ Value(Wrap), Empty }} fn main() {{}}"),
        ErrorCode::E0201,
        "enum `Wrap` expects 1 type argument, but none were given",
    );
}

// 40
#[test]
fn a40_enum_payload_non_generic_given_arguments() {
    expect_one(
        &format!("{USER_TYPES} enum Box {{ Value(Color<i64>), Empty }} fn main() {{}}"),
        ErrorCode::E0201,
        "enum `Color` is not generic, but 1 type argument was given",
    );
}

// 41
#[test]
fn a41_enum_payload_unknown_generic_head() {
    let all = check("enum Box { Value(Bogus<i64>), Empty } fn main() {}");
    assert_eq!(all.len(), 1, "{all:?}");
    assert_eq!(all[0].code, ErrorCode::E0350);
    assert_eq!(all[0].message, "cannot find type `Bogus`");
}

// 42. A bare unknown name in a payload was never reported at all.
#[test]
fn a42_enum_payload_unknown_bare_name() {
    let all = check("enum Box { Value(Bogus), Empty } fn main() {}");
    assert_eq!(all.len(), 1, "{all:?}");
    assert_eq!(all[0].code, ErrorCode::E0350);
    assert_eq!(all[0].message, "cannot find type `Bogus`");
}

// 43. The declaration's own parameter is a bound name inside its payloads.
#[test]
fn a43_enum_payload_uses_own_type_parameter() {
    ok("enum Wrap<T> { Val(T), Empty } fn main() {}");
}

// 44. Also when nested inside another generic.
#[test]
fn a44_enum_payload_nests_own_type_parameter() {
    ok("enum Wrap<T> { Val(Option<T>), Pair(T, Result<T, String>), Empty } fn main() {}");
}

// 45. A recursive payload instantiates the enum itself, with the right count.
#[test]
fn a45_enum_payload_recursive_instantiation() {
    ok("enum Wrap<T> { Val(T), Nested(Wrap<T>), Empty } fn main() {}");
    expect_one(
        "enum Wrap<T> { Val(T), Nested(Wrap<T, T>), Empty } fn main() {}",
        ErrorCode::E0201,
        "enum `Wrap` expects 1 type argument, but 2 were given",
    );
}

// 46. The parameter is bound by ITS declaration only: `T` in a function
// signature outside `Wrap<T>` is still an unknown type.
#[test]
fn a46_type_parameter_unknown_outside_its_declaration() {
    let all = check("enum Wrap<T> { Val(T) } fn f(t: T) -> i64 { return 1; } fn main() {}");
    assert_eq!(all.len(), 1, "{all:?}");
    assert_eq!(all[0].code, ErrorCode::E0350);
    assert_eq!(all[0].message, "cannot find type `T`");
}

// 47. A non-generic enum binds nothing, so `T` in its payload is unknown.
#[test]
fn a47_non_generic_enum_payload_cannot_use_t() {
    let all = check("enum Plain { A(T) } fn main() {}");
    assert_eq!(all.len(), 1, "{all:?}");
    assert_eq!(all[0].code, ErrorCode::E0350);
    assert_eq!(all[0].message, "cannot find type `T`");
}

// 48. A generic interface's signatures are validated too, with `T` bound;
// before, they were skipped wholesale.
#[test]
fn a48_generic_interface_signature_binds_its_parameter() {
    ok("enum Wrap<T> { Val(T) } \
        interface Conv<T> { fn conv(self) -> T; fn many(self) -> Wrap<T>; fn take(self, t: T) -> i64; } \
        fn main() {}");
}

// 49
#[test]
fn a49_generic_interface_signature_wrong_count() {
    expect_one(
        "enum Wrap<T> { Val(T) } interface Conv<T> { fn conv(self) -> Wrap<T, T>; } fn main() {}",
        ErrorCode::E0201,
        "enum `Wrap` expects 1 type argument, but 2 were given",
    );
    expect_one(
        "enum Wrap<T> { Val(T) } interface Conv<T> { fn conv(self) -> Wrap; } fn main() {}",
        ErrorCode::E0201,
        "enum `Wrap` expects 1 type argument, but none were given",
    );
}

// 50. Only the interface's own parameters are bound.
#[test]
fn a50_generic_interface_signature_unknown_parameter() {
    let all = check("interface Conv<T> { fn conv(self) -> U; } fn main() {}");
    assert_eq!(all.len(), 1, "{all:?}");
    assert_eq!(all[0].code, ErrorCode::E0350);
    assert_eq!(all[0].message, "cannot find type `U`");
}

// 51. `Self` is bound by every interface: conformance substitutes the
// receiver for it.
#[test]
fn a51_self_in_interface_signature() {
    ok("interface From<E> { fn from(self, e: E) -> Self; } fn main() {}");
    ok("interface Dup { fn dup(self) -> Self; } \
        class C implements Dup { pub init(self) {} pub fn dup(self) -> C { return new C(); } } \
        fn main() {}");
}

// 52. The payload diagnostic points at the variant that carries it.
#[test]
fn a52_enum_payload_label_is_the_variant_span() {
    let d = expect_one(
        "enum Box {\n    Ok(i64),\n    Value(Option<i64, i64>),\n    Empty\n}\nfn main() {}",
        ErrorCode::E0201,
        "enum `Option` expects 1 type argument, but 2 were given",
    );
    let label = d.labels.first().expect("primary label");
    assert_eq!((label.span.line, label.span.col), (3, 5), "{d:?}");
}

// 53. Each malformed payload is reported once, and the well-formed ones
// contribute nothing.
#[test]
fn a53_one_diagnostic_per_malformed_payload() {
    let found = arity_errors(
        "enum Box { A(Option<i64, i64>), B(Option<i64>), C(Result<i64>), D(i64, Option) } fn main() {}",
    );
    let messages: Vec<&str> = found.iter().map(|d| d.message.as_str()).collect();
    assert_eq!(
        messages,
        vec![
            "enum `Option` expects 1 type argument, but 2 were given",
            "enum `Result` expects 2 type arguments, but 1 was given",
            "enum `Option` expects 1 type argument, but none were given",
        ]
    );
}

// 54. A module's checker judges its own declarations the same way: a module
// interface may name `Self` and a later module enum.
#[test]
fn a54_module_declarations_are_judged_after_registration() {
    let src = "pub interface I { fn f(self, x: Wrap) -> i64; fn me(self) -> Self; } \
        pub enum Wrap { Value(i64) } \
        pub enum Holder<T> { Some(Wrap), Pair(T, Option<T>), Empty }";
    let checker = check_module(src);
    assert!(checker.errors.is_empty(), "{:?}", checker.errors);
}

/// Check `src` as the body of module `m`, the way `typecheck_phase` checks an
/// imported module: its enums carry the `m::` identity.
fn check_module(src: &str) -> crate::semantic::TypeChecker {
    let tokens = crate::lexer::Lexer::new(src).tokenize().expect("lex");
    let (program, parse_errors) = crate::parser::Parser::new(tokens).parse();
    assert!(parse_errors.is_empty(), "parse errors: {parse_errors:?}");
    let mut checker = crate::semantic::TypeChecker::new();
    crate::register_prelude(&mut checker).expect("prelude");
    checker.set_module_path("m");
    checker.check_module_program(&program);
    checker
}

// 55. A module's enum has the identity `m::Wrap`, and a class field written
// before the declaration used to keep the bare `Wrap<i64>` and mismatch it.
#[test]
fn a55_module_class_field_before_generic_enum_shares_its_identity() {
    let checker = check_module(
        "pub class Holder { w: Wrap<i64>; pub init(self) { self.w = Wrap::Empty; } \
            pub fn get(self) -> Wrap<i64> { return self.w; } \
            pub fn set(self, w: Wrap<i64>) { self.w = w; } } \
         pub enum Wrap<T> { Value(T), Empty }",
    );
    assert!(checker.errors.is_empty(), "{:?}", checker.errors);
    let field = &checker
        .symbols
        .lookup_class("Holder")
        .expect("Holder")
        .fields["w"]
        .ty;
    assert_eq!(
        *field,
        Type::Generic("m::Wrap".to_string(), vec![Type::I64])
    );
}

// 56. A payload written before the generic enum it names is canonical too.
#[test]
fn a56_module_payload_before_generic_enum_is_canonical() {
    let checker = check_module(
        "pub enum Later { Holds(Wrap<i64>), Nothing } pub enum Wrap<T> { Value(T), Empty }",
    );
    assert!(checker.errors.is_empty(), "{:?}", checker.errors);
    let later = checker.symbols.lookup_enum("Later").expect("Later");
    assert_eq!(later.name, "m::Later");
    assert_eq!(
        later.variants[0].payload_types,
        vec![Type::Generic("m::Wrap".to_string(), vec![Type::I64])]
    );
}

// 57. An interface signature written before the generic enum names the same
// type the implementing class's method does, so conformance holds.
#[test]
fn a57_module_interface_before_generic_enum_conformance_holds() {
    let checker = check_module(
        "pub interface I { fn f(self, x: Wrap<i64>) -> i64; } \
         pub class C implements I { pub init(self) {} \
            pub fn f(self, x: Wrap<i64>) -> i64 { return match x { Wrap::Value(n) => n, Wrap::Empty => 0 }; } } \
         pub enum Wrap<T> { Value(T), Empty }",
    );
    assert!(checker.errors.is_empty(), "{:?}", checker.errors);
}

#[test]
fn bound_parameters_shadow_module_enum_during_normalization() {
    for other in ["pub enum T { X }", "pub enum T<U> { X(U) }"] {
        let checker = check_module(&format!(
            "pub enum Wrap<T> {{ Val(T), Nested(Option<T>) }}              pub interface I<T> {{ fn f(self, x: T) -> Option<T>; fn me(self) -> Self; }}              {other} pub enum Self {{ X }}"
        ));
        assert!(checker.errors.is_empty(), "{:?}", checker.errors);
        let wrap = checker.symbols.lookup_enum("Wrap").unwrap();
        assert_eq!(
            wrap.variants[0].payload_types,
            vec![Type::Named("T".into())]
        );
        assert_eq!(
            wrap.variants[1].payload_types,
            vec![Type::Generic(
                "Option".into(),
                vec![Type::Named("T".into())]
            )]
        );
        let iface = checker.symbols.lookup_interface("I").unwrap();
        assert_eq!(iface.methods["f"].params, vec![Type::Named("T".into())]);
        assert_eq!(
            iface.methods["f"].return_type,
            Type::Generic("Option".into(), vec![Type::Named("T".into())])
        );
        assert_eq!(iface.methods["me"].return_type, Type::Named("Self".into()));
    }
}
