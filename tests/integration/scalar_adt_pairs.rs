//! Scalar ADT pair value semantics, ABI boundaries, suspension and allocation.
use super::support::{compile_and_run, compile_and_run_with_env};

fn check(source: &str, expected: &str) {
    let (out, ok) = compile_and_run(source);
    assert!(ok, "{out}");
    assert_eq!(out, expected);
}

macro_rules! perspective {
    ($name:ident, $source:literal, $expected:literal) => {
        #[test]
        fn $name() {
            check($source, $expected);
        }
    };
}

perspective!(
    pair_01_i64_zero_is_some,
    "fn main() { let x: Option<i64> = Some(0); println(x.is_some()); println(x.unwrap()); }",
    "true\n0\n"
);
perspective!(
    pair_02_i64_extremes,
    "fn main() { let a: Option<i64> = Some(9223372036854775807); let b: Option<i64> = Some(-9223372036854775807 - 1); println(a.unwrap()); println(b.unwrap()); }",
    "9223372036854775807\n-9223372036854775808\n"
);
perspective!(
    pair_03_none_and_default,
    "fn main() { let x: Option<i64> = None; println(x.is_none()); println(x.unwrap_or(-7)); }",
    "true\n-7\n"
);
perspective!(
    pair_04_false_payload,
    "fn main() { let x: Option<bool> = Some(false); println(x.is_some()); println(x.unwrap()); }",
    "true\nfalse\n"
);
perspective!(
    pair_05_f64_signed_zero,
    "fn main() { let x: Option<f64> = Some(-0.0); println(1.0 / x.unwrap() < 0.0); }",
    "true\n"
);
perspective!(
    pair_06_f64_fraction,
    "fn main() { let x: Option<f64> = Some(1.25); println(x.unwrap() == 1.25); }",
    "true\n"
);
perspective!(
    pair_07_void_none,
    "fn main() { let x: Option<void> = None; println(x.is_none()); }",
    "true\n"
);
perspective!(
    pair_08_option_match,
    "fn main() { let x: Option<i64> = Some(19); match x { Some(n) => println(n), None => println(0) } }",
    "19\n"
);
perspective!(
    pair_09_result_both_variants,
    "fn main() { let a: Result<i64, bool> = Ok(8); let b: Result<i64, bool> = Err(false); println(a.unwrap()); println(b.unwrap_err()); }",
    "8\nfalse\n"
);
perspective!(
    pair_10_result_float_error,
    "fn main() { let x: Result<bool, f64> = Err(2.5); println(x.unwrap_err() == 2.5); }",
    "true\n"
);
perspective!(
    pair_11_result_void_success,
    "fn main() { let x: Result<void, i64> = Ok(); println(x.is_ok()); }",
    "true\n"
);
perspective!(
    pair_12_option_question,
    "fn pass(x: Option<i64>) -> Option<i64> { let n = x?; return Some(n + 1); } fn main() { println(pass(Some(4)).unwrap()); println(pass(None).is_none()); }",
    "5\ntrue\n"
);
perspective!(
    pair_13_result_question,
    "fn pass(x: Result<i64, bool>) -> Result<i64, bool> { let n = x?; return Ok(n + 1); } fn main() { println(pass(Ok(4)).unwrap()); println(pass(Err(false)).unwrap_err()); }",
    "5\nfalse\n"
);
perspective!(
    pair_14_direct_argument_return,
    "fn same(x: Option<f64>) -> Option<f64> { return x; } fn main() { println(same(Some(4.5)).unwrap() == 4.5); }",
    "true\n"
);
perspective!(
    pair_15_indirect_argument_return,
    "fn same(x: Option<i64>) -> Option<i64> { return x; } fn invoke(f: fn(Option<i64>) -> Option<i64>) -> i64 { return f(Some(31)).unwrap(); } fn main() { println(invoke(same)); }",
    "31\n"
);
perspective!(
    pair_16_lambda_pair_return,
    "fn main() { let f = |n: i64| -> Option<i64> { return Some(n + 2); }; println(f(5).unwrap()); }",
    "7\n"
);
perspective!(
    pair_17_option_map_scalar_to_reference,
    r#"fn text(n: i64) -> String { return "value"; } fn main() { let x: Option<i64> = Some(1); println(x.map(text).unwrap()); }"#,
    "value\n"
);
perspective!(
    pair_18_option_map_reference_to_scalar,
    r#"fn number(s: String) -> i64 { return 17; } fn main() { let x: Option<String> = Some("value"); println(x.map(number).unwrap()); }"#,
    "17\n"
);
perspective!(
    pair_19_result_map_preserves_error_across_representation,
    r#"fn text(n: i64) -> String { return "value"; } fn main() { let x: Result<i64, bool> = Err(false); println(x.map(text).unwrap_err()); }"#,
    "false\n"
);
perspective!(
    pair_20_result_and_then_preserves_error_across_representation,
    r#"fn text(n: i64) -> Result<String, bool> { return Ok("value"); } fn main() { let x: Result<i64, bool> = Err(false); println(x.and_then(text).unwrap_err()); }"#,
    "false\n"
);
perspective!(
    pair_21_result_or_else_preserves_ok_across_representation,
    r#"fn text(n: bool) -> Result<i64, String> { return Err("value"); } fn main() { let x: Result<i64, bool> = Ok(23); println(x.or_else(text).unwrap()); }"#,
    "23\n"
);
perspective!(
    pair_22_array_roundtrip,
    "import std::collections::Array; fn main() { let xs: Array<Option<i64>> = [Some(5), None, Some(9)]; gc_collect(); println(xs[0].unwrap()); println(xs[1].is_none()); println(xs[2].unwrap()); }",
    "5\ntrue\n9\n"
);
perspective!(
    pair_23_class_field_roundtrip,
    "class Holder { pub value: Option<i64>; pub tail: i64; } fn main() { let h = new Holder(Some(29), 41); gc_collect(); println(h.value.unwrap()); println(h.tail); }",
    "29\n41\n"
);
perspective!(
    pair_24_nested_option_roundtrip,
    "fn main() { let x: Option<Option<i64>> = Some(Some(37)); gc_collect(); println(x.unwrap().unwrap()); }",
    "37\n"
);
perspective!(
    pair_25_async_local_across_suspend,
    "async fn main() { let x: Option<i64> = Some(47); let y: Result<f64, bool> = Ok(1.5); await sleep(0); gc_collect(); println(x.unwrap()); println(y.unwrap() == 1.5); }",
    "47\ntrue\n"
);
perspective!(
    pair_26_async_result_across_suspend,
    "async fn produce() -> Option<i64> { await sleep(0); return Some(53); } async fn main() { let x = await produce(); gc_collect(); println(x.unwrap()); }",
    "53\n"
);
perspective!(
    pair_27_constructors_allocate_zero,
    "fn main() { let before = gc_allocated_bytes(); let x: Option<i64> = Some(59); let y: Option<bool> = None; let z: Result<f64, i64> = Ok(1.25); let after = gc_allocated_bytes(); println(after == before); println(x.unwrap()); println(y.is_none()); println(z.unwrap() == 1.25); }",
    "true\n59\ntrue\ntrue\n"
);
perspective!(
    pair_28_map_get_allocate_zero,
    "import std::collections::Map; fn main() { let m: Map<i64, i64> = Map::new(); m.insert(1, 61); let before = gc_allocated_bytes(); let x = m.get(1); let y = m.get(2); let after = gc_allocated_bytes(); println(after == before); println(x.unwrap()); println(y.is_none()); }",
    "true\n61\ntrue\n"
);

#[test]
fn pair_29_gc_stress_preserves_pair_and_neighbor_reference() {
    let source = r#"class Holder { pub value: Option<i64>; pub text: String; }
fn main() { let h = new Holder(Some(67), "a" + "b"); let pair: Result<i64, bool> = Ok(71); gc_minor_collect(); gc_collect(); println(h.value.unwrap()); println(h.text); println(pair.unwrap()); }"#;
    for mode in ["alloc", "minor"] {
        let (out, ok) = compile_and_run_with_env(source, &[("WILLOW_GC_STRESS", mode)]);
        assert!(ok, "{mode}: {out}");
        assert_eq!(out, "67\nab\n71\n");
    }
}

perspective!(
    pair_30_capture_pair_before_reference,
    r#"fn main() { let pair: Option<i64> = Some(73); let text = "kept" + "!"; let f = || { gc_collect(); println(pair.unwrap()); println(text); }; gc_minor_collect(); f(); }"#,
    "73\nkept!\n"
);
perspective!(
    pair_31_class_pair_before_reference,
    r#"class Holder { pub pair: Result<i64, bool>; pub text: String; } fn main() { let h = new Holder(Ok(79), "kept" + "!"); gc_minor_collect(); gc_collect(); println(h.pair.unwrap()); println(h.text); }"#,
    "79\nkept!\n"
);
perspective!(
    pair_32_static_pair,
    "class Store { pub static mut value: Option<i64> = Some(83); } fn main() { println(Store::value.unwrap()); Store::value = None; gc_collect(); println(Store::value.is_none()); Store::value = Some(89); println(Store::value.unwrap()); }",
    "83\ntrue\n89\n"
);
perspective!(
    pair_33_enum_pair_before_reference,
    r#"enum Packet { Data(Option<i64>, String), Empty } fn main() { let value = Packet::Data(Option::Some(97), "kept" + "!"); gc_minor_collect(); gc_collect(); match value { Packet::Data(n, s) => { println(n.unwrap()); println(s); }, Packet::Empty => println("empty") } }"#,
    "97\nkept!\n"
);
perspective!(
    pair_34_nested_result_payload,
    "fn main() { let value: Result<Option<i64>, i64> = Ok(Some(101)); gc_collect(); println(value.unwrap().unwrap()); }",
    "101\n"
);
perspective!(
    pair_35_array_mutable_element_reference,
    "import std::collections::Array; fn replace(value: &mut Option<i64>) { value = Some(103); } fn main() { let mut values: Array<Option<i64>> = [Some(1), Some(2)]; replace(&values[0]); gc_collect(); println(values[0].unwrap()); println(values[1].unwrap()); }",
    "103\n2\n"
);
perspective!(
    pair_36_channel_roundtrip,
    "fn main() { let channel: Channel<Option<i64>> = Channel::new(); let value: Option<i64> = Some(107); let absent: Option<i64> = None; channel.send(value); channel.send(absent); gc_collect(); println(channel.recv().unwrap()); println(channel.recv().is_none()); }",
    "107\ntrue\n"
);
perspective!(
    pair_37_select_channel_roundtrip,
    r#"async fn main() { let channel: Channel<Option<i64>> = Channel::new(); let value: Option<i64> = Some(109); channel.send(value); gc_collect(); select { let item = channel.recv() => { println(item.unwrap()); } default => { println("empty"); } } }"#,
    "109\n"
);
perspective!(
    pair_38_map_pair_value_roundtrip,
    "import std::collections::Map; fn main() { let mut map: Map<i64, Option<i64>> = Map::new(); let value: Option<i64> = Some(113); let absent: Option<i64> = None; map.insert(1, value); map.insert(2, absent); gc_collect(); println(map.get(1).unwrap().unwrap()); println(map.get(2).unwrap().is_none()); println(map.get(3).is_none()); }",
    "113\ntrue\ntrue\n"
);
perspective!(
    pair_39_blocking_cell_pair,
    "fn main() { let value: Option<i64> = Some(127); let cell = BlockingCell<Option<i64>>::new(value); gc_collect(); println(cell.get().unwrap()); let absent: Option<i64> = None; cell.set(absent); println(cell.get().is_none()); }",
    "127\ntrue\n"
);
perspective!(
    pair_40_mutex_pair,
    "async fn main() { let value: Option<i64> = Some(131); let mutex: Mutex<Option<i64>> = Mutex::new(value); lock mutex as mut item { println(item.unwrap()); item = Some(137); } gc_collect(); lock mutex as item { println(item.unwrap()); } }",
    "131\n137\n"
);

#[test]
fn pair_41_runnable_example() {
    let (out, ok) = super::support::compile_file_and_run("example/scalar_adt_pairs.wi");
    assert!(ok, "{out}");
    assert_eq!(out, "42\ntrue\n43\ntrue\n44\n45\n");
}

perspective!(
    pair_42_frozen_array_snapshot_survives_borrow_mutation,
    "import std::collections::Array; fn replace(value: &mut Option<i64>) { value = Some(103); } fn main() { let mut values: Array<Option<i64>> = [Some(1), Some(2)]; let snapshot = values.freeze(); replace(&values[0]); gc_collect(); println(snapshot[0].unwrap()); println(values[0].unwrap()); }",
    "1\n103\n"
);
perspective!(
    pair_43_array_borrow_survives_pop_gc,
    "import std::collections::Array; fn replace(value: &mut Option<i64>, values: Array<Option<i64>>) { values.pop(); gc_collect(); value = Some(103); println(value.unwrap()); } fn main() { let mut values: Array<Option<i64>> = [Some(1)]; replace(&values[0], values); println(values.len()); }",
    "103\n0\n"
);

#[test]
fn pair_44_deterministic_allocation_scaling() {
    for count in [1, 16, 128] {
        let mut source = String::from(
            "import std::collections::Map; fn main() { let m: Map<i64,i64> = Map::new(); m.insert(1,7); let before = gc_allocated_bytes(); let mut sum = 0; ",
        );
        for i in 0..count {
            source.push_str(&format!("let a{i}: Option<i64> = Some({i}); let b{i}: Result<i64,bool> = Err(false); sum += a{i}.unwrap_or(0); if b{i}.is_err() {{ sum += 1; }} sum += m.get(1).unwrap_or(0); if m.get(2).is_none() {{ sum += 1; }} "));
        }
        source.push_str(
            "let allocated = gc_allocated_bytes() - before; println(allocated); println(sum); }",
        );
        check(
            &source,
            &format!("0\n{}\n", count * (count - 1) / 2 + 9 * count),
        );
        let relocations = super::support::compile_and_collect_relocation_targets_all(&source, &[]);
        assert_eq!(
            relocations
                .iter()
                .filter(|name| name.as_str() == "willow_map_get_into")
                .count(),
            2 * count
        );
        assert!(!relocations.iter().any(|name| name == "willow_gc_alloc_layout" || name == "willow_alloc_enum_variant"));
        eprintln!(
            "scalar-pair count={count} constructions={} map-lookups={} gc-allocation-bytes=0 emitted-map-calls={}",
            2 * count,
            2 * count,
            2 * count
        );
    }
}

perspective!(
    pair_45_bounded_channel_send_survives_retry,
    "async fn send(channel: Channel<Option<i64>>) { let value: Option<i64> = Some(2); channel.send(value); } async fn main() { let channel = Channel<Option<i64>>::with_capacity(1); let first: Option<i64> = Some(1); channel.send(first); let task = send(channel); await sleep(1); gc_collect(); println(channel.recv().unwrap()); await task; println(channel.recv().unwrap()); }",
    "1\n2\n"
);
perspective!(
    pair_46_async_pair_parameters_and_result,
    "async fn combine(a: Option<i64>, text: String, b: Result<i64,bool>) -> Result<i64,bool> { await sleep(0); gc_collect(); println(text); return Ok(a.unwrap() + b.unwrap()); } async fn main() { let x = await combine(Some(3), \"kept\" + \"!\", Ok(4)); println(x.unwrap()); }",
    "kept!\n7\n"
);

perspective!(
    pair_47_question_boxed_to_pair,
    "fn convert(x: Result<String,i64>) -> Result<i64,i64> { let text = x?; return Ok(1); } fn main() { println(convert(Err(7)).unwrap_err()); }",
    "7\n"
);
perspective!(
    pair_48_question_pair_to_boxed,
    "fn convert(x: Result<i64,i64>) -> Result<String,i64> { let number = x?; return Ok(\"ok\"); } fn main() { println(convert(Err(9)).unwrap_err()); }",
    "9\n"
);

#[test]
fn pair_49_closure_physical_word_boundaries() {
    for count in [30, 31, 32, 63, 64, 128] {
        for prefix_ref in [false, true] {
            let mut source = String::from("fn make() -> closure() -> i64 { ");
            if prefix_ref {
                source.push_str("let atext = \"early\" + \"!\"; ");
            }
            for i in 0..count {
                source.push_str(&format!("let c{i:03}: Option<i64> = Some({i}); "));
            }
            source.push_str("let ztext = \"kept\" + \"!\"; return || -> i64 { let mut sum = 0; ");
            if prefix_ref {
                source.push_str("println(atext); ");
            }
            for i in 0..count {
                source.push_str(&format!("sum += c{i:03}.unwrap(); "));
            }
            source.push_str("gc_minor_collect(); println(ztext); return sum; }; } fn main() { let f = make(); gc_collect(); println(f()); }");
            let (out, ok) = compile_and_run_with_env(&source, &[("WILLOW_GC_STRESS", "all")]);
            assert!(ok, "count={count}, prefix_ref={prefix_ref}: {out}");
            let prefix = if prefix_ref { "early!\n" } else { "" };
            assert_eq!(out, format!("{prefix}kept!\n{}\n", count * (count - 1) / 2));
        }
    }
}

#[test]
fn pair_50_enum_physical_word_boundaries() {
    for count in [30, 31, 32, 63, 64, 128] {
        for prefix_ref in [false, true] {
            let mut types = std::iter::repeat_n("Option<i64>", count)
                .collect::<Vec<_>>()
                .join(", ");
            let mut values = (0..count)
                .map(|i| format!("Some({i})"))
                .collect::<Vec<_>>()
                .join(", ");
            let mut names = (0..count)
                .map(|i| format!("p{i}"))
                .collect::<Vec<_>>()
                .join(", ");
            let sum = (0..count)
                .map(|i| format!("p{i}.unwrap()"))
                .collect::<Vec<_>>()
                .join(" + ");
            if prefix_ref {
                types = format!("String, {types}");
                values = format!("\"early\" + \"!\", {values}");
                names = format!("early, {names}");
            }
            let print_early = if prefix_ref { "println(early);" } else { "" };
            let source = format!(
                "enum Wide {{ Data({types}, String) }} fn make() -> Wide {{ return Wide::Data({values}, \"kept\" + \"!\"); }} fn main() {{ let x = make(); gc_collect(); match x {{ Wide::Data({names}, text) => {{ gc_minor_collect(); {print_early} println(text); println({sum}); }} }} }}"
            );
            let (out, ok) = compile_and_run_with_env(&source, &[("WILLOW_GC_STRESS", "all")]);
            assert!(ok, "count={count}, prefix_ref={prefix_ref}: {out}");
            let prefix = if prefix_ref { "early!\n" } else { "" };
            assert_eq!(out, format!("{prefix}kept!\n{}\n", count * (count - 1) / 2));
        }
    }
}

#[test]
fn pair_51_reference_capture_keeps_original_buffer_after_resize() {
    let source = r#"
import std::collections::Array;
fn grow(a: Array<Option<i64>>) -> i64 { a.push(Some(2)); gc_minor_collect(); return 0; }
fn write(x: &mut Option<i64>, ignored: i64) { x = Some(99); println(x.unwrap()); }
fn main() {
    let a: Array<Option<i64>> = [Some(1)];
    write(&a[0], grow(a));
    println(a[0].unwrap());
}
"#;
    let (out, ok) = compile_and_run_with_env(source, &[("WILLOW_GC_STRESS", "all")]);
    assert!(ok, "{out}");
    assert_eq!(out, "99\n1\n");
}

#[test]
fn pair_52_reference_resize_and_await_preserve_both_buffers() {
    let source = r#"
import std::collections::Array;
fn grow(a: Array<Result<i64,bool>>) -> i64 { a.push(Err(false)); return 0; }
async fn pause(n: i64) -> i64 { await yield(); gc_minor_collect(); return n; }
fn write(x: &mut Result<i64,bool>, ignored: i64) { gc_collect(); x = Ok(99); println(x.unwrap()); }
async fn main() {
    let a: Array<Result<i64,bool>> = [Ok(1)];
    write(&a[0], await pause(grow(a)));
    println(a[0].unwrap());
    println(a[1].unwrap_err());
}
"#;
    let (out, ok) = compile_and_run_with_env(source, &[("WILLOW_GC_STRESS", "all")]);
    assert!(ok, "{out}");
    assert_eq!(out, "99\n1\nfalse\n");
}

#[test]
fn pair_53_reference_without_growth_stays_attached() {
    let source = r#"
import std::collections::Array;
fn push(a: Array<Option<i64>>) -> i64 { a.push(Some(2)); gc_minor_collect(); return 0; }
fn write(x: &mut Option<i64>, ignored: i64) { x = Some(99); }
fn main() {
    let a: Array<Option<i64>> = [];
    a.push(Some(1));
    write(&a[0], push(a));
    println(a[0].unwrap());
}
"#;
    let (out, ok) = compile_and_run_with_env(source, &[("WILLOW_GC_STRESS", "all")]);
    assert!(ok, "{out}");
    assert_eq!(out, "99\n");
}

#[test]
fn pair_54_reference_survives_later_argument_pop() {
    for (ty, constructor) in [("Option<i64>", "Some"), ("Result<i64,bool>", "Ok")] {
        let source = format!(
            r#"
import std::collections::Array;
fn remove(a: Array<{ty}>) -> i64 {{ let old = a.pop(); gc_collect(); return old.unwrap(); }}
fn write(x: &mut {ty}, removed: i64) {{ x = {constructor}(99); println(removed); println(x.unwrap()); }}
fn main() {{ let a: Array<{ty}> = [{constructor}(1)]; write(&a[0], remove(a)); println(a.len()); }}
"#
        );
        let (out, ok) = compile_and_run_with_env(&source, &[("WILLOW_GC_STRESS", "all")]);
        assert!(ok, "{ty}: {out}");
        assert_eq!(out, "1\n99\n0\n");
    }
}

#[test]
fn pair_55_reference_survives_later_argument_pop_and_suspend() {
    for (ty, constructor) in [("Option<i64>", "Some"), ("Result<i64,bool>", "Ok")] {
        let source = format!(
            r#"
import std::collections::Array;
fn remove(a: Array<{ty}>) -> i64 {{ let old = a.pop(); gc_collect(); return old.unwrap(); }}
async fn pause(n: i64) -> i64 {{ await yield(); gc_minor_collect(); return n; }}
fn write(x: &mut {ty}, removed: i64) {{ x = {constructor}(99); println(removed); println(x.unwrap()); }}
async fn main() {{ let a: Array<{ty}> = [{constructor}(1)]; write(&a[0], await pause(remove(a))); println(a.len()); }}
"#
        );
        let (out, ok) = compile_and_run_with_env(&source, &[("WILLOW_GC_STRESS", "all")]);
        assert!(ok, "{ty}: {out}");
        assert_eq!(out, "1\n99\n0\n");
    }
}

#[test]
fn pair_56_pop_push_reuses_borrowed_slot() {
    for (ty, constructor) in [("Option<i64>", "Some"), ("Result<i64,bool>", "Ok")] {
        let source = format!(
            r#"
import std::collections::Array;
fn write(x: &mut {ty}, a: Array<{ty}>) {{
    let old = a.pop(); gc_collect(); a.push({constructor}(2)); gc_minor_collect();
    println(x.unwrap()); x = {constructor}(99); println(old.unwrap());
}}
fn main() {{ let a: Array<{ty}> = [{constructor}(1)]; write(&a[0], a); println(a[0].unwrap()); }}
"#
        );
        let (out, ok) = compile_and_run_with_env(&source, &[("WILLOW_GC_STRESS", "all")]);
        assert!(ok, "{ty}: {out}");
        assert_eq!(out, "2\n1\n99\n");
    }
}

#[test]
fn pair_57_multiple_pops_preserve_borrowed_high_slot() {
    for (ty, constructor) in [("Option<i64>", "Some"), ("Result<i64,bool>", "Ok")] {
        let source = format!(
            r#"
import std::collections::Array;
fn replace(a: Array<{ty}>) -> i64 {{
    a.pop(); a.pop(); gc_collect(); a.push({constructor}(3));
    gc_minor_collect(); a.push({constructor}(4)); return 0;
}}
async fn pause(n: i64) -> i64 {{ await yield(); gc_minor_collect(); return n; }}
fn write(x: &mut {ty}, ignored: i64) {{ println(x.unwrap()); x = {constructor}(99); }}
async fn main() {{ let a: Array<{ty}> = [{constructor}(1), {constructor}(2)]; write(&a[1], await pause(replace(a))); println(a[0].unwrap()); println(a[1].unwrap()); }}
"#
        );
        let (out, ok) = compile_and_run_with_env(&source, &[("WILLOW_GC_STRESS", "all")]);
        assert!(ok, "{ty}: {out}");
        assert_eq!(out, "4\n3\n99\n");
    }
}
