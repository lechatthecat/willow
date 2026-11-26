//! An operator type error leaves its result untyped, so later uses of it do
//! not report again. A task used where its result fits gets an `await` hint,
//! and a call through a callable field shows the binding to write.
use crate::diagnostics::{Diagnostic, Severity};

const PRELUDE: &str = "import std::collections::Array;
async fn f() -> i64 { return 1; }
async fn opt() -> Option<i64> { return Some(1); }
async fn nums() -> Array<i64> { return [1]; }
fn take(n: i64) -> i64 { return n; }
fn pick(xs: Array<i64>) -> i64 { return xs[0]; }
async fn later(xs: Array<i64>) -> i64 { return xs[0]; }
class Store { pub n: i64; pub static async fn load() -> i64 { return 2; } }
class Inner { pub before: fn(i64, i64) -> bool; }
class Heap { pub before: fn(i64, i64) -> bool; pub inner: Inner; }
fn make() -> Heap { let lt = |a: i64, b: i64| -> bool { return a < b; }; return new Heap(lt, new Inner(lt)); }
";

fn check(items: &str) -> Vec<Diagnostic> {
    let source = format!("{PRELUDE}{items}");
    let tokens = crate::lexer::Lexer::new(&source).tokenize().expect("lex");
    let (program, parse_errors) = crate::parser::Parser::new(tokens).parse();
    assert!(parse_errors.is_empty(), "{source}: {parse_errors:?}");
    let mut checker = crate::semantic::TypeChecker::new();
    crate::register_prelude(&mut checker).expect("prelude");
    checker.check_program(&program);
    checker
        .errors
        .into_iter()
        .filter(|d| d.severity == Severity::Error)
        .collect()
}

fn in_main(body: &str) -> Vec<Diagnostic> {
    check(&format!("fn main() {{ {body} }}"))
}

fn in_async(body: &str) -> Vec<Diagnostic> {
    check(&format!("async fn main() {{ {body} }}"))
}

#[test]
fn operator_errors_do_not_cascade_into_later_uses() {
    for (name, body) in [
        ("println", "let o: Option<i64> = Some(3); println(o + 1);"),
        (
            "let with annotation",
            "let o: Option<i64> = Some(3); let x = o * 2; let y: i64 = x + 1;",
        ),
        ("unary minus", "let o: Option<i64> = Some(3); println(-o);"),
        ("power", "let p = 2 ** 0.5; println(p + 1.0);"),
        ("mixed numbers", "let q = 1 + 1.5; let r: f64 = q;"),
        ("bitwise", "let b = true & 1; println(b);"),
        ("string minus", "let s = \"a\" - 1; let t: String = s;"),
        (
            "call argument",
            "let o: Option<i64> = Some(3); println(take(o + 1));",
        ),
        (
            "nested",
            "let o: Option<i64> = Some(3); println((o + 1) + 2);",
        ),
        (
            "comparison",
            "let o: Option<i64> = Some(3); if (o + 1) > 2 { println(1); }",
        ),
        (
            "array element",
            "let o: Option<i64> = Some(3); let xs: Array<i64> = [o + 1]; println(xs[0]);",
        ),
    ] {
        let errors = in_main(body);
        assert_eq!(errors.len(), 1, "{name}: {errors:?}");
    }
}

#[test]
fn operator_error_in_return_reports_once() {
    let errors =
        check("fn g(o: Option<i64>) -> i64 { return o + 1; } fn main() { println(g(Some(1))); }");
    assert_eq!(errors.len(), 1, "{errors:?}");
}

#[test]
fn separate_operator_errors_are_each_reported() {
    let errors = in_main("let o: Option<i64> = Some(3); println(o + 1); println(o - 1);");
    assert_eq!(errors.len(), 2, "{errors:?}");
    let errors = in_main("let a = 1 + 1.5; println(a);");
    assert_eq!(errors.len(), 1, "{errors:?}");
    assert!(
        errors[0].helps.iter().any(|h| h.contains("f64::from_i64")),
        "{errors:?}"
    );
}

#[test]
fn well_typed_operators_keep_their_result_type() {
    for body in [
        "let a: f64 = 1.5 * 2.0; let b: i64 = 7 % 3; let c: i64 = 1 << 2;",
        "let d: f64 = 2.0 ** 0.5; let e: i64 = 2 ** 3; let s: String = \"a\" + \"b\";",
        "let n: i64 = -3; let m: f64 = -1.5; let k: bool = !(1 < 2);",
    ] {
        let errors = in_main(body);
        assert!(errors.is_empty(), "{body}: {errors:?}");
    }
}

fn await_help(errors: &[Diagnostic]) -> String {
    assert_eq!(errors.len(), 1, "{errors:?}");
    assert!(
        errors[0].message.starts_with("mismatched types"),
        "{errors:?}"
    );
    errors[0].helps.join("\n")
}

#[test]
fn task_where_its_result_fits_suggests_await_in_async_code() {
    for (name, body, help) in [
        (
            "let",
            "let x: i64 = f(); println(x);",
            "`f()` is an async call that gives `Task<i64>`; write `await f()` to wait for its `i64` result",
        ),
        (
            "assignment",
            "let mut y = 0; y = f(); println(y);",
            "write `await f()`",
        ),
        ("call argument", "println(take(f()));", "write `await f()`"),
        (
            "static async method",
            "let s = new Store(0); s.n = Store::load(); println(s.n);",
            "`Store::load()` is an async call that gives `Task<i64>`; write `await Store::load()`",
        ),
        (
            "task variable",
            "let t = f(); let a: i64 = t; println(a);",
            "`t` is a `Task<i64>`; write `await t` to wait for its `i64` result",
        ),
        (
            "option result",
            "let o: Option<i64> = opt(); println(o.unwrap_or(0));",
            "write `await opt()` to wait for its `Option<i64>` result",
        ),
        (
            "argument too complex to quote",
            "let n: i64 = later([1, 2]); println(n);",
            "this is an async call that gives `Task<i64>`; `await` it to wait for its `i64` result",
        ),
    ] {
        let help_text = await_help(&in_async(body));
        assert!(help_text.contains(help), "{name}: {help_text}");
    }
}

#[test]
fn task_return_suggests_await() {
    let errors =
        check("async fn g() -> i64 { return f(); } async fn main() { println(await g()); }");
    assert!(
        await_help(&errors).contains("write `await f()`"),
        "{errors:?}"
    );
}

#[test]
fn task_in_sync_code_explains_the_call_is_async() {
    for items in [
        "fn g() -> i64 { let x: i64 = f(); return x; } fn main() { println(g()); }",
        "fn g() -> i64 { return f(); } fn main() { println(g()); }",
        "fn main() { println(take(f())); }",
    ] {
        let help = await_help(&check(items));
        assert!(
            help.contains("`f()` is an async call that gives `Task<i64>`, not its `i64` result")
                && help.contains("mark this function `async` and write `await f()`"),
            "{items}: {help}"
        );
    }
}

#[test]
fn task_whose_result_does_not_fit_gets_no_await_hint() {
    for body in [
        "let s: String = f();",
        "let xs: Array<String> = nums();",
        "let b: bool = opt();",
        "println(pick(f()));",
    ] {
        let errors = in_async(body);
        assert_eq!(errors.len(), 1, "{body}: {errors:?}");
        assert!(
            !errors[0].helps.iter().any(|h| h.contains("await")),
            "{body}: {errors:?}"
        );
    }
}

fn callable_help(items: &str) -> String {
    let errors = check(items);
    assert_eq!(errors.len(), 1, "{items}: {errors:?}");
    assert!(
        errors[0].message.contains("no method `before`"),
        "{items}: {errors:?}"
    );
    errors[0].helps.join("\n")
}

#[test]
fn callable_field_help_names_the_receiver_and_arguments() {
    for (name, items, binding) in [
        (
            "method receiver",
            "class H { pub before: fn(i64, i64) -> bool; pub fn lt(self, a: i64, b: i64) -> bool { return self.before(a, b); } } fn main() {}",
            "`let before = self.before; before(a, b)`",
        ),
        (
            "nested field receiver",
            "class H { pub inner: Inner; pub fn lt(self, a: i64) -> bool { return self.inner.before(a, 1); } } fn main() {}",
            "`let before = self.inner.before; before(a, 1)`",
        ),
        (
            "local receiver",
            "fn main() { let h = make(); println(h.before(1, 2)); }",
            "`let before = h.before; before(1, 2)`",
        ),
        (
            "call receiver",
            "fn main() { println(make().before(3, 4)); }",
            "`let before = make().before; before(3, 4)`",
        ),
        (
            "call argument",
            "fn main() { let h = make(); println(h.before(take(1), 2)); }",
            "`let before = h.before; before(take(1), 2)`",
        ),
        (
            "no arguments",
            "class Z { pub before: fn() -> bool; pub fn go(self) -> bool { return self.before(); } } fn main() {}",
            "`let before = self.before; before()`",
        ),
        (
            "arguments too complex to quote",
            "fn main() { let h = make(); println(h.before([1][0] + 1, 2)); }",
            "`let before = h.before; before(...)`",
        ),
    ] {
        let help = callable_help(items);
        assert!(help.contains(binding), "{name}: {help}");
        assert!(help.contains("callable field"), "{name}: {help}");
    }
}

#[test]
fn callable_field_help_without_a_quotable_receiver_keeps_the_generic_text() {
    let help = callable_help("fn main() { println([make()][0].before(1, 2)); }");
    assert!(!help.contains("let before = ["), "{help}");
    assert!(help.contains("callable field"), "{help}");
}

#[test]
fn suggested_callable_field_binding_type_checks() {
    let errors = check(
        "class H { pub before: fn(i64, i64) -> bool; pub fn lt(self, a: i64, b: i64) -> bool { let before = self.before; return before(a, b); } }
fn main() { let h = make(); let before = h.before; println(before(1, 2)); }",
    );
    assert!(errors.is_empty(), "{errors:?}");
}

fn rendered(expr_source: &str) -> Option<String> {
    let source = format!("fn probe() {{ let value = {expr_source}; }}");
    let tokens = crate::lexer::Lexer::new(&source).tokenize().expect("lex");
    let (program, errors) = crate::parser::Parser::new(tokens).parse();
    assert!(errors.is_empty(), "{expr_source}: {errors:?}");
    let crate::parser::ast::Item::Function(function) = &program.items[0] else {
        unreachable!()
    };
    let crate::parser::ast::Stmt::Let(binding) = &function.body.stmts[0] else {
        unreachable!()
    };
    super::expr_text::simple_expr_text(&binding.init)
}

#[test]
fn quoted_expressions_stay_short_and_source_like() {
    for (expr, expected) in [
        ("f()", Some("f()")),
        ("Store::load()", Some("Store::load()")),
        ("a.b[0].c(1, true, \"s\")", Some("a.b[0].c(1, true, \"s\")")),
        ("\"a{b}\"", None),
        ("\"say \\\"hi\\\"\"", None),
        ("1 + 2", None),
        ("f(|x| x)", None),
    ] {
        assert_eq!(rendered(expr).as_deref(), expected, "{expr}");
    }
    let long = format!("f({})", "x, ".repeat(30) + "x");
    assert_eq!(rendered(&long), None);
    // A receiver chain deeper than the length bound gives up before
    // descending further.
    let deep = format!("a{}", ".b".repeat(400));
    assert_eq!(rendered(&deep), None);
    let calls = format!("a{}", ".m()".repeat(400));
    assert_eq!(rendered(&calls), None);
}
