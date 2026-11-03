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

#[test]
fn array_join_semantics() {
    // Twenty independent semantic perspectives, plus native debug/release/GC paths.
    let cases = [
        r#"empty.join(",") == """#,
        r#"empty.join("") == """#,
        r#"["a"].join(",") == "a""#,
        r#"[""].join("日") == """#,
        r#"["a", "b"].join(",") == "a,b""#,
        r#"["a", "b", "c"].join("::") == "a::b::c""#,
        r#"["a", "b"].join("") == "ab""#,
        r#"["", ""].join(",") == ",""#,
        r#"["", "a"].join(",") == ",a""#,
        r#"["a", ""].join(",") == "a,""#,
        r#"["a", "", "b"].join(",") == "a,,b""#,
        r#"["日本", "語"].join("") == "日本語""#,
        r#"["a", "b"].join("😀") == "a😀b""#,
        r#"["é", "😀"].join("日") == "é日😀""#,
        r#"["é", "é"].join("") == "éé""#,
        r#"["a,b", "c"].join(",") == "a,b,c""#,
        r#""a,,b,".split(",").join(",") == "a,,b,""#,
        r#""😀é".split("").join("") == "😀é""#,
        r#"["b", "a", "b"].join("-") == "b-a-b""#,
        r#"["a", "b"].join("日".repeat(100)).len() == 302"#,
    ];
    let body: String = cases
        .iter()
        .map(|expr| format!("println({expr});\n"))
        .collect();
    let source = format!(
        "import std::collections::Array; fn main() {{ let empty: Array<String> = []; {body} }}"
    );
    for run in [
        compile_and_run,
        compile_and_run_release,
        compile_and_run_gc_stress_all,
    ] {
        let (out, ok) = run(&source);
        assert!(ok, "{out}");
        assert_eq!(out, "true\n".repeat(cases.len()));
    }
}

#[test]
fn array_join_invalid_arguments() {
    for expr in [
        r#"["a"].join()"#,
        r#"["a"].join(",", ".")"#,
        r#"["a"].join(1)"#,
        r#"[1].join(",")"#,
        r#"[true].join(",")"#,
        r#"[["a"]].join(",")"#,
    ] {
        let source = format!("fn main() {{ println({expr}); }}");
        assert!(compile_error_stderr(&source).contains("E0201"), "{source}");
    }
    assert!(
        compile_error_stderr(r#"fn main() { let s = ","; ["a"].join(&s); }"#).contains("E1703")
    );
}

#[test]
fn array_join_example_and_dynamic_gc() {
    for run in [
        compile_and_run,
        compile_and_run_release,
        compile_and_run_gc_stress_all,
    ] {
        let (out, ok) = run(include_str!("../../../example/array_join.wi"));
        assert!(ok, "{out}");
        assert_eq!(out, "日本 / Willow / 😀\n119999\ntrue\n");
        let (out, ok) = run(r#"
import std::collections::Array;
fn assemble(parts: Array<String>, sep: String) -> String { return parts.join(sep); }
async fn process() -> String {
    let parts = ["日".repeat(2), "😀".repeat(2)];
    let sep = "-".repeat(2);
    await sleep(1);
    gc_collect();
    let result = assemble(parts, sep);
    parts.push("changed");
    parts[0] = "changed";
    gc_collect();
    return result;
}
async fn main() { println(await process()); }
"#);
        assert!(ok, "{out}");
        assert_eq!(out, "日日--😀😀\n");
    }
}

#[test]
fn array_join_generated_calls_scale_with_callsites() {
    for count in [1, 8, 32] {
        for bytes in [1, 1024] {
            let source = format!(
                "fn main() {{ let parts = [\"{}\", \"b\"]; {} }}",
                "a".repeat(bytes),
                "parts.join(\",\");".repeat(count)
            );
            let symbols = compile_and_collect_relocation_targets_all(&source, &[]);
            assert_eq!(
                symbols
                    .iter()
                    .filter(|s| s.contains("Array") && s.ends_with(".join"))
                    .count(),
                count,
                "{symbols:?}"
            );
            // Joining shares one linear string assembly operation, independent
            // of user callsite count or input bytes. Unused source methods such
            // as `toString` are not instantiated, so they add no second one.
            assert_eq!(
                symbols
                    .iter()
                    .filter(|s| *s == "willow_string_concat_array")
                    .count(),
                1,
                "{symbols:?}"
            );
            assert!(
                !symbols.iter().any(|s| s.ends_with(".toString")),
                "{symbols:?}"
            );
        }
    }
}

#[test]
fn string_find_sentinel_example() {
    for run in [
        compile_and_run,
        compile_and_run_release,
        compile_and_run_gc_stress,
    ] {
        let (out, ok) = run(include_str!("../../../example/string_find.wi"));
        assert!(ok, "{out}");
        assert_eq!(out, "本\ntrue\ntrue\n0\n");
    }
}

#[test]
fn string_find_sentinel_diagnostic_is_rendered() {
    let stderr =
        compile_error_stderr(r#"fn main() { let index = "abc".find("b"); index.unwrap_or(0); }"#);
    assert!(stderr.contains("E0201"), "{stderr}");
    assert!(stderr.contains("String.find(needle)"), "{stderr}");
    assert!(stderr.contains("-1 means not found"), "{stderr}");
    assert!(stderr.contains("index >= 0"), "{stderr}");
}

#[test]
fn string_bytes_literals_and_access() {
    // Perspectives 1–24: boundaries, UTF-8 widths, NUL, escapes, composition.
    let cases = [
        r#""abc".byte_at(0) == b'a'"#,
        r#""abc".byte_at(2) == b'c'"#,
        r#""é".byte_at(0) == b'\xc3'"#,
        r#""é".byte_at(1) == b'\xa9'"#,
        r#""水".byte_at(0) == b'\xe6'"#,
        r#""水".byte_at(2) == b'\xb4'"#,
        r#""🌲".byte_at(0) == b'\xf0'"#,
        r#""🌲".byte_at(3) == b'\xb2'"#,
        r#""a\0b".byte_at(1) == b'\0'"#,
        r#""a\0b".byte_at(2) == b'b'"#,
        r#"b'\n' == 10"#,
        r#"b'\r' == 13"#,
        r#"b'\t' == 9"#,
        r#"b'\e' == 27"#,
        r#"b'\\' == 92"#,
        r#"b'\'' == 39"#,
        r#"b'\"' == 34"#,
        r#"b'"' == 34"#,
        r#"b' ' == 32"#,
        r#"b'\xFF' == 255"#,
        r#"b'9' - b'0' == 9"#,
        r#"("a" + "b").byte_at(1) == b'b'"#,
        r#"" x ".trim().byte_at(0) == b'x'"#,
        r#""é".byte_at(1) == 204"#,
    ];
    let body: String = cases
        .iter()
        .map(|expr| format!("println({expr});\n"))
        .collect();
    let source = format!("fn main() {{ {body} }}");
    for run in [
        compile_and_run,
        compile_and_run_release,
        compile_and_run_gc_stress_all,
    ] {
        let (out, ok) = run(&source);
        assert!(ok, "{out}");
        assert_eq!(out, "true\n".repeat(cases.len()));
    }
}

#[test]
fn string_bytes_invalid_arguments() {
    for expr in [
        r#""x".byte_at()"#,
        r#""x".byte_at(0, 1)"#,
        r#""x".byte_at(true)"#,
        r#""x".byte_at("0")"#,
        r#""x".byte_at(0.0)"#,
        "1.byte_at(0)",
    ] {
        let source = format!("fn main() {{ println({expr}); }}");
        assert!(compile_error_stderr(&source).contains("E0201"), "{source}");
    }
    assert!(compile_error_stderr(r#"fn main() { let i = 0; "x".byte_at(&i); }"#).contains("E1703"));
}

#[test]
fn string_bytes_recover_bounds() {
    let cases = [
        r#""".byte_at(0)"#,
        r#""a".byte_at(-1)"#,
        r#""a".byte_at(1)"#,
        r#""a".byte_at(9223372036854775807)"#,
        r#""a".byte_at(-9223372036854775807 - 1)"#,
    ];
    let body: String = cases
        .iter()
        .map(|expr| {
            format!(
                r#"
        if true {{
            defer match recover() {{
                Some(info) => println(info.message.contains("String.byte_at requires")),
                None => {{}}
            }}
            println(1 + {expr});
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
        compile_and_run_gc_stress_all,
    ] {
        let (out, ok) = run(&source);
        assert!(ok, "{out}");
        assert_eq!(out, "true\n".repeat(cases.len()));
    }
}

#[test]
fn string_bytes_example_and_dynamic_gc() {
    for run in [
        compile_and_run,
        compile_and_run_release,
        compile_and_run_gc_stress_all,
    ] {
        let (out, ok) = run(include_str!("../../../example/string_bytes.wi"));
        assert!(ok, "{out}");
        assert_eq!(out, "123\n42\ntrue\n");
        let (out, ok) = run(r#"
fn receiver() -> String { println("receiver"); return "é".repeat(2); }
fn index() -> i64 { gc_collect(); println("index"); return 3; }
async fn read(s: String) -> i64 {
    await sleep(1);
    gc_collect();
    return s.byte_at(s.len() - 1);
}
async fn main() {
    println(receiver().byte_at(index()) == b'\xa9');
    println(await read("🌲".repeat(32)) == b'\xb2');
    let empty = "";
    let mut sum = 0;
    for i in 0..empty.len() { sum += empty.byte_at(i); }
    println(sum);
    let values = [b'a', b'b'];
    println(values[1] == b'b');
    match b'a' { 97 => println("match"), _ => println("bad") }
}
"#);
        assert!(ok, "{out}");
        assert_eq!(out, "receiver\nindex\ntrue\ntrue\n0\ntrue\nmatch\n");
    }
}

#[test]
fn string_bytes_generated_calls_scale_with_callsites() {
    for count in [1, 8, 32] {
        for bytes in [1, 1024] {
            let source = format!(
                "fn main() {{ let s = \"{}\"; {} }}",
                "a".repeat(bytes),
                "s.byte_at(0);".repeat(count)
            );
            let symbols = compile_and_collect_relocation_targets_all(&source, &[]);
            assert_eq!(
                symbols
                    .iter()
                    .filter(|s| *s == "willow_string_byte_at")
                    .count(),
                count
            );
        }
    }
}

#[test]
fn string_transforms_semantics() {
    let cases = [
        r#""abc".ends_with("bc")"#,
        r#"!"abc".ends_with("ab")"#,
        r#"!"a".ends_with("aa")"#,
        r#""".ends_with("")"#,
        r#""日本😀".ends_with("😀")"#,
        r#""aaaaa".replace("aa", "b") == "bba""#,
        r#""aaa".replace("a", "aa") == "aaaaaa""#,
        r#""abc".replace("z", "x") == "abc""#,
        r#""abc".replace("abc", "") == """#,
        r#""".replace("", "x") == "x""#,
        r#""é😀".replace("", "|") == "|é|😀|""#,
        r#""日本日本".replace("日本", "語") == "語語""#,
        r#""a.b".replace(".", "-") == "a-b""#,
        r#""".replace("x", "y") == """#,
        r#""  a b  ".trim_start() == "a b  ""#,
        r#""  a b  ".trim_end() == "  a b""#,
        r#""　日本　".trim_start() == "日本　""#,
        r#""　日本　".trim_end() == "　日本""#,
        r#""   ".trim_start() == """#,
        r#""   ".trim_end() == """#,
        r#""".trim_start().trim_end() == """#,
        r#""AbZ 09 ÉΣ日本".to_lower() == "abz 09 ÉΣ日本""#,
        r#""aBz 09 éß日本".to_upper() == "ABZ 09 éß日本""#,
        r#""".to_upper().to_lower() == """#,
        r#"" ABC ".trim_start().trim_end().to_lower().replace("b", "z").ends_with("zc")"#,
        r#""a\0b".replace("\0", "X") == "aXb""#,
        r#""A\0z".to_lower() == "a\0z""#,
        r#""a\0z".to_upper() == "A\0Z""#,
        r#""a\0".ends_with("\0")"#,
    ];
    let body: String = cases
        .iter()
        .map(|expr| format!("println({expr});\n"))
        .collect();
    for run in [
        compile_and_run,
        compile_and_run_release,
        compile_and_run_gc_stress_all,
    ] {
        let (out, ok) = run(&format!("fn main() {{ {body} }}"));
        assert!(ok, "{out}");
        assert_eq!(out, "true\n".repeat(cases.len()));
    }
}

#[test]
fn string_transforms_invalid_arguments() {
    for expr in [
        r#""x".ends_with()"#,
        r#""x".ends_with(1)"#,
        r#""x".ends_with("x", "y")"#,
        r#""x".replace("x")"#,
        r#""x".replace(1, "y")"#,
        r#""x".replace("x", 1)"#,
        r#""x".replace("x", "y", "z")"#,
        r#""x".trim_start(1)"#,
        r#""x".trim_end(1)"#,
        r#""x".to_lower(1)"#,
        r#""x".to_upper(1)"#,
    ] {
        assert!(
            compile_error_stderr(&format!("fn main() {{ {expr}; }}")).contains("E0201"),
            "{expr}"
        );
    }
}

#[test]
fn string_transforms_example_and_dynamic_gc() {
    for run in [
        compile_and_run,
        compile_and_run_release,
        compile_and_run_gc_stress_all,
    ] {
        let (out, ok) = run(include_str!("../../../example/string_transforms.wi"));
        assert!(ok, "{out}");
        assert_eq!(
            out,
            "true\ntwo two\ntrue\ntrue\nwillow É日本\nWILLOW é日本\n|é|😀|\n"
        );
        let (out, ok) = run(r#"
fn pattern() -> String { gc_collect(); return "Ab".repeat(1); }
fn replacement() -> String { gc_collect(); return "éZ".repeat(1); }
async fn transform(s: String) -> String {
    await sleep(1);
    return s.replace(pattern(), replacement()).trim_start().trim_end().to_lower().to_upper();
}
async fn main() {
    let original = " Ab ".repeat(1024);
    let result = await transform(original);
    gc_collect();
    println(result.ends_with("éZ"));
    println(original.ends_with("Ab "));
    println(result.len());
}
"#);
        assert!(ok, "{out}");
        assert_eq!(out, "true\ntrue\n5118\n");
    }
}

#[test]
fn string_transforms_generated_calls_scale_with_callsites() {
    for count in [1, 8, 32] {
        for bytes in [1, 1024] {
            for (expr, symbol) in [
                ("s.ends_with(s)", "ends_with"),
                ("s.replace(s, s)", "replace"),
                ("s.trim_start()", "trim_start"),
                ("s.trim_end()", "trim_end"),
                ("s.to_lower()", "to_lower"),
                ("s.to_upper()", "to_upper"),
            ] {
                let source = format!(
                    "fn main() {{ let s = \"{}\"; {} }}",
                    "x".repeat(bytes),
                    format!("{expr};").repeat(count)
                );
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
fn string_bytes_recover_keeps_live_values_across_panic_gc() {
    let source = r#"
fn keep(prefix: String, suffix: String) {
    let saved = prefix + suffix;
    if true {
        defer match recover() {
            Some(info) => {
                gc_collect();
                println(saved);
                println(info.message.contains("String.byte_at requires"));
            },
            None => {}
        }
        "".byte_at(-1);
    }
    println(saved);
}
fn main() { keep("still-", "alive"); }
"#;
    for run in [
        compile_and_run,
        compile_and_run_release,
        compile_and_run_gc_stress_all,
    ] {
        let (out, ok) = run(source);
        assert!(ok, "{out}");
        assert_eq!(out, "still-alive\ntrue\nstill-alive\n");
    }
}
