use super::super::support::*;

#[test]
fn string_methods_utf8_and_composition() {
    let cases = [
        // Explicit perspectives: byte semantics, ranges, all search boundaries,
        // literal splitting, Unicode scalar splitting, whitespace, and composition.
        r#""日本語".len() == 9"#,
        r#""日本語".substring(0, 6) == "日本""#,
        r#""日本語".slice(6, 9) == "語""#,
        r#""abc".slice(1, 1) == """#,
        r#""".slice(0, 0) == """#,
        r#""😀é".slice(0, 4) == "😀""#,
        r#""abc".substring(0, 3) == "abc""#,
        r#""日本語日本".find("本") == 3"#,
        r#""abc".find("z") == -1"#,
        r#""abc".find("") == 0"#,
        r#""".find("") == 0"#,
        r#""abc".find("abcd") == -1"#,
        r#""ababa".find("aba") == 0"#,
        r#""abc".contains("bc")"#,
        r#"!"abc".contains("bd")"#,
        r#""".contains("")"#,
        r#""日本語".starts_with("日本")"#,
        r#"!"日本語".starts_with("本")"#,
        r#""abc".starts_with("")"#,
        r#"!"a".starts_with("ab")"#,
        r#""a,,b,".split(",").len() == 4"#,
        r#"",a".split(",")[0] == """#,
        r#""a,,b,".split(",")[1] == """#,
        r#""a,,b,".split(",")[3] == """#,
        r#""".split(",").len() == 1"#,
        r#""abc".split("z")[0] == "abc""#,
        r#""a.b".split(".")[1] == "b""#,
        r#""aaaaa".split("aa")[2] == "a""#,
        r#""日本語".split("本")[1] == "語""#,
        r#""😀é".split("").len() == 4"#,
        r#""😀é".split("")[1] == "😀""#,
        r#""".split("").len() == 2"#,
        r#""  a b  ".trim() == "a b""#,
        r#""　日本語　".trim() == "日本語""#,
        r#""   ".trim() == """#,
        r#""".trim() == """#,
        r#""日本".repeat(3) == "日本日本日本""#,
        r#""abc".repeat(0) == """#,
        r#""abc".repeat(1) == "abc""#,
        r#""".repeat(9223372036854775807) == """#,
        r#"(" a" + "b ").trim().slice(0, 1).repeat(2) == "aa""#,
        r#""abc".toString().split("b")[1].contains("c")"#,
    ];
    let body: String = cases
        .iter()
        .map(|expr| format!("println({expr});\n"))
        .collect();
    let source = format!("fn main() {{ {body} }}");
    for run in [
        compile_and_run,
        compile_and_run_release,
        compile_and_run_gc_stress,
    ] {
        let (out, ok) = run(&source);
        assert!(ok, "{out}");
        assert_eq!(out, "true\n".repeat(cases.len()));
    }
}

#[test]
fn string_methods_invalid_arguments() {
    for expr in [
        r#""x".substring(0)"#,
        r#""x".slice(0, 1, 2)"#,
        r#""x".substring("0", 1)"#,
        r#""x".repeat(true)"#,
        r#""x".split(1)"#,
        r#""x".contains(1)"#,
        r#""x".find(1)"#,
        r#""x".starts_with(1)"#,
        r#""x".trim(1)"#,
        r#"1.repeat(2)"#,
    ] {
        let source = format!("fn main() {{ println({expr}); }}");
        assert!(compile_error_stderr(&source).contains("E0201"), "{source}");
    }
    assert!(compile_error_stderr(r#"fn main() { let n = 2; "x".repeat(&n); }"#).contains("E1703"));
}

#[test]
fn string_methods_panics_recover_without_continuing_expression() {
    let cases = [
        r#""abc".slice(-1, 1)"#,
        r#""abc".slice(2, 1)"#,
        r#""abc".slice(0, 4)"#,
        r#""日本".slice(1, 3)"#,
        r#""日本".slice(0, 4)"#,
        r#""abc".repeat(-1)"#,
        r#""ab".repeat(9223372036854775807)"#,
    ];
    let body: String = cases
        .iter()
        .map(|expr| {
            format!(
                r#"
        if true {{
            defer match recover() {{ Some(_) => println("recovered"), None => {{}} }}
            println({expr});
            println("unreachable");
        }}
    "#
            )
        })
        .collect();
    let source = format!("fn main() {{ {body} }}");
    for run in [
        compile_and_run,
        compile_and_run_release,
        compile_and_run_gc_stress,
    ] {
        let (out, ok) = run(&source);
        assert!(ok, "{out}");
        assert_eq!(out, "recovered\n".repeat(cases.len()));
    }
}

#[test]
fn string_methods_example() {
    let (out, ok) = compile_and_run(include_str!("../../../example/string_methods.wi"));
    assert!(ok, "{out}");
    assert_eq!(
        out,
        "18\n日本\n語\n10\ntrue\ntrue\n4\nWillowWillow\n0\n0\n日本語,Willow,,\n"
    );
}

#[test]
fn string_methods_generated_calls_scale_with_callsites() {
    for count in [1, 8, 32] {
        for bytes in [8, 1024] {
            let text = "x".repeat(bytes);
            for (expr, symbol) in [
                ("s.slice(0, 1)", "substring"),
                ("s.split(\"x\")", "split"),
                ("s.contains(\"z\")", "contains"),
                ("s.find(\"z\")", "find"),
                ("s.trim()", "trim"),
                ("s.repeat(2)", "repeat"),
                ("s.starts_with(\"x\")", "starts_with"),
            ] {
                let calls = format!("{expr};").repeat(count);
                let source = format!("fn main() {{ let s = \"{text}\"; {calls} }}");
                let symbols = compile_and_collect_relocation_targets_all(&source, &[]);
                assert_eq!(
                    symbols
                        .iter()
                        .filter(|s| *s == &format!("willow_string_{symbol}"))
                        .count(),
                    count
                );
            }
        }
    }
}

#[test]
fn string_methods_dynamic_values_survive_gc_and_await() {
    let source = r#"
import std::collections::Array;
class Text { pub value: String; pub fn pieces(self) -> Array<String> { return self.value.split(","); } }
fn cut(s: String, start: i64, end: i64) -> String { return s.slice(start, end); }
async fn process(s: String) -> String {
    let parts = new Text(s.trim()).pieces();
    await sleep(1);
    gc_collect();
    return cut(parts[1], 0, 6).repeat(2);
}
async fn main() {
    let mut total = 0;
    for i in 0..32 {
        let input = " prefix," + "日本語,suffix ";
        let result = await process(input);
        gc_collect();
        if result == "日本日本" && result.find("本") == 3 && result.contains("本日") && result.starts_with("日本") {
            total = total + 1;
        }
    }
    println(total);
}
"#;
    for run in [
        compile_and_run,
        compile_and_run_release,
        compile_and_run_gc_stress,
    ] {
        let (out, ok) = run(source);
        assert!(ok, "{out}");
        assert_eq!(out, "32\n");
    }
}
