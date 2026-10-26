use super::support::*;

#[test]
fn constrained_methods_scalar_domains_execute() {
    let source = r#"
class Box<T> {
    pub value: T;
    pub fn render() -> String where T: i64 | f64 | bool | String { return self.value.toString(); }
    pub fn suffix(s: String) -> String where T: String { return self.value + s; }
    pub fn get() -> T { return self.value; }
}
fn main() {
    println(new Box<i64>(42).render());
    println(new Box<f64>(1.5).render());
    println(new Box<bool>(true).render());
    println(new Box<String>("hello").render());
    println(new Box<String>("hello").suffix("!"));
    println(new Box<i64>(7).get());
}
"#;
    for run in [
        compile_and_run,
        compile_and_run_release,
        compile_and_run_gc_stress,
    ] {
        let (out, ok) = run(source);
        assert!(ok, "{out}");
        assert_eq!(out, "42\n1.5\ntrue\nhello\nhello!\n7\n");
    }
}

#[test]
fn constrained_methods_validate_unused_alternatives_and_availability() {
    for source in [
        "class Box<T> { pub value: T; pub fn f() -> String where T: String | i64 { return self.value + \"!\"; } } fn main() {}",
        "class Box<T> { pub value: T; pub fn f() -> String where U: String { return \"x\"; } } fn main() {}",
        "class Box<T> { pub value: T; pub fn f() -> String where T: String | String { return self.value; } } fn main() {}",
        "class Box<T> { pub value: T; pub fn f() -> String where T: T { return \"x\"; } } fn main() {}",
        "class Box { pub fn f() where T: i64 {} } fn main() {}",
        "class Box<T> { pub value: T; pub fn f() -> String where T: String { return self.value; } } fn main() { println(new Box<i64>(1).f()); }",
        "class Box<T> { pub value: T; pub fn f() -> String where T: String | i64 { return self.value.noSuchMethod(); } } fn main() {}",
    ] {
        assert!(expect_compile_error(source), "{source}");
    }
}

#[test]
fn source_arrays_literal_methods_do_not_require_imports() {
    let source = r#"
fn main() {
    let xs = [[1], [2, 3]];
    println(xs.len());
    xs.push([4]);
    println(xs.pop()[0]);
    let frozen = xs.freeze();
    xs[0] = [9];
    println(frozen[0][0]);
    println([true, false].toString());
    println([1.5, 2.0].toString());
}
"#;
    for run in [
        compile_and_run,
        compile_and_run_release,
        compile_and_run_gc_stress,
    ] {
        let (out, ok) = run(source);
        assert!(ok, "{out}");
        assert_eq!(out, "2\n4\n1\n[true, false]\n[1.5, 2]\n");
    }
    for source in [
        "fn main() { let xs: Array<i64> = [1]; }",
        "fn main() { [1].__abi_len(); }",
        "fn main() { [1].__abi_resize(9); }",
        "fn main() { [1].__abi_copy(); }",
        "fn main() { [\"x\"].__abi_concat(); }",
        "fn main() { \"x\".__abi_debug(); }",
    ] {
        assert!(expect_compile_error(source), "{source}");
    }
}

#[test]
fn source_array_constructors_use_declaration_dispatch() {
    let source = r#"
import std::collections::Array;
import std::collections::Array as Items;
fn empty() -> Array<String> { return Items::new(); }
fn main() {
    let numbers: Array<i64> = Array::new();
    numbers.push(42);
    let words = empty();
    words.push("hello");
    let flags = Array<bool>::new();
    let qualified: Array<i64> = std::collections::Array::new();
    qualified.push(9);
    flags.push(true);
    println(numbers[0]);
    println(words[0]);
    println(flags[0]);
    println(qualified[0]);
}
"#;
    for run in [
        compile_and_run,
        compile_and_run_release,
        compile_and_run_gc_stress,
    ] {
        let (out, ok) = run(source);
        assert!(ok, "{out}");
        assert_eq!(out, "42\nhello\ntrue\n9\n");
    }
    for source in [
        "fn main() { let a = Array<i64>::new(); }",
        "import std::collections::Array; fn main() { let a = Array<i64>::new(1); }",
        "import std::collections::Array; fn main() { let a = Array<i64, String>::new(); }",
    ] {
        assert!(expect_compile_error(source), "{source}");
    }
}

#[test]
fn source_array_nominal_specializations_remain_distinct() {
    let source = r#"
class Cell<T> { pub value: T; }
fn main() {
    let numbers = [new Cell<i64>(42)];
    let words = [new Cell<String>("kept")];
    numbers.push(new Cell<i64>(7));
    words.push(new Cell<String>("last"));
    gc_collect();
    println(numbers.pop().value);
    println(words.pop().value);
    println(numbers[0].value);
    println(words[0].value);
}
"#;
    for run in [
        compile_and_run,
        compile_and_run_release,
        compile_and_run_gc_stress,
    ] {
        let (out, ok) = run(source);
        assert!(ok, "{out}");
        assert_eq!(out, "7\nlast\n42\nkept\n");
    }
}

#[test]
fn source_arrays_example() {
    let source = include_str!("../../example/source_arrays.wi");
    let (out, ok) = compile_and_run(source);
    assert!(ok, "{out}");
    assert_eq!(out, "source methods\nmethods\n2\n[\"source\"]\n42\n");
}

#[test]
fn constrained_methods_imported_declarations_preserve_domains() {
    for (argument, valid) in [("String", true), ("i64", false)] {
        let expression = if valid { "\"ok\"" } else { "42" };
        let main = format!(
            "import values::Display; fn main() {{ println(new Display<{argument}>({expression}).text()); }}"
        );
        let project = TestProject::new(
            "constrained_imports",
            &[
                (
                    "values.wi",
                    "module values; pub class Display<T> { pub value: T; pub fn text() -> String where T: String { return self.value + \"!\"; } }",
                ),
                ("main.wi", &main),
            ],
        );
        let result = project.compile("main.wi");
        assert_eq!(
            result.status.success(),
            valid,
            "{}",
            String::from_utf8_lossy(&result.stderr)
        );
        if valid {
            let result = project.run_with_env_timeout(
                &[("WILLOW_GC_STRESS", "alloc,scheduler")],
                std::time::Duration::from_secs(30),
            );
            assert!(
                result.status.success(),
                "{}",
                String::from_utf8_lossy(&result.stderr)
            );
            assert_eq!(String::from_utf8_lossy(&result.stdout), "ok!\n");
        }
    }
}

#[test]
fn source_array_method_operands_execute_once_before_body() {
    let source = r#"
import std::collections::Array;
class Cell { pub text: String; }
fn receiver(xs: Array<i64>) -> Array<i64> { println("receiver"); return xs; }
fn argument(xs: Array<i64>) -> i64 { println("argument"); xs.push(7); return 9; }
fn cell(xs: Array<Cell>) -> Cell {
    xs.push(new Cell("nested"));
    gc_collect();
    return new Cell("last");
}
fn main() {
    let xs: Array<i64> = [];
    receiver(xs).push(argument(xs));
    println(xs.toString());
    let cells = [new Cell("first")];
    cells.push(cell(cells));
    gc_collect();
    println(cells.len());
    println(cells[0].text);
    println(cells[1].text);
    println(cells[2].text);
}
"#;
    for run in [
        compile_and_run,
        compile_and_run_release,
        compile_and_run_gc_stress,
    ] {
        let (out, ok) = run(source);
        assert!(ok, "{out}");
        assert_eq!(out, "receiver\nargument\n[7, 9]\n3\nfirst\nnested\nlast\n");
    }
}
