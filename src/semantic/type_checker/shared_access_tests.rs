//! Task-boundary safety: rejected sharing and supported ownership patterns.
use crate::diagnostics::label::LabelKind;
use crate::diagnostics::{Diagnostic, ErrorCode, Severity};

const DECLARATIONS: &str = r#"
import std::collections::Array;
class Cell { pub n: i64; pub fn set(self, n: i64) { self.n = n; } }
class Holder { pub cell: Cell; }
class Immutable { pub n: i64; }
async fn change(c: Cell) { c.set(1); }
async fn nested(h: Holder) { h.cell.set(1); }
async fn array(xs: Array<i64>) { xs[0] = 1; }
async fn locked(m: Mutex<Cell>) { lock m as c { c.set(1); } }
async fn receive(ch: Channel<Cell>) { let c = ch.recv(); c.set(1); }
async fn read(c: Immutable) -> i64 { return c.n; }
async fn local() { let c = new Cell(0); c.set(1); }
async fn pair(a: Cell, b: Cell) { a.set(1); b.set(2); }
fn observe(c: Cell) { println(c.n); }
"#;

fn check(body: &str) -> (String, Vec<Diagnostic>) {
    let source = format!("{DECLARATIONS}\nasync fn main() {{ {body} }}");
    let tokens = crate::lexer::Lexer::new(&source).tokenize().unwrap();
    let (program, errors) = crate::parser::Parser::new(tokens).parse();
    assert!(errors.is_empty(), "{errors:?}");
    let mut checker = super::TypeChecker::new();
    checker.set_enforce_send_sync(true);
    crate::register_prelude(&mut checker).unwrap();
    checker.check_program(&program);
    let errors = checker
        .errors
        .into_iter()
        .filter(|d| d.severity == Severity::Error)
        .collect();
    (source, errors)
}

#[test]
fn shared_access_rejects_with_boundary_witnesses() {
    // Each case identifies the earlier ownership boundary, not just an error code.
    for (name, body, code, boundary) in [
        (
            "two writers",
            "let c = new Cell(0); let a = change(c); let b = change(c); await a; await b;",
            ErrorCode::E2402,
            "c",
        ),
        (
            "writer and parent reader",
            "let c = new Cell(0); let a = change(c); println(c.n); await a;",
            ErrorCode::E2402,
            "c",
        ),
        (
            "writer and parent writer",
            "let c = new Cell(0); let a = change(c); c.set(2); await a;",
            ErrorCode::E2402,
            "c",
        ),
        (
            "capture alias",
            "let c = new Cell(0); let alias = c; let a = change(c); alias.set(2); await a;",
            ErrorCode::E2402,
            "c",
        ),
        (
            "shared field",
            "let h = new Holder(new Cell(0)); let a = nested(h); h.cell.set(2); await a;",
            ErrorCode::E2402,
            "h",
        ),
        (
            "field alias",
            "let h = new Holder(new Cell(0)); let alias = h.cell; let a = nested(h); alias.set(2); await a;",
            ErrorCode::E2402,
            "h",
        ),
        (
            "array writer",
            "let xs = [0]; let a = array(xs); xs[0] = 2; await a;",
            ErrorCode::E2402,
            "xs",
        ),
        (
            "channel writer",
            "let ch: Channel<Cell> = Channel::new(); let a = receive(ch); let c = new Cell(0); ch.send(c); c.set(2); await a;",
            ErrorCode::E2408,
            "c",
        ),
        (
            "channel reader",
            "let ch: Channel<Cell> = Channel::new(); let c = new Cell(0); ch.send(c); println(c.n);",
            ErrorCode::E2408,
            "c",
        ),
        (
            "channel alias",
            "let ch: Channel<Cell> = Channel::new(); let c = new Cell(0); let alias = c; ch.send(c); alias.set(2);",
            ErrorCode::E2408,
            "c",
        ),
        (
            "channel nested alias",
            "let ch: Channel<Holder> = Channel::new(); let h = new Holder(new Cell(0)); let alias = h.cell; ch.send(h); alias.set(2);",
            ErrorCode::E2408,
            "h",
        ),
        (
            "two channel transfers",
            "let ch: Channel<Cell> = Channel::new(); let c = new Cell(0); ch.send(c); ch.send(c);",
            ErrorCode::E2408,
            "c",
        ),
        (
            "mutex old alias",
            "let c = new Cell(0); let m = Mutex::new(c); let a = locked(m); c.set(2); await a;",
            ErrorCode::E2408,
            "c",
        ),
    ] {
        let (source, errors) = check(body);
        let diagnostic = errors
            .iter()
            .find(|d| {
                d.code == code
                    && d.labels.iter().any(|l| {
                        l.kind == LabelKind::Secondary && l.message.contains("ownership boundary")
                    })
            })
            .unwrap_or_else(|| panic!("{name}: no boundary witness: {errors:?}"));
        let witness = diagnostic
            .labels
            .iter()
            .find(|l| l.kind == LabelKind::Secondary && l.message.contains("ownership boundary"))
            .unwrap();
        assert_eq!(
            &source[witness.span.start..witness.span.end],
            boundary,
            "{name}"
        );
        assert!(
            witness.span.start < diagnostic.primary_span().unwrap().start,
            "{name}"
        );
    }
}

#[test]
fn shared_access_accepts_safe_patterns() {
    for (name, body) in [
        (
            "mutex writers",
            "let m = Mutex::new(new Cell(0)); let a = locked(m); let b = locked(m); await a; await b;",
        ),
        (
            "channel transfer",
            "let ch: Channel<Cell> = Channel::new(); let a = receive(ch); ch.send(new Cell(0)); await a;",
        ),
        (
            "immutable sharing",
            "let c = new Immutable(7); let a = read(c); let b = read(c); println(await a + await b);",
        ),
        (
            "independent task locals",
            "let a = local(); let b = local(); await a; await b;",
        ),
        (
            "sequential loan",
            "let c = new Cell(0); await change(c); c.set(2); await change(c);",
        ),
        (
            "alias after join",
            "let c = new Cell(0); let alias = c; let a = change(c); await a; alias.set(2);",
        ),
        (
            "independent objects",
            "let x = new Cell(0); let y = new Cell(0); let a = change(x); let b = change(y); await a; await b;",
        ),
        (
            "channel receive owns",
            "let ch: Channel<Cell> = Channel::new(); ch.send(new Cell(0)); let c = ch.recv(); c.set(2); let m = Mutex::new(c); await locked(m);",
        ),
        (
            "field after join",
            "let h = new Holder(new Cell(0)); await nested(h); h.cell.set(2);",
        ),
        (
            "local mutation",
            "let c = new Cell(0); c.set(2); println(c.n);",
        ),
        (
            "scalar copied from lock",
            "let m = Mutex::new(new Cell(0)); let mut n = 0; lock m as c { c.set(2); n = c.n; } println(n);",
        ),
        (
            "array after join",
            "let xs = [0]; await array(xs); xs[0] = 2;",
        ),
    ] {
        let (_, errors) = check(body);
        assert!(errors.is_empty(), "{name}: {errors:?}");
    }
}

#[test]
fn shared_access_witness_distinguishes_arguments_of_one_task() {
    for used in ["left", "right"] {
        let (source, errors) = check(&format!(
            "let left = new Cell(0); let right = new Cell(0); let t = pair(left, right); {used}.set(3); await t;"
        ));
        let diagnostic = errors.iter().find(|d| d.code == ErrorCode::E2402).unwrap();
        let origin = diagnostic
            .labels
            .iter()
            .find(|l| l.message.contains("ownership boundary"))
            .unwrap();
        assert_eq!(
            &source[origin.span.start..origin.span.end],
            used,
            "{diagnostic:?}"
        );
    }
}

#[test]
fn shared_access_independent_scaling_dimensions() {
    use super::transfer_ownership::TOTAL_COUNTS;
    for shape in [
        "objects",
        "escape_edges",
        "tasks",
        "alias_chain",
        "alias_fanout",
        "conflicts",
    ] {
        for n in [16, 64, 256] {
            let mut body = String::from("let c0 = new Cell(0); let owned = new Cell(0);");
            for i in 0..n {
                match shape {
                    "objects" => body.push_str(&format!("let object{i} = new Cell(0);")),
                    "escape_edges" => body.push_str("observe(c0);"),
                    "tasks" => body.push_str("await change(owned);"),
                    "alias_chain" => body.push_str(&format!("let c{} = c{i};", i + 1)),
                    "alias_fanout" => body.push_str(&format!("let a{i} = c0;")),
                    _ => body.push_str(&format!(
                        "let v{i} = new Cell(0); let t{i} = change(v{i}); v{i}.set(2); await t{i};"
                    )),
                }
            }
            body.push_str("await change(owned);");
            TOTAL_COUNTS.with(|c| c.set((0, 0, 0)));
            let (_, errors) = check(&body);
            if shape == "conflicts" {
                assert_eq!(errors.len(), n);
                assert!(errors.iter().all(|d| {
                    d.labels
                        .iter()
                        .any(|l| l.message.contains("ownership boundary"))
                }));
            } else {
                assert!(errors.is_empty(), "{shape}: {errors:?}");
            }
            let (events, nodes, roots) = TOTAL_COUNTS.with(|c| c.get());
            eprintln!(
                "shared access {shape} n={n}: events={events} nodes={nodes} root_steps={roots} diagnostics={}",
                errors.len()
            );
            assert!(events <= 60 * n + 500, "{shape}: {events}");
            assert!(nodes <= 12 * n + 100, "{shape}: {nodes}");
            assert!(roots <= 30 * n + 200, "{shape}: {roots}");
        }
    }
}
