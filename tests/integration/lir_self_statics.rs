//! `Self::` and the implicit receiver on the LIR-walking backend
//! (willow-0g8j.13, willow-0g8j.14, willow-h7hv).
//!
//! Three defects, one root: a class body can name things two ways, and the
//! compiler took the shorter spelling to mean something weaker than it does.
//!
//!   * `Self::` — inside a class body `Self` is the class the body is DECLARED
//!     in. The walker's eligibility had no idea which class it was inside, so
//!     every `Self::method(..)`, `Self::prop` and `Self::prop = ..` resolved to
//!     nothing and the whole method fell back to the AST emitter. Eligibility
//!     and emission now resolve it the same way — out of the lowered
//!     `Class::method` name and out of `FuncGen::current_class` — because a
//!     name admitted under one class and emitted under another would be a
//!     miscompile, not a fallback.
//!   * THE IMPLICIT RECEIVER — `pub fn get() -> i64 { return self.value; }`
//!     declares an instance method whose receiver is not written out.
//!     `MethodDecl::has_self` records only the explicit (legacy) `self`
//!     spelling, and `is_static` is what actually decides whether a method has
//!     a receiver. HIR lowering keyed the receiver binding on `has_self`, so
//!     `self` was unbound, lowering FAILED, and the method had no lowered IR at
//!     all — a silent fallback, and a `WILLOW_LIR_REQUIRE=1` error.
//!   * THE VTABLE SLOT — the same `has_self` mistake in
//!     `register_class_own_vmethods` denied an implicit-self `open`/`override`
//!     method its virtual slot. That one was not a fallback: virtual dispatch
//!     found two implementations with nowhere to dispatch through and the
//!     compiler panicked, on BOTH backends.
//!
//! Every runtime test is differential: the same program with the walker on and
//! off must print the same thing, and the "on" side sets
//! `WILLOW_LIR_REQUIRE=1` so a body that quietly fell back is a compile error
//! rather than the AST emitter being compared against itself.
//!
//! Perspectives:
//!   1. every `Self::`-using body is logged as walker-compiled
//!   2. an instance method calls a static of its own class through `Self::`
//!   3. a static method calls another static through `Self::`
//!   4. `Self::` arguments keep their order
//!   5. `Self::` in an INHERITED body names the class that declared it
//!   6. a `Self::` static property read
//!   7. a `Self::` static property assignment
//!   8. read and assignment of the same property in one body
//!   9. `Self::` inside a loop body
//!  10. `Self::` inside a `match` arm
//!  11. `Self::` inside a `defer` body
//!  12. a `Self::` factory that constructs its own class
//!  13. `Self::` inside a constructor body
//!  14. `Self::name` and `Class::name` are the same call
//!  15. `Self::` inside an async method
//!  16. a `Self::` static returning a String, under GC stress
//!  17. an implicit receiver reads a field
//!  18. an implicit receiver takes declared parameters after it
//!  19. an implicit receiver writes a field
//!  20. an implicit receiver calls another method through `self`
//!  21. an implicit receiver and a `&mut` parameter in one signature
//!  22. an implicit receiver on an async method
//!  23. an implicit-self `override` dispatches on the runtime class
//!  24. an implicit-self method satisfies an interface
//!  25. explicit and implicit receivers in the same class
//!  26. a subclass's implicit-self method reads an inherited field
//!  27. every implicit-self body is logged as walker-compiled
//!  28. `example/lir_self_statics.wi` compiles with no fallback at all
//!  29. the two examples that used to be the last corpus holdouts

use super::support::{
    compile_with_compiler_env, compile_with_env_and_run, compile_with_env_and_run_under,
};

/// The walker is on by default, so the "on" side names it explicitly rather
/// than trusting the ambient environment. `WILLOW_LIR_REQUIRE=1` is what makes
/// the comparison meaningful: without it a rejected body would fall back and
/// both sides would run the same AST emitter.
const LIR_ON: [(&str, &str); 2] = [("WILLOW_LIR_BACKEND", "1"), ("WILLOW_LIR_REQUIRE", "1")];
const LIR_OFF: [(&str, &str); 1] = [("WILLOW_LIR_BACKEND", "0")];
const LIR_LOG: [(&str, &str); 3] = [
    ("WILLOW_LIR_BACKEND", "1"),
    ("WILLOW_LIR_REQUIRE", "1"),
    ("WILLOW_LIR_LOG", "1"),
];

#[track_caller]
fn assert_both_backends(source: &str, expected: &str) {
    let (with_lir, ok_on) = compile_with_env_and_run(source, &LIR_ON);
    assert!(
        ok_on,
        "the walker must claim every method in this program: {with_lir}"
    );
    let (without_lir, ok_off) = compile_with_env_and_run(source, &LIR_OFF);
    assert!(ok_off, "AST-path run failed: {without_lir}");
    assert_eq!(
        with_lir, without_lir,
        "the two backends disagreed about a method body"
    );
    assert_eq!(with_lir, expected);
}

// 1. The log line is the proof: `Self::` used to make a method fall back
// silently, and a program that merely RUNS proves nothing about which emitter
// compiled it.
#[test]
fn lir_self_01_every_self_body_is_logged_as_walker_compiled() {
    let src = "class Meter {
    pub n: i64;
    pub static mut seen: i64 = 0;
    pub static fn twice(v: i64) -> i64 { return v * 2; }
    pub static fn note() -> i64 { Self::seen = Self::seen + 1; return Self::seen; }
    pub fn viaself(self) -> i64 { return Self::twice(self.n); }
}

fn main() { println(new Meter(3).viaself() + Meter::note()); }";
    let (ok, stderr) = compile_with_compiler_env(src, &LIR_LOG);
    assert!(ok, "the program must compile: {stderr}");
    for name in ["Meter::twice", "Meter::note", "Meter::viaself"] {
        assert!(
            stderr.contains(&format!("[lir] compiling `{name}` from lowered IR")),
            "`{name}` was not walker-compiled:\n{stderr}"
        );
    }
}

// 2. The bead's own repro: an instance method reaching a static of its own
// class through `Self::`.
#[test]
fn lir_self_02_an_instance_method_calls_a_static() {
    assert_both_backends(
        "class Helper {
    pub n: i64;
    pub static fn twice(v: i64) -> i64 { return v * 2; }
    pub fn viaself(self) -> i64 { return Self::twice(self.n); }
}

fn main() { println(new Helper(3).viaself()); }",
        "6\n",
    );
}

// 3. A STATIC body has no receiver, so `Self` cannot come from one: it is the
// declaring class either way.
#[test]
fn lir_self_03_a_static_method_calls_a_static() {
    assert_both_backends(
        "class Helper {
    pub static fn twice(v: i64) -> i64 { return v * 2; }
    pub static fn quad(v: i64) -> i64 { return Self::twice(Self::twice(v)); }
}

fn main() { println(Helper::quad(3)); }",
        "12\n",
    );
}

// 4. A resolved name must not disturb the arguments: the call is an ordinary
// static call once `Self` is mapped.
#[test]
fn lir_self_04_arguments_keep_their_order() {
    assert_both_backends(
        "class Digits {
    pub static fn pick(a: i64, b: i64, c: i64) -> i64 { return a * 100 + b * 10 + c; }
    pub fn order(self) -> i64 { return Self::pick(1, 2, 3); }
}

fn main() { println(new Digits().order()); }",
        "123\n",
    );
}

// 5. `Self` is the class the BODY is declared in, not the receiver's runtime
// class — an inherited method still resolves it to the base that declared it.
#[test]
fn lir_self_05_an_inherited_body_names_its_declaring_class() {
    assert_both_backends(
        "open class Base {
    pub n: i64;
    pub static fn tag() -> String { return \"base\"; }
    pub fn label(self) -> String { return Self::tag(); }
}

class Kid extends Base {}

fn main() {
    let k: Base = new Kid(1);
    println(k.label());
    println(new Base(1).label());
}",
        "base\nbase\n",
    );
}

// 6. A static PROPERTY read is a different HIR node from a static call, and it
// needed the same resolution — plus a checker-typed fallback in lowering, which
// only the call arm had.
#[test]
fn lir_self_06_a_static_property_read() {
    assert_both_backends(
        "class Store {
    pub static mut count: i64 = 7;
    pub static fn peek() -> i64 { return Self::count; }
}

fn main() { println(Store::peek()); }",
        "7\n",
    );
}

// 7. The assignment half: a `Self::prop = ..` statement resolves to the same
// global slot the emitter stores into.
#[test]
fn lir_self_07_a_static_property_assignment() {
    assert_both_backends(
        "class Store {
    pub static mut count: i64 = 0;
    pub static fn set(v: i64) { Self::count = v; }
}

fn main() {
    Store::set(4);
    println(Store::count);
}",
        "4\n",
    );
}

// 8. Both halves in one body: the read must see the store, which it can only do
// if the two resolved to the same storage.
#[test]
fn lir_self_08_a_read_and_an_assignment_of_one_property() {
    assert_both_backends(
        "class Counter {
    pub static mut total: i64 = 0;
    pub static fn bump(by: i64) -> i64 {
        Self::total = Self::total + by;
        return Self::total;
    }
}

fn main() {
    println(Counter::bump(5));
    println(Counter::bump(2));
    println(Counter::total);
}",
        "5\n7\n7\n",
    );
}

// 9. Inside a loop body the call is re-emitted per iteration, from the same
// resolved symbol.
#[test]
fn lir_self_09_inside_a_loop_body() {
    assert_both_backends(
        "class Sum {
    pub static fn weigh(n: i64) -> i64 { return n * 2; }
    pub fn total(self, rounds: i64) -> i64 {
        let mut sum = 0;
        let mut i = 0;
        while i < rounds {
            sum = sum + Self::weigh(i);
            i = i + 1;
        }
        return sum;
    }
}

fn main() { println(new Sum().total(4)); }",
        "12\n",
    );
}

// 10. A `match` arm body is checked deep inside the expression walker, where
// the enclosing function is out of reach — which is exactly why the enclosing
// class travels in the type context rather than as an argument.
#[test]
fn lir_self_10_inside_a_match_arm() {
    assert_both_backends(
        "class Grade {
    pub static fn label(n: i64) -> String { return \"n\" + n.toString(); }
    pub fn of(self, kind: i64) -> String {
        match kind {
            0 => { return Self::label(0); }
            _ => { return Self::label(9); }
        }
    }
}

fn main() {
    let g = new Grade();
    println(g.of(0));
    println(g.of(1));
}",
        "n0\nn9\n",
    );
}

// 11. A `defer` body is a separate HIR island, walked under its own scope; the
// class it belongs to is still this one.
#[test]
fn lir_self_11_inside_a_defer_body() {
    assert_both_backends(
        "class Closing {
    pub static fn note() -> String { return \"closed\"; }
    pub fn run(self) -> i64 {
        defer { println(Self::note()); }
        return 3;
    }
}

fn main() { println(new Closing().run()); }",
        "closed\n3\n",
    );
}

// 12. A factory: the resolved class is also the class being constructed, so a
// wrong mapping would allocate the wrong layout rather than fail to link.
#[test]
fn lir_self_12_a_factory_constructs_its_own_class() {
    assert_both_backends(
        "class Node {
    pub v: i64;
    pub static fn make(v: i64) -> Node { return new Node(v); }
    pub static fn start(v: i64) -> Node { return Self::make(v + 1); }
}

fn main() { println(Node::start(40).v); }",
        "41\n",
    );
}

// 13. A constructor is a method (`Class::init`), so its body is walked too and
// `Self::` in it resolves against the class under construction.
#[test]
fn lir_self_13_inside_a_constructor_body() {
    assert_both_backends(
        "class Tagged {
    pub v: i64;
    pub static mut made: i64 = 0;
    pub static fn seal(v: i64) -> i64 { return v * 3; }
    pub init(self, v: i64) {
        self.v = Self::seal(v);
        Self::made = Self::made + 1;
    }
}

fn main() {
    let t = new Tagged(4);
    println(t.v);
    println(Tagged::made);
}",
        "12\n1\n",
    );
}

// 14. The two spellings must produce one call. If they resolved differently the
// walker would be admitting a name the emitter sends somewhere else.
#[test]
fn lir_self_14_self_and_the_class_name_agree() {
    assert_both_backends(
        "class Both {
    pub static fn twice(v: i64) -> i64 { return v * 2; }
    pub fn spelled(self) -> i64 { return Self::twice(5) - Both::twice(5); }
}

fn main() { println(new Both().spelled()); }",
        "0\n",
    );
}

// 15. An async method's body is compiled into a resumable frame by a different
// path; the class it belongs to has to reach that path too.
#[test]
fn lir_self_15_inside_an_async_method() {
    assert_both_backends(
        "class Slow {
    pub n: i64;
    pub static fn twice(v: i64) -> i64 { return v * 2; }
    pub async fn doubled(self) -> i64 { return Self::twice(self.n); }
}

async fn main() { println(await new Slow(21).doubled()); }",
        "42\n",
    );
}

// 16. A `Self::` static that ALLOCATES: the returned String is a fresh object,
// so under stress every allocation is a collection point.
#[test]
fn lir_self_16_an_allocating_static_under_gc_stress() {
    let src = "class Naming {
    pub n: i64;
    pub static fn tag(v: i64) -> String { return \"tag-\" + v.toString(); }
    pub fn named(self) -> String { return Self::tag(self.n) + \"!\"; }
}

fn main() {
    let n = new Naming(2);
    println(n.named());
    println(n.named());
}";
    let expected = "tag-2!\ntag-2!\n";
    let stress = [("WILLOW_GC_STRESS", "alloc")];
    let (on, ok_on) = compile_with_env_and_run_under(src, &LIR_ON, &stress);
    assert!(ok_on, "walker build under GC stress failed: {on}");
    let (off, ok_off) = compile_with_env_and_run_under(src, &LIR_OFF, &stress);
    assert!(ok_off, "AST build under GC stress failed: {off}");
    assert_eq!(on, off, "the two backends disagreed under GC stress");
    assert_eq!(on, expected);
}

// 17. The implicit receiver, at its smallest: no `self` in the parameter list,
// `self` in the body. Lowering used to leave that name unbound and drop the
// whole method.
#[test]
fn lir_self_17_an_implicit_receiver_reads_a_field() {
    assert_both_backends(
        "class Counter {
    pub value: i64;
    pub fn get() -> i64 { return self.value; }
}

fn main() { println(new Counter(40).get()); }",
        "40\n",
    );
}

// 18. Declared parameters follow a receiver that is not written down, so the
// lowered parameter list is one longer than the declaration — the numbering the
// async frame layout also depends on.
#[test]
fn lir_self_18_an_implicit_receiver_takes_parameters() {
    assert_both_backends(
        "class Counter {
    pub value: i64;
    pub fn plus(a: i64, b: i64) -> i64 { return self.value * 100 + a * 10 + b; }
}

fn main() { println(new Counter(4).plus(1, 2)); }",
        "412\n",
    );
}

// 19. The receiver is a real pointer, not a read-only copy: a store through it
// has to reach the caller's object.
#[test]
fn lir_self_19_an_implicit_receiver_writes_a_field() {
    assert_both_backends(
        "class Counter {
    pub value: i64;
    pub fn bump(by: i64) { self.value = self.value + by; }
}

fn main() {
    let c = new Counter(1);
    c.bump(41);
    println(c.value);
}",
        "42\n",
    );
}

// 20. A method call ON the implicit receiver: the receiver has to be in hand to
// pass it on.
#[test]
fn lir_self_20_an_implicit_receiver_calls_another_method() {
    assert_both_backends(
        "class Chain {
    pub value: i64;
    pub fn base() -> i64 { return self.value; }
    pub fn scaled() -> i64 { return self.base() * 3; }
}

fn main() { println(new Chain(5).scaled()); }",
        "15\n",
    );
}

// 21. An unwritten receiver and a written `&mut` parameter in one signature:
// the pointer parameter must land in the slot after the receiver.
#[test]
fn lir_self_21_an_implicit_receiver_with_a_reference_parameter() {
    assert_both_backends(
        "class Adder {
    pub by: i64;
    pub fn add_to(slot: &mut i64) { slot = slot + self.by; }
}

fn main() {
    let mut n = 40;
    new Adder(2).add_to(&n);
    println(n);
}",
        "42\n",
    );
}

// 22. The async frame lays its slots out from the LOWERED parameter list, which
// starts with the receiver whether or not the source wrote it.
#[test]
fn lir_self_22_an_implicit_receiver_on_an_async_method() {
    assert_both_backends(
        "class Slow {
    pub n: i64;
    pub async fn doubled(step: i64) -> i64 { return self.n * 2 + step; }
}

async fn main() { println(await new Slow(20).doubled(2)); }",
        "42\n",
    );
}

// 23. The vtable half (willow-h7hv): an implicit-self `open`/`override` pair
// used to panic the compiler outright — two implementations, no slot to
// dispatch through.
#[test]
fn lir_self_23_an_implicit_self_override_dispatches_virtually() {
    assert_both_backends(
        "open class Row {
    pub index: i64;
    pub open fn width() -> i64 { return self.index; }
}

class WideRow extends Row {
    pub override fn width() -> i64 { return self.index + 100; }
}

fn main() {
    let plain: Row = new Row(3);
    let wide: Row = new WideRow(3);
    println(plain.width());
    println(wide.width());
}",
        "3\n103\n",
    );
}

// 24. An interface requires an INSTANCE method, which an implicit-self method
// is: the receiver is what it lacks a spelling for, not what it lacks.
#[test]
fn lir_self_24_an_implicit_self_method_satisfies_an_interface() {
    assert_both_backends(
        "interface Describes {
    fn describe(self) -> String;
}

class Ticket implements Describes {
    pub seat: i64;
    pub fn describe() -> String { return \"seat \" + self.seat.toString(); }
}

fn spoken() -> String {
    let d: Describes = new Ticket(9);
    return d.describe();
}

fn main() { println(spoken()); }",
        "seat 9\n",
    );
}

// 25. Both spellings in one class: the explicit `self` is the legacy form and
// has to keep working beside the implicit one.
#[test]
fn lir_self_25_both_receiver_spellings_in_one_class() {
    assert_both_backends(
        "class Mixed {
    pub v: i64;
    pub fn spelled(self) -> i64 { return self.v; }
    pub fn unspelled() -> i64 { return self.v + 1; }
}

fn main() {
    let m = new Mixed(7);
    println(m.spelled());
    println(m.unspelled());
}",
        "7\n8\n",
    );
}

// 26. An inherited field through an implicit receiver: the receiver's type is
// the class the body is declared in, so the base's slots are in scope.
#[test]
fn lir_self_26_a_subclass_body_reads_an_inherited_field() {
    assert_both_backends(
        "open class Base {
    pub n: i64;
}

class Kid extends Base {
    pub fn doubled() -> i64 { return self.n * 2; }
}

fn main() { println(new Kid(21).doubled()); }",
        "42\n",
    );
}

// 27. The other silent-fallback proof: an implicit-self method had NO lowered
// IR at all, so the log is what shows it now has one.
#[test]
fn lir_self_27_every_implicit_body_is_logged_as_walker_compiled() {
    let src = "class Counter {
    pub value: i64;
    pub fn get() -> i64 { return self.value; }
    pub fn plus(n: i64) -> i64 { return self.value + n; }
}

fn main() {
    let c = new Counter(40);
    println(c.get());
    println(c.plus(2));
}";
    let (ok, stderr) = compile_with_compiler_env(src, &LIR_LOG);
    assert!(ok, "the program must compile: {stderr}");
    for name in ["Counter::get", "Counter::plus"] {
        assert!(
            stderr.contains(&format!("[lir] compiling `{name}` from lowered IR")),
            "`{name}` was not walker-compiled:\n{stderr}"
        );
    }
}

// 28. The shipped example: the same story at program scale, with no fallback
// anywhere in it.
#[test]
fn lir_self_28_the_example_file_compiles_with_no_fallback() {
    let src = std::fs::read_to_string("example/lir_self_statics.wi")
        .expect("example/lir_self_statics.wi must exist");
    assert_both_backends(
        &src,
        "1\n2\n2\nseat 2\n12\nseat\nrow\nclosed 2\n2\n42\n4\n3\n103\nseat 9\n",
    );
}

// 29. The two examples these defects kept out of the walker: they were the last
// files in `example/` that `WILLOW_LIR_REQUIRE=1` could not compile.
#[test]
fn lir_self_29_the_last_two_corpus_holdouts_compile() {
    for (path, expected) in [
        ("example/static_members.wi", "3\n25\n40\n42\n"),
        ("example/self_demo.wi", "10\n10\n10\n"),
    ] {
        let src = std::fs::read_to_string(path).unwrap_or_else(|_| panic!("{path} must exist"));
        let (out, ok) = compile_with_env_and_run(&src, &LIR_ON);
        assert!(ok, "{path} still falls back to the AST backend:\n{out}");
        assert_eq!(out, expected, "{path} printed the wrong thing");
    }
}
