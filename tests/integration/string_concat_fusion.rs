// Fused String concatenation chains (willow-8hq4.29).
//
// A left-nested `+` chain whose intermediates are compiler temporaries is
// lowered to one `willow_string_concat_n` call that sizes the result once and
// formats `i64.toString()` operands directly into it. These tests pin the
// observable semantics the rewrite must preserve: operand evaluation order,
// empty and non-ASCII operands, integer extremes, results larger than a TLAB
// object, collections (including relocation) while operands are evaluated,
// suspension mid-chain, defer and lambda bodies, `format`, and panics raised
// by a middle operand.

use super::support::*;

#[test]
fn concat_fusion_empty_and_non_ascii_operands() {
    let src = r#"
fn empty() -> String { return ""; }
fn main() {
    let a = "";
    let b = "é水";
    let n = 0;
    println("[" + a + empty() + a + "]");
    println(a + b + n.toString() + a + "🌲" + b);
    println((a + b + a + a).len());
}
"#;
    let (out, ok) = compile_and_run(src);
    assert!(ok, "{out}");
    assert_eq!(out, "[]\né水0🌲é水\n5\n");
}

#[test]
fn concat_fusion_formats_integer_extremes() {
    let src = r#"
fn main() {
    let max = 9223372036854775807;
    let min = -9223372036854775807 - 1;
    let zero = 0;
    let neg = -7;
    println("max=" + max.toString() + " min=" + min.toString() + " zero=" + zero.toString() + " neg=" + neg.toString() + ".");
    let mut digits = "";
    let mut i = 0;
    while i < 20 {
        let p = i * 1000003 - 9;
        digits = digits + p.toString() + ",";
        i = i + 1;
    }
    println(digits);
}
"#;
    let (out, ok) = compile_and_run(src);
    assert!(ok, "{out}");
    let mut expected =
        "max=9223372036854775807 min=-9223372036854775808 zero=0 neg=-7.\n".to_string();
    for i in 0..20i64 {
        expected.push_str(&format!("{},", i * 1000003 - 9));
    }
    expected.push('\n');
    assert_eq!(out, expected);
}

#[test]
fn concat_fusion_builds_results_larger_than_a_tlab_object() {
    let src = r#"
fn main() {
    let mut block = "";
    let mut i = 0;
    while i < 600 {
        block = block + "abcdefgh";
        i = i + 1;
    }
    let n = 12345;
    let joined = block + "|" + n.toString() + "|" + block + "|" + block;
    println(joined.len());
    println(joined.substring(4795, 4812));
}
"#;
    let (out, ok) = compile_and_run(src);
    assert!(ok, "{out}");
    // 3 * 4800 + 3 separators + 5 digits.
    assert_eq!(out, "14408\ndefgh|12345|abcde\n");
}

#[test]
fn concat_fusion_preserves_operand_evaluation_order() {
    let src = r#"
fn part(tag: String) -> String {
    println("eval " + tag);
    return tag;
}
fn num(v: i64) -> i64 {
    println("num " + v.toString());
    return v;
}
fn main() {
    let s = part("a") + part("b") + num(1).toString() + part("c") + num(2).toString();
    println(s);
}
"#;
    let (out, ok) = compile_and_run(src);
    assert!(ok, "{out}");
    assert_eq!(out, "eval a\neval b\nnum 1\neval c\nnum 2\nab1c2\n");
}

const GC_DURING_OPERANDS: &str = r#"
fn churn(tag: String) -> String {
    let mut junk = "";
    let mut i = 0;
    while i < 200 {
        junk = junk + "x" + i.toString();
        i = i + 1;
    }
    gc_minor_collect();
    gc_collect();
    return tag + junk.len().toString();
}
fn main() {
    let head = "head-" + "é";
    let mut round = 0;
    while round < 3 {
        let n = round * 11;
        let s = head + ":" + churn("a") + ":" + n.toString() + ":" + churn("b") + ":" + head;
        println(s);
        round = round + 1;
    }
}
"#;

fn gc_during_operands_expected() -> String {
    let junk: usize = (0..200).map(|i: i64| 1 + i.to_string().len()).sum();
    let mut out = String::new();
    for round in 0..3 {
        out.push_str(&format!("head-é:a{junk}:{}:b{junk}:head-é\n", round * 11));
    }
    out
}

#[test]
fn concat_fusion_survives_collections_inside_operands() {
    let (out, ok) = compile_and_run(GC_DURING_OPERANDS);
    assert!(ok, "{out}");
    assert_eq!(out, gc_during_operands_expected());
}

#[test]
fn concat_fusion_survives_relocating_stress_inside_operands() {
    let (out, ok) = compile_and_run_gc_stress_mode(GC_DURING_OPERANDS, "relocate");
    assert!(ok, "{out}");
    assert_eq!(out, gc_during_operands_expected());
}

#[test]
fn concat_fusion_survives_every_gc_stress_mode() {
    let (out, ok) = compile_and_run_gc_stress_all(GC_DURING_OPERANDS);
    assert!(ok, "{out}");
    assert_eq!(out, gc_during_operands_expected());
}

#[test]
fn concat_fusion_runs_in_release_builds() {
    let (out, ok) = compile_and_run_release(GC_DURING_OPERANDS);
    assert!(ok, "{out}");
    assert_eq!(out, gc_during_operands_expected());
}

#[test]
fn concat_fusion_across_an_await() {
    let src = r#"
async fn later(v: i64) -> String {
    await sleep(1);
    return "<" + v.toString() + ">";
}
async fn main() {
    let name = "wí";
    let n = 41;
    let s = name + "=" + n.toString() + await later(n + 1) + "/" + name;
    println(s);
}
"#;
    let (out, ok) = compile_and_run(src);
    assert!(ok, "{out}");
    assert_eq!(out, "wí=41<42>/wí\n");
}

#[test]
fn concat_fusion_inside_defer_and_lambda_bodies() {
    let src = r#"
fn describe(label: String, factor: i64) -> closure(i64) -> String {
    return |x: i64| label + "=" + (x * factor).toString() + ";" + label;
}
fn main() {
    let tag = "τ";
    let n = -3;
    defer println("defer " + tag + n.toString() + "/" + tag);
    let f = describe("k", 7);
    println(f(6));
}
"#;
    let (out, ok) = compile_and_run(src);
    assert!(ok, "{out}");
    assert_eq!(out, "k=42;k\ndefer τ-3/τ\n");
}

#[test]
fn concat_fusion_with_format_arguments() {
    let src = r#"
fn main() {
    let a = "é水";
    let n = -42;
    let x = 1.5;
    println(format("{}:{}:{}!{}", a, n, n * 2, x));
    println(format("x{}y{}", n, true));
    println(format("{}{}{}", "", n, ""));
}
"#;
    let (out, ok) = compile_and_run(src);
    assert!(ok, "{out}");
    assert_eq!(out, "é水:-42:-84!1.5\nx-42ytrue\n-42\n");
}

#[test]
fn concat_fusion_panic_in_a_middle_operand() {
    let src = r#"
fn boom(v: i64) -> String {
    if v > 0 {
        panic("operand failed {}", v);
    }
    return "ok";
}
fn main() {
    let n = 5;
    println("before");
    let s = "a" + n.toString() + boom(n) + "z";
    println(s);
}
"#;
    let (out, ok) = compile_and_run_with_env(src, &[]);
    assert!(!ok, "a panicking operand must abort the program: {out}");
    assert!(out.starts_with("before\n"), "{out}");
    assert!(out.contains("operand failed 5"), "{out}");
    assert!(!out.contains("a5"), "the chain must not complete: {out}");
}

#[test]
fn concat_fusion_recovered_panic_leaves_prior_value() {
    let src = r#"
fn boom(v: i64) -> String {
    if v > 0 {
        panic("bad {}", v);
    }
    return "fine";
}
fn attempt(v: i64) -> String {
    let mut status = "unset";
    if true {
        defer match recover() {
            Some(info) => println("recovered: " + info.message),
            None => {}
        }
        status = "s" + v.toString() + boom(v) + "e" + v.toString();
    }
    return status;
}
fn main() {
    println(attempt(0));
    println(attempt(3));
}
"#;
    let (out, ok) = compile_and_run(src);
    assert!(ok, "{out}");
    assert_eq!(out, "s0finee0\nrecovered: bad 3\nunset\n");
}

#[test]
fn concat_fusion_self_updating_accumulator() {
    let src = r#"
fn main() {
    let mut text = "";
    let mut i = 0;
    while i < 5 {
        text = text + "[" + i.toString() + "]";
        i = i + 1;
    }
    println(text);
}
"#;
    let (out, ok) = compile_and_run(src);
    assert!(ok, "{out}");
    assert_eq!(out, "[0][1][2][3][4]\n");
}

#[test]
fn concat_fusion_keeps_named_intermediates_observable() {
    let src = r#"
fn main() {
    let n = 9;
    let left = "L" + n.toString();
    let both = left + "-" + left + "-" + n.toString();
    println(left);
    println(both);
}
"#;
    let (out, ok) = compile_and_run(src);
    assert!(ok, "{out}");
    assert_eq!(out, "L9\nL9-L9-9\n");
}

#[test]
fn concat_fusion_reads_fields_and_elements_at_their_position() {
    let src = r#"
import std::collections::Array;
class Box {
    pub text: String;
    pub init(self, text: String) { self.text = text; }
}
fn swap(b: Box, items: Array<String>) -> String {
    b.text = "new";
    items[0] = "B";
    return "|";
}
fn main() {
    let b = new Box("old");
    let items = ["A"];
    let s = b.text + items[0] + swap(b, items) + b.text + items[0];
    println(s);
}
"#;
    let (out, ok) = compile_and_run(src);
    assert!(ok, "{out}");
    assert_eq!(out, "oldA|newB\n");
}

#[test]
fn concat_fusion_emits_one_n_ary_call_for_the_chain() {
    let src = r#"
fn main() {
    let a = "x";
    let n = 3;
    let s = a + ":" + n.toString() + ":" + a;
    println(s);
}
"#;
    for release in [false, true] {
        let targets = compile_and_collect_relocation_targets_mode(src, &[], release);
        let n_ary = targets
            .iter()
            .filter(|name| name.as_str() == "willow_string_concat_n")
            .count();
        assert_eq!(n_ary, 1, "release={release}: {targets:?}");
        assert!(
            !targets.iter().any(|name| name == "willow_string_concat"),
            "the pairwise entry must not remain: {targets:?}"
        );
        assert!(
            !targets.iter().any(|name| name == "willow_i64_to_string"),
            "toString must be formatted in place: {targets:?}"
        );
        assert!(
            targets.iter().any(|name| name == "willow_panic_depth"),
            "the fused call keeps its panic check: {targets:?}"
        );
    }
}

#[test]
fn concat_fusion_keeps_two_operand_adds_on_the_pairwise_entry() {
    let src = r#"
fn main() {
    let a = "x";
    let b = "y";
    println(a + b);
}
"#;
    let targets = compile_and_collect_relocation_targets(src, &[]);
    assert!(
        targets.iter().any(|name| name == "willow_string_concat"),
        "{targets:?}"
    );
    assert!(
        !targets.iter().any(|name| name == "willow_string_concat_n"),
        "{targets:?}"
    );
}

#[test]
fn concat_fusion_with_many_parts_in_one_expression() {
    let mut expr = String::from("\"\"");
    let mut expected = String::new();
    for i in 0..300 {
        expr.push_str(&format!(" + \"p\" + ({i} - 150).toString()"));
        expected.push_str(&format!("p{}", i - 150));
    }
    let src = format!("fn main() {{\n    let s = {expr};\n    println(s);\n}}\n");
    let (out, ok) = compile_and_run(&src);
    assert!(ok, "{out}");
    assert_eq!(out, format!("{expected}\n"));
}

#[test]
fn concat_fusion_parts_from_concurrent_tasks() {
    let src = r#"
async fn label(id: i64) -> String {
    let mut acc = "";
    let mut i = 0;
    while i < 50 {
        acc = acc + "t" + id.toString() + "." + i.toString() + ";";
        i = i + 1;
    }
    return acc;
}
async fn main() {
    let a = label(1);
    let b = label(2);
    let ra = await a;
    let rb = await b;
    println(ra.len() + rb.len());
    println(ra.substring(0, 12) + "|" + rb.substring(0, 12));
}
"#;
    let (out, ok) = compile_and_run(src);
    assert!(ok, "{out}");
    let one: usize = (0..50).map(|i: i64| 4 + i.to_string().len()).sum();
    assert_eq!(out, format!("{}\nt1.0;t1.1;t1|t2.0;t2.1;t2\n", one * 2));
}
