use super::support::*;

fn literal(text: &str) -> String {
    serde_json::to_string(text).unwrap()
}

#[test]
fn json_values_escapes_and_number_boundaries() {
    // Independent acceptance perspectives, batched into one executable.
    let valid = [
        "null",
        "true",
        "false",
        "0",
        "-0",
        "9223372036854775807",
        "-9223372036854775808",
        "18446744073709551616",
        "1e999999999",
        "-1.25E-999999",
        "[]",
        "{}",
        "[1,true,null]",
        r#"{"x":[{"y":2}]}"#,
        r#""""#,
        r#""日本😀""#,
        r#""\\\"\/\b\f\n\r\t""#,
        r#""\u0000\u007f\u0080\u07ff\u0800\uffff""#,
        r#""\ud83d\ude00""#,
        r#"{"a":1,"a":2}"#,
        " \t\r\n [ 1 , 2 ] \r\n",
        r#"{"\u0061":"é"}"#,
    ];
    let mut source = String::from("import std::json; fn main() {");
    for input in valid {
        source.push_str(&format!(
            "if true {{ let value = json::parse({}).unwrap(); let text = json::serialize(value).unwrap(); \
             if json::serialize(json::parse(text).unwrap()).unwrap() != text {{ panic(\"roundtrip\"); }} println(text); }}\n",
            literal(input)
        ));
    }
    source.push('}');
    for run in [
        compile_and_run,
        compile_and_run_release,
        compile_and_run_gc_stress,
    ] {
        let (out, ok) = run(&source);
        assert!(ok, "{out}");
        let lines: Vec<_> = out.lines().collect();
        assert_eq!(lines.len(), valid.len());
        for (input, output) in valid.iter().zip(lines) {
            match serde_json::from_str::<serde_json::Value>(input) {
                Ok(expected) => assert_eq!(
                    serde_json::from_str::<serde_json::Value>(output).unwrap(),
                    expected,
                    "{input}"
                ),
                // Deliberately unbounded exponents exceed serde_json's f64
                // domain; JSON preserves their exact source spelling.
                Err(_) => assert_eq!(output, *input),
            }
        }
    }
}

#[test]
fn json_invalid_inputs_have_positions() {
    let invalid = [
        "",
        " ",
        "+1",
        "01",
        "-01",
        "-",
        "1.",
        "1e",
        "1e+",
        "NaN",
        "Infinity",
        "truex",
        "nul",
        "[",
        "{",
        "[1,]",
        "{\"a\":1,}",
        "{\"a\" 1}",
        "{1:2}",
        "[1 2]",
        "null false",
        "\"",
        "\"x\n\"",
        "\"\\q\"",
        "\"\\u12\"",
        "\"\\ud800\"",
        "\"\\udc00\"",
        "\"\\ud800\\u0041\"",
        "[}",
    ];
    let mut source = String::from("import std::json; fn main() {");
    for input in invalid {
        source.push_str(&format!(
            "if true {{ let result = json::parse({}); if let Err(e) = result {{ \
                if e.position < 0 || e.position > {} || e.message.len() == 0 {{ panic(\"position\"); }} \
             }} else {{ panic(\"accepted invalid JSON\"); }} }}\n",
            literal(input), input.len()
        ));
    }
    source.push_str(
        "if let Err(e) = json::parse(\"[1,]\") { if e.position != 3 { panic(\"offset\"); } } \
         println(\"ok\"); }",
    );
    let (out, ok) = compile_and_run(&source);
    assert!(ok, "{out}");
    assert_eq!(out, "ok\n");
}

#[test]
fn json_native_values_lookup_task_purity_and_gc() {
    let source = r#"
import std::json;
import std::json::{Value, Member};
import std::collections::Array;
@effects(no_wait) fn transform(s: String) -> String {
    return json::serialize(json::parse(s).unwrap()).unwrap();
}
async fn work() -> String {
    await yield();
    return transform("{\"a\":\"\\ud83d\\ude00\"}");
}
async fn main() {
    let data = json::parse("{\"a\":1,\"a\":2,\"b\":[\"é\",null]}").unwrap();
    println(json::serialize(json::get(data, "a").unwrap()).unwrap());
    println(json::serialize(json::at(json::get(data, "b").unwrap(), 0).unwrap()).unwrap());
    println(json::get(data, "missing").is_none());
    println(json::at(data, 0).is_none());
    println(json::get(Value::Null, "a").is_none());
    let values: Array<Value> = [Value::Bool(true), Value::Number("-0")];
    let value = Value::Object([new Member("x", Value::Array(values))]);
    values.push(Value::Null);
    println(json::serialize(value).unwrap());
    println(await work());
}
"#;
    let (out, ok) = compile_and_run_gc_stress(source);
    assert!(ok, "{out}");
    assert_eq!(
        out,
        "2\n\"é\"\ntrue\ntrue\ntrue\n{\"x\":[true,-0,null]}\n{\"a\":\"😀\"}\n"
    );
}

#[test]
fn json_depth_cycles_and_invalid_constructed_numbers() {
    let source = r#"
import std::json;
import std::json::Value;
import std::collections::Array;
fn main() {
    let deep = "[".repeat(256) + "0" + "]".repeat(256);
    println(json::serialize(json::parse(deep).unwrap()).unwrap() == deep);
    println(json::parse("[".repeat(20000)).is_err());
    println(json::parse("{\"x\":".repeat(257) + "0" + "}".repeat(257)).is_err());
    let cycle: Array<Value> = [];
    let root = Value::Array(cycle);
    cycle.push(root);
    println(json::serialize(root).is_err());
    for number in ["", "01", "-01", "nan", "inf", " 1", "1 ", "+1", "1e", "1."] {
        if !json::serialize(Value::Number(number)).is_err() { panic("invalid number"); }
    }
    let shared = Value::Array([Value::Null]);
    println(json::serialize(Value::Array([shared, shared])).unwrap());
}
"#;
    let (out, ok) = compile_and_run(source);
    assert!(ok, "{out}");
    assert_eq!(out, "true\ntrue\ntrue\ntrue\n[[null],[null]]\n");
}

#[test]
fn json_parser_operation_counts_are_linear() {
    // Count observations in the actual parser, not a separate model.
    let mut source = include_str!("../../std/json.wi")
        .replace(
            "pub failure: String;",
            "pub failure: String; pub steps: i64;",
        )
        .replace(
            "pub fn peek() -> i64 {",
            "pub fn peek() -> i64 { self.steps = self.steps + 1;",
        )
        .replace(
            "new Parser(input, 0, \"\")",
            "new Parser(input, 0, \"\", 0)",
        )
        .replace("new Parser(n, 0, \"\")", "new Parser(n, 0, \"\", 0)");
    source.push_str(
        r#"
fn main() {
    for n in [32, 64, 128, 256] {
        for input in [
            "[" + "0,".repeat(n) + "0]",
            "{\"x\":".repeat(n) + "0" + "}".repeat(n),
            "{" + "\"duplicate\":0,".repeat(n) + "\"duplicate\":0}",
            "\"" + "\\u0061".repeat(n) + "\"",
            "\"" + "日本".repeat(n) + "\"",
            "1" + "0".repeat(n)
        ] {
            let parser = new Parser(input, 0, "", 0);
            parser.run().unwrap();
            if parser.steps > input.len() * 20 + 20 { panic("nonlinear work"); }
            println(parser.steps);
        }
    }
}
"#,
    );
    let (out, ok) = compile_and_run(&source);
    assert!(ok, "{out}");
    let counts: Vec<usize> = out.lines().map(|line| line.parse().unwrap()).collect();
    assert_eq!(counts.len(), 24);
    for shape in 0..6 {
        for scale in 1..4 {
            assert!(counts[scale * 6 + shape] <= counts[(scale - 1) * 6 + shape] * 2 + 20);
        }
    }
    eprintln!("JSON parser input observations: {counts:?}");
}

#[test]
fn json_runnable_example() {
    let (out, ok) = compile_and_run(include_str!("../../example/json.wi"));
    assert!(ok, "{out}");
    assert_eq!(
        out,
        "Willow\n{\"name\":\"Willow\",\"count\":18446744073709551615,\"tags\":[true,null]}\n3\n"
    );
}

#[test]
fn json_unicode_decoding_matches_scalars_and_escapes() {
    let source = r#"
import std::json::{parse, serialize, Value};
import std::text;
fn main() {
    let value = parse("\"\\u0000\\u007f\\u0080\\u07ff\\u0800\\uffff\\udbff\\udfff\"").unwrap();
    let expected = text::from_scalar(0) + text::from_scalar(127)
        + text::from_scalar(128) + text::from_scalar(2047)
        + text::from_scalar(2048) + text::from_scalar(65535)
        + text::from_scalar(1114111);
    println(match value { Value::String(s) => s == expected, _ => false });
    let escaped = parse("\"\\\"\\\\\\/\\b\\f\\n\\r\\t\"").unwrap();
    let expected_escaped = "\"\\/" + text::from_scalar(8)
        + text::from_scalar(12) + "\n\r\t";
    println(match escaped { Value::String(s) => s == expected_escaped, _ => false });
    println(serialize(Value::String(expected_escaped)).unwrap());
    println(serialize(parse("-0.001E+999999999999999999999999").unwrap()).unwrap());
}
"#;
    let (out, ok) = compile_and_run_gc_stress(source);
    assert!(ok, "{out}");
    assert_eq!(
        out,
        "true\ntrue\n\"\\\"\\\\/\\u0008\\u000c\\u000a\\u000d\\u0009\"\n-0.001E+999999999999999999999999\n"
    );
}

#[test]
fn json_module_alias_and_transitive_imports() {
    let files = [
        (
            "helper.wi",
            r#"
import std::json as codec;
import std::json::Value as Data;
pub fn read() -> Data { return codec::parse("[42]").unwrap(); }
"#,
        ),
        (
            "main.wi",
            r#"
import helper;
import std::json::{serialize as encode, at as item};
fn main() { println(encode(item(helper::read(), 0).unwrap()).unwrap()); }
"#,
        ),
        // Reserved standard modules must never load a filesystem impostor.
        ("std/json.wi", "this must not be parsed"),
    ];
    let (out, ok) = compile_temp_project_with_env_and_run(&files, "main.wi", &[]);
    assert!(ok, "{out}");
    assert_eq!(out, "42\n");
}

#[test]
fn json_effect_queries_read_embedded_source() {
    let source = temp_path(format!("willow_json_effects_{}.wi", unique_test_id()));
    fs::write(&source, include_str!("../../example/json.wi")).unwrap();
    for selector in ["parse", "serialize"] {
        let output = Command::new(env!("CARGO_BIN_EXE_willow"))
            .args([
                "effects",
                selector,
                "--source",
                &source,
                "--format",
                "json",
                "--all",
                "--explain",
            ])
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        let result: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
        assert_eq!(result["status"], "ok", "{result}");
        let bounds = result["result"]["bounds"]["runtime"].as_u64().unwrap();
        assert_eq!(
            bounds & 6,
            0,
            "JSON must neither block nor suspend: {result}"
        );
        assert_eq!(
            result["result"]["selected"]["location"]["path"],
            "<std>/json.wi"
        );
    }
    let _ = fs::remove_file(source);
}

#[test]
fn json_scalar_encoder_rejects_non_scalars() {
    let source = r#"
import std::text as unicode;
fn attempt(value: i64) {
    defer match recover() {
        Some(info) => println(info.message),
        None => println("unexpected success")
    }
    unicode::from_scalar(value);
}
fn main() {
    for value in [-1, 55296, 56319, 56320, 57343, 1114112, 9223372036854775807] {
        attempt(value);
    }
}
"#;
    let (out, ok) = compile_and_run_gc_stress(source);
    assert!(ok, "{out}");
    assert_eq!(out, "invalid Unicode scalar\n".repeat(7));
}
