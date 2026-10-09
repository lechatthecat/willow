use super::support::*;

#[test]
fn frozen_iteration_runtime_matrix() {
    let cases = [
        (
            "local",
            "let a = [1, 2, 3].freeze(); for x in a { println(x); }",
            "1\n2\n3\n",
        ),
        (
            "expression",
            "for x in [4, 5].freeze() { println(x); }",
            "4\n5\n",
        ),
        (
            "empty",
            "let a: Array<i64> = []; for x in a.freeze() { println(x); }",
            "",
        ),
        ("singleton", "for x in [9].freeze() { println(x); }", "9\n"),
        (
            "bool",
            "for x in [true, false].freeze() { println(x); }",
            "true\nfalse\n",
        ),
        (
            "float",
            "for x in [1.5, 2.5].freeze() { println(x > 2.0); }",
            "false\ntrue\n",
        ),
        (
            "string",
            "for x in [\"one\", \"two\"].freeze() { println(x); }",
            "one\ntwo\n",
        ),
        (
            "nested",
            "for a in [[1, 2], [3]].freeze() { for x in a { println(x); } }",
            "1\n2\n3\n",
        ),
        (
            "break",
            "for x in [1, 2, 3].freeze() { if x == 2 { break; } println(x); }",
            "1\n",
        ),
        (
            "continue",
            "for x in [1, 2, 3].freeze() { if x == 2 { continue; } println(x); }",
            "1\n3\n",
        ),
        (
            "discard",
            "for _ in [1, 2].freeze() { println(7); }",
            "7\n7\n",
        ),
        (
            "snapshot",
            "let a = [1, 2]; let b = a.freeze(); a.push(3); for x in b { println(x); }",
            "1\n2\n",
        ),
        (
            "reassign",
            "let mut a = [1, 2].freeze(); for x in a { a = [8].freeze(); println(x); } println(a[0]);",
            "1\n2\n8\n",
        ),
        (
            "nested_frozen",
            "for x in [1, 2].freeze() { for y in [3, 4].freeze() { println(x + y); } }",
            "4\n5\n5\n6\n",
        ),
        (
            "defer",
            "defer { for x in [1, 2].freeze() { println(x); } }",
            "1\n2\n",
        ),
        (
            "body_defer",
            "for x in [1, 2].freeze() { defer println(x); }",
            "1\n2\n",
        ),
    ];
    let mut source = String::from("import std::collections::Array;\n");
    let mut expected = String::new();
    for (name, body, output) in cases {
        source.push_str(&format!("fn case_{name}() {{ {body} }}\n"));
        expected.push_str(output);
    }
    source.push_str("fn main() {");
    for (name, _, _) in cases {
        source.push_str(&format!("case_{name}();"));
    }
    source.push('}');
    for run in [
        compile_and_run,
        compile_and_run_release,
        compile_and_run_gc_stress,
    ] {
        let (out, ok) = run(&source);
        assert!(ok, "{out}");
        assert_eq!(out, expected);
    }
}

#[test]
fn frozen_iteration_fields_calls_async_and_roots() {
    let source = include_str!("../../example/frozen_iteration.wi");
    for run in [
        compile_and_run,
        compile_and_run_release,
        compile_and_run_gc_stress,
    ] {
        let (out, ok) = run(source);
        assert!(ok, "{out}");
        assert_eq!(out, "1\n2\n3\n6\nmade\n4\n5\n1\n2\n3\n6\n");
    }
}

#[test]
fn frozen_iteration_trip_counts_scale_with_length() {
    let source = r#"
import std::collections::Array;
fn count(n: i64) {
    let values: Array<i64> = [];
    for x in 0..n { values.push(x); }
    let mut trips = 0;
    let mut sum = 0;
    for x in values.freeze() { trips += 1; sum += x; }
    println(trips);
    println(sum);
}
fn main() { count(0); count(1); count(16); count(256); count(4096); }
"#;
    for run in [compile_and_run, compile_and_run_release] {
        let (out, ok) = run(source);
        assert!(ok, "{out}");
        assert_eq!(out, "0\n0\n1\n0\n16\n120\n256\n32640\n4096\n8386560\n");
    }
}
