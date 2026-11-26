//! Willow generics have no interface bounds. A written bound, a `where`
//! clause, and a method call on a type parameter each explain the limit and
//! name the interface-parameter and function-parameter alternatives.
use crate::diagnostics::{Diagnostic, ErrorCode, Severity};

const PRELUDE: &str = "import std::collections::Array;
interface Show { fn show(self) -> String; }
interface Scale { fn scale(self, by: i64, label: String) -> f64; }
interface Wrap<T> { fn wrap(self) -> T; }
class N implements Show { pub v: i64; pub fn show(self) -> String { return \"n\"; } }
";

fn parse_errors(source: &str) -> Vec<Diagnostic> {
    let source = format!("{PRELUDE}\n{source}");
    let tokens = crate::lexer::Lexer::new(&source).tokenize().expect("lex");
    let (_, errors) = crate::parser::Parser::new(tokens).parse();
    errors
}

fn check_errors(body: &str) -> Vec<Diagnostic> {
    let source = format!("{PRELUDE}\n{body}");
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

fn assert_bound_error(name: &str, errors: &[Diagnostic], written: &str, interface: &str) {
    assert_eq!(errors.len(), 1, "{name}: {errors:?}");
    let error = &errors[0];
    assert_eq!(error.code, ErrorCode::E0102, "{name}: {error:?}");
    assert_eq!(
        error.message,
        format!("type parameter bounds are not supported: `{written}`"),
        "{name}"
    );
    assert!(
        error
            .notes
            .iter()
            .any(|n| n.contains("cannot require an interface")),
        "{name}: {:?}",
        error.notes
    );
    let help = error.helps.join("\n");
    assert!(
        help.contains(&format!("`x: {interface}`"))
            && help.contains(&format!("`Array<{interface}>`")),
        "{name}: {help}"
    );
    assert!(help.contains("op: fn("), "{name}: {help}");
}

#[test]
fn written_bounds_explain_the_limit_everywhere_type_parameters_appear() {
    for (name, source, written, interface) in [
        (
            "function",
            "fn join<T: Show>(xs: Array<T>) -> String { return \"\"; }",
            "T: Show",
            "Show",
        ),
        (
            "second parameter",
            "fn pair<A, B: Show>(a: A, b: B) -> A { return a; }",
            "B: Show",
            "Show",
        ),
        (
            "several bounds",
            "fn both<T: Show + Scale>(x: T) -> T { return x; }",
            "T: Show + Scale",
            "Show",
        ),
        (
            "generic interface bound",
            "fn unwrap<T: Wrap<i64>>(x: T) -> T { return x; }",
            "T: Wrap<i64>",
            "Wrap<i64>",
        ),
        (
            "class",
            "class Shelf<T: Show> { pub items: Array<T>; }",
            "T: Show",
            "Show",
        ),
        (
            "enum",
            "enum Tree<T: Show> { Leaf(T), Empty }",
            "T: Show",
            "Show",
        ),
        (
            "interface",
            "interface Holder<T: Show> { fn get(self) -> T; }",
            "T: Show",
            "Show",
        ),
        (
            "async function",
            "async fn later<T: Show>(x: T) -> T { return x; }",
            "T: Show",
            "Show",
        ),
        (
            "public function",
            "pub fn shown<T: Show>(x: T) -> T { return x; }",
            "T: Show",
            "Show",
        ),
        (
            "class with methods",
            "class Shelf<T: Show> { pub item: T; pub fn get(self) -> T { return self.item; } }",
            "T: Show",
            "Show",
        ),
        (
            "array bound",
            "fn f<T: Array<i64>>(x: T) -> T { return x; }",
            "T: Array<i64>",
            "Array<i64>",
        ),
        (
            "function where clause",
            "fn join<T>(xs: Array<T>) -> String where T: Show { return \"\"; }",
            "where T: Show",
            "Show",
        ),
        (
            "function where clause without return type",
            "fn each<T>(xs: Array<T>) where T: Show { }",
            "where T: Show",
            "Show",
        ),
        (
            "class where clause",
            "class Shelf<T> where T: Show { pub items: Array<T>; }",
            "where T: Show",
            "Show",
        ),
    ] {
        let errors = parse_errors(source);
        assert_bound_error(name, &errors, written, interface);
    }
}

#[test]
fn where_clause_reports_each_predicate() {
    let errors =
        parse_errors("fn pair<A, B>(a: A, b: B) -> A where A: Show, B: Scale { return a; }");
    let messages: Vec<_> = errors.iter().map(|d| d.message.as_str()).collect();
    assert_eq!(
        messages,
        [
            "type parameter bounds are not supported: `where A: Show`",
            "type parameter bounds are not supported: `where B: Scale`",
        ],
        "{errors:?}"
    );
}

#[test]
fn malformed_where_clause_is_rejected_once() {
    for source in [
        "fn f<T>(x: T) -> T where { return x; }",
        "fn f<T>(x: T) -> T where T { return x; }",
    ] {
        let errors = parse_errors(source);
        assert_eq!(errors.len(), 1, "{source}: {errors:?}");
        assert_eq!(
            errors[0].message, "`where` clauses are not supported here",
            "{source}"
        );
    }
}

#[test]
fn bound_label_covers_the_bound_text() {
    for (source, covered) in [
        (
            "fn join<T: Show>(xs: Array<T>) -> String { return \"\"; }",
            ": Show",
        ),
        (
            "fn join<T>(xs: Array<T>) -> String where T: Show { return \"\"; }",
            "where T: Show",
        ),
    ] {
        let errors = parse_errors(source);
        let full = format!("{PRELUDE}\n{source}");
        let start = PRELUDE.len() + 1 + source.find(covered).expect("covered text");
        let span = errors[0].labels[0].span;
        assert_eq!(
            (span.start, span.end),
            (start, start + covered.len()),
            "{}",
            &full[span.start..span.end]
        );
    }
}

#[test]
fn bound_errors_do_not_cascade_into_later_items() {
    let errors = parse_errors(
        "fn join<T: Show>(xs: Array<T>) -> String { let s = xs.len(); return \"\"; }
fn main() { println(join([new N(1)])); }",
    );
    assert_eq!(errors.len(), 1, "{errors:?}");
}

#[test]
fn method_where_constraint_is_still_accepted() {
    let errors = parse_errors(
        "class Cell<T> { pub v: T; pub fn twice(self) -> T where T: i64 | f64 { return self.v; } }",
    );
    assert!(errors.is_empty(), "{errors:?}");
}

#[test]
fn method_call_on_type_parameter_names_the_alternatives() {
    for (name, body, help) in [
        (
            "interface declares the method",
            "fn join<T>(xs: Array<T>) -> String { let mut s = \"\"; for x in xs { s = s + x.show(); } return s; }",
            "take `Show` instead of `T`, as in `x: Show` or `Array<Show>`, so `show` dispatches through the interface; or keep `T` and pass the operation as a parameter, as in `show: fn(T) -> String`",
        ),
        (
            "method parameters carried into the function type",
            "fn f<T>(x: T) -> f64 { return x.scale(2, \"k\"); }",
            "`scale: fn(T, i64, String) -> f64`",
        ),
        (
            "no interface declares the method",
            "fn g<T>(x: T) -> i64 { return x.frob(1); }",
            "`frob: fn(T) -> R` with `R` its result type",
        ),
        (
            "generic interface is not an alternative",
            "fn w<T>(x: T) -> i64 { return x.wrap(); }",
            "`wrap: fn(T) -> R`",
        ),
        (
            "class type parameter",
            "class Shelf<T> { pub item: T; pub fn label(self) -> String { return self.item.show(); } }",
            "take `Show` instead of `T`",
        ),
    ] {
        let errors = check_errors(body);
        let error = errors
            .iter()
            .find(|d| d.code == ErrorCode::E0201 && d.message.contains("has no method"))
            .unwrap_or_else(|| panic!("{name}: {errors:?}"));
        assert!(
            error
                .notes
                .iter()
                .any(|n| n.contains("no interface bounds")),
            "{name}: {:?}",
            error.notes
        );
        let helps = error.helps.join("\n");
        assert!(helps.contains(help), "{name}: {helps}");
    }
}

#[test]
fn method_call_on_type_parameter_reports_once_per_call() {
    let errors = check_errors(
        "fn join<T>(xs: Array<T>) -> String { let mut s = \"\"; for x in xs { s = s + x.show() + x.show(); } return s; }",
    );
    let count = errors
        .iter()
        .filter(|d| d.message.contains("has no method `show`"))
        .count();
    assert_eq!(count, 2, "{errors:?}");
    assert_eq!(errors.len(), 2, "{errors:?}");
}

#[test]
fn suggested_alternatives_type_check() {
    for body in [
        "fn join(xs: Array<Show>) -> String { let mut s = \"\"; for x in xs { s = s + x.show(); } return s; }
fn main() { println(join([new N(1), new N(2)])); }",
        "fn join<T>(xs: Array<T>, show: fn(T) -> String) -> String { let mut s = \"\"; for x in xs { s = s + show(x); } return s; }
fn main() { println(join([new N(1), new N(2)], |n| n.show())); }",
    ] {
        let errors = check_errors(body);
        assert!(errors.is_empty(), "{body}: {errors:?}");
    }
}
