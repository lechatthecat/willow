use super::support::*;

#[test]
fn contextual_constructors_and_match_arms_execute() {
    let (out, ok) = compile_file_and_run("example/buildgraph_regressions.wi");
    assert!(ok, "{out}");
    assert_eq!(out, "1\n0\n0\n0\n0\n0\n2\nsafe\nsafe\n3\n");
}

#[test]
fn contextual_match_perspectives() {
    let mut source = String::from("import std::collections::Array; fn main() {\n");
    let cases = [
        ("true => [1], false => []", "1"),
        ("true => [], false => [1]", "0"),
        ("false => [], true => [1]", "1"),
        ("false => [1], true => []", "0"),
        ("true => [1.5], false => []", "1"),
        ("true => [], false => [1.5]", "0"),
        ("true => [true], false => []", "1"),
        ("true => [], false => [true]", "0"),
        ("true => [\"hi\"], false => []", "1"),
        ("true => [], false => [\"hi\"]", "0"),
        ("true => [[1]], false => []", "1"),
        ("true => [], false => [[1]]", "0"),
        ("true => [[1]], false => [[]]", "1"),
        ("true => [[]], false => [[1]]", "1"),
        (
            "true => (match false { true => [], false => [] }), false => [1]",
            "0",
        ),
        (
            "true => [1], false => (match false { true => [], false => [] })",
            "1",
        ),
    ];
    let mut expected = String::new();
    for (i, (arms, output)) in cases.into_iter().enumerate() {
        source.push_str(&format!(
            "let xs{i} = match true {{ {arms} }}; println(xs{i}.len());\n"
        ));
        expected.push_str(output);
        expected.push('\n');
    }
    source.push('}');
    let (out, ok) = compile_and_run(&source);
    assert!(ok, "{out}");
    assert_eq!(out, expected);
}

#[test]
fn contextual_constructor_twenty_perspectives_and_gc() {
    let mut source = String::from("import std::collections::{Array, Map};\n");
    let mut body = String::from("fn main() {\n");
    let pairs = [
        ("i64", "i64", "1", "2"),
        ("String", "i64", "\"key\"", "2"),
        ("i64", "String", "1", "\"value\""),
        ("String", "Array<String>", "\"key\"", "[\"value\"]"),
    ];
    for (i, (key, value, k, v)) in pairs.into_iter().enumerate() {
        let ty = format!("Map<{key}, {value}>");
        source.push_str(&format!("class H{i} {{ pub m: {ty}; }} class E{i} {{ pub m: {ty}; pub init(self, m: {ty}) {{ self.m = m; }} }} fn empty{i}() -> {ty} {{ return Map::new(); }} fn use{i}(m: {ty}) -> i64 {{ m.insert({k}, {v}); return m.len(); }}\n"));
        // Memberwise argument, explicit argument, field assignment, return,
        // and ordinary call argument, each for scalar/reference combinations.
        body.push_str(&format!("let h{i} = new H{i}(Map::new()); println(use{i}(h{i}.m)); let e{i} = new E{i}(Map::new()); println(use{i}(e{i}.m)); h{i}.m = Map::new(); println(use{i}(h{i}.m)); println(use{i}(empty{i}())); println(use{i}(Map::new()));\n"));
        body.push_str(&format!(
            "let typed{i} = new H{i}(Map<{key}, {value}>::new()); println(use{i}(typed{i}.m));\n"
        ));
    }
    body.push('}');
    source.push_str(&body);
    let (out, ok) = compile_and_run_with_env(&source, &[("WILLOW_GC_STRESS", "alloc")]);
    assert!(ok, "{out}");
    assert_eq!(out, "1\n".repeat(24));
}

#[test]
fn contextual_channel_constructor_preserves_gc_element_kind() {
    let source = r#"
class Inbox { pub messages: Channel<String>; }
async fn main() {
    let inbox = new Inbox(Channel::with_capacity(1));
    inbox.messages.send("retained message");
    let mut i = 0;
    while i < 100 { let garbage = "garbage-" + i.toString(); i = i + 1; }
    let message = inbox.messages.recv();
    println(message);
    let empty = new Inbox(Channel::new());
    empty.messages.send("unbounded message");
    let end = empty.messages.recv();
    println(end);
}
"#;
    let (out, ok) = compile_and_run_with_env(source, &[("WILLOW_GC_STRESS", "alloc")]);
    assert!(ok, "{out}");
    assert_eq!(out, "retained message\nunbounded message\n");
}

#[test]
fn contextual_constructor_generated_calls_scale_with_sites() {
    for n in [4, 16, 64] {
        let mut source = String::from(
            "import std::collections::Map; class H { pub m: Map<String, i64>; } fn main() {",
        );
        for i in 0..n {
            source.push_str(&format!(
                "let h{i} = new H(Map::new()); println(h{i}.m.len());"
            ));
        }
        source.push('}');
        let relocations = compile_and_collect_relocation_targets_all(&source, &[]);
        let count = relocations
            .iter()
            .filter(|name| name.ends_with("willow_map_new"))
            .count();
        assert_eq!(count, n, "{n}: {relocations:?}");
    }
}

#[test]
fn contextual_constructors_and_matches_in_imported_module() {
    let files = [
        (
            "graph.wi",
            "module graph; import std::collections::{Array, Map}; pub class Registry { pub names: Map<String, i64>; } pub fn empty() -> Registry { return new Registry(Map::new()); } pub fn values() -> Array<i64> { let xs = match false { true => [1], false => [] }; return xs; }",
        ),
        (
            "main.wi",
            "import graph::{empty, values}; fn main() { let r = empty(); r.names.insert(\"one\", 1); println(r.names.len()); println(values().len()); }",
        ),
    ];
    let (out, ok) = compile_temp_project_and_run(&files, "main.wi");
    assert!(ok, "{out}");
    assert_eq!(out, "1\n0\n");
}
