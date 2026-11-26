//! `f64::floorr(x)` names the missing member of a known primitive type, and
//! `x.trunc_to_i64()` points to the static (or free) function that exists.
use crate::diagnostics::{Diagnostic, ErrorCode, Severity};

fn check(body: &str) -> Vec<Diagnostic> {
    let source = format!("fn main() {{ let x = 2.5; let n = 3; {body} }}");
    let tokens = crate::lexer::Lexer::new(&source).tokenize().expect("lex");
    let (program, parse_errors) = crate::parser::Parser::new(tokens).parse();
    assert!(parse_errors.is_empty(), "{source}: {parse_errors:?}");
    let mut checker = crate::semantic::TypeChecker::new();
    crate::register_prelude(&mut checker).expect("prelude");
    checker.check_program(&program);
    checker
        .errors
        .into_iter()
        .filter(|d| d.severity == Severity::Error)
        .collect()
}

fn single(body: &str) -> Diagnostic {
    let mut errors = check(body);
    assert_eq!(errors.len(), 1, "{body}: {errors:?}");
    errors.remove(0)
}

#[test]
fn unknown_static_on_a_primitive_names_the_member() {
    for (name, body, message, help, note) in [
        (
            "f64 typo",
            "println(f64::floorr(1.0));",
            "no static function `floorr` on `f64`",
            Some("did you mean `f64::floor`?"),
            "static functions of `f64`: abs, min, max, clamp, sqrt, floor",
        ),
        (
            "i64 typo",
            "println(i64::maxx(1, 2));",
            "no static function `maxx` on `i64`",
            Some("did you mean `i64::max`?"),
            "static functions of `i64`: abs, min, max, clamp, parse",
        ),
        (
            "conversion typo",
            "println(f64::trunc_to_i62(x));",
            "no static function `trunc_to_i62` on `f64`",
            Some("did you mean `f64::trunc_to_i64`?"),
            "trunc_to_i64",
        ),
        (
            "free pow",
            "println(f64::pow(2.0, x));",
            "no static function `pow` on `f64`",
            Some("`pow` is a free function, not a static function of `f64`: write `pow(2.0, x)`"),
            "static functions of `f64`",
        ),
        (
            "free powf",
            "println(f64::powf(2.0, 0.5));",
            "no static function `powf` on `f64`",
            Some("write `powf(2.0, 0.5)`"),
            "static functions of `f64`",
        ),
        (
            "free function with unquotable arguments",
            "println(f64::pow(x + 1.0, 2.0));",
            "no static function `pow` on `f64`",
            Some("write `pow(...)`"),
            "static functions of `f64`",
        ),
        (
            "no near match",
            "println(f64::frobnicate(1.0));",
            "no static function `frobnicate` on `f64`",
            None,
            "static functions of `f64`",
        ),
        (
            "i64 is not offered pow",
            "println(i64::pow(2, 3));",
            "no static function `pow` on `i64`",
            None,
            "static functions of `i64`",
        ),
        (
            "String",
            "println(String::nope(\"a\"));",
            "no static function `nope` on `String`",
            None,
            "`String` has no static functions",
        ),
        (
            "u8",
            "println(u8::max(1, 2));",
            "no static function `max` on `u8`",
            None,
            "`u8` has no static functions",
        ),
    ] {
        let error = single(body);
        assert_eq!(error.code, ErrorCode::E0502, "{name}: {error:?}");
        assert_eq!(error.message, message, "{name}");
        let helps = error.helps.join("\n");
        match help {
            Some(help) => assert!(helps.contains(help), "{name}: {helps}"),
            None => assert!(helps.is_empty(), "{name}: {helps}"),
        }
        assert!(
            error.notes.iter().any(|n| n.contains(note)),
            "{name}: {:?}",
            error.notes
        );
    }
}

#[test]
fn unknown_static_does_not_cascade() {
    let errors = check("let y = f64::floorr(x); let z: f64 = y + 1.0; println(z);");
    assert_eq!(errors.len(), 1, "{errors:?}");
}

#[test]
fn unknown_type_name_keeps_the_module_or_class_error() {
    let error = single("println(Nope::f(1));");
    assert_eq!(error.code, ErrorCode::E0350, "{error:?}");
    assert!(error.message.contains("not a module or class"), "{error:?}");
}

#[test]
fn method_call_on_a_primitive_points_to_the_function() {
    for (name, body, help) in [
        (
            "conversion",
            "println(x.trunc_to_i64());",
            "`trunc_to_i64` is a static function, not a method: write `f64::trunc_to_i64(x)`",
        ),
        ("math", "println(x.sqrt());", "write `f64::sqrt(x)`"),
        (
            "extra arguments follow the receiver",
            "println(n.max(4));",
            "write `i64::max(n, 4)`",
        ),
        (
            "three arguments",
            "println(x.clamp(0.0, 1.0));",
            "write `f64::clamp(x, 0.0, 1.0)`",
        ),
        (
            "other owner type",
            "println(n.from_i64());",
            "write `f64::from_i64(n)`",
        ),
        (
            "free function",
            "println(x.powf(2.0));",
            "`powf` is a free function, not a method: write `powf(x, 2.0)`",
        ),
        (
            "near static name",
            "println(x.floorr());",
            "did you mean the static function `f64::floor`? write `f64::floor(x)`",
        ),
        (
            "unquotable receiver",
            "println((x + 1.0).sqrt());",
            "write `f64::sqrt(value, ...)` with the receiver as the first argument",
        ),
        (
            "field receiver",
            "let xs = [x]; println(xs[0].round());",
            "write `f64::round(xs[0])`",
        ),
    ] {
        let error = single(body);
        assert_eq!(error.code, ErrorCode::E0201, "{name}: {error:?}");
        let helps = error.helps.join("\n");
        assert!(helps.contains(help), "{name}: {helps}");
        assert!(helps.contains("available methods"), "{name}: {helps}");
    }
}

#[test]
fn unrelated_primitive_methods_get_no_function_hint() {
    for body in [
        "println(x.frobnicate());",
        "println(n.trunc_to_i64());",
        "println(n.sqrt());",
        "println(n.powf(2.0));",
        "println(\"s\".sqrt());",
    ] {
        let error = single(body);
        let helps = error.helps.join("\n");
        assert!(
            !helps.contains("static function") && !helps.contains("free function"),
            "{body}: {helps}"
        );
    }
}

#[test]
fn suggested_calls_type_check() {
    let errors = check(
        "println(f64::floor(1.0)); println(i64::max(1, 2)); println(pow(2.0, x)); println(powf(2.0, 0.5));
println(f64::trunc_to_i64(x)); println(f64::sqrt(x)); println(i64::max(n, 4)); println(f64::clamp(x, 0.0, 1.0));
println(f64::from_i64(n)); println(powf(x, 2.0)); let xs = [x]; println(f64::round(xs[0]));",
    );
    assert!(errors.is_empty(), "{errors:?}");
}

#[test]
fn primitive_static_table_matches_the_intrinsics() {
    use crate::semantic::ids::SemanticType as T;
    for (owner, name, first) in super::resolve::PRIMITIVE_STATICS {
        let entry = crate::semantic::intrinsics::namespace_builtin(owner, name)
            .unwrap_or_else(|| panic!("{owner}::{name} is not a builtin"));
        let expected = match first {
            "i64" => T::I64,
            "f64" => T::F64,
            "String" => T::String,
            other => panic!("{other}"),
        };
        assert_eq!(entry.params[0], expected, "{owner}::{name}");
    }
}
