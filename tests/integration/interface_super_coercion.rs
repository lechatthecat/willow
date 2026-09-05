//! Interface → super-interface coercion when an interface has SEVERAL supers
//! (willow-1fc6).
//!
//! An interface value is a two-word box, `[object | vtable]`, and a call
//! through it indexes the vtable by a slot number computed from the receiver's
//! STATIC interface. Widening used to be a no-op: the same box was handed on
//! under the wider type. That is only sound while the target's slots sit at the
//! same indices, which stops being true the moment an interface has more than
//! one super — `interface C extends A, B` lays A's table out first, so a `C`
//! box reused as a `B` box dispatches `b()` through the slot holding `a()`.
//! Wrong method, and usually a wrong ABI on top of it.
//!
//! An interface's vtable now embeds each super's table verbatim, in order, so
//! every super occupies a contiguous run, and widening advances the vtable
//! pointer to that run. Offset zero — a single-`extends` chain, or the first of
//! several supers — still reuses the box untouched; anything else allocates a
//! rebox. The tests below drive the widening from every position that stores a
//! value, and from every shape of `extends` graph the offset arithmetic has to
//! survive.
//!
//! 24 perspectives:
//!   1 dispatch on the child itself      13 an instance-method argument
//!   2 the FIRST super is offset zero    14 the third super accumulates two
//!   3 a function argument                  preceding tables
//!   4 a return value                    15 a String-returning method through
//!   5 a local initialiser                  a preceding i64 super
//!   6 an assignment                     16 a String PARAMETER likewise
//!   7 an array literal element          17 a diamond's shared root
//!   8 `Array::push`                     18 the diamond's second branch
//!   9 an indexed array store            19 a transitive offset composes
//!  10 a constructor's field             20 a default method through a second
//!  11 a field assignment                   super
//!  12 a static-method argument          21 widening an already-widened value
//!                                       22 narrowing is still rejected
//!                                       23 the example runs
//!                                       24 the example under GC stress, and
//!                                          fully claimed by the LIR walker

use super::support::{
    compile_and_run_with_env, compile_with_compiler_env, compile_with_env_and_run_under,
};

/// No extra compiler environment: the ordinary build.
const PLAIN: [(&str, &str); 0] = [];
const STRESS: [(&str, &str); 1] = [("WILLOW_GC_STRESS", "alloc")];

const EXAMPLE: &str = include_str!("../../example/interface_super_coercion.wi");
const EXAMPLE_OUTPUT: &str =
    "11\n21\n41\n21\n22\n21\n22\n21\n22\n22\n21\n22\n21\n22\n31\n11\n12\n21\n22\n";

/// `All extends Alpha, Beta, Gamma`, so `Beta` starts at slot 2 of an `All`
/// vtable and `Gamma` at slot 4. Every method answers a distinct number, so a
/// call through the wrong slot cannot coincidentally print the right one.
const DECLS: &str = r#"
import std::collections::Array;

interface Alpha { fn alpha(self) -> i64; fn alpha2(self) -> i64; }
interface Beta { fn beta(self) -> i64; fn beta2(self) -> i64; }
interface Gamma { fn gamma(self) -> i64; }
interface All extends Alpha, Beta, Gamma { fn all(self) -> i64; }

class Impl implements All {
    pub fn alpha(self) -> i64 { return 11; }
    pub fn alpha2(self) -> i64 { return 12; }
    pub fn beta(self) -> i64 { return 21; }
    pub fn beta2(self) -> i64 { return 22; }
    pub fn gamma(self) -> i64 { return 31; }
    pub fn all(self) -> i64 { return 41; }
}
"#;

/// The shared declarations followed by `rest`.
fn program(rest: &str) -> String {
    format!("{DECLS}{rest}")
}

/// The program compiles and prints `expected`. Since willow-0g8j.3 a coercion
/// the walker refuses is a compile error, so a passing run is the proof it
/// emitted the coercion itself.
#[track_caller]
fn assert_both(source: &str, expected: &str) {
    let (out, ok) = compile_and_run_with_env(source, &PLAIN);
    assert!(ok, "program failed: {out}");
    assert_eq!(out, expected, "wrong output");
}

// 1. The baseline: dispatching on the child interface itself, where no
// widening happens at all. If this were wrong the rest would prove nothing.
#[test]
fn super_coercion_01_child_dispatch_uses_child_slots() {
    assert_both(
        &program(
            "fn main() { let v: All = new Impl(); \
             println(v.alpha()); println(v.beta()); println(v.gamma()); println(v.all()); }",
        ),
        "11\n21\n31\n41\n",
    );
}

// 2. The FIRST super's table is a plain prefix, so that widening reuses the
// box exactly as before. This is the case the old code got right.
#[test]
fn super_coercion_02_first_super_is_offset_zero() {
    assert_both(
        &program(
            "fn use_alpha(a: Alpha) -> i64 { return a.alpha2(); } \
             fn main() { let v: All = new Impl(); println(use_alpha(v)); }",
        ),
        "12\n",
    );
}

// 3. THE bug: a second super passed as an argument. `beta()` sits at slot 2 of
// an `All` vtable; before the fix the call went through slot 0 and ran
// `alpha()`.
#[test]
fn super_coercion_03_second_super_as_an_argument() {
    assert_both(
        &program(
            "fn use_beta(b: Beta) -> i64 { return b.beta(); } \
             fn main() { let v: All = new Impl(); println(use_beta(v)); }",
        ),
        "21\n",
    );
}

// 4. The same coercion on the way OUT of a function.
#[test]
fn super_coercion_04_second_super_as_a_return_value() {
    assert_both(
        &program(
            "fn as_beta(v: All) -> Beta { return v; } \
             fn main() { let v: All = new Impl(); println(as_beta(v).beta2()); }",
        ),
        "22\n",
    );
}

// 5. A `let` with a declared super type.
#[test]
fn super_coercion_05_second_super_as_a_local_initialiser() {
    assert_both(
        &program("fn main() { let v: All = new Impl(); let b: Beta = v; println(b.beta()); }"),
        "21\n",
    );
}

// 6. Assignment into an existing super-typed local, which is a separate
// coercion site from the initialiser.
#[test]
fn super_coercion_06_second_super_by_assignment() {
    assert_both(
        &program(
            "fn main() { let v: All = new Impl(); let mut b: Beta = v; b = v; \
             println(b.beta2()); }",
        ),
        "22\n",
    );
}

// 7. An array literal's elements are stored into the array's declared element
// type, so the literal widens too.
#[test]
fn super_coercion_07_array_literal_element() {
    assert_both(
        &program(
            "fn main() { let v: All = new Impl(); let xs: Array<Beta> = [v]; \
             println(xs[0].beta()); }",
        ),
        "21\n",
    );
}

// 8. `push` takes the element type, so it coerces like any other argument.
#[test]
fn super_coercion_08_array_push() {
    assert_both(
        &program(
            "fn main() { let v: All = new Impl(); let xs: Array<Beta> = []; xs.push(v); \
             println(xs[0].beta2()); }",
        ),
        "22\n",
    );
}

// 9. An indexed store is a third array path, distinct from the literal and
// from `push`.
#[test]
fn super_coercion_09_indexed_array_store() {
    assert_both(
        &program(
            "fn main() { let v: All = new Impl(); let xs: Array<Beta> = [v]; xs[0] = v; \
             println(xs[0].beta()); }",
        ),
        "21\n",
    );
}

// 10. A constructor argument lands in a declared field, which widens.
#[test]
fn super_coercion_10_constructor_field() {
    assert_both(
        &program(
            "class Holder { pub value: Beta; } \
             fn main() { let v: All = new Impl(); let h = new Holder(v); \
             println(h.value.beta()); }",
        ),
        "21\n",
    );
}

// 11. ... and so does a later store into that field.
#[test]
fn super_coercion_11_field_assignment() {
    assert_both(
        &program(
            "class Holder { pub value: Beta; } \
             fn main() { let v: All = new Impl(); let h = new Holder(v); h.value = v; \
             println(h.value.beta2()); }",
        ),
        "22\n",
    );
}

// 12. A static method's parameter.
#[test]
fn super_coercion_12_static_method_argument() {
    assert_both(
        &program(
            "class Sink { pub static fn take(value: Beta) -> i64 { return value.beta(); } } \
             fn main() { let v: All = new Impl(); println(Sink::take(v)); }",
        ),
        "21\n",
    );
}

// 13. An instance method's parameter, which is passed after the receiver.
#[test]
fn super_coercion_13_instance_method_argument() {
    assert_both(
        &program(
            "class Sink { pub fn take(self, value: Beta) -> i64 { return value.beta2(); } } \
             fn main() { let v: All = new Impl(); println(new Sink().take(v)); }",
        ),
        "22\n",
    );
}

// 14. The THIRD super starts after both preceding tables, so the offset is a
// sum and not just "not zero" — `Gamma` is at slot 4, past Alpha's two and
// Beta's two.
#[test]
fn super_coercion_14_third_super_accumulates_two_tables() {
    assert_both(
        &program(
            "fn use_gamma(g: Gamma) -> i64 { return g.gamma(); } \
             fn main() { let v: All = new Impl(); println(use_gamma(v)); \
             let g: Gamma = v; println(g.gamma()); }",
        ),
        "31\n31\n",
    );
}

// 15. A wrong slot is not only the wrong method, it is the wrong ABI. Here the
// target method returns a String while the method occupying the preceding
// super's slot returns an i64, so a mis-dispatch would hand a raw integer to
// the string runtime.
#[test]
fn super_coercion_15_string_return_through_an_i64_super() {
    assert_both(
        r#"
interface Number { fn value(self) -> i64; }
interface Text extends Number { fn text(self) -> String; }
interface Pair { fn pair(self) -> i64; }
interface Both extends Pair, Text { fn both(self) -> i64; }

class Impl implements Both {
    pub fn value(self) -> i64 { return 7; }
    pub fn text(self) -> String { return "seven"; }
    pub fn pair(self) -> i64 { return 2; }
    pub fn both(self) -> i64 { return 3; }
}

fn describe(t: Text) -> String { return t.text(); }

fn main() {
    let v: Both = new Impl();
    println(describe(v));
    let t: Text = v;
    println(t.value());
}
"#,
        "seven\n7\n",
    );
}

// 16. The mirror of 15 on the argument side: the target method takes a String
// where the preceding super's method takes an i64.
#[test]
fn super_coercion_16_string_parameter_through_an_i64_super() {
    assert_both(
        r#"
interface Adder { fn add(self, n: i64) -> i64; }
interface Joiner { fn join(self, s: String) -> String; }
interface Both extends Adder, Joiner {}

class Impl implements Both {
    pub fn add(self, n: i64) -> i64 { return n + 1; }
    pub fn join(self, s: String) -> String { return s + "!"; }
}

fn shout(j: Joiner) -> String { return j.join("hi"); }

fn main() {
    let v: Both = new Impl();
    println(shout(v));
}
"#,
        "hi!\n",
    );
}

// 17. A diamond: both middle interfaces extend one root, so the root's slots
// appear in BOTH middle regions rather than being deduplicated. That
// repetition is what keeps each region contiguous.
#[test]
fn super_coercion_17_diamond_shared_root() {
    assert_both(
        &diamond("fn main() { let v: Join = new Impl(); let r: Root = v; println(r.root()); }"),
        "1\n",
    );
}

// 18. Widening the diamond to its SECOND branch: both the branch's own method
// and the root method it inherited must answer from the embedded region.
#[test]
fn super_coercion_18_diamond_second_branch() {
    assert_both(
        &diamond(
            "fn use_right(r: Right) -> i64 { return r.root() * 10 + r.right(); } \
             fn main() { let v: Join = new Impl(); println(use_right(v)); \
             let r: Right = v; println(r.root()); }",
        ),
        "13\n1\n",
    );
}

/// A diamond — `Left` and `Right` both extend `Root`, `Join` extends both —
/// followed by `rest`.
fn diamond(rest: &str) -> String {
    format!(
        r#"
interface Root {{ fn root(self) -> i64; }}
interface Left extends Root {{ fn left(self) -> i64; }}
interface Right extends Root {{ fn right(self) -> i64; }}
interface Join extends Left, Right {{ fn join(self) -> i64; }}

class Impl implements Join {{
    pub fn root(self) -> i64 {{ return 1; }}
    pub fn left(self) -> i64 {{ return 2; }}
    pub fn right(self) -> i64 {{ return 3; }}
    pub fn join(self) -> i64 {{ return 4; }}
}}
{rest}
"#
    )
}

// 19. Two levels of `extends`: the offset of a grandparent is the offset of
// the parent plus the parent's own offset for it.
#[test]
fn super_coercion_19_transitive_offsets_compose() {
    assert_both(
        r#"
interface A { fn a(self) -> i64; }
interface B { fn b(self) -> i64; }
interface Mid extends A, B { fn mid(self) -> i64; }
interface Pad { fn pad(self) -> i64; }
interface Top extends Pad, Mid { fn top(self) -> i64; }

class Impl implements Top {
    pub fn a(self) -> i64 { return 1; }
    pub fn b(self) -> i64 { return 2; }
    pub fn mid(self) -> i64 { return 3; }
    pub fn pad(self) -> i64 { return 4; }
    pub fn top(self) -> i64 { return 5; }
}

fn use_b(v: B) -> i64 { return v.b(); }
fn use_mid(v: Mid) -> i64 { return v.mid(); }

fn main() {
    let v: Top = new Impl();
    println(use_b(v));
    println(use_mid(v));
    let m: Mid = v;
    println(use_b(m));
}
"#,
        "2\n3\n2\n",
    );
}

// 20. A DEFAULT interface method is injected into the implementing class and
// occupies a slot like any other, so it has to survive the same widening.
#[test]
fn super_coercion_20_default_method_through_a_second_super() {
    assert_both(
        r#"
interface First { fn first(self) -> i64; }
interface Counted {
    fn count(self) -> i64;
    fn twice(self) -> i64 { return self.count() * 2; }
}
interface Both extends First, Counted {}

class Impl implements Both {
    pub fn first(self) -> i64 { return 9; }
    pub fn count(self) -> i64 { return 5; }
}

fn use_counted(c: Counted) -> i64 { return c.twice(); }

fn main() {
    let v: Both = new Impl();
    println(use_counted(v));
    let c: Counted = v;
    println(c.twice());
    println(c.count());
}
"#,
        "10\n10\n5\n",
    );
}

// 21. Widening a value that was itself already widened: the second conversion
// starts from the embedded region, not from the original table.
#[test]
fn super_coercion_21_widening_an_already_widened_value() {
    assert_both(
        r#"
interface Root { fn root(self) -> i64; }
interface Pad { fn pad(self) -> i64; }
interface Mid extends Pad, Root { fn mid(self) -> i64; }
interface Top extends Pad, Mid { fn top(self) -> i64; }

class Impl implements Top {
    pub fn root(self) -> i64 { return 1; }
    pub fn pad(self) -> i64 { return 2; }
    pub fn mid(self) -> i64 { return 3; }
    pub fn top(self) -> i64 { return 4; }
}

fn main() {
    let v: Top = new Impl();
    let m: Mid = v;
    let r: Root = m;
    println(r.root());
    let same: Mid = m;
    println(same.mid());
}
"#,
        "1\n3\n",
    );
}

// 22. Widening runs one way only. A super has no slot for the child's own
// methods, and the checker still says so — the layout change must not have
// turned interface subtyping into a two-way street.
#[test]
fn super_coercion_22_narrowing_is_still_rejected() {
    let (ok, stderr) =
        compile_with_compiler_env(&program("fn narrow(a: Alpha) -> All { return a; }"), &PLAIN);
    assert!(!ok, "interface narrowing unexpectedly compiled");
    assert!(
        stderr.contains("error[E0201]"),
        "expected a type mismatch: {stderr}"
    );
}

// 23. The runnable example, which exercises every store position in one
// program.
#[test]
fn super_coercion_23_the_example_runs() {
    assert_both(EXAMPLE, EXAMPLE_OUTPUT);
}

// 24. The rebox ALLOCATES, so the object it copies over has to stay rooted
// across that allocation — under stress every allocation collects. The log
// check beside it names the functions the walker had to take.
#[test]
fn super_coercion_24_example_is_gc_safe_and_fully_lir() {
    let (out, ok) = compile_with_env_and_run_under(EXAMPLE, &PLAIN, &STRESS);
    assert!(ok, "stress run failed: {out}");
    assert_eq!(out, EXAMPLE_OUTPUT, "wrong output");

    let (ok, stderr) = compile_with_compiler_env(EXAMPLE, &[("WILLOW_LIR_LOG", "1")]);
    assert!(ok, "example did not compile through LIR: {stderr}");
    for function in ["via_argument", "via_return", "main"] {
        assert!(
            stderr.contains(&format!("[lir] compiling `{function}` from lowered IR")),
            "`{function}` was not walker-compiled: {stderr}"
        );
    }
}
