use super::support::*;

// Permanent T? -> Option<T> parser normalization — willow-glaj.4/.10.

fn run(source: &str) -> String {
    let (out, ok) = compile_and_run(source);
    assert!(ok, "{out}");
    out
}

#[test]
fn option_sugar_01_t_question_assigns_to_explicit_option() {
    assert_eq!(
        run(
            r#"fn make() -> String? { return Some("x"); } fn main() { let v: Option<String> = make(); println(v.unwrap()); }"#
        ),
        "x\n"
    );
}

#[test]
fn option_sugar_02_explicit_option_assigns_to_t_question() {
    assert_eq!(
        run(
            r#"fn make() -> Option<String> { return Some("x"); } fn main() { let v: String? = make(); println(v.unwrap()); }"#
        ),
        "x\n"
    );
}

#[test]
fn option_sugar_03_argument_types_are_identical() {
    assert_eq!(
        run(
            r#"fn show(v: String?) { println(v.unwrap_or("none")); } fn main() { let v: Option<String> = Some("arg"); show(v); }"#
        ),
        "arg\n"
    );
}

#[test]
fn option_sugar_04_return_types_are_identical() {
    assert_eq!(
        run(
            r#"fn pass(v: Option<String>) -> String? { return v; } fn main() { println(pass(Some("return")).unwrap()); }"#
        ),
        "return\n"
    );
}

#[test]
fn option_sugar_05_repeated_suffix_preserves_three_states() {
    assert_eq!(
        run(r#"
fn show(v: String??) {
    match v { Some(inner) => println(inner.is_none()), None => println("outer-none") }
}
fn main() { show(None); show(Some(None)); show(Some(Some("value"))); }
"#),
        "outer-none\ntrue\nfalse\n"
    );
}

#[test]
fn option_sugar_06_option_of_sugar_equals_nested_option() {
    assert_eq!(
        run(
            r#"fn pass(v: Option<String?>) -> Option<Option<String>> { return v; } fn main() { println(pass(Some(None)).unwrap().is_none()); }"#
        ),
        "true\n"
    );
}

#[test]
fn option_sugar_07_none_uses_niche_through_sugar() {
    assert_eq!(
        run(r#"fn main() { let v: String? = None; println(v.is_none()); }"#),
        "true\n"
    );
}

#[test]
fn option_sugar_08_none_uses_boxed_scalar_through_sugar() {
    assert_eq!(
        run(r#"fn main() { let v: i64? = None; println(v.is_none()); println(v.unwrap_or(9)); }"#),
        "true\n9\n"
    );
}

#[test]
fn option_sugar_09_none_in_argument() {
    assert_eq!(
        run(
            r#"fn absent(v: i64?) -> bool { return v.is_none(); } fn main() { println(absent(None)); }"#
        ),
        "true\n"
    );
}

#[test]
fn option_sugar_10_none_in_return() {
    assert_eq!(
        run(r#"fn absent() -> i64? { return None; } fn main() { println(absent().is_none()); }"#),
        "true\n"
    );
}

#[test]
fn option_sugar_11_none_in_reassignment() {
    assert_eq!(
        run(r#"fn main() { let mut v: String? = Some("x"); v = None; println(v.is_none()); }"#),
        "true\n"
    );
}

#[test]
fn option_sugar_12_none_in_constructor_and_field() {
    assert_eq!(
        run(
            r#"class Holder { pub value: String?; } fn main() { let h = new Holder(None); println(h.value.is_none()); h.value = Some("field"); println(h.value.unwrap()); }"#
        ),
        "true\nfield\n"
    );
}

#[test]
fn option_sugar_13_none_in_array_element() {
    assert_eq!(
        run(
            r#"import std::collections::Array; fn main() { let xs: Array<String?> = [None, Some("x")]; println(xs[0].is_none()); println(xs[1].unwrap()); }"#
        ),
        "true\nx\n"
    );
}

#[test]
fn option_sugar_14_none_in_map_value() {
    assert_eq!(
        run(
            r#"import std::collections::Map; fn main() { let mut m: Map<i64, String?> = Map::new(); m.insert(1, None); println(m.get(1).unwrap().is_none()); }"#
        ),
        "true\n"
    );
}

#[test]
fn option_sugar_15_none_in_channel_value() {
    assert_eq!(
        run(
            r#"fn main() { let ch: Channel<String?> = Channel::new(); ch.send(None); println(ch.recv().is_none()); }"#
        ),
        "true\n"
    );
}

#[test]
fn option_sugar_16_none_in_contextual_ternary() {
    assert_eq!(
        run(
            r#"fn main() { let a: String? = true ? Some("x") : None; let b: String? = false ? Some("x") : None; println(a.unwrap()); println(b.is_none()); }"#
        ),
        "x\ntrue\n"
    );
}

#[test]
fn option_sugar_17_async_frame_uses_canonical_option() {
    assert_eq!(
        run(
            r#"async fn make() -> String? { await sleep(1); return None; } async fn main() { let v: Option<String> = await make(); println(v.is_none()); }"#
        ),
        "true\n"
    );
}

#[test]
fn option_sugar_18_nil_is_removed_before_inference() {
    assert_compile_error_contains(
        "fn main() { let value = nil; }",
        &[
            "error[E0201]",
            "`nil` has been removed",
            "replace `nil` with `None`",
        ],
    );
}

#[test]
fn option_sugar_19_plain_value_is_not_implicitly_lifted() {
    assert_compile_error_contains(
        "fn main() { let value: i64? = 7; }",
        &["error[E0201]", "expected `Option<i64>`", "found `i64`"],
    );
}

#[test]
fn option_sugar_20_gc_reference_round_trip_uses_niche() {
    let source = r#"
class Node { pub value: i64; }
fn main() { let v: Node? = Some(new Node(42)); gc_minor_collect(); println(v.unwrap().value); }
"#;
    let (out, ok) = compile_and_run_with_env(source, &[("WILLOW_GC_VERIFY_BARRIER", "1")]);
    assert!(ok, "{out}");
    assert_eq!(out, "42\n");
}
