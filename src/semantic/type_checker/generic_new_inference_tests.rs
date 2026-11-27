//! `new C(args)` infers a generic class's type arguments from its constructor
//! arguments, the same way a generic function call does.
use crate::diagnostics::{Diagnostic, ErrorCode, Severity};

const PRELUDE: &str = "import std::collections::Array;
class Heap<T> { pub items: Array<T>; pub before: fn(T, T) -> bool; }
class Box<T> { pub v: T; }
class Pair<A, B> { pub a: A; pub b: B; }
class Kv<K, V> { pub k: K; pub v: V; pub init(self, k: K, v: V) { self.k = k; self.v = v; } }
class Fixed<T> { pub n: i64; }
open class Base { pub id: i64; }
class Tagged<T> extends Base { pub tag: T; }
interface Named { fn name(self) -> String; }
class Label<T> implements Named { pub v: T; pub fn name(self) -> String { return \"l\"; } }
";

fn check(body: &str) -> (String, Vec<Diagnostic>) {
    let source = format!("{PRELUDE}\nfn main() {{ {body} }}");
    let tokens = crate::lexer::Lexer::new(&source).tokenize().expect("lex");
    let (program, parse_errors) = crate::parser::Parser::new(tokens).parse();
    assert!(parse_errors.is_empty(), "{source}: {parse_errors:?}");
    let mut checker = crate::semantic::TypeChecker::new();
    crate::register_prelude(&mut checker).expect("prelude");
    checker.check_program(&program);
    let errors = checker
        .errors
        .into_iter()
        .filter(|d| d.severity == Severity::Error)
        .collect();
    (source, errors)
}

#[test]
fn generic_new_infers_type_arguments_from_constructor_arguments() {
    for (name, body) in [
        ("memberwise scalar", "let b = new Box(5); let n: i64 = b.v;"),
        (
            "memberwise string",
            "let b = new Box(\"s\"); let s: String = b.v;",
        ),
        (
            "init constructor",
            "let p = new Kv(\"k\", 2.5); let k: String = p.k; let v: f64 = p.v;",
        ),
        (
            "two parameters memberwise",
            "let p = new Pair(1, true); let b: bool = p.b;",
        ),
        (
            "array literal",
            "let h = new Heap([3, 1], |a: i64, b: i64| -> bool { return a < b; }); let n: i64 = h.items[0];",
        ),
        (
            "untyped closure parameters",
            "let h = new Heap([3, 1], |a, b| a < b); let f: fn(i64, i64) -> bool = h.before;",
        ),
        (
            "empty array fixed by typed closure",
            "let h = new Heap([], |a: String, b: String| -> bool { return a < b; }); let s: Array<String> = h.items;",
        ),
        (
            "nested generic argument",
            "let n = new Box(new Box(2)); let v: i64 = n.v.v;",
        ),
        (
            "option payload",
            "let s = new Box(Option::Some(3)); let v: i64 = s.v.unwrap_or(0);",
        ),
        (
            "inherited base field",
            "let t = new Tagged(1, \"t\"); let id: i64 = t.id; let s: String = t.tag;",
        ),
        (
            "explicit type arguments still accepted",
            "let b = new Box<i64>(4); let v: i64 = b.v;",
        ),
        (
            "expected type context",
            "let e: Box<String> = new Box(\"x\"); let s: String = e.v;",
        ),
        (
            "interface target",
            "let n: Named = new Label(1.5); let s: String = n.name();",
        ),
        (
            "array of inferred instances",
            "let arr = [new Box(1), new Box(2)]; let v: i64 = arr[1].v;",
        ),
        (
            "same instance as explicit form",
            "let a: Box<i64> = new Box(1); let b = new Box<i64>(2); let c: Box<i64> = new Box(3); let d = [a, b, c];",
        ),
    ] {
        let (source, found) = check(body);
        assert!(found.is_empty(), "{name}: {source}\n{found:?}");
    }
}

#[test]
fn generic_new_inside_generic_function_uses_the_function_type_parameter() {
    let source = format!(
        "{PRELUDE}\nfn wrap<T>(v: T) -> Box<T> {{ return new Box(v); }}\nfn main() {{ let b: Box<String> = wrap(\"s\"); let n: Box<i64> = wrap(1); }}"
    );
    let tokens = crate::lexer::Lexer::new(&source).tokenize().expect("lex");
    let (program, parse_errors) = crate::parser::Parser::new(tokens).parse();
    assert!(parse_errors.is_empty(), "{parse_errors:?}");
    let mut checker = crate::semantic::TypeChecker::new();
    crate::register_prelude(&mut checker).expect("prelude");
    checker.check_program(&program);
    let found: Vec<_> = checker
        .errors
        .iter()
        .filter(|d| d.severity == Severity::Error)
        .collect();
    assert!(found.is_empty(), "{found:?}");
}

#[test]
fn generic_new_reports_the_unresolved_type_parameter() {
    for (name, body, missing, shown, note) in [
        (
            "empty array and untyped closure",
            "let h = new Heap([], |a, b| -> bool { return true; });",
            "`T`",
            "new Heap<T>(...)",
            "`T` appears only in `items`, `before`",
        ),
        (
            "partial binding keeps the bound argument",
            "let p = new Pair(\"s\", []);",
            "`B`",
            "new Pair<String, B>(...)",
            "`B` appears only in `b`",
        ),
        (
            "type parameter used by no constructor parameter",
            "let f = new Fixed(1);",
            "`T`",
            "new Fixed<T>(...)",
            "no constructor parameter mentions `T`",
        ),
    ] {
        let (source, found) = check(body);
        assert_eq!(found.len(), 1, "{name}: {source}\n{found:?}");
        let d = &found[0];
        assert_eq!(d.code, ErrorCode::E0201, "{name}: {d:?}");
        assert!(
            d.message
                .contains(&format!("cannot infer type parameter {missing}")),
            "{name}: {}",
            d.message
        );
        let helps = d.helps.join("\n");
        assert!(helps.contains(shown), "{name}: {helps}");
        assert!(helps.contains("keep the arguments"), "{name}: {helps}");
        assert!(
            d.notes.iter().any(|n| n.contains(note)),
            "{name}: {:?}",
            d.notes
        );
    }
}

#[test]
fn generic_new_conflicting_argument_is_labelled() {
    for (body, arg) in [
        (
            "let c = new Heap([1, 2], |a: String, b: String| -> bool { return true; });",
            "|a: String, b: String| -> bool { return true; }",
        ),
        (
            "let p = new Kv(\"k\", 1); let q: Kv<String, String> = new Kv(\"k\", 2);",
            "2",
        ),
    ] {
        let (source, found) = check(body);
        assert!(!found.is_empty(), "{source}");
        let body_start = source.find("fn main()").expect("main");
        let start = body_start + source[body_start..].rfind(arg).expect("argument");
        let hit = found.iter().any(|d| {
            d.labels
                .iter()
                .any(|l| l.span.start <= start && start + arg.len() <= l.span.end.max(start + 1))
        });
        assert!(hit, "{body}: {found:?}");
    }
}

#[test]
fn generic_new_wrong_argument_count_reports_once() {
    for body in ["let q = new Pair(\"s\");", "let k = new Kv(1, 2, 3);"] {
        let (source, found) = check(body);
        assert_eq!(found.len(), 1, "{source}\n{found:?}");
        assert_eq!(found[0].code, ErrorCode::E0845, "{found:?}");
        assert!(found[0].message.contains("argument"), "{found:?}");
    }
}

#[test]
fn generic_new_argument_errors_are_not_duplicated() {
    for body in [
        "let a = new Box(undefined_y);",
        "let h = new Heap([3, 1], |a, b| -> bool { return undefined_x; });",
        "let k = new Kv(undefined_z, 1);",
    ] {
        let (source, found) = check(body);
        assert_eq!(found.len(), 1, "{source}\n{found:?}");
    }
}

#[test]
fn generic_new_empty_array_keeps_an_expected_element_type() {
    for body in [
        "let g: Box<Array<i64>> = new Box([]); let n: i64 = g.v.len();",
        "let h = new Heap([], |a: i64, b: i64| -> bool { return a < b; }); let n: i64 = h.items.len();",
    ] {
        let (source, found) = check(body);
        assert!(found.is_empty(), "{source}\n{found:?}");
    }
}

#[test]
fn generic_function_empty_array_argument_follows_the_same_rules() {
    let head =
        "fn heap<T>(items: Array<T>, before: fn(T, T) -> bool) -> i64 { return items.len(); }
fn pair<A, B>(a: A, b: B) -> A { return a; }
fn wrap<T>(v: T) -> Box<T> { return new Box(v); }";
    for (body, expect_error) in [
        (
            "let n = heap([], |a: i64, b: i64| -> bool { return a < b; });",
            false,
        ),
        ("let h: Box<Array<i64>> = wrap([]);", false),
        ("let p = pair(\"s\", []);", true),
    ] {
        let source = format!("{PRELUDE}\n{head}\nfn main() {{ {body} }}");
        let tokens = crate::lexer::Lexer::new(&source).tokenize().expect("lex");
        let (program, parse_errors) = crate::parser::Parser::new(tokens).parse();
        assert!(parse_errors.is_empty(), "{parse_errors:?}");
        let mut checker = crate::semantic::TypeChecker::new();
        crate::register_prelude(&mut checker).expect("prelude");
        checker.check_program(&program);
        let found: Vec<_> = checker
            .errors
            .iter()
            .filter(|d| d.severity == Severity::Error)
            .collect();
        if expect_error {
            assert_eq!(found.len(), 1, "{body}: {found:?}");
            assert!(found[0].message.contains("`B`"), "{found:?}");
        } else {
            assert!(found.is_empty(), "{body}: {found:?}");
        }
    }
}

#[test]
fn generic_new_inference_scales_linearly() {
    let mut per_size = Vec::new();
    for size in [8usize, 32, 128] {
        let mut body = String::new();
        for i in 0..size {
            body.push_str(&format!(
                "let b{i} = new Box(new Box({i})); let h{i} = new Heap([{i}], |a, b| a < b); let k{i} = new Kv(\"k\", b{i}); "
            ));
        }
        let source = format!("{PRELUDE}\nfn main() {{ {body} }}");
        let tokens = crate::lexer::Lexer::new(&source).tokenize().expect("lex");
        let (program, parse_errors) = crate::parser::Parser::new(tokens).parse();
        assert!(parse_errors.is_empty(), "{parse_errors:?}");
        super::generics::GENERIC_NEW_INFERENCES.with(|c| c.set(0));
        let mut checker = crate::semantic::TypeChecker::new();
        crate::register_prelude(&mut checker).expect("prelude");
        checker.check_program(&program);
        let found: Vec<_> = checker
            .errors
            .iter()
            .filter(|d| d.severity == Severity::Error)
            .collect();
        assert!(found.is_empty(), "{found:?}");
        let inferences = super::generics::GENERIC_NEW_INFERENCES.with(|c| c.get());
        let instances = checker
            .generics
            .registry
            .borrow()
            .class_specialization_count();
        eprintln!("generic new n={size}: inferences={inferences} instances={instances}");
        per_size.push((size, inferences, instances));
    }
    let (base, c0, i0) = per_size[0];
    assert!(c0 >= 1 && i0 >= 1, "{per_size:?}");
    for (size, count, instances) in &per_size[1..] {
        // One inference per `new`, and the same few instances for every size.
        assert!(
            *count <= (size / base) * c0,
            "superlinear growth: {per_size:?}"
        );
        assert_eq!(*instances, i0, "instances grow with repeats: {per_size:?}");
    }
}

#[test]
fn nested_inferred_constructors_keep_constant_executor_nesting() {
    std::thread::Builder::new()
        .stack_size(1024 * 1024)
        .spawn(|| {
            let mut baseline = None;
            for depth in [1, 64, 512] {
                // Every constructor instantiates Box<i64>; only expression
                // depth grows, without an increasing type or instance graph.
                let expression = format!("{}7{}", "new Box(".repeat(depth), ").v".repeat(depth));
                crate::compiler_stack::take_max_run_nesting();
                let (_, errors) = check(&format!("let value: i64 = {expression};"));
                assert!(errors.is_empty(), "{errors:?}");
                let nesting = crate::compiler_stack::take_max_run_nesting();
                eprintln!("inferred constructors: depth={depth}, executor nesting={nesting}");
                assert_eq!(nesting, *baseline.get_or_insert(nesting));
            }
        })
        .unwrap()
        .join()
        .unwrap();
}
