//! Control flow inside a body the walker keeps as an HIR island
//! (willow-0g8j.2.16).
//!
//! LIR lowers a function-level `if` into blocks of its own, but a `match` arm,
//! a `select` case and a `defer` block stay in HIR shape, and the statements
//! admitted inside them were only `expr`, `name = ...` and `let`. An `if` there
//! took the whole function back to the AST emitter — whatever the `if`
//! contained, which is why the reported reason used to blame the `panic` inside
//! it rather than the `if` itself.
//!
//! The second refusal is the same shape from the other side: a `match` every
//! arm of which leaves. The walker admitted one only when the checker had typed
//! the `match` itself `!`, and the checker types it that way only where the
//! surrounding code needs it — the same all-arms-return `match` written as a
//! function body's ending is typed by its arms and was refused. Divergence is
//! now read off the arms, and admitted wherever nothing follows the `match` in
//! the same Cranelift block.
//!
//! Every test is differential: the same source under the AST emitter and under
//! the walker must print the same thing, and `WILLOW_LIR_REQUIRE=1` turns any
//! fallback into a compile error, so a coverage regression cannot pass by
//! comparing the AST path against itself.
//!
//! 25 perspectives:
//!   1 a guard before the arm's return    14 an arm-local String under GC
//!   2 the guarded panic actually fires   15 every arm returns
//!   3 both branches return               16 return and panic mixed
//!   4 a guard that falls through         17 an all-leaving match nested
//!   5 a branch writes an outer local     18 an i64 scrutinee with `_`
//!   6 both branches write it             19 a String result from a guard
//!   7 nested ifs in one arm              20 an f64 arm with a guard
//!   8 a `let` inside a branch            21 a bool scrutinee, guard per arm
//!   9 a GC `let` inside a branch         22 a guard in a downcast arm
//!  10 the guard reads the binding        23 an `if` inside a `defer` block
//!  11 an arm inside a `while`            24 an `if` inside a `select` case
//!  12 an arm inside a `for`              25 both examples are fully LIR
//!  13 an arm inside a lambda

use super::support::{compile_and_run_with_env, compile_with_compiler_env};

const AST: [(&str, &str); 1] = [("WILLOW_LIR_BACKEND", "0")];
const LIR: [(&str, &str); 2] = [("WILLOW_LIR_BACKEND", "1"), ("WILLOW_LIR_REQUIRE", "1")];
const LIR_STRESS: [(&str, &str); 3] = [
    ("WILLOW_LIR_BACKEND", "1"),
    ("WILLOW_LIR_REQUIRE", "1"),
    ("WILLOW_GC_STRESS", "alloc"),
];
const LIR_LOG: [(&str, &str); 3] = [
    ("WILLOW_LIR_BACKEND", "1"),
    ("WILLOW_LIR_REQUIRE", "1"),
    ("WILLOW_LIR_LOG", "1"),
];

/// `expected` must come out of all three configurations, and `functions` must
/// each be named in the walker's selection log — otherwise the second copy of
/// the right answer came from the AST emitter too.
fn assert_arm_control_flow(source: &str, expected: &str, functions: &[&str]) {
    for env in [&AST[..], &LIR[..], &LIR_STRESS[..]] {
        let (out, ok) = compile_and_run_with_env(source, env);
        assert!(ok, "run failed under {env:?}: {out}");
        assert_eq!(out, expected, "wrong output under {env:?}");
    }
    assert_walker_compiled(source, functions);
}

fn assert_walker_compiled(source: &str, functions: &[&str]) {
    let (ok, stderr) = compile_with_compiler_env(source, &LIR_LOG);
    assert!(ok, "logged LIR compile failed: {stderr}");
    for function in functions {
        let sync = format!("[lir] compiling `{function}` from lowered IR");
        let coop = format!("[lir] compiling async `{function}` from lowered IR");
        assert!(
            stderr.contains(&sync) || stderr.contains(&coop),
            "`{function}` did not use the LIR walker: {stderr}"
        );
    }
}

/// A program that panics under a guard: both emitters must fail, and the
/// message must be the one the guarded `panic` carries.
fn assert_same_panic(source: &str, message: &str) {
    for env in [&AST[..], &LIR[..]] {
        let (out, ok) = compile_and_run_with_env(source, env);
        assert!(!ok, "expected a panic under {env:?}: {out}");
        assert!(
            out.contains(message),
            "report under {env:?} is missing `{message}`:\n{out}"
        );
    }
}

const SHAPE: &str = "enum Shape { Square(i64), Circle(i64) }\n";

// 1. The bead's own repro: a guard that would leave, on the path where it does
//    not, followed by the arm's ordinary `return`.
#[test]
fn arm_flow_01_guard_before_the_arms_return() {
    assert_arm_control_flow(
        &format!(
            "{SHAPE}\
             fn measure(s: Shape) -> i64 {{\n\
             \x20   match s {{\n\
             \x20       Shape::Square(side) => {{\n\
             \x20           if side <= 0 {{ panic(\"a square needs a positive side\"); }}\n\
             \x20           return side * side;\n\
             \x20       }}\n\
             \x20       Shape::Circle(r) => {{ return 3 * r * r; }}\n\
             \x20   }}\n\
             }}\n\
             fn main() {{ println(measure(Shape::Square(4))); println(measure(Shape::Circle(2))); }}\n"
        ),
        "16\n12\n",
        &["measure", "main"],
    );
}

// 2. The other path through the same guard. A `panic` inside an arm's `if` has
//    to actually unwind, not merely compile.
#[test]
fn arm_flow_02_the_guarded_panic_fires() {
    assert_same_panic(
        &format!(
            "{SHAPE}\
             fn measure(s: Shape) -> i64 {{\n\
             \x20   match s {{\n\
             \x20       Shape::Square(side) => {{\n\
             \x20           if side <= 0 {{ panic(\"a square needs a positive side\"); }}\n\
             \x20           return side * side;\n\
             \x20       }}\n\
             \x20       Shape::Circle(r) => {{ return 3 * r * r; }}\n\
             \x20   }}\n\
             }}\n\
             fn main() {{ println(measure(Shape::Square(0 - 1))); }}\n"
        ),
        "a square needs a positive side",
    );
}

// 3. Both branches leave, so nothing follows the `if` and the arm's block ends
//    inside it.
#[test]
fn arm_flow_03_both_branches_return() {
    assert_arm_control_flow(
        &format!(
            "{SHAPE}\
             fn measure(s: Shape) -> i64 {{\n\
             \x20   match s {{\n\
             \x20       Shape::Square(side) => {{\n\
             \x20           if side > 10 {{ return 100; }} else {{ return side * side; }}\n\
             \x20       }}\n\
             \x20       Shape::Circle(r) => {{ return r; }}\n\
             \x20   }}\n\
             }}\n\
             fn main() {{ println(measure(Shape::Square(20))); println(measure(Shape::Square(3))); }}\n"
        ),
        "100\n9\n",
        &["measure", "main"],
    );
}

// 4. A guard whose branch falls through: control reaches the statement after
//    the `if`, which is the join block the emitter built for it.
#[test]
fn arm_flow_04_a_guard_that_falls_through() {
    assert_arm_control_flow(
        &format!(
            "{SHAPE}\
             fn measure(s: Shape) -> i64 {{\n\
             \x20   match s {{\n\
             \x20       Shape::Square(side) => {{\n\
             \x20           if side > 100 {{ return 0; }}\n\
             \x20           if side < 0 {{ return 0; }}\n\
             \x20           return side * side;\n\
             \x20       }}\n\
             \x20       Shape::Circle(r) => {{ return r; }}\n\
             \x20   }}\n\
             }}\n\
             fn main() {{ println(measure(Shape::Square(5))); println(measure(Shape::Square(200))); }}\n"
        ),
        "25\n0\n",
        &["measure", "main"],
    );
}

// 5. An arm run for its effect: the branch writes a local of the enclosing
//    function, so the write has to survive the branch's own scope restore.
#[test]
fn arm_flow_05_a_branch_writes_an_outer_local() {
    assert_arm_control_flow(
        &format!(
            "{SHAPE}\
             fn main() {{\n\
             \x20   let mut n = 0;\n\
             \x20   let s = Shape::Square(6);\n\
             \x20   match s {{\n\
             \x20       Shape::Square(side) => {{ if side > 5 {{ n = side; }} }}\n\
             \x20       Shape::Circle(r) => {{ n = r; }}\n\
             \x20   }}\n\
             \x20   println(n);\n\
             }}\n"
        ),
        "6\n",
        &["main"],
    );
}

// 6. Both branches write it, so the join block reads a value defined on two
//    different incoming edges.
#[test]
fn arm_flow_06_both_branches_write_an_outer_local() {
    assert_arm_control_flow(
        &format!(
            "{SHAPE}\
             fn pick(s: Shape) -> i64 {{\n\
             \x20   let mut n = 0;\n\
             \x20   match s {{\n\
             \x20       Shape::Square(side) => {{\n\
             \x20           if side > 5 {{ n = side; }} else {{ n = 0 - side; }}\n\
             \x20       }}\n\
             \x20       Shape::Circle(r) => {{ n = r; }}\n\
             \x20   }}\n\
             \x20   return n;\n\
             }}\n\
             fn main() {{ println(pick(Shape::Square(7))); println(pick(Shape::Square(2))); }}\n"
        ),
        "7\n-2\n",
        &["pick", "main"],
    );
}

// 7. Nesting: an `if` inside a branch of another `if`, both inside one arm.
#[test]
fn arm_flow_07_nested_ifs_in_one_arm() {
    assert_arm_control_flow(
        &format!(
            "{SHAPE}\
             fn grade(s: Shape) -> i64 {{\n\
             \x20   match s {{\n\
             \x20       Shape::Square(side) => {{\n\
             \x20           if side > 0 {{\n\
             \x20               if side > 10 {{ return 2; }} else {{ return 1; }}\n\
             \x20           }}\n\
             \x20           return 0;\n\
             \x20       }}\n\
             \x20       Shape::Circle(r) => {{ return r; }}\n\
             \x20   }}\n\
             }}\n\
             fn main() {{\n\
             \x20   println(grade(Shape::Square(20)));\n\
             \x20   println(grade(Shape::Square(4)));\n\
             \x20   println(grade(Shape::Square(0 - 1)));\n\
             }}\n"
        ),
        "2\n1\n0\n",
        &["grade", "main"],
    );
}

// 8. A `let` declared inside a branch is scoped to that branch — the emitter
//    snapshots `vars` around each one, exactly as it does around an arm.
#[test]
fn arm_flow_08_a_let_inside_a_branch() {
    assert_arm_control_flow(
        &format!(
            "{SHAPE}\
             fn measure(s: Shape) -> i64 {{\n\
             \x20   match s {{\n\
             \x20       Shape::Square(side) => {{\n\
             \x20           if side > 0 {{\n\
             \x20               let doubled = side * 2;\n\
             \x20               return doubled + 1;\n\
             \x20           }}\n\
             \x20           return 0;\n\
             \x20       }}\n\
             \x20       Shape::Circle(r) => {{ return r; }}\n\
             \x20   }}\n\
             }}\n\
             fn main() {{ println(measure(Shape::Square(4))); }}\n"
        ),
        "9\n",
        &["measure", "main"],
    );
}

// 9. The GC-managed version of 8: the branch's rooted slot has to be popped on
//    the way to the join, or the shadow stack drifts.
#[test]
fn arm_flow_09_a_gc_let_inside_a_branch() {
    assert_arm_control_flow(
        &format!(
            "{SHAPE}\
             fn name(s: Shape) -> String {{\n\
             \x20   let mut out = \"?\";\n\
             \x20   match s {{\n\
             \x20       Shape::Square(side) => {{\n\
             \x20           if side > 0 {{\n\
             \x20               let tag = \"square \" + \"ok\";\n\
             \x20               out = tag;\n\
             \x20           }} else {{\n\
             \x20               let tag = \"square \" + \"bad\";\n\
             \x20               out = tag;\n\
             \x20           }}\n\
             \x20       }}\n\
             \x20       Shape::Circle(r) => {{ out = \"circle\"; }}\n\
             \x20   }}\n\
             \x20   return out;\n\
             }}\n\
             fn main() {{\n\
             \x20   println(name(Shape::Square(2)));\n\
             \x20   println(name(Shape::Square(0)));\n\
             \x20   println(name(Shape::Circle(1)));\n\
             }}\n"
        ),
        "square ok\nsquare bad\ncircle\n",
        &["name", "main"],
    );
}

// 10. The guard's condition reads the arm's own pattern binding, which the
//     branch check has to see in scope.
#[test]
fn arm_flow_10_the_guard_reads_the_binding() {
    assert_arm_control_flow(
        "enum Tagged { Named(String), Anonymous }\n\
         fn describe(t: Tagged) -> String {\n\
         \x20   match t {\n\
         \x20       Tagged::Named(n) => {\n\
         \x20           if n == \"root\" { return \"the root\"; }\n\
         \x20           return n;\n\
         \x20       }\n\
         \x20       Tagged::Anonymous => { return \"anonymous\"; }\n\
         \x20   }\n\
         }\n\
         fn main() {\n\
         \x20   println(describe(Tagged::Named(\"root\")));\n\
         \x20   println(describe(Tagged::Named(\"leaf\")));\n\
         \x20   println(describe(Tagged::Anonymous));\n\
         }\n",
        "the root\nleaf\nanonymous\n",
        &["describe", "main"],
    );
}

// 11. The arm sits inside a `while`, so the blocks the `if` builds are created
//     once but entered on every iteration.
#[test]
fn arm_flow_11_an_arm_inside_a_while() {
    assert_arm_control_flow(
        &format!(
            "{SHAPE}\
             fn main() {{\n\
             \x20   let mut i = 0;\n\
             \x20   let mut total = 0;\n\
             \x20   while i < 5 {{\n\
             \x20       let s = Shape::Square(i);\n\
             \x20       match s {{\n\
             \x20           Shape::Square(side) => {{ if side % 2 == 0 {{ total = total + side; }} }}\n\
             \x20           Shape::Circle(r) => {{ total = total + r; }}\n\
             \x20       }}\n\
             \x20       i = i + 1;\n\
             \x20   }}\n\
             \x20   println(total);\n\
             }}\n"
        ),
        "6\n",
        &["main"],
    );
}

// 12. The same inside a `for`, whose body the walker lowers differently.
#[test]
fn arm_flow_12_an_arm_inside_a_for() {
    assert_arm_control_flow(
        &format!(
            "{SHAPE}\
             fn main() {{\n\
             \x20   let mut total = 0;\n\
             \x20   for i in 0..5 {{\n\
             \x20       match Shape::Square(i) {{\n\
             \x20           Shape::Square(side) => {{\n\
             \x20               if side > 2 {{ total = total + side; }} else {{ total = total + 1; }}\n\
             \x20           }}\n\
             \x20           Shape::Circle(r) => {{ total = total + r; }}\n\
             \x20       }}\n\
             \x20   }}\n\
             \x20   println(total);\n\
             }}\n"
        ),
        "10\n",
        &["main"],
    );
}

// 13. A lambda is compiled under its own symbol, so its arm bodies are vetted
//     and emitted on their own terms.
#[test]
fn arm_flow_13_an_arm_inside_a_lambda() {
    assert_arm_control_flow(
        &format!(
            "{SHAPE}\
             fn apply(f: fn(Shape) -> i64, s: Shape) -> i64 {{ return f(s); }}\n\
             fn main() {{\n\
             \x20   let measure = |s: Shape| -> i64 {{\n\
             \x20       match s {{\n\
             \x20           Shape::Square(side) => {{\n\
             \x20               if side < 0 {{ return 0; }}\n\
             \x20               return side * side;\n\
             \x20           }}\n\
             \x20           Shape::Circle(r) => {{ return r; }}\n\
             \x20       }}\n\
             \x20   }};\n\
             \x20   println(apply(measure, Shape::Square(3)));\n\
             \x20   println(apply(measure, Shape::Square(0 - 3)));\n\
             }}\n"
        ),
        "9\n0\n",
        &["apply", "main"],
    );
}

// 14. An arm-local String kept live across a branch that allocates. Under
//     `WILLOW_GC_STRESS=alloc` every allocation collects, so an unrooted slot
//     shows up as a corrupted read rather than as a leak.
#[test]
fn arm_flow_14_an_arm_local_string_under_gc() {
    assert_arm_control_flow(
        &format!(
            "{SHAPE}\
             fn render(s: Shape) -> String {{\n\
             \x20   match s {{\n\
             \x20       Shape::Square(side) => {{\n\
             \x20           let head = \"square\";\n\
             \x20           if side > 0 {{\n\
             \x20               let tail = \" of \" + \"side\";\n\
             \x20               return head + tail;\n\
             \x20           }}\n\
             \x20           return head + \" (degenerate)\";\n\
             \x20       }}\n\
             \x20       Shape::Circle(r) => {{ return \"circle\"; }}\n\
             \x20   }}\n\
             }}\n\
             fn main() {{\n\
             \x20   println(render(Shape::Square(1)));\n\
             \x20   println(render(Shape::Square(0)));\n\
             }}\n"
        ),
        "square of side\nsquare (degenerate)\n",
        &["render", "main"],
    );
}

// 15. Every arm returns and the `match` is the function's whole body. The
//     checker does not type this one `!` — the arms do the leaving — and the
//     walker used to require that type before admitting the shape.
#[test]
fn arm_flow_15_every_arm_returns() {
    assert_arm_control_flow(
        &format!(
            "{SHAPE}\
             fn measure(s: Shape) -> i64 {{\n\
             \x20   match s {{\n\
             \x20       Shape::Square(side) => {{ return side * side; }}\n\
             \x20       Shape::Circle(r) => {{ return 3 * r * r; }}\n\
             \x20   }}\n\
             }}\n\
             fn main() {{ println(measure(Shape::Square(4))); println(measure(Shape::Circle(2))); }}\n"
        ),
        "16\n12\n",
        &["measure", "main"],
    );
}

// 16. The mixed form: one arm returns, the other unwinds. Still every path
//     leaves, so the merge block is unreachable.
#[test]
fn arm_flow_16_return_and_panic_mixed() {
    assert_arm_control_flow(
        &format!(
            "{SHAPE}\
             fn squares_only(s: Shape) -> i64 {{\n\
             \x20   match s {{\n\
             \x20       Shape::Square(side) => {{ return side * side; }}\n\
             \x20       Shape::Circle(r) => {{ panic(\"circles are not measured here\"); }}\n\
             \x20   }}\n\
             }}\n\
             fn main() {{ println(squares_only(Shape::Square(5))); }}\n"
        ),
        "25\n",
        &["squares_only", "main"],
    );
}

// 17. Divergence nests: the inner `match` leaves on every arm, which makes the
//     outer arm holding it leave too.
#[test]
fn arm_flow_17_an_all_leaving_match_nested() {
    assert_arm_control_flow(
        &format!(
            "{SHAPE}\
             fn combine(a: Shape, b: Shape) -> i64 {{\n\
             \x20   match a {{\n\
             \x20       Shape::Square(side) => {{\n\
             \x20           match b {{\n\
             \x20               Shape::Square(other) => {{ return side + other; }}\n\
             \x20               Shape::Circle(r) => {{ return side - r; }}\n\
             \x20           }}\n\
             \x20       }}\n\
             \x20       Shape::Circle(r) => {{ return r; }}\n\
             \x20   }}\n\
             }}\n\
             fn main() {{\n\
             \x20   println(combine(Shape::Square(4), Shape::Square(6)));\n\
             \x20   println(combine(Shape::Square(4), Shape::Circle(1)));\n\
             \x20   println(combine(Shape::Circle(9), Shape::Square(1)));\n\
             }}\n"
        ),
        "10\n3\n9\n",
        &["combine", "main"],
    );
}

// 18. An i64 scrutinee with a `_` catch-all, so the last arm always matches and
//     the emitter never builds a fallthrough edge into the merge block.
#[test]
fn arm_flow_18_an_i64_scrutinee_with_a_wildcard() {
    assert_arm_control_flow(
        "fn kind(n: i64) -> String {\n\
         \x20   match n {\n\
         \x20       0 => { return \"zero\"; }\n\
         \x20       1 => {\n\
         \x20           if n == 1 { return \"one\"; }\n\
         \x20           return \"impossible\";\n\
         \x20       }\n\
         \x20       _ => { return \"many\"; }\n\
         \x20   }\n\
         }\n\
         fn main() { println(kind(0)); println(kind(1)); println(kind(9)); }\n",
        "zero\none\nmany\n",
        &["kind", "main"],
    );
}

// 19. A String result produced under a guard: the value crosses the join as a
//     GC reference, not as an integer.
#[test]
fn arm_flow_19_a_string_result_from_a_guard() {
    assert_arm_control_flow(
        &format!(
            "{SHAPE}\
             fn label(s: Shape) -> String {{\n\
             \x20   match s {{\n\
             \x20       Shape::Square(side) => {{\n\
             \x20           if side > 3 {{ return \"big square\"; }}\n\
             \x20           return \"small square\";\n\
             \x20       }}\n\
             \x20       Shape::Circle(r) => {{ return \"circle\"; }}\n\
             \x20   }}\n\
             }}\n\
             fn main() {{\n\
             \x20   println(label(Shape::Square(9)));\n\
             \x20   println(label(Shape::Square(1)));\n\
             \x20   println(label(Shape::Circle(1)));\n\
             }}\n"
        ),
        "big square\nsmall square\ncircle\n",
        &["label", "main"],
    );
}

// 20. An f64 arm: the branch condition and the returned value live in a
//     different Cranelift type from every perspective above.
#[test]
fn arm_flow_20_an_f64_arm_with_a_guard() {
    assert_arm_control_flow(
        "enum Reading { Value(f64), Missing }\n\
         fn clamp(r: Reading) -> f64 {\n\
         \x20   match r {\n\
         \x20       Reading::Value(v) => {\n\
         \x20           if v > 10.0 { return 10.0; }\n\
         \x20           if v < 0.0 { return 0.0; }\n\
         \x20           return v;\n\
         \x20       }\n\
         \x20       Reading::Missing => { return 0.0; }\n\
         \x20   }\n\
         }\n\
         fn main() {\n\
         \x20   println(clamp(Reading::Value(12.5)));\n\
         \x20   println(clamp(Reading::Value(2.5)));\n\
         \x20   println(clamp(Reading::Missing));\n\
         }\n",
        "10\n2.5\n0.0\n",
        &["clamp", "main"],
    );
}

// 21. A bool scrutinee, with a guard in each arm: two arms, two `if`s, four
//     branch blocks feeding two joins.
#[test]
fn arm_flow_21_a_bool_scrutinee_with_a_guard_per_arm() {
    assert_arm_control_flow(
        "fn decide(flag: bool, n: i64) -> i64 {\n\
         \x20   match flag {\n\
         \x20       true => {\n\
         \x20           if n > 0 { return n; }\n\
         \x20           return 0;\n\
         \x20       }\n\
         \x20       false => {\n\
         \x20           if n > 0 { return 0 - n; }\n\
         \x20           return 0;\n\
         \x20       }\n\
         \x20   }\n\
         }\n\
         fn main() {\n\
         \x20   println(decide(true, 5));\n\
         \x20   println(decide(false, 5));\n\
         \x20   println(decide(true, 0 - 5));\n\
         }\n",
        "5\n-5\n0\n",
        &["decide", "main"],
    );
}

// 22. A class-downcast arm on an interface value, with a guard that calls the
//     downcast instance's own method.
#[test]
fn arm_flow_22_a_guard_in_a_downcast_arm() {
    assert_arm_control_flow(
        "interface Animal { fn name(self) -> String; }\n\
         class Dog implements Animal {\n\
         \x20   pub fn name(self) -> String { return \"Rex\"; }\n\
         \x20   pub fn legs(self) -> i64 { return 4; }\n\
         }\n\
         class Bird implements Animal {\n\
         \x20   pub fn name(self) -> String { return \"Pip\"; }\n\
         }\n\
         fn legs(a: Animal) -> i64 {\n\
         \x20   match a {\n\
         \x20       Dog(d) => {\n\
         \x20           if d.legs() > 2 { return d.legs(); }\n\
         \x20           return 0;\n\
         \x20       }\n\
         \x20       _ => { return 2; }\n\
         \x20   }\n\
         }\n\
         fn main() { println(legs(new Dog())); println(legs(new Bird())); }\n",
        "4\n2\n",
        &["legs", "main"],
    );
}

// 23. A `defer` block is replayed by the unwinder with no root bracket of its
//     own, so an `if` there is admitted but a `let` inside it is not.
#[test]
fn arm_flow_23_an_if_inside_a_defer_block() {
    assert_arm_control_flow(
        "fn run(n: i64) -> i64 {\n\
         \x20   let mut seen = 0;\n\
         \x20   defer {\n\
         \x20       if n > 0 {\n\
         \x20           println(\"positive\");\n\
         \x20       } else {\n\
         \x20           println(\"non-positive\");\n\
         \x20       }\n\
         \x20   }\n\
         \x20   seen = n;\n\
         \x20   return seen;\n\
         }\n\
         fn main() { println(run(3)); println(run(0)); }\n",
        "positive\n3\nnon-positive\n0\n",
        &["run", "main"],
    );
}

// 24. A `select` case body is bracketed like an arm, so it takes the same `if`.
#[test]
fn arm_flow_24_an_if_inside_a_select_case() {
    assert_arm_control_flow(
        "async fn feed(ch: Channel<i64>) -> i64 {\n\
         \x20   ch.send(7);\n\
         \x20   return 0;\n\
         }\n\
         async fn consume(ch: Channel<i64>) -> i64 {\n\
         \x20   let mut total = 0;\n\
         \x20   select {\n\
         \x20       let v = ch.recv() => {\n\
         \x20           if v > 5 {\n\
         \x20               total = total + v;\n\
         \x20           } else {\n\
         \x20               total = total - v;\n\
         \x20           }\n\
         \x20       }\n\
         \x20   }\n\
         \x20   return total;\n\
         }\n\
         async fn main() {\n\
         \x20   let ch = Channel<i64>::new();\n\
         \x20   let f = feed(ch);\n\
         \x20   let c = consume(ch);\n\
         \x20   println(await c);\n\
         \x20   await f;\n\
         }\n",
        "7\n",
        &["consume"],
    );
}

// 25. The two examples that carry these shapes compile entirely through the
//     walker, so a future refusal shows up here and not only as a coverage
//     number nobody re-measures.
#[test]
fn arm_flow_25_the_examples_are_fully_lir() {
    for example in [
        "example/match_arm_control_flow.wi",
        "example/return_paths.wi",
    ] {
        let source = std::fs::read_to_string(example).expect("example is readable");
        let (ok, stderr) = compile_with_compiler_env(&source, &LIR_LOG);
        assert!(ok, "{example} did not compile under the walker: {stderr}");
        assert!(
            !stderr.contains("fell back to the AST backend"),
            "{example} fell back: {stderr}"
        );
    }
}
