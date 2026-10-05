//! Match guards and `if let` / `while let` diagnostics (willow-jz15.6).
use super::{ErrorCode, check_source};
use crate::diagnostics::Severity;

const OPTION: &str = "enum Option<T> { Some(T), None }\n";

fn codes(source: &str) -> Vec<ErrorCode> {
    check_source(&format!("{OPTION}{source}"))
        .into_iter()
        .map(|d| d.code)
        .collect()
}

#[test]
fn guarded_arms_never_count_toward_exhaustiveness() {
    for (source, code) in [
        (
            "enum E { A, B } fn f(e: E) -> i64 { return match e { E::A => 1, E::B if true => 2 }; } fn main() {}",
            ErrorCode::E1202,
        ),
        (
            "fn f(b: bool) -> i64 { return match b { true => 1, false if b => 2 }; } fn main() {}",
            ErrorCode::E1207,
        ),
        (
            "fn f(n: i64) -> i64 { return match n { x if x > 0 => 1, 0 => 2 }; } fn main() {}",
            ErrorCode::E1206,
        ),
        (
            "fn f(n: i64) -> i64 { return match n { _ if n > 0 => 1, 0 => 2 }; } fn main() {}",
            ErrorCode::E1206,
        ),
        (
            "fn f(o: Option<i64>) -> i64 { return match o { Some(v) => v, None if true => 0 }; } fn main() {}",
            ErrorCode::E1202,
        ),
    ] {
        assert_eq!(codes(source), [code], "{source}");
    }
}

#[test]
fn guarded_arm_with_unguarded_fallback_is_exhaustive_and_reachable() {
    for source in [
        "fn f(n: i64) -> i64 { return match n { x if x > 0 => 1, _ => 2 }; } fn main() {}",
        "fn f(b: bool) -> i64 { return match b { true if b => 1, true => 2, false => 3 }; } fn main() {}",
        "fn f(o: Option<i64>) -> i64 { return match o { Some(v) if v > 1 => v, Some(v) => 0 - v, None => 0 }; } fn main() {}",
        // A guard may use the arm's bindings, `&&`, calls and a nested match.
        "fn ok(x: i64) -> bool { return x > 2; } fn f(n: i64) -> i64 { return match n { x if ok(x) && match x { 3 => false, _ => true } => 1, _ => 2 }; } fn main() {}",
    ] {
        assert!(codes(source).is_empty(), "{source}: {:?}", codes(source));
    }
}

#[test]
fn guard_must_be_bool_and_sees_only_arm_bindings() {
    let source = "fn f(n: i64) -> i64 { return match n { x if x => 1, _ => 2 }; } fn main() {}";
    let errors = check_source(&format!("{OPTION}{source}"));
    assert_eq!(errors.len(), 1, "{errors:?}");
    assert_eq!(errors[0].code, ErrorCode::E0203);
    assert!(
        errors[0]
            .message
            .contains("match guard must be `bool`, found `i64`")
    );

    // A sibling arm's binding is not in scope in a guard.
    let source = "fn f(n: i64) -> i64 { return match n { y if y > 9 => 1, x if y > 0 => 2, _ => 3 }; } fn main() {}";
    assert_eq!(codes(source), [ErrorCode::E0350], "{source}");
}

#[test]
fn guard_after_wildcard_is_unreachable() {
    let source = "fn f(n: i64) -> i64 { return match n { _ => 1, x if x > 0 => 2 }; } fn main() {}";
    let errors = check_source(&format!("{OPTION}{source}"));
    assert_eq!(errors.len(), 1, "{errors:?}");
    assert_eq!(errors[0].code, ErrorCode::W1201);
    assert_eq!(errors[0].severity, Severity::Warning);
    assert_eq!(errors[0].message, "unreachable match arm");
}

#[test]
fn irrefutable_if_let_and_while_let_warn_by_form() {
    for (source, message) in [
        (
            "fn main() { let n = 3; if let x = n { println(x); } }",
            "irrefutable `if let` pattern",
        ),
        (
            "fn main() { let n = 3; if let _ = n { println(n); } else { println(0); } }",
            "irrefutable `if let` pattern",
        ),
        (
            "fn main() { let n = 3; while let _ = n { break; } }",
            "irrefutable `while let` pattern",
        ),
    ] {
        let errors = check_source(&format!("{OPTION}{source}"));
        assert_eq!(errors.len(), 1, "{source}: {errors:?}");
        assert_eq!(errors[0].code, ErrorCode::W1201);
        assert_eq!(errors[0].severity, Severity::Warning);
        assert_eq!(errors[0].message, message, "{source}");
    }
}

#[test]
fn refutable_if_let_and_while_let_do_not_warn() {
    for source in [
        "fn main() { let o: Option<i64> = None; if let Some(x) = o { println(x); } }",
        "fn main() { let o: Option<i64> = None; if let None = o { println(0); } else if let Some(x) = o { println(x); } }",
        "fn main() { let o: Option<i64> = None; while let Some(x) = o { println(x); break; } }",
        "fn main() { let b = true; if let true = b { println(1); } }",
        "fn main() { let n = 4; if let 4 = n { println(1); } }",
    ] {
        assert!(codes(source).is_empty(), "{source}: {:?}", codes(source));
    }
}

#[test]
fn if_let_bindings_are_scoped_to_the_then_block() {
    for source in [
        "fn main() { let o: Option<i64> = None; if let Some(x) = o { println(x); } println(x); }",
        "fn main() { let o: Option<i64> = None; if let Some(x) = o { println(x); } else { println(x); } }",
        "fn main() { let o: Option<i64> = None; while let Some(x) = o { break; } println(x); }",
    ] {
        assert_eq!(codes(source), [ErrorCode::E0350], "{source}");
    }
}

#[test]
fn if_let_and_while_let_in_definite_assignment_and_return_analysis() {
    // The fallback branch must initialize the field too.
    let partial = "class C { pub v: i64; pub init(self, o: Option<i64>) { if let Some(x) = o { self.v = x; } } } fn main() {}";
    assert_eq!(codes(partial), [ErrorCode::E0842]);
    let total = "class C { pub v: i64; pub init(self, o: Option<i64>) { if let Some(x) = o { self.v = x; } else { self.v = 0; } } } fn main() {}";
    assert!(codes(total).is_empty(), "{:?}", codes(total));

    // `while let` can exit without returning, so a value is still required.
    let loop_only =
        "fn f(o: Option<i64>) -> i64 { while let Some(x) = o { return x; } } fn main() {}";
    assert_eq!(codes(loop_only), [ErrorCode::E0205]);
    // `if let ... else` returning on both branches is a complete return.
    let both = "fn f(o: Option<i64>) -> i64 { if let Some(x) = o { return x; } else { return 0; } } fn main() {}";
    assert!(codes(both).is_empty(), "{:?}", codes(both));
}

#[test]
fn if_let_pattern_type_errors_are_reported() {
    for (source, code) in [
        (
            "fn main() { let n = 3; if let true = n { println(n); } }",
            ErrorCode::E1205,
        ),
        (
            "fn main() { let o: Option<i64> = None; if let Option::Nope(x) = o { println(x); } }",
            ErrorCode::E1208,
        ),
        (
            "fn main() { let o: Option<i64> = None; while let Some(a, b) = o { break; } }",
            ErrorCode::E1209,
        ),
    ] {
        assert!(
            codes(source).contains(&code),
            "{source}: {:?}",
            codes(source)
        );
    }
}
