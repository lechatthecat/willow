//! Explicit perspectives: annotations; function parameters/returns; fields;
//! generic functions/classes; Option; Result; arrays; constants; match;
//! literal range; negative literals; implicit mixing; widening; truncation;
//! checked conversions; unsigned comparisons; add/sub/mul; div/rem; negation;
//! bitwise/complement; shifts; power; compound assignment; recovery; wrapping
//! methods; checked methods; closure capture; task/await; channel; display;
//! debug/release parity; no_panic; proof bounds; AI type output; native I8 ABI.
use super::support::*;

fn run(source: &str, release: bool) -> String {
    let (out, ok) = if release {
        compile_and_run_release(source)
    } else {
        compile_and_run(source)
    };
    assert!(ok, "{out}");
    out
}

#[test]
fn u8_scalar_representations_and_boundaries() {
    let source = include_str!("../../example/u8_scalar.wi");
    for release in [false, true] {
        assert_eq!(
            run(source, release),
            "255\n255\n255\n255\n0\ntrue\n0\ntrue\ntrue\n127\nmax\n"
        );
    }
}

#[test]
fn u8_scalar_conversion_and_explicit_arithmetic() {
    let source = r#"
fn probe(x: u8, y: u8) {
 println(x.wrapping_add(y)); println(x.wrapping_sub(y));
 println(x.wrapping_mul(y)); println(x.wrapping_neg());
 println(x.checked_add(y).is_none()); println(x.checked_sub(y).unwrap());
 println(x.checked_mul(y).is_none()); println(x.checked_neg().is_none());
 println(x > y); println(x >= y); println(y < x); println(y <= x);
 println(x == y); println(x != y); println(x / y); println(x % y);
 println(x & y); println(x | y); println(x ^ y); println(!x);
 println(x << 1); println(x >> 1); println(x << 0); println(x >> 7);
 println(x.to_string()); println("byte=" + x.to_string());
}
fn main() {
 probe(255, 2);
 println((-1).wrapping_to_u8()); println(256.wrapping_to_u8());
 println((-1).checked_to_u8().is_none()); println(256.checked_to_u8().is_none());
 println(255.checked_to_u8().unwrap().to_i64());
 let zero: u8 = 0;
 println(zero.checked_neg().unwrap()); println(zero.wrapping_neg());
 println(zero.checked_sub(1).is_none()); println(zero.checked_add(255).unwrap());
 println(zero.checked_mul(255).unwrap());
 let channel = Channel<u8>::with_capacity(1); channel.send(200); println(channel.recv());
}
"#;
    let expected = "1\n253\n254\n1\ntrue\n253\ntrue\ntrue\ntrue\ntrue\ntrue\ntrue\nfalse\ntrue\n127\n1\n2\n255\n253\n0\n254\n127\n255\n1\n255\nbyte=255\n255\n0\ntrue\ntrue\n255\n0\n0\ntrue\n255\n0\n200\n";
    for release in [false, true] {
        assert_eq!(run(source, release), expected);
    }
}

#[test]
fn u8_scalar_overflow_recovery_and_release_wrap() {
    let mut source = String::from(
        r#"
fn add(a: u8, b: u8) -> u8 { return a + b; }
fn sub(a: u8, b: u8) -> u8 { return a - b; }
fn mul(a: u8, b: u8) -> u8 { return a * b; }
fn neg(a: u8) -> u8 { return -a; }
fn pow(a: u8, b: u8) -> u8 { return a ** b; }
fn shl(a: u8, b: u8) -> u8 { return a << b; }
fn shr(a: u8, b: u8) -> u8 { return a >> b; }
fn div(a: u8, b: u8) -> u8 { return a / b; }
fn rem(a: u8, b: u8) -> u8 { return a % b; }
fn main() {
"#,
    );
    let cases = [
        ("println(add(255, 1));", "0"),
        ("println(sub(0, 1));", "255"),
        ("println(mul(128, 2));", "0"),
        ("println(neg(1));", "255"),
        ("println(pow(2, 8));", "0"),
        ("let x: u8 = 2 ** 8; println(x);", "0"),
        ("println(shl(1, 8));", "1"),
        ("println(shr(255, 255));", "1"),
        ("let mut x: u8 = 255; x += 1; println(x);", "0"),
        ("let mut x: u8 = 0; x -= 1; println(x);", "255"),
        ("let mut x: u8 = 128; x *= 2; println(x);", "0"),
        ("println(div(1, 0));", "panic"),
        ("println(rem(1, 0));", "panic"),
    ];
    for (statement, _) in cases {
        source.push_str(
            "if true { defer match recover() { Some(info) => println(\"panic\"), None => {} } ",
        );
        source.push_str(statement);
        source.push_str(" }\n");
    }
    source.push_str("println(\"end\"); }");
    assert_eq!(run(&source, false), "panic\n".repeat(cases.len()) + "end\n");
    let expected = cases
        .iter()
        .map(|(_, value)| format!("{value}\n"))
        .collect::<String>()
        + "end\n";
    assert_eq!(run(&source, true), expected);
}

#[test]
fn u8_scalar_rejects_invalid_literals_and_mixing() {
    for body in [
        "let x: u8 = 256;",
        "let x: u8 = -1;",
        "let x: u8 = -256;",
        "let x: u8 = 0 + -1;",
        "let x: u8 = 1; let y: i64 = x;",
        "let y: i64 = 1; let x: u8 = y;",
        "let x: u8 = 1; let y: i64 = 1; println(x + y);",
        "let x: u8 = 1; let y: i64 = 1; println(x < y);",
        "let x: u8 = 1; println(x.wrapping_add(256));",
        "let x: u8 = 1; match x { 256 => {}, _ => {} }",
    ] {
        let path = temp_path(format!("u8_reject_{}.wi", unique_test_id()));
        fs::write(&path, format!("fn main() {{ {body} }}")).unwrap();
        let out = Command::new(env!("CARGO_BIN_EXE_willow"))
            .args(["check", &path])
            .output()
            .unwrap();
        let _ = fs::remove_file(path);
        assert!(!out.status.success(), "accepted: {body}");
        assert!(!String::from_utf8_lossy(&out.stderr).contains("internal compiler error"));
    }
}

#[test]
fn u8_scalar_effect_contracts_and_proof_bounds() {
    let explicit = r#"
@effects(no_panic, no_alloc)
fn add(a: u8, b: u8) -> u8 { return a.wrapping_add(b); }
@effects(no_panic, no_alloc)
fn narrow(a: i64) -> Option<u8> { return a.checked_to_u8(); }
@effects(no_panic)
fn shift(a: u8) -> u8 { return a >> 7; }
@effects(no_panic)
@pure
@verified
fn inc(x: u8) -> u8 requires x < 255 { return x + 1; }
fn main() { println(add(255, 1)); println(narrow(128).unwrap()); println(shift(255)); println(inc(254)); }
"#;
    for release in [false, true] {
        assert_eq!(run(explicit, release), "0\n128\n1\n255\n");
    }
    for body in ["return x + 1;", "return x << 8;"] {
        let path = temp_path(format!("u8_effects_{}.wi", unique_test_id()));
        fs::write(
            &path,
            format!("@effects(no_panic) fn f(x: u8) -> u8 {{ {body} }} fn main() {{}} "),
        )
        .unwrap();
        let out = Command::new(env!("CARGO_BIN_EXE_willow"))
            .args(["check", &path])
            .output()
            .unwrap();
        let _ = fs::remove_file(path);
        assert!(!out.status.success(), "accepted unsafe no_panic: {body}");
    }
}

#[test]
fn u8_scalar_overflow_codegen_scales_with_operation_sites() {
    for sites in [1, 4, 16, 64] {
        let mut source = String::from("fn chain(x: u8) -> u8 { let mut y = x;\n");
        for _ in 0..sites {
            source.push_str("y += 1;\n");
        }
        source.push_str("return y; } fn main() { println(chain(0)); }");
        let symbols = compile_and_collect_relocation_targets_all(&source, &[]);
        let checks = symbols
            .iter()
            .filter(|name| name.ends_with("willow_int_overflow_panic"))
            .count();
        assert_eq!(checks, sites, "overflow guard count at {sites} sites");
        println!("u8 sites={sites} overflow_guard_calls={checks}");
    }
}

#[test]
fn u8_scalar_generic_identity_and_mutable_storage() {
    let source = r#"
import std::collections::Array;
fn identity<T>(value: T) -> T { return value; }
class Box<T> { pub value: T; }
fn apply(f: fn(u8) -> u8, x: u8) -> u8 { return f(x); }
fn pass(x: u8) -> u8 { return x; }
fn set(x: &mut u8) { x = 255; }
fn main() {
 println(identity<bool>(true)); println(identity<u8>(200));
 let byte = new Box<u8>(128); let truth = new Box<bool>(true);
 byte.value += 1; println(byte.value); println(truth.value);
 let values: Array<u8> = [128, 255]; values[0] += 1; println(values[0]);
 values.push(200); println(values[2]); println(values.to_string());
 let some: Option<u8> = Some(255); println(some.unwrap());
 let result: Result<u8, u8> = Err(200); println(result.unwrap_err());
 let fixed: [u8; 2] = [128, 255]; println(fixed[1]);
 println(apply(pass, 200)); println(apply(|v: u8| v >> 1, 200));
 let mut mutable: u8 = 128; set(&mutable); println(mutable);
}
"#;
    for release in [false, true] {
        assert_eq!(
            run(source, release),
            "true\n200\n129\ntrue\n129\n200\n[129, 255, 200]\n255\n200\n255\n200\n100\n255\n"
        );
    }
}

#[test]
fn u8_scalar_standard_collections() {
    let source = r#"
import std::collections::Map;
import std::set::HashSet;
import std::sorted_map::SortedMap;
fn main() {
 let map: Map<u8, u8> = Map::new(); map.insert(255, 128); println(map.get(255).unwrap()); println(map.freeze().get(255).unwrap());
 let set: HashSet<u8> = new HashSet(); println(set.insert(255)); println(set.contains(255)); println(set.remove(255));
 let sorted: SortedMap<u8, u8> = new SortedMap();
 sorted.insert(255, 200); sorted.insert(0, 128); sorted.insert(128, 255);
 println(sorted.keys().to_string()); println(sorted.get(255).unwrap());
 println(sorted.remove(128).unwrap()); println(sorted.keys().to_string());
}
"#;
    for release in [false, true] {
        assert_eq!(
            run(source, release),
            "128\n128\ntrue\ntrue\ntrue\n[0, 128, 255]\n200\n255\n[0, 255]\n"
        );
    }
}

#[test]
fn u8_scalar_ai_type_output() {
    let path = temp_path(format!("u8_type_{}.wi", unique_test_id()));
    fs::write(&path, "fn identity(x: u8) -> u8 { return x; } fn main() {}").unwrap();
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
    assert_eq!(value["result"]["type_display"], "fn(u8) -> u8");
}

#[test]
fn u8_scalar_runtime_contract_arithmetic() {
    let mut source = String::new();
    let expressions = [
        "x + 1", "0 - x", "x * 2", "-x", "x ** 2", "x << 8", "x >> 8", "x / 0", "x % 0",
    ];
    for (i, expr) in expressions.iter().enumerate() {
        source.push_str(&format!("fn c{i}(x: u8) requires ({expr}) == 0 {{}}\n"));
    }
    source.push_str("fn shift(x: u8) requires (x << 1) == 254 {} fn main() { shift(255);\n");
    for i in 0..expressions.len() {
        source.push_str(&format!("if true {{ defer match recover() {{ Some(info) => println(\"panic\"), None => {{}} }} c{i}(255); }}\n"));
    }
    source.push_str("println(\"end\"); }");
    for release in [false, true] {
        assert_eq!(
            run(&source, release),
            "panic\n".repeat(expressions.len()) + "end\n"
        );
    }
}

#[test]
fn u8_scalar_lock_and_task_storage() {
    let source = r#"
async fn read(m: Mutex<u8>) -> u8 { lock m as value { return value; } }
async fn main() {
 let byte: u8 = 200; let m = Mutex::new(byte);
 lock m as mut value { value += 1; }
 println(await read(m));
 let rw = RwLock::new(byte);
 lock write rw as mut value { value += 2; }
 lock read rw as value { println(value); }
}
"#;
    for release in [false, true] {
        assert_eq!(run(source, release), "201\n202\n");
    }
}
