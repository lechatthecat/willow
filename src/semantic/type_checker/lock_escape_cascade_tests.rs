//! A protected non-Sync value that leaves its lock is one mistake. The checker
//! reports where it left the lock (or its first escaping use) and counts the
//! later uses of its aliases on that error instead of reporting each one.
use crate::diagnostics::{Diagnostic, ErrorCode, Severity};

const PRELUDE: &str = "import std::collections::Array;
class Shared { pub params: Array<f64>; pub names: Array<String>; }
class Holder { pub xs: Array<f64>; }
fn total(xs: Array<f64>) -> f64 { let mut s = 0.0; for x in xs { s += x; } return s; }
fn same(xs: Array<f64>) -> Array<f64> { return xs; }
";

const STORED: &str = "protected non-Sync value is stored outside its lock scope";
const OPERATION: &str = "protected non-Sync value may escape through this operation";

fn check(body: &str) -> (String, Vec<Diagnostic>) {
    let source = format!(
        "{PRELUDE}async fn work(m: Mutex<Shared>) -> f64 {{ {body} }}\nasync fn main() {{ println(await work(Mutex::new(new Shared([1.0], [\"a\"])))); }}"
    );
    let tokens = crate::lexer::Lexer::new(&source).tokenize().expect("lex");
    let (program, parse_errors) = crate::parser::Parser::new(tokens).parse();
    assert!(parse_errors.is_empty(), "{source}: {parse_errors:?}");
    let mut checker = crate::semantic::TypeChecker::new();
    checker.set_enforce_send_sync(true);
    crate::register_prelude(&mut checker).expect("prelude");
    checker.check_program(&program);
    let errors = checker
        .errors
        .into_iter()
        .filter(|d| d.severity == Severity::Error)
        .collect();
    (source, errors)
}

/// The reported lock escapes as `(message, covered text, dependent count)`.
fn escapes(source: &str, errors: &[Diagnostic]) -> Vec<(String, String, usize)> {
    errors
        .iter()
        .filter(|d| d.code == ErrorCode::E2408)
        .map(|d| {
            let span = d.labels[0].span;
            let dependent = d
                .notes
                .iter()
                .find_map(|n| n.split(" later use").next()?.parse().ok())
                .unwrap_or(0);
            (
                d.message.clone(),
                source[span.start..span.end].to_string(),
                dependent,
            )
        })
        .collect()
}

#[test]
fn one_escaped_value_reports_its_store_once() {
    for (name, body, dependent) in [
        (
            "no later use",
            "let mut snap: Array<f64> = []; lock m as r { snap = r.params; } return 0.0;",
            0,
        ),
        (
            "for loop over the value",
            "let mut snap: Array<f64> = []; lock m as r { snap = r.params; } let d: Array<f64> = []; for x in snap { d.push(x * 2.0); } return total(d);",
            1,
        ),
        (
            "assignment chain",
            "let mut snap: Array<f64> = []; lock m as r { snap = r.params; } let a = snap; let b = a; return total(b) + total(snap);",
            4,
        ),
        (
            "call result",
            "let mut snap: Array<f64> = []; lock m as r { snap = r.params; } let c = same(snap); for x in c { println(x); } return total(c);",
            3,
        ),
        (
            "uses inside a loop body",
            "let mut s: Array<f64> = []; lock m as r { s = r.params; } let mut t = 0.0; for i in 0..3 { t += s[0]; s.push(1.0); } return t;",
            2,
        ),
        (
            "while loop",
            "let mut s: Array<f64> = []; lock m as r { s = r.params; } let mut i = 0; while i < s.len() { i += 1; } return total(s);",
            2,
        ),
        (
            "branch uses",
            "let mut s: Array<f64> = []; lock m as r { s = r.params; } if s.len() > 0 { return s[0]; } else { return total(s); }",
            3,
        ),
        (
            "match on a derived value",
            "let mut s: Array<f64> = []; lock m as r { s = r.params; } let c = same(s); return match c.len() { 0 => 0.0, _ => c[0] };",
            3,
        ),
        (
            "write binding",
            "let mut s: Array<f64> = []; lock m as mut w { s = w.params; } return total(s);",
            1,
        ),
        (
            "closure capture",
            "let mut s: Array<f64> = []; lock m as r { s = r.params; } let f = || total(s); return f() + f();",
            1,
        ),
        (
            "array element",
            "let mut s: Array<f64> = []; lock m as r { s = r.params; } let first = s[0]; return first + s[0];",
            2,
        ),
    ] {
        let covered = "params";
        let (source, errors) = check(body);
        let found = escapes(&source, &errors);
        assert_eq!(
            found,
            [(STORED.to_string(), covered.to_string(), dependent)],
            "{name}: {errors:?}"
        );
        assert_eq!(errors.len(), 1, "{name}: {errors:?}");
    }
}

#[test]
fn escape_without_a_store_reports_the_first_use() {
    for (name, body, message, covered, dependent) in [
        (
            "pushed into an outer array",
            "let hs: Array<Array<f64>> = []; lock m as r { hs.push(r.params); } return total(hs[0]) + total(hs[0]);",
            OPERATION,
            "params",
            4,
        ),
        (
            "stored in an outer object field",
            "let h = new Holder([]); lock m as r { h.xs = r.params; } return total(h.xs) + total(h.xs);",
            OPERATION,
            "params",
            4,
        ),
    ] {
        let (source, errors) = check(body);
        let found = escapes(&source, &errors);
        assert_eq!(
            found,
            [(message.to_string(), covered.to_string(), dependent)],
            "{name}: {errors:?}"
        );
        assert_eq!(errors.len(), 1, "{name}: {errors:?}");
    }
}

#[test]
fn separate_mistakes_are_each_reported_in_source_order() {
    for (name, body, expected) in [
        (
            "two stores in one lock",
            "let mut s: Array<f64> = []; let mut n: Array<String> = []; lock m as r { s = r.params; n = r.names; } println(n[0]); return total(s);",
            vec![(STORED, "params", 2), (STORED, "names", 0)],
        ),
        (
            "two locks",
            "let mut a: Array<f64> = []; lock m as r { a = r.params; } let mut b: Array<f64> = []; lock m as q { b = q.params; } return total(b) + total(a);",
            vec![(STORED, "params", 1), (STORED, "params", 1)],
        ),
    ] {
        let (source, errors) = check(body);
        let found = escapes(&source, &errors);
        let expected: Vec<_> = expected
            .into_iter()
            .map(|(m, c, d)| (m.to_string(), c.to_string(), d))
            .collect();
        assert_eq!(found, expected, "{name}: {errors:?}");
    }
}

#[test]
fn copying_out_of_the_lock_is_accepted() {
    let (source, errors) = check(
        "let snap: Array<f64> = []; lock m as r { for x in r.params { snap.push(x); } } return total(snap);",
    );
    assert!(errors.is_empty(), "{source}: {errors:?}");
}

#[test]
fn rwlock_escape_reports_the_store_first() {
    // The original shape: the store is the mistake and the loop its
    // consequence; `RwLock<Shared>` not being Sync is a separate error.
    let source = format!(
        "{PRELUDE}async fn reader(shared: RwLock<Shared>) -> f64 {{
    let mut snap: Array<f64> = [];
    lock read shared as r {{ snap = r.params; }}
    let doubled: Array<f64> = [];
    for x in snap {{ doubled.push(x * 2.0); }}
    return total(doubled);
}}
async fn main() {{ println(await reader(RwLock::new(new Shared([1.0, 2.0], [\"a\"])))); }}"
    );
    let tokens = crate::lexer::Lexer::new(&source).tokenize().expect("lex");
    let (program, parse_errors) = crate::parser::Parser::new(tokens).parse();
    assert!(parse_errors.is_empty(), "{parse_errors:?}");
    let mut checker = crate::semantic::TypeChecker::new();
    checker.set_enforce_send_sync(true);
    crate::register_prelude(&mut checker).expect("prelude");
    checker.check_program(&program);
    let errors: Vec<_> = checker
        .errors
        .into_iter()
        .filter(|d| d.severity == Severity::Error)
        .collect();
    assert_eq!(errors[0].message, STORED, "{errors:?}");
    assert!(
        errors[0]
            .notes
            .iter()
            .any(|n| n == "1 later use depends on this escape and is not reported separately"),
        "{:?}",
        errors[0].notes
    );
    assert!(
        errors[1..].iter().all(|d| d.code == ErrorCode::E2402),
        "{errors:?}"
    );
}

#[test]
fn many_dependent_uses_still_report_one_error() {
    for size in [4usize, 64] {
        let mut body = String::from(
            "let mut s: Array<f64> = []; lock m as r { s = r.params; } let mut t = 0.0; ",
        );
        for _ in 0..size {
            body.push_str("t += total(s); ");
        }
        body.push_str("return t;");
        let (source, errors) = check(&body);
        assert_eq!(
            escapes(&source, &errors),
            [(STORED.to_string(), "params".to_string(), size)],
            "{size}: {errors:?}"
        );
    }
}
