use super::*;

#[test]
fn test_map_float_key_semantics_example() {
    let (out, ok) = compile_and_run(include_str!("../../../example/map_float_keys.wi"));
    assert!(ok, "compilation failed");
    assert_eq!(
        out,
        "true\n1\n20\n20\n{0.0: 20}\ntrue\nfalse\nNaN cannot be used as a Map key\n1\n"
    );
}

#[test]
fn test_map_float_nan_faults_propagate_and_recover() {
    let mut source = String::from("import std::collections::Map;\n");
    let operations = [
        ("Map<f64, i64>", "keys", "keys.insert(nan, 99)"),
        ("Map<f64, i64>", "keys", "keys.get(nan)"),
        ("Map<f64, i64>", "keys", "keys.contains(nan)"),
        ("FrozenMap<f64, i64>", "keys.freeze()", "keys.get(nan)"),
        ("FrozenMap<f64, i64>", "keys.freeze()", "keys.contains(nan)"),
        ("Map<f64, String>", "strings", "keys.get(nan)"),
        (
            "FrozenMap<f64, String>",
            "strings.freeze()",
            "keys.get(nan)",
        ),
    ];
    for (index, (ty, _, operation)) in operations.iter().enumerate() {
        source.push_str(&format!(
            "fn probe{index}(keys: {ty}, nan: f64) {{ {operation}; println(999); }}\n"
        ));
    }
    source.push_str(
        r#"
fn main() {
    let keys: Map<f64, i64> = Map::new();
    keys.insert(-0.0, 20);
    let strings: Map<f64, String> = Map::new();
    strings.insert(-0.0, "alive");
    let nan = 0.0 / 0.0;
"#,
    );
    for (index, (_, receiver, _)) in operations.iter().enumerate() {
        source.push_str(&format!(
            r#"
    if true {{
        defer match recover() {{ Some(info) => println(info.message), None => {{}} }}
        probe{index}({receiver}, nan);
        println(888);
    }}
"#
        ));
    }
    source.push_str(
        r#"
    println(keys.len());
    println(keys.get(0.0).unwrap());
    println(strings.get(0.0).unwrap());
    let values: Map<i64, f64> = Map::new();
    values.insert(1, nan);
    println(values.contains(1));
    println(nan == nan);
}
"#,
    );
    let expected = format!(
        "{}1\n20\nalive\ntrue\nfalse\n",
        "NaN cannot be used as a Map key\n".repeat(operations.len())
    );
    for run in [
        compile_and_run,
        compile_and_run_release,
        compile_and_run_gc_stress_all,
    ] {
        let (out, ok) = run(&source);
        assert!(ok, "{out}");
        assert_eq!(out, expected);
    }
}

#[test]
fn test_map_float_nan_without_recover_fails() {
    let (out, ok) = compile_and_run_check_exit(
        r#"
import std::collections::Map;
fn main() {
    let keys: Map<f64, i64> = Map::new();
    keys.contains(0.0 / 0.0);
    println("unreachable");
}
"#,
    );
    assert!(!ok, "{out}");
    assert!(out.contains("NaN cannot be used as a Map key"), "{out}");
    assert!(!out.contains("unreachable"), "{out}");
}

#[test]
fn test_println_i64() {
    let (out, ok) = compile_and_run("fn main() { println(42); }");
    assert!(ok, "compilation failed");
    assert_eq!(out.trim(), "42");
}

#[test]
fn test_println_string_literal() {
    let (out, ok) = compile_and_run(r#"fn main() { println("Hello, world!"); }"#);
    assert!(ok, "compilation failed");
    assert_eq!(out, "Hello, world!\n");
}

#[test]
fn test_print_string_variable() {
    let src = r#"
fn main() {
    let greeting: String = "hello";
    print(greeting);
    println(" willow");
}
"#;
    let (out, ok) = compile_and_run(src);
    assert!(ok, "compilation failed");
    assert_eq!(out, "hello willow\n");
}

#[test]
fn test_string_concatenation() {
    let src = r#"
fn greet(name: String) -> String {
    return "Hello, " + name;
}

fn main() {
    let punctuation = "!";
    println(greet("Willow") + punctuation);
    println("a" + "b" + "c");
}
"#;
    let (out, ok) = compile_and_run(src);
    assert!(ok, "compilation failed");
    assert_eq!(out, "Hello, Willow!\nabc\n");
}

#[test]
fn test_string_concatenation_rejects_non_string_rhs() {
    assert_compile_error_contains(
        r#"
fn main() {
    println("count: " + 3);
}
"#,
        &[
            "error[E0202]",
            "cannot concatenate `String` with `i64`",
            ".toString()",
        ],
    );
}

#[test]
fn test_print_no_newline() {
    let (out, ok) = compile_and_run("fn main() { print(1); print(2); println(3); }");
    assert!(ok, "compilation failed");
    assert_eq!(out.trim(), "123");
}

#[test]
fn test_println_bool() {
    let (out, ok) = compile_and_run("fn main() { println(true); println(false); }");
    assert!(ok, "compilation failed");
    assert_eq!(out, "true\nfalse\n");
}

#[test]
fn test_println_f64() {
    let (out, ok) = compile_and_run("fn main() { println(2.5); println(-0.5); }");
    assert!(ok, "compilation failed");
    assert_eq!(out, "2.5\n-0.5\n");
}

#[test]
fn test_print_expression_results() {
    let src = r#"
fn main() {
    print(1 + 2);
    print(3 * 4);
    println(5 == 5);
}
"#;
    let (out, ok) = compile_and_run(src);
    assert!(ok, "compilation failed");
    assert_eq!(out, "312true\n");
}

#[test]
fn test_comments_are_ignored() {
    let src = r#"
fn main() {
    // Comments can sit on their own line.
    println(1); // And after statements.
}
"#;
    let (out, ok) = compile_and_run(src);
    assert!(ok, "compilation failed");
    assert_eq!(out.trim(), "1");
}
