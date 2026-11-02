//! Demand-driven generic method specialization and dispatch roots.
use super::support::*;

#[test]
fn unused_method_count_does_not_increase_generated_bodies() {
    for methods in [1, 8, 32, 128] {
        let unused: String = (0..methods)
            .map(|i| format!("pub fn unused{i}() -> i64 {{ return {i}; }}"))
            .collect();
        for instances in [1, 2, 4] {
            for calls in [1, 16] {
                let types = [
                    ("i64", "7"),
                    ("String", "\"x\""),
                    ("bool", "true"),
                    ("f64", "1.5"),
                ];
                let mut body = String::new();
                for (n, (ty, value)) in types[..instances].iter().enumerate() {
                    body.push_str(&format!("let a{n} = new Cell<{ty}>({value});"));
                    body.push_str(&format!("println(a{n}.get());").repeat(calls));
                }
                let source = format!(
                    "class Cell<T> {{ pub value: T; pub fn get() -> T {{ return self.value; }} {unused} }} fn main() {{ {body} }}"
                );
                let symbols = compile_and_collect_defined_symbols(&source, &[]);
                let emitted: Vec<_> = symbols
                    .iter()
                    .filter(|s| s.starts_with("Cell$mono$") && s.contains('.'))
                    .collect();
                assert_eq!(emitted.len(), instances, "{symbols:#?}");
                assert!(emitted.iter().all(|s| s.ends_with(".get")), "{emitted:?}");
                println!(
                    "unused={methods} instances={instances} calls={calls} emitted={}",
                    emitted.len()
                );
            }
        }
    }
}

#[test]
fn reachable_generic_method_execution_perspectives() {
    // Each case exercises a different demand edge or retained dispatch root.
    let cases = [
        (
            "static",
            "class C<T> { pub static fn get() -> i64 { return 1; } } fn main() { println(C<i64>::get()); }",
            "1\n",
        ),
        (
            "instance",
            "class C<T> { pub value: T; pub fn get() -> T { return self.value; } } fn main() { println(new C<i64>(2).get()); }",
            "2\n",
        ),
        (
            "private helper",
            "class C<T> { fn help() -> i64 { return 3; } pub fn get() -> i64 { return self.help(); } } fn main() { println(new C<i64>().get()); }",
            "3\n",
        ),
        (
            "static helper",
            "class C<T> { static fn help() -> i64 { return 4; } pub static fn get() -> i64 { return C<T>::help(); } } fn main() { println(C<i64>::get()); }",
            "4\n",
        ),
        (
            "recursive",
            "class C<T> { pub fn get(n: i64) -> i64 { if n == 0 { return 5; } return self.get(n - 1); } } fn main() { println(new C<i64>().get(8)); }",
            "5\n",
        ),
        (
            "mutually recursive",
            "class C<T> { fn help(n: i64) -> i64 { return self.get(n); } pub fn get(n: i64) -> i64 { if n == 0 { return 6; } return self.help(n - 1); } } fn main() { println(new C<i64>().get(8)); }",
            "6\n",
        ),
        (
            "constructor edge",
            "class C<T> { pub value: i64; pub init(self) { self.value = C<T>::get(); } static fn get() -> i64 { return 7; } } fn main() { println(new C<i64>().value); }",
            "7\n",
        ),
        (
            "lambda edge",
            "class C<T> { pub fn get() -> i64 { let f = || -> i64 { return self.help(); }; return f(); } fn help() -> i64 { return 8; } } fn main() { println(new C<i64>().get()); }",
            "8\n",
        ),
        (
            "generic function edge",
            "class C<T> { pub value: T; pub fn get() -> T { return self.value; } } fn get<U>(x: C<U>) -> U { return x.get(); } fn main() { println(get(new C<i64>(9))); }",
            "9\n",
        ),
        (
            "second class edge",
            "class D<T> { pub value: T; pub fn get() -> T { return self.value; } } class C<T> { pub value: T; pub fn get() -> T { return new D<T>(self.value).get(); } } fn main() { println(new C<i64>(10).get()); }",
            "10\n",
        ),
        (
            "interface slot",
            "interface I { fn get() -> i64; } class C<T> implements I { pub fn get() -> i64 { return 11; } } fn show(x: I) { println(x.get()); } fn main() { show(new C<i64>()); }",
            "11\n",
        ),
        (
            "default slot",
            "interface I { fn get(self) -> i64 { return 12; } } class C<T> implements I {} fn show(x: I) { println(x.get()); } fn main() { show(new C<i64>()); }",
            "12\n",
        ),
        (
            "generic interface",
            "interface I<T> { fn get() -> T; } class C<T> implements I<T> { pub value: T; pub fn get() -> T { return self.value; } } fn show(x: I<i64>) { println(x.get()); } fn main() { show(new C<i64>(13)); }",
            "13\n",
        ),
        (
            "open slot",
            "open class C<T> { pub open fn get() -> i64 { return 14; } } fn main() { println(new C<i64>().get()); }",
            "14\n",
        ),
        (
            "constraint",
            "class C<T> { pub value: T; pub fn get() -> String where T: String { return self.value; } } fn main() { println(new C<String>(\"15\").get()); }",
            "15\n",
        ),
        (
            "distinct instances",
            "class C<T> { pub value: T; pub fn get() -> T { return self.value; } } fn main() { println(new C<i64>(16).get()); println(new C<String>(\"x\").get()); }",
            "16\nx\n",
        ),
        (
            "defer edge",
            "class C<T> { pub fn get() { defer self.help(); } fn help() { println(17); } } fn main() { new C<i64>().get(); }",
            "17\n",
        ),
        (
            "array new",
            "import std::collections::Array; fn main() { let a: Array<i64> = Array::new(); a.push(18); println(a.pop()); }",
            "18\n",
        ),
        (
            "array constraint transitive",
            "import std::collections::Array; fn main() { let a = [\"19\"]; println(a.join(\",\")); }",
            "19\n",
        ),
        (
            "unused sibling",
            "class C<T> { pub value: T; pub fn get() -> T { return self.value; } pub fn unused() -> i64 { return 99; } } fn main() { println(new C<i64>(20).get()); }",
            "20\n",
        ),
    ];
    for (perspective, source, expected) in cases {
        let (output, ok) = compile_and_run(source);
        assert!(ok, "{perspective}: {output}");
        assert_eq!(output, expected, "{perspective}");
    }
}

#[test]
fn reachable_generic_methods_example() {
    let source = include_str!("../../example/reachable_generic_methods.wi");
    for run in [compile_and_run, compile_and_run_release] {
        let (out, ok) = run(source);
        assert!(ok, "{out}");
        assert_eq!(out, "42\nhello\n0\n");
    }
}

#[test]
fn dispatch_roots_are_emitted_without_direct_calls() {
    let source = "interface I { fn required(self) -> i64; } open class Base { pub open fn overridden() -> i64 { return 0; } } class Cell<T> extends Base implements I { pub fn required() -> i64 { return 1; } pub override fn overridden() -> i64 { return 2; } pub fn unused() -> i64 { return 3; } } open class Open<T> { pub open fn slot() -> i64 { return 4; } pub fn unused() -> i64 { return 5; } } fn main() { let a = new Cell<i64>(); let b = new Open<i64>(); }";
    let symbols = compile_and_collect_defined_symbols(source, &[]);
    for (class, method) in [
        ("Cell", "required"),
        ("Cell", "overridden"),
        ("Open", "slot"),
    ] {
        assert!(
            symbols
                .iter()
                .any(|s| s.starts_with(&format!("{class}$mono$"))
                    && s.ends_with(&format!(".{method}"))),
            "{symbols:?}"
        );
    }
    assert!(
        !symbols.iter().any(|s| s.ends_with(".unused")),
        "{symbols:?}"
    );
}

#[test]
fn reachable_method_chain_and_fanout_emit_each_body_once() {
    for depth in [1, 8, 32] {
        let mut methods = String::from("pub fn step0() -> i64 { return 1; }");
        for n in 1..depth {
            methods.push_str(&format!(
                "pub fn step{n}() -> i64 {{ return self.step{}() + self.step{}(); }}",
                n - 1,
                n - 1
            ));
        }
        let source = format!(
            "class Cell<T> {{ {methods} }} fn main() {{ let a = new Cell<i64>(); println(a.step{}()); println(a.step{}()); }}",
            depth - 1,
            depth - 1
        );
        let symbols = compile_and_collect_defined_symbols(&source, &[]);
        assert_eq!(
            symbols
                .iter()
                .filter(|s| s.starts_with("Cell$mono$") && s.contains(".step"))
                .count(),
            depth,
            "{symbols:?}"
        );
    }
}

#[test]
fn unused_generic_methods_still_receive_source_diagnostics() {
    let source = "class Cell<T> { pub fn unused() -> i64 { return missing(); } } fn main() { let a = new Cell<i64>(); }";
    let errors = compile_error_stderr(source);
    assert!(errors.contains("missing"), "{errors}");
    assert!(errors.contains("error["), "{errors}");
}

#[test]
fn type_only_class_use_emits_no_method_bodies() {
    let source = "class Cell<T> { pub fn unused() -> i64 { return 1; } } fn take(c: Cell<i64>) {} fn main() {}";
    let symbols = compile_and_collect_defined_symbols(source, &[]);
    assert!(
        !symbols
            .iter()
            .any(|s| s.starts_with("Cell$mono$") && s.contains('.')),
        "{symbols:?}"
    );
}
