//! Safepoint-free leaf closures and root-free closure call sites
//! (willow-8hq4.28).
//!
//! A lambda body that is a safepoint-free leaf (willow-8hq4.14) binds its
//! closure environment, captures and parameters without roots and has no
//! entry poll. A closure whose body cannot raise tags bit 0 of its code word,
//! so a call through it skips the `willow_panic_depth` protocol; every other
//! closure takes the checked path. The call site keeps no roots of its own,
//! because every target roots its GC parameters before its first safepoint.
//!
//! Perspectives:
//!   01 a comparison leaf body adds no root and no poll (debug and release)
//!   02 an arithmetic leaf body is a leaf in release; debug overflow checks
//!      give it a panic path, so debug keeps the roots
//!   03 debug overflow inside an expression lambda still panics; release wraps
//!   04 a closure call site pushes no caller root
//!   05 a `fn` value call keeps its panic bracket on every site
//!   06 an allocating closure body keeps its roots
//!   07 a looping closure body keeps its poll
//!   08 a calling closure body keeps its root and poll
//!   09 a panicking block lambda still unwinds through the call
//!   10 a parameter divisor in an expression lambda still panics on zero
//!   11 a no-panic allocating block lambda is correct under GC stress
//!   12 captured references read by a leaf survive moving GCs between calls
//!   13 captures survive a collection inside a non-leaf closure body
//!   14 reference arguments survive a collection inside the callee
//!   15 void leaf closures
//!   16 floating-point leaf closures
//!   17 a leaf returning a captured reference under GC stress
//!   18 lambdas built inside lambdas
//!   19 disabling panic effects keeps every closure checked and the results
//!   20 closures called round-robin through an array (the benchmark shape)
//!   21 leaf closures built and called by sync code inside an async task
//!   22 generated panic-protocol references and code bytes scale linearly
//!      with chained call sites (operands are not duplicated per path)
//!   23 panics raised by a closure are recoverable by the caller's `recover`
//!   24 the runnable example
use super::support::{
    compile_and_collect_relocation_targets_mode, compile_and_measure_text_bytes, compile_and_run,
    compile_and_run_gc_stress_all, compile_and_run_release, compile_and_run_with_env,
};

const ROOT: &str = "willow_push_root";
const POLL: &str = "willow_gc_stop_flag";
const PANIC_DEPTH: &str = "willow_panic_depth";
const NO_EFFECTS: [(&str, &str); 1] = [("WILLOW_PANIC_EFFECTS", "0")];

fn count(source: &str, release: bool, symbol: &str) -> usize {
    compile_and_collect_relocation_targets_mode(source, &[], release)
        .iter()
        .filter(|name| name.as_str() == symbol)
        .count()
}

/// `[debug, release]` cost of four extra lambdas `|x: i64| <body>` capturing
/// an object `p` (`p.n` is 3), each built and called once by `main`, beyond
/// four comparison-only leaf lambdas with the same captures built and called
/// the same way. Only the lambda bodies differ.
fn lambda_delta(body: &str, symbol: &str) -> [usize; 2] {
    let with = raw_lambda_delta(body, symbol);
    let base = raw_lambda_delta(BASE_BODY, symbol);
    [with[0] - base[0], with[1] - base[1]]
}

const BASE_BODY: &str = "x > p.n ? x : p.n";

fn raw_lambda_delta(body: &str, symbol: &str) -> [usize; 2] {
    [false, true].map(|release| {
        let source = |count: usize| {
            let makers: String = (0..count)
                .map(|i| {
                    format!(
                        "fn make{i}(p: P) -> closure(i64) -> i64 {{ return |x: i64| {body}; }}\n"
                    )
                })
                .collect();
            let calls: String = (0..count)
                .map(|i| format!("let f{i} = make{i}(p); println(f{i}(4));\n"))
                .collect();
            format!(
                "class P {{ pub n: i64; pub s: String; }}\n{makers}fn main() {{\nlet p = new P(3, \"s\");\n{calls}}}"
            )
        };
        count(&source(5), release, symbol) - count(&source(1), release, symbol)
    })
}

/// `[debug, release]` cost of four extra call sites through `f`.
fn call_site_delta(ty: &str, value: &str, symbol: &str) -> [usize; 2] {
    [false, true].map(|release| {
        let source = |sites: usize| {
            let body = "total = total + f(total);".repeat(sites);
            format!(
                "fn run(f: {ty}) -> i64 {{ let mut total = 1; {body} return total; }}\n\
                 fn double(x: i64) -> i64 {{ return x * 2; }}\n\
                 fn main() {{ let n = 1; println(run({value})); }}"
            )
        };
        count(&source(5), release, symbol) - count(&source(1), release, symbol)
    })
}

fn both(source: &str) -> [(String, bool); 2] {
    [compile_and_run(source), compile_and_run_release(source)]
}

#[test]
fn comparison_leaf_lambda_has_no_root_or_poll() {
    // Relative to the comparison baseline; a looping body adds polls and an
    // allocating one adds roots on the same scale (perspectives 06/07).
    let body = "x > p.n && x != 100 ? 1 : 0";
    assert_eq!(lambda_delta(body, POLL), [0, 0]);
    assert_eq!(lambda_delta(body, ROOT), [0, 0]);
}

#[test]
fn arithmetic_leaf_lambda_is_a_leaf_in_release_only() {
    let body = "x * p.n + 3 - x";
    let [debug, release] = lambda_delta(body, ROOT);
    assert_eq!(release, 0);
    assert!(debug >= 4, "debug={debug}");
    assert_eq!(lambda_delta(body, POLL)[1], 0);
}

#[test]
fn debug_overflow_in_expression_lambda_still_panics() {
    let source = "fn make(n: i64) -> closure(i64) -> i64 { return |x| x + n; }\n\
         fn main() { let f = make(1); println(f(41)); println(f(9223372036854775807)); println(\"after\"); }";
    let (out, ok) = compile_and_run(source);
    assert!(!ok, "{out}");
    assert!(out.starts_with("42\n"), "{out}");
    assert!(!out.contains("after"), "{out}");
    let (out, ok) = compile_and_run_release(source);
    assert!(ok, "{out}");
    assert_eq!(out, "42\n-9223372036854775808\nafter\n");
}

#[test]
fn closure_call_site_pushes_no_root() {
    let closure = call_site_delta("closure(i64) -> i64", "|x| x + n", ROOT);
    assert_eq!(closure, [0, 0]);
    let function = call_site_delta("fn(i64) -> i64", "double", ROOT);
    assert_eq!(function, [0, 0]);
}

#[test]
fn function_value_call_keeps_panic_bracket() {
    // A `fn` value carries no tag, so each site keeps the full protocol: the
    // same references as a closure site's checked path.
    // Release reuses one depth snapshot across adjacent `fn` sites in a block;
    // a closure's checked path is a block of its own and takes a fresh one.
    let function = call_site_delta("fn(i64) -> i64", "double", PANIC_DEPTH);
    let closure = call_site_delta("closure(i64) -> i64", "|x| x + n", PANIC_DEPTH);
    assert!(function.iter().all(|&d| d >= 4), "{function:?}");
    assert!(closure.iter().all(|&d| d >= 8), "{closure:?}");
}

#[test]
fn allocating_lambda_keeps_roots() {
    let body = "{ let t = p.s + x.toString(); return t.len() + p.n; }";
    assert!(lambda_delta(body, ROOT).iter().all(|&d| d >= 4));
}

#[test]
fn looping_lambda_keeps_poll() {
    let body = "{ let mut i = 0; while i < x { i = i + p.n; } return i; }";
    assert!(lambda_delta(body, POLL).iter().all(|&d| d >= 4));
}

#[test]
fn calling_lambda_keeps_root_and_poll() {
    let source = |count: usize| {
        let makers: String = (0..count)
            .map(|i| {
                format!(
                    "fn make{i}(n: i64) -> closure(i64) -> i64 {{ return |x: i64| helper(x, n); }}\n"
                )
            })
            .collect();
        let calls: String = (0..count)
            .map(|i| format!("let f{i} = make{i}(3); println(f{i}(4));\n"))
            .collect();
        format!(
            "fn helper(a: i64, b: i64) -> i64 {{ return a + b; }}\n{makers}fn main() {{\n{calls}}}"
        )
    };
    for release in [false, true] {
        let delta = count(&source(5), release, ROOT) - count(&source(1), release, ROOT);
        assert!(delta >= 4, "release={release}: {delta}");
    }
}

#[test]
fn panicking_block_lambda_unwinds_through_call() {
    let source = "fn make(n: i64) -> closure(i64) -> i64 { return |x: i64| { if x > n { panic(\"too big\"); } return x; }; }\n\
         fn main() { let f = make(5); println(f(3)); println(f(9)); println(\"after\"); }";
    for (out, ok) in both(source) {
        assert!(!ok, "{out}");
        assert!(out.starts_with("3\n"), "{out}");
        assert!(!out.contains("after"), "{out}");
    }
}

#[test]
fn parameter_divisor_lambda_panics_on_zero() {
    let source = "fn make(n: i64) -> closure(i64) -> i64 { return |x| n / x; }\n\
         fn main() { let f = make(12); println(f(4)); println(f(0)); println(\"after\"); }";
    for (out, ok) in both(source) {
        assert!(!ok, "{out}");
        assert!(out.starts_with("3\n"), "{out}");
        assert!(!out.contains("after"), "{out}");
    }
}

#[test]
fn no_panic_allocating_lambda_survives_gc_stress() {
    let source = r#"
fn make(prefix: String) -> closure(i64) -> String {
    return |x: i64| { let s = prefix + x.toString(); return s + "!"; };
}
fn main() {
    let f = make("v");
    let mut i = 0;
    while i < 30 { gc_minor_collect(); let s = f(i); if i % 10 == 0 { println(s); } i = i + 1; }
}
"#;
    let (out, ok) = compile_and_run_gc_stress_all(source);
    assert!(ok, "{out}");
    assert_eq!(out, "v0!\nv10!\nv20!\n");
}

#[test]
fn captured_references_survive_moving_gc_between_leaf_calls() {
    let source = r#"
import std::collections::Array;
class P { pub x: i64; pub name: String; }
fn reader(p: P, k: i64) -> closure(i64) -> i64 { return |y| p.x * k + y; }
fn main() {
    let mut fs: Array<closure(i64) -> i64> = [];
    let mut i = 0;
    while i < 40 { fs.push(reader(new P(i, "p" + i.toString()), 3)); i = i + 1; }
    let mut total = 0;
    i = 0;
    while i < 40 {
        gc_minor_collect();
        let garbage = "g" + i.toString();
        gc_collect();
        let f = fs[i];
        total = total + f(garbage.len());
        i = i + 1;
    }
    println(total);
}
"#;
    let expected: i64 = (0..40)
        .map(|i: i64| 3 * i + format!("g{i}").len() as i64)
        .sum();
    for (out, ok) in [
        compile_and_run_gc_stress_all(source),
        compile_and_run_release(source),
    ] {
        assert!(ok, "{out}");
        assert_eq!(out, format!("{expected}\n"));
    }
}

#[test]
fn captures_survive_collection_inside_non_leaf_body() {
    let source = r##"
class P { pub x: i64; pub name: String; }
fn make(p: P, tag: String) -> closure(i64) -> String {
    return |y: i64| {
        gc_minor_collect();
        let junk = tag + y.toString();
        gc_collect();
        return p.name + ":" + (p.x + junk.len()).toString() + tag;
    };
}
fn main() {
    let f = make(new P(7, "seven"), "#");
    println(f(1));
    gc_minor_collect();
    println(f(22));
}
"##;
    for (out, ok) in [
        compile_and_run_gc_stress_all(source),
        compile_and_run_release(source),
    ] {
        assert!(ok, "{out}");
        assert_eq!(out, "seven:9#\nseven:10#\n");
    }
}

#[test]
fn reference_arguments_survive_collection_in_callee() {
    let source = r#"
fn make(sep: String) -> closure(String, String) -> String {
    return |a: String, b: String| { gc_minor_collect(); let t = a + sep; gc_collect(); return t + b; };
}
fn main() {
    let f = make("-");
    let mut i = 0;
    while i < 3 { println(f("a" + i.toString(), "b" + i.toString())); i = i + 1; }
}
"#;
    let (out, ok) = compile_and_run_gc_stress_all(source);
    assert!(ok, "{out}");
    assert_eq!(out, "a0-b0\na1-b1\na2-b2\n");
}

#[test]
fn void_leaf_closure() {
    let source = "fn make(n: i64) -> closure(i64) -> void { return |x: i64| { let y = x > n; }; }\n\
         fn main() { let f = make(2); f(1); f(3); println(\"done\"); }";
    for (out, ok) in both(source) {
        assert!(ok, "{out}");
        assert_eq!(out, "done\n");
    }
}

#[test]
fn float_leaf_closure() {
    let source = "fn make(k: f64) -> closure(f64) -> f64 { return |x| x * k + 0.25; }\n\
         fn main() { let f = make(1.5); println(f(2.0)); println(f(-1.0)); }";
    for (out, ok) in both(source) {
        assert!(ok, "{out}");
        assert_eq!(out, "3.25\n-1.25\n");
    }
}

#[test]
fn leaf_returning_captured_reference_under_gc_stress() {
    let source = r#"
fn make(a: String, b: String) -> closure(bool) -> String { return |first| first ? a : b; }
fn main() {
    let f = make("left" + 1.toString(), "right" + 2.toString());
    let mut i = 0;
    while i < 4 { gc_minor_collect(); println(f(i % 2 == 0)); i = i + 1; }
}
"#;
    let (out, ok) = compile_and_run_gc_stress_all(source);
    assert!(ok, "{out}");
    assert_eq!(out, "left1\nright2\nleft1\nright2\n");
}

#[test]
fn lambdas_built_inside_lambdas() {
    let source = r#"
fn main() {
    let base = 10;
    let outer = |k: i64| {
        let inner = |y: i64| y * k + base;
        gc_minor_collect();
        return inner(k) + inner(1);
    };
    println(outer(2));
    println(outer(3));
}
"#;
    for (out, ok) in [
        compile_and_run_gc_stress_all(source),
        compile_and_run_release(source),
    ] {
        assert!(ok, "{out}");
        assert_eq!(out, "26\n32\n");
    }
}

#[test]
fn disabled_panic_effects_keep_closures_checked() {
    let ok_source = "fn make(n: i64) -> closure(i64) -> i64 { return |x| x > n ? x : n; }\n\
         fn main() { let f = make(3); println(f(1) + f(9)); }";
    let (out, ok) = compile_and_run_with_env(ok_source, &NO_EFFECTS);
    assert!(ok, "{out}");
    assert_eq!(out, "12\n");
    let panic_source = "fn make(n: i64) -> closure(i64) -> i64 { return |x| n / x; }\n\
         fn main() { let f = make(3); println(f(1)); println(f(0)); println(\"after\"); }";
    let (out, ok) = compile_and_run_with_env(panic_source, &NO_EFFECTS);
    assert!(!ok, "{out}");
    assert!(out.starts_with("3\n") && !out.contains("after"), "{out}");
}

#[test]
fn closures_called_round_robin_through_an_array() {
    let source = r#"
import std::collections::Array;
fn adder(n: i64) -> closure(i64) -> i64 { return |x| x + n; }
fn main() {
    let fs: Array<closure(i64) -> i64> = [];
    let mut i = 0;
    while i < 64 { fs.push(adder(i % 7 + 1)); i = i + 1; }
    let mut sum = 0;
    let mut round = 0;
    while round < 50 { i = 0; while i < 64 { let f = fs[i]; sum = sum + f(i); i = i + 1; } round = round + 1; }
    println(sum);
}
"#;
    let expected: i64 = 50 * (0..64).map(|i: i64| i + i % 7 + 1).sum::<i64>();
    for (out, ok) in both(source) {
        assert!(ok, "{out}");
        assert_eq!(out, format!("{expected}\n"));
    }
}

#[test]
fn leaf_closures_inside_async_function() {
    let source = r#"
class Box { pub n: i64; }
fn make(b: Box) -> closure(i64) -> bool { return |n| b.n > n; }
fn hit(b: Box, i: i64) -> bool { let f = make(b); gc_minor_collect(); return f(i); }
async fn work(s: String) -> i64 {
    let mut hits = 0;
    let mut i = 0;
    let b = new Box(s.len());
    while i < 6 {
        await sleep(1);
        if hit(b, i) { hits = hits + 1; }
        i = i + 1;
    }
    return hits;
}
async fn main() { println(await work("abc" + "d")); }
"#;
    let (out, ok) = compile_and_run_gc_stress_all(source);
    assert!(ok, "{out}");
    assert_eq!(out, "4\n");
}

#[test]
fn panic_protocol_references_scale_linearly_with_call_sites() {
    for release in [false, true] {
        let source = |sites: usize| {
            let body = "total = total + f(total);".repeat(sites);
            format!(
                "fn run(f: closure(i64) -> i64) -> i64 {{ let mut total = 1; {body} return total; }}\n\
                 fn main() {{ let n = 1; println(run(|x| x + n)); }}"
            )
        };
        let refs = [1, 2, 8].map(|sites| count(&source(sites), release, PANIC_DEPTH));
        let per_site = refs[1] - refs[0];
        assert!(per_site > 0, "release={release}");
        assert_eq!(refs[2] - refs[0], 7 * per_site, "release={release}");
    }
}

#[test]
fn chained_call_site_code_grows_linearly() {
    // Each site's argument is the sum of every earlier result. Emitting the
    // call once per tag path let the optimizer rematerialize that sum on both
    // paths, so code grew quadratically with the chain length.
    let source = |sites: usize| {
        let body = "total = total + f(total);".repeat(sites);
        format!(
            "fn run(f: closure(i64) -> i64) -> i64 {{ let mut total = 1; {body} return total; }}\n\
             fn main() {{ let n = 1; println(run(|x| x + n)); }}"
        )
    };
    for release in [false, true] {
        let bytes = [1, 9, 65].map(|sites| compile_and_measure_text_bytes(&source(sites), release));
        let short = (bytes[1] - bytes[0]) / 8;
        let long = (bytes[2] - bytes[1]) / 56;
        assert!(
            long * 4 <= short * 5,
            "release={release}: {short} bytes/site at 1..9 sites, {long} at 9..65 ({bytes:?})"
        );
    }
}

#[test]
fn closure_panic_is_recoverable() {
    let source = r#"
fn make(n: i64) -> closure(i64) -> i64 { return |x| n / x; }
fn guarded(f: closure(i64) -> i64, x: i64) -> i64 {
    let mut result = -1;
    if true {
        defer match recover() { Some(info) => println("recovered"), None => {} }
        result = f(x);
    }
    return result;
}
fn main() { let f = make(8); println(guarded(f, 2)); println(guarded(f, 0)); println("after"); }
"#;
    for (out, ok) in both(source) {
        assert!(ok, "{out}");
        assert_eq!(out, "4\nrecovered\n-1\nafter\n");
    }
}

#[test]
fn runnable_example() {
    let source = include_str!("../../example/leaf_closure_calls.wi");
    for (out, ok) in [
        compile_and_run_gc_stress_all(source),
        compile_and_run_release(source),
    ] {
        assert!(ok, "{out}");
        assert_eq!(out, EXAMPLE_OUTPUT);
    }
}

const EXAMPLE_OUTPUT: &str = "3025\n25\ntotal=3025\n";
