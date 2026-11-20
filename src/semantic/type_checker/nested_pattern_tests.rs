//! Checker perspectives for nested patterns, let chains and omitted enum
//! names: exhaustiveness and reachability through nesting, binding types,
//! diagnostics, and the scaling of the usefulness check.
use super::exhaustiveness::USEFULNESS_OPS;
use crate::diagnostics::{Diagnostic, ErrorCode, Severity};

fn check_source(source: &str) -> Vec<Diagnostic> {
    let tokens = crate::lexer::Lexer::new(source).tokenize().expect("lex");
    let (program, parse_errors) = crate::parser::Parser::new(tokens).parse();
    assert!(parse_errors.is_empty(), "parse errors: {parse_errors:?}");
    let mut checker = crate::semantic::TypeChecker::new();
    crate::register_prelude(&mut checker).expect("prelude");
    checker.check_program(&program);
    checker.errors
}

const DECLS: &str = "enum Shape { Circle(i64), Rect(i64, i64) }
enum Color { Red, Green, Blue }
enum Tagged { Pair(Shape, Color), Named(String, Option<i64>) }
";

fn diagnostics(body: &str) -> Vec<Diagnostic> {
    check_source(&format!("{DECLS}{body}\nfn main() {{}}"))
}

fn codes(body: &str) -> Vec<ErrorCode> {
    diagnostics(body).into_iter().map(|d| d.code).collect()
}

fn assert_clean(body: &str) {
    let found = diagnostics(body);
    assert!(found.is_empty(), "{body}\n{found:?}");
}

fn assert_only(body: &str, code: ErrorCode, message_part: &str) {
    let found = diagnostics(body);
    assert_eq!(found.len(), 1, "{body}\n{found:?}");
    assert_eq!(found[0].code, code, "{body}\n{found:?}");
    assert!(
        found[0].message.contains(message_part),
        "{body}: `{}` lacks `{message_part}`",
        found[0].message
    );
}

#[test]
fn exhaustive_nested_matches_are_clean() {
    for body in [
        "fn f(r: Result<Option<i64>, String>) -> i64 { return match r { Ok(Some(n)) => n, Ok(None) => 0, Err(_) => -1 }; }",
        "fn f(t: Tagged) -> i64 { return match t { Tagged::Pair(Shape::Circle(_), _) => 1, Tagged::Pair(Shape::Rect(_, _), Color::Red) => 2, Tagged::Pair(_, _) => 3, Tagged::Named(_, Some(_)) => 4, Tagged::Named(_, None) => 5 }; }",
        "fn f(p: (Option<i64>, Option<i64>)) -> i64 { return match p { (Some(a), Some(b)) => a + b, (Some(a), None) => a, (None, _) => 0 }; }",
        "fn f(o: Option<Option<Option<i64>>>) -> i64 { return match o { Some(Some(Some(v))) => v, Some(Some(None)) => 2, Some(None) => 1, None => 0 }; }",
        // Literals inside payloads: both bools cover, integers need a fallback.
        "fn f(o: Option<bool>) -> i64 { return match o { Some(true) => 1, Some(false) => 0, None => -1 }; }",
        "fn f(o: Option<i64>) -> i64 { return match o { Some(0) => 0, Some(-1) => 1, Some(n) => n, None => -1 }; }",
        // Omitted enum names inside a nested position.
        "fn f(o: Option<Shape>) -> i64 { return match o { Some(Circle(r)) => r, Some(Rect(w, h)) => w * h, None => 0 }; }",
    ] {
        assert_clean(body);
    }
}

#[test]
fn missing_nested_values_report_a_witness() {
    for (body, witness) in [
        (
            "fn f(r: Result<Option<i64>, String>) -> i64 { return match r { Ok(Some(n)) => n, Err(_) => -1 }; }",
            "`Result::Ok(Option::None)` not covered",
        ),
        (
            "fn f(t: Tagged) -> i64 { return match t { Tagged::Pair(Shape::Circle(_), _) => 1, Tagged::Pair(Shape::Rect(_, _), Color::Red) => 2, Tagged::Pair(_, Color::Blue) => 3, Tagged::Named(_, _) => 4 }; }",
            "`Tagged::Pair(Shape::Rect(_, _), Color::Green)` not covered",
        ),
        (
            "fn f(o: Option<bool>) -> i64 { return match o { Some(true) => 1, None => -1 }; }",
            "`Option::Some(false)` not covered",
        ),
        (
            "fn f(o: Option<Option<Option<i64>>>) -> i64 { return match o { Some(Some(Some(v))) => v, Some(None) => 1, None => 0 }; }",
            "`Option::Some(Option::Some(Option::None))` not covered",
        ),
    ] {
        assert_only(body, ErrorCode::E1202, witness);
    }
    // A top-level variant no arm names keeps the variant wording.
    assert_only(
        "fn f(r: Result<Option<i64>, String>) -> i64 { return match r { Ok(Some(n)) => n, Ok(None) => 0 }; }",
        ErrorCode::E1202,
        "variant `Result::Err` not covered",
    );
    // An integer literal payload needs a catch-all for the other integers.
    assert_only(
        "fn f(o: Option<i64>) -> i64 { return match o { Some(0) => 0, None => -1 }; }",
        ErrorCode::E1202,
        "`Option::Some(_)` not covered",
    );
    // A guarded arm adds nothing; the unguarded `Some(0)` still names `Some`.
    assert_only(
        "fn f(o: Option<i64>) -> i64 { return match o { Some(0) => 0, Some(n) if n > 0 => n, None => -1 }; }",
        ErrorCode::E1202,
        "`Option::Some(_)` not covered",
    );
}

#[test]
fn unreachable_nested_arms_warn() {
    for body in [
        "fn f(r: Result<Option<i64>, String>) -> i64 { return match r { Ok(_) => 1, Ok(Some(n)) => n, Err(_) => -1 }; }",
        "fn f(o: Option<Option<i64>>) -> i64 { return match o { Some(Some(v)) => v, Some(None) => 1, None => 0, Some(Some(3)) => 3 }; }",
        "fn f(p: (Option<i64>, Color)) -> i64 { return match p { (_, Color::Red) => 1, (None, _) => 2, (Some(_), _) => 3, (None, Color::Red) => 4 }; }",
        "fn f(o: Option<bool>) -> i64 { return match o { Some(true) => 1, Some(false) => 0, None => -1, Some(_) => 2 }; }",
    ] {
        let found = diagnostics(body);
        assert_eq!(found.len(), 1, "{body}\n{found:?}");
        assert_eq!(found[0].code, ErrorCode::W1201, "{body}");
        assert_eq!(found[0].severity, Severity::Warning, "{body}");
    }
}

#[test]
fn guarded_nested_arms_never_cover_and_stay_reachable() {
    assert_only(
        "fn f(o: Option<Option<i64>>) -> i64 { return match o { Some(Some(v)) if v > 0 => v, Some(None) => 1, None => 0 }; }",
        ErrorCode::E1202,
        "`Option::Some(Option::Some(_))` not covered",
    );
    assert_clean(
        "fn f(o: Option<Option<i64>>) -> i64 { return match o { Some(Some(v)) if v > 0 => v, Some(Some(v)) => 0 - v, Some(None) => 1, None => 0 }; }",
    );
    // A guard `let` step is a refutable guard too.
    assert_only(
        "fn f(o: Option<i64>, p: Option<i64>) -> i64 { return match o { Some(v) if let Some(w) = p => v + w, None => 0 }; }",
        ErrorCode::E1202,
        "variant `Option::Some` not covered",
    );
}

#[test]
fn nested_bindings_get_payload_types() {
    // `name` is a String: using it as an integer is a type error.
    let found = codes(
        "fn f(t: Tagged) -> i64 { return match t { Tagged::Named(name, Some(v)) => name + v, _ => 0 }; }",
    );
    assert!(!found.is_empty(), "String + i64 must not type-check");
    assert_clean(
        "fn f(t: Tagged) -> String { return match t { Tagged::Named(name, Some(v)) => name + v.to_string(), _ => \"\" }; }",
    );
    // Generic payloads are substituted before nested patterns are typed.
    assert_clean(
        "enum Pair<T> { Both(T, T), Zero } fn f(p: Pair<Option<String>>) -> String { return match p { Pair::Both(Some(a), Some(b)) => a + b, Pair::Both(_, _) => \"\", Pair::Zero => \"0\" }; }",
    );
    // Tuple fields bound through a let chain keep their types.
    let found = codes(
        "fn f(p: Option<(String, i64)>) -> i64 { if let Some((s, n)) = p && n > 0 { return s; } return 0; }",
    );
    assert!(!found.is_empty(), "returning a String as i64 must fail");
}

#[test]
fn pattern_shape_errors_are_reported_once() {
    // Duplicate binding inside one nested pattern.
    assert_eq!(
        codes(
            "fn f(p: Option<(i64, i64)>) -> i64 { return match p { Some((a, a)) => a, None => 0 }; }"
        ),
        [ErrorCode::E1210]
    );
    // Duplicate binding in a tuple `let` and in a let-chain step.
    assert_eq!(
        codes("fn f() -> i64 { let (a, a) = (1, 2); return a; }"),
        [ErrorCode::E1210]
    );
    assert_eq!(
        codes(
            "fn f(o: Option<(i64, i64)>) -> i64 { if let Some((b, b)) = o { return b; } return 0; }"
        ),
        [ErrorCode::E1210]
    );
    // Unknown variant inside a nested payload.
    assert_eq!(
        codes(
            "fn f(o: Option<Shape>) -> i64 { return match o { Some(Shape::Square(_)) => 1, _ => 0 }; }"
        ),
        [ErrorCode::E1208]
    );
    // Wrong payload arity inside a nested payload.
    assert_eq!(
        codes(
            "fn f(o: Option<Shape>) -> i64 { return match o { Some(Shape::Rect(w)) => w, _ => 0 }; }"
        ),
        [ErrorCode::E1209]
    );
    // A bare variant on a payload that is no enum.
    assert_eq!(
        codes("fn f(o: Option<i64>) -> i64 { return match o { Some(Circle(r)) => r, _ => 0 }; }"),
        [ErrorCode::E1205]
    );
}

#[test]
fn let_chain_scoping_and_conditions() {
    assert_clean(
        "fn f(a: Option<i64>, b: Result<i64, String>) -> i64 { if let Some(x) = a && let Ok(y) = b && x < y { return x + y; } return 0; }",
    );
    // Chain bindings are not visible in the else block.
    let found = codes(
        "fn f(a: Option<i64>) -> i64 { if let Some(x) = a && x > 0 { return x; } else { return x; } }",
    );
    assert!(!found.is_empty(), "chain binding leaked into else");
    // A non-bool condition step is E0203.
    assert_eq!(
        codes("fn f(a: Option<i64>) -> i64 { if let Some(x) = a && x { return x; } return 0; }"),
        [ErrorCode::E0203]
    );
    // A leading condition is a condition step too: E0203, not a pattern error.
    for body in [
        "fn f(n: i64, o: Option<i64>) -> i64 { if n && let Some(x) = o { return x; } return 0; }",
        "fn f(n: i64, o: Option<i64>) -> i64 { while n && let Some(x) = o { return x; } return 0; }",
    ] {
        assert_eq!(codes(body), [ErrorCode::E0203], "{body}");
    }
    assert_clean(
        "fn f(n: i64, o: Option<i64>) -> i64 { if n > 0 && let Some(x) = o && x < n { return x; } return 0; }",
    );
    // A later step sees an earlier binding.
    assert_clean(
        "fn f(a: Option<Option<i64>>) -> i64 { if let Some(inner) = a && let Some(v) = inner { return v; } return 0; }",
    );
}

#[test]
fn let_else_and_tuple_let_rules() {
    assert_clean(
        "fn f(o: Option<Option<i64>>) -> i64 { let Some(Some(v)) = o else { return -1; }; return v; }",
    );
    assert_eq!(
        codes(
            "fn f(o: Option<i64>) -> i64 { let Some(v) = o else { println(\"x\"); }; return v; }"
        ),
        [ErrorCode::E1212]
    );
    assert_only(
        "fn f(p: (i64, Option<i64>)) -> i64 { let (a, None) = p; return a; }",
        ErrorCode::E1213,
        "`(_, Option::Some(_))` not covered",
    );
    assert_clean("fn f(p: (i64, (String, i64))) -> i64 { let (a, (_, b)) = p; return a + b; }");
    // An irrefutable let-else pattern warns.
    let found = diagnostics(
        "fn f(p: (i64, i64)) -> i64 { let (a, b) = p else { return 0; }; return a + b; }",
    );
    assert_eq!(found.len(), 1, "{found:?}");
    assert_eq!(found[0].severity, Severity::Warning);
}

#[test]
fn nested_interface_downcasts_need_a_fallback() {
    let decls = "interface Animal { fn legs(self) -> i64; } class Dog implements Animal { pub n: i64; pub fn legs(self) -> i64 { return 4; } }";
    assert_only(
        &format!(
            "{decls} fn f(o: Option<Animal>) -> i64 {{ return match o {{ Some(Dog(d)) => d.n, None => 0 }}; }}"
        ),
        ErrorCode::E1202,
        "`Option::Some(_)` not covered",
    );
    assert_clean(&format!(
        "{decls} fn f(o: Option<Animal>) -> i64 {{ return match o {{ Some(Dog(d)) => d.n, Some(_) => 1, None => 0 }}; }}"
    ));
}

fn usefulness_ops(body: &str) -> usize {
    USEFULNESS_OPS.with(|ops| ops.set(0));
    let found = diagnostics(body);
    assert!(found.is_empty(), "{body}\n{found:?}");
    USEFULNESS_OPS.with(|ops| ops.get())
}

/// `Option` nested `depth` times, matched by one arm per level.
fn deep_option_match(depth: usize) -> String {
    let mut ty = "i64".to_string();
    for _ in 0..depth {
        ty = format!("Option<{ty}>");
    }
    let mut arms = Vec::new();
    for level in 0..=depth {
        let mut pattern = if level == depth {
            "v".to_string()
        } else {
            "None".to_string()
        };
        for _ in 0..level {
            pattern = format!("Some({pattern})");
        }
        let value = if level == depth {
            "v".to_string()
        } else {
            level.to_string()
        };
        arms.push(format!("{pattern} => {value}"));
    }
    format!(
        "fn f(o: {ty}) -> i64 {{ return match o {{ {} }}; }}",
        arms.join(", ")
    )
}

/// A flat enum of `n` variants, one arm each.
fn wide_enum_match(n: usize) -> String {
    let variants: Vec<String> = (0..n).map(|i| format!("V{i}")).collect();
    let arms: Vec<String> = (0..n).map(|i| format!("Wide::V{i} => {i}")).collect();
    format!(
        "enum Wide {{ {} }} fn f(w: Wide) -> i64 {{ return match w {{ {} }}; }}",
        variants.join(", "),
        arms.join(", ")
    )
}

/// `n` outer variants each carrying an `n`-variant enum, every pair named.
fn nested_enum_match(n: usize) -> String {
    let inner: Vec<String> = (0..n).map(|i| format!("I{i}")).collect();
    let outer: Vec<String> = (0..n).map(|i| format!("O{i}(Inner)")).collect();
    let mut arms = Vec::new();
    for o in 0..n {
        for i in 0..n {
            arms.push(format!("Outer::O{o}(Inner::I{i}) => {}", o * n + i));
        }
    }
    format!(
        "enum Inner {{ {} }} enum Outer {{ {} }} fn f(x: Outer) -> i64 {{ return match x {{ {} }}; }}",
        inner.join(", "),
        outer.join(", "),
        arms.join(", ")
    )
}

/// Operation counts stay linear in the total number of pattern nodes for
/// every shape: depth (arm `k` nests `k` levels, so `(d+1)(d+2)/2` nodes),
/// variant count (`n` nodes), and outer x inner pairs (`2n^2` nodes).
#[test]
fn usefulness_scales_linearly_with_nesting_and_variants() {
    let mut report = Vec::new();
    for (name, build, nodes, sizes) in [
        (
            "depth",
            deep_option_match as fn(usize) -> String,
            (|d| (d + 1) * (d + 2) / 2) as fn(usize) -> usize,
            [4usize, 8, 16, 32],
        ),
        ("variants", wide_enum_match, |n| n, [16, 32, 64, 128]),
        (
            "outer*inner",
            nested_enum_match,
            |n| 2 * n * n,
            [4, 8, 16, 32],
        ),
    ] {
        let counts: Vec<usize> = sizes.iter().map(|&n| usefulness_ops(&build(n))).collect();
        let node_counts: Vec<usize> = sizes.iter().map(|&n| nodes(n)).collect();
        report.push(format!(
            "{name}: sizes {sizes:?} nodes {node_counts:?} -> ops {counts:?}"
        ));
        for (&ops, &input) in counts.iter().zip(&node_counts) {
            assert!(
                ops <= 2 * input + 8,
                "{name}: {ops} ops for {input} pattern nodes: {counts:?}"
            );
        }
        for (ops, input) in counts.windows(2).zip(node_counts.windows(2)) {
            let input_ratio = input[1] as f64 / input[0] as f64;
            let op_ratio = ops[1] as f64 / ops[0] as f64;
            assert!(
                op_ratio <= input_ratio * 1.25,
                "{name}: ops grew {op_ratio:.2}x for {input_ratio:.2}x input: {counts:?}"
            );
        }
    }
    println!("{}", report.join("\n"));
}
