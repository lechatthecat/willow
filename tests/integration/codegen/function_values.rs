use super::*;

// ── Function values, lambdas and indirect calls (willow-0g8j.2.2) ────────────
//
// The eligibility boundary is pinned by the `f*` unit tests in
// `src/backend/cranelift/lir_gen.rs`. These pin the OUTPUT. What makes the
// shape non-trivial is that a lambda is a SEPARATE function compiled under a
// symbol the walker never invented — the backend's span-keyed table names it —
// and that a call through a value has no statically known target, so the
// panic-depth protocol stays conservative.

#[test]
fn lir_diff_58_named_function_values() {
    assert_program_output(
        r#"
fn double(x: i64) -> i64 { return x * 2; }
fn square(x: i64) -> i64 { return x * x; }
fn apply(f: fn(i64) -> i64, v: i64) -> i64 { return f(v); }
fn pick(want_square: bool) -> fn(i64) -> i64 {
    if want_square { return square; }
    return double;
}
fn main() {
    println(apply(double, 21));
    println(apply(square, 7));
    let g: fn(i64) -> i64 = pick(true);
    println(g(9));
    println(apply(pick(false), 5));
}
"#,
        "42\n49\n81\n10\n",
    );
}

#[test]
fn lir_diff_59_lambda_values_including_nested() {
    // The inner lambda is lifted too, and its body is not part of the outer
    // one's block graph — lowering it inline would put the blocks in the wrong
    // function.
    assert_program_output(
        r#"
fn apply(f: fn(i64) -> i64, v: i64) -> i64 { return f(v); }
fn main() {
    println(apply(|x: i64| x + 1, 41));
    let times: fn(i64) -> i64 = |x: i64| -> i64 { return x * 10; };
    println(times(4));
    println(apply(|x: i64| apply(|y: i64| y * 3, x + 1), 2));
}
"#,
        "42\n40\n9\n",
    );
}

#[test]
fn lir_diff_60_shadowing_and_void_returning_function_values() {
    // `weigh` the local value shadows `weigh` the top-level function, so the
    // callee resolution order decides which one runs. The `void` cases are the
    // other signature an indirect call has: no result to merge.
    assert_program_output(
        r#"
fn weigh(n: i64) -> i64 { return n * 3; }
fn shout(s: String) { println("say " + s); }
fn run(f: fn(String) -> void, s: String) { f(s); }
fn main() {
    let weigh: fn(i64) -> i64 = |n: i64| n + 100;
    println(weigh(2));
    run(shout, "hi");
    let quiet: fn(String) -> void = |s: String| println("[" + s + "]");
    run(quiet, "there");
}
"#,
        "102\nsay hi\n[there]\n",
    );
}

#[test]
fn lir_diff_61_array_of_function_values() {
    // The callee comes out of an array element, so the value reaching the call
    // is loaded rather than named — and a lambda sits in the same array as two
    // named functions.
    assert_program_output(
        r#"
import std::collections::Array;
fn double(x: i64) -> i64 { return x * 2; }
fn negate(x: i64) -> i64 { return 0 - x; }
fn main() {
    let fs: Array<fn(i64) -> i64> = [double, negate, |x: i64| x + 7];
    let mut i = 0;
    while i < fs.len() {
        let g = fs[i];
        println(g(10));
        i = i + 1;
    }
}
"#,
        "20\n-10\n17\n",
    );
}

#[test]
fn lir_diff_62_gc_managed_values_cross_an_indirect_call() {
    // Each call allocates a new string, and the argument to the second call is
    // the first call's result — so a missing root would free a live string.
    assert_program_output(
        r#"
fn shout(s: String) -> String { return s + "!"; }
fn twice(f: fn(String) -> String, s: String) -> String { return f(f(s)); }
fn main() {
    println(twice(shout, "hi"));
    println(twice(|s: String| "[" + s + "]", "core"));
}
"#,
        "hi!!\n[[core]]\n",
    );
}

#[test]
fn lir_diff_63_option_combinators() {
    // `map`/`and_then`/`or_else` are the methods that CALL their operand, with
    // both spellings of a function value, and across both option
    // representations: `Option<i64>` is boxed, `Option<String>` is the niche.
    assert_program_output(
        r#"
fn label(v: i64) -> String {
    if v > 3 { return "big"; }
    return "small";
}
fn main() {
    let some: Option<i64> = Some(4);
    let none: Option<i64> = None;
    println(some.map(|v: i64| v * 10).unwrap_or(-1));
    println(none.map(|v: i64| v * 10).unwrap_or(-1));
    println(some.map(label).unwrap_or("?"));
    println(none.map(label).unwrap_or("?"));
    println(some.and_then(|v: i64| Option::Some(v + 1)).unwrap_or(-1));
    println(none.and_then(|v: i64| Option::Some(v + 1)).unwrap_or(-1));
    println(some.or_else(|| Option::Some(99)).unwrap_or(-1));
    println(none.or_else(|| Option::Some(99)).unwrap_or(-1));
}
"#,
        "40\n-1\nbig\n?\n5\n-1\n4\n99\n",
    );
}

#[test]
fn lir_diff_64_result_combinators() {
    // The `Result` side, including `map_err` — the only combinator that
    // rebuilds the error slot — and the two merges that pass the receiver
    // through one arm and the callable's own box through the other.
    assert_program_output(
        r#"
fn parse_even(v: i64) -> Result<i64, String> {
    if v % 2 == 0 { return Ok(v / 2); }
    return Err("odd");
}
fn label(v: i64) -> String {
    if v > 2 { return "big"; }
    return "small";
}
fn main() {
    let ok: Result<i64, String> = parse_even(8);
    let bad: Result<i64, String> = parse_even(7);
    println(ok.map(|v: i64| v * 10).unwrap_or(-1));
    println(bad.map(|v: i64| v * 10).unwrap_or(-1));
    println(ok.map(label).unwrap_or("?"));
    println(bad.map(label).unwrap_or("?"));
    println(ok.map_err(|e: String| "e:" + e).unwrap_or(-1));
    println(bad.map_err(|e: String| "e:" + e).unwrap_err());
    println(ok.and_then(|v: i64| parse_even(v)).unwrap_or(-1));
    println(bad.and_then(|v: i64| parse_even(v)).unwrap_err());
    println(ok.or_else(|e: String| Result::Ok(0)).unwrap());
    println(bad.or_else(|e: String| Result::Ok(0)).unwrap());
}
"#,
        "40\n-1\nbig\n?\n4\ne:odd\n2\nodd\n4\n0\n",
    );
}

#[test]
fn lir_diff_65_recursion_through_a_function_value() {
    // The recursion's step comes from a parameter, so the call graph is not
    // statically known — the panic-depth protocol has to stay conservative and
    // the frame push/pop still has to balance.
    assert_program_output(
        r#"
fn step(n: i64) -> i64 { return n - 1; }
fn walk(f: fn(i64) -> i64, n: i64) -> i64 {
    if n <= 0 { return 0; }
    return 1 + walk(f, f(n));
}
fn compose(f: fn(i64) -> i64, g: fn(i64) -> i64, v: i64) -> i64 {
    return f(g(v));
}
fn main() {
    println(walk(step, 5));
    println(walk(|n: i64| n - 2, 9));
    println(compose(step, |n: i64| n * 2, 10));
}
"#,
        "5\n5\n19\n",
    );
}

#[test]
fn lir_diff_66_function_value_into_a_class_method() {
    // The receiver's field is written through the callable's result, so the
    // method's own `self` has to survive the indirect call.
    assert_program_output(
        r#"
class Counter {
    pub total: i64;
    pub fn bump(self, by: fn(i64) -> i64) -> i64 {
        self.total = by(self.total);
        return self.total;
    }
}
fn triple(x: i64) -> i64 { return x * 3; }
fn main() {
    let c = new Counter(2);
    println(c.bump(triple));
    println(c.bump(|x: i64| x + 4));
    println(c.total);
}
"#,
        "6\n10\n10\n",
    );
}

#[test]
fn lir_diff_67_indirect_calls_and_combinators_under_gc_stress() {
    // Every iteration allocates: the argument string, the callee's result, and
    // the `Option` the combinator rebuilds. Collecting at every allocation is
    // what turns a missing root into a wrong answer rather than a lucky one.
    assert_output_under_gc_stress(
        r#"
fn wrap(s: String) -> String { return "<" + s + ">"; }
fn apply(f: fn(String) -> String, s: String) -> String { return f(s); }
fn main() {
    let mut i = 0;
    let mut last = "";
    while i < 40 {
        last = apply(wrap, "x" + "y");
        i = i + 1;
    }
    println(last);
    let mut j = 0;
    let mut seen = 0;
    while j < 40 {
        let o: Option<String> = Some("s" + "t");
        let got: String = o.map(|s: String| s + "!").unwrap_or("");
        if got != "" { seen = seen + 1; }
        j = j + 1;
    }
    println(seen);
}
"#,
        "<xy>\n40\n",
    );
}
