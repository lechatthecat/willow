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

// Generic callers (willow-jz15.97): a generic call made from inside another
// generic function binds the callee's parameters to the caller's own, whose
// names may coincide with the callee's. Lambda parameters are still inferred
// from those bindings, and no conflicting-parameter error cascades.
const GENERIC_CALLER_DECLS: &str = "
fn fold_cl<T, A>(xs: Array<T>, init: A, f: closure(A, T) -> A) -> A { let mut acc = init; for x in xs { acc = f(acc, x); } return acc; }
fn keep_cl<T>(xs: Array<T>, f: closure(T) -> bool) -> Array<T> { let out: Array<T> = []; for x in xs { if f(x) { out.push(x); } } return out; }
fn apply_first_cl<T>(f: closure(T) -> T, x: T) -> T { return f(x); }
fn compose_cl<T, U, V>(x: T, f: closure(T) -> U, g: closure(U) -> V) -> V { return g(f(x)); }
";

fn check_items(items: &str, body: &str) -> Vec<Diagnostic> {
    check_source(&format!(
        "{DECLS}{GENERIC_CALLER_DECLS}\n{items}\nfn main() {{ {body} }}"
    ))
}

#[track_caller]
fn clean_items(items: &str, body: &str) {
    let errors = check_items(items, body);
    assert!(errors.is_empty(), "{items}: {errors:?}");
}

/// Diagnostics a generic caller must never produce for a well-formed lambda.
#[track_caller]
fn no_inference_cascade(errors: &[Diagnostic]) {
    let rendered = format!("{errors:?}");
    assert!(
        !rendered.contains("cannot infer type for lambda parameter"),
        "{rendered}"
    );
    assert!(!rendered.contains("conflicting types"), "{rendered}");
}

#[test]
fn g01_fold_from_generic_caller_with_the_same_parameter_name() {
    clean_items(
        "fn count_where<T>(xs: Array<T>, pred: closure(T) -> bool) -> i64 { return fold_cl(xs, 0, |n, x| if pred(x) { n + 1 } else { n }); }",
        "let n: i64 = count_where([1, 2, 3], |v| v > 1);",
    );
}

#[test]
fn g02_fold_from_generic_caller_with_a_distinct_parameter_name() {
    clean_items(
        "fn count_where<E>(xs: Array<E>, pred: closure(E) -> bool) -> i64 { return fold_cl(xs, 0, |n, x| if pred(x) { n + 1 } else { n }); }",
        "let n: i64 = count_where([1, 2, 3], |v| v > 1);",
    );
}

#[test]
fn g03_caller_parameter_bound_to_a_differently_named_callee_parameter() {
    clean_items(
        "fn total<T>(xs: Array<i64>, init: T, f: closure(T, i64) -> T) -> T { return fold_cl(xs, init, |acc, x| f(acc, x)); }",
        "let n: i64 = total([1, 2], 0, |a, b| a + b);",
    );
}

#[test]
fn g04_crossed_parameter_names() {
    clean_items(
        "fn remap<U, T>(xs: Array<U>, f: closure(U) -> T) -> Array<T> { return map_cl(xs, |x| f(x)); }",
        "let ys: Array<bool> = remap([1, 2], |v| v > 1);",
    );
}

#[test]
fn g05_result_parameter_bound_to_the_caller_parameter_by_the_lambda_body() {
    clean_items(
        "fn twice_all<T>(xs: Array<T>, f: closure(T) -> T) -> Array<T> { return map_cl(xs, |x| f(f(x))); }",
        "let ys: Array<i64> = twice_all([1, 2], |v| v + 1);",
    );
}

#[test]
fn g06_plain_fn_formal_from_generic_caller() {
    clean_items(
        "fn keep_all<T>(xs: Array<T>) -> Array<T> { return keep(xs, |x| true); }",
        "let ys: Array<i64> = keep_all([1, 2]);",
    );
}

#[test]
fn g07_every_callee_parameter_name_reused_by_the_caller() {
    clean_items(
        "fn pipe<T, U, V>(x: T, f: closure(T) -> U, g: closure(U) -> V) -> V { return compose_cl(x, |a| f(a), |b| g(b)); }",
        "let s: bool = pipe(1, |v| v + 1, |w| w > 1);",
    );
}

#[test]
fn g08_explicit_type_arguments_naming_the_caller_parameter() {
    clean_items(
        "fn flags<T>(xs: Array<T>, pred: closure(T) -> bool) -> Array<bool> { return map_cl<T, bool>(xs, |x| pred(x)); }",
        "let ys: Array<bool> = flags([1], |v| v > 0);",
    );
}

#[test]
fn g09_generic_class_argument_from_generic_caller() {
    clean_items(
        "fn cell_apply<T>(c: Cell<T>, f: closure(T) -> T) -> T { return cell_map(c, |v| f(v)); }",
        "let n: i64 = cell_apply(new Cell<i64>(4), |v| v * 2);",
    );
}

#[test]
fn g10_enum_argument_from_generic_caller() {
    clean_items(
        "fn present<T>(o: Option<T>) -> Option<bool> { return opt_map(o, |v| true); }",
        "let o: Option<bool> = present(Option::Some(3));",
    );
}

#[test]
fn g11_nested_generic_calls_from_generic_caller() {
    clean_items(
        "fn weigh<T>(xs: Array<T>, pred: closure(T) -> bool, w: closure(T) -> i64) -> i64 { return fold_cl(keep_cl(xs, |x| pred(x)), 0, |a, x| a + w(x)); }",
        "let n: i64 = weigh([1, 2, 3], |v| v > 1, |v| v * 10);",
    );
}

#[test]
fn g12_lambda_before_the_argument_that_binds_the_caller_parameter() {
    clean_items(
        "fn again<T>(x: T, f: closure(T) -> T) -> T { return apply_first_cl(|v| f(v), x); }",
        "let n: i64 = again(3, |v| v + 1);",
    );
}

#[test]
fn g13_annotated_workaround_still_accepted() {
    clean_items(
        "fn count_where<T>(xs: Array<T>, pred: closure(T) -> bool) -> i64 { return fold_cl(xs, 0, |n: i64, x: T| if pred(x) { n + 1 } else { n }); }",
        "let n: i64 = count_where([1], |v| v > 0);",
    );
}

#[test]
fn g14_generic_class_method_calls_generic_function() {
    clean_items(
        "class Bag<T> { pub items: Array<T>; pub fn count(self, pred: closure(T) -> bool) -> i64 { return fold_cl(self.items, 0, |n, x| if pred(x) { n + 1 } else { n }); } }",
        "let b = new Bag<i64>([1, 2, 3]); let n: i64 = b.count(|v| v > 1);",
    );
}

#[test]
fn g15_nested_lambda_inside_lambda_from_generic_caller() {
    clean_items(
        "fn deep<T>(xs: Array<T>, f: closure(T) -> T) -> Array<T> { return map_cl(xs, |x| map_cl([x], |y| f(y))[0]); }",
        "let ys: Array<i64> = deep([1], |v| v + 1);",
    );
}

#[test]
fn g16_caller_parameter_misused_in_lambda_is_a_real_error() {
    let errors = check_items(
        "fn bump<T>(xs: Array<T>) -> Array<i64> { return map(xs, |x| x + 1); }",
        "",
    );
    assert!(!errors.is_empty(), "T + 1 must be rejected");
    no_inference_cascade(&errors);
}

#[test]
fn g17_lambda_result_mismatching_the_caller_parameter_is_reported() {
    let errors = check_items(
        "fn reset<T>(xs: Array<T>, init: T) -> T { return fold(xs, init, |a, x| 1); }",
        "",
    );
    assert!(!errors.is_empty(), "an i64 result cannot be the caller's T");
    assert!(
        !format!("{errors:?}").contains("cannot infer type for lambda parameter"),
        "{errors:?}"
    );
}

#[test]
fn g18_truly_unbound_parameter_still_requires_an_annotation() {
    let from_main = check("let n = run(|v| 1, 2);");
    let from_generic = check_items("fn g<T>(x: T) -> i64 { return run(|v| 1, 2); }", "");
    assert!(
        format!("{from_main:?}").contains("cannot infer type for lambda parameter `v`"),
        "{from_main:?}"
    );
    assert_eq!(
        format!("{:?}", from_main.iter().map(|e| e.code).collect::<Vec<_>>()),
        format!(
            "{:?}",
            from_generic.iter().map(|e| e.code).collect::<Vec<_>>()
        ),
        "{from_generic:?}"
    );
}

#[test]
fn g19_wrong_lambda_arity_from_generic_caller_is_reported() {
    let errors = check_items(
        "fn count_where<T>(xs: Array<T>) -> i64 { return fold_cl(xs, 0, |n| n); }",
        "",
    );
    assert!(
        !errors.is_empty(),
        "a one-parameter lambda cannot be closure(A, T) -> A"
    );
}

#[test]
fn g20_same_callee_from_generic_and_plain_callers() {
    clean_items(
        "fn count_where<T>(xs: Array<T>, pred: closure(T) -> bool) -> i64 { return fold_cl(xs, 0, |n, x| if pred(x) { n + 1 } else { n }); }",
        "let a: i64 = count_where([\"a\", \"bb\"], |s| s == \"a\"); let b: i64 = fold_cl([1, 2], 0, |n, x| n + x);",
    );
}

#[test]
fn g21_capturing_lambda_into_fn_formal_from_generic_caller_is_e1011() {
    let errors = check_items(
        "fn keep_where<T>(xs: Array<T>, pred: closure(T) -> bool) -> Array<T> { return keep(xs, |x| pred(x)); }",
        "",
    );
    assert_eq!(errors.len(), 1, "{errors:?}");
    assert!(format!("{errors:?}").contains("E1011"), "{errors:?}");
}

#[test]
fn g22_recursive_generic_call_types_its_lambda() {
    clean_items(
        "fn drain<T>(xs: Array<T>, n: i64, f: closure(T) -> T) -> Array<T> { if n == 0 { return xs; } return drain(map_cl(xs, |x| f(x)), n - 1, |y| f(y)); }",
        "let ys: Array<i64> = drain([1], 2, |v| v + 1);",
    );
}
