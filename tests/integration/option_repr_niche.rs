use super::support::*;

// Central Option representation — willow-glaj.3.
//
// A GC-reference payload uses zero as None and the payload pointer itself as
// Some. Scalars and nested Options retain the boxed tagged
// representation. These end-to-end perspectives exercise every source-level
// producer/consumer that can otherwise accidentally load a tag from offset 0.

fn run(source: &str) -> String {
    let (out, ok) = compile_and_run(source);
    assert!(ok, "{out}");
    out
}

#[test]
fn option_niche_01_string_some_matches_without_tag_load() {
    assert_eq!(
        run(
            r#"fn main() { let v: Option<String> = Some("hello"); match v { Some(s) => println(s), None => println("none") } }"#
        ),
        "hello\n"
    );
}

#[test]
fn option_niche_02_string_none_matches_without_dereferencing_zero() {
    assert_eq!(
        run(
            r#"fn main() { let v: Option<String> = None; match v { Some(s) => println(s), None => println("none") } }"#
        ),
        "none\n"
    );
}

#[test]
fn option_niche_03_predicates_use_pointer_presence() {
    assert_eq!(
        run(
            r#"fn main() { let a: Option<String> = Some("x"); let b: Option<String> = None; println(a.is_some()); println(a.is_none()); println(b.is_some()); println(b.is_none()); }"#
        ),
        "true\nfalse\nfalse\ntrue\n"
    );
}

#[test]
fn option_niche_04_unwrap_returns_the_reference_payload() {
    assert_eq!(
        run(r#"fn main() { let v: Option<String> = Some("unwrapped"); println(v.unwrap()); }"#),
        "unwrapped\n"
    );
}

#[test]
fn option_niche_05_unwrap_or_handles_some_and_none() {
    assert_eq!(
        run(
            r#"fn main() { let a: Option<String> = Some("a"); let b: Option<String> = None; println(a.unwrap_or("x")); println(b.unwrap_or("x")); }"#
        ),
        "a\nx\n"
    );
}

#[test]
fn option_niche_06_map_niche_to_niche() {
    assert_eq!(
        run(
            r#"fn main() { let v: Option<String> = Some("a"); let r = v.map(|s: String| s + "b"); println(r.unwrap()); }"#
        ),
        "ab\n"
    );
}

#[test]
fn option_niche_07_map_niche_to_boxed_scalar() {
    assert_eq!(
        run(
            r#"fn main() { let v: Option<String> = Some("a"); let r = v.map(|s: String| 7); println(r.unwrap()); }"#
        ),
        "7\n"
    );
}

#[test]
fn option_niche_08_map_boxed_scalar_to_niche() {
    assert_eq!(
        run(
            r#"fn main() { let v: Option<i64> = Some(7); let r = v.map(|n: i64| "value"); println(r.unwrap()); }"#
        ),
        "value\n"
    );
}

#[test]
fn option_niche_09_and_then_constructs_niche_none_and_some() {
    assert_eq!(
        run(r#"
fn convert(n: i64) -> Option<String> { if n > 0 { return Some("yes"); } return None; }
fn main() { let a: Option<i64> = Some(1); let b: Option<i64> = Some(0); println(a.and_then(convert).unwrap_or("no")); println(b.and_then(convert).unwrap_or("no")); }
"#),
        "yes\nno\n"
    );
}

#[test]
fn option_niche_10_or_else_preserves_or_creates_niche_value() {
    assert_eq!(
        run(r#"
fn fallback() -> Option<String> { return Some("fallback"); }
fn main() { let a: Option<String> = Some("kept"); let b: Option<String> = None; println(a.or_else(fallback).unwrap()); println(b.or_else(fallback).unwrap()); }
"#),
        "kept\nfallback\n"
    );
}

#[test]
fn option_niche_11_try_propagates_niche_to_niche() {
    assert_eq!(
        run(r#"
fn pass(v: Option<String>) -> Option<String> { let s = v?; return Some(s + "!"); }
fn main() { println(pass(Some("ok")).unwrap()); println(pass(None).is_none()); }
"#),
        "ok!\ntrue\n"
    );
}

#[test]
fn option_niche_12_try_converts_niche_none_to_boxed_none() {
    assert_eq!(
        run(r#"
fn length(v: Option<String>) -> Option<i64> { let s = v?; return Some(3); }
fn main() { println(length(None).is_none()); println(length(Some("abc")).unwrap()); }
"#),
        "true\n3\n"
    );
}

#[test]
fn option_niche_13_try_converts_boxed_none_to_niche_none() {
    assert_eq!(
        run(r#"
fn label(v: Option<i64>) -> Option<String> { let n = v?; return Some("present"); }
fn main() { println(label(None).is_none()); println(label(Some(1)).unwrap()); }
"#),
        "true\npresent\n"
    );
}

#[test]
fn option_niche_14_nested_some_none_is_distinct_from_outer_none() {
    assert_eq!(
        run(r#"
fn show(v: Option<Option<String>>) {
    match v { Some(inner) => println(inner.is_none()), None => println("outer-none") }
}
fn main() { show(Some(None)); show(None); show(Some(Some("x"))); }
"#),
        "true\nouter-none\nfalse\n"
    );
}

#[test]
fn option_niche_15_class_payload_uses_pointer_niche() {
    assert_eq!(
        run(r#"
class Boxed { pub value: String; }
fn main() { let v: Option<Boxed> = Some(new Boxed("class")); match v { Some(b) => println(b.value), None => println("none") } }
"#),
        "class\n"
    );
}

#[test]
fn option_niche_16_array_payload_uses_pointer_niche() {
    assert_eq!(
        run(r#"
import std::collections::Array;
fn main() { let v: Option<Array<i64>> = Some([4, 5]); match v { Some(xs) => println(xs[1]), None => println(0) } }
"#),
        "5\n"
    );
}

#[test]
fn option_niche_17_option_values_survive_field_storage_and_minor_gc() {
    assert_eq!(
        run(r#"
class Holder { pub value: Option<String>; }
fn main() { let h = new Holder(Some("kept" + "-field")); gc_minor_collect(); println(h.value.unwrap()); }
"#),
        "kept-field\n"
    );
}

#[test]
fn option_niche_18_option_values_survive_array_storage_and_minor_gc() {
    assert_eq!(
        run(r#"
import std::collections::Array;
fn main() { let xs: Array<Option<String>> = [Some("kept" + "-array"), None]; gc_minor_collect(); println(xs[0].unwrap()); println(xs[1].is_none()); }
"#),
        "kept-array\ntrue\n"
    );
}

#[test]
fn option_niche_19_map_get_reference_value_returns_niche_option() {
    assert_eq!(
        run(r#"
import std::collections::Map;
fn main() { let mut m: Map<i64, String> = Map::new(); m.insert(1, "map-value"); println(m.get(1).unwrap()); println(m.get(2).is_none()); }
"#),
        "map-value\ntrue\n"
    );
}

#[test]
fn option_niche_20_map_of_nested_option_keeps_outer_boxed() {
    assert_eq!(
        run(r#"
import std::collections::Map;
fn main() {
    let mut m: Map<i64, Option<String>> = Map::new();
    let absent: Option<String> = None;
    let present: Option<String> = Some("nested");
    m.insert(1, absent); m.insert(2, present);
    match m.get(1) { Some(inner) => println(inner.is_none()), None => println("missing") }
    match m.get(2) { Some(inner) => println(inner.unwrap()), None => println("missing") }
    println(m.get(3).is_none());
}
"#),
        "true\nnested\ntrue\n"
    );
}

#[test]
fn option_niche_21_function_parameter_and_return_are_one_word() {
    assert_eq!(
        run(r#"
fn echo(v: Option<String>) -> Option<String> { return v; }
fn main() { println(echo(Some("abi")).unwrap()); println(echo(None).is_none()); }
"#),
        "abi\ntrue\n"
    );
}

#[test]
fn option_niche_22_async_frame_preserves_niche_option() {
    assert_eq!(
        run(r#"
async fn make() -> Option<String> { await sleep(1); return Some("async"); }
async fn main() { let value = await make(); println(value.unwrap()); }
"#),
        "async\n"
    );
}

#[test]
fn option_niche_23_channel_round_trips_niche_option() {
    assert_eq!(
        run(r#"
fn main() { let ch: Channel<Option<String>> = Channel::new(); let present: Option<String> = Some("channel"); let absent: Option<String> = None; ch.send(present); ch.send(absent); println(ch.recv().unwrap()); println(ch.recv().is_none()); }
"#),
        "channel\ntrue\n"
    );
}

#[test]
fn option_niche_24_some_and_none_do_not_allocate_outer_objects() {
    assert_eq!(
        run(r#"
fn main() {
    let text = "already-allocated";
    let before = gc_allocated_bytes();
    let some: Option<String> = Some(text);
    let none: Option<String> = None;
    let after = gc_allocated_bytes();
    println(after == before);
    println(some.unwrap());
    println(none.is_none());
}
"#),
        "true\nalready-allocated\ntrue\n"
    );
}

#[test]
fn option_niche_25_gc_stress_and_barrier_verifier_preserve_payload() {
    let source = r#"
class Holder { pub value: Option<String>; }
fn main() {
    let h = new Holder(Some("stress" + "-value"));
    let mut i = 0;
    while i < 64 { let junk = "x" + "y"; i = i + 1; }
    gc_minor_collect();
    println(h.value.unwrap());
}
"#;
    let (out, ok) = compile_and_run_with_env(
        source,
        &[
            ("WILLOW_GC_STRESS", "alloc"),
            ("WILLOW_GC_VERIFY_BARRIER", "1"),
        ],
    );
    assert!(ok, "{out}");
    assert_eq!(out, "stress-value\n");
}

#[test]
fn option_niche_26_lir_selection_and_ast_selection_are_equivalent() {
    let source = r#"
fn main() { let v: Option<String> = Some("same"); match v { Some(s) => println(s), None => println("none") } }
"#;
    let (ast, ast_ok) = compile_and_run_with_env(source, &[("WILLOW_LIR_BACKEND", "0")]);
    let (lir, lir_ok) = compile_and_run_with_env(source, &[("WILLOW_LIR_BACKEND", "1")]);
    assert!(ast_ok, "{ast}");
    assert!(lir_ok, "{lir}");
    assert_eq!(ast, "same\n");
    assert_eq!(lir, ast);
}

#[test]
fn option_niche_27_shorthand_and_explicit_linked_lists_allocate_equally() {
    assert_eq!(
        run(r#"
class OptionNode { pub value: i64; pub next: Option<OptionNode>; }
class SugarNode { pub value: i64; pub next: SugarNode?; }

fn option_bytes() -> i64 {
    let before = gc_allocated_bytes();
    let mut head: Option<OptionNode> = None;
    let mut i = 0;
    while i < 128 { head = Some(new OptionNode(i, head)); i = i + 1; }
    let used = gc_allocated_bytes() - before;
    println(head.is_some());
    return used;
}

fn sugar_bytes() -> i64 {
    let before = gc_allocated_bytes();
    let mut head: SugarNode? = None;
    let mut i = 0;
    while i < 128 { head = Some(new SugarNode(i, head)); i = i + 1; }
    let used = gc_allocated_bytes() - before;
    println(head.is_some());
    return used;
}

fn main() {
    let option_used = option_bytes();
    gc_collect();
    let sugar_used = sugar_bytes();
    println(option_used == sugar_used);
}
"#),
        "true\ntrue\ntrue\n"
    );
}
