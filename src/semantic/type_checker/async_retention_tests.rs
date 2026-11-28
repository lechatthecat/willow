use super::*;
use crate::diagnostics::{ErrorCode, Severity};
use crate::semantic::TypeChecker;

fn parse(source: &str) -> Program {
    let (program, errors) =
        crate::parser::Parser::new(crate::lexer::Lexer::new(source).tokenize().unwrap()).parse();
    assert!(errors.is_empty(), "{source}: {errors:?}");
    program
}

#[test]
fn async_retention_control_flow_perspectives() {
    let decl = "let mut op: fn(i64) -> i64 = |x| x;";
    let cases = [
        ("no wait", "println(op(1));", false),
        ("unused before wait", "await sleep(0);", false),
        ("dead before wait", "println(op(1)); await sleep(0);", false),
        ("live after wait", "await sleep(0); println(op(1));", true),
        (
            "overwrite",
            "await sleep(0); op = |x| x + 1; println(op(1));",
            false,
        ),
        (
            "read overwrite rhs",
            "await sleep(0); op = |x| x + op(1);",
            true,
        ),
        (
            "overwrite before wait",
            "op = |x| x + 1; await sleep(0); println(op(1));",
            true,
        ),
        (
            "separate branches",
            "if flag { await sleep(0); } else { println(op(1)); }",
            false,
        ),
        (
            "branch live",
            "if flag { await sleep(0); } println(op(1));",
            true,
        ),
        (
            "both kill",
            "await sleep(0); if flag { op = |x| x; } else { op = |x| x+1; } println(op(1));",
            false,
        ),
        (
            "one kill",
            "await sleep(0); if flag { op = |x| x; } println(op(1));",
            true,
        ),
        (
            "return cuts edge",
            "if flag { await sleep(0); return; } println(op(1));",
            false,
        ),
        (
            "unreachable wait",
            "println(op(1)); return; await sleep(0); println(op(2));",
            false,
        ),
        (
            "loop carried",
            "while flag { println(op(1)); await sleep(0); }",
            true,
        ),
        (
            "loop overwrite",
            "while flag { op = |x| x; println(op(1)); await sleep(0); }",
            false,
        ),
        (
            "break",
            "while flag { println(op(1)); await sleep(0); break; }",
            false,
        ),
        (
            "continue",
            "while flag { println(op(1)); await sleep(0); continue; }",
            true,
        ),
        (
            "break continuation",
            "while flag { await sleep(0); break; } println(op(1));",
            true,
        ),
        (
            "shadow",
            "if flag { let op: i64 = 1; await sleep(0); println(op); }",
            false,
        ),
        (
            "outer past shadow",
            "if flag { let op: i64 = 1; await sleep(0); println(op); } println(op(1));",
            true,
        ),
        (
            "late declaration",
            "println(op(1)); await sleep(0); let later: fn(i64) -> i64 = |x| x; println(later(1));",
            false,
        ),
        (
            "closure capture",
            "await sleep(0); let f = |x: i64| op(x); println(f(1));",
            true,
        ),
        (
            "ternary disjoint",
            "let n = flag ? await number() : op(1);",
            false,
        ),
        (
            "defer return",
            "defer { println(op(1)); } await sleep(0); return;",
            true,
        ),
        (
            "defer fallthrough",
            "defer { println(op(1)); } await sleep(0);",
            true,
        ),
        (
            "defer registered later",
            "await sleep(0); op = |x| x; defer { println(op(1)); }",
            false,
        ),
        (
            "defer scope ended",
            "if flag { defer { println(op(1)); } } await sleep(0);",
            false,
        ),
        (
            "defer conditional registration",
            "if flag { defer { println(op(1)); } await sleep(0); }",
            true,
        ),
        (
            "constant dead branch",
            "if false { await sleep(0); println(op(1)); }",
            false,
        ),
        (
            "constant dead loop",
            "while false { await sleep(0); println(op(1)); }",
            false,
        ),
        (
            "indirect call operand",
            "println(op(await number()));",
            true,
        ),
        (
            "for backedge",
            "for i in 0..2 { println(op(i)); await sleep(0); }",
            true,
        ),
    ];
    for (name, body, bad) in cases {
        let source = format!(
            "async fn number() -> i64 {{ return 1; }} async fn check(flag: bool) {{ {decl} {body} }}"
        );
        let program = parse(&source);
        let mut checker = TypeChecker::new();
        checker.set_enforce_send_sync(true);
        checker.check_program(&program);
        let errors: Vec<_> = checker
            .errors
            .iter()
            .filter(|d| d.severity == Severity::Error)
            .collect();
        assert_eq!(
            errors.iter().any(|d| d.code == ErrorCode::E2402),
            bad,
            "{name}: {errors:?}"
        );
        assert!(
            errors.iter().all(|d| d.code == ErrorCode::E2402),
            "{name}: {errors:?}"
        );
    }
}

#[test]
fn async_retention_synthetic_lifetimes() {
    let cases = [
        (
            "for before wait",
            "let ops: Array<fn(i64) -> i64> = [|x| x]; for op in ops { println(op(1)); } await sleep(0);",
            false,
        ),
        (
            "for item dead",
            "let ops: Array<fn(i64) -> i64> = [|x| x]; for op in ops { println(op(1)); await sleep(0); break; }",
            false,
        ),
        (
            "for iterator live",
            "let ops: Array<fn(i64) -> i64> = [|x| x]; for op in ops { println(op(1)); await sleep(0); }",
            true,
        ),
        (
            "for item live",
            "let ops: Array<fn(i64) -> i64> = [|x| x]; for op in ops { await sleep(0); println(op(1)); break; }",
            true,
        ),
        (
            "deferred operand scope ended",
            "if true { let mut op: fn(i64) -> i64 = |x| x; defer consume(op); } await sleep(0);",
            false,
        ),
        (
            "deferred operand live",
            "let mut op: fn(i64) -> i64 = |x| x; defer consume(op); await sleep(0);",
            true,
        ),
        (
            "deferred operand no wait",
            "let mut op: fn(i64) -> i64 = |x| x; defer consume(op);",
            false,
        ),
        (
            "channel implicit wait",
            "let mut op: fn(i64) -> i64 = |x| x; let ch = Channel<i64>::new(); ch.send(1); println(op(1));",
            true,
        ),
        (
            "channel wait after last use",
            "let mut op: fn(i64) -> i64 = |x| x; println(op(1)); let ch = Channel<i64>::new(); ch.send(1);",
            false,
        ),
    ];
    for (name, body, bad) in cases {
        let source = format!(
            "import std::collections::Array; fn consume(op: fn(i64) -> i64) {{ println(op(1)); }} async fn check() {{ {body} }}"
        );
        let program = parse(&source);
        let mut checker = TypeChecker::new();
        checker.set_enforce_send_sync(true);
        checker.check_program(&program);
        let errors: Vec<_> = checker
            .errors
            .iter()
            .filter(|d| d.severity == Severity::Error)
            .collect();
        assert_eq!(
            errors.iter().any(|d| d.code == ErrorCode::E2402),
            bad,
            "{name}: {errors:?}"
        );
        assert!(
            errors.iter().all(|d| d.code == ErrorCode::E2402),
            "{name}: {errors:?}"
        );
    }
}

#[test]
fn async_retention_intermediate_operands() {
    let cases = [
        (
            "nested return",
            "consume(identity(op), await number());",
            true,
        ),
        (
            "two nested returns",
            "consume(identity(identity(op)), await number());",
            true,
        ),
        (
            "field",
            "let holder = new Holder(op); consume(holder.value, await number());",
            true,
        ),
        (
            "index",
            "let ops: Array<fn(i64) -> i64> = [op]; consume(ops[0], await number());",
            true,
        ),
        (
            "literal closure",
            "consume(|x| { base = base + x; return base; }, await number());",
            true,
        ),
        (
            "conditional result",
            "consume(flag ? identity(op) : op, await number());",
            true,
        ),
        (
            "consumed before wait",
            "consume(identity(op), 1); await sleep(0);",
            false,
        ),
        (
            "scalar result",
            "consume_number(evaluate(op), await number());",
            false,
        ),
        (
            "later operand",
            "consume_number(await number(), evaluate(op));",
            true,
        ),
        ("no wait", "consume(identity(op), 1);", false),
        (
            "field consumed",
            "let holder = new Holder(op); consume(holder.value, 1); await sleep(0);",
            false,
        ),
        (
            "index consumed",
            "let ops: Array<fn(i64) -> i64> = [op]; consume(ops[0], 1); await sleep(0);",
            false,
        ),
    ];
    for (name, body, bad) in cases {
        let source = format!(
            r#"
import std::collections::Array;
class Holder {{ pub value: fn(i64) -> i64; }}
fn identity(op: fn(i64) -> i64) -> fn(i64) -> i64 {{ return op; }}
fn evaluate(op: fn(i64) -> i64) -> i64 {{ return op(1); }}
fn consume(op: fn(i64) -> i64, n: i64) {{ println(op(n)); }}
fn consume_number(a: i64, b: i64) {{ println(a + b); }}
async fn number() -> i64 {{ return 1; }}
async fn check(flag: bool) {{ let mut base = 1; let op: fn(i64) -> i64 = |x| {{ base = base + x; return base; }}; {body} }}
"#
        );
        let program = parse(&source);
        let mut checker = TypeChecker::new();
        checker.set_enforce_send_sync(true);
        checker.check_program(&program);
        let errors: Vec<_> = checker
            .errors
            .iter()
            .filter(|d| d.severity == Severity::Error)
            .collect();
        assert_eq!(
            errors.iter().any(|d| d.code == ErrorCode::E2402),
            bad,
            "{name}: {errors:?}"
        );
        assert!(
            errors.iter().all(|d| d.code == ErrorCode::E2402),
            "{name}: {errors:?}"
        );
        if bad && name != "later operand" {
            assert!(
                errors.iter().any(|d| d
                    .labels
                    .iter()
                    .any(|label| label.message.contains("intermediate value"))),
                "{name}: {errors:?}"
            );
        }
    }
}

#[test]
fn async_retention_patterns_and_deferred_callees() {
    let cases = [
        (
            "guard discarded fallback",
            "match op { _ if await choose() => {}, _ => {} }",
            false,
        ),
        (
            "binding live",
            "match op { f => { await sleep(0); println(f(1)); } }",
            true,
        ),
        (
            "binding dead",
            "match op { f => { println(f(1)); await sleep(0); } }",
            false,
        ),
        (
            "binding no wait",
            "match op { f => { println(f(1)); } }",
            false,
        ),
        (
            "binding unused",
            "match op { f => { await sleep(0); } }",
            false,
        ),
        (
            "binding shadow",
            "match op { op => { await sleep(0); println(op(1)); } }",
            true,
        ),
        (
            "binding after wait",
            "await sleep(0); op = |x| x; match op { f => { println(f(1)); } }",
            false,
        ),
        (
            "guard fallback",
            "match op { f if await choose() => { println(1); }, g => { println(g(1)); } }",
            true,
        ),
        (
            "guard binding",
            "match op { f if await choose() => { println(f(1)); }, _ => {} }",
            true,
        ),
        (
            "guard no wait",
            "match op { f if flag => { println(f(1)); }, g => { println(g(1)); } } await sleep(0);",
            false,
        ),
        (
            "guard dead body",
            "match op { f if flag => { println(f(1)); await sleep(0); }, g => { println(g(1)); } }",
            false,
        ),
        (
            "payload live",
            "match Boxed::One(op) { Boxed::One(f) => { await sleep(0); println(f(1)); }, Boxed::Empty => {} }",
            true,
        ),
        (
            "payload dead",
            "match Boxed::One(op) { Boxed::One(f) => { println(f(1)); await sleep(0); }, Boxed::Empty => {} }",
            false,
        ),
        (
            "normalized payload",
            "match Boxed::One(op) { One(f) => { await sleep(0); println(f(1)); }, Empty => {} }",
            true,
        ),
        (
            "payload guard fallback",
            "match Boxed::One(op) { One(f) if await choose() => {}, One(g) => { println(g(1)); }, Empty => {} }",
            true,
        ),
        (
            "class binding live",
            "let obj: Value = new Holder(op); match obj { Holder(h) => { await sleep(0); let callback = h.value; println(callback(1)); }, _ => {} }",
            true,
        ),
        (
            "class binding dead",
            "let obj: Value = new Holder(op); match obj { Holder(h) => { let callback = h.value; println(callback(1)); await sleep(0); }, _ => {} }",
            false,
        ),
        (
            "fieldless nested variant binds nothing",
            "let Red = op; match Tint::Paint(Color::Red) { Paint(Red) => { await sleep(0); println(Red(1)); }, _ => {} }",
            true,
        ),
        (
            "fieldless nested variant outer dead",
            "let Red = op; match Tint::Paint(Color::Red) { Paint(Red) => { println(Red(1)); await sleep(0); }, _ => {} }",
            false,
        ),
        ("defer live", "defer op(1); await sleep(0);", true),
        (
            "defer reassigned",
            "defer op(1); op = |x| x + 2; await sleep(0);",
            true,
        ),
        ("defer no wait", "defer op(1);", false),
        (
            "defer no wait reassigned",
            "defer op(1); op = |x| x + 2;",
            false,
        ),
        (
            "defer scope ended",
            "if flag { defer op(1); } await sleep(0);",
            false,
        ),
        (
            "defer conditional",
            "if flag { defer op(1); await sleep(0); }",
            true,
        ),
        (
            "defer after wait",
            "await sleep(0); op = |x| x; defer op(1);",
            false,
        ),
        ("defer direct", "defer plain(1); await sleep(0);", false),
    ];
    for (name, body, bad) in cases {
        let program = parse(&format!(
            r#"
enum Boxed {{ One(fn(i64) -> i64), Empty }}
enum Color {{ Red, Green }}
enum Tint {{ Paint(Color), Blank }}
interface Value {{}}
class Holder implements Value {{ pub value: fn(i64) -> i64; }}
fn plain(x: i64) {{ println(x); }}
async fn choose() -> bool {{ return false; }}
async fn check(flag: bool) {{ let mut op: fn(i64) -> i64 = |x| x; {body} }}
"#
        ));
        let mut checker = TypeChecker::new();
        checker.set_enforce_send_sync(true);
        checker.check_program(&program);
        let errors: Vec<_> = checker
            .errors
            .iter()
            .filter(|d| d.severity == Severity::Error)
            .collect();
        assert_eq!(
            errors.iter().any(|d| d.code == ErrorCode::E2402),
            bad,
            "{name}: {errors:?}"
        );
        assert!(
            errors.iter().all(|d| d.code == ErrorCode::E2402),
            "{name}: {errors:?}"
        );
        if name == "defer reassigned" {
            assert!(
                errors.iter().any(|d| d
                    .labels
                    .iter()
                    .any(|l| l.message.contains("deferred callee"))),
                "{errors:?}"
            );
        }
    }
}

#[test]
fn async_retention_pattern_defer_scaling() {
    for n in [16, 32, 64, 128] {
        for deferred in [false, true] {
            let body = if deferred {
                "if true { let mut op: fn(i64) -> i64 = |x| x; defer op(1); await sleep(0); }"
                    .repeat(n)
            } else {
                let arms = "f if await choose() => {},".repeat(n);
                format!(
                    "let mut op: fn(i64) -> i64 = |x| x; match op {{ {arms} g => {{ println(g(1)); }} }}"
                )
            };
            let program = parse(&format!(
                "async fn choose() -> bool {{ return false; }} async fn work() {{ {body} }}"
            ));
            let mut checker = TypeChecker::new();
            checker.set_enforce_send_sync(true);
            checker.check_program(&program);
            let Item::Function(f) = &program.items[1] else {
                unreachable!()
            };
            let result = analyze(
                &f.body,
                &checker.lambda_captures,
                &checker.expr_types,
                &checker.builtin_calls,
                &checker.analysis_calls,
                &checker.local.exhaustive_matches,
                &checker.pattern_resolutions,
            );
            assert!(checker.errors.iter().any(|e| e.code == ErrorCode::E2402));
            assert!(result.nodes <= 12 * n + 10, "{}", result.nodes);
            assert!(result.facts <= 22 * n + 10, "{}", result.facts);
            assert!(result.edge_visits <= 24 * n + 10, "{}", result.edge_visits);
            println!(
                "pattern/defer n={n} deferred={deferred} blocks={} facts={} edges={}",
                result.nodes, result.facts, result.edge_visits
            );
        }
    }
}

#[test]
fn async_retention_resolved_scalar_output_preserves_cleanup_kills() {
    let cases = [
        ("", "println(1)", false),
        ("", "print(1)", false),
        ("", "std::io::println(2.0)", false),
        ("", "std::io::print(true)", false),
        ("import std::io as out;", "out::println(1)", false),
        ("import std::io as out;", "out::print(1)", false),
        ("import std::io::println as line;", "line(1)", false),
        ("import std::io::print as write;", "write(1)", false),
        ("", "println(\"text\")", false),
        (
            "",
            "let println = |n: i64| { panic(\"stop\"); }; println(1)",
            true,
        ),
        ("", "println(1 / zero)", true),
        ("", "std::io::println(1 / zero)", true),
        ("import std::io::println as line;", "line(1 / zero)", true),
        (
            "fn value() -> i64 { panic(\"stop\"); }",
            "println(value())",
            true,
        ),
        (
            "fn println(n: i64) { panic(\"stop\"); }",
            "println(1)",
            true,
        ),
        ("fn print(n: i64) { panic(\"stop\"); }", "print(1)", true),
        (
            "import std::io::println as line;",
            "let line = |n: i64| { panic(\"stop\"); }; line(1)",
            true,
        ),
    ];
    for (declarations, call, bad) in cases {
        let source = format!(
            "{declarations} async fn work(zero: i64) {{ let mut op: fn(i64) -> i64 = |x| x; defer {{ std::io::println(op(1)); }} defer {{ {call}; op = |x| x + 1; }} await sleep(0); }}"
        );
        let program = parse(&source);
        let mut checker = TypeChecker::new();
        checker.set_enforce_send_sync(true);
        checker.check_program(&program);
        let errors: Vec<_> = checker
            .errors
            .iter()
            .filter(|e| e.severity == Severity::Error)
            .collect();
        assert_eq!(
            errors.iter().any(|e| e.code == ErrorCode::E2402),
            bad,
            "{source}: {errors:?}"
        );
        assert!(
            errors.iter().all(|e| e.code == ErrorCode::E2402),
            "{source}: {errors:?}"
        );
    }
}

#[test]
fn async_retention_ordered_cleanup() {
    let cases = [
        (
            "overwrite",
            "defer { op = |x| x + 1; println(op(1)); }",
            true,
            false,
        ),
        (
            "both branches",
            "defer { if flag { op = |x| x + 1; } else { op = |x| x + 2; } println(op(1)); }",
            true,
            false,
        ),
        (
            "one branch",
            "defer { if flag { op = |x| x + 1; } println(op(1)); }",
            true,
            true,
        ),
        (
            "read before overwrite",
            "defer { println(op(1)); op = |x| x + 1; }",
            true,
            true,
        ),
        (
            "rhs reads old",
            "defer { op = identity(op); println(op(1)); }",
            true,
            true,
        ),
        (
            "unreachable read",
            "defer { if false { println(op(1)); } }",
            true,
            false,
        ),
        (
            "constant overwrite",
            "defer { if true { op = |x| x + 1; } println(op(1)); }",
            true,
            false,
        ),
        (
            "unreachable overwrite",
            "defer { if false { op = |x| x + 1; } println(op(1)); }",
            true,
            true,
        ),
        (
            "shadow local",
            "defer { let op: fn(i64) -> i64 = |x| x + 1; println(op(1)); }",
            true,
            false,
        ),
        (
            "shadow init reads",
            "defer { let op = identity(op); println(op(1)); }",
            true,
            true,
        ),
        (
            "loop zero iterations",
            "defer { while flag { op = |x| x + 1; } println(op(1)); }",
            true,
            true,
        ),
        (
            "loop overwrite",
            "defer { while true { op = |x| x + 1; println(op(1)); } }",
            true,
            false,
        ),
        (
            "loop unreachable",
            "defer { while false { println(op(1)); } }",
            true,
            false,
        ),
        (
            "nested overwrite",
            "defer { if flag { if flag { op = |x| x; } else { op = |x| x; } } else { op = |x| x; } println(op(1)); }",
            true,
            false,
        ),
        (
            "match overwrite",
            "defer match flag { true => { op = |x| x; println(op(1)); }, false => { op = |x| x; println(op(1)); } };",
            true,
            false,
        ),
        (
            "match read branch",
            "defer match flag { true => { op = |x| x; println(op(1)); }, false => { println(op(1)); } };",
            true,
            true,
        ),
        (
            "lifo kills",
            "defer { println(op(1)); } defer { op = |x| x; }",
            true,
            false,
        ),
        (
            "lifo reads",
            "defer { op = |x| x; } defer { println(op(1)); }",
            true,
            true,
        ),
        ("no await", "defer { println(op(1)); }", false, false),
        (
            "ended scope",
            "if flag { defer { println(op(1)); } }",
            true,
            false,
        ),
    ];
    for (name, body, wait, bad) in cases {
        let wait = if wait { "await sleep(0);" } else { "" };
        let program = parse(&format!(
            "fn identity(op: fn(i64) -> i64) -> fn(i64) -> i64 {{ return op; }} async fn check(flag: bool) {{ let mut op: fn(i64) -> i64 = |x| x; {body} {wait} }}"
        ));
        let mut checker = TypeChecker::new();
        checker.set_enforce_send_sync(true);
        checker.check_program(&program);
        let errors: Vec<_> = checker
            .errors
            .iter()
            .filter(|e| e.severity == Severity::Error)
            .collect();
        assert_eq!(
            errors.iter().any(|e| e.code == ErrorCode::E2402),
            bad,
            "{name}: {errors:?}"
        );
        assert!(
            errors.iter().all(|e| e.code == ErrorCode::E2402),
            "{name}: {errors:?}"
        );
    }
}

#[test]
fn async_retention_shared_ordered_cleanup_scaling() {
    for n in [16, 32, 64, 128] {
        let cleanup = "defer { op = |x| x + 1; println(op(1)); }".repeat(n);
        let exits = "if flag { return; }".repeat(n);
        let program = parse(&format!(
            "async fn work(flag: bool) {{ let mut op: fn(i64) -> i64 = |x| x; {cleanup} await sleep(0); {exits} }}"
        ));
        let mut checker = TypeChecker::new();
        checker.set_enforce_send_sync(true);
        checker.check_program(&program);
        assert!(
            !checker.errors.iter().any(|e| e.severity == Severity::Error),
            "{:?}",
            checker.errors
        );
        let Item::Function(f) = &program.items[0] else {
            unreachable!()
        };
        let result = analyze(
            &f.body,
            &checker.lambda_captures,
            &checker.expr_types,
            &checker.builtin_calls,
            &checker.analysis_calls,
            &checker.local.exhaustive_matches,
            &checker.pattern_resolutions,
        );
        assert!(result.nodes <= 12 * n + 10, "{}", result.nodes);
        assert!(result.facts <= 12 * n + 10, "{}", result.facts);
        assert!(result.edge_visits <= 16 * n + 10, "{}", result.edge_visits);
        assert!(
            result.summary_counts.0 <= 4 * n,
            "{:?}",
            result.summary_counts
        );
        assert!(
            result.summary_counts.1 <= 8 * n,
            "{:?}",
            result.summary_counts
        );
        println!(
            "cleanup summaries n={n} nodes={} visits={}",
            result.summary_counts.0, result.summary_counts.1
        );
        println!(
            "ordered cleanup n={n} blocks={} facts={} edges={}",
            result.nodes, result.facts, result.edge_visits
        );
    }
}

#[test]
fn async_retention_cleanup_continuations() {
    let cases = [
        (
            "fallthrough",
            "await sleep(0); if true { defer { op = |x| x + 1; } } println(op(1));",
            false,
        ),
        (
            "optional scope",
            "await sleep(0); if flag { defer { op = |x| x; } } println(op(1));",
            true,
        ),
        (
            "both scopes",
            "await sleep(0); if flag { defer { op = |x| x; } } else { defer { op = |x| x; } } println(op(1));",
            false,
        ),
        (
            "nested scope",
            "await sleep(0); if true { if true { defer { op = |x| x; } } } println(op(1));",
            false,
        ),
        (
            "scope read before kill",
            "await sleep(0); if true { defer { println(op(1)); op = |x| x; } } println(op(1));",
            true,
        ),
        (
            "scope conditional kill",
            "await sleep(0); if true { defer { if flag { op = |x| x; } } } println(op(1));",
            true,
        ),
        (
            "scope both kill",
            "await sleep(0); if true { defer { if flag { op = |x| x; } else { op = |x| x; } } } println(op(1));",
            false,
        ),
        (
            "break",
            "while true { await sleep(0); defer { op = |x| x; } break; } println(op(1));",
            false,
        ),
        (
            "nested break",
            "while true { await sleep(0); if true { defer { op = |x| x; } break; } } println(op(1));",
            false,
        ),
        (
            "break conditional kill",
            "while true { await sleep(0); defer { if flag { op = |x| x; } } break; } println(op(1));",
            true,
        ),
        (
            "break reads old",
            "while true { await sleep(0); defer { println(op(1)); op = |x| x; } break; } println(op(1));",
            true,
        ),
        (
            "break skips outer cleanup",
            "defer { op = |x| x; } while true { await sleep(0); break; } println(op(1));",
            true,
        ),
        (
            "continue",
            "while true { println(op(1)); await sleep(0); defer { op = |x| x; } continue; }",
            false,
        ),
        (
            "nested continue",
            "while true { println(op(1)); await sleep(0); if true { defer { op = |x| x; } continue; } }",
            false,
        ),
        (
            "continue conditional kill",
            "while true { println(op(1)); await sleep(0); defer { if flag { op = |x| x; } } continue; }",
            true,
        ),
        (
            "continue reads old",
            "while true { println(op(1)); await sleep(0); defer { println(op(1)); op = |x| x; } continue; }",
            true,
        ),
        (
            "continue skips outer cleanup",
            "defer { op = |x| x; } while true { println(op(1)); await sleep(0); continue; }",
            true,
        ),
        (
            "fallthrough skips outer cleanup",
            "defer { op = |x| x; } await sleep(0); if true {} println(op(1));",
            true,
        ),
        (
            "nested defer",
            "await sleep(0); if true { defer { defer { op = |x| x; } } } println(op(1));",
            false,
        ),
        (
            "lifo scope",
            "await sleep(0); if true { defer { println(op(1)); } defer { op = |x| x; } } println(op(1));",
            false,
        ),
    ];
    for (name, body, bad) in cases {
        let program = parse(&format!(
            "async fn check(flag: bool) {{ let mut op: fn(i64) -> i64 = |x| x; {body} }}"
        ));
        let mut checker = TypeChecker::new();
        checker.set_enforce_send_sync(true);
        checker.check_program(&program);
        let errors: Vec<_> = checker
            .errors
            .iter()
            .filter(|e| e.severity == Severity::Error)
            .collect();
        assert_eq!(
            errors.iter().any(|e| e.code == ErrorCode::E2402),
            bad,
            "{name}: {errors:?}"
        );
        assert!(
            errors.iter().all(|e| e.code == ErrorCode::E2402),
            "{name}: {errors:?}"
        );
    }
}

#[test]
fn async_retention_exhaustive_match_overwrites() {
    let matches = [
        (
            "bool",
            "match flag { true => { op = |x| x; }, false => { op = |x| x; } }",
            false,
        ),
        (
            "bool read",
            "match flag { true => { println(op(1)); }, false => { op = |x| x; } }",
            true,
        ),
        (
            "bool unchanged",
            "match flag { true => {}, false => { op = |x| x; } }",
            true,
        ),
        (
            "bool guard",
            "match flag { true if flag => { op = |x| x; }, true => { op = |x| x; }, false => { op = |x| x; } }",
            false,
        ),
        (
            "bool guard read",
            "match flag { true if flag => { op = |x| x; }, true => { println(op(1)); }, false => { op = |x| x; } }",
            true,
        ),
        (
            "enum",
            "match tag { Tag::A => { op = |x| x; }, Tag::B => { op = |x| x; } }",
            false,
        ),
        (
            "enum read",
            "match tag { Tag::A => { println(op(1)); }, Tag::B => { op = |x| x; } }",
            true,
        ),
        (
            "enum unchanged",
            "match tag { Tag::A => {}, Tag::B => { op = |x| x; } }",
            true,
        ),
        (
            "enum guard",
            "match tag { Tag::A if flag => { op = |x| x; }, Tag::A => { op = |x| x; }, Tag::B => { op = |x| x; } }",
            false,
        ),
        (
            "enum guard read",
            "match tag { Tag::A if flag => { op = |x| x; }, Tag::A => { println(op(1)); }, Tag::B => { op = |x| x; } }",
            true,
        ),
    ];
    for (name, matched, bad) in matches {
        for cleanup in [false, true] {
            let body = if cleanup {
                format!("defer {{ {matched} println(op(1)); }} await sleep(0);")
            } else {
                format!("await sleep(0); {matched} println(op(1));")
            };
            let program = parse(&format!(
                "enum Tag {{ A, B }} async fn check(flag: bool, tag: Tag) {{ let mut op: fn(i64) -> i64 = |x| x; {body} }}"
            ));
            let mut checker = TypeChecker::new();
            checker.set_enforce_send_sync(true);
            checker.check_program(&program);
            let errors: Vec<_> = checker
                .errors
                .iter()
                .filter(|e| e.severity == Severity::Error)
                .collect();
            assert_eq!(
                errors.iter().any(|e| e.code == ErrorCode::E2402),
                bad,
                "{name} cleanup={cleanup}: {errors:?}"
            );
            assert!(
                errors.iter().all(|e| e.code == ErrorCode::E2402),
                "{name} cleanup={cleanup}: {errors:?}"
            );
        }
    }
}

#[test]
fn async_retention_exhaustive_match_scaling() {
    for n in [16, 32, 64, 128] {
        let variants = (0..n).map(|i| format!("V{i},")).collect::<String>();
        let arms = (0..n)
            .map(|i| format!("Tag::V{i} => {{ op = |x| x; }},"))
            .collect::<String>();
        for qualified in [false, true] {
            let arms = if qualified {
                arms.clone()
            } else {
                arms.replace("Tag::", "")
            };
            for cleanup in [false, true] {
                let matched = format!("match tag {{ {arms} }} println(op(1));");
                let body = if cleanup {
                    format!("defer {{ {matched} }} await sleep(0);")
                } else {
                    format!("await sleep(0); {matched}")
                };
                let program = parse(&format!(
                    "enum Tag {{ {variants} }} async fn check(tag: Tag) {{ let mut op: fn(i64) -> i64 = |x| x; {body} }}"
                ));
                let mut checker = TypeChecker::new();
                checker.set_enforce_send_sync(true);
                checker.check_program(&program);
                assert!(
                    !checker.errors.iter().any(|e| e.severity == Severity::Error),
                    "{:?}",
                    checker.errors
                );
                let Item::Function(f) = &program.items[1] else {
                    unreachable!()
                };
                // Body-local coverage facts are restored after checking a body;
                // reconstruct the proven match ids solely for counting this CFG.
                let mut exhaustive = HashSet::new();
                for event in AstWalk::new(AstEvent::Block(&f.body)) {
                    if let AstEvent::Expr(Expr::Match(m)) = event {
                        exhaustive.insert(m.id);
                    }
                }
                let result = analyze(
                    &f.body,
                    &checker.lambda_captures,
                    &checker.expr_types,
                    &checker.builtin_calls,
                    &checker.analysis_calls,
                    &exhaustive,
                    &checker.pattern_resolutions,
                );
                assert!(result.nodes <= 8 * n + 8);
                assert!(result.facts <= 16 * n + 8);
                assert!(result.edge_visits <= 20 * n + 8);
                assert!(result.summary_counts.0 <= 8 * n + 8);
                assert!(result.summary_counts.1 <= 20 * n + 8);
                println!(
                    "exhaustive n={n} qualified={qualified} cleanup={cleanup} blocks={} facts={} edges={} summary={:?}",
                    result.nodes, result.facts, result.edge_visits, result.summary_counts
                );
            }
        }
    }
}

#[test]
fn async_retention_literal_expression_branches() {
    let cases = [
        ("false && await choose()", false),
        ("true && await choose()", true),
        ("true || await choose()", false),
        ("false || await choose()", true),
        ("true ? false : await choose()", false),
        ("false ? await choose() : true", false),
        ("true ? await choose() : false", true),
        ("false ? true : await choose()", true),
        ("flag && await choose()", true),
        ("flag || await choose()", true),
        ("flag ? false : await choose()", true),
        ("flag ? await choose() : true", true),
        ("false && (true && await choose())", false),
        ("true || (false || await choose())", false),
        ("true ? (false && await choose()) : await choose()", false),
        ("false ? await choose() : (true || await choose())", false),
        ("true && (false || await choose())", true),
        ("false || (true && await choose())", true),
        ("true ? (true && await choose()) : false", true),
        ("false ? true : (false || await choose())", true),
    ];
    for (expr, bad) in cases {
        let program = parse(&format!(
            "async fn choose() -> bool {{ return true; }} async fn check(flag: bool) {{ let mut op: fn(i64) -> i64 = |x| x; let b = {expr}; println(op(1)); }}"
        ));
        let mut checker = TypeChecker::new();
        checker.set_enforce_send_sync(true);
        checker.check_program(&program);
        let errors: Vec<_> = checker
            .errors
            .iter()
            .filter(|e| e.severity == Severity::Error)
            .collect();
        assert_eq!(
            errors.iter().any(|e| e.code == ErrorCode::E2402),
            bad,
            "{expr}: {errors:?}"
        );
        assert!(
            errors.iter().all(|e| e.code == ErrorCode::E2402),
            "{expr}: {errors:?}"
        );
    }
}

#[test]
fn async_retention_resolved_fieldless_variants() {
    let cases = [
        (
            "later read",
            "{a} => {{}}, {b} => {{ println(op(1)); }}",
            "",
            true,
        ),
        (
            "earlier overwrite",
            "{a} => {{ op = |x| x; }}, {b} => {{ println(op(1)); }}",
            "",
            true,
        ),
        (
            "both overwrite",
            "{a} => {{ op = |x| x; }}, {b} => {{ op = |x| x; }}",
            "println(op(1));",
            false,
        ),
        ("both dead", "{a} => {{}}, {b} => {{}}", "", false),
        (
            "guard fallback",
            "{a} if flag => {{}}, {a} => {{}}, {b} => {{ println(op(1)); }}",
            "",
            true,
        ),
    ];
    for qualified in [false, true] {
        let (a, b) = if qualified {
            ("Tag::A", "Tag::B")
        } else {
            ("A", "B")
        };
        for (name, arms, tail, bad) in cases {
            let arms = arms
                .replace("{a}", a)
                .replace("{b}", b)
                .replace("{{", "{")
                .replace("}}", "}");
            for cleanup in [false, true] {
                let matched = format!("match tag {{ {arms} }} {tail}");
                let body = if cleanup {
                    format!("defer {{ {matched} }} await sleep(0);")
                } else {
                    format!("await sleep(0); {matched}")
                };
                assert_variant_retention(
                    &body,
                    bad,
                    &format!("{name} qualified={qualified} cleanup={cleanup}"),
                );
            }
        }
        assert_variant_retention(
            &format!("match tag {{ {a} => {{}}, {b} => {{ await sleep(0); println(op(1)); }} }}"),
            true,
            "later arm suspension",
        );
        assert_variant_retention(
            &format!(
                "let mut A: fn(i64) -> i64 = |x| x; match tag {{ {a} => {{ await sleep(0); println(A(1)); }}, {b} => {{}} }}"
            ),
            true,
            "variant must not shadow local",
        );
    }
}

fn assert_variant_retention(body: &str, bad: bool, name: &str) {
    let program = parse(&format!(
        "enum Tag {{ A, B }} async fn check(tag: Tag, flag: bool) {{ let mut op: fn(i64) -> i64 = |x| x; {body} }}"
    ));
    let mut checker = TypeChecker::new();
    checker.set_enforce_send_sync(true);
    checker.check_program(&program);
    let errors: Vec<_> = checker
        .errors
        .iter()
        .filter(|e| e.severity == Severity::Error)
        .collect();
    assert_eq!(
        errors.iter().any(|e| e.code == ErrorCode::E2402),
        bad,
        "{name}: {errors:?}"
    );
    assert!(
        errors.iter().all(|e| e.code == ErrorCode::E2402),
        "{name}: {errors:?}"
    );
}

#[test]
fn async_retention_literal_match_guards() {
    for pattern in ["_", "value"] {
        for guard in ["false", "true", "flag"] {
            let body = format!(
                "match true {{ {pattern} if {guard} => {{ await sleep(0); println(op(1)); }}, _ => {{}} }}"
            );
            assert_guard_retention(&body, guard != "false");
            let body = format!(
                "match true {{ {pattern} if {guard} => {{}}, _ => {{ await sleep(0); println(op(1)); }} }}"
            );
            assert_guard_retention(&body, guard != "true");
        }
    }
    for cleanup in [false, true] {
        for guard in ["false", "true", "flag"] {
            let matched = format!(
                "match true {{ _ if {guard} => {{ op = |x| x + 1; }}, _ => {{ println(op(1)); }} }} println(op(1));"
            );
            let body = if cleanup {
                format!("defer {{ {matched} }} await sleep(0);")
            } else {
                format!("await sleep(0); {matched}")
            };
            assert_guard_retention(&body, guard != "true");
        }
        let matched = "match true { _ if false => { println(op(1)); }, _ => { op = |x| x + 1; } } println(op(1));";
        let body = if cleanup {
            format!("defer {{ {matched} }} await sleep(0);")
        } else {
            format!("await sleep(0); {matched}")
        };
        assert_guard_retention(&body, false);
    }
}

fn assert_guard_retention(body: &str, bad: bool) {
    let program = parse(&format!(
        "async fn check(flag: bool) {{ let mut op: fn(i64) -> i64 = |x| x; {body} }}"
    ));
    let mut checker = TypeChecker::new();
    checker.set_enforce_send_sync(true);
    checker.check_program(&program);
    let errors: Vec<_> = checker
        .errors
        .iter()
        .filter(|e| e.severity == Severity::Error)
        .collect();
    assert_eq!(
        errors.iter().any(|e| e.code == ErrorCode::E2402),
        bad,
        "{body}: {errors:?}"
    );
    assert!(
        errors.iter().all(|e| e.code == ErrorCode::E2402),
        "{body}: {errors:?}"
    );
}

#[test]
fn async_retention_intermediate_scaling() {
    for n in [16, 32, 64, 128] {
        let body = "consume(identity(op), await number());".repeat(n);
        let program = parse(&format!(
            r#"
fn identity(op: fn(i64) -> i64) -> fn(i64) -> i64 {{ return op; }}
fn consume(op: fn(i64) -> i64, n: i64) {{ println(op(n)); }}
async fn number() -> i64 {{ return 1; }}
async fn work() {{ let op: fn(i64) -> i64 = |x| x; {body} }}
"#
        ));
        let mut checker = TypeChecker::new();
        checker.set_enforce_send_sync(true);
        checker.check_program(&program);
        let Item::Function(f) = &program.items[3] else {
            unreachable!()
        };
        let result = analyze(
            &f.body,
            &checker.lambda_captures,
            &checker.expr_types,
            &checker.builtin_calls,
            &checker.analysis_calls,
            &checker.local.exhaustive_matches,
            &checker.pattern_resolutions,
        );
        assert_eq!(result.values.len(), n);
        assert!(result.nodes <= 4 * n + 4, "{}", result.nodes);
        assert!(result.facts <= 8 * n, "{}", result.facts);
        assert!(result.edge_visits <= 10 * n, "{}", result.edge_visits);
        println!(
            "intermediate n={n} blocks={} facts={} edges={}",
            result.nodes, result.facts, result.edge_visits
        );
    }
}

#[test]
fn async_retention_straight_line_scaling() {
    for n in [16, 32, 64, 128] {
        let declarations = (0..n)
            .map(|i| format!("let x{i} = {i};"))
            .collect::<String>();
        let reads = (0..n)
            .map(|i| format!("println(x{i});"))
            .collect::<String>();
        for suspend in [false, true] {
            let wait = if suspend { "await sleep(0);" } else { "" };
            let program = parse(&format!(
                "async fn check() {{ {declarations} {wait} {reads} }}"
            ));
            let Item::Function(f) = &program.items[0] else {
                unreachable!()
            };
            let result = analyze(
                &f.body,
                &HashMap::new(),
                &HashMap::new(),
                &Default::default(),
                &HashMap::new(),
                &HashSet::new(),
                &HashMap::new(),
            );
            assert_eq!(result.locals.len(), if suspend { n } else { 0 });
            assert!(result.nodes <= 6, "{}", result.nodes);
            assert!(result.facts <= 2 * n, "{}", result.facts);
            assert!(result.edge_visits <= 2 * n, "{}", result.edge_visits);
            println!(
                "straight n={n} suspend={suspend} blocks={} facts={} edges={}",
                result.nodes, result.facts, result.edge_visits
            );
        }
    }
}

#[test]
fn async_retention_branch_scaling() {
    for n in [16, 32, 64, 128] {
        for nested in [false, true] {
            let body = if nested {
                format!("{}await sleep(0);{}", "if flag {".repeat(n), "}".repeat(n))
            } else {
                "if flag { await sleep(0); } else { println(0); }".repeat(n)
            };
            let program = parse(&format!(
                "async fn check(flag: bool) {{ let x = 1; {body} println(x); }}"
            ));
            let Item::Function(f) = &program.items[0] else {
                unreachable!()
            };
            let result = analyze(
                &f.body,
                &HashMap::new(),
                &HashMap::new(),
                &Default::default(),
                &HashMap::new(),
                &HashSet::new(),
                &HashMap::new(),
            );
            assert_eq!(result.locals.len(), 1);
            assert!(result.nodes <= 10 * n, "{}", result.nodes);
            assert!(result.facts <= 10 * n, "{}", result.facts);
            assert!(result.edge_visits <= 15 * n, "{}", result.edge_visits);
            println!(
                "branch n={n} nested={nested} blocks={} facts={} edges={}",
                result.nodes, result.facts, result.edge_visits
            );
        }
    }
}

#[test]
fn async_retention_never_continuations() {
    let cases = [
        (
            "unreachable await",
            "panic(\"stop\"); await sleep(0); println(op(1));",
            false,
        ),
        (
            "unreachable read",
            "await sleep(0); panic(\"stop\"); println(op(1));",
            false,
        ),
        (
            "reachable read",
            "await sleep(0); println(op(1)); panic(\"stop\");",
            true,
        ),
        (
            "panic operand read",
            "await sleep(0); panic(op(1) == 1 ? \"a\" : \"b\");",
            true,
        ),
        (
            "panic operand await",
            "panic(await choose() ? \"a\" : \"b\"); println(op(1));",
            false,
        ),
        (
            "earlier operand across await",
            "panic(op(1) == 1 && await choose() ? \"a\" : \"b\");",
            false,
        ),
        (
            "conditional panic",
            "if flag { panic(\"stop\"); } await sleep(0); println(op(1));",
            true,
        ),
        (
            "both branches panic",
            "if flag { panic(\"a\"); } else { panic(\"b\"); } await sleep(0); println(op(1));",
            false,
        ),
        (
            "cleanup unreachable read",
            "defer { panic(\"stop\"); println(op(1)); } await sleep(0);",
            false,
        ),
        (
            "cleanup reachable read",
            "defer { println(op(1)); panic(\"stop\"); } await sleep(0);",
            true,
        ),
        (
            "cleanup conditional panic",
            "defer { if flag { panic(\"stop\"); } println(op(1)); } await sleep(0);",
            true,
        ),
        (
            "cleanup older read",
            "defer { println(op(1)); } defer { panic(\"stop\"); } await sleep(0);",
            true,
        ),
        (
            "cleanup overwrite before panic",
            "defer { println(op(1)); } defer { op = |x| x + 1; panic(\"stop\"); } await sleep(0);",
            false,
        ),
        (
            "cleanup normal and panic overwrite",
            "defer { println(op(1)); } defer { if flag { op = |x| x + 1; panic(\"stop\"); } else { op = |x| x + 2; } } await sleep(0);",
            false,
        ),
        (
            "cleanup panic without overwrite",
            "defer { println(op(1)); } defer { if flag { panic(\"stop\"); } else { op = |x| x + 2; } } await sleep(0);",
            true,
        ),
        (
            "only normal cleanup reaches following read",
            "await sleep(0); if true { defer { if flag { panic(\"stop\"); } else { op = |x| x + 1; } } } println(op(1));",
            false,
        ),
        (
            "normal cleanup preserves following read",
            "await sleep(0); if true { defer { if flag { op = |x| x + 1; panic(\"stop\"); } } } println(op(1));",
            true,
        ),
        (
            "saved panic registration",
            "defer panic(\"stop\"); await sleep(0); println(op(1));",
            true,
        ),
        (
            "saved panic older cleanup",
            "defer { println(op(1)); } defer panic(\"stop\"); await sleep(0);",
            true,
        ),
        (
            "saved panic unreachable continuation",
            "await sleep(0); if true { defer panic(\"stop\"); } println(op(1));",
            false,
        ),
        (
            "panic runs cleanup",
            "defer { println(op(1)); } await sleep(0); panic(\"stop\");",
            true,
        ),
        (
            "panic before await runs cleanup",
            "defer { println(op(1)); } panic(\"stop\"); await sleep(0);",
            false,
        ),
        (
            "recovery resumes outer scope",
            "await sleep(0); if true { defer { recover(); } panic(\"stop\"); } println(op(1));",
            true,
        ),
        (
            "recovery skips rest of inner scope",
            "if true { defer { recover(); } panic(\"stop\"); await sleep(0); println(op(1)); }",
            false,
        ),
        (
            "cleanup recovery resumes",
            "defer { if true { defer { recover(); } panic(\"stop\"); } println(op(1)); } await sleep(0);",
            true,
        ),
        (
            "cleanup recovery overwritten",
            "defer { if true { defer { op = |x| x + 1; recover(); } panic(\"stop\"); } println(op(1)); } await sleep(0);",
            false,
        ),
    ];
    for (name, body, bad) in cases {
        let program = parse(&format!(
            "async fn choose() -> bool {{ return true; }} async fn check(flag: bool) {{ let mut op: fn(i64) -> i64 = |x| x; {body} }}"
        ));
        let mut checker = TypeChecker::new();
        checker.set_enforce_send_sync(true);
        checker.check_program(&program);
        let errors: Vec<_> = checker
            .errors
            .iter()
            .filter(|e| e.severity == Severity::Error)
            .collect();
        assert_eq!(
            errors.iter().any(|e| e.code == ErrorCode::E2402),
            bad,
            "{name}: {errors:?}"
        );
        assert!(
            errors.iter().all(|e| e.code == ErrorCode::E2402),
            "{name}: {errors:?}"
        );
    }
}

#[test]
fn async_retention_unwind_cleanup_scaling() {
    for n in [16, 32, 64, 128] {
        let cleanup =
            "defer { if flag { op = |x| x + 1; panic(\"stop\"); } else { op = |x| x + 2; } }"
                .repeat(n);
        let exits = "if flag { return; }".repeat(n);
        let program = parse(&format!(
            "async fn work(flag: bool) {{ let mut op: fn(i64) -> i64 = |x| x; defer {{ println(op(1)); }} {cleanup} await sleep(0); {exits} }}"
        ));
        let mut checker = TypeChecker::new();
        checker.set_enforce_send_sync(true);
        checker.check_program(&program);
        assert!(
            !checker.errors.iter().any(|e| e.severity == Severity::Error),
            "{:?}",
            checker.errors
        );
        let Item::Function(f) = &program.items[0] else {
            unreachable!()
        };
        let result = analyze(
            &f.body,
            &checker.lambda_captures,
            &checker.expr_types,
            &checker.builtin_calls,
            &checker.analysis_calls,
            &checker.local.exhaustive_matches,
            &checker.pattern_resolutions,
        );
        assert!(result.nodes <= 10 * n + 10, "{}", result.nodes);
        assert!(result.facts <= 16 * n + 10, "{}", result.facts);
        assert!(result.edge_visits <= 24 * n + 10, "{}", result.edge_visits);
        assert!(
            result.summary_counts.0 <= 6 * n + 2,
            "{:?}",
            result.summary_counts
        );
        assert!(
            result.summary_counts.1 <= 12 * n + 2,
            "{:?}",
            result.summary_counts
        );
        println!(
            "unwind n={n} blocks={} facts={} edges={} summary={:?}",
            result.nodes, result.facts, result.edge_visits, result.summary_counts
        );
    }
}

#[test]
fn async_retention_nested_unwind_summary_scaling() {
    for n in [16, 32, 64, 128] {
        let source = format!(
            "async fn work() {{ {} recover(); panic(\"stop\"); {} await sleep(0); }}",
            "defer {".repeat(n),
            "}".repeat(n)
        );
        let program = parse(&source);
        let mut checker = TypeChecker::new();
        checker.check_program(&program);
        assert!(
            !checker.errors.iter().any(|e| e.severity == Severity::Error),
            "{:?}",
            checker.errors
        );
        let Item::Function(f) = &program.items[0] else {
            unreachable!()
        };
        let result = analyze(
            &f.body,
            &checker.lambda_captures,
            &checker.expr_types,
            &checker.builtin_calls,
            &checker.analysis_calls,
            &checker.local.exhaustive_matches,
            &checker.pattern_resolutions,
        );
        assert!(
            result.summary_counts.0 <= 10 * n,
            "{:?}",
            result.summary_counts
        );
        assert!(
            result.summary_counts.1 <= 10 * n,
            "{:?}",
            result.summary_counts
        );
        println!(
            "nested unwind n={n} blocks={} summary={:?}",
            result.nodes, result.summary_counts
        );
    }
}

#[test]
fn async_retention_exceptional_operations() {
    for operation in [
        "fail();",
        "println(values[zero]);",
        "println(1 / zero);",
        "println(1 % zero);",
        "values[zero] = 2;",
    ] {
        for context in 0..6 {
            let overwrite = "op = |x| x + 1;";
            let body = match context {
                0 => format!(
                    "defer {{ println(op(1)); }} defer {{ {operation} {overwrite} }} await sleep(0);"
                ),
                1 => format!(
                    "defer {{ println(op(1)); }} defer {{ {overwrite} {operation} }} await sleep(0);"
                ),
                2 => format!(
                    "await sleep(0); if true {{ defer {{ recover(); }} {operation} {overwrite} }} println(op(1));"
                ),
                3 => format!(
                    "await sleep(0); if true {{ defer {{ recover(); }} {overwrite} {operation} }} println(op(1));"
                ),
                4 => format!("await sleep(0); {operation} {overwrite} println(op(1));"),
                _ => format!("defer {{ println(op(1)); }} defer {{ {operation} {overwrite} }}"),
            };
            let program = parse(&format!(
                "fn fail() {{ panic(\"stop\"); }} async fn check(zero: i64) {{ let mut values = [1]; let mut op: fn(i64) -> i64 = |x| x; {body} }}"
            ));
            let mut checker = TypeChecker::new();
            checker.set_enforce_send_sync(true);
            checker.check_program(&program);
            let errors: Vec<_> = checker
                .errors
                .iter()
                .filter(|e| e.severity == Severity::Error)
                .collect();
            assert_eq!(
                errors.iter().any(|e| e.code == ErrorCode::E2402),
                matches!(context, 0 | 2),
                "{operation} context={context}: {errors:?}"
            );
            assert!(
                errors.iter().all(|e| e.code == ErrorCode::E2402),
                "{operation} context={context}: {errors:?}"
            );
        }
    }
}

#[test]
fn async_retention_select_commit_unwinds_before_case() {
    for binding in [false, true] {
        for overwrite_first in [false, true] {
            for has_default in [false, true] {
                let arm = if binding {
                    "let value = await t"
                } else {
                    "await t"
                };
                let before = if overwrite_first {
                    "op = |x| x + 1;"
                } else {
                    ""
                };
                let default = if has_default {
                    "default => { op = |x| x + 2; }"
                } else {
                    ""
                };
                let source = format!(
                    "async fn slow() -> i64 {{ await sleep(1000); return 1; }} async fn check() {{ let t = slow(); t.cancel(); let mut op: fn(i64) -> i64 = |x| x; await sleep(0); if true {{ defer {{ recover(); }} {before} select {{ {arm} => {{ op = |x| x + 1; }} {default} }} }} println(op(1)); }}"
                );
                let program = parse(&source);
                let mut checker = TypeChecker::new();
                checker.set_enforce_send_sync(true);
                checker.check_program(&program);
                let errors: Vec<_> = checker
                    .errors
                    .iter()
                    .filter(|e| e.severity == Severity::Error)
                    .collect();
                assert_eq!(
                    errors.iter().any(|e| e.code == ErrorCode::E2402),
                    !overwrite_first || !has_default,
                    "binding={binding} before={overwrite_first} default={default}: {errors:?}"
                );
                assert!(
                    errors.iter().all(|e| e.code == ErrorCode::E2402),
                    "{errors:?}"
                );
            }
        }
    }
}

#[test]
fn async_retention_sparse_scaling() {
    for n in [16, 32, 64, 128] {
        let body = (0..n)
            .map(|i| format!("let x{i} = {i}; println(x{i}); await sleep(0);"))
            .collect::<String>();
        let program = parse(&format!("async fn check() {{ {body} }}"));
        let Item::Function(f) = &program.items[0] else {
            unreachable!()
        };
        let result = analyze(
            &f.body,
            &HashMap::new(),
            &HashMap::new(),
            &Default::default(),
            &HashMap::new(),
            &HashSet::new(),
            &HashMap::new(),
        );
        assert!(result.locals.is_empty());
        assert!(result.nodes <= 20 * n + 3, "{}", result.nodes);
        assert!(result.facts <= 8 * n, "{}", result.facts);
        assert!(result.edge_visits <= 10 * n, "{}", result.edge_visits);
        println!(
            "n={n} nodes={} facts={} edges={}",
            result.nodes, result.facts, result.edge_visits
        );
    }
}
