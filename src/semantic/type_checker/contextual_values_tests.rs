//! A value `match`/ternary whose arms are distinct classes takes the type it
//! flows into when every arm converts to it (willow-jz15.47).
use super::*;
use crate::{lexer::Lexer, parser::Parser};

const PREFIX: &str = r#"
import std::collections::Array;
interface Filter { fn name(self) -> String; }
interface Box<T> { fn get(self) -> T; }
class A implements Filter, Box<i64> { pub fn name(self) -> String { return "a"; } pub fn get(self) -> i64 { return 1; } }
class B implements Filter, Box<i64> { pub fn name(self) -> String { return "b"; } pub fn get(self) -> i64 { return 2; } }
class C implements Filter { pub fn name(self) -> String { return "c"; } }
open class Shape {}
class Square extends Shape {}
class Circle extends Shape {}
class Rock {}
enum E { X, Y(i64), Z }
"#;

fn errors_of(body: &str) -> (String, Vec<Diagnostic>) {
    let source = format!("{PREFIX}{body}");
    let errors = check_source(&source)
        .into_iter()
        .filter(|e| e.severity == Severity::Error)
        .collect();
    (source, errors)
}

#[test]
fn contextual_values_positive_perspectives() {
    let m = "match e { E::X => new A(), E::Y(v) => new B(), E::Z => new C() }";
    let t = "b ? new A() : new B()";
    let cases = [
        ("match return", format!("fn f(e: E) -> Filter {{ return {m}; }}")),
        ("match let", format!("fn f(e: E) {{ let x: Filter = {m}; }}")),
        (
            "match argument",
            format!("fn g(x: Filter) {{}} fn f(e: E) {{ g({m}); }}"),
        ),
        (
            "match method argument",
            format!(
                "class H {{ pub fn g(self, x: Filter) {{}} }} fn f(e: E) {{ new H().g({m}); }}"
            ),
        ),
        (
            "match static argument",
            format!("class H {{ pub static fn g(x: Filter) {{}} }} fn f(e: E) {{ H::g({m}); }}"),
        ),
        (
            "match constructor field",
            format!("class H {{ pub x: Filter; }} fn f(e: E) {{ let h = new H({m}); }}"),
        ),
        (
            "match field assignment",
            format!("class H {{ pub x: Filter; pub fn set(self, e: E) {{ self.x = {m}; }} }}"),
        ),
        (
            "match local assignment",
            format!("fn f(e: E) {{ let mut x: Filter = new A(); x = {m}; }}"),
        ),
        (
            "match array element",
            format!("fn f(e: E) -> Array<Filter> {{ return [{m}, new A()]; }}"),
        ),
        (
            "match array push",
            format!("fn f(e: E) {{ let xs: Array<Filter> = []; xs.push({m}); }}"),
        ),
        ("ternary return", format!("fn f(b: bool) -> Filter {{ return {t}; }}")),
        ("ternary let", format!("fn f(b: bool) {{ let x: Filter = {t}; }}")),
        (
            "ternary argument",
            format!("fn g(x: Filter) {{}} fn f(b: bool) {{ g({t}); }}"),
        ),
        (
            "ternary inside match arm",
            format!(
                "fn f(e: E, b: bool) -> Filter {{ return match e {{ E::X => {t}, _ => new C() }}; }}"
            ),
        ),
        (
            "match inside ternary branch",
            format!("fn f(e: E, b: bool) -> Filter {{ return b ? {m} : new C(); }}"),
        ),
        (
            "diverging arm",
            "fn f(e: E) -> Filter { return match e { E::X => new A(), E::Y(v) => { return new C(); }, _ => new B() }; }".to_string(),
        ),
        (
            "base class",
            "fn f(b: bool) -> Shape { return match b { true => new Square(), false => new Circle() }; }".to_string(),
        ),
        (
            "base class ternary",
            "fn f(b: bool) -> Shape { return b ? new Square() : new Circle(); }".to_string(),
        ),
        (
            "generic interface",
            "fn f(b: bool) -> Box<i64> { return match b { true => new A(), false => new B() }; }"
                .to_string(),
        ),
        (
            "interface arm then class arm",
            "fn f(x: Filter, b: bool) -> String { let y = match b { true => x, false => new A() }; return y.name(); }".to_string(),
        ),
        (
            "class arm then interface arm",
            "fn f(x: Filter, b: bool) -> String { let y = match b { true => new A(), false => x }; return y.name(); }".to_string(),
        ),
        (
            "same class stays concrete",
            "fn f(b: bool) -> A { let y = match b { true => new A(), false => new A() }; return y; }".to_string(),
        ),
        (
            "wildcard and bindings",
            "fn f(n: i64) -> Filter { return match n { 0 => new A(), 1 => new B(), _ => new C() }; }"
                .to_string(),
        ),
    ];
    for (name, body) in cases {
        let (_, errors) = errors_of(&body);
        assert!(errors.is_empty(), "{name}: {errors:?}");
    }
}

#[test]
fn contextual_values_record_expected_type() {
    let source = format!(
        "{PREFIX}fn f(e: E) -> Filter {{ return match e {{ E::X => new A(), _ => new B() }}; }} fn g(b: bool) -> Filter {{ return b ? new A() : new B(); }}"
    );
    let (program, parse) = Parser::new(Lexer::new(&source).tokenize().unwrap()).parse();
    assert!(parse.is_empty(), "{parse:?}");
    let mut checker = TypeChecker::new();
    checker.check_program(&program);
    assert!(checker.errors.is_empty(), "{:?}", checker.errors);
    let mut seen = 0;
    for item in &program.items {
        let Item::Function(f) = item else { continue };
        let Some(Stmt::Return(r)) = f.body.stmts.first() else {
            continue;
        };
        let value = r.value.as_ref().unwrap();
        assert!(matches!(value, Expr::Match(_) | Expr::Ternary(_)));
        assert_eq!(
            checker.expr_types[&value.id()],
            Type::Named("Filter".into())
        );
        seen += 1;
    }
    assert_eq!(seen, 2);
}

#[test]
fn contextual_values_reject_unrelated_arm() {
    for (body, code, bad) in [
        (
            "fn f(e: E) -> Filter { return match e { E::X => new A(), _ => new Rock() }; }",
            ErrorCode::E1201,
            "_ => new Rock()",
        ),
        (
            "fn f(e: E) { let x: Filter = match e { E::X => new A(), _ => new Rock() }; }",
            ErrorCode::E1201,
            "_ => new Rock()",
        ),
        (
            "fn f(b: bool) -> Filter { return b ? new A() : new Rock(); }",
            ErrorCode::E0902,
            "new Rock()",
        ),
        (
            "fn f(b: bool) -> Shape { return match b { true => new Square(), false => new A() }; }",
            ErrorCode::E1201,
            "false => new A()",
        ),
    ] {
        let (source, errors) = errors_of(body);
        let error = errors
            .iter()
            .find(|e| e.code == code)
            .unwrap_or_else(|| panic!("{body}: {errors:?}"));
        assert_eq!(
            error.primary_span().unwrap().start,
            source.find(bad).unwrap(),
            "{body}"
        );
        assert!(
            errors.iter().all(|e| e.code != ErrorCode::E0800),
            "{errors:?}"
        );
    }
}

#[test]
fn contextual_values_unannotated_help_names_common_type() {
    for (body, code, help) in [
        (
            "fn f(e: E) { let x = match e { E::X => new A(), _ => new C() }; }",
            ErrorCode::E1201,
            "`let value: Filter = ...;`",
        ),
        (
            "fn f(b: bool) { let x = b ? new A() : new C(); }",
            ErrorCode::E0902,
            "`let value: Filter = ...;`",
        ),
        (
            "fn f(b: bool) { let x = b ? new Square() : new Circle(); }",
            ErrorCode::E0902,
            "`let value: Shape = ...;`",
        ),
        (
            "fn f(b: bool) { let x = match b { true => new A(), false => new Rock() }; }",
            ErrorCode::E1201,
            "annotate the destination",
        ),
        (
            "fn f(b: bool) { let x = match b { true => 1, false => \"s\" }; }",
            ErrorCode::E1201,
            "annotate the destination",
        ),
    ] {
        let (_, errors) = errors_of(body);
        let error = errors
            .iter()
            .find(|e| e.code == code)
            .unwrap_or_else(|| panic!("{body}: {errors:?}"));
        assert!(
            error.helps.iter().any(|h| h.contains(help)),
            "{body}: {:?}",
            error.helps
        );
    }
}

#[test]
fn contextual_values_unification_work_is_linear() {
    // Alternating classes: the first conflict unifies to the expected type,
    // every later arm is accepted by one compatibility check against it.
    for n in [16, 64, 256, 1024] {
        let mut arms = String::new();
        for i in 0..n {
            let class = ["A", "B", "C"][i % 3];
            arms.push_str(&format!("{i} => new {class}(),"));
        }
        let body =
            format!("fn f(n: i64) -> Filter {{ return match n {{ {arms} _ => new A() }}; }}");
        UNIFY_TO_EXPECTED_CALLS.with(|count| count.set(0));
        let (_, errors) = errors_of(&body);
        assert!(errors.is_empty(), "{errors:?}");
        assert_eq!(
            UNIFY_TO_EXPECTED_CALLS.with(|count| count.get()),
            1,
            "n={n}"
        );
    }
}
