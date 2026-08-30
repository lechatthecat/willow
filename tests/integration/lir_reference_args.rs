//! `&place` arguments compiled from Lowered IR (willow-0g8j.2.17).
//!
//! The walker could already lower a reference argument — it just refused to in
//! debug builds, because a debug build also emits the reference-call hook that
//! lets a panic inside the callee name the place the caller handed it, and only
//! the AST emitter knew how to write that record. The walker now emits it, from
//! the lowered node, at every call site the AST path emits it from and at none
//! of the ones it skips.
//!
//! Every string the hook passes has to be byte-identical to the AST spelling.
//! The literals are declared once, from the AST, before any function is
//! compiled, and `emit_string_literal` answers an undeclared string with a null
//! pointer — so a place name only this path could invent would reach the
//! runtime as a null. That is why the panic perspectives compare whole reports
//! rather than merely checking that a report exists.
//!
//! Putting `&x` into the walker's subset also made a long-standing storage bug
//! reachable from two emitters instead of one. A local whose address is taken
//! used to be promoted from an SSA variable to a stack slot AT THE `&`, which
//! put the promoting store wherever that use sat in the control-flow graph: it
//! re-ran on every iteration of an enclosing loop (losing the callee's writes)
//! and never ran at all on a branch that took no address (reading uninitialised
//! bytes). Address-taken locals are now stack-backed from their declaration in
//! both emitters. Perspectives 16, 17, 22, 24, 26, 27 and 28 are that fix.
//!
//! Every test is differential: the same source under the AST emitter and under
//! the walker must print the same thing, and `WILLOW_LIR_REQUIRE=1` turns any
//! fallback into a compile error so a coverage regression cannot pass by
//! comparing the AST path against itself.
//!
//! An indirect call had the same half-record for longer still: interface
//! dispatch wrote the reference-call context and never cleared it, on BOTH
//! emitters, so a panic anywhere later in the same function was reported under
//! a place the callee had already given back. Perspectives 30, 31 and 32 are
//! that fix (willow-0g8j.11).
//!
//! 34 perspectives:
//!   1 `&mut i64` writes back            17 a reference call in one arm
//!   2 `&` reads without writing         18 a String reference under GC
//!   3 `&mut bool`                       19 `&` of a value parameter
//!   4 `&mut String` (GC-managed)        20 a panic under the call
//!   5 `& String` returns a new one      21 a panic after the call
//!   6 a field place                     22 an f64 reference in a loop
//!   7 the whole object rebound          23 a reference call in a lambda
//!   8 an array element, literal index   24 a nested field place in a loop
//!   9 an array element, variable index  25 a reference call in a ternary
//!  10 two reference parameters          26 a recursive reference parameter
//!  11 reference and value mixed         27 a local declared in the loop body
//!  12 an instance method                28 one local per branch
//!  13 a class static                    29 the example is fully LIR
//!  14 interface dispatch                30 a panic under interface dispatch
//!  15 virtual dispatch                  31 a panic after interface dispatch
//!  16 a reference call in a loop        32 a panic after virtual dispatch
//!                                        33 an enum binding reference after a loop
//!                                        34 a whole binding reference in a branch

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
fn assert_reference_args(source: &str, expected: &str, functions: &[&str]) {
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

/// The temporary source path differs per run, per emitter and per platform, so
/// a report is compared with every path ending in `.wi:` replaced by
/// `<src>.wi:` — the whole token, directories included.
fn without_source_paths(out: &str) -> String {
    out.lines()
        .map(|line| {
            let mut rest = line;
            let mut normalized = String::new();
            while let Some(end) = rest.find(".wi:") {
                let token = rest[..end]
                    .rfind(char::is_whitespace)
                    .map(|i| i + 1)
                    .unwrap_or(0);
                normalized.push_str(&rest[..token]);
                normalized.push_str("<src>.wi:");
                rest = &rest[end + ".wi:".len()..];
            }
            normalized.push_str(rest);
            normalized
        })
        .collect::<Vec<_>>()
        .join("\n")
}

/// A program that panics: both emitters must produce the same whole report,
/// and it must contain `expected_parts`.
fn assert_same_panic(source: &str, expected_parts: &[&str]) {
    let (ast_out, ast_ok) = compile_and_run_with_env(source, &AST);
    assert!(!ast_ok, "expected a panic on the AST path: {ast_out}");
    let (lir_out, lir_ok) = compile_and_run_with_env(source, &LIR);
    assert!(!lir_ok, "expected a panic on the LIR path: {lir_out}");
    let ast_report = without_source_paths(&ast_out);
    let lir_report = without_source_paths(&lir_out);
    assert_eq!(
        ast_report, lir_report,
        "the two emitters reported the panic differently"
    );
    for part in expected_parts {
        assert!(
            lir_report.contains(part),
            "report is missing `{part}`:\n{lir_report}"
        );
    }
}

// 1. The base case: the callee writes through the pointer and the caller's own
//    local is what changed.
#[test]
fn reference_args_01_a_mutable_i64_writes_back() {
    assert_reference_args(
        "fn bump(v: &mut i64) { v = v + 1; }\n\
         fn main() { let mut n = 41; bump(&n); println(n); }\n",
        "42\n",
        &["main", "bump"],
    );
}

// 2. An immutable `&` reads the caller's place and leaves it alone.
#[test]
fn reference_args_02_an_immutable_reference_only_reads() {
    assert_reference_args(
        "fn twice(v: & i64) -> i64 { return v + v; }\n\
         fn main() {\n\
        \x20   let n = 21;\n\
        \x20   println(twice(&n));\n\
        \x20   println(n);\n\
         }\n",
        "42\n21\n",
        &["main", "twice"],
    );
}

// 3. A bool is a byte-sized place; the pointer ABI is the same.
#[test]
fn reference_args_03_a_mutable_bool() {
    assert_reference_args(
        "fn flip(v: &mut bool) { v = !v; }\n\
         fn main() { let mut on = false; flip(&on); println(on); }\n",
        "true\n",
        &["main", "flip"],
    );
}

// 4. A GC-managed local is already stack-backed for the collector's sake, so
//    this is the case where the address the callee gets IS the root slot.
#[test]
fn reference_args_04_a_mutable_string() {
    assert_reference_args(
        "fn suffix(text: &mut String) { text = text + \"!\"; }\n\
         fn main() { let mut s = \"hi\"; suffix(&s); println(s); }\n",
        "hi!\n",
        &["main", "suffix"],
    );
}

// 5. Reading a String through `&` and returning a fresh one leaves the
//    caller's binding untouched.
#[test]
fn reference_args_05_an_immutable_string_reference() {
    assert_reference_args(
        "fn shout(text: & String) -> String { return text + \"?\"; }\n\
         fn main() {\n\
        \x20   let s = \"hi\";\n\
        \x20   println(shout(&s));\n\
        \x20   println(s);\n\
         }\n",
        "hi?\nhi\n",
        &["main", "shout"],
    );
}

// 6. A field is a place: the address is computed from the object pointer, so
//    no local storage decision is involved at all.
#[test]
fn reference_args_06_a_field_place() {
    assert_reference_args(
        "class Cell { pub n: i64; }\n\
         fn bump(v: &mut i64) { v = v + 1; }\n\
         fn main() {\n\
        \x20   let c = new Cell(7);\n\
        \x20   bump(&c.n);\n\
        \x20   println(c.n);\n\
         }\n",
        "8\n",
        &["main", "bump"],
    );
}

// 7. `&mut Cell` references the local holding the pointer, not the object, so
//    the callee can rebind the caller's variable to a different object.
#[test]
fn reference_args_07_the_whole_object_is_rebound() {
    assert_reference_args(
        "class Cell { pub n: i64; }\n\
         fn replace(c: &mut Cell) { c = new Cell(99); }\n\
         fn main() {\n\
        \x20   let mut c = new Cell(7);\n\
        \x20   replace(&c);\n\
        \x20   println(c.n);\n\
         }\n",
        "99\n",
        &["main", "replace"],
    );
}

// 8. An array element with a constant index.
#[test]
fn reference_args_08_an_array_element_with_a_literal_index() {
    assert_reference_args(
        "import std::collections::Array;\n\
         fn bump(v: &mut i64) { v = v + 1; }\n\
         fn main() {\n\
        \x20   let mut xs: Array<i64> = [1, 2];\n\
        \x20   bump(&xs[1]);\n\
        \x20   println(xs[0]);\n\
        \x20   println(xs[1]);\n\
         }\n",
        "1\n3\n",
        &["main", "bump"],
    );
}

// 9. The index can be a variable; the element address is still computed once,
//    before the call.
#[test]
fn reference_args_09_an_array_element_with_a_variable_index() {
    assert_reference_args(
        "import std::collections::Array;\n\
         fn bump(v: &mut i64) { v = v + 1; }\n\
         fn main() {\n\
        \x20   let mut xs: Array<i64> = [1, 2, 3];\n\
        \x20   let i = 2;\n\
        \x20   bump(&xs[i]);\n\
        \x20   println(xs[i]);\n\
         }\n",
        "4\n",
        &["main", "bump"],
    );
}

// 10. Two reference parameters in one call: both places must be addressed, and
//     the debug hook records the LAST one, exactly as the AST path does.
#[test]
fn reference_args_10_two_reference_parameters() {
    assert_reference_args(
        "fn spread(lo: &mut i64, hi: &mut i64) {\n\
        \x20   lo = lo - 1;\n\
        \x20   hi = hi + 1;\n\
         }\n\
         fn main() {\n\
        \x20   let mut lo = 10;\n\
        \x20   let mut hi = 20;\n\
        \x20   spread(&lo, &hi);\n\
        \x20   println(lo);\n\
        \x20   println(hi);\n\
         }\n",
        "9\n21\n",
        &["main", "spread"],
    );
}

// 11. Reference and value parameters mixed, with a return value: only the
//     reference slot takes a pointer.
#[test]
fn reference_args_11_reference_and_value_parameters_mixed() {
    assert_reference_args(
        "fn scale(v: &mut i64, by: i64, tag: String) -> String {\n\
        \x20   v = v * by;\n\
        \x20   return tag + \"!\";\n\
         }\n\
         fn main() {\n\
        \x20   let mut n = 6;\n\
        \x20   println(scale(&n, 7, \"done\"));\n\
        \x20   println(n);\n\
         }\n",
        "done!\n42\n",
        &["main", "scale"],
    );
}

// 12. An instance method's reference parameter sits after the hidden receiver,
//     so the mode list and the argument list are off by one.
#[test]
fn reference_args_12_an_instance_method() {
    assert_reference_args(
        "class Adder {\n\
        \x20   pub by: i64;\n\
        \x20   pub fn nudge(self, v: &mut i64) { v = v + self.by; }\n\
         }\n\
         fn main() {\n\
        \x20   let a = new Adder(5);\n\
        \x20   let mut n = 10;\n\
        \x20   a.nudge(&n);\n\
        \x20   println(n);\n\
         }\n",
        "15\n",
        &["main"],
    );
}

// 13. A class static takes no receiver, and the report names it `Class::method`.
#[test]
fn reference_args_13_a_class_static() {
    assert_reference_args(
        "class Tools {\n\
        \x20   pub static fn bump(v: &mut i64) { v = v + 3; }\n\
         }\n\
         fn main() {\n\
        \x20   let mut n = 10;\n\
        \x20   Tools::bump(&n);\n\
        \x20   println(n);\n\
         }\n",
        "13\n",
        &["main"],
    );
}

// 14. Interface dispatch: the callee is unknown until run time, so the pointer
//     goes through an indirect signature that has to declare the slot as a
//     pointer rather than as the parameter's own type.
#[test]
fn reference_args_14_interface_dispatch() {
    assert_reference_args(
        "interface Scale {\n\
        \x20   fn nudge(self, v: &mut i64);\n\
         }\n\
         class Step implements Scale {\n\
        \x20   pub by: i64;\n\
        \x20   pub fn nudge(self, v: &mut i64) { v = v + self.by; }\n\
         }\n\
         class Jump implements Scale {\n\
        \x20   pub by: i64;\n\
        \x20   pub fn nudge(self, v: &mut i64) { v = v * self.by; }\n\
         }\n\
         fn main() {\n\
        \x20   let step: Scale = new Step(5);\n\
        \x20   let jump: Scale = new Jump(3);\n\
        \x20   let mut n = 10;\n\
        \x20   step.nudge(&n);\n\
        \x20   println(n);\n\
        \x20   jump.nudge(&n);\n\
        \x20   println(n);\n\
         }\n",
        "15\n45\n",
        &["main"],
    );
}

// 15. Virtual dispatch through a base-class handle: the report names the
//     STATIC class, because that is what the call site says.
#[test]
fn reference_args_15_virtual_dispatch() {
    assert_reference_args(
        "open class Base {\n\
        \x20   pub by: i64;\n\
        \x20   pub open fn nudge(self, v: &mut i64) { v = v + self.by; }\n\
         }\n\
         class Child extends Base {\n\
        \x20   pub override fn nudge(self, v: &mut i64) { v = v * self.by; }\n\
         }\n\
         fn main() {\n\
        \x20   let b = new Base(5);\n\
        \x20   let c: Base = new Child(4);\n\
        \x20   let mut n = 10;\n\
        \x20   b.nudge(&n);\n\
        \x20   println(n);\n\
        \x20   c.nudge(&n);\n\
        \x20   println(n);\n\
         }\n",
        "15\n60\n",
        &["main"],
    );
}

// 16. The loop case. Promoting the local at the `&` put the initialising store
//     inside the loop body, so every iteration wrote the ORIGINAL value back
//     over the callee's result and this printed 0.
#[test]
fn reference_args_16_a_reference_call_in_a_loop() {
    assert_reference_args(
        "fn bump(v: &mut i64) { v = v + 2; }\n\
         fn main() {\n\
        \x20   let mut n = 0;\n\
        \x20   let mut i = 0;\n\
        \x20   while i < 4 {\n\
        \x20       bump(&n);\n\
        \x20       i = i + 1;\n\
        \x20   }\n\
        \x20   println(n);\n\
         }\n",
        "8\n",
        &["main", "bump"],
    );
}

// 17. The branch case, and the sharper one: with the promotion sitting in the
//     arm, the arm that ran never initialised the slot, so this printed
//     uninitialised stack bytes — and a DIFFERENT number under each emitter.
#[test]
fn reference_args_17_a_reference_call_in_one_arm() {
    assert_reference_args(
        "fn bump(v: &mut i64) { v = v + 2; }\n\
         fn main() {\n\
        \x20   let mut n = 1;\n\
        \x20   if n > 0 {\n\
        \x20       bump(&n);\n\
        \x20   } else {\n\
        \x20       bump(&n);\n\
        \x20       bump(&n);\n\
        \x20   }\n\
        \x20   println(n);\n\
         }\n",
        "3\n",
        &["main", "bump"],
    );
}

// 18. The pointer handed over is the local's GC root slot, so a collection
//     while the callee holds it must still find the live string.
#[test]
fn reference_args_18_a_string_reference_under_collection() {
    assert_reference_args(
        "fn relabel(text: &mut String) {\n\
        \x20   gc_collect();\n\
        \x20   text = text + \" kept\";\n\
        \x20   gc_collect();\n\
         }\n\
         fn main() {\n\
        \x20   let mut s = \"value\";\n\
        \x20   relabel(&s);\n\
        \x20   gc_collect();\n\
        \x20   println(s);\n\
         }\n",
        "value kept\n",
        &["main", "relabel"],
    );
}

// 19. A parameter is a place too. It cannot be declared mutable, so `&` is the
//     only mode it reaches — but its storage decision is the same one, and it
//     is made when the parameter is bound rather than at the `&`.
#[test]
fn reference_args_19_an_immutable_reference_to_a_parameter() {
    assert_reference_args(
        "fn twice(v: & i64) -> i64 { return v + v; }\n\
         fn run(n: i64) -> i64 {\n\
        \x20   let mut total = 0;\n\
        \x20   let mut i = 0;\n\
        \x20   while i < 3 {\n\
        \x20       total = total + twice(&n);\n\
        \x20       i = i + 1;\n\
        \x20   }\n\
        \x20   return total;\n\
         }\n\
         fn main() { println(run(10)); }\n",
        "60\n",
        &["main", "run", "twice"],
    );
}

// 20. The debug record itself: a panic raised inside the callee names the
//     parameter, its mode and type, the ampersand's position, and the place.
#[test]
fn reference_args_20_a_panic_under_the_call_reports_the_reference() {
    assert_same_panic(
        "fn bump(v: &mut i64) {\n\
        \x20   v = v + 1;\n\
        \x20   panic(\"inside\");\n\
         }\n\
         fn main() {\n\
        \x20   let mut n = 41;\n\
        \x20   bump(&n);\n\
        \x20   println(n);\n\
         }\n",
        &[
            "runtime panic: inside",
            "reference call: bump parameter `v` &mut i64 at <src>.wi:7:10 using local `n`",
        ],
    );
}

// 21. The other half of the record: the context is cleared when the call
//     returns, so a later panic must not blame a reference that is long done.
#[test]
fn reference_args_21_a_panic_after_the_call_reports_no_reference() {
    let source = "fn bump(v: &mut i64) { v = v + 1; }\n\
                  fn main() {\n\
                 \x20   let mut n = 41;\n\
                 \x20   bump(&n);\n\
                 \x20   panic(\"after\");\n\
                  }\n";
    assert_same_panic(source, &["runtime panic: after"]);
    let (out, _) = compile_and_run_with_env(source, &LIR);
    assert!(
        !out.contains("reference call:"),
        "the reference context outlived the call it described:\n{out}"
    );
}

// 22. A float place: the slot is written and read as an f64, which is the one
//     case where getting the stack slot's type wrong is silently wrong rather
//     than a verifier error.
#[test]
fn reference_args_22_an_f64_reference_in_a_loop() {
    assert_reference_args(
        "fn halve(v: &mut f64) { v = v / 2.0; }\n\
         fn main() {\n\
        \x20   let mut x = 9.0;\n\
        \x20   let mut i = 0;\n\
        \x20   while i < 2 {\n\
        \x20       halve(&x);\n\
        \x20       i = i + 1;\n\
        \x20   }\n\
        \x20   println(x);\n\
         }\n",
        "2.25\n",
        &["main", "halve"],
    );
}

// 23. A lambda body compiles as its own function, so the address-taken set it
//     is given must be the lambda's own and not the enclosing function's.
#[test]
fn reference_args_23_a_reference_call_inside_a_lambda() {
    assert_reference_args(
        "fn bump(v: &mut i64) { v = v + 4; }\n\
         fn main() {\n\
        \x20   let run = || -> i64 {\n\
        \x20       let mut n = 1;\n\
        \x20       bump(&n);\n\
        \x20       return n;\n\
        \x20   };\n\
        \x20   println(run());\n\
         }\n",
        "5\n",
        &["bump"],
    );
}

// 24. A nested field walks two objects to reach the place, and does it once per
//     iteration.
#[test]
fn reference_args_24_a_nested_field_place_in_a_loop() {
    assert_reference_args(
        "class Inner { pub n: i64; }\n\
         class Outer { pub inner: Inner; }\n\
         fn bump(v: &mut i64) { v = v + 1; }\n\
         fn main() {\n\
        \x20   let o = new Outer(new Inner(7));\n\
        \x20   let mut i = 0;\n\
        \x20   while i < 3 {\n\
        \x20       bump(&o.inner.n);\n\
        \x20       i = i + 1;\n\
        \x20   }\n\
        \x20   println(o.inner.n);\n\
         }\n",
        "10\n",
        &["main", "bump"],
    );
}

// 25. Only one arm of a ternary is evaluated, so the address is taken on a path
//     the other arm does not reach.
#[test]
fn reference_args_25_a_reference_call_in_a_ternary() {
    assert_reference_args(
        "fn bump(v: &mut i64) -> i64 { v = v + 1; return v; }\n\
         fn main() {\n\
        \x20   let mut n = 5;\n\
        \x20   let out = n > 0 ? bump(&n) : 0;\n\
        \x20   println(out);\n\
        \x20   println(n);\n\
         }\n",
        "6\n6\n",
        &["main", "bump"],
    );
}

// 26. A reference parameter passed straight on to the next recursion: the
//     callee's own parameter is already a pointer, so `&v` must forward it
//     rather than take the address of the pointer.
#[test]
fn reference_args_26_a_recursive_reference_parameter() {
    assert_reference_args(
        "fn countdown(v: &mut i64, steps: i64) {\n\
        \x20   if steps == 0 { return; }\n\
        \x20   v = v + steps;\n\
        \x20   countdown(&v, steps - 1);\n\
         }\n\
         fn main() {\n\
        \x20   let mut n = 0;\n\
        \x20   countdown(&n, 4);\n\
        \x20   println(n);\n\
         }\n",
        "10\n",
        &["main", "countdown"],
    );
}

// 27. A binding declared inside the loop body is genuinely re-initialised each
//     iteration, and must keep being so once it is stack-backed.
#[test]
fn reference_args_27_a_local_declared_in_the_loop_body() {
    assert_reference_args(
        "fn bump(v: &mut i64) { v = v + 1; }\n\
         fn main() {\n\
        \x20   let mut total = 0;\n\
        \x20   let mut i = 0;\n\
        \x20   while i < 3 {\n\
        \x20       let mut local = 10;\n\
        \x20       bump(&local);\n\
        \x20       total = total + local;\n\
        \x20       i = i + 1;\n\
        \x20   }\n\
        \x20   println(total);\n\
         }\n",
        "33\n",
        &["main", "bump"],
    );
}

// 28. Two locals, each addressed from one arm only: the arm that does not run
//     must leave its local exactly as declared.
#[test]
fn reference_args_28_one_local_per_branch() {
    assert_reference_args(
        "fn bump(v: &mut i64) { v = v + 1; }\n\
         fn main() {\n\
        \x20   let mut a = 1;\n\
        \x20   let mut b = 2;\n\
        \x20   let flag = true;\n\
        \x20   if flag { bump(&a); } else { bump(&b); }\n\
        \x20   println(a);\n\
        \x20   println(b);\n\
         }\n",
        "2\n2\n",
        &["main", "bump"],
    );
}

// 29. The shipped example, which is the same story at program scale, has to
//     compile entirely through the walker.
#[test]
fn reference_args_29_the_example_is_fully_lir() {
    assert_reference_args(
        include_str!("../../example/reference_args_control_flow.wi"),
        "10\n15\n12\n14\n3\n9\n",
        &["main", "add", "halve", "restock"],
    );
}

// 30. Interface dispatch writes the record through an indirect call, where the
//     callee is unknown until run time: the parameter's name and type are
//     `<unknown>` on both paths, and a panic inside the callee still names the
//     ampersand's position and the caller's place.
#[test]
fn reference_args_30_a_panic_under_interface_dispatch_reports_the_reference() {
    assert_same_panic(
        "interface Scale {\n\
        \x20   fn nudge(self, v: &mut i64);\n\
         }\n\
         class Step implements Scale {\n\
        \x20   pub by: i64;\n\
        \x20   pub fn nudge(self, v: &mut i64) {\n\
        \x20       v = v + self.by;\n\
        \x20       panic(\"inside\");\n\
        \x20   }\n\
         }\n\
         fn main() {\n\
        \x20   let step: Scale = new Step(5);\n\
        \x20   let mut n = 10;\n\
        \x20   step.nudge(&n);\n\
        \x20   println(n);\n\
         }\n",
        &[
            "runtime panic: inside",
            "reference call: nudge parameter `<unknown>` &mut <unknown> at <src>.wi:14:16 using local `n`",
        ],
    );
}

// 31. The half of perspective 30 that neither emitter used to do
//     (willow-0g8j.11): interface dispatch recorded the reference call and
//     never cleared it, so a later, unrelated panic in the SAME function was
//     reported as if it had happened inside the callee, under a place the
//     callee had already given back. Both emitters clear it now.
#[test]
fn reference_args_31_a_panic_after_interface_dispatch_reports_no_reference() {
    let source = "interface Scale {\n\
                 \x20   fn nudge(self, v: &mut i64);\n\
                  }\n\
                  class Step implements Scale {\n\
                 \x20   pub by: i64;\n\
                 \x20   pub fn nudge(self, v: &mut i64) { v = v + self.by; }\n\
                  }\n\
                  fn main() {\n\
                 \x20   let step: Scale = new Step(5);\n\
                 \x20   let mut n = 10;\n\
                 \x20   step.nudge(&n);\n\
                 \x20   println(n);\n\
                 \x20   panic(\"after\");\n\
                  }\n";
    assert_same_panic(source, &["15", "runtime panic: after"]);
    for env in [&AST[..], &LIR[..]] {
        let (out, _) = compile_and_run_with_env(source, env);
        assert!(
            !out.contains("reference call:"),
            "the reference context outlived the interface call it described:\n{out}"
        );
    }
}

// 32. The sibling indirect path: a virtual call through a base-class handle
//     goes out through a vtable slot rather than an interface table, and it
//     must clear the record on return for the same reason.
#[test]
fn reference_args_32_a_panic_after_virtual_dispatch_reports_no_reference() {
    let source = "open class Base {\n\
                 \x20   pub by: i64;\n\
                 \x20   pub open fn nudge(self, v: &mut i64) { v = v + self.by; }\n\
                  }\n\
                  class Child extends Base {\n\
                 \x20   pub override fn nudge(self, v: &mut i64) { v = v * self.by; }\n\
                  }\n\
                  fn main() {\n\
                 \x20   let c: Base = new Child(3);\n\
                 \x20   let mut n = 10;\n\
                 \x20   c.nudge(&n);\n\
                 \x20   println(n);\n\
                 \x20   panic(\"after\");\n\
                  }\n";
    assert_same_panic(source, &["30", "runtime panic: after"]);
    for env in [&AST[..], &LIR[..]] {
        let (out, _) = compile_and_run_with_env(source, env);
        assert!(
            !out.contains("reference call:"),
            "the reference context outlived the virtual call it described:\n{out}"
        );
    }
}

// 33. A payload binding is declared at arm entry. The loop makes this match use
//     structural LIR lowering; the two branches then prove its slot was
//     initialised before either reference site rather than inside the first one.
#[test]
fn reference_args_33_an_enum_binding_reference_in_a_loop() {
    assert_reference_args(
        "enum Number { One(i64), Empty }\n\
         fn read(value: & i64) -> i64 { return value; }\n\
         fn update(number: Number, first: bool) -> i64 {\n\
        \x20   match number {\n\
        \x20       Number::One(n) => {\n\
        \x20           let mut i = 0;\n\
        \x20           while i < 1 { i = i + 1; }\n\
        \x20           let mut out = 0;\n\
        \x20           if first { out = read(&n); } else { out = read(&n) + read(&n); }\n\
        \x20           return out;\n\
        \x20       }\n\
        \x20       Number::Empty => { return -1; }\n\
        \x20   }\n\
         }\n\
         fn main() { println(update(Number::One(7), true)); println(update(Number::One(7), false)); }\n",
        "7\n14\n",
        &["update", "read", "main"],
    );
}

// 34. A whole-value binding carries the scrutinee's real type. The two branch
//     emitters must see one slot created at that binding, rather than whichever
//     branch containing `&value` happened to be emitted first.
#[test]
fn reference_args_34_a_whole_binding_reference_in_a_branch() {
    assert_reference_args(
        "fn read(value: & bool) -> bool { return value; }\n\
         fn update(start: bool, once: bool) -> bool {\n\
        \x20   match start {\n\
        \x20       value => {\n\
        \x20           if once { return read(&value); } else { return read(&value) && read(&value); }\n\
        \x20       }\n\
        \x20   }\n\
         }\n\
         fn main() { println(update(true, true)); println(update(true, false)); }\n",
        "true\ntrue\n",
        &["update", "read", "main"],
    );
}
