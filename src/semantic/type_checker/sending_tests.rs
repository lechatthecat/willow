//! `@sending` ownership contracts: fresh results and consumed parameters.
use crate::diagnostics::{Diagnostic, ErrorCode, Severity};

const PRELUDE: &str = "import std::collections::Array;
class Res { pub grads: Array<f64>; pub n: i64; }
class Holder { pub a: Array<i64>; }
class G { pub static mut SHARED: Array<i64> = []; }
fn opaque(a: Array<i64>) {}
fn plain() -> Array<i64> { return [1]; }
@sending fn fresh() -> Array<i64> { return [1, 2]; }
@sending fn fresh_res(n: i64) -> Result<Res, String> {
    let g: Array<f64> = [];
    for i in 0..n { g.push(1.0); }
    return Ok(new Res(g, n));
}
@sending fn maybe() -> Option<Array<i64>> { return Some([3]); }
@sending fn nested() -> Result<Option<Array<i64>>, String> { return Ok(Some([4])); }
fn give(@sending a: Array<i64>, c: Channel<Array<i64>>) { c.send(a); }
async fn take(a: Array<i64>) { a[0] = 7; }
";

fn errors(source: &str) -> Vec<Diagnostic> {
    let source = format!("{PRELUDE}\n{source}");
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

fn parse_error(source: &str) -> String {
    let tokens = crate::lexer::Lexer::new(source).tokenize().expect("lex");
    let (_, parse_errors) = crate::parser::Parser::new(tokens).parse();
    assert!(!parse_errors.is_empty(), "{source}: accepted");
    parse_errors[0].message.clone()
}

#[test]
fn sending_results_are_owned_by_the_caller() {
    for (name, body) in [
        (
            "plain result",
            "async fn main() { let c: Channel<Array<i64>> = Channel::new(); c.send(fresh()); }",
        ),
        (
            "Ok binding",
            "async fn w(out: Channel<Res>) { match fresh_res(3) { Ok(g) => { out.send(g); } Err(_) => { out.close(); } } }",
        ),
        (
            "Some binding",
            "async fn w(c: Channel<Array<i64>>) { match maybe() { Some(a) => { c.send(a); } None => {} } }",
        ),
        (
            "nested payload",
            "async fn w(c: Channel<Array<i64>>) { match nested() { Ok(Some(a)) => { c.send(a); } _ => {} } }",
        ),
        (
            "try propagation",
            "fn w(out: Channel<Res>) -> Result<i64, String> { let g = fresh_res(2)?; out.send(g); return Ok(1); }",
        ),
        (
            "let binding",
            "async fn main() { let c: Channel<Array<i64>> = Channel::new(); let a = fresh(); c.send(a); }",
        ),
        (
            "task loan",
            "async fn main() { let a = fresh(); await take(a); println(a[0]); }",
        ),
        (
            "chained sending",
            "@sending fn again() -> Array<i64> { let a = fresh(); a.push(3); return a; }",
        ),
        (
            "returns own parameter",
            "@sending fn pass(@sending a: Array<i64>) -> Array<i64> { return a; }",
        ),
        (
            "contained fresh value",
            "@sending fn hold() -> Holder { let a = [1]; return new Holder(a); }",
        ),
        (
            "sync result",
            "@sending fn count() -> i64 { return 1; } fn main() { println(count()); }",
        ),
        (
            "async sending awaited",
            "@sending async fn later() -> Array<i64> { return [5]; } async fn main() { let c: Channel<Array<i64>> = Channel::new(); c.send(await later()); }",
        ),
        (
            "sending parameter",
            "async fn main() { let c: Channel<Array<i64>> = Channel::new(); let a = [1]; give(a, c); }",
        ),
        (
            "sending parameter into task",
            "async fn eat(@sending r: Res) { println(r.n); } async fn main() { let t = eat(new Res([], 1)); await t; }",
        ),
        (
            "fresh branches",
            "@sending fn pick(b: bool) -> Array<i64> { if b { return [1]; } let x = fresh(); return x; }",
        ),
    ] {
        let found = errors(body);
        assert!(found.is_empty(), "{name}: {found:?}");
    }
}

#[test]
fn sending_rejects_aliased_values() {
    for (name, body, message) in [
        (
            "returns borrowed parameter",
            "@sending fn leak(a: Array<i64>) -> Array<i64> { return a; }",
            "`@sending` function cannot return a borrowed",
        ),
        (
            "returns global",
            "@sending fn leak() -> Array<i64> { return G::SHARED; }",
            "`@sending` function cannot return a borrowed",
        ),
        (
            "returns plain call result",
            "@sending fn leak() -> Array<i64> { return plain(); }",
            "`@sending` function cannot return a borrowed",
        ),
        (
            "returns stored alias",
            "@sending fn leak(h: Holder) -> Array<i64> { let a = [1]; h.a = a; return a; }",
            "`@sending` function cannot return",
        ),
        (
            "returns opaque argument",
            "@sending fn leak() -> Array<i64> { let a = [1]; opaque(a); return a; }",
            "alias may have escaped",
        ),
        (
            "returns captured value",
            "@sending fn leak() -> Array<i64> { let a = [1]; let f: fn() -> i64 = || a[0]; return a; }",
            "alias may have escaped",
        ),
        (
            "returns field of parameter",
            "@sending fn leak(h: Holder) -> Array<i64> { return h.a; }",
            "`@sending` function cannot return a borrowed",
        ),
        (
            "unannotated producer",
            "async fn main() { let c: Channel<Array<i64>> = Channel::new(); c.send(plain()); }",
            "cannot transfer a borrowed non-Sync value",
        ),
        (
            "use after sending argument",
            "async fn main() { let c: Channel<Array<i64>> = Channel::new(); let a = [1]; give(a, c); println(a[0]); }",
            "use of a non-Sync value after ownership transfer",
        ),
        (
            "alias used after send",
            "async fn main() { let c: Channel<Array<i64>> = Channel::new(); let a = fresh(); let b = a; c.send(a); println(b[0]); }",
            "use of a non-Sync value after ownership transfer",
        ),
        (
            "borrowed sending argument",
            "fn relay(a: Array<i64>, c: Channel<Array<i64>>) { give(a, c); }",
            "cannot transfer a borrowed non-Sync value",
        ),
        (
            "sending argument twice",
            "fn two(@sending a: Array<i64>, @sending b: Array<i64>) {} fn main() { let a = [1]; two(a, a); }",
            "transferred more than once",
        ),
        (
            "outer value in loop",
            "async fn main() { let c: Channel<Array<i64>> = Channel::new(); let a = [1]; for i in 0..2 { give(a, c); } }",
            "outer non-Sync value in a repeated loop",
        ),
        (
            "function value forgets sending",
            "async fn main() { let c: Channel<Array<i64>> = Channel::new(); let f: fn() -> Array<i64> = fresh; c.send(f()); }",
            "cannot transfer a borrowed non-Sync value",
        ),
        (
            "loan and transfer in one call",
            "async fn both(a: Array<i64>, @sending b: Array<i64>) {} async fn main() { let a = [1]; await both(a, a); }",
            "after permanent transfer",
        ),
    ] {
        let found = errors(body);
        assert!(
            found
                .iter()
                .any(|d| matches!(d.code, ErrorCode::E2402 | ErrorCode::E2408)
                    && d.message.contains(message)),
            "{name}: {found:?}"
        );
    }
}

#[test]
fn sending_help_names_the_contract_not_locks() {
    let found =
        errors("async fn main() { let c: Channel<Array<i64>> = Channel::new(); c.send(plain()); }");
    let help = found
        .iter()
        .find(|d| d.code == ErrorCode::E2408)
        .map(|d| d.helps.join("\n"))
        .expect("E2408 with help");
    assert!(help.contains("@sending"), "{help}");
    assert!(!help.contains("lock"), "{help}");
    let found = errors("@sending fn leak(a: Array<i64>) -> Array<i64> { return a; }");
    let help = found[0].helps.join("\n");
    assert!(
        help.contains("`@sending`") && !help.contains("lock"),
        "{help}"
    );
}

#[test]
fn sending_returns_protected_values_as_lock_escapes() {
    let found = errors(
        "@sending async fn leak(m: Mutex<Array<i64>>) -> Array<i64> { lock m as a { return a; } return [1]; }",
    );
    assert!(
        found.iter().any(|d| d.code == ErrorCode::E2408),
        "{found:?}"
    );
    assert!(
        !found
            .iter()
            .any(|d| d.message.contains("`@sending` function cannot return")),
        "a protected value is reported once, as a lock escape: {found:?}"
    );
}

#[test]
fn sending_declaration_positions() {
    for (source, message) in [
        (
            "class C { @sending fn m(self) -> Array<i64> { return [1]; } }",
            "not supported on methods",
        ),
        (
            "class C { fn m(self, @sending a: Array<i64>) {} }",
            "not supported on methods",
        ),
        (
            "class C { pub a: Array<i64>; init(self, @sending a: Array<i64>) { self.a = a; } }",
            "not supported on constructors",
        ),
        (
            "interface I { @sending fn m(self) -> Array<i64>; }",
            "not supported on interface methods",
        ),
        (
            "@sending fn g<T>(x: T) -> T { return x; }",
            "not supported on generic functions",
        ),
        (
            "fn g(@owned a: Array<i64>) {}",
            "unknown parameter attribute",
        ),
    ] {
        let message_found = parse_error(source);
        assert!(message_found.contains(message), "{source}: {message_found}");
    }
}

#[test]
fn sending_checks_scale_linearly() {
    let mut per_size = Vec::new();
    for size in [8usize, 32, 128] {
        let mut source = String::new();
        // Sending results sent directly, matched payloads, consumed
        // parameters and checked returns, each repeated `size` times.
        source.push_str("async fn main() { let c: Channel<Array<i64>> = Channel::new();");
        for i in 0..size {
            source.push_str(&format!(
                "c.send(fresh()); match maybe() {{ Some(m{i}) => {{ c.send(m{i}); }} None => {{}} }} let a{i} = [1]; give(a{i}, c);"
            ));
        }
        source.push('}');
        for i in 0..size {
            source.push_str(&format!(
                "@sending fn r{i}(@sending p: Array<i64>, b: bool) -> Array<i64> {{ if b {{ return p; }} let x = fresh(); return x; }}"
            ));
        }
        super::transfer_ownership::TOTAL_COUNTS.with(|c| c.set((0, 0, 0)));
        let found = errors(&source);
        assert!(found.is_empty(), "{found:?}");
        let counts = super::transfer_ownership::TOTAL_COUNTS.with(|c| c.get());
        eprintln!(
            "sending n={size}: events={} nodes={} root_steps={}",
            counts.0, counts.1, counts.2
        );
        per_size.push((size, counts));
    }
    let (base, (t0, n0, r0)) = per_size[0];
    for (size, (ticks, nodes, roots)) in &per_size[1..] {
        let factor = size / base;
        // The prelude adds a constant; growth must stay within the input factor.
        assert!(
            *ticks <= factor * t0 && *nodes <= factor * n0 && *roots <= factor * r0,
            "superlinear growth: {per_size:?}"
        );
    }
}
