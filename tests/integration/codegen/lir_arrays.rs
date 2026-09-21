use super::*;

// ── LIR backend: Array<T> (willow-0g8j.4) ───────────────────────────────────
// The walker now emits array literals, indexing, index-assignment and the
// builtin len/push/pop/toString. An array is a GC handle whose pointer is
// stable across growth, so an array local uses the same entry-rooted slot a
// String local does; temporaries are rooted around any sub-expression that can
// collect. Eligibility perspectives 1-15 are unit tests in
// `src/backend/cranelift/lir_gen.rs`; these are perspectives 16-38, each
// asserting the program's output, the stress-tagged ones additionally under
// `WILLOW_GC_STRESS=alloc`: 16 literal + len + index, 17 element kinds
// round-trip through the 64-bit word ABI (i64/f64/bool), 18 push past the
// initial capacity, 19 pop, 20 index assignment, 21 for-over-array, 22 array
// passed to and returned from a function, 23 toString, 24 nested arrays,
// 25 array of String built by concatenation, 26 array local live across later
// allocations (stress), 27 array of String under stress, 28 array literal whose
// elements allocate (stress), 29 index expression that allocates while the
// array is a temporary (stress), 30 index-assignment with an allocating value
// (stress), 31 push with an allocating value (stress), 32 loop that grows an
// array of String (stress), 33 early return out of a loop holding array locals
// (stress), 34 array argument rooted across a later allocating argument
// (stress), 35 out-of-bounds read panics identically on both paths,
// 36 index-assignment whose *receiver* is a temporary (stress), 37 push whose
// receiver is a temporary (stress). The last two are what actually pin the
// index-assign and push roots: with a named local receiver the entry slot
// already keeps the handle alive, so only a temporary can prove them.
// 38 `for` over an array observes a `push`/`pop` made inside the body — the
// length is re-read on every header entry, not snapshot on entry.

#[test]
fn lir_diff_32_array_literal_len_index() {
    assert_program_output(
        r#"
import std::collections::Array;

fn f() -> i64 {
    let xs = [10, 20, 30];
    return xs.len() + xs[0] + xs[2];
}
fn main() { println(f()); }
"#,
        "43\n",
    );
}

#[test]
fn lir_diff_33_array_element_kinds() {
    assert_program_output(
        r#"
import std::collections::Array;

fn ints() -> i64 { let xs = [1, 2]; return xs[1]; }
fn floats() -> f64 { let xs = [1.5, 2.5]; return xs[0] + xs[1]; }
fn bools() -> bool { let xs = [false, true]; return xs[1]; }
fn main() {
    println(ints());
    println(floats());
    println(bools());
}
"#,
        "2\n4\ntrue\n",
    );
}

#[test]
fn lir_diff_34_push_grows_past_capacity() {
    assert_program_output(
        r#"
import std::collections::Array;

fn build(n: i64) -> i64 {
    let mut xs = [0];
    let mut i = 0;
    while i < n {
        xs.push(i);
        i = i + 1;
    }
    return xs.len();
}
fn main() { println(build(0)); println(build(1)); println(build(9)); }
"#,
        "1\n2\n10\n",
    );
}

#[test]
fn lir_diff_35_pop() {
    assert_program_output(
        r#"
import std::collections::Array;

fn drain() -> i64 {
    let mut xs = [1, 2, 3];
    let a = xs.pop();
    let b = xs.pop();
    return a * 10 + b + xs.len();
}
fn main() { println(drain()); }
"#,
        "33\n",
    );
}

#[test]
fn lir_diff_36_index_assignment() {
    assert_program_output(
        r#"
import std::collections::Array;

fn f() -> i64 {
    let mut xs = [1, 2, 3];
    xs[0] = 100;
    xs[2] = xs[0] + xs[1];
    return xs[0] + xs[1] + xs[2];
}
fn main() { println(f()); }
"#,
        "204\n",
    );
}

#[test]
fn lir_diff_37_for_over_array() {
    assert_program_output(
        r#"
import std::collections::Array;

fn total(xs: Array<i64>) -> i64 {
    let mut t = 0;
    for x in xs {
        t = t + x;
    }
    return t;
}
fn main() { println(total([1, 2, 3, 4])); }
"#,
        "10\n",
    );
}

#[test]
fn lir_diff_38_array_argument_and_return() {
    assert_program_output(
        r#"
import std::collections::Array;

fn doubled(xs: Array<i64>) -> Array<i64> {
    let mut out = [xs[0] * 2];
    let mut i = 1;
    while i < xs.len() {
        out.push(xs[i] * 2);
        i = i + 1;
    }
    return out;
}
fn main() {
    let ys = doubled([1, 2, 3]);
    println(ys[0] + ys[1] + ys[2]);
}
"#,
        "12\n",
    );
}

#[test]
fn lir_diff_39_array_to_string() {
    assert_program_output(
        r#"
import std::collections::Array;

fn f() -> String {
    let mut xs = [1, 2];
    xs.push(3);
    return xs.toString();
}
fn g() -> String { let xs = ["a", "b"]; return xs.toString(); }
fn main() { println(f()); println(g()); }
"#,
        "[1, 2, 3]\n[\"a\", \"b\"]\n",
    );
}

#[test]
fn lir_diff_40_nested_arrays() {
    assert_program_output(
        r#"
import std::collections::Array;

fn f() -> i64 {
    let grid = [[1, 2], [3, 4]];
    return grid[0][1] + grid[1][0] + grid.len() + grid[1].len();
}
fn main() { println(f()); }
"#,
        "9\n",
    );
}

#[test]
fn lir_diff_41_array_of_built_strings() {
    assert_program_output(
        r#"
import std::collections::Array;

fn f() -> String {
    let mut xs = ["a" + "1", "b" + "2"];
    xs.push("c" + "3");
    return xs[0] + xs[1] + xs[2];
}
fn main() { println(f()); }
"#,
        "a1b2c3\n",
    );
}

#[test]
fn lir_diff_42_array_local_live_across_allocation_stress() {
    assert_output_under_gc_stress(
        r#"
import std::collections::Array;

fn churn(n: i64) -> String {
    let mut s = "";
    let mut i = 0;
    while i < n {
        s = s + "x";
        i = i + 1;
    }
    return s;
}

fn f() -> i64 {
    let xs = [1, 2, 3];
    // Every one of these allocates while `xs` must stay live.
    let a = churn(4);
    let b = churn(5);
    let ok_a = a == "xxxx" ? 10 : 0;
    let ok_b = b == "xxxxx" ? 100 : 0;
    return xs[0] + xs[1] + xs[2] + ok_a + ok_b;
}
fn main() { println(f()); }
"#,
        "116\n",
    );
}

#[test]
fn lir_diff_43_array_of_strings_stress() {
    assert_output_under_gc_stress(
        r#"
import std::collections::Array;

fn f(n: i64) -> String {
    let mut xs = ["seed"];
    let mut i = 0;
    while i < n {
        xs.push("v" + "0");
        i = i + 1;
    }
    let mut out = xs[0];
    let mut j = 1;
    while j < xs.len() {
        out = out + "/" + xs[j];
        j = j + 1;
    }
    return out;
}
fn main() { println(f(3)); }
"#,
        "seed/v0/v0/v0\n",
    );
}

#[test]
fn lir_diff_44_allocating_literal_elements_stress() {
    assert_output_under_gc_stress(
        r#"
import std::collections::Array;

fn make(tag: String) -> String { return "<" + tag + ">"; }

fn f() -> String {
    // Every element allocates, so the fresh array must be rooted for the
    // whole fill.
    let xs = [make("a"), make("b"), make("c")];
    return xs[0] + xs[1] + xs[2];
}
fn main() { println(f()); }
"#,
        "<a><b><c>\n",
    );
}

#[test]
fn lir_diff_45_allocating_index_on_temporary_stress() {
    assert_output_under_gc_stress(
        r#"
import std::collections::Array;

fn pick(n: i64) -> i64 { let s = "i" + "dx"; return s == "idx" ? n : n + 1; }
fn build() -> Array<i64> { return [7, 8, 9]; }

fn f() -> i64 {
    // The array is a temporary and the index expression allocates: the handle
    // has to be rooted across the index evaluation.
    return build()[pick(1)];
}
fn main() { println(f()); }
"#,
        "8\n",
    );
}

#[test]
fn lir_diff_46_index_assign_allocating_value_stress() {
    assert_output_under_gc_stress(
        r#"
import std::collections::Array;

fn f() -> String {
    let mut xs = ["a", "b"];
    xs[1] = "c" + "d";
    return xs[0] + xs[1];
}
fn main() { println(f()); }
"#,
        "acd\n",
    );
}

#[test]
fn lir_diff_47_push_allocating_value_stress() {
    assert_output_under_gc_stress(
        r#"
import std::collections::Array;

fn f(n: i64) -> i64 {
    let mut xs = ["s"];
    let mut i = 0;
    while i < n {
        xs.push("p" + "q");
        i = i + 1;
    }
    let mut total = 0;
    let mut j = 0;
    while j < xs.len() {
        if xs[j] == "pq" {
            total = total + 1;
        }
        j = j + 1;
    }
    return total;
}
fn main() { println(f(4)); }
"#,
        "4\n",
    );
}

#[test]
fn lir_diff_48_many_live_arrays_stress() {
    assert_output_under_gc_stress(
        r#"
import std::collections::Array;

fn f() -> i64 {
    let a = ["a" + "1"];
    let b = ["b" + "2"];
    let c = [1, 2, 3];
    let d = ["d" + "4"];
    let e = [c[0] + 1, c[2] + 1];
    let mut t = e[0] + e[1];
    if a[0] == "a1" { t = t + 1; }
    if b[0] == "b2" { t = t + 10; }
    if d[0] == "d4" { t = t + 100; }
    return t;
}
fn main() { println(f()); }
"#,
        "117\n",
    );
}

#[test]
fn lir_diff_49_early_return_with_array_locals_stress() {
    assert_output_under_gc_stress(
        r#"
import std::collections::Array;

fn find(limit: i64) -> String {
    let names = ["a" + "a", "b" + "b", "c" + "c"];
    let mut i = 0;
    while i < limit {
        if i == 1 {
            // Early return out of a loop: exactly this frame's roots pop.
            return names[i] + "!";
        }
        i = i + 1;
    }
    return names[0];
}
fn main() { println(find(5)); println(find(1)); }
"#,
        "bb!\naa\n",
    );
}

#[test]
fn lir_diff_50_array_argument_rooted_across_later_argument_stress() {
    assert_output_under_gc_stress(
        r#"
import std::collections::Array;

fn combine(xs: Array<String>, tag: String) -> String { return xs[0] + tag; }
fn make(n: i64) -> Array<String> { return ["e" + "0"]; }

fn f() -> String {
    // The first argument is a fresh array; building the second one allocates
    // before the call, so the array has to be rooted meanwhile.
    return combine(make(1), "t" + "1");
}
fn main() { println(f()); }
"#,
        "e0t1\n",
    );
}

#[test]
fn lir_diff_54_for_observes_mutation_during_iteration() {
    // The loop bound is re-read on every header entry on both paths: a `push`
    // in the body extends the walk, a `pop` cuts it short.
    assert_program_output(
        r#"
import std::collections::Array;

fn grow() -> i64 {
    let xs = [1];
    let mut n = 0;
    for x in xs {
        n = n + 1;
        if n < 3 {
            xs.push(n * 10);
        }
    }
    return n * 100 + xs.len();
}

fn shrink() -> i64 {
    let xs = [1, 2, 3, 4, 5, 6];
    let mut n = 0;
    for x in xs {
        n = n + 1;
        xs.pop();
    }
    return n;
}
fn main() { println(grow()); println(shrink()); }
"#,
        "303\n3\n",
    );
}

#[test]
fn lir_diff_52_index_assign_on_temporary_array_stress() {
    assert_output_under_gc_stress(
        r#"
import std::collections::Array;

fn build() -> Array<String> { return ["a", "b"]; }
fn mk() -> String { return "c" + "d"; }

fn f() -> i64 {
    // The assigned-to array is a temporary nothing else roots, and the value
    // expression allocates: without a root the write lands in freed memory.
    build()[0] = mk();
    return 1;
}
fn main() { println(f()); }
"#,
        "1\n",
    );
}

#[test]
fn lir_diff_53_push_on_temporary_array_stress() {
    assert_output_under_gc_stress(
        r#"
import std::collections::Array;

fn build() -> Array<String> { return ["a", "b"]; }
fn mk() -> String { return "c" + "d"; }

fn f() -> i64 {
    // Same shape for `push`: the receiver is a temporary and the pushed value
    // allocates before the runtime call takes over rooting.
    build().push(mk());
    return 1;
}
fn main() { println(f()); }
"#,
        "1\n",
    );
}

#[test]
fn lir_diff_51_out_of_bounds_panics_identically() {
    let source = r#"
import std::collections::Array;

fn f(i: i64) -> i64 { let xs = [1, 2]; return xs[i]; }
fn main() { println(f(5)); }
"#;
    let (out, ok) = compile_with_env_and_run_combined(source, &PLAIN);
    assert!(!ok, "out-of-bounds must fail: {out}");
    // The panic text names the (temporary) source file, so check everything
    // else: the message, and the call-stack frame the panic is attributed to.
    assert!(
        out.contains("array index out of bounds: the length is 2 but the index is 5"),
        "unexpected panic text: {out}"
    );
    assert!(out.contains("0: f at"), "missing call frame: {out}");
}
