use super::*;

// ── Closures: lambdas that capture (willow-0g8j.2.12) ───────────────────────
//
// A capturing lambda is a `closure`, not a `fn`: its value is a GC object
// whose payload word 0 holds the lifted function's code address and whose
// words 1..N hold the captured values, and the lifted body takes that object
// as a hidden leading argument. The unit tests in
// `semantic::type_checker::check_lambda_match` pin WHICH names are captured;
// these pin what the compiled program then prints, which is the only thing
// that catches a slot written at the wrong offset, an environment that is not
// a GC root, or an indirect call that forgets to pass it.
//
// 27 perspectives:
//   1 i64 capture read twice        15 transitive capture two frames up
//   2 mixed capture types in order  16 an environment per loop iteration
//   3 String capture                17 the body rebinds a captured name
//   4 f64 capture                   18 a method local, not `self`
//   5 bool capture                  19 eight slots, each at its own offset
//   6 the frame is gone by then     20 the environment is a GC root
//   7 one environment per closure   21 a captured Option, matched
//   8 a later write is not seen     22 a closure returning a closure
//   9 repeat calls see one copy     23 a capture-free lambda as a closure
//  10 an element write is shared    24 a captured array, iterated
//  11 a field write is shared       25 E1011: not a `fn` value
//  12 a closure parameter           26 E1009: no write to a capture
//  13 a closure over closures       27 E1002: no capture of `self`
//  14 (see 13)

#[test]
fn clo_01_an_i64_capture_reads_the_same_value_every_call() {
    assert_program_output(
        "fn main() { let n = 7; let f = |x: i64| x + n; println(f(1)); println(f(2)); }",
        "8\n9\n",
    );
}

#[test]
fn clo_02_mixed_capture_types_land_in_first_mention_order() {
    // Three slots of three different representations: a GC reference, an
    // integer and a bool. A slot written at the wrong offset would swap them.
    assert_program_output(
        r#"
fn main() {
    let label = "id";
    let k = 3;
    let flag = true;
    let f = |x: i64| label + ":" + (flag ? x * k : 0).toString();
    println(f(4));
}
"#,
        "id:12\n",
    );
}

#[test]
fn clo_03_a_string_capture_is_a_gc_reference() {
    assert_program_output(
        r#"fn main() { let s = "hi"; let f = |t: String| s + "-" + t; println(f("there")); }"#,
        "hi-there\n",
    );
}

#[test]
fn clo_04_an_f64_capture_keeps_its_representation() {
    assert_program_output(
        "fn main() { let r = 1.5; let f = |x: f64| x * r; println(f(4.0)); }",
        "6\n",
    );
}

#[test]
fn clo_05_a_bool_capture_keeps_its_representation() {
    assert_program_output(
        "fn main() { let on = false; let f = |x: i64| on ? x : 0 - x; println(f(5)); }",
        "-5\n",
    );
}

#[test]
fn clo_06_a_closure_outlives_the_frame_it_captured_from() {
    // The environment is a heap object, so `n` is still readable after
    // `adder` has returned.
    assert_program_output(
        r#"
fn adder(n: i64) -> closure(i64) -> i64 { return |x: i64| x + n; }
fn main() { let a = adder(10); println(a(1)); println(a(2)); }
"#,
        "11\n12\n",
    );
}

#[test]
fn clo_07_each_closure_value_gets_its_own_environment() {
    assert_program_output(
        r#"
fn adder(n: i64) -> closure(i64) -> i64 { return |x: i64| x + n; }
fn main() { let a = adder(10); let b = adder(100); println(a(1)); println(b(1)); println(a(2)); }
"#,
        "11\n101\n12\n",
    );
}

#[test]
fn clo_08_a_later_write_to_the_enclosing_local_is_not_seen() {
    // The copy is taken where the closure value is built, so the closure and
    // the enclosing frame disagree from that point on — by design.
    assert_program_output(
        r#"
fn main() {
    let mut n = 1;
    let f = |x: i64| x + n;
    n = 100;
    println(f(0));
    println(n);
}
"#,
        "1\n100\n",
    );
}

#[test]
fn clo_09_repeat_calls_see_one_unchanging_copy() {
    assert_program_output(
        r#"
fn twice(f: closure(i64) -> i64, v: i64) -> i64 { return f(f(v)); }
fn main() { let k = 3; println(twice(|x: i64| x * k, 2)); }
"#,
        "18\n",
    );
}

#[test]
fn clo_10_an_element_write_through_a_captured_array_is_shared() {
    // By value copies the SLOT, not the object it points at: the enclosing
    // frame sees the write, exactly as it would through a parameter.
    assert_program_output(
        r#"
import std::collections::Array;
fn main() {
    let xs: Array<i64> = [1, 2];
    let f = |v: i64| { xs[0] = v; return xs[0] + xs[1]; };
    println(f(40));
    println(xs[0]);
}
"#,
        "42\n40\n",
    );
}

#[test]
fn clo_11_a_field_write_through_a_captured_object_is_shared() {
    assert_program_output(
        r#"
class Box { pub value: i64; }
fn main() {
    let b = new Box(1);
    let f = |v: i64| { b.value = b.value + v; return b.value; };
    println(f(41));
    println(b.value);
}
"#,
        "42\n42\n",
    );
}

#[test]
fn clo_12_a_closure_parameter_is_called_with_its_environment() {
    // The callee address comes out of the object's first word and the object
    // itself goes in as the hidden leading argument; dropping either would be
    // a call into the wrong code or a body reading an empty environment.
    assert_program_output(
        r#"
fn twice(f: closure(i64) -> i64, v: i64) -> i64 { return f(f(v)); }
fn main() { let k = 3; println(twice(|x: i64| x * k, 2)); println(twice(|x: i64| x + 1, 2)); }
"#,
        "18\n4\n",
    );
}

#[test]
fn clo_13_a_closure_can_capture_and_call_other_closures() {
    assert_program_output(
        r#"
fn compose(f: closure(i64) -> i64, g: closure(i64) -> i64) -> closure(i64) -> i64 {
    return |x: i64| f(g(x));
}
fn main() {
    let a = 1;
    let b = 10;
    let h = compose(|x: i64| x + a, |x: i64| x * b);
    println(h(4));
}
"#,
        "41\n",
    );
}

#[test]
fn clo_15_a_capture_two_frames_up_rides_both_environments() {
    // The inner lambda cannot reach `main`'s frame, so the outer closure has
    // to capture `base` in order to hand it on.
    assert_program_output(
        r#"
fn main() {
    let base = 100;
    let outer = |x: i64| { let inner = |y: i64| y + base; return inner(x) + inner(0); };
    println(outer(5));
}
"#,
        "205\n",
    );
}

#[test]
fn clo_16_a_closure_built_in_a_loop_captures_that_iteration() {
    assert_program_output(
        r#"
fn main() {
    let mut total = 0;
    let mut i = 1;
    while i <= 3 {
        let step = i * 10;
        let f = |x: i64| x + step;
        total = total + f(1);
        i = i + 1;
    }
    println(total);
}
"#,
        "63\n",
    );
}

#[test]
fn clo_17_the_body_may_rebind_a_captured_name() {
    // The capture and the body's own `let` are separate locals in the lifted
    // function, so the initializer still reads the captured copy.
    assert_program_output(
        r#"
fn main() {
    let seed = 5;
    let f = |x: i64| { let seed = seed * x; return seed + 1; };
    println(f(4));
    println(seed);
}
"#,
        "21\n5\n",
    );
}

#[test]
fn clo_18_a_method_local_is_capturable_where_self_is_not() {
    assert_program_output(
        r#"
class Counter {
    pub count: i64;
    pub fn plus(self, extra: i64) -> i64 {
        let start = self.count;
        let f = |step: i64| start + step;
        return f(extra);
    }
}
fn main() { println(new Counter(40).plus(2)); }
"#,
        "42\n",
    );
}

#[test]
fn clo_19_eight_slots_each_land_at_their_own_offset() {
    // Each capture contributes a distinct decimal digit, so any two slots
    // swapped or aliased shows up in the printed number.
    assert_program_output(
        r#"
fn main() {
    let a = 1; let b = 2; let c = 3; let d = 4;
    let e = 5; let g = 6; let h = 7; let i = 8;
    let f = |x: i64| a + b * 10 + c * 100 + d * 1000 + e * 10000 + g * 100000 + h * 1000000 + i * 10000000 + x;
    println(f(0));
}
"#,
        "87654321\n",
    );
}

#[test]
fn clo_20_the_environment_keeps_its_captures_alive_across_a_collection() {
    // Under stress the collector runs at every allocation, so a capture the
    // environment's ref mask does not cover would be swept while the closure
    // still holds it.
    let source = r#"
import std::collections::Array;

fn churn(n: i64) -> i64 {
    let mut t = 0;
    let mut i = 0;
    while i < n {
        let junk: Array<i64> = [i, i + 1, i + 2];
        t = t + junk[0];
        i = i + 1;
    }
    return t;
}

fn main() {
    let label = "kept";
    let xs: Array<i64> = [11, 22];
    let f = |k: i64| label + ":" + (xs[0] + k).toString();
    println(churn(200));
    println(f(1));
    println(churn(200));
    println(f(2));
}
"#;
    let (out, ok) = compile_and_run_gc_stress_all(source);
    assert!(ok, "gc stress run failed: {out}");
    assert_eq!(out, "19900\nkept:12\n19900\nkept:13\n");
}

#[test]
fn clo_21_a_captured_option_is_matched_in_the_body() {
    assert_program_output(
        r#"
fn main() {
    let fallback = 7;
    let o: Option<i64> = Some(35);
    let f = |extra: i64| match o { Some(v) => v + extra, None => fallback, };
    println(f(fallback));
}
"#,
        "42\n",
    );
}

#[test]
fn clo_22_a_closure_can_return_a_closure() {
    // The inner lambda captures `b` from the outer lambda's params and `a`
    // from the outer environment, so the second environment is built inside a
    // lifted body rather than in a named function.
    assert_program_output(
        r#"
fn curry(a: i64) -> closure(i64) -> closure(i64) -> i64 {
    return |b: i64| |c: i64| a + b + c;
}
fn main() { let add1 = curry(1); let add12 = add1(2); println(add12(3)); }
"#,
        "6\n",
    );
}

#[test]
fn clo_23_a_capture_free_lambda_still_answers_to_a_closure_parameter() {
    // It simply carries an environment with no slots in it, so the two kinds
    // of lambda can be mixed in one call site.
    assert_program_output(
        r#"
fn twice(f: closure(i64) -> i64, v: i64) -> i64 { return f(f(v)); }
fn main() { println(twice(|x: i64| x + 1, 2)); }
"#,
        "4\n",
    );
}

#[test]
fn clo_24_a_captured_array_can_be_iterated() {
    assert_program_output(
        r#"
import std::collections::Array;
fn main() {
    let xs: Array<i64> = [1, 2, 3];
    let f = |bump: i64| { let mut t = 0; for v in xs { t = t + v + bump; } return t; };
    println(f(10));
}
"#,
        "36\n",
    );
}

#[test]
fn clo_25_a_capturing_lambda_is_refused_where_a_fn_value_is_wanted() {
    let (ok, stderr) = compile_with_compiler_env(
        "fn apply(f: fn(i64) -> i64, v: i64) -> i64 { return f(v); }\n\
         fn main() { let n = 1; println(apply(|x: i64| x + n, 2)); }",
        &PLAIN,
    );
    assert!(!ok, "a capturing lambda must not pass as a `fn` value");
    assert!(
        stderr.contains("E1011") && stderr.contains("captures `n`"),
        "the refusal must name the capture: {stderr}"
    );
}

#[test]
fn clo_26_a_write_to_a_capture_is_refused() {
    let (ok, stderr) = compile_with_compiler_env(
        "fn main() { let mut n = 1; let f = |x: i64| { n = n + x; return n; }; println(f(1)); }",
        &PLAIN,
    );
    assert!(!ok, "a write to a capture must not compile");
    assert!(
        stderr.contains("E1009") && stderr.contains("cannot assign to captured variable `n`"),
        "the refusal must be about the capture, not about mutability: {stderr}"
    );
}

#[test]
fn clo_27_a_capture_of_self_is_refused() {
    let (ok, stderr) = compile_with_compiler_env(
        "class C { pub v: i64; pub fn m(self) -> i64 { let f = |x: i64| x + self.v; return f(1); } }\n\
         fn main() { println(new C(1).m()); }",
        &PLAIN,
    );
    assert!(!ok, "`self` must stay uncapturable");
    assert!(
        stderr.contains("E1002") && stderr.contains("cannot capture `self`"),
        "the refusal must name `self`: {stderr}"
    );
}
