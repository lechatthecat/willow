use super::*;

#[test]
fn lir_diff_12_string_param_roundtrip() {
    assert_program_output(
        r#"
fn shout(s: String) -> String { return s; }
fn main() { println(shout("hello")); }
"#,
        "hello\n",
    );
}

#[test]
fn lir_diff_13_string_concat_chain() {
    assert_program_output(
        r#"
fn tag(a: String, b: String) -> String { return "<" + a + "/" + b + ">"; }
fn main() { println(tag("x", "y")); }
"#,
        "<x/y>\n",
    );
}

#[test]
fn lir_diff_14_string_equality() {
    assert_program_output(
        r#"
fn same(a: String, b: String) -> bool { return a == b; }
fn main() {
    println(same("ab", "a" + "b"));
    println(same("ab", "ac"));
}
"#,
        "true\nfalse\n",
    );
}

#[test]
fn lir_diff_15_string_inequality() {
    assert_program_output(
        r#"
fn differs(a: String, b: String) -> bool { return a != b; }
fn main() {
    println(differs("ab", "a" + "b"));
    println(differs("ab", "ac"));
}
"#,
        "false\ntrue\n",
    );
}

#[test]
fn lir_diff_16_string_ternary() {
    assert_program_output(
        r#"
fn pick(c: bool) -> String { return c ? "yes" + "!" : "no" + "?"; }
fn main() { println(pick(true)); println(pick(false)); }
"#,
        "yes!\nno?\n",
    );
}

#[test]
fn lir_diff_17_string_loop_accumulator() {
    // The reassigned GC local: one entry root, not one per iteration.
    assert_program_output(
        r#"
fn rep(n: i64) -> String {
    let mut s = "";
    let mut i = 0;
    while i < n {
        s = s + "ab";
        i = i + 1;
    }
    return s;
}
fn main() { println(rep(4)); println(rep(0)); }
"#,
        "abababab\n\n",
    );
}

#[test]
fn lir_diff_18_mixed_scalar_and_gc_locals() {
    assert_program_output(
        r#"
fn describe(n: i64) -> String {
    let doubled = n * 2;
    let label = "n=";
    let big = doubled > 10;
    return big ? label + "big" : label + "small";
}
fn main() { println(describe(9)); println(describe(2)); }
"#,
        "n=big\nn=small\n",
    );
}

#[test]
fn lir_diff_19_nested_string_calls() {
    assert_program_output(
        r#"
fn wrap(s: String) -> String { return "[" + s + "]"; }
fn twice(s: String) -> String { return wrap(wrap(s)); }
fn main() { println(twice("q")); }
"#,
        "[[q]]\n",
    );
}

#[test]
fn lir_diff_20_local_live_across_later_allocation() {
    // `a` is only reachable through its slot while `b` and `c` allocate.
    assert_program_output(
        r#"
fn three() -> String {
    let a = "x" + "1";
    let b = "y" + "2";
    let c = "z" + "3";
    return a + b + c;
}
fn main() { println(three()); }
"#,
        "x1y2z3\n",
    );
}

#[test]
fn lir_diff_21_local_live_across_allocation_under_gc_stress() {
    // Same program as 20, but every allocation collects: without the entry
    // root `a` and `b` are reclaimed and the output is garbage.
    assert_output_under_gc_stress(
        r#"
fn three() -> String {
    let a = "x" + "1";
    let b = "y" + "2";
    let c = "z" + "3";
    return a + b + c;
}
fn main() {
    let mut i = 0;
    while i < 20 {
        println(three());
        i = i + 1;
    }
}
"#,
        &"x1y2z3\n".repeat(20),
    );
}

#[test]
fn lir_diff_22_loop_accumulator_under_gc_stress() {
    assert_output_under_gc_stress(
        r#"
fn rep(n: i64) -> String {
    let mut s = "";
    let mut i = 0;
    while i < n {
        s = s + "ab";
        i = i + 1;
    }
    return s;
}
fn main() {
    let mut k = 0;
    while k < 10 {
        println(rep(5));
        k = k + 1;
    }
}
"#,
        &"ababababab\n".repeat(10),
    );
}

#[test]
fn lir_diff_23_many_live_gc_locals_under_gc_stress() {
    assert_output_under_gc_stress(
        r#"
fn combine() -> String {
    let a = "a" + "1";
    let b = "b" + "2";
    let c = "c" + "3";
    let d = "d" + "4";
    let e = "e" + "5";
    return a + b + c + d + e;
}
fn main() {
    let mut i = 0;
    while i < 15 {
        println(combine());
        i = i + 1;
    }
}
"#,
        &"a1b2c3d4e5\n".repeat(15),
    );
}

#[test]
fn lir_diff_24_gc_call_argument_rooted_across_later_argument() {
    // The first argument is built, then the SECOND argument allocates: the
    // first must be a root while that happens.
    assert_output_under_gc_stress(
        r#"
fn join(a: String, b: String) -> String { return a + "|" + b; }
fn main() {
    let mut i = 0;
    while i < 15 {
        println(join("l" + "1", "r" + "2"));
        i = i + 1;
    }
}
"#,
        &"l1|r2\n".repeat(15),
    );
}

#[test]
fn lir_diff_25_concat_lhs_rooted_across_rhs_allocation() {
    assert_output_under_gc_stress(
        r#"
fn build() -> String { return ("p" + "q") + ("r" + "s"); }
fn main() {
    let mut i = 0;
    while i < 15 {
        println(build());
        i = i + 1;
    }
}
"#,
        &"pqrs\n".repeat(15),
    );
}

#[test]
fn lir_diff_26_equality_operands_survive_collection() {
    assert_output_under_gc_stress(
        r#"
fn matches() -> bool { return ("a" + "b") == ("a" + "b"); }
fn main() {
    let mut i = 0;
    while i < 15 {
        println(matches());
        i = i + 1;
    }
}
"#,
        &"true\n".repeat(15),
    );
}

#[test]
fn lir_diff_27_early_return_out_of_loop_balances_roots() {
    // A `return` from inside the loop must pop exactly the roots this frame
    // pushed; if it popped too few, the caller's own roots would leak and the
    // repeated calls below would drift.
    assert_output_under_gc_stress(
        r#"
fn first_long(n: i64) -> String {
    let prefix = "p" + "-";
    let mut i = 0;
    let mut acc = "";
    while i < n {
        acc = acc + "z";
        if i == 2 {
            return prefix + acc;
        }
        i = i + 1;
    }
    return prefix + "none";
}
fn main() {
    let mut k = 0;
    while k < 15 {
        println(first_long(10));
        println(first_long(1));
        k = k + 1;
    }
}
"#,
        &"p-zzz\np-none\n".repeat(15),
    );
}

#[test]
fn lir_diff_28_deep_recursion_with_gc_locals() {
    // Every frame pushes roots at entry and pops them at return; an imbalance
    // would either exhaust the root stack or free a live parent value.
    assert_output_under_gc_stress(
        r#"
fn chain(n: i64) -> String {
    let here = "n" + "!";
    if n <= 0 { return here; }
    let rest = chain(n - 1);
    return here + rest;
}
fn main() { println(chain(60)); }
"#,
        &format!("{}\n", "n!".repeat(61)),
    );
}

#[test]
fn lir_diff_29_void_fn_with_gc_locals_pops_roots() {
    // No `return` statement at all: the implicit void return has to pop.
    assert_output_under_gc_stress(
        r#"
fn emit(tag: String) {
    let body = tag + "-body";
    let full = body + "!";
    println(full);
}
fn main() {
    let mut i = 0;
    while i < 15 {
        emit("t");
        i = i + 1;
    }
}
"#,
        &"t-body!\n".repeat(15),
    );
}

#[test]
fn lir_diff_30_unassigned_gc_slot_is_scanned_safely() {
    // `late` is rooted from entry but only assigned after several collections
    // have already scanned its slot; the null initialization is what keeps
    // that scan from reading stack garbage.
    assert_output_under_gc_stress(
        r#"
fn late_binding(n: i64) -> String {
    let mut i = 0;
    let mut churn = "";
    while i < n {
        churn = churn + "c";
        i = i + 1;
    }
    let late = churn + "-late";
    return late;
}
fn main() {
    let mut k = 0;
    while k < 10 {
        println(late_binding(3));
        k = k + 1;
    }
}
"#,
        &"ccc-late\n".repeat(10),
    );
}

#[test]
fn lir_diff_31_print_and_println_of_strings() {
    assert_program_output(
        r#"
fn show(s: String) {
    print(s);
    print("|");
    println(s + s);
}
fn main() { show("ab"); }
"#,
        "ab|abab\n",
    );
}
