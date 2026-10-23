//! The expected result type reaches the arguments of lock constructors and
//! generic calls, so an empty collection argument takes the annotated element
//! types instead of `void` placeholders (willow-jz15.96).
use super::*;
use crate::{lexer::Lexer, parser::Parser};

const PREFIX: &str = r#"
import std::collections::Array;
import std::collections::Map;
interface Search { fn value(self) -> i64; }
class A implements Search { pub fn value(self) -> i64 { return 1; } }
class B implements Search { pub fn value(self) -> i64 { return 2; } }
class Holder<T> {
    pub v: T;
    pub static fn make(x: T) -> Holder<T> { return new Holder<T>(x); }
}
class Pair<L, R> {
    pub left: L;
    pub right: R;
}
fn wrap<T>(x: T) -> Holder<T> { return new Holder<T>(x); }
fn pair<L, R>(l: L, r: R) -> Pair<L, R> { return new Pair<L, R>(l, r); }
fn make_lock<T>(x: T) -> Mutex<T> { return Mutex::new(x); }
fn ident<T>(x: T) -> T { return x; }
fn tagged<T>(tag: i64, x: T) -> Holder<T> { return new Holder<T>(x); }
"#;

fn checked(body: &str) -> (TypeChecker, Vec<Diagnostic>) {
    let source = format!("{PREFIX}{body}");
    let (program, parse) = Parser::new(Lexer::new(&source).tokenize().unwrap()).parse();
    assert!(parse.is_empty(), "{parse:?}");
    let mut checker = TypeChecker::new();
    crate::register_prelude(&mut checker).expect("prelude");
    checker.check_program(&program);
    let errors = checker
        .errors
        .iter()
        .filter(|d| d.severity == Severity::Error)
        .cloned()
        .collect();
    (checker, errors)
}

fn errors_of(body: &str) -> Vec<Diagnostic> {
    checked(body).1
}

#[test]
fn contextual_ctor_args_positive_perspectives() {
    let cases = [
        (
            "mutex map let",
            "fn f() { let m: Mutex<Map<i64, i64>> = Mutex::new(Map::new()); }",
        ),
        (
            "mutex array let",
            "fn f() { let m: Mutex<Array<i64>> = Mutex::new([]); }",
        ),
        (
            "rwlock map let",
            "fn f() { let m: RwLock<Map<String, Array<i64>>> = RwLock::new(Map::new()); }",
        ),
        (
            "rwlock array let",
            "fn f() { let m: RwLock<Array<String>> = RwLock::new([]); }",
        ),
        (
            "blocking cell array",
            "fn f() { let m: BlockingCell<Array<i64>> = BlockingCell::new([]); }",
        ),
        (
            "blocking rw cell map",
            "fn f() { let m: BlockingRwCell<Map<i64, bool>> = BlockingRwCell::new(Map::new()); }",
        ),
        (
            "return",
            "fn f() -> Mutex<Array<i64>> { return Mutex::new([]); }",
        ),
        (
            "argument",
            "fn g(m: Mutex<Map<i64, i64>>) {} fn f() { g(Mutex::new(Map::new())); }",
        ),
        (
            "memberwise field",
            "class H { pub m: Mutex<Array<i64>>; } fn f() { let h = new H(Mutex::new([])); }",
        ),
        (
            "nested locks",
            "fn f() { let m: Mutex<Mutex<Array<i64>>> = Mutex::new(Mutex::new([])); }",
        ),
        (
            "lock as array element",
            "fn f() { let xs: Array<Mutex<Array<i64>>> = [Mutex::new([]), Mutex::new([1])]; }",
        ),
        (
            "option payload",
            "fn f() { let m: Mutex<Option<Array<i64>>> = Mutex::new(Some([])); }",
        ),
        (
            "fieldless variant",
            "fn f() { let m: Mutex<Option<Map<i64, i64>>> = Mutex::new(None); }",
        ),
        (
            "frozen array",
            "fn f() { let m: RwLock<FrozenArray<i64>> = RwLock::new([].freeze()); }",
        ),
        (
            "frozen map",
            "fn f() { let m: RwLock<FrozenMap<String, i64>> = RwLock::new(Map::new().freeze()); }",
        ),
        (
            "explicit type argument",
            "fn f() { let m = Mutex<Array<i64>>::new([]); }",
        ),
        (
            "channel",
            "fn f() { let m: Mutex<Channel<i64>> = Mutex::new(Channel::new()); }",
        ),
        (
            "interface elements",
            "fn f() { let m: Mutex<Array<Search>> = Mutex::new([new A(), new B()]); }",
        ),
        (
            "ternary",
            "fn f(b: bool) { let m: Mutex<Array<i64>> = Mutex::new(b ? [] : [1]); }",
        ),
        (
            "generic class result array",
            "fn f() { let h: Holder<Array<i64>> = wrap([]); }",
        ),
        (
            "generic class result map",
            "fn f() { let h: Holder<Map<i64, String>> = wrap(Map::new()); }",
        ),
        (
            "generic lock result",
            "fn f() { let m: Mutex<Map<String, i64>> = make_lock(Map::new()); }",
        ),
        (
            "generic identity",
            "fn f() { let xs: Array<Map<i64, i64>> = ident([Map::new()]); }",
        ),
        (
            "generic two parameters",
            "fn f() { let p: Pair<Array<i64>, Map<i64, i64>> = pair([], Map::new()); }",
        ),
        (
            "generic with fixed parameter",
            "fn f() { let h: Holder<Array<bool>> = tagged(1, []); }",
        ),
        (
            "generic inside lock",
            "fn f() { let m: Mutex<Holder<Array<i64>>> = Mutex::new(wrap([])); }",
        ),
        (
            "lock inside generic",
            "fn f() { let h: Holder<Mutex<Array<i64>>> = wrap(Mutex::new([])); }",
        ),
        (
            "generic return statement",
            "fn f() -> Holder<Array<i64>> { return wrap([]); }",
        ),
        (
            "generic argument position",
            "fn g(h: Holder<Map<i64, i64>>) {} fn f() { g(wrap(Map::new())); }",
        ),
        (
            "generic ternary argument",
            "fn f(b: bool) { let h: Holder<Array<i64>> = wrap(b ? [] : [1]); }",
        ),
        (
            "generic variant argument",
            "fn f() { let h: Holder<Option<Array<i64>>> = wrap(Some([])); }",
        ),
        (
            "generic fieldless variant argument",
            "fn f() { let h: Holder<Option<Map<i64, i64>>> = wrap(None); }",
        ),
        (
            "explicit lock type argument with variant",
            "fn f() { let m = Mutex<Option<Array<i64>>>::new(None); }",
        ),
        (
            "interface result keeps concrete argument",
            "fn f() { let s: Search = ident(new A()); }",
        ),
    ];
    for (name, body) in cases {
        let (checker, errors) = checked(body);
        assert!(errors.is_empty(), "{name}: {errors:?}");
        // No node keeps a placeholder type: the empty literal and the
        // placeholder constructor themselves record the expected types.
        let placeholders: Vec<_> = checker
            .expr_types
            .values()
            .map(type_name)
            .filter(|ty| ty.contains("<void") || ty.contains(", void"))
            .collect();
        assert!(placeholders.is_empty(), "{name}: {placeholders:?}");
    }
}

#[test]
fn contextual_ctor_args_reject_wrong_contents() {
    let cases = [
        (
            "wrong element in lock",
            "fn f() { let m: Mutex<Array<i64>> = Mutex::new([\"a\"]); }",
            "array element expects `i64`, found `String`",
        ),
        (
            "wrong scalar in lock",
            "fn f() { let m: Mutex<i64> = Mutex::new(\"a\"); }",
            "expected `Mutex<i64>`, found `Mutex<String>`",
        ),
        (
            "wrong lock kind",
            "fn f() { let m: RwLock<Array<i64>> = Mutex::new([]); }",
            "expected `RwLock<Array<i64>>`, found `Mutex<Array<void>>`",
        ),
        (
            "explicit map type arguments win",
            "fn f() { let m: Mutex<Map<i64, i64>> = Mutex::new(Map<String, i64>::new()); }",
            "found `Mutex<Map<String, i64>>`",
        ),
        (
            "wrong element in generic",
            "fn f() { let h: Holder<Array<i64>> = wrap([\"a\"]); }",
            "array element expects `i64`, found `String`",
        ),
        (
            "wrong element in generic ternary",
            "fn f(b: bool) { let h: Holder<Array<i64>> = wrap(b ? [] : [\"a\"]); }",
            "array element expects `i64`, found `String`",
        ),
        (
            "unrelated generic result",
            "fn f() { let m: Mutex<i64> = wrap(1); }",
            "mismatched types",
        ),
        (
            "explicit lock type argument mismatch",
            "fn f() { let m = Mutex<i64>::new(\"a\"); }",
            "`Mutex<i64>::new` expects `i64`, got `String`",
        ),
    ];
    for (name, body, message) in cases {
        let errors = errors_of(body);
        assert_eq!(errors.len(), 1, "{name}: {errors:?}");
        assert!(errors[0].message.contains(message), "{name}: {errors:?}");
    }
}

#[test]
fn contextual_ctor_args_without_context_keep_placeholders() {
    // Unannotated construction has no context to take; it stays what it was.
    let (checker, errors) = checked("fn f() { let m = Mutex::new([]); let h = wrap(Map::new()); }");
    assert!(errors.is_empty(), "{errors:?}");
    let names: Vec<_> = checker.expr_types.values().map(type_name).collect();
    assert!(names.iter().any(|n| n == "Mutex<Array<void>>"), "{names:?}");
}

#[test]
fn contextual_ctor_args_local_binding_shadows_generic() {
    // A callable local named like a generic function is called as a value; the
    // generic's expected-result context must not apply to it.
    let errors = errors_of("fn f() { let wrap = |x: Array<i64>| x.len(); let n: i64 = wrap([]); }");
    assert!(errors.is_empty(), "{errors:?}");
}

#[test]
fn contextual_ctor_args_work_is_linear() {
    use super::check_collections::ARRAY_ELEMENT_CHECKS;
    use super::generics::{EXPECTED_RESULT_HINTS, GENERIC_CALL_CHECKS};
    // Alternating lock and generic layers around one empty array: each layer
    // is checked once, each generic call matches its result once, and the
    // single bad element is reported once at any depth.
    for depth in [4usize, 8, 16, 32] {
        let mut ty = "Array<i64>".to_string();
        let mut value = "[\"bad\"]".to_string();
        for layer in 0..depth {
            if layer % 2 == 0 {
                ty = format!("Mutex<{ty}>");
                value = format!("Mutex::new({value})");
            } else {
                ty = format!("Holder<{ty}>");
                value = format!("wrap({value})");
            }
        }
        GENERIC_CALL_CHECKS.with(|count| count.set(0));
        EXPECTED_RESULT_HINTS.with(|count| count.set(0));
        ARRAY_ELEMENT_CHECKS.with(|count| count.set(0));
        let errors = errors_of(&format!("fn f() {{ let v: {ty} = {value}; }}"));
        let calls = GENERIC_CALL_CHECKS.with(|count| count.get());
        let hints = EXPECTED_RESULT_HINTS.with(|count| count.get());
        let elements = ARRAY_ELEMENT_CHECKS.with(|count| count.get());
        eprintln!("depth={depth} generic_calls={calls} hints={hints} elements={elements}");
        assert_eq!(errors.len(), 1, "depth {depth}: {errors:?}");
        assert!(errors[0].message.contains("array element expects `i64`"));
        // PREFIX's own generic bodies are checked abstractly once each.
        let source_calls = depth / 2;
        assert!(
            calls >= source_calls && calls <= source_calls + 8,
            "depth {depth}: {calls}"
        );
        assert_eq!(hints, source_calls, "depth {depth}");
        assert_eq!(elements, 1, "depth {depth}");
    }
}

#[test]
fn contextual_ctor_args_generic_class_static_calls_instantiate() {
    // A static call on a generic class template in an expecting position is
    // still instantiated: the call is recorded against the instance and checked
    // against its concrete signature, exactly as without an expected type.
    let cases = [
        (
            "let",
            "fn f() { let h: Holder<i64> = Holder<i64>::make(1); }",
        ),
        (
            "return",
            "fn f() -> Holder<i64> { return Holder<i64>::make(1); }",
        ),
        (
            "argument",
            "fn g(h: Holder<i64>) {} fn f() { g(Holder<i64>::make(1)); }",
        ),
        (
            "array element",
            "fn f() { let hs: Array<Holder<i64>> = [Holder<i64>::make(1)]; }",
        ),
        (
            "lock argument",
            "fn f() { let m: Mutex<Holder<i64>> = Mutex::new(Holder<i64>::make(1)); }",
        ),
        (
            "empty collection argument",
            "fn f() { let h: Holder<Array<i64>> = Holder<Array<i64>>::make([]); }",
        ),
    ];
    for (name, body) in cases {
        let (checker, errors) = checked(body);
        assert!(errors.is_empty(), "{name}: {errors:?}");
        assert!(
            checker
                .generics
                .objects
                .values()
                .any(|instance| instance.starts_with("Holder$mono$")),
            "{name}: {:?}",
            checker.generics.objects
        );
    }
    // The template checks still run: arity and argument type.
    let arity = errors_of("fn f() { let h: Holder<i64> = Holder::make(1); }");
    assert!(
        arity
            .iter()
            .any(|e| e.message.contains("expects 1 type argument(s), got 0")),
        "{arity:?}"
    );
    let wrong = errors_of("fn f() { let h: Holder<i64> = Holder<i64>::make(\"a\"); }");
    assert_eq!(wrong.len(), 1, "{wrong:?}");
}
