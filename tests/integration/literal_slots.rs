//! Compiler-owned literal slots: runtime behavior and structural scaling.
use super::support::*;

#[test]
fn literal_slots_strings_survive_static_init_lambdas_async_and_gc() {
    let source = r#"
class Text { pub static value: String = "é水🦀"; }
fn text() -> String { let f = || "é水🦀"; return f(); }
async fn worker() -> String { await sleep(1); gc_collect(); return "é水🦀"; }
async fn main() {
    let mut i = 0;
    while i < 32 {
        let task = worker();
        let value = await task;
        gc_collect();
        if value != Text::value { println("bad async"); }
        if Text::value != text() { println("bad lambda"); }
        i = i + 1;
    }
    println(text());
    println("");
    println("é");
}
"#;
    for run in [
        compile_and_run,
        compile_and_run_release,
        compile_and_run_gc_stress_all,
    ] {
        let (out, ok) = run(source);
        assert!(ok, "{out}");
        assert_eq!(out, "é水🦀\n\né\n");
    }
}

#[test]
fn literal_slots_data_scales_with_unique_literals_not_uses() {
    fn source(unique: usize, uses: usize) -> String {
        let mut source = String::from("fn main() {\n");
        for _ in 0..uses {
            for index in 0..unique {
                source.push_str(&format!("println(\"literal {index}\");\n"));
            }
        }
        source.push_str("}\n");
        source
    }
    let count = |source: &str| {
        compile_and_collect_defined_symbols(source, &[])
            .iter()
            .filter(|name| name.starts_with("willow_str_") && name.ends_with("_slot"))
            .count()
    };
    let baseline = count(&source(1, 1));
    for unique in [1, 8, 32] {
        for uses in [1, 4, 16] {
            let program = source(unique, uses);
            let slots = count(&program);
            assert_eq!(slots, baseline + unique - 1);
            let targets = compile_and_collect_relocation_targets_all(&program, &[]);
            assert!(!targets.iter().any(|name| name == "willow_string_literal"));
            assert_eq!(
                targets
                    .iter()
                    .filter(|name| name.as_str() == "willow_string_literal_slot")
                    .count(),
                unique * uses
            );
            println!(
                "unique={unique} uses={uses} slots={slots} literal_calls={}",
                unique * uses
            );
        }
    }
}

#[test]
fn literal_slots_shared_imports_and_module_static_initializers() {
    let files = [
        (
            "texts.wi",
            r#"
pub class Text { pub static value: String = "shared"; }
pub fn text() -> String { return "shared"; }
"#,
        ),
        (
            "main.wi",
            r#"
import texts;
fn main() {
    let mut i = 0;
    while i < 16 {
        gc_collect();
        if texts::text() != "shared" { println("bad function"); }
        if texts::Text::value != "shared" { println("bad static"); }
        i = i + 1;
    }
    println(texts::Text::value);
}
"#,
        ),
    ];
    let (out, ok) = compile_temp_project_with_env_and_run_under(
        &files,
        "main.wi",
        &[],
        &[("WILLOW_GC_STRESS", "all")],
    );
    assert!(ok, "{out}");
    assert_eq!(out, "shared\n");
}
