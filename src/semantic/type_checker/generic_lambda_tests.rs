//! Lambda arguments to generic functions (willow-jz15.64): a lambda whose
//! formal still mentions a type parameter is typed after the other arguments,
//! from what they bound, and its own type then binds the rest.
use crate::diagnostics::Diagnostic;

fn check_source(source: &str) -> Vec<Diagnostic> {
    let tokens = crate::lexer::Lexer::new(source).tokenize().expect("lex");
    let (program, parse_errors) = crate::parser::Parser::new(tokens).parse();
    assert!(parse_errors.is_empty(), "parse errors: {parse_errors:?}");
    let mut checker = crate::semantic::TypeChecker::new();
    crate::register_prelude(&mut checker).expect("prelude");
    checker.check_program(&program);
    checker.errors
}

const DECLS: &str = "import std::collections::Array;
fn map<T, U>(xs: Array<T>, f: fn(T) -> U) -> Array<U> { let out: Array<U> = []; for x in xs { out.push(f(x)); } return out; }
fn map_cl<T, U>(xs: Array<T>, f: closure(T) -> U) -> Array<U> { let out: Array<U> = []; for x in xs { out.push(f(x)); } return out; }
fn keep<T>(xs: Array<T>, f: fn(T) -> bool) -> Array<T> { let out: Array<T> = []; for x in xs { if f(x) { out.push(x); } } return out; }
fn fold<T, A>(xs: Array<T>, init: A, f: fn(A, T) -> A) -> A { let mut acc = init; for x in xs { acc = f(acc, x); } return acc; }
fn apply_first<T>(f: fn(T) -> T, x: T) -> T { return f(x); }
fn compose<T, U, V>(x: T, f: fn(T) -> U, g: fn(U) -> V) -> V { return g(f(x)); }
fn run<T>(f: fn(T) -> i64, x: i64) -> i64 { return x; }
fn identity<T>(x: T) -> T { return x; }
fn opt_map<T, U>(o: Option<T>, f: fn(T) -> U) -> Option<U> { return match o { Some(v) => Some(f(v)), None => None }; }
class Cell<T> { pub value: T; }
fn cell_map<T, U>(c: Cell<T>, f: closure(T) -> U) -> U { return f(c.value); }
";

fn check(body: &str) -> Vec<Diagnostic> {
    check_source(&format!("{DECLS}\nfn main() {{ {body} }}"))
}

#[track_caller]
fn clean(body: &str) {
    let errors = check(body);
    assert!(errors.is_empty(), "{body}: {errors:?}");
}

/// Exactly one diagnostic, with `code`, whose message or help mentions `text`.
#[track_caller]
fn single(body: &str, code: &str, text: &str) -> Diagnostic {
    let errors = check(body);
    assert_eq!(errors.len(), 1, "{body}: {errors:?}");
    let error = errors.into_iter().next().unwrap();
    assert_eq!(format!("{:?}", error.code), code, "{body}: {error:?}");
    let rendered = format!("{error:?}");
    assert!(rendered.contains(text), "{body}: {error:?}");
    error
}

#[test]
fn p01_unannotated_lambda_takes_t_from_array_argument() {
    clean("let ys: Array<i64> = map([1, 2, 3], |x| x * 2);");
}

#[test]
fn p02_annotated_lambda_into_closure_parameter() {
    clean("let ys: Array<i64> = map_cl([1, 2], |x: i64| x * 2);");
}

#[test]
fn p03_unannotated_lambda_into_closure_parameter() {
    clean("let ys: Array<i64> = map_cl([1, 2], |x| x * 2);");
}

#[test]
fn p04_capturing_lambda_into_closure_parameter() {
    clean("let k = 10; let ys: Array<i64> = map_cl([1, 2], |x| x + k);");
}

#[test]
fn p05_capturing_lambda_into_generic_fn_parameter_is_e1011() {
    single(
        "let k = 10; let ys = map([1, 2], |x| x + k);",
        "E1011",
        "closure(T) -> U",
    );
}

#[test]
fn p06_no_cascade_after_capture_rejection() {
    single(
        "let k = 10; let ys = map([1, 2], |x| x + k); let zs = keep(ys, |y| y > 3); let n: i64 = fold(zs, 0, |a, b| a + b);",
        "E1011",
        "captures `k`",
    );
}

#[test]
fn p07_no_cascade_after_failed_instantiation() {
    single(
        "let ys = map([1, 2], |x: String| 1); let zs = keep(ys, |y| y > 3); let n = fold(zs, 0, |a, b| a + b); let w = identity(n);",
        "E0201",
        "conflicting types for type parameter `T`",
    );
}

#[test]
fn p08_lambda_before_the_argument_that_binds_t() {
    clean("let n: i64 = apply_first(|x| x + 1, 5);");
}

#[test]
fn p09_result_parameter_comes_from_lambda_body() {
    clean("let flags: Array<bool> = map([1, 2], |x| x > 1);");
    single(
        "let flags: Array<i64> = map([1, 2], |x| x > 1);",
        "E0201",
        "found `Array<bool>`",
    );
}

#[test]
fn p10_fold_binds_accumulator_from_init_and_element_from_array() {
    clean("let total: i64 = fold([1, 2, 3], 0, |acc, x| acc + x);");
    clean("let joined: String = fold([1, 2], \"\", |acc, x| acc + \"!\");");
}

#[test]
fn p11_later_lambda_reads_type_bound_by_earlier_lambda() {
    clean("let ok: bool = compose(4, |a| a * 2, |b| b > 3);");
    clean("let s: String = compose(true, |a| a ? 1 : 0, |b| b > 0 ? \"y\" : \"n\");");
}

#[test]
fn p12_parameter_only_in_unannotated_lambda_still_needs_annotation() {
    single("let n = run(|x| 1, 3);", "E1001", "lambda parameter `x`");
}

#[test]
fn p13_parameter_only_in_annotated_lambda_is_inferred() {
    clean("let n: i64 = run(|x: String| 1, 3);");
}

#[test]
fn p14_lambda_annotation_conflicting_with_bound_t() {
    single(
        "let ys = map([1, 2], |x: String| 1);",
        "E0201",
        "conflicting types for type parameter `T`",
    );
}

#[test]
fn p15_lambda_arity_mismatch_names_the_arity() {
    single(
        "let ys = map([1], |a: i64, b: i64| a);",
        "E0201",
        "callable taking 1 argument(s), found one taking 2",
    );
}

#[test]
fn p16_error_in_lambda_body_is_reported_once() {
    let errors = check("let ys = map([1], |x| x + true);");
    assert_eq!(errors.len(), 1, "{errors:?}");
    assert!(
        !format!("{errors:?}").contains("generic"),
        "derived generic error: {errors:?}"
    );
}

#[test]
fn p17_explicit_type_arguments_still_type_the_lambda() {
    clean("let flags: Array<bool> = map<i64, bool>([1], |x| x > 0);");
}

#[test]
fn p18_closure_value_into_generic_fn_parameter_names_the_closure() {
    single(
        "let k = 1; let add: closure(i64) -> i64 = |x| x + k; let ys = map([1], add);",
        "E0201",
        "declare the parameter `closure(...)`",
    );
}

#[test]
fn p19_block_bodied_lambda_returns_bind_result() {
    clean(
        "let labels: Array<String> = map([1, -1], |x| { if x > 0 { return \"pos\"; } return \"neg\"; });",
    );
}

#[test]
fn p20_generic_call_inside_lambda_body() {
    clean("let ys: Array<i64> = map([1, 2], |x| identity(x) + 1);");
}

#[test]
fn p21_nested_generic_calls_with_lambdas() {
    clean("let n: i64 = fold(keep(map([1, 2, 3], |x| x * 10), |y| y > 10), 0, |a, b| a + b);");
}

#[test]
fn p22_generic_class_argument_binds_lambda_parameter() {
    clean("let c = new Cell<String>(\"w\"); let n: String = cell_map(c, |s| s + \"!\");");
}

#[test]
fn p23_enum_argument_binds_lambda_parameter() {
    clean("let o: Option<bool> = opt_map(Option::Some(3), |v| v > 2);");
}

#[test]
fn p24_unknown_argument_poisons_without_derived_errors() {
    let errors = check("let ys = map(missing, |x| x + 1); let zs = keep(ys, |y| y > 1);");
    assert_eq!(errors.len(), 1, "{errors:?}");
    assert!(format!("{errors:?}").contains("missing"), "{errors:?}");
}

#[test]
fn p25_wrong_value_argument_count_still_checks_lambda() {
    single(
        "let ys = map([1], |x| x, 3);",
        "E0201",
        "expected 2 value arguments, got 3",
    );
}

#[test]
fn p26_non_generic_capturing_lambda_diagnostic_is_unchanged() {
    let errors = check_source(
        "fn apply(f: fn(i64) -> i64) -> i64 { return f(1); } fn main() { let k = 2; let n = apply(|x| x + k); }",
    );
    assert_eq!(errors.len(), 1, "{errors:?}");
    assert!(
        format!("{errors:?}").contains("closure(i64) -> i64"),
        "{errors:?}"
    );
}
