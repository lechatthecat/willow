use super::*;

#[test]
fn test_map_confined_scalar_example() {
    let source = include_str!("../../../example/map_confined.wi");
    for run in [
        compile_and_run,
        compile_and_run_release,
        compile_and_run_gc_stress_all,
    ] {
        let (out, ok) = run(source);
        assert!(ok, "{out}");
        assert_eq!(out, "false\n128\n99\n2.5\ntrue\n");
    }
}

#[test]
fn test_map_scalar_aliases_through_distinct_mutexes_and_original() {
    let source = include_str!("../../../example/map_scalar_aliases.wi");
    for workers in ["1", "4"] {
        let (out, ok) = compile_and_run_with_env(source, &[("WILLOW_WORKERS", workers)]);
        assert!(ok, "workers={workers}: {out}");
        assert_eq!(out, "8192\ntrue\n12288\ntrue\n", "workers={workers}");
    }
    let (out, ok) = compile_and_run_gc_stress_all(source);
    assert!(ok, "{out}");
    assert_eq!(out, "8192\ntrue\n12288\ntrue\n");
}

#[test]
fn test_map_scalar_storage_example() {
    let source = include_str!("../../../example/map_scalar_storage.wi");
    for run in [
        compile_and_run,
        compile_and_run_release,
        compile_and_run_gc_stress_all,
    ] {
        let (out, ok) = run(source);
        assert!(ok, "{out}");
        assert_eq!(
            out,
            "-9\n42\n0\n-1\n2\n{0: 0, 7: 42}\ntrue\nfalse\n1\n-2.5\n"
        );
    }
}

#[test]
fn test_map_float_key_semantics_example() {
    let (out, ok) = compile_and_run(include_str!("../../../example/map_float_keys.wi"));
    assert!(ok, "compilation failed");
    assert_eq!(
        out,
        "true\n1\n20\n20\n{0: 20}\ntrue\nfalse\nNaN cannot be used as a Map key\n1\n"
    );
}

#[test]
fn test_map_float_nan_faults_propagate_and_recover() {
    let mut source = String::from("import std::collections::Map;\n");
    let operations = [
        ("Map<f64, i64>", "keys", "keys.insert(nan, 99)"),
        ("Map<f64, i64>", "keys", "keys.get(nan)"),
        ("Map<f64, i64>", "keys", "keys.remove(nan)"),
        ("Map<f64, i64>", "keys", "keys.contains(nan)"),
        ("FrozenMap<f64, i64>", "keys.freeze()", "keys.get(nan)"),
        ("FrozenMap<f64, i64>", "keys.freeze()", "keys.contains(nan)"),
        ("Map<f64, String>", "strings", "keys.get(nan)"),
        ("Map<f64, String>", "strings", "keys.remove(nan)"),
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

#[test]
fn test_map_iteration_snapshot_example() {
    for run in [
        compile_and_run,
        compile_and_run_release,
        compile_and_run_gc_stress_all,
    ] {
        let (out, ok) = run(include_str!("../../../example/map_iteration.wi"));
        assert!(ok, "{out}");
        assert_eq!(out, "a\n1\nb\n2\n1\nc\n3\n3\n3\n-1\n");
    }
}

#[test]
fn test_map_iteration_value_representations_and_control_flow() {
    let source = r#"
import std::collections::Map;
import std::collections::Array;
fn total(m: Map<i64, i64>) -> i64 {
    let mut n = 0;
    for e in m { let (k, v) = e; if k == 1 { continue; } n += v; if k == 3 { break; } }
    return n;
}
fn main() {
    let m: Map<i64, i64> = Map::new();
    println(m.keys().len()); println(m.values().len()); println(m.entries().len());
    for e in m { println(999); }
    println(match m.remove(9) { Some(v) => v, None => -1 });
    m.insert(3, 30); m.insert(-1, -10); m.insert(1, 10);
    m.insert(2, 20); m.insert(1, 11);
    let keys = m.keys(); let values = m.values(); let entries = m.entries();
    m.remove(-1); m.insert(0, 0); m.insert(1, 111);
    for k in keys { println(k); }
    for v in values { println(v); }
    for e in entries { let (k, v) = e; println(k + v); }
    println(total(m));
    let f: Map<f64, bool> = Map::new();
    f.insert(1.5, true); f.insert(-2.0, false); f.insert(-0.0, true); f.insert(0.0, false);
    for e in f { let (k, v) = e; println(k); println(v); }
    println(match f.remove(-0.0) { Some(v) => v, None => true }); println(f.contains(0.0));
    let b: Map<bool, String> = Map::new(); b.insert(true, "yes"); b.insert(false, "no");
    let bs = b.entries(); b.remove(false); b.remove(true);
    for e in bs { let (k, v) = e; println(k); println(v); }
    let strings: Map<String, String> = Map::new();
    strings.insert("z", "last" + "!"); strings.insert("a", "first" + "!");
    let snap = strings.entries(); strings.remove("a"); strings.remove("z");
    for e in snap { let (k, v) = e; println(k + v); }
    let opts: Map<i64, Option<i64>> = Map::new(); opts.insert(1, Some(42)); opts.insert(2, None);
    let ov = opts.values(); ov[0] = Some(99);
    for e in opts { let (k, v) = e; println(match v { Some(n) => n, None => -1 }); }
    println(match opts.remove(1) { Some(v) => match v { Some(n) => n, None => -2 }, None => -3 });
    let nested: Map<i64, Array<i64>> = Map::new(); nested.insert(1, [7, 8]);
    for e in nested { let (k, v) = e; nested.remove(k); println(v[1]); }
    for outer in m { for inner in m { let (k, v) = inner; if k == 0 { println(v); } } }
}
"#;
    let expected = "0\n0\n0\n-1\n-1\n1\n2\n3\n-10\n11\n20\n30\n-11\n12\n22\n33\n50\n-2\nfalse\n0\nfalse\n1.5\ntrue\nfalse\nfalse\nfalse\nno\ntrue\nyes\nafirst!\nzlast!\n42\n-1\n42\n8\n0\n0\n0\n0\n";
    for run in [
        compile_and_run,
        compile_and_run_release,
        compile_and_run_gc_stress_all,
    ] {
        let (out, ok) = run(source);
        assert!(ok, "{out}");
        assert_eq!(out, expected);
    }
}

#[test]
fn test_map_iteration_async_snapshot_and_return() {
    let source = r#"
import std::collections::Map;
async fn main() {
    let m: Map<i64, String> = Map::new(); m.insert(2, "two"); m.insert(1, "one");
    for entry in m { let (k, v) = entry; m.remove(k); await sleep(1); println(v); }
    println(m.len());
}
"#;
    let (out, ok) = compile_and_run_gc_stress_all(source);
    assert!(ok, "{out}");
    assert_eq!(out, "one\ntwo\n0\n");
}

#[test]
fn test_map_iteration_rejects_invalid_arguments() {
    for call in [
        "m.remove()",
        "m.remove(1, 2)",
        "m.remove(true)",
        "m.keys(1)",
        "m.values(1)",
        "m.entries(1)",
        "m.freeze().remove(1)",
    ] {
        assert_compile_error_contains(
            &format!(
                "import std::collections::Map; fn main() {{ let m: Map<i64, i64> = Map::new(); {call}; }}"
            ),
            &["error[E0201]"],
        );
    }
}
