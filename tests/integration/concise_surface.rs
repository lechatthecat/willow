use super::support::*;

#[test]
fn concise_surface_example() {
    let (out, ok) = compile_and_run(include_str!("../../example/concise_surface.wi"));
    assert!(ok, "{out}");
    assert_eq!(out, "score=2\n4\n7\n5\n2.0\n-0.0\n");
}

#[test]
fn concise_surface_output_helpers_preserve_constructor_initialization() {
    let source = r#"
import std::io as out;
import std::io::println as line;
fn tidy() { print("direct "); println(1); }
fn qualified() { std::io::print("qualified "); std::io::println(2); }
fn aliased() { out::print("aliased "); line(3); }
class C {
    pub x: i64;
    pub init(self) {
        defer match recover() { Some(_) => {}, None => {} };
        tidy(); qualified(); aliased();
        self.x = 4;
    }
}
fn main() { println(C().x); }
"#;
    let (out, ok) = compile_and_run(source);
    assert!(ok, "{out}");
    assert_eq!(out, "direct 1\nqualified 2\naliased 3\n4\n");
}

#[test]
fn concise_surface_constructors_and_tails() {
    // Memberwise, explicit, generic/inferred/contextual, legacy construction;
    // function/method/default/closure/async tails, early return, cleanup, tuples.
    let source = r#"
import std::collections;
class Box<T> { pub value: T; }
class Empty<T> { pub init(self) {} }
class Holder { pub callback: fn(i64) -> i64; }
class Counter { pub n: i64; pub init(self, n: i64) { self.n = n + 1; } }
interface Value { fn value(self) -> i64 { 12 } }
class Default implements Value {}
fn early(x: i64) -> i64 { if x == 0 { return 10; } x + 1 }
fn cleanup() -> i64 { defer println("defer"); 4 }
fn tuple() -> i64 { let (a, b) = (2, 3); a + b }
fn generic<T>(v: T) -> T { v }
async fn task() -> i64 { 6 }
async fn main() {
    println(Box(3).value);
    println(Box<i64>(4).value);
    let empty: Empty<i64> = Empty();
    println(Counter(4).n);
    println(new Counter(5).n);
    println(Default().value());
    println(early(0)); println(early(2));
    println(cleanup()); println(tuple()); println(generic(8));
    println(await task());
    let f = || -> i64 { if true { 9 } else { 10 } };
    println(f());
    let xs = [1];
    let holder = Holder(|value: i64| value + xs[0]);
    let callback = holder.callback;
    println(callback(2));
}
"#;
    let (out, ok) = compile_and_run_gc_stress(source);
    assert!(ok, "{out}");
    assert_eq!(out, "3\n4\n5\n6\n12\n10\n3\ndefer\n4\n5\n8\n6\n9\n3\n");
}

#[test]
fn concise_surface_output_resolution() {
    // Item/module aliases, full paths, user function/parameter/local shadowing,
    // methods named print, scalar output and matching inferred callable types.
    let source = r#"
import std::collections;
import std::io as output;
import std::io::println as line;
fn println(x: i64) -> i64 { x + 1 }
fn apply(print: fn(i64) -> i64) -> i64 { print(3) }
class Printer { pub fn print(self) -> i64 { 8 } }
fn main() {
    line(println(2));
    let print = |x: i64| x * 2;
    line(print(3)); line(apply(print)); line(Printer().print());
    output::print("ok"); std::io::println(true);
    line(2.0); line([1.0, 2.0].to_string());
}
"#;
    let (out, ok) = compile_and_run(source);
    assert!(ok, "{out}");
    assert_eq!(out, "3\n6\n6\n8\noktrue\n2.0\n[1.0, 2.0]\n");
}

#[test]
fn concise_surface_migration_diagnostics() {
    for (source, expected) in [
        (
            "fn f() -> i64 { 1; } fn main() {}",
            "tail expression without `;`",
        ),
        (
            "fn f() -> i64 { if true { 1 } 2 } fn main() {}",
            "tail position",
        ),
        ("fn main() { while false { 1 } }", "tail position"),
        ("fn main() { defer { 1 } }", "tail position"),
        ("fn f() -> i64 { true } fn main() {}", "mismatched types"),
        ("fn main() { let p = println; }", "typed lambda"),
        ("fn main() { println(); }", "argument"),
        ("fn main() { print(1, 2); }", "argument"),
        ("fn main() { let n = 1; println(&n); }", "remove `&`"),
        (
            "class C { pub n: i64; } fn main() { println(C(1)); }",
            "cannot println",
        ),
        ("class C { pub n: i64; } fn main() { C(); }", "constructor"),
    ] {
        let error = compile_error_stderr(source);
        assert!(error.contains(expected), "{source}: {error}");
    }
}

#[test]
fn concise_surface_json_number_spelling() {
    let (out, ok) = compile_and_run(
        r#"
import std::json;
fn main() {
    println(json::serialize(json::parse("2").unwrap()).unwrap());
    println(json::serialize(json::Value::Number((2.0).to_string())).unwrap());
    println(json::serialize(json::Value::Number((-0.0).to_string())).unwrap());
    println(json::serialize(json::Value::Number((1.0 / 0.0).to_string())).is_err());
}
"#,
    );
    assert!(ok, "{out}");
    assert_eq!(out, "2\n2.0\n-0.0\ntrue\n");
}

#[test]
fn concise_surface_imported_constructors() {
    let files = [
        (
            "main.wi",
            r#"
import model;
import model::User as Person;
import model::Box as Container;
fn main() {
    println(model::User(1).score);
    println(Person(2).score);
    println(model::Box<i64>(3).value);
    println(Container(4).value);
}
"#,
        ),
        (
            "model.wi",
            "pub class User { pub score: i64; } pub class Box<T> { pub value: T; }",
        ),
    ];
    let (out, ok) = compile_temp_project_and_run(&files, "main.wi");
    assert!(ok, "{out}");
    assert_eq!(out, "1\n2\n3\n4\n");
}

#[test]
fn concise_surface_emitted_output_calls_scale_linearly() {
    for count in [8, 32, 128] {
        let source = format!("fn main() {{ {} }}", "std::io::println(2.0);".repeat(count));
        let targets = compile_and_collect_relocation_targets_all(&source, &[]);
        let actual = targets
            .iter()
            .filter(|name| name.contains("willow_println_f64"))
            .count();
        assert_eq!(actual, count, "{targets:?}");
        eprintln!("output sites={count}, runtime calls={actual}");
    }
}

#[test]
fn concise_surface_constructor_effects_and_statement_compatibility() {
    let source = r#"
class Holder { pub callback: fn() -> void; }
fn output() { println(1); }
@effects(no_io) fn store() -> Holder { Holder(output) }
fn init() -> i64 { 3 }
fn main() {
    println(init());
    let holder = store();
    while false { match true { true => println(1), false => println(2) } }
    defer { match true { true => println(5), false => println(6) } }
    if true { match false { true => println(7), false => println(8) } }
    println(9);
}
"#;
    let (out, ok) = compile_and_run(source);
    assert!(ok, "{out}");
    assert_eq!(out, "3\n8\n9\n5\n");
    for (source, expected) in [
        (
            "class C {} @effects(no_alloc) fn f() { C(); } fn main() {}",
            "E2610",
        ),
        ("class C {} @pure fn f() { C(); } fn main() {}", "pure"),
        (
            "@effects(no_io) fn f() { println(1); } fn main() {}",
            "E2610",
        ),
        ("class C {} fn main() { defer C(); }", "E0905"),
        (
            "class C { pub init(self, m: Mutex<i64>) { lock m as v { println(v); } } } fn main() { let m = Mutex<i64>::new(1); lock m as held { C(m); } }",
            "E2605",
        ),
    ] {
        let error = compile_error_stderr(source);
        assert!(error.contains(expected), "{source}: {error}");
    }
}

#[test]
fn concise_surface_qualified_functions_precede_constructors() {
    let files = [
        (
            "model.wi",
            r#"
pub class C { pub n: i64; }
pub fn C(n: i64) -> i64 { n + 10 }
pub class Box<T> { pub n: T; }
pub fn G<T>(n: T) -> T { n }
pub class Only { pub n: i64; }
"#,
        ),
        (
            "main.wi",
            r#"
import model;
import model as m;
import model::C as Item;
import model::G as Generic;
class Local { pub n: i64; }
fn Local(n: i64) -> i64 { n + 20 }
fn main() {
    println(model::C(1)); println(m::C(2)); println(Item(3)); println(Local(4));
    println(model::G<i64>(5)); println(m::G<i64>(6)); println(Generic<i64>(7));
    let inferred: i64 = m::G(8); println(inferred);
    println(new model::C(9).n); println(new m::Box<i64>(10).n);
    println(model::Only(11).n); println(m::Only(12).n);
}
"#,
        ),
    ];
    let (out, ok) = compile_temp_project_and_run(&files, "main.wi");
    assert!(ok, "{out}");
    assert_eq!(out, "11\n12\n13\n24\n5\n6\n7\n8\n9\n10\n11\n12\n");
}

#[test]
fn concise_surface_scalar_output_in_async_cleanup() {
    let (out, ok) = compile_and_run(
        r#"
import std::io::println as line;
import std::io as out;
async fn work() {
    let mut op: fn(i64) -> i64 = |x| x;
    defer { std::io::println(op(1)); }
    defer { println(1); out::print(2.0); line(true); op = |x| x + 1; }
    await sleep(0);
}
async fn main() { await work(); }
"#,
    );
    assert!(ok, "{out}");
    assert_eq!(out, "1\n2.0true\n2\n");
}
