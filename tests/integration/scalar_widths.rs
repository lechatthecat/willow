//! Explicit perspectives: 1 signed minima; 2 unsigned maxima; 3 contextual
//! literals; 4 function arguments/returns; 5 signed comparison; 6 unsigned
//! comparison; 7 add/sub/mul; 8 division/remainder; 9 bitwise/complement;
//! 10 shifts; 11 power; 12 wrapping methods; 13 checked methods; 14 every
//! integer conversion pair; 15 lossless widening; 16 conversion rejection;
//! 17 decimal parsing; 18 decimal formatting; 19 Unicode boundaries;
//! 20 invalid Unicode; 21 character literals/escapes; 22 const/match;
//! 23 generic identity; 24 class fields; 25 Option/Result; 26 packed arrays;
//! 27 fixed arrays; 28 mutable references; 29 closures/function values;
//! 30 tasks; 31 channels; 32 array growth/slice/freeze; 33 GC retention;
//! 34 debug overflow/release wrap; 35 zero division; 36 signed division
//! overflow; 37 runtime contracts; 38 effect contracts/proofs;
//! 39 type query; 40 deterministic generated overflow-call counts.
use super::support::*;
use std::fmt::Write as _;

const NEW_INTEGERS: &[(&str, u32, bool)] = &[
    ("i8", 8, true),
    ("i16", 16, true),
    ("i32", 32, true),
    ("u16", 16, false),
    ("u32", 32, false),
    ("u64", 64, false),
];
const ALL_INTEGERS: &[(&str, u32, bool)] = &[
    ("i8", 8, true),
    ("i16", 16, true),
    ("i32", 32, true),
    ("i64", 64, true),
    ("u8", 8, false),
    ("u16", 16, false),
    ("u32", 32, false),
    ("u64", 64, false),
];

fn bounds(bits: u32, signed: bool) -> (i128, i128) {
    if signed {
        (-(1_i128 << (bits - 1)), (1_i128 << (bits - 1)) - 1)
    } else {
        (0, (1_i128 << bits) - 1)
    }
}

fn run(source: &str, release: bool) -> String {
    let (out, ok) = if release {
        compile_and_run_release(source)
    } else {
        compile_and_run(source)
    };
    assert!(ok, "{out}\nsource:\n{source}");
    out
}

fn both_modes(source: &str, expected: &str) {
    for release in [false, true] {
        assert_eq!(run(source, release), expected, "release={release}");
    }
}

fn reject(source: &str) {
    let path = temp_path(format!("scalar_reject_{}.wi", unique_test_id()));
    fs::write(&path, source).unwrap();
    let out = Command::new(env!("CARGO_BIN_EXE_willow"))
        .args(["check", &path])
        .output()
        .unwrap();
    let _ = fs::remove_file(path);
    assert!(!out.status.success(), "accepted: {source}");
    let errors = String::from_utf8_lossy(&out.stderr);
    assert!(!errors.contains("internal compiler error"), "{errors}");
}

#[test]
fn scalar_widths_runnable_example() {
    both_modes(
        include_str!("../../example/scalar_widths.wi"),
        "-128\n-32768\n-2147483648\n65535\n4294967295\n18446744073709551615\n-128\ntrue\n-1\n65535\n18446744073709551615\n🌲\n127794\ntrue\ntrue\ntree\n",
    );
}

#[test]
fn scalar_widths_negative_literal_context() {
    let mut source = String::from("fn identity_i64(x: i64) -> i64 { return x; }\n");
    let mut main = String::from("fn main() {\n");
    let mut expected = String::new();
    for &(ty, bits, signed) in ALL_INTEGERS {
        if !signed {
            continue;
        }
        let (min, _) = bounds(bits, signed);
        writeln!(
            source,
            "@effects(no_panic) @pure @verified fn compare_{ty}(x: {ty}) -> bool {{ return -1 < x; }}\n@effects(no_panic) @pure @verified fn compare_min_{ty}(x: {ty}) -> bool {{ return {min} < x; }}"
        )
        .unwrap();
        writeln!(
            main,
            r#"if true {{
    let x: {ty} = 1;
    println(-1 + x);
    println(x + -1);
    println(-1 < x);
    println(x > -1);
    println(-1 == x);
    println(x == -1);
    println({min} + x);
    println(x + {min});
    println({min} < x);
    println(x > {min});
    let negatives: {ty} = -1 + -2;
    println(negatives);
    println(compare_{ty}(x));
    println(compare_min_{ty}(x));
}}"#,
        )
        .unwrap();
        writeln!(
            expected,
            "0\n0\ntrue\ntrue\nfalse\nfalse\n{}\n{}\ntrue\ntrue\n-3\ntrue\ntrue",
            min + 1,
            min + 1
        )
        .unwrap();
    }
    main.push_str("let defaults = -1 + -2; println(identity_i64(defaults)); }\n");
    source.push_str(&main);
    expected.push_str("-3\n");
    both_modes(&source, &expected);
    for expression in ["-1 + x", "x + -1", "-1 < x", "x > -1"] {
        reject(&format!(
            "fn main() {{ let x: f64 = 1.0; println({expression}); }}"
        ));
    }
}

#[test]
fn scalar_widths_arithmetic_and_explicit_overflow() {
    let mut functions = String::new();
    let mut main = String::from("fn main() {\n");
    let mut expected = String::new();
    for &(ty, bits, signed) in NEW_INTEGERS {
        let (min, max) = bounds(bits, signed);
        writeln!(
            functions,
            r#"fn probe_{ty}(x: {ty}, y: {ty}, low: {ty}) {{
 println(x.wrapping_add(y)); println(x.wrapping_sub(y));
 println(x.wrapping_mul(y)); println(x.wrapping_neg());
 println(x.checked_add(y).is_none()); println(x.checked_sub(y).unwrap());
 println(x.checked_mul(y).is_none()); println(low.checked_neg().is_none());
 println(x > y); println(x >= x); println(low < y); println(low <= low);
 println(x == y); println(x != y); println(x / y); println(x % y);
 println(x & y); println(x | y); println(x ^ y); println(!x);
 println(x << 1); println(x >> 1); println(low >> 1);
 println(y + 1); println(y * 3); println(y ** 3);
 println(x.to_string()); println("value=" + x.to_string());
}}"#
        )
        .unwrap();
        writeln!(main, "probe_{ty}({max}, 2, {min});").unwrap();
        let add = if signed { min + 1 } else { 1 };
        let mul = if signed { -2 } else { max - 1 };
        let neg = if signed { -max } else { 1 };
        let complement = if signed { min } else { 0 };
        writeln!(
            expected,
            "{add}\n{}\n{mul}\n{neg}\ntrue\n{}\ntrue\n{signed}",
            max - 2,
            max - 2
        )
        .unwrap();
        writeln!(
            expected,
            "true\ntrue\ntrue\ntrue\nfalse\ntrue\n{}\n1\n2\n{max}\n{}\n{complement}",
            max / 2,
            max - 2
        )
        .unwrap();
        writeln!(
            expected,
            "{mul}\n{}\n{}\n3\n6\n8\n{max}\nvalue={max}",
            max / 2,
            min / 2
        )
        .unwrap();
    }
    main.push('}');
    both_modes(&(functions + &main), &expected);
}

#[test]
fn scalar_widths_all_integer_conversion_pairs_and_decimal_parsing() {
    let mut source = String::from("fn main() {\n");
    let mut expected = String::new();
    for &(from, bits, signed) in ALL_INTEGERS {
        let (min, max) = bounds(bits, signed);
        writeln!(
            source,
            "let max_{from}: {from} = {max}; let min_{from}: {from} = {min};"
        )
        .unwrap();
        for &(to, target_bits, target_signed) in ALL_INTEGERS {
            let (target_min, target_max) = bounds(target_bits, target_signed);
            for (name, value) in [("max", max), ("min", min)] {
                writeln!(source, "println({name}_{from}.checked_to_{to}().is_some()); println({name}_{from}.wrapping_to_{to}());").unwrap();
                let mut wrapped = value.rem_euclid(1_i128 << target_bits);
                if target_signed && wrapped > target_max {
                    wrapped -= 1_i128 << target_bits;
                }
                writeln!(
                    expected,
                    "{}\n{wrapped}",
                    (target_min..=target_max).contains(&value)
                )
                .unwrap();
            }
            if target_min <= min && target_max >= max {
                writeln!(
                    source,
                    "println(max_{from}.to_{to}()); println(min_{from}.to_{to}());"
                )
                .unwrap();
                writeln!(expected, "{max}\n{min}").unwrap();
            }
        }
        writeln!(source, "println(\"{max}\".parse_{from}().unwrap()); println(\"{min}\".parse_{from}().unwrap());").unwrap();
        writeln!(
            source,
            "println(\"{}\".parse_{from}().is_none()); println(\" 1\".parse_{from}().is_none());",
            max + 1
        )
        .unwrap();
        writeln!(expected, "{max}\n{min}\ntrue\ntrue").unwrap();
    }
    source.push('}');
    both_modes(&source, &expected);
}

#[test]
fn scalar_widths_char_literals_comparisons_and_checked_construction() {
    let source = r#"
const TREE: char = '🌲';
fn same(x: char) -> char { return x; }
fn main() {
 let ascii: char = 'A'; let last: char = '\u{10FFFF}';
 println(ascii); println(same(TREE)); println(last.to_u32()); println(TREE.to_i32());
 println('\0'.to_u32()); println('\n'.to_u32()); println('\''.to_u32());
 println('\\'.to_u32()); println(ascii < TREE); println(TREE <= last);
 println(last > TREE); println(ascii >= ascii); println(ascii == TREE); println(ascii != TREE);
 println("🌲".parse_char().unwrap() == TREE); println("".parse_char().is_none());
 println("ab".parse_char().is_none()); println("é".parse_char().is_none());
 println(0.checked_to_char().unwrap().to_u32());
 println(55295.checked_to_char().unwrap().to_u32());
 println(57344.checked_to_char().unwrap().to_u32());
 println(1114111.checked_to_char().unwrap().to_u32());
 println(55296.checked_to_char().is_none()); println(57343.checked_to_char().is_none());
 println(1114112.checked_to_char().is_none()); println((-1).checked_to_char().is_none());
 let large: u64 = 18446744073709551615; println(large.checked_to_char().is_none());
 println(ascii.checked_to_i8().unwrap()); println(TREE.checked_to_u8().is_none());
 println(TREE.to_string()); println("tree=" + TREE.to_string());
 println(match TREE { '🌲' => "matched", _ => "other" });
}
"#;
    both_modes(
        source,
        "A\n🌲\n1114111\n127794\n0\n10\n39\n92\ntrue\ntrue\ntrue\ntrue\nfalse\ntrue\ntrue\ntrue\ntrue\ntrue\n0\n55295\n57344\n1114111\ntrue\ntrue\ntrue\ntrue\ntrue\n65\ntrue\n🌲\ntree=🌲\nmatched\n",
    );
}

#[test]
fn scalar_widths_generic_storage_arrays_references_tasks_and_channels() {
    let mut source = String::from(
        "import std::collections::Array; fn identity<T>(x: T) -> T { return x; } class Cell<T> { pub value: T; }\n",
    );
    let mut main = String::from("async fn main() { println(identity<bool>(true));\n");
    let mut expected = String::from("true\n");
    let mut cases: Vec<_> = NEW_INTEGERS
        .iter()
        .map(|&(ty, bits, signed)| {
            let (min, max) = bounds(bits, signed);
            (
                ty,
                min.to_string(),
                max.to_string(),
                min.to_string(),
                max.to_string(),
            )
        })
        .collect();
    cases.push(("char", "'A'".into(), "'🌲'".into(), "A".into(), "🌲".into()));
    for (ty, low, high, low_text, high_text) in cases {
        writeln!(source, "fn pass_{ty}(x: {ty}) -> {ty} {{ return x; }} fn set_{ty}(x: &mut {ty}) {{ x = {high}; }} async fn task_{ty}(x: {ty}) -> {ty} {{ await sleep(1); return x; }}").unwrap();
        writeln!(
            main,
            r#"if true {{
 let value: {ty} = {high}; println(identity<{ty}>(value));
 let cell = new Cell<{ty}>(value); println(cell.value);
 let some: Option<{ty}> = Some(value); println(some.unwrap());
 let result: Result<{ty}, {ty}> = Err({low}); println(result.unwrap_err());
 let values: Array<{ty}> = [{low}, value]; values.push(value);
 println(values.len()); println(values.pop());
 let sliced = values.slice(0, 2); println(sliced[0]); println(sliced[1]);
 set_{ty}(&values[0]); println(values[0]); println(values[1]);
 let frozen = values.freeze(); println(frozen[0]);
 let fixed: [{ty}; 2] = [{low}, {high}]; println(fixed[1]);
 let captured = || value; println(captured());
 let callable = pass_{ty}; println(callable(value));
 let mut mutable: {ty} = {low}; set_{ty}(&mutable); println(mutable);
 let channel = Channel<{ty}>::with_capacity(1); channel.send(value); println(channel.recv());
 println(await task_{ty}(value)); gc_collect(); println(values[1]);
 println(match value {{ {high} => true, _ => false }});
}}"#
        )
        .unwrap();
        writeln!(expected, "{high_text}\n{high_text}\n{high_text}\n{low_text}\n3\n{high_text}\n{low_text}\n{high_text}").unwrap();
        for _ in 0..10 {
            writeln!(expected, "{high_text}").unwrap();
        }
        expected.push_str("true\n");
    }
    main.push('}');
    both_modes(&(source + &main), &expected);
}

#[test]
fn scalar_widths_async_select_scalar_transport() {
    let mut cases: Vec<_> = NEW_INTEGERS
        .iter()
        .map(|&(ty, bits, signed)| {
            let (min, max) = bounds(bits, signed);
            let value = if signed { min } else { max }.to_string();
            (ty, value.clone(), value)
        })
        .collect();
    cases.push(("u8", "255".into(), "255".into()));
    cases.push(("char", "'\\u{10FFFF}'".into(), "\u{10FFFF}".into()));
    let mut source = String::from("async fn main() {\n");
    let mut expected = String::new();
    for (ty, literal, display) in cases {
        writeln!(
            source,
            r#"if true {{
 let value: {ty} = {literal};
 let channel = Channel<{ty}>::with_capacity(1);
 channel.send(value);
 println(channel.recv());
 select {{ channel.send(value) => {{}} }}
 select {{ let received = channel.recv() => {{ println(received); }} }}
}}"#
        )
        .unwrap();
        writeln!(expected, "{display}\n{display}").unwrap();
    }
    source.push('}');
    both_modes(&source, &expected);
}

#[test]
fn scalar_widths_debug_recovery_and_release_wrapping() {
    let mut source = String::new();
    let mut main = String::from("fn main() {\n");
    let mut debug = String::new();
    let mut release = String::new();
    for &(ty, bits, signed) in NEW_INTEGERS {
        let (min, max) = bounds(bits, signed);
        for (name, expr) in [
            ("add", "a+b"),
            ("sub", "a-b"),
            ("mul", "a*b"),
            ("pow", "a**b"),
            ("shl", "a<<b"),
            ("shr", "a>>b"),
            ("div", "a/b"),
            ("rem", "a%b"),
        ] {
            writeln!(
                source,
                "fn {name}_{ty}(a: {ty}, b: {ty}) -> {ty} {{ return {expr}; }}"
            )
            .unwrap();
        }
        writeln!(source, "fn neg_{ty}(a: {ty}) -> {ty} {{ return -a; }}").unwrap();
        let mut cases = vec![
            (format!("add_{ty}({max}, 1)"), min.to_string()),
            (format!("sub_{ty}({min}, 1)"), max.to_string()),
            (
                format!("mul_{ty}({max}, 2)"),
                if signed {
                    "-2".into()
                } else {
                    (max - 1).to_string()
                },
            ),
            (
                format!("neg_{ty}({})", if signed { min } else { 1 }),
                if signed {
                    min.to_string()
                } else {
                    max.to_string()
                },
            ),
            (format!("pow_{ty}(2, {bits})"), "0".into()),
            (format!("shl_{ty}(1, {bits})"), "1".into()),
            (format!("shr_{ty}(1, {bits})"), "1".into()),
            (format!("div_{ty}(1, 0)"), "panic".into()),
            (format!("rem_{ty}(1, 0)"), "panic".into()),
        ];
        if signed {
            cases.push((format!("div_{ty}({min}, -1)"), "panic".into()));
            cases.push((format!("rem_{ty}({min}, -1)"), "panic".into()));
        }
        for (expr, value) in cases {
            writeln!(main,"if true {{ defer match recover() {{ Some(info) => println(\"panic\"), None => {{}} }} println({expr}); }}").unwrap();
            debug.push_str("panic\n");
            writeln!(release, "{value}").unwrap();
        }
        writeln!(main,"if true {{ defer match recover() {{ Some(info) => println(\"panic\"), None => {{}} }} let mut x: {ty} = {max}; x += 1; println(x); }}").unwrap();
        debug.push_str("panic\n");
        writeln!(release, "{min}").unwrap();
    }
    main.push_str("println(\"end\"); }");
    debug.push_str("end\n");
    release.push_str("end\n");
    source.push_str(&main);
    assert_eq!(run(&source, false), debug);
    assert_eq!(run(&source, true), release);
}

#[test]
fn scalar_widths_signed_minimum_remainder_overflow_constant_and_dynamic() {
    let mut source = String::new();
    let mut main = String::from("fn main() {\n");
    for &(ty, bits, signed) in ALL_INTEGERS.iter().filter(|(_, _, signed)| *signed) {
        let (min, _) = bounds(bits, signed);
        writeln!(
            source,
            "fn remainder_{ty}(x: {ty}, y: {ty}) -> {ty} {{ return x % y; }}"
        )
        .unwrap();
        for statement in [
            format!("let value: {ty} = {min} % -1; println(value);"),
            format!("println(remainder_{ty}({min}, -1));"),
        ] {
            writeln!(main, "if true {{ defer match recover() {{ Some(info) => println(\"panic\"), None => {{}} }} {statement} }}").unwrap();
        }
    }
    main.push_str("println(\"end\"); }");
    both_modes(&(source + &main), &("panic\n".repeat(8) + "end\n"));
}

#[test]
fn scalar_widths_runtime_and_effect_contracts() {
    let mut source = String::new();
    let mut main = String::from("fn main() {\n");
    for &(ty, bits, signed) in NEW_INTEGERS {
        let (_, max) = bounds(bits, signed);
        writeln!(source,"fn contract_{ty}(x: {ty}) requires (x + 1) == 0 {{}}\n@effects(no_panic, no_alloc) fn wrap_{ty}(x: {ty}) -> {ty} {{ return x.wrapping_add(1); }}\n@effects(no_panic, no_alloc) fn parse_{ty}(x: String) -> Option<{ty}> {{ return x.parse_{ty}(); }}").unwrap();
        writeln!(main,"if true {{ defer match recover() {{ Some(info) => println(\"panic\"), None => {{}} }} contract_{ty}({max}); }} println(wrap_{ty}(0)); println(parse_{ty}(\"1\").unwrap());").unwrap();
        reject(&format!(
            "@effects(no_panic) fn unsafe_{ty}(x: {ty}) -> {ty} {{ return x + 1; }} fn main() {{}}"
        ));
    }
    source.push_str("@effects(no_panic) @pure @verified fn step(x: u64) -> u64 requires x < 18446744073709551615 { return x + 1; }\n");
    main.push_str("println(step(18446744073709551614)); }");
    both_modes(
        &(source + &main),
        &("panic\n1\n1\n".repeat(NEW_INTEGERS.len()) + "18446744073709551615\n"),
    );
}

#[test]
fn scalar_widths_reject_ranges_mixing_and_invalid_char_operations() {
    for &(ty, bits, signed) in NEW_INTEGERS {
        let (min, max) = bounds(bits, signed);
        for body in [
            format!("let x: {ty} = {};", max + 1),
            format!("let x: {ty} = {};", min - 1),
            format!("let x: {ty} = 1; let y: i64 = x;"),
            format!("let x: {ty} = 1; let y: i64 = 1; println(x + y);"),
            format!(
                "let x: {ty} = 1; match x {{ {} => {{}}, _ => {{}} }}",
                max + 1
            ),
        ] {
            reject(&format!("fn main() {{ {body} }}"));
        }
    }
    for body in [
        "let x: char = '';",
        "let x: char = 'ab';",
        r"let x: char = '\u{D800}';",
        r"let x: char = '\u{110000}';",
        "let x: char = 65;",
        "let x: u32 = 'A';",
        "println('a' + 'b');",
        "println(-'a');",
        "println('a'.wrapping_add('b'));",
        "println(65.wrapping_to_char());",
        "let x: i32 = 1; println(x.to_i8());",
        "let x: u64 = 18446744073709551616;",
        "let x = 9223372036854775808;",
    ] {
        reject(&format!("fn main() {{ {body} }}"));
    }
}

#[test]
fn scalar_widths_codegen_counts_depend_on_operations_not_available_widths() {
    for sites in [1, 4, 16] {
        for used_types in [1, 3, 6] {
            let mut source = String::new();
            let mut main = String::from("fn main() {\n");
            for (index, &(ty, _, _)) in NEW_INTEGERS.iter().take(used_types).enumerate() {
                writeln!(source, "fn chain_{ty}(x: {ty}) -> {ty} {{ let mut y = x;").unwrap();
                for _ in (index..sites).step_by(used_types) {
                    source.push_str("y += 1;\n");
                }
                source.push_str("return y; }\n");
                writeln!(main, "println(chain_{ty}(0));").unwrap();
            }
            main.push('}');
            source.push_str(&main);
            let symbols = compile_and_collect_relocation_targets_all(&source, &[]);
            let checks = symbols
                .iter()
                .filter(|name| name.ends_with("willow_int_overflow_panic"))
                .count();
            assert_eq!(checks, sites, "sites={sites} used_types={used_types}");
            println!("sites={sites} used_types={used_types} overflow_calls={checks}");
        }
    }
}

#[test]
fn scalar_widths_ai_signature_names() {
    let path = temp_path(format!("scalar_query_{}.wi", unique_test_id()));
    fs::write(&path,"fn identity(a: i8, b: i16, c: i32, d: u16, e: u32, f: u64, g: char) -> char { return g; } fn main() {}").unwrap();
    let out = Command::new(env!("CARGO_BIN_EXE_willow"))
        .args(["type", "identity", "--source", &path, "--format", "json"])
        .output()
        .unwrap();
    let _ = fs::remove_file(path);
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let value: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(
        value["result"]["type_display"],
        "fn(i8, i16, i32, u16, u32, u64, char) -> char"
    );
}

#[test]
fn scalar_widths_collection_snapshots_and_display() {
    let mut cases = Vec::new();
    for &(ty, bits, signed) in ALL_INTEGERS {
        let (low, high) = bounds(bits, signed);
        let middle = if signed { -1 } else { 1_i128 << (bits - 1) };
        cases.push((
            ty,
            low.to_string(),
            middle.to_string(),
            high.to_string(),
            low.to_string(),
            middle.to_string(),
            high.to_string(),
        ));
    }
    cases.push((
        "char",
        "'A'".into(),
        "'é'".into(),
        "'🌲'".into(),
        "A".into(),
        "é".into(),
        "🌲".into(),
    ));
    let mut source = String::from(
        "import std::collections::{Array, Map};\nimport std::set::HashSet;\nimport std::sorted_map::SortedMap;\nfn main() {\n",
    );
    let mut expected = String::new();
    for (ty, low, middle, high, low_text, middle_text, high_text) in cases {
        writeln!(
            source,
            r#"
        if true {{
            println("{ty}");
            let low: {ty} = {low}; let middle: {ty} = {middle}; let high: {ty} = {high};
            let map: Map<{ty}, {ty}> = Map::new();
            map.insert(high, low); map.insert(low, high); map.insert(middle, middle);
            println(map.get(low).unwrap() == high && map.get(high).unwrap() == low);
            println(map.to_string());
            let keys = map.keys(); let values = map.values(); let entries = map.entries();
            println(keys.to_string()); println(values.to_string());
            println(keys.len() == 3 && keys[0] == low && keys[1] == middle && keys[2] == high
                && values[0] == high && values[1] == middle && values[2] == low);
            let (k0, v0) = entries[0]; let (k1, v1) = entries[1]; let (k2, v2) = entries[2];
            println(entries.len() == 3 && k0 == low && v0 == high && k1 == middle
                && v1 == middle && k2 == high && v2 == low);
            let frozen = map.freeze();
            println(frozen.get(low).unwrap() == high && frozen.get(high).unwrap() == low);
            println(map.remove(low).unwrap() == high && map.remove(high).unwrap() == low
                && map.remove(middle).unwrap() == middle);
            println(map.len() == 0); gc_collect();
            let (saved_key, saved_value) = entries[2];
            println(keys[0] == low && values[2] == low && saved_key == high && saved_value == low);
            println(frozen.len() == 3 && frozen.get(low).unwrap() == high
                && frozen.get(middle).unwrap() == middle && frozen.get(high).unwrap() == low);
            let array: Array<{ty}> = [high, low, middle]; array.sort();
            println(array.to_string());
            let frozen_array = array.freeze();
            println(frozen_array[0] == low && frozen_array[1] == middle && frozen_array[2] == high);
            let set: HashSet<{ty}> = new HashSet();
            println(set.insert(high) && set.insert(low) && set.insert(middle) && !set.insert(high));
            println(set.to_string());
            println(set.remove(low) && !set.contains(low) && !set.remove(low));
            println(set.to_string());
            let sorted: SortedMap<{ty}, {ty}> = new SortedMap();
            sorted.insert(high, low); sorted.insert(low, high); sorted.insert(middle, middle);
            println(sorted.keys().to_string());
            println(sorted.get(low).unwrap() == high && sorted.get(high).unwrap() == low
                && sorted.first_key().unwrap() == low && sorted.last_key().unwrap() == high);
            println(sorted.remove(middle).unwrap() == middle);
            println(sorted.keys().to_string());
        }}
        "#
        )
        .unwrap();
        let ascending = format!("[{low_text}, {middle_text}, {high_text}]");
        let descending = format!("[{high_text}, {middle_text}, {low_text}]");
        // Map display orders rendered keys; snapshots use scalar ordering.
        let mut map_entries = [
            (&low_text, &high_text),
            (&middle_text, &middle_text),
            (&high_text, &low_text),
        ];
        map_entries.sort_unstable_by(|a, b| a.0.cmp(b.0));
        let map_text = format!(
            "{{{}}}",
            map_entries
                .map(|(key, value)| format!("{key}: {value}"))
                .join(", ")
        );
        for line in [
            ty,
            "true",
            &map_text,
            &ascending,
            &descending,
            "true",
            "true",
            "true",
            "true",
            "true",
            "true",
            "true",
            &ascending,
            "true",
            "true",
            &format!("{{{low_text}, {middle_text}, {high_text}}}"),
            "true",
            &format!("{{{middle_text}, {high_text}}}"),
            &ascending,
            "true",
            "true",
            &format!("[{low_text}, {high_text}]"),
        ] {
            writeln!(expected, "{line}").unwrap();
        }
    }
    source.push_str("}\n");
    both_modes(&source, &expected);
}
