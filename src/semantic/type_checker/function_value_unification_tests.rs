//! One function type: a named function, a captureless lambda and a capturing
//! lambda are all values of `fn(...) -> R`, interchangeable wherever such a
//! value is accepted. The only remaining distinction is whether a value may
//! cross tasks, which is decided by what it captures, not by how it was
//! written.
use crate::diagnostics::{Diagnostic, ErrorCode};

fn check_source(source: &str) -> Vec<Diagnostic> {
    let tokens = crate::lexer::Lexer::new(source).tokenize().expect("lex");
    let (program, parse_errors) = crate::parser::Parser::new(tokens).parse();
    assert!(parse_errors.is_empty(), "parse errors: {parse_errors:?}");
    let mut checker = crate::semantic::TypeChecker::new();
    // As in a normal build, which runs tasks on several workers.
    checker.set_enforce_send_sync(true);
    crate::register_prelude(&mut checker).expect("prelude");
    checker.check_program(&program);
    checker.errors
}

const DECLS: &str = "import std::collections::Array;
fn inc(x: i64) -> i64 { return x + 1; }
fn double(x: i64) -> i64 { return x * 2; }
fn lt(a: i64, b: i64) -> bool { return a < b; }
fn apply(f: fn(i64) -> i64, x: i64) -> i64 { return f(x); }
fn gapply<T>(f: fn(T) -> T, x: T) -> T { return f(x); }
fn gcompare<T>(f: fn(T, T) -> bool, a: T, b: T) -> bool { return f(a, b); }
fn either<T>(a: T, b: T) -> T { return a; }
fn run_all(fs: Array<fn(i64) -> i64>, x: i64) -> i64 { let mut v = x; for f in fs { v = f(v); } return v; }
fn grun_all<T>(fs: Array<fn(T) -> T>, x: T) -> T { let mut v = x; for f in fs { v = f(v); } return v; }
fn hof(g: fn(fn(i64) -> i64) -> i64) -> i64 { return g(inc); }
fn maybe(o: Option<fn(i64) -> i64>) -> i64 { return match o { Some(f) => f(1), None => 0 }; }
fn frozen(fs: FrozenArray<fn(i64) -> i64>) -> i64 { return fs.len(); }
fn pick(capture: bool, k: i64) -> fn(i64) -> i64 { if capture { return |x: i64| x + k; } return double; }
async fn in_task(f: fn(i64) -> i64, x: i64) -> i64 { return f(x); }
class Stage { pub run: fn(i64) -> i64; }
interface Shared extends Sync {}
class SharedStage implements Shared { pub run: fn(i64) -> i64; }
";

fn check(body: &str) -> Vec<Diagnostic> {
    check_source(&format!("{DECLS}\nfn main() {{ {body} }}"))
}

#[track_caller]
fn rejected(body: &str, code: ErrorCode, text: &str) {
    let errors = check(body);
    assert!(
        errors.iter().any(|e| e.code == code
            && (e.message.contains(text) || e.helps.iter().any(|h| h.contains(text)))),
        "{body}: expected {code:?} mentioning {text:?}, got {errors:?}"
    );
    assert!(
        errors.iter().all(|e| !e.message.contains("closure")),
        "{body}: a diagnostic still names a separate closure type: {errors:?}"
    );
}

#[test]
fn every_function_value_is_accepted_wherever_a_function_type_is() {
    let accepted = [
        // A parameter takes a named function, a captureless and a capturing lambda.
        "println(apply(inc, 1));",
        "println(apply(|x: i64| x * 3, 1));",
        "let k = 4; println(apply(|x: i64| x + k, 1));",
        // Generic parameters, inferred and explicit.
        "println(gapply(inc, 1));",
        "println(gapply<i64>(inc, 1));",
        "println(gcompare(lt, 1, 2));",
        "println(gcompare(|a: i64, b: i64| lt(a, b), 1, 2));",
        // A type parameter bound by both kinds joins them.
        "let k = 1; let f = either(inc, |x: i64| x + k); println(f(1));",
        // Array elements: a mixed literal, to a concrete and a generic parameter.
        "let k = 2; println(run_all([inc, double, |x: i64| x + k], 1));",
        "let k = 2; let fs = [inc, |x: i64| x + k]; println(grun_all(fs, 1));",
        "let fs: Array<fn(i64) -> i64> = [inc]; fs.push(|x: i64| x * 5); println(run_all(fs, 1));",
        // Fields of a class hold either kind and are reassignable across kinds.
        "let k = 3; let s = new Stage(inc); s.run = |x: i64| x + k; let f = s.run; println(f(1));",
        // Return values of both kinds through one declared result type.
        "let f = pick(true, 2); let g = pick(false, 2); println(f(1) + g(1));",
        // A higher-order parameter receives a lambda that takes a function value.
        "println(hof(|f: fn(i64) -> i64| f(10)));",
        // A ternary joins both kinds.
        "let k = 1; let f = if k > 0 { inc } else { |x: i64| x + k }; println(f(1));",
        // Immutable builtins are covariant in what they hold.
        "println(maybe(Option::Some(inc)));",
        "let k = 1; println(maybe(Option::Some(|x: i64| x + k)));",
        "println(frozen([inc, double].freeze()));",
        // A mutable binding first holding a named function takes a lambda later.
        "let k = 5; let mut g = inc; g = |x: i64| x + k; println(g(1));",
        // An annotated immutable binding keeps what the value can do.
        "let f: fn(i64) -> i64 = inc; let t = in_task(f, 1);",
        // Passing to a task: a named function, a captureless and an immutably capturing lambda.
        "let t = in_task(inc, 1);",
        "let t = in_task(|x: i64| x * 2, 1);",
        "let k = 9; let t = in_task(|x: i64| x + k, 1);",
        // A class that promises to be shareable holds shareable function values.
        "let k = 1; let s = new SharedStage(|x: i64| x + k); let f = s.run; println(f(1));",
        "let s = new SharedStage(double); let f = s.run; println(f(1));",
    ];
    assert!(accepted.len() >= 20);
    for body in accepted {
        let errors = check(body);
        assert!(errors.is_empty(), "{body}: {errors:?}");
    }
}

#[test]
fn only_task_local_function_values_are_kept_from_other_tasks() {
    // A lambda that mutates a capture is task-local wherever it is written.
    rejected(
        "let mut n = 0; let t = in_task(|x: i64| { n = n + x; return n; }, 1);",
        ErrorCode::E2402,
        "task-local",
    );
    rejected(
        "let mut n = 0; let f = |x: i64| { n = n + x; return n; }; let t = in_task(f, 1);",
        ErrorCode::E2402,
        "task-local",
    );
    rejected(
        "let mut n = 0; let s = new SharedStage(|x: i64| { n = n + x; return n; });",
        ErrorCode::E2402,
        "task-local",
    );
    // A plain class whose field only ever holds shareable values crosses tasks.
    let stage_task = "async fn use_stage(s: Stage) -> i64 { let f = s.run; return f(1); }";
    let errors = check_source(&format!(
        "{DECLS}\n{stage_task}\nasync fn main() {{ println(await use_stage(new Stage(inc))); }}"
    ));
    assert!(errors.is_empty(), "{errors:?}");
    // Once the program stores a task-local value into the field, it does not.
    let errors = check_source(&format!(
        "{DECLS}\n{stage_task}\nfn keep() {{ let mut n = 0; let s = new Stage(|x: i64| {{ n = n + x; return n; }}); }}\nasync fn main() {{ println(await use_stage(new Stage(inc))); }}"
    ));
    assert!(
        errors.iter().any(|e| e.code == ErrorCode::E2402
            && e.helps
                .iter()
                .any(|h| h.contains("every value the program stores"))),
        "{errors:?}"
    );
}

#[test]
fn a_mutable_container_of_shareable_values_does_not_widen_in_place() {
    // `fs` holds only shareable values; letting `run_all`'s parameter view it
    // as an array of any function value would let a store add a task-local one.
    let errors = check_source(&format!(
        "{DECLS}\nasync fn shareable(fs: Array<fn(i64) -> i64>) -> i64 {{ return run_all(fs, 1); }}\nfn main() {{}}"
    ));
    assert!(
        errors
            .iter()
            .any(|e| e.helps.iter().any(|h| h.contains("cannot widen in place"))),
        "{errors:?}"
    );
}

#[test]
fn the_separate_closure_spelling_is_gone() {
    let tokens = crate::lexer::Lexer::new("fn f(g: closure(i64) -> i64) {} fn main() {}")
        .tokenize()
        .expect("lex");
    let (_, errors) = crate::parser::Parser::new(tokens).parse();
    let error = errors
        .iter()
        .find(|e| e.code == ErrorCode::E0107)
        .unwrap_or_else(|| panic!("{errors:?}"));
    assert!(
        error
            .helps
            .iter()
            .any(|h| h.contains("accepts both named functions and capturing lambdas")),
        "{error:?}"
    );
}

#[test]
fn shareable_class_promise_follows_interface_chains() {
    for declarations in [
        "interface A extends Sync {} interface B extends A {} class C implements B { pub f: fn(i64) -> i64; }",
        "interface B extends A {} interface A extends Send {} class C implements B { pub f: fn(i64) -> i64; }",
    ] {
        let source = format!(
            "{declarations}\nfn inc(x: i64) -> i64 {{ return x + 1; }}\nasync fn use_c(c: C) -> i64 {{ let f = c.f; return f(1); }}\nasync fn main() {{ println(await use_c(new C(inc))); }}"
        );
        let errors = check_source(&source);
        assert!(errors.is_empty(), "{declarations}: {errors:?}");
    }
    // An `extends` cycle terminates and promises nothing: an unwritten
    // top-level field stays shareable, but a nested function type keeps the
    // task-local kind a promise would have made shareable.
    let tokens = crate::lexer::Lexer::new(
        "interface A extends B {} interface B extends A {} class C implements A { pub f: fn(i64) -> i64; pub g: Array<fn(i64) -> i64>; } fn main() {}",
    )
    .tokenize()
    .expect("lex");
    let (program, _) = crate::parser::Parser::new(tokens).parse();
    let fields = program
        .items
        .iter()
        .find_map(|item| match item {
            crate::parser::ast::Item::Class(c) => Some(c.fields.clone()),
            _ => None,
        })
        .unwrap();
    use crate::parser::ast::Type;
    assert!(matches!(fields[0].ty, Type::Fn(..)), "{:?}", fields[0].ty);
    assert!(
        matches!(&fields[1].ty, Type::Generic(_, args) if matches!(args[0], Type::Closure(..))),
        "{:?}",
        fields[1].ty
    );
}
