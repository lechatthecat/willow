//! Task loans inside blocks, branches and loops.
//!
//! A loan ends at an `await` that runs unconditionally in the same block
//! instance as the call that lent the value. Inside a loop only values created
//! in the current iteration can be lent.
use crate::diagnostics::{Diagnostic, ErrorCode, Severity};

const PRELUDE: &str = "import std::collections::Array;
class Box { pub n: i64; }
@sending fn mk() -> Array<i64> { return [1, 2, 3]; }
fn plain() -> Array<i64> { return [1, 2, 3]; }
async fn take(xs: Array<i64>) -> i64 { return xs.len(); }
async fn boxed(b: Box) -> i64 { b.n = 2; return b.n; }
";

fn errors(body: &str) -> Vec<Diagnostic> {
    let source = format!("{PRELUDE}\nasync fn main() {{ {body} }}");
    let tokens = crate::lexer::Lexer::new(&source).tokenize().expect("lex");
    let (program, parse_errors) = crate::parser::Parser::new(tokens).parse();
    assert!(parse_errors.is_empty(), "{source}: {parse_errors:?}");
    let mut checker = crate::semantic::TypeChecker::new();
    checker.set_enforce_send_sync(true);
    crate::register_prelude(&mut checker).expect("prelude");
    checker.check_program(&program);
    checker
        .errors
        .into_iter()
        .filter(|d| d.severity == Severity::Error)
        .collect()
}

#[test]
fn task_loans_follow_blocks_and_iterations() {
    for (name, body) in [
        (
            "iteration literal",
            "for i in 0..2 { let c: Array<i64> = [i]; println(await take(c)); }",
        ),
        (
            "iteration task awaited later",
            "for i in 0..2 { let c = [i]; let t = take(c); println(i); println(await t); println(c[0]); }",
        ),
        (
            "while iteration",
            "let mut i = 0; while i < 2 { let c = [i]; await take(c); c.push(1); i += 1; }",
        ),
        (
            "nested loop value",
            "for i in 0..2 { for j in 0..2 { let c = [i, j]; await take(c); println(c[1]); } }",
        ),
        (
            "branch loan",
            "let a = [1]; if a.len() > 0 { await take(a); } println(a[0]);",
        ),
        (
            "else branch loan",
            "let a = [1]; if a.len() > 5 { println(0); } else { let t = take(a); await t; } println(a[0]);",
        ),
        (
            "match arm loan",
            "let a = [1]; match a.len() { 1 => { await take(a); } _ => {} } println(a[0]);",
        ),
        (
            "detached iteration task",
            "let tasks: Array<Task<i64>> = []; for i in 0..3 { let c = [i]; tasks.push(take(c)); } for t in tasks { println(await t); }",
        ),
        (
            "sending result",
            "let a = mk(); println(await take(a)); println(a[0]);",
        ),
        ("sending argument", "println(await take(mk()));"),
        (
            "sending result per iteration",
            "for i in 0..2 { let a = mk(); await take(a); a.push(i); }",
        ),
        (
            "continue before await",
            "for i in 0..3 { let c = [i]; let t = take(c); if i == 1 { continue; } await t; println(c[0]); }",
        ),
        (
            "break after await",
            "for i in 0..3 { let c = [i]; await take(c); if c[0] == 1 { break; } }",
        ),
        (
            "class per iteration",
            "for i in 0..2 { let b = new Box(i); println(await boxed(b)); println(b.n); }",
        ),
        (
            "if-let branch value",
            "if let Option::Some(n) = Option::Some(1) { let a = [n]; let t = take(a); await t; println(a[0]); }",
        ),
    ] {
        let found = errors(body);
        assert!(found.is_empty(), "{name}: {found:?}");
    }
}

#[test]
fn task_loans_reject_unsound_paths_with_a_specific_reason() {
    for (name, body, message, help) in [
        (
            "outer value in loop",
            "let a = [1]; for i in 0..2 { await take(a); }",
            "created outside this loop iteration",
            "inside the loop body",
        ),
        (
            "outer iteration value in inner loop",
            "for i in 0..2 { let c = [i]; for j in 0..2 { await take(c); } }",
            "created outside this loop iteration",
            "inside the loop body",
        ),
        (
            "outer class value in loop",
            "let b = new Box(0); for i in 0..2 { println(await boxed(b)); }",
            "created outside this loop iteration",
            "inside the loop body",
        ),
        (
            "iteration value joined with outer alias",
            "let mut o = [0]; for i in 0..2 { let c = [i]; o = c; await take(c); }",
            "created outside this loop iteration",
            "inside the loop body",
        ),
        (
            "await only in nested branch",
            "for i in 0..2 { let c = [i]; let t = take(c); if i > 0 { await t; } println(c[0]); }",
            "stays lent",
            "same block",
        ),
        (
            "branch loan awaited after branch",
            "let a = [1]; let mut t = take([0]); if a.len() > 0 { t = take(a); } await t; println(a[0]);",
            "stays lent",
            "same block",
        ),
        (
            "await in short-circuit operand",
            "let a = [1]; let t = take(a); let ok = a.len() > 5 && (await t) > 0; println(a[0]);",
            "",
            "",
        ),
        (
            "use before await in iteration",
            "for i in 0..2 { let c = [i]; let t = take(c); println(c[0]); await t; }",
            "unavailable until its task has been awaited",
            "await the task",
        ),
        (
            "second loan in iteration",
            "for i in 0..2 { let c = [i]; let t = take(c); let u = take(c); await t; await u; }",
            "outstanding task loan",
            "await the task",
        ),
        (
            "unannotated call result",
            "let d = plain(); println(await take(d));",
            "cannot lend a borrowed non-Sync value",
            "`@sending`",
        ),
        (
            "unannotated argument",
            "println(await take(plain()));",
            "cannot lend a borrowed non-Sync value",
            "`@sending`",
        ),
        (
            "escaped alias in branch",
            "let a = [1]; let h: Array<Array<i64>> = []; if a.len() > 0 { h.push(a); await take(a); }",
            "alias may have escaped",
            "await the task",
        ),
    ] {
        let found = errors(body);
        let hit = found.iter().find(|d| {
            matches!(d.code, ErrorCode::E2402 | ErrorCode::E2408)
                && (d.message.contains(message) || d.notes.iter().any(|n| n.contains(message)))
        });
        let Some(hit) = hit else {
            panic!("{name}: {found:?}");
        };
        let helps = hit.helps.join("\n");
        assert!(helps.contains(help), "{name}: {helps}");
        assert!(
            !helps.contains("use a fresh local Array/Map and await its task"),
            "{name}: help recommends the rejected shape: {helps}"
        );
    }
}

#[test]
fn task_loans_report_one_primary_error_per_value() {
    for body in [
        "let a = [1]; for i in 0..2 { await take(a); }",
        "for i in 0..2 { let c = [i]; let t = take(c); let u = take(c); await t; await u; }",
        "let d = plain(); for i in 0..2 { await take(d); }",
    ] {
        let found = errors(body);
        let mut spans: Vec<_> = found
            .iter()
            .filter(|d| d.code == ErrorCode::E2402)
            .map(|d| d.labels[0].span)
            .collect();
        let total = spans.len();
        spans.sort_by_key(|s| (s.start, s.end));
        spans.dedup();
        assert!(total >= 1 && total == spans.len(), "{body}: {found:?}");
    }
}

#[test]
fn task_loan_flow_scales_linearly() {
    let mut per_size = Vec::new();
    for size in [8usize, 32, 128] {
        let mut body = String::new();
        // Many loops, each with a branch loan and an iteration loan, plus one
        // loop nest `size` deep.
        for i in 0..size {
            body.push_str(&format!(
                "for k{i} in 0..2 {{ let c{i} = [k{i}]; let t{i} = take(c{i}); await t{i}; if c{i}[0] > 0 {{ await take(c{i}); }} }}"
            ));
        }
        for i in 0..size {
            body.push_str(&format!("for d{i} in 0..2 {{ "));
        }
        body.push_str("let deep = [1]; await take(deep);");
        for _ in 0..size {
            body.push('}');
        }
        super::transfer_ownership::TOTAL_COUNTS.with(|c| c.set((0, 0, 0)));
        let found = errors(&body);
        assert!(found.is_empty(), "{found:?}");
        let counts = super::transfer_ownership::TOTAL_COUNTS.with(|c| c.get());
        eprintln!(
            "loan flow n={size}: events={} nodes={} root_steps={}",
            counts.0, counts.1, counts.2
        );
        per_size.push((size, counts));
    }
    let (base, (t0, n0, r0)) = per_size[0];
    for (size, (ticks, nodes, roots)) in &per_size[1..] {
        let factor = size / base;
        assert!(
            *ticks <= factor * t0 && *nodes <= factor * n0 && *roots <= factor * r0.max(1),
            "superlinear growth: {per_size:?}"
        );
    }
}
