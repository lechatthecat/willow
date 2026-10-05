//! Bitwise and shift operators on `i64` (willow-jz15.8).
//!
//! Precedence, loosest to tightest: `||`, `&&`, comparisons, `|`, `^`, `&`,
//! `<<`/`>>`, `+`/`-`, `*`/`/`/`%`, prefix `-`/`!`, `**`. `!` on `i64` is the
//! bitwise complement. `>>` is arithmetic (sign-extending). A shift amount
//! outside `0..64` panics recoverably in debug builds and is masked to its low
//! six bits in release builds; `<<` never checks for lost high bits.
//!
//! Perspectives:
//!   1  `&`, `|`, `^` on positive, negative and runtime operands
//!   2  `!` on `i64` is the complement; `!` on `bool` is unchanged
//!   3  `<<` reaches the sign bit (`1 << 63 == i64::MIN`) and drops high bits
//!   4  `>>` sign-extends negative values
//!   5  shift amounts 0 and 63 are in range in both modes
//!   6  `x & mask == 0` is `(x & mask) == 0` (bitwise binds tighter than `==`)
//!   7  `1 << n - 1` is `1 << (n - 1)`; `|` < `^` < `&` < shifts
//!   8  compound `&= |= ^= <<= >>=` on locals, fields and array elements
//!   9  debug: an out-of-range amount (64, -1, runtime) panics recoverably
//!  10  release: an out-of-range amount is masked to its low six bits
//!  11  constant-foldable expressions (incl. module consts) give runtime values
//!  12  nested generic `>>` (`Option<Option<i64>>`) still closes types
//!  13  `< <` with a space is not a shift
//!  14  `bool & bool` is rejected with a `&&` hint (also `|` -> `||`, `^` -> `!=`)
//!  15  `f64`, `String` and mixed operands are rejected
//!  16  `!` on `f64` names both accepted operand types
//!  17  `r? & m` / `r? | m` read `?` as try-propagation; a lambda ternary
//!      branch `c ? |x| ... : ...` still parses
//!  18  `&` reference call arguments still work next to binary `&`
//!  19  bitwise operators inside an `async fn` and a class method
//!  20  a xorshift hash matches the same computation in Rust
//!  21  an unused out-of-range shift still panics in debug only
//!  22  a bitset popcount loop
//!  23  an uncaught shift panic exits non-zero and reports `file:line:col`
//!  24  generated code: one raiser call per variable-amount shift in debug,
//!      none for in-range literal amounts or `& | ^ !`, none in release

use std::fs;
use std::process::Command;

use crate::support::*;

/// `(guarded statement, debug output, release output)`. Each statement runs in
/// its own scope with a `recover()` defer.
const SHIFT_CASES: &[(&str, &str, &str)] = &[
    // 9 / 10
    (
        "println(shl(1, 64));",
        "integer overflow: `<<` shift amount outside 0..64",
        "1",
    ),
    (
        "println(shl(1, 0 - 1));",
        "integer overflow: `<<` shift amount outside 0..64",
        "-9223372036854775808",
    ),
    (
        "println(shr(0 - 256, 65));",
        "integer overflow: `>>` shift amount outside 0..64",
        "-128",
    ),
    (
        "println(1 << 64);",
        "integer overflow: `<<` shift amount outside 0..64",
        "1",
    ),
    (
        "let mut x = 3; x <<= 66; println(x);",
        "integer overflow: `<<` shift amount outside 0..64",
        "12",
    ),
    (
        "let mut x = 0 - 8; x >>= 0 - 63; println(x);",
        "integer overflow: `>>` shift amount outside 0..64",
        "-4",
    ),
    // 5
    ("println(shl(5, 0));", "5", "5"),
    ("println(shr(0 - 1, 63));", "-1", "-1"),
    (
        "println(shl(1, 63));",
        "-9223372036854775808",
        "-9223372036854775808",
    ),
];

fn shift_matrix_source() -> String {
    let mut source = String::from(
        "fn shl(a: i64, b: i64) -> i64 { return a << b; }\n\
         fn shr(a: i64, b: i64) -> i64 { return a >> b; }\n\
         fn main() {\n",
    );
    for (statement, _, _) in SHIFT_CASES {
        source.push_str(
            "    if true {\n        \
             defer match recover() { Some(info) => println(info.message), None => {} }\n        ",
        );
        source.push_str(statement);
        source.push_str("\n    }\n");
    }
    source.push_str("    println(\"end\");\n}\n");
    source
}

fn shift_expected(release: bool) -> String {
    let mut out = String::new();
    for (_, debug, masked) in SHIFT_CASES {
        out.push_str(if release { masked } else { debug });
        out.push('\n');
    }
    out.push_str("end\n");
    out
}

fn run_both(source: &str) -> (String, String) {
    let (debug, ok) = compile_and_run(source);
    assert!(ok, "debug run failed: {debug}");
    let (release, ok) = compile_and_run_release(source);
    assert!(ok, "release run failed: {release}");
    (debug, release)
}

#[test]
fn bitwise_01_to_04_values_in_both_modes() {
    let source = r#"
fn main() {
    let a = 12;
    let b = 10;
    let neg = 0 - 6;
    println(a & b);
    println(a | b);
    println(a ^ b);
    println(neg & 255);
    println(neg | 1);
    println(neg ^ neg);
    println(!a);
    println(!neg);
    println(!0);
    println(!true);
    println(1 << 4);
    println(1 << 63);
    println(3 << 62);
    println((0 - 16) >> 2);
    println((0 - 1) >> 40);
    println(9223372036854775807 >> 62);
}
"#;
    let expected = "8\n14\n6\n250\n-5\n0\n-13\n5\n-1\nfalse\n16\n\
                    -9223372036854775808\n-4611686018427387904\n-4\n-1\n1\n";
    let (debug, release) = run_both(source);
    assert_eq!(debug, expected);
    assert_eq!(release, expected);
}

#[test]
fn bitwise_05_09_10_shift_amount_policy() {
    let (debug, release) = run_both(&shift_matrix_source());
    assert_eq!(debug, shift_expected(false));
    assert_eq!(release, shift_expected(true));
}

#[test]
fn bitwise_06_07_precedence() {
    let source = r#"
fn main() {
    let x = 6;
    let n = 4;
    println(x & 2 == 2);
    println(x & 1 != 0);
    println(1 << n - 1);
    println(1 + 2 << 3);
    println(1 | 2 ^ 3 & 6);
    println(8 >> 1 | 1);
    println(-x & 7);
    println(!x & 7);
    println(2 ** 3 << 1);
    println(x > 4 && x & 4 == 4 || false);
}
"#;
    // `1 | (2 ^ (3 & 6))` = 1 | (2 ^ 2) = 1; `(-6) & 7` = 2; `(!6) & 7` = 1.
    let expected = "true\nfalse\n8\n24\n1\n5\n2\n1\n16\ntrue\n";
    let (debug, release) = run_both(source);
    assert_eq!(debug, expected);
    assert_eq!(release, expected);
}

#[test]
fn bitwise_08_compound_assignments() {
    let source = r#"
class Flags {
    pub bits: i64;
}

fn main() {
    let mut m = 1;
    m <<= 10;
    m |= 5;
    m &= 0 - 2;
    m ^= 7;
    m >>= 1;
    println(m);
    let f = new Flags(0);
    f.bits |= 8;
    f.bits <<= 2;
    f.bits ^= 1;
    println(f.bits);
    let mut words = [255, 1];
    words[0] &= 15;
    words[1] <<= 3;
    words[1] >>= 1;
    println(words[0]);
    println(words[1]);
}
"#;
    let expected = "513\n33\n15\n4\n";
    let (debug, release) = run_both(source);
    assert_eq!(debug, expected);
    assert_eq!(release, expected);
}

#[test]
fn bitwise_11_constant_folding_matches_runtime() {
    let source = r#"
const MASK: i64 = 240;
const SHIFT: i64 = 4;

fn id(v: i64) -> i64 { return v; }

fn main() {
    println((MASK >> SHIFT) == (id(MASK) >> id(SHIFT)));
    println((MASK & 48 | 3 ^ 1) == (id(MASK) & id(48) | id(3) ^ id(1)));
    println(!MASK == !id(MASK));
    println((0 - 1) << 63);
    println(MASK >> SHIFT);
}
"#;
    let expected = "true\ntrue\ntrue\n-9223372036854775808\n15\n";
    let (debug, release) = run_both(source);
    assert_eq!(debug, expected);
    assert_eq!(release, expected);
}

#[test]
fn bitwise_12_nested_generic_closers_are_not_shifts() {
    let source = r#"
fn main() {
    let nested: Option<Option<i64>> = Some(Some(16));
    match nested {
        Some(inner) => match inner {
            Some(v) => println(v >> 2),
            None => println(0),
        },
        None => println(0),
    }
    let a = 3;
    let b = 4;
    println(a < b);
    println(b > a);
}
"#;
    let (debug, release) = run_both(source);
    assert_eq!(debug, "4\ntrue\ntrue\n");
    assert_eq!(release, debug);
}

#[test]
fn bitwise_13_spaced_angle_brackets_are_not_a_shift() {
    let stderr = compile_error_stderr("fn main() { let x = 1 < < 2; }\n");
    assert!(stderr.contains("error["), "{stderr}");
    let stderr = compile_error_stderr("fn main() { let x = 8 > > 2; }\n");
    assert!(stderr.contains("error["), "{stderr}");
}

#[test]
fn bitwise_14_bool_operands_suggest_logical_operators() {
    for (op, hint) in [("&", "&&"), ("|", "||"), ("^", "!=")] {
        let stderr = compile_error_stderr(&format!(
            "fn main() {{ let a = true; let b = false; println(a {op} b); }}\n"
        ));
        assert!(stderr.contains("error[E0202]"), "{op}: {stderr}");
        assert!(
            stderr.contains(&format!(
                "cannot apply operator `{op}` to `bool` and `bool`"
            )),
            "{op}: {stderr}"
        );
        assert!(
            stderr.contains(&format!("for `bool` operands, use `{hint}`")),
            "{op}: {stderr}"
        );
    }
    let stderr = compile_error_stderr("fn main() { println(true << 1); }\n");
    assert!(stderr.contains("error[E0202]"), "{stderr}");
    assert!(!stderr.contains("for `bool` operands"), "{stderr}");
}

#[test]
fn bitwise_15_non_integer_operands_are_rejected() {
    for expr in [
        "1.0 & 2.0",
        "1 | 2.0",
        "\"a\" ^ \"b\"",
        "1 << 2.0",
        "1.5 >> 1",
    ] {
        let stderr = compile_error_stderr(&format!("fn main() {{ println({expr}); }}\n"));
        assert!(stderr.contains("error[E0202]"), "{expr}: {stderr}");
        assert!(
            stderr.contains("is defined only for `i64` operands"),
            "{expr}: {stderr}"
        );
    }
    let stderr = compile_error_stderr("fn main() { let mut x = 1.0; x |= 2.0; println(x); }\n");
    assert!(stderr.contains("error[E0202]"), "{stderr}");
}

#[test]
fn bitwise_16_not_on_float_names_accepted_types() {
    let stderr = compile_error_stderr("fn main() { println(!1.5); }\n");
    assert!(stderr.contains("error[E0202]"), "{stderr}");
    assert!(
        stderr.contains("requires `bool` or `i64`, found `f64`"),
        "{stderr}"
    );
}

#[test]
fn bitwise_17_try_propagation_before_bitwise_operators() {
    let source = r#"
fn low(o: Option<i64>) -> Option<i64> {
    return Some(o? & 3);
}

fn set(o: Option<i64>) -> Option<i64> {
    return Some(o? | 8);
}

fn main() {
    match low(Some(7)) { Some(v) => println(v), None => println("none") }
    match set(Some(1)) { Some(v) => println(v), None => println("none") }
    match set(None) { Some(v) => println(v), None => println("none") }
    let c = true;
    let f = c ? |x: i64| x | 1 : |x: i64| x & 1;
    println(f(4));
}
"#;
    let (debug, release) = run_both(source);
    assert_eq!(debug, "3\n9\nnone\n5\n");
    assert_eq!(release, debug);
}

#[test]
fn bitwise_18_reference_arguments_still_parse() {
    let source = r#"
fn bump(x: &mut i64) {
    x |= 16;
}

fn main() {
    let mut v = 1;
    bump(&v);
    println(v);
    println(v & 17);
}
"#;
    let (debug, release) = run_both(source);
    assert_eq!(debug, "17\n17\n");
    assert_eq!(release, debug);
}

#[test]
fn bitwise_19_async_and_method_bodies() {
    let source = r#"
class Mask {
    pub bits: i64;

    pub fn has(self, flag: i64) -> bool {
        return self.bits & flag != 0;
    }
}

async fn mix(a: i64, b: i64) -> i64 {
    return (a << 8) | (b & 255);
}

async fn main() {
    let m = new Mask(5);
    println(m.has(4));
    println(m.has(2));
    println(await mix(1, 511));
}
"#;
    let (debug, release) = run_both(source);
    assert_eq!(debug, "true\nfalse\n511\n");
    assert_eq!(release, debug);
}

#[test]
fn bitwise_20_xorshift_hash_matches_rust() {
    let source = r#"
fn step(seed: i64) -> i64 {
    let mut x = seed;
    x ^= x << 13;
    x ^= x >> 7;
    x ^= x << 17;
    return x;
}

fn main() {
    let mut h = 88172645463325252;
    for _ in 0..1000 {
        h = step(h);
    }
    println(h);
}
"#;
    let mut h: i64 = 88172645463325252;
    for _ in 0..1000 {
        h ^= h << 13;
        h ^= h >> 7;
        h ^= h << 17;
    }
    let (debug, release) = run_both(source);
    assert_eq!(debug, format!("{h}\n"));
    assert_eq!(release, debug);
}

#[test]
fn bitwise_21_unused_out_of_range_shift_still_faults_in_debug() {
    let source = r#"
fn amount() -> i64 { return 99; }

fn main() {
    if true {
        defer match recover() { Some(info) => println(info.message), None => {} }
        let unused = 1 << amount();
        println("after");
    }
}
"#;
    let (debug, release) = run_both(source);
    assert_eq!(debug, "integer overflow: `<<` shift amount outside 0..64\n");
    assert_eq!(release, "after\n");
}

#[test]
fn bitwise_22_bitset_popcount() {
    let source = r#"
fn popcount(v: i64) -> i64 {
    let mut x = v;
    let mut count = 0;
    while x != 0 {
        x &= x.wrapping_sub(1);
        count += 1;
    }
    return count;
}

fn main() {
    let mut set = 0;
    for i in 0..64 {
        if i % 3 == 0 {
            set |= 1 << i;
        }
    }
    println(popcount(set));
    println(set >> 63 & 1);
    println(popcount(!0));
}
"#;
    let (debug, release) = run_both(source);
    assert_eq!(debug, "22\n1\n64\n");
    assert_eq!(release, debug);
}

#[test]
fn bitwise_23_uncaught_shift_panic_reports_location() {
    let id = unique_test_id();
    let src_path = temp_path(format!("willow_bitwise_{id}.wi"));
    let bin_path = temp_path(format!("willow_bitwise_{id}"));
    fs::write(
        &src_path,
        "fn main() {\n    let n = 70;\n    println(1 >> n);\n}\n",
    )
    .unwrap();
    let built = Command::new(env!("CARGO_BIN_EXE_willow"))
        .args(["build", &src_path, "-o", &bin_path])
        .output()
        .expect("failed to run compiler");
    assert!(
        built.status.success(),
        "{}",
        String::from_utf8_lossy(&built.stderr)
    );
    let out = Command::new(&bin_path).output().expect("run binary");
    let _ = fs::remove_file(&src_path);
    remove_output_artifacts(&bin_path);
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(!out.status.success());
    assert!(
        stderr.contains("integer overflow: `>>` shift amount outside 0..64"),
        "{stderr}"
    );
    assert!(stderr.contains(":3:15"), "{stderr}");
}

#[test]
fn bitwise_24_shift_guards_scale_with_variable_shift_sites() {
    let raisers = |source: &str, release| {
        compile_and_collect_relocation_targets_mode(source, &[], release)
            .iter()
            .filter(|name| *name == "willow_int_overflow_panic")
            .count()
    };
    for sites in [4, 32] {
        let mut variable = String::new();
        let mut literal = String::new();
        for i in 0..sites {
            let op = if i % 2 == 0 { "<<" } else { ">>" };
            variable.push_str(&format!(
                "fn f{i}(a: i64, b: i64) -> i64 {{ return (a {op} b) & {i} | a ^ !b; }}\n"
            ));
            literal.push_str(&format!(
                "fn f{i}(a: i64, b: i64) -> i64 {{ return (a {op} {}) & b | a ^ !b; }}\n",
                i % 64
            ));
        }
        for source in [&mut variable, &mut literal] {
            source.push_str("fn main() {\n");
            for i in 0..sites {
                source.push_str(&format!("    println(f{i}(1, 2));\n"));
            }
            source.push_str("}\n");
        }
        assert_eq!(raisers(&variable, false), sites, "debug sites={sites}");
        assert_eq!(raisers(&variable, true), 0, "release sites={sites}");
        assert_eq!(raisers(&literal, false), 0, "literal sites={sites}");
    }
}
