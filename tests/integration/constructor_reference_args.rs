//! Reference arguments to `new` and `super.init(...)` (willow-0g8j.10,
//! willow-dssc).
//!
//! A constructor parameter can be `&`/`&mut` like any other parameter, and
//! `init` has no other spelling: `new C(&place)` and `super.init(&place)` are
//! the only ways to reach it. Both had lost the parameter MODES on the way to
//! codegen, so the address the caller meant to pass was emitted as an ordinary
//! value that the constructor then dereferenced as a pointer -- a segfault on
//! the AST path, and on the LIR path a debug reference-call record that named
//! `<unknown>` and outlived the call, attributing a later, unrelated panic to a
//! constructor that had already returned.
//!
//! The type checker had the matching hole: `check_new` built its parameter list
//! from `ConstructorInfo::params` (`Vec<Type>`), which drops `ParamMode`
//! entirely, while `check_super_init` right beside it used `param_infos` and
//! checked modes properly. So `&x` on a by-value parameter compiled as if the
//! `&` were not written, a missing `&` on a `&mut` parameter was accepted, an
//! immutable local could be mutated through `&mut`, and `&x` on an implicit
//! memberwise constructor reached the LIR emitter as a node it has no case for
//! and panicked the compiler.
//!
//! Every runtime perspective is differential: the same source under the AST
//! emitter and under the walker must print the same thing, with
//! `WILLOW_LIR_REQUIRE=1` so a coverage regression cannot pass by comparing the
//! AST path against itself. Note that a class method body is not in the
//! walker's subset yet (willow-0g8j.2.18), so `init` itself is always AST code;
//! what the walker compiles here is the *caller*.
//!
//! 34 perspectives:
//!   1 `&mut i64` writes back            18 a memberwise ctor takes values
//!   2 `&` reads without writing         19 a String reference in a loop
//!   3 `&mut bool`                       20 a `new` inside another `init`
//!   4 `&mut f64`                        21 a panic inside `init`
//!   5 `&mut String` (GC-managed)        22 a panic after `new` returned
//!   6 two reference parameters          23 a panic inside the base `init`
//!   7 reference and value mixed         24 a panic after `super.init`
//!   8 a field place                     25 `&` on a memberwise ctor
//!   9 an array element place            26 `&` on a by-value parameter
//!  10 a `new` in a loop                 27 a missing `&`
//!  11 a `new` in a lambda               28 an immutable local as `&mut`
//!  12 a `new` in a match arm            29 two `&mut` on one place
//!  13 a `new` on each `if` branch       30 a reference of the wrong type
//!  14 `super.init` forwards it          31 `&` on a by-value base parameter
//!  15 `super.init` mixed with a value   32 the example runs the same
//!  16 two writes through one pointer    33 the example is fully LIR
//!  17 the pointer passed on again       34 nested calls restore the outer debug context

use super::support::{
    assert_compile_error_contains, compile_and_run_with_env, compile_with_compiler_env,
};

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
/// each be named in the walker's selection log -- otherwise the second copy of
/// the right answer came from the AST emitter too.
fn assert_ctor_refs(source: &str, expected: &str, functions: &[&str]) {
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
/// `<src>.wi:` -- the whole token, directories included.
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

/// A program that panics: both emitters must produce the same whole report, it
/// must contain `expected_parts`, and it must contain none of `absent_parts`.
fn assert_same_panic(source: &str, expected_parts: &[&str], absent_parts: &[&str]) {
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
    for part in absent_parts {
        assert!(
            !lir_report.contains(part),
            "report should not mention `{part}`:\n{lir_report}"
        );
    }
}

// 1. The base case, and the shape the bead was filed on: the constructor writes
//    through the pointer and the caller's own local is what changed. Before the
//    fix the AST emitter passed the value 41 as an address and segfaulted.
#[test]
fn ctor_refs_01_a_mutable_i64_writes_back() {
    assert_ctor_refs(
        "class Counter {\n\
        \x20   pub start: i64;\n\
        \x20   pub init(self, seed: &mut i64) { seed = seed + 1; self.start = seed; }\n\
         }\n\
         fn main() { let mut n = 41; let c = new Counter(&n); println(c.start); println(n); }\n",
        "42\n42\n",
        &["main"],
    );
}

// 2. An immutable `&` parameter is still passed by address; it just cannot be
//    assigned. The caller's local must survive the call unchanged.
#[test]
fn ctor_refs_02_an_immutable_reference_reads_without_writing() {
    assert_ctor_refs(
        "class Mirror {\n\
        \x20   pub seen: i64;\n\
        \x20   pub init(self, source: &i64) { self.seen = source * 2; }\n\
         }\n\
         fn main() { let n = 21; let m = new Mirror(&n); println(m.seen); println(n); }\n",
        "42\n21\n",
        &["main"],
    );
}

// 3. A one-byte parameter: the pointee width has to follow the declared type,
//    not the pointer.
#[test]
fn ctor_refs_03_a_mutable_bool() {
    assert_ctor_refs(
        "class Flag {\n\
        \x20   pub raised: bool;\n\
        \x20   pub init(self, ready: &mut bool) { ready = !ready; self.raised = ready; }\n\
         }\n\
         fn main() { let mut ok = false; let f = new Flag(&ok); println(f.raised); println(ok); }\n",
        "true\ntrue\n",
        &["main"],
    );
}

// 4. A float parameter, so the load and store go through the FP registers.
#[test]
fn ctor_refs_04_a_mutable_f64() {
    assert_ctor_refs(
        "class Scale {\n\
        \x20   pub factor: f64;\n\
        \x20   pub init(self, v: &mut f64) { v = v * 2.0; self.factor = v; }\n\
         }\n\
         fn main() { let mut x = 1.5; let s = new Scale(&x); println(s.factor); println(x); }\n",
        "3\n3\n",
        &["main"],
    );
}

// 5. A GC-managed place: the constructor rebinds the caller's `String` local, so
//    the new handle has to be stored back and rooted on both sides.
#[test]
fn ctor_refs_05_a_mutable_string() {
    assert_ctor_refs(
        "class Banner {\n\
        \x20   pub text: String;\n\
        \x20   pub init(self, caption: &mut String) { caption = caption + \"!\"; self.text = caption; }\n\
         }\n\
         fn main() { let mut c = \"hi\"; let b = new Banner(&c); println(b.text); println(c); }\n",
        "hi!\nhi!\n",
        &["main"],
    );
}

// 6. Two reference parameters at once: each one must reach its own place, in
//    argument order.
#[test]
fn ctor_refs_06_two_reference_parameters() {
    assert_ctor_refs(
        "class Two {\n\
        \x20   pub a: i64;\n\
        \x20   pub b: i64;\n\
        \x20   pub init(self, a: &mut i64, b: &mut i64) { a = a + 1; b = b + 10; self.a = a; self.b = b; }\n\
         }\n\
         fn main() {\n\
        \x20   let mut x = 1;\n\
        \x20   let mut y = 2;\n\
        \x20   let t = new Two(&x, &y);\n\
        \x20   println(t.a + t.b);\n\
        \x20   println(x + y);\n\
         }\n",
        "14\n14\n",
        &["main"],
    );
}

// 7. Mixed modes in one signature: dropping the modes used to shift every later
//    argument's interpretation, so the value parameter is checked too.
#[test]
fn ctor_refs_07_reference_and_value_mixed() {
    assert_ctor_refs(
        "class Tally {\n\
        \x20   pub total: i64;\n\
        \x20   pub step: i64;\n\
        \x20   pub init(self, running: &mut i64, step: i64) { running = running + step; self.total = running; self.step = step; }\n\
         }\n\
         fn main() {\n\
        \x20   let mut running = 30;\n\
        \x20   let t = new Tally(&running, 12);\n\
        \x20   println(t.total);\n\
        \x20   println(t.step);\n\
        \x20   println(running);\n\
         }\n",
        "42\n12\n42\n",
        &["main"],
    );
}

// 8. The place is a field of another object rather than a local, so the address
//    is an interior pointer into the heap.
#[test]
fn ctor_refs_08_a_field_place() {
    assert_ctor_refs(
        "class Box { pub v: i64; }\n\
         class Counter {\n\
        \x20   pub start: i64;\n\
        \x20   pub init(self, seed: &mut i64) { seed = seed + 1; self.start = seed; }\n\
         }\n\
         fn main() { let b = new Box(10); let c = new Counter(&b.v); println(c.start); println(b.v); }\n",
        "11\n11\n",
        &["main"],
    );
}

// 9. The place is an array element, whose address is computed from a base and an
//    index rather than named by a slot.
#[test]
fn ctor_refs_09_an_array_element_place() {
    assert_ctor_refs(
        "import std::collections::Array;\n\
         class Counter {\n\
        \x20   pub start: i64;\n\
        \x20   pub init(self, seed: &mut i64) { seed = seed + 1; self.start = seed; }\n\
         }\n\
         fn main() {\n\
        \x20   let xs: Array<i64> = [1, 2, 3];\n\
        \x20   let c = new Counter(&xs[1]);\n\
        \x20   println(c.start);\n\
        \x20   println(xs[1]);\n\
         }\n",
        "3\n3\n",
        &["main"],
    );
}

// 10. The same local is handed to a constructor on every iteration: the address
//     has to stay stable across the loop instead of being re-derived from a
//     fresh copy, or the accumulated writes are lost.
#[test]
fn ctor_refs_10_a_constructor_call_in_a_loop() {
    assert_ctor_refs(
        "class Tally {\n\
        \x20   pub total: i64;\n\
        \x20   pub init(self, running: &mut i64) { running = running + 5; self.total = running; }\n\
         }\n\
         fn accumulate(rounds: i64) -> i64 {\n\
        \x20   let mut running = 0;\n\
        \x20   let mut i = 0;\n\
        \x20   while i < rounds { let t = new Tally(&running); i = i + 1; }\n\
        \x20   return running;\n\
         }\n\
         fn main() { println(accumulate(4)); }\n",
        "20\n",
        &["accumulate", "main"],
    );
}

// 11. The call sits in a lambda body, which is compiled as its own function with
//     its own frame for the address-taken local.
#[test]
fn ctor_refs_11_a_constructor_call_in_a_lambda() {
    assert_ctor_refs(
        "class Counter {\n\
        \x20   pub start: i64;\n\
        \x20   pub init(self, seed: &mut i64) { seed = seed + 1; self.start = seed; }\n\
         }\n\
         fn main() {\n\
        \x20   let bump = |x: i64| -> i64 { let mut v = x; let c = new Counter(&v); return c.start + v; };\n\
        \x20   println(bump(20));\n\
         }\n",
        "42\n",
        &["main"],
    );
}

// 12. The call is inside a `match` arm, so it is emitted in a block the walker
//     reached through the match's control-flow graph.
#[test]
fn ctor_refs_12_a_constructor_call_in_a_match_arm() {
    assert_ctor_refs(
        "enum Choice { First, Second }\n\
         class Counter {\n\
        \x20   pub start: i64;\n\
        \x20   pub init(self, seed: &mut i64) { seed = seed + 1; self.start = seed; }\n\
         }\n\
         fn from_choice(c: Choice) -> i64 {\n\
        \x20   let mut a = 40;\n\
        \x20   match c {\n\
        \x20       Choice::First => { let h = new Counter(&a); return h.start + a; }\n\
        \x20       Choice::Second => { return 0; }\n\
        \x20   }\n\
         }\n\
         fn main() { println(from_choice(Choice::First)); println(from_choice(Choice::Second)); }\n",
        "82\n0\n",
        &["from_choice", "main"],
    );
}

// 13. One address-taken local per branch: a local promoted to a stack slot only
//     where the `&` appears would be uninitialised on the branch that never
//     takes an address.
#[test]
fn ctor_refs_13_a_constructor_call_on_each_branch() {
    assert_ctor_refs(
        "class Counter {\n\
        \x20   pub start: i64;\n\
        \x20   pub init(self, seed: &mut i64) { seed = seed + 1; self.start = seed; }\n\
         }\n\
         fn pick(flag: bool) -> i64 {\n\
        \x20   let mut a = 1;\n\
        \x20   let mut b = 10;\n\
        \x20   if flag { let c = new Counter(&a); return c.start + a; }\n\
        \x20   let c = new Counter(&b);\n\
        \x20   return c.start + b;\n\
         }\n\
         fn main() { println(pick(true)); println(pick(false)); }\n",
        "4\n22\n",
        &["pick", "main"],
    );
}

// 14. `super.init(&balance)` forwards a parameter that is ALREADY a pointer, so
//     the base constructor has to receive the caller's original place and not
//     the address of the derived constructor's own copy.
#[test]
fn ctor_refs_14_super_init_forwards_a_reference() {
    assert_ctor_refs(
        "open class Ledger {\n\
        \x20   pub opening: i64;\n\
        \x20   pub init(self, balance: &mut i64) { balance = balance * 2; self.opening = balance; }\n\
         }\n\
         class Journal extends Ledger {\n\
        \x20   pub entries: i64;\n\
        \x20   pub init(self, balance: &mut i64, entries: i64) { super.init(&balance); self.entries = entries; }\n\
         }\n\
         fn main() {\n\
        \x20   let mut balance = 7;\n\
        \x20   let j = new Journal(&balance, 3);\n\
        \x20   println(j.opening);\n\
        \x20   println(j.entries);\n\
        \x20   println(balance);\n\
         }\n",
        "14\n3\n14\n",
        &["main"],
    );
}

// 15. `super.init` with a reference and a value argument together.
#[test]
fn ctor_refs_15_super_init_mixed_with_a_value() {
    assert_ctor_refs(
        "open class Ledger {\n\
        \x20   pub opening: i64;\n\
        \x20   pub note: i64;\n\
        \x20   pub init(self, balance: &mut i64, note: i64) { balance = balance + note; self.opening = balance; self.note = note; }\n\
         }\n\
         class Journal extends Ledger {\n\
        \x20   pub entries: i64;\n\
        \x20   pub init(self, balance: &mut i64) { super.init(&balance, 8); self.entries = 1; }\n\
         }\n\
         fn main() {\n\
        \x20   let mut balance = 34;\n\
        \x20   let j = new Journal(&balance);\n\
        \x20   println(j.opening + j.note + j.entries);\n\
        \x20   println(balance);\n\
         }\n",
        "51\n42\n",
        &["main"],
    );
}

// 16. More than one write through the same pointer, so a constructor that
//     re-read its parameter sees its own earlier store.
#[test]
fn ctor_refs_16_two_writes_through_one_pointer() {
    assert_ctor_refs(
        "class Counter {\n\
        \x20   pub start: i64;\n\
        \x20   pub init(self, seed: &mut i64) { seed = seed + 1; seed = seed * 2; self.start = seed; }\n\
         }\n\
         fn main() { let mut n = 20; let c = new Counter(&n); println(c.start); println(n); }\n",
        "42\n42\n",
        &["main"],
    );
}

// 17. The constructor hands its own reference parameter on to a free function,
//     so the pointer crosses two frames before the write lands.
#[test]
fn ctor_refs_17_the_pointer_is_passed_on_again() {
    assert_ctor_refs(
        "fn bump(v: &mut i64) { v = v + 1; }\n\
         class Counter {\n\
        \x20   pub start: i64;\n\
        \x20   pub init(self, seed: &mut i64) { bump(&seed); self.start = seed; }\n\
         }\n\
         fn main() { let mut n = 41; let c = new Counter(&n); println(c.start); println(n); }\n",
        "42\n42\n",
        &["main"],
    );
}

// 18. The implicit memberwise constructor keeps taking its fields by value: the
//     mode plumbing must not turn a field into a reference parameter.
#[test]
fn ctor_refs_18_a_memberwise_constructor_takes_values() {
    assert_ctor_refs(
        "class Plain { pub v: i64; }\n\
         fn main() { let mut n = 3; let p = new Plain(n); n = 99; println(p.v); println(n); }\n",
        "3\n99\n",
        &["main"],
    );
}

// 19. A GC-managed reference argument in a loop, so the collector runs between
//     the stores and the rooted slot has to hold the current handle.
#[test]
fn ctor_refs_19_a_string_reference_in_a_loop() {
    assert_ctor_refs(
        "class Banner {\n\
        \x20   pub text: String;\n\
        \x20   pub init(self, caption: &mut String) { caption = caption + \"x\"; self.text = caption; }\n\
         }\n\
         fn grow(rounds: i64) -> String {\n\
        \x20   let mut caption = \"\";\n\
        \x20   let mut i = 0;\n\
        \x20   while i < rounds { let b = new Banner(&caption); i = i + 1; }\n\
        \x20   return caption;\n\
         }\n\
         fn main() { println(grow(4)); }\n",
        "xxxx\n",
        &["grow", "main"],
    );
}

// 20. A constructor that itself runs a `new` with a reference argument, so the
//     debug reference-call record of the inner call cannot be left standing over
//     the outer one.
#[test]
fn ctor_refs_20_a_constructor_call_inside_another_constructor() {
    assert_ctor_refs(
        "class Inner {\n\
        \x20   pub v: i64;\n\
        \x20   pub init(self, seed: &mut i64) { seed = seed + 1; self.v = seed; }\n\
         }\n\
         class Outer {\n\
        \x20   pub total: i64;\n\
        \x20   pub init(self, seed: &mut i64) { let i = new Inner(&seed); self.total = i.v + seed; }\n\
         }\n\
         fn main() { let mut n = 20; let o = new Outer(&n); println(o.total); println(n); }\n",
        "42\n21\n",
        &["main"],
    );
}

// 21. A panic raised inside `init` names the place the CALLER passed, with the
//     real parameter name. Before the fix the LIR path printed `<unknown>` for
//     both the parameter and its type, because `new` never handed the parameter
//     debug table to the argument emitter.
#[test]
fn ctor_refs_21_a_panic_inside_init_names_the_place() {
    assert_same_panic(
        "class Counter {\n\
        \x20   pub start: i64;\n\
        \x20   pub init(self, seed: &mut i64) { seed = seed + 1; self.start = seed; panic(\"inside init\"); }\n\
         }\n\
         fn main() { let mut s = 41; let c = new Counter(&s); println(c.start); }\n",
        &[
            "runtime panic: inside init",
            "reference call:",
            "parameter `seed` &mut i64",
            "using local `s`",
        ],
        &["<unknown>"],
    );
}

// 22. The record is cleared when the constructor returns: a later, unrelated
//     panic in the same function must not be attributed to it (willow-0g8j.11).
#[test]
fn ctor_refs_22_a_panic_after_new_returned_is_not_a_reference_call() {
    assert_same_panic(
        "class Counter {\n\
        \x20   pub start: i64;\n\
        \x20   pub init(self, seed: &mut i64) { seed = seed + 1; self.start = seed; }\n\
         }\n\
         fn main() {\n\
        \x20   let mut s = 41;\n\
        \x20   let c = new Counter(&s);\n\
        \x20   println(c.start);\n\
        \x20   panic(\"after the constructor\");\n\
         }\n",
        &["runtime panic: after the constructor"],
        &["reference call:", "<unknown>"],
    );
}

// 23. The same report through `super.init`: the base constructor panics and the
//     record names the derived constructor's own parameter as the place.
#[test]
fn ctor_refs_23_a_panic_inside_the_base_init() {
    assert_same_panic(
        "open class Ledger {\n\
        \x20   pub opening: i64;\n\
        \x20   pub init(self, balance: &mut i64) { balance = balance * 2; self.opening = balance; panic(\"inside base init\"); }\n\
         }\n\
         class Journal extends Ledger {\n\
        \x20   pub entries: i64;\n\
        \x20   pub init(self, balance: &mut i64) { super.init(&balance); self.entries = 1; }\n\
         }\n\
         fn main() { let mut t = 5; let j = new Journal(&t); println(j.opening); }\n",
        &[
            "runtime panic: inside base init",
            "reference call:",
            "parameter `balance` &mut i64",
            "using local `balance`",
        ],
        &["<unknown>"],
    );
}

// 24. And `super.init` clears its record too, so a panic after it returned is
//     reported on its own.
#[test]
fn ctor_refs_24_a_panic_after_super_init_returned() {
    assert_same_panic(
        "open class Ledger {\n\
        \x20   pub opening: i64;\n\
        \x20   pub init(self, balance: &mut i64) { balance = balance * 2; self.opening = balance; }\n\
         }\n\
         class Journal extends Ledger {\n\
        \x20   pub entries: i64;\n\
        \x20   pub init(self, balance: &mut i64) { super.init(&balance); self.entries = 1; }\n\
         }\n\
         fn main() {\n\
        \x20   let mut t = 5;\n\
        \x20   let j = new Journal(&t);\n\
        \x20   println(j.opening);\n\
        \x20   panic(\"after super init\");\n\
         }\n",
        &["runtime panic: after super init"],
        &["reference call:", "<unknown>"],
    );
}

// 25. The compiler-panic repro: a memberwise constructor has no reference
//     parameter to match, and the walker had no case for the node the accepted
//     `&` produced. It is a type error now, the same one a free function gives.
#[test]
fn ctor_refs_25_a_reference_on_a_memberwise_constructor_is_rejected() {
    assert_compile_error_contains(
        "class Plain { pub v: i64; }\n\
         fn main() { let mut s = 3; let p = new Plain(&s); println(p.v); }\n",
        &[
            "error[E1703]",
            "unexpected reference argument",
            "parameter expects `i64`, not `& i64`",
        ],
    );
}

// 26. The same rejection for an explicit `init` whose parameter is by value.
//     This used to compile with the `&` silently dropped.
#[test]
fn ctor_refs_26_a_reference_on_a_by_value_parameter_is_rejected() {
    assert_compile_error_contains(
        "class Counter {\n\
        \x20   pub start: i64;\n\
        \x20   pub init(self, seed: i64) { self.start = seed; }\n\
         }\n\
         fn main() { let mut s = 41; let c = new Counter(&s); println(c.start); }\n",
        &[
            "error[E1703]",
            "unexpected reference argument",
            "parameter expects `i64`, not `& i64`",
        ],
    );
}

// 27. The opposite hole: a reference parameter given a plain value. The `&` is
//     required at the call site, with the fix suggesting where to write it.
#[test]
fn ctor_refs_27_a_missing_ampersand_is_rejected() {
    assert_compile_error_contains(
        "class Counter {\n\
        \x20   pub start: i64;\n\
        \x20   pub init(self, seed: &mut i64) { seed = seed + 1; self.start = seed; }\n\
         }\n\
         fn main() { let mut s = 41; let c = new Counter(s); println(c.start); }\n",
        &[
            "error[E1702]",
            "expected reference argument for reference parameter",
            "expected `&` before this argument",
            "write `&s`",
        ],
    );
}

// 28. `&mut` needs a mutable place. Accepting this let a constructor write
//     through an immutable local.
#[test]
fn ctor_refs_28_an_immutable_local_cannot_be_passed_as_mut() {
    assert_compile_error_contains(
        "class Counter {\n\
        \x20   pub start: i64;\n\
        \x20   pub init(self, seed: &mut i64) { seed = seed + 1; self.start = seed; }\n\
         }\n\
         fn main() { let s = 41; let c = new Counter(&s); println(c.start); }\n",
        &[
            "error[E1701]",
            "cannot pass immutable variable `s` as `&mut`",
            "declared immutable here",
        ],
    );
}

// 29. Constructor arguments obey the aliasing rule too: two `&mut` on one place
//     would give `init` two live pointers to the same word.
#[test]
fn ctor_refs_29_two_mutable_references_to_one_place_are_rejected() {
    assert_compile_error_contains(
        "class Two {\n\
        \x20   pub a: i64;\n\
        \x20   pub b: i64;\n\
        \x20   pub init(self, a: &mut i64, b: &mut i64) { a = a + 1; b = b + 1; self.a = a; self.b = b; }\n\
         }\n\
         fn main() { let mut x = 1; let t = new Two(&x, &x); println(t.a); }\n",
        &[
            "error[E1706]",
            "cannot pass `x` while it aliases a mutable reference",
        ],
    );
}

// 30. A reference argument is checked against the pointee type, and reports the
//     reference-specific mismatch rather than the by-value one.
#[test]
fn ctor_refs_30_a_reference_of_the_wrong_type_is_rejected() {
    assert_compile_error_contains(
        "class Counter {\n\
        \x20   pub start: i64;\n\
        \x20   pub init(self, seed: &mut i64) { seed = seed + 1; self.start = seed; }\n\
         }\n\
         fn main() { let mut s = 4.5; let c = new Counter(&s); println(c.start); }\n",
        &[
            "error[E1705]",
            "reference argument type mismatch",
            "found `f64`",
        ],
    );
}

// 31. `super.init` already checked its modes; this pins that it still does, so
//     the two constructor call forms cannot drift apart again.
#[test]
fn ctor_refs_31_a_reference_on_a_by_value_base_parameter_is_rejected() {
    assert_compile_error_contains(
        "open class Ledger {\n\
        \x20   pub opening: i64;\n\
        \x20   pub init(self, balance: i64) { self.opening = balance; }\n\
         }\n\
         class Journal extends Ledger {\n\
        \x20   pub entries: i64;\n\
        \x20   pub init(self, balance: &mut i64) { super.init(&balance); self.entries = 1; }\n\
         }\n\
         fn main() { let mut t = 5; let j = new Journal(&t); println(j.opening); }\n",
        &["error[E1703]", "unexpected reference argument"],
    );
}

// 32. The shipped example prints the same thing under both emitters.
#[test]
fn ctor_refs_32_the_example_runs_the_same_under_both_emitters() {
    let source = std::fs::read_to_string("example/constructor_reference_args.wi")
        .expect("example/constructor_reference_args.wi");
    for env in [&AST[..], &LIR[..], &LIR_STRESS[..]] {
        let (out, ok) = compile_and_run_with_env(&source, env);
        assert!(ok, "example failed under {env:?}: {out}");
        assert_eq!(out, "42\n22\n6\n20\nhi!/hi!\n31\n6\n", "under {env:?}");
    }
}

// 33. And every function in it is the walker's work, not the AST emitter's.
#[test]
fn ctor_refs_33_the_example_is_fully_lir() {
    let source = std::fs::read_to_string("example/constructor_reference_args.wi")
        .expect("example/constructor_reference_args.wi");
    assert_walker_compiled(
        &source,
        &[
            "from_local",
            "from_field",
            "from_element",
            "accumulate",
            "shout",
            "open_journal",
            "in_lambda",
            "main",
        ],
    );
}

// 34. An inner reference call temporarily replaces the outer constructor's
//     metadata. Once it returns, a panic in the outer constructor must again
//     name the place `main` passed, not the inner call's parameter or nothing.
#[test]
fn ctor_refs_34_a_nested_reference_call_restores_the_outer_context() {
    assert_same_panic(
        "class Inner {\n\
        \x20   pub value: i64;\n\
        \x20   pub init(self, seed: &mut i64) { seed = seed + 1; self.value = seed; }\n\
         }\n\
         class Outer {\n\
        \x20   pub value: i64;\n\
        \x20   pub init(self, seed: &mut i64) {\n\
        \x20       let inner = new Inner(&seed);\n\
        \x20       self.value = inner.value;\n\
        \x20       panic(\"outer init\");\n\
        \x20   }\n\
         }\n\
         fn main() { let mut n = 1; let outer = new Outer(&n); println(outer.value); }\n",
        &[
            "runtime panic: outer init",
            "reference call:",
            "parameter `seed` &mut i64",
            "using local `n`",
        ],
        &["using local `seed`", "<unknown>"],
    );
}
