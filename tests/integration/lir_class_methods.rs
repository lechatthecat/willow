//! Class METHOD bodies on the LIR-walking backend (willow-0g8j.2.18).
//!
//! The walker was a free-function backend by accident. Eligibility could vet a
//! method, lowering already produced one under `Class::method`, and the emitter
//! already knew how to emit it — but `compile_class_method_inner` went straight
//! to `emit_block(&m.body)` and never asked. Two things followed:
//!
//!   * every method in every program was compiled by the AST emitter, so a
//!     differential test whose work lived in methods compared that emitter
//!     against itself; and
//!   * `WILLOW_LIR_REQUIRE=1` passed on such a program while checking nothing,
//!     which is exactly the assurance it exists to provide.
//!
//! Admitting method bodies rests on facts the compile path now has to get
//! right, and every perspective here is a way of getting one of them wrong:
//!
//!   * THE KEY — a method's lowered body is stored under `Class::method`, not
//!     under its mangled symbol, so a path that looked up the symbol would find
//!     nothing and silently fall back;
//!   * THE RECEIVER — `self` is parameter 0 of the lowered function and the
//!     first argument of the method ABI. In an ASYNC method the parameters live
//!     in a heap frame, and numbering the lowered ones from the first DECLARED
//!     parameter's slot reads every one of them a slot late;
//!   * THE CONSTRUCTOR — `init` is a method, so a constructor body is walked
//!     too, `super.init(...)` included: the base's declared `init` when it has
//!     one, and otherwise stores into the base's memberwise field slots;
//!   * THE REST OF THE LANGUAGE — `defer`, `match`, loops, allocation, virtual
//!     self-calls and reference parameters are the same constructs inside a
//!     method as outside one.
//!
//! Each runtime test is differential: the same program compiled with the walker
//! on and off must print the same thing. The "on" side also sets
//! `WILLOW_LIR_REQUIRE=1`, so a body that quietly fell back is a compile error
//! rather than a comparison of the AST emitter against itself.
//!
//! Perspectives:
//!   1. every method body in a program is logged as walker-compiled
//!   2. an instance method reads and writes `self`'s fields
//!   3. a declared parameter arrives after the receiver, not in its slot
//!   4. four parameters keep their order and their types
//!   5. a static method: the ABI reserves the receiver slot, the body has none
//!   6. an explicit constructor body
//!   7. a constructor delegating to a DECLARED base constructor
//!   8. a constructor delegating to a MEMBERWISE base
//!   9. `super.init` up a three-level chain
//!  10. a zero-argument `super.init`
//!  11. `super.init` arguments are expressions, evaluated in order
//!  12. a virtual self-call from an inherited non-virtual method
//!  13. an override reached through a base-typed slot
//!  14. `defer` inside a method body
//!  15. a `defer` flushed by an early `return`
//!  16. a `match` inside a method
//!  17. a `while` loop inside a method
//!  18. a `&mut` parameter of a method writes through to the caller
//!  19. a `&` parameter of a method
//!  20. a `&mut` constructor parameter forwarded through `super.init`
//!  21. a method that allocates, under GC stress on both backends
//!  22. an async method reads its receiver, not the first parameter's slot
//!  23. an async method's parameters keep their order across a park
//!  24. an async STATIC method has no receiver in its frame
//!  25. an async method loops across a park
//!  26. a method whose body the walker refuses is a `WILLOW_LIR_REQUIRE` error
//!  27. a refused method still compiles and runs when REQUIRE is off
//!  28. `example/lir_class_methods.wi` compiles with no fallback at all

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

// 1. The log line proves the body came from lowered IR. Without it a test can
// only observe that the program ran, which the AST emitter also achieves.
#[test]
fn lir_method_01_every_body_is_logged_as_walker_compiled() {
    let src = "class Point {
    pub x: i64;
    pub y: i64;
    pub init(self, x: i64, y: i64) { self.x = x; self.y = y; }
    pub fn sum(self) -> i64 { return self.x + self.y; }
    pub static fn origin() -> i64 { return 0; }
}

fn main() { println(new Point(2, 3).sum() + Point::origin()); }";
    let (ok, stderr) = compile_with_compiler_env(src, &LIR_LOG);
    assert!(ok, "the program must compile: {stderr}");
    for name in ["Point::init", "Point::sum", "Point::origin"] {
        assert!(
            stderr.contains(&format!("[lir] compiling `{name}` from lowered IR")),
            "`{name}` was not walker-compiled:\n{stderr}"
        );
    }
}

// 2. Fields are addressed off the receiver, so a read and a write in the same
// body must land on the same object.
#[test]
fn lir_method_02_instance_method_reads_and_writes_self() {
    let src = "class Cell {
    pub v: i64;
    pub fn bump(self) { self.v = self.v + 1; }
    pub fn get(self) -> i64 { return self.v; }
}

fn main() {
    let c = new Cell(5);
    c.bump();
    c.bump();
    println(c.get());
}";
    assert_both_backends(src, "7\n");
}

// 3. The receiver occupies argument 0, so a body that numbered the lowered
// parameters from zero would read the receiver POINTER as `n`.
#[test]
fn lir_method_03_declared_parameter_follows_the_receiver() {
    let src = "class Adder {
    pub base: i64;
    pub fn plus(self, n: i64) -> i64 { return self.base + n; }
}

fn main() { println(new Adder(10).plus(7)); }";
    assert_both_backends(src, "17\n");
}

// 4. Order and type together: a shift by one slot would also swap types here.
#[test]
fn lir_method_04_four_parameters_keep_order_and_type() {
    let src = "class Mixer {
    pub tag: String;
    pub fn mix(self, a: i64, b: String, c: bool, d: f64) -> String {
        return self.tag + \"/\" + a.toString() + b + c.toString() + d.toString();
    }
}

fn main() { println(new Mixer(\"m\").mix(1, \"x\", true, 2.5)); }";
    assert_both_backends(src, "m/1xtrue2.5\n");
}

// 5. A static method keeps the hidden receiver slot in the ABI (callers pass a
// dummy), but binds no `self`. Its lowered parameter list starts at the first
// declared parameter, unlike an instance method's.
#[test]
fn lir_method_05_static_method_has_no_receiver() {
    let src = "class Math {
    pub n: i64;
    pub static fn add(a: i64, b: i64) -> i64 { return a + b; }
    pub static fn label() -> String { return \"math\"; }
}

fn main() {
    println(Math::add(4, 5));
    println(Math::label());
}";
    assert_both_backends(src, "9\nmath\n");
}

// 6. `init` is a method whose body runs on an object the caller allocated, so
// its field stores must land in that object.
#[test]
fn lir_method_06_explicit_constructor_body() {
    let src = "class Range2 {
    pub lo: i64;
    pub hi: i64;
    pub init(self, lo: i64, span: i64) {
        self.lo = lo;
        self.hi = lo + span;
    }
}

fn main() {
    let r = new Range2(3, 4);
    println(r.lo);
    println(r.hi);
}";
    assert_both_backends(src, "3\n7\n");
}

// 7. `super.init` on a base that DECLARES a constructor is a call to that
// constructor with the receiver already in hand.
#[test]
fn lir_method_07_super_init_calls_a_declared_base_constructor() {
    let src = "open class Base {
    pub a: i64;
    pub init(self, a: i64) { self.a = a * 10; }
}

class Sub extends Base {
    pub b: i64;
    pub init(self, a: i64, b: i64) {
        super.init(a);
        self.b = b;
    }
}

fn main() {
    let s = new Sub(2, 3);
    println(s.a);
    println(s.b);
}";
    assert_both_backends(src, "20\n3\n");
}

// 8. A base with no declared constructor: the arguments go straight into its
// memberwise field slots, which are the object's leading words.
#[test]
fn lir_method_08_super_init_fills_a_memberwise_base() {
    let src = "open class Tag {
    pub text: String;
    pub weight: i64;
}

class Stamp extends Tag {
    pub seal: String;
    pub init(self, text: String, weight: i64) {
        super.init(text, weight);
        self.seal = \"*\";
    }
}

fn main() {
    let s = new Stamp(\"hi\", 4);
    println(s.text);
    println(s.weight);
    println(s.seal);
}";
    assert_both_backends(src, "hi\n4\n*\n");
}

// 9. Three levels: each `init` runs its base's before its own stores, so the
// fields end up in declaration order however deep the chain is.
#[test]
fn lir_method_09_super_init_up_a_three_level_chain() {
    let src = "open class L1 {
    pub a: i64;
    pub init(self, a: i64) { self.a = a; }
}

open class L2 extends L1 {
    pub b: i64;
    pub init(self, a: i64) {
        super.init(a + 1);
        self.b = a + 2;
    }
}

class L3 extends L2 {
    pub c: i64;
    pub init(self, a: i64) {
        super.init(a);
        self.c = self.a + self.b;
    }
}

fn main() {
    let v = new L3(10);
    println(v.a);
    println(v.b);
    println(v.c);
}";
    assert_both_backends(src, "11\n12\n23\n");
}

// 10. A zero-argument `super.init` has no argument to borrow a source position
// from, and the emitted call still needs one for its call-chain frame.
#[test]
fn lir_method_10_zero_argument_super_init() {
    let src = "open class Root {
    pub a: i64;
    pub init(self) { self.a = 9; }
}

class Leaf extends Root {
    pub b: i64;
    pub init(self) {
        super.init();
        self.b = self.a + 1;
    }
}

fn main() {
    let l = new Leaf();
    println(l.a + l.b);
}";
    assert_both_backends(src, "19\n");
}

// 11. The arguments are ordinary expressions and are evaluated left to right,
// before the base constructor runs.
#[test]
fn lir_method_11_super_init_arguments_are_expressions() {
    let src = "fn note(n: i64) -> i64 {
    println(n);
    return n;
}

open class Pair {
    pub x: i64;
    pub y: i64;
    pub init(self, x: i64, y: i64) {
        self.x = x;
        self.y = y;
    }
}

class Shifted extends Pair {
    pub z: i64;
    pub init(self, n: i64) {
        super.init(note(n * 2), note(n + 3));
        self.z = self.x + self.y;
    }
}

fn main() {
    let s = new Shifted(4);
    println(s.z);
}";
    assert_both_backends(src, "8\n7\n15\n");
}

// 12. A non-virtual method inherited by a subclass makes VIRTUAL self-calls, so
// the one compiled body runs the subclass's override.
#[test]
fn lir_method_12_virtual_self_call_from_an_inherited_method() {
    let src = "open class Fee {
    pub amount: i64;
    pub open fn rate(self) -> i64 { return 1; }
    pub fn charged(self) -> i64 { return self.amount * self.rate(); }
}

class Premium extends Fee {
    pub override fn rate(self) -> i64 { return 3; }
}

fn main() {
    println(new Fee(5).charged());
    println(new Premium(5).charged());
}";
    assert_both_backends(src, "5\n15\n");
}

// 13. Through a base-typed slot the concrete class is not statically known, so
// the call reads the descriptor slot.
#[test]
fn lir_method_13_override_through_a_base_typed_slot() {
    let src = "open class Animal {
    pub n: i64;
    pub open fn speak(self) -> String { return \"...\"; }
}

class Dog extends Animal {
    pub override fn speak(self) -> String { return \"woof\"; }
}

fn heard(a: Animal) -> String { return a.speak(); }

fn main() {
    println(heard(new Animal(1)));
    println(heard(new Dog(1)));
}";
    assert_both_backends(src, "...\nwoof\n");
}

// 14. A `defer` in a method runs at the end of the method's scope, after the
// return value has been computed.
#[test]
fn lir_method_14_defer_inside_a_method() {
    let src = "class Job {
    pub id: i64;
    pub fn run(self) -> i64 {
        defer println(\"done\");
        println(\"start\");
        return self.id;
    }
}

fn main() { println(new Job(3).run()); }";
    assert_both_backends(src, "start\ndone\n3\n");
}

// 15. An early `return` has to flush the scope it leaves, which is a different
// exit path from the fallthrough one above.
#[test]
fn lir_method_15_defer_flushed_by_an_early_return() {
    let src = "class Gate {
    pub open_now: bool;
    pub fn check(self) -> String {
        defer println(\"closed\");
        if self.open_now {
            return \"in\";
        }
        return \"out\";
    }
}

fn main() {
    println(new Gate(true).check());
    println(new Gate(false).check());
}";
    assert_both_backends(src, "closed\nin\nclosed\nout\n");
}

// 16. A `match` over a value derived from `self`: the dispatch chain spans
// several LIR blocks inside the method body.
#[test]
fn lir_method_16_match_inside_a_method() {
    let src = "class Crew {
    pub size: i64;
    pub fn describe(self) -> String {
        return match self.size {
            0 => \"none\",
            1 => \"one\",
            _ => \"many\",
        };
    }
}

fn main() {
    println(new Crew(0).describe());
    println(new Crew(1).describe());
    println(new Crew(7).describe());
}";
    assert_both_backends(src, "none\none\nmany\n");
}

// 17. A loop inside a method body, with the induction variable and the
// accumulator both local to the method.
#[test]
fn lir_method_17_loop_inside_a_method() {
    let src = "class Sum {
    pub upto: i64;
    pub fn total(self) -> i64 {
        let mut t = 0;
        let mut i = 0;
        while i < self.upto {
            t = t + i;
            i = i + 1;
        }
        return t;
    }
}

fn main() { println(new Sum(5).total()); }";
    assert_both_backends(src, "10\n");
}

// 18. A `&mut` parameter of a method is passed by address, so the write lands
// in the CALLER's storage and survives the call.
#[test]
fn lir_method_18_mut_reference_parameter_writes_through() {
    let src = "class Step {
    pub by: i64;
    pub fn apply(self, slot: &mut i64) {
        slot = slot + self.by;
    }
}

fn main() {
    let s = new Step(4);
    let mut n = 10;
    s.apply(&n);
    s.apply(&n);
    println(n);
}";
    assert_both_backends(src, "18\n");
}

// 19. A `&` parameter is read-only but still an address: the callee dereferences
// it rather than receiving a copied word.
#[test]
fn lir_method_19_shared_reference_parameter() {
    let src = "class Scale {
    pub factor: i64;
    pub fn of(self, v: &i64) -> i64 { return v * self.factor; }
}

fn main() {
    let s = new Scale(3);
    let v = 7;
    println(s.of(&v));
}";
    assert_both_backends(src, "21\n");
}

// 20. A constructor's `&mut` parameter is already an address; `super.init(&p)`
// forwards the SAME place, so both constructors write to the caller's local.
#[test]
fn lir_method_20_mut_reference_forwarded_through_super_init() {
    let src = "open class Opening {
    pub start: i64;
    pub init(self, balance: &mut i64) {
        balance = balance * 2;
        self.start = balance;
    }
}

class Book extends Opening {
    pub entries: i64;
    pub init(self, balance: &mut i64, entries: i64) {
        super.init(&balance);
        balance = balance + 1;
        self.entries = entries;
    }
}

fn main() {
    let mut money = 5;
    let b = new Book(&money, 2);
    println(b.start);
    println(b.entries);
    println(money);
}";
    assert_both_backends(src, "10\n2\n11\n");
}

// 21. A method that allocates must root what it holds across the allocation.
// GC stress collects at every allocation site, so a missing root is a use of
// freed memory rather than a silently surviving object.
#[test]
fn lir_method_21_allocating_method_under_gc_stress() {
    let src = "import std::collections::Array;

class Report {
    pub title: String;
    pub fn lines(self, n: i64) -> Array<String> {
        let out: Array<String> = [];
        let mut i = 0;
        while i < n {
            out.push(self.title + \"-\" + i.toString());
            i = i + 1;
        }
        return out;
    }
}

fn main() {
    let r = new Report(\"row\");
    let ls = r.lines(3);
    println(ls.len());
    println(ls[0]);
    println(ls[2]);
}";
    let expected = "3\nrow-0\nrow-2\n";
    let stress = [("WILLOW_GC_STRESS", "alloc")];
    let (on, ok_on) = compile_with_env_and_run_under(src, &LIR_ON, &stress);
    assert!(ok_on, "walker build under GC stress failed: {on}");
    let (off, ok_off) = compile_with_env_and_run_under(src, &LIR_OFF, &stress);
    assert!(ok_off, "AST build under GC stress failed: {off}");
    assert_eq!(on, off, "the two backends disagreed under GC stress");
    assert_eq!(on, expected);
}

// 22. THE async off-by-one. An async method's parameters live in the heap
// frame; the lowered list starts with `self`, and the frame reserves a slot for
// it ahead of the declared parameters. Numbering the lowered list from the
// first DECLARED parameter's slot reads `self` out of `extra`'s slot — which
// prints nothing at all, because the "receiver" is then the integer 5.
#[test]
fn lir_method_22_async_method_reads_its_receiver() {
    let src = "class Box {
    pub v: i64;
    pub async fn get(self, extra: i64) -> i64 {
        await sleep(1);
        return self.v + extra;
    }
}

async fn main() {
    let b = new Box(6);
    println(await b.get(5));
}";
    assert_both_backends(src, "11\n");
}

// 23. Three parameters after the receiver: a one-slot shift would still produce
// a number, just the wrong one, so the values are chosen to disagree.
#[test]
fn lir_method_23_async_method_parameter_order_survives_a_park() {
    let src = "class Joiner {
    pub head: String;
    pub async fn join(self, a: i64, b: String, c: bool) -> String {
        await sleep(1);
        return self.head + a.toString() + b + c.toString();
    }
}

async fn main() {
    let j = new Joiner(\"h:\");
    println(await j.join(1, \"x\", false));
}";
    assert_both_backends(src, "h:1xfalse\n");
}

// 24. A static async method has NO receiver slot, so its lowered parameter list
// and its frame slots start together — the opposite convention from an instance
// method, and both have to be right at once.
#[test]
fn lir_method_24_async_static_method_has_no_receiver_slot() {
    let src = "class Timer {
    pub n: i64;
    pub static async fn after(a: i64, b: i64) -> i64 {
        await sleep(1);
        return a * 100 + b;
    }
}

async fn main() { println(await Timer::after(3, 4)); }";
    assert_both_backends(src, "304\n");
}

// 25. A loop that parks on every iteration: the receiver and the locals are
// read back out of the frame after each resume, not just once.
#[test]
fn lir_method_25_async_method_loops_across_a_park() {
    let src = "class Ticker {
    pub step: i64;
    pub async fn run(self, times: i64) -> i64 {
        let mut t = 0;
        let mut i = 0;
        while i < times {
            await sleep(1);
            t = t + self.step;
            i = i + 1;
        }
        return t;
    }
}

async fn main() { println(await new Ticker(3).run(4)); }";
    assert_both_backends(src, "12\n");
}

// 26. `WILLOW_LIR_REQUIRE=1` now polices methods, and `Self::method` resolves
// against the class whose body is being compiled rather than forcing fallback.
#[test]
fn lir_method_26_self_static_call_is_walker_compiled() {
    let src = "class Helper {
    pub n: i64;
    pub static fn twice(v: i64) -> i64 { return v * 2; }
    pub fn viaself(self) -> i64 { return Self::twice(self.n); }
}

fn main() { println(new Helper(3).viaself()); }";
    let (ok, stderr) = compile_with_compiler_env(src, &LIR_LOG);
    assert!(
        ok,
        "`Self::twice` must stay in the walker subset:\n{stderr}"
    );
    for name in ["Helper::twice", "Helper::viaself"] {
        assert!(
            stderr.contains(&format!("[lir] compiling `{name}` from lowered IR")),
            "`{name}` was not walker-compiled:\n{stderr}"
        );
    }
}

// 27. The resolved call produces the same result on both emitters.
#[test]
fn lir_method_27_self_static_call_runs_on_both_backends() {
    let src = "class Helper {
    pub n: i64;
    pub static fn twice(v: i64) -> i64 { return v * 2; }
    pub fn viaself(self) -> i64 { return Self::twice(self.n); }
    pub fn plain(self) -> i64 { return self.n + 1; }
}

fn main() {
    let h = new Helper(3);
    println(h.viaself());
    println(h.plain());
}";
    let (on, ok_on) = compile_with_env_and_run(src, &[("WILLOW_LIR_BACKEND", "1")]);
    assert!(ok_on, "fallback build failed: {on}");
    let (off, ok_off) = compile_with_env_and_run(src, &LIR_OFF);
    assert!(ok_off, "AST-path run failed: {off}");
    assert_eq!(on, off);
    assert_eq!(on, "6\n4\n");
}

// 28. The example file: every method in it, constructors included, must be
// claimed by the walker, and both backends must print the same thing.
#[test]
fn lir_method_28_example_file_compiles_with_no_fallback() {
    let src = std::fs::read_to_string("example/lir_class_methods.wi")
        .expect("example/lir_class_methods.wi must exist");
    assert_both_backends(
        &src,
        "12\n12\n- left ops\nops:12\n30\n60\nmany\n- left pay\npay:30\n- left void\nvoid:empty\nops+pay\n42\n*seal7*\n112\n24\n11\n",
    );
}
