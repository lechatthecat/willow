use super::support::*;

const EXAMPLE: &str = include_str!("../../example/string_metadata.wi");

#[test]
fn string_metadata_example_debug_release_and_relocation() {
    for (out, ok) in [
        compile_and_run(EXAMPLE),
        compile_and_run_release(EXAMPLE),
        compile_and_run_gc_stress_mode(EXAMPLE, "relocate"),
    ] {
        assert!(ok, "{out}");
        assert_eq!(out, "0\n9\n2\n3\n4\n0\n18\n");
    }
}

#[test]
fn string_metadata_lengths_and_contexts() {
    let source = r#"
fn size(s: String) -> i64 { return s.len(); }
fn source() -> String { println("evaluated"); return "é"; }
fn main() {
    println("".len());
    println("a".len());
    println("abc".len());
    println("é".len());
    println("水".len());
    println("🌲".len());
    println("é".len());
    println("a\0b".len());
    println(size("é水🌲"));
    println(source().len());
    println(("é" + "水").len());
    let n = 12345;
    println(n.toString().len());
    println("  é  ".trim().len());
    println("ab".repeat(8192).len());
    let parts = "é,水".split(",");
    println(parts[0].len());
    println(parts[1].len());
    let mut text = "a";
    let mut i = 0;
    while i < 3 { println(text.len()); text = text + "é"; i = i + 1; }
    defer { println(text.len()); }
}
"#;
    for (out, ok) in [compile_and_run(source), compile_and_run_release(source)] {
        assert!(ok, "{out}");
        assert_eq!(
            out,
            "0\n1\n3\n2\n3\n4\n3\n3\n9\nevaluated\n2\n5\n5\n2\n16384\n2\n3\n1\n3\n5\n7\n"
        );
    }
}

#[test]
fn string_metadata_invalid_substring_boundaries_still_panic() {
    for (start, end) in [(-1, 0), (0, 10), (5, 2), (1, 2), (2, 3), (5, 8), (10, 10)] {
        let source = format!(
            r#"fn main() {{ let s = "é水🌲"; println(s.len()); println(s.substring({start}, {end})); }}"#
        );
        let (out, ok) = compile_and_run_check_exit(&source);
        assert!(!ok, "{start}..{end}: {out}");
        assert!(out.contains("String.substring requires"), "{out}");
    }
}

#[test]
fn string_metadata_no_length_calls_at_increasing_site_counts() {
    for release in [false, true] {
        for count in [1, 8, 32] {
            let mut source = String::new();
            for i in 0..count {
                source.push_str(&format!(
                    "fn size{i}(s: String) -> i64 {{ return s.len(); }}\n"
                ));
            }
            source.push_str("fn main() { let s = \"é水🌲\";");
            for i in 0..count {
                source.push_str(&format!("println(size{i}(s));"));
            }
            source.push('}');
            let names = compile_and_collect_relocation_targets_mode(&source, &[], release);
            assert!(
                !names.iter().any(|name| name.contains("willow_string_len")),
                "{names:?}"
            );
        }
    }
}
