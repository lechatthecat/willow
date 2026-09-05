//! Guards for the backend's `compiler invariant violated` panics
//! (willow-uqzx, catalog item 14).
//!
//! The Cranelift backend used to answer impossible states — a method the type
//! checker never approved, a field with no layout slot, a call through a value
//! that is not a function — by emitting `iconst.i64 0` and carrying on. That
//! turned a compiler bug into a wrong program: the generated binary ran, read a
//! zero where an object pointer belonged, and produced a wrong answer or a
//! segfault far from the cause. Those sites now panic with a
//! `compiler invariant violated: ...` message instead.
//!
//! A panic is only an improvement if it is genuinely unreachable through the
//! normal pipeline. That is what this file tests. Each case feeds the compiler
//! the program that *would* reach one of those arms and asserts two things:
//!
//!   1. compilation fails with a real diagnostic (`error[E...]`), so the user
//!      gets a source-located message; and
//!   2. stderr carries no `compiler invariant violated` text, so the rejection
//!      came from the front end and the backend arm was never entered.
//!
//! Assertion 2 is the one that matters. If a front-end check is ever weakened,
//! these tests fail with the ICE text rather than silently letting malformed
//! input reach codegen.
//!
//! The positive controls at the end compile and run the valid form of the same
//! constructs, proving the conversion did not make legitimate programs panic.

use super::support::{compile_and_run, compile_error_stderr};

/// Assert the front end rejects `source` before the backend sees it.
fn assert_rejected_before_codegen(source: &str) {
    let stderr = compile_error_stderr(source);
    assert!(
        !stderr.contains("compiler invariant violated"),
        "reached a backend invariant panic instead of a front-end diagnostic:\n{stderr}"
    );
    assert!(
        stderr.contains("error[E"),
        "expected a coded diagnostic, got:\n{stderr}"
    );
}

// ---------------------------------------------------------------------------
// emit_collections.rs:168 — checked Map method reached codegen
// ---------------------------------------------------------------------------

/// Perspective 1: a method name that no `Map` has at all.
#[test]
fn map_unknown_method_never_reaches_codegen() {
    assert_rejected_before_codegen(
        r#"
import std::collections::Map;

fn main() {
    let m: Map<String, i64> = Map::new();
    m.drain();
}
"#,
    );
}

/// Perspective 2: a real `Map` method called with the wrong arity. The backend
/// arm indexes `m.args` positionally, so a short argument list would panic on
/// the index rather than fall through to the catch-all.
#[test]
fn map_insert_wrong_arity_never_reaches_codegen() {
    assert_rejected_before_codegen(
        r#"
import std::collections::Map;

fn main() {
    let m: Map<String, i64> = Map::new();
    m.insert("k");
}
"#,
    );
}

/// Perspective 3: a name that exists on another collection but not on `Map`.
/// Checks the rejection is keyed to the receiver type, not to a global method
/// name pool.
#[test]
fn map_borrowing_an_array_method_never_reaches_codegen() {
    assert_rejected_before_codegen(
        r#"
import std::collections::Map;

fn main() {
    let m: Map<String, i64> = Map::new();
    m.push(1);
}
"#,
    );
}

// ---------------------------------------------------------------------------
// emit_builtins.rs:134 — checked Channel method reached codegen
// ---------------------------------------------------------------------------

/// Perspective 4: an unknown `Channel` method.
#[test]
fn channel_unknown_method_never_reaches_codegen() {
    assert_rejected_before_codegen(
        r#"
async fn main() {
    let ch = Channel<i64>::new();
    ch.flush();
}
"#,
    );
}

/// Perspective 5: `send` with no value. The backend reads `args[0]`.
#[test]
fn channel_send_wrong_arity_never_reaches_codegen() {
    assert_rejected_before_codegen(
        r#"
async fn main() {
    let ch = Channel<i64>::new();
    ch.send();
}
"#,
    );
}

/// Perspective 6: `send` with a value of the wrong element type. The backend
/// coerces the argument to the channel's element width without rechecking it.
#[test]
fn channel_send_wrong_element_type_never_reaches_codegen() {
    assert_rejected_before_codegen(
        r#"
async fn main() {
    let ch = Channel<i64>::new();
    ch.send("not a number");
}
"#,
    );
}

// ---------------------------------------------------------------------------
// emit_interface.rs:56 — checked interface method has no vtable slot
// ---------------------------------------------------------------------------

/// Perspective 7: calling a method through an interface value that the
/// interface does not declare. There is no slot to index.
#[test]
fn interface_method_not_on_interface_never_reaches_codegen() {
    assert_rejected_before_codegen(
        r#"
interface Named {
    fn name(self) -> String;
}

class Person implements Named {
    pub label: String;

    pub fn name(self) -> String {
        return self.label;
    }

    pub fn secret(self) -> i64 {
        return 7;
    }
}

fn describe(n: Named) -> i64 {
    return n.secret();
}

fn main() {
    println(describe(new Person("Alice")));
}
"#,
    );
}

/// Perspective 8: a class that claims an interface but omits one of its
/// methods. Its vtable would have a hole where the missing slot belongs.
#[test]
fn incomplete_interface_impl_never_reaches_codegen() {
    assert_rejected_before_codegen(
        r#"
interface Shape {
    fn area(self) -> i64;
    fn perimeter(self) -> i64;
}

class Square implements Shape {
    pub side: i64;

    pub fn area(self) -> i64 {
        return self.side * self.side;
    }
}

fn main() {
    let s = new Square(3);
    println(s.area());
}
"#,
    );
}

/// Perspective 9: an interface method implemented with the wrong signature.
/// The slot exists, but an indirect call through it would use a signature the
/// callee does not have.
#[test]
fn interface_impl_signature_mismatch_never_reaches_codegen() {
    assert_rejected_before_codegen(
        r#"
interface Shape {
    fn scaled(self, factor: i64) -> i64;
}

class Square implements Shape {
    pub side: i64;

    pub fn scaled(self) -> i64 {
        return self.side;
    }
}

fn main() {
    let s = new Square(3);
    println(s.scaled(2));
}
"#,
    );
}

// ---------------------------------------------------------------------------
// emit_interface.rs:492 — checked class method has no dispatch target
// ---------------------------------------------------------------------------

/// Perspective 10: a method the class does not declare.
#[test]
fn class_unknown_method_never_reaches_codegen() {
    assert_rejected_before_codegen(
        r#"
class Counter {
    pub value: i64;

    pub fn get(self) -> i64 {
        return self.value;
    }
}

fn main() {
    let c = new Counter(1);
    println(c.increment());
}
"#,
    );
}

/// Perspective 11: a private method called from outside the class. Visibility
/// is a front-end concern; the backend would happily find a dispatch target.
#[test]
fn class_private_method_from_outside_never_reaches_codegen() {
    assert_rejected_before_codegen(
        r#"
class Counter {
    pub value: i64;

    fn hidden(self) -> i64 {
        return self.value;
    }
}

fn main() {
    let c = new Counter(1);
    println(c.hidden());
}
"#,
    );
}

/// Perspective 12: a method called on a class whose name is not declared
/// anywhere. There is neither a layout nor a type id for it.
#[test]
fn unknown_class_never_reaches_codegen() {
    assert_rejected_before_codegen(
        r#"
fn main() {
    let c = new NoSuchClass(1);
    println(c.get());
}
"#,
    );
}

// ---------------------------------------------------------------------------
// emit_interface.rs:675 — checked method has unsupported receiver type
// ---------------------------------------------------------------------------

/// Perspective 13: a method call on `i64`, which has no method table.
#[test]
fn method_on_integer_never_reaches_codegen() {
    assert_rejected_before_codegen(
        r#"
fn main() {
    let x = 10;
    println(x.increment());
}
"#,
    );
}

/// Perspective 14: a method call on `bool`.
#[test]
fn method_on_bool_never_reaches_codegen() {
    assert_rejected_before_codegen(
        r#"
fn main() {
    let flag = true;
    println(flag.negate());
}
"#,
    );
}

// ---------------------------------------------------------------------------
// emit_object.rs:24 / :401 — checked field has no object-layout slot
// ---------------------------------------------------------------------------

/// Perspective 15: reading a field the class never declared.
#[test]
fn unknown_field_read_never_reaches_codegen() {
    assert_rejected_before_codegen(
        r#"
class Point {
    pub x: i64;
}

fn main() {
    let p = new Point(1);
    println(p.y);
}
"#,
    );
}

/// Perspective 16: writing a field the class never declared. The store path has
/// its own layout lookup, separate from the load path.
#[test]
fn unknown_field_write_never_reaches_codegen() {
    assert_rejected_before_codegen(
        r#"
class Point {
    pub x: i64;
}

fn main() {
    let p = new Point(1);
    p.y = 5;
    println(p.x);
}
"#,
    );
}

/// Perspective 17: reading a private field from outside the class. The slot
/// exists, so only the front end can stop this.
#[test]
fn private_field_from_outside_never_reaches_codegen() {
    assert_rejected_before_codegen(
        r#"
class Point {
    x: i64;
}

fn main() {
    let p = new Point(1);
    println(p.x);
}
"#,
    );
}

// ---------------------------------------------------------------------------
// emit_object.rs:145 / :156 — object literal class has no layout or type id
// ---------------------------------------------------------------------------

/// Perspective 18: constructing a class with the wrong number of fields. The
/// backend walks the layout and the initializer list in lockstep.
#[test]
fn object_literal_wrong_field_count_never_reaches_codegen() {
    assert_rejected_before_codegen(
        r#"
class Point {
    pub x: i64;
    pub y: i64;
}

fn main() {
    let p = new Point(1);
    println(p.x);
}
"#,
    );
}

/// Perspective 19: constructing a class with a field of the wrong type.
#[test]
fn object_literal_wrong_field_type_never_reaches_codegen() {
    assert_rejected_before_codegen(
        r#"
class Point {
    pub x: i64;
}

fn main() {
    let p = new Point("one");
    println(p.x);
}
"#,
    );
}

// ---------------------------------------------------------------------------
// emit_object.rs:47 — checked format call has no literal format string
// ---------------------------------------------------------------------------

/// Perspective 20: `format` with a runtime string instead of a literal. The
/// backend expands placeholders at compile time and has nothing to expand here.
#[test]
fn format_with_non_literal_pattern_never_reaches_codegen() {
    assert_rejected_before_codegen(
        r#"
fn main() {
    let pattern = "value {}";
    println(format(pattern, 1));
}
"#,
    );
}

/// Perspective 21: `format` with more placeholders than arguments.
#[test]
fn format_placeholder_arity_mismatch_never_reaches_codegen() {
    assert_rejected_before_codegen(
        r#"
fn main() {
    println(format("{} and {}", 1));
}
"#,
    );
}

// ---------------------------------------------------------------------------
// emit_option_result.rs:442 — indirect call target is not a function
// ---------------------------------------------------------------------------

/// Perspective 22: `Option::map` handed a plain integer. The backend builds a
/// signature from the argument's `Type::Fn`, which this value does not have.
#[test]
fn option_map_non_function_never_reaches_codegen() {
    assert_rejected_before_codegen(
        r#"
fn main() {
    let v = Option::Some(7).map(3);
    println(v.unwrap_or(0));
}
"#,
    );
}

/// Perspective 23: `Option::and_then` handed a string.
#[test]
fn option_and_then_non_function_never_reaches_codegen() {
    assert_rejected_before_codegen(
        r#"
fn main() {
    let v = Option::Some(7).and_then("nope");
    println(v.unwrap_or(0));
}
"#,
    );
}

/// Perspective 24: `Option::map` with a closure of the wrong arity. A one-slot
/// payload cannot fill a two-parameter signature.
#[test]
fn option_map_wrong_closure_arity_never_reaches_codegen() {
    assert_rejected_before_codegen(
        r#"
fn main() {
    let v = Option::Some(7).map(|a: i64, b: i64| a + b);
    println(v.unwrap_or(0));
}
"#,
    );
}

/// Perspective 25: `Result::map` handed a non-function, covering the `Result`
/// combinators as well as the `Option` ones.
#[test]
fn result_map_non_function_never_reaches_codegen() {
    assert_rejected_before_codegen(
        r#"
fn main() {
    let r: Result<i64, String> = Result::Ok(7);
    let v = r.map(42);
    println(v.unwrap_or(0));
}
"#,
    );
}

/// Perspective 26: a closure whose parameter type does not match the payload.
/// The signature would be built from a type the payload cannot satisfy.
#[test]
fn option_map_closure_param_type_mismatch_never_reaches_codegen() {
    assert_rejected_before_codegen(
        r#"
fn main() {
    let v = Option::Some(7).map(|s: String| s);
    println(v.unwrap_or("none"));
}
"#,
    );
}

// ---------------------------------------------------------------------------
// Positive controls: the valid forms still compile and run.
// ---------------------------------------------------------------------------

/// Perspective 27: every construct guarded above, in its legal form, in one
/// program. Proves the invariant panics are unreachable in working code rather
/// than merely rare.
#[test]
fn valid_forms_of_every_guarded_construct_still_run() {
    let (out, ok) = compile_and_run(
        r#"
import std::collections::Map;

interface Named {
    fn name(self) -> String;
}

class Person implements Named {
    pub label: String;

    pub fn name(self) -> String {
        return self.label;
    }
}

fn describe(n: Named) -> String {
    return n.name();
}

fn main() {
    let m: Map<String, i64> = Map::new();
    m.insert("k", 1);
    println(m.len());

    let p = new Person("Alice");
    println(describe(p));
    p.label = "Bob";
    println(p.label);

    println(format("{} and {}", 1, 2));

    println(Option::Some(7).map(|x: i64| x * 2).unwrap_or(0));

    let r: Result<i64, String> = Result::Ok(7);
    println(r.map(|x: i64| x + 1).unwrap_or(0));
}
"#,
    );
    assert!(ok, "program failed to run: {out}");
    assert!(
        !out.contains("compiler invariant violated"),
        "valid program tripped an invariant panic: {out}"
    );
    assert_eq!(out.trim(), "1\nAlice\nBob\n1 and 2\n14\n8");
}

/// Perspective 28: the same guarded constructs under a release build, where
/// the effect and dispatch optimizations that consume this metadata are on.
#[test]
fn valid_forms_still_run_in_release() {
    let (out, ok) = super::support::compile_and_run_release(
        r#"
class Counter {
    pub value: i64;

    pub fn bump(self) -> i64 {
        self.value = self.value + 1;
        return self.value;
    }
}

fn main() {
    let c = new Counter(0);
    println(c.bump());
    println(c.bump());
    println(format("total {}", c.value));
    println(Option::Some(3).and_then(|x: i64| Option::Some(x * 3)).unwrap_or(0));
}
"#,
    );
    assert!(ok, "release program failed to run: {out}");
    assert_eq!(out.trim(), "1\n2\ntotal 2\n9");
}

// ---------------------------------------------------------------------------
// The non-Cranelift sweep: lir_gen.rs and the downcast pattern
// ---------------------------------------------------------------------------
//
// The AST emitter panics when a checked class has no runtime type id, but the
// LIR generator used to substitute 0 — and 0 is not a class, since ids start at
// 1. A bogus id lands in object word 0, where nothing reads it until an `is`
// test or a downcast pattern reads it and answers wrong. The downcast lowering
// had the mirror-image hole: an unknown class returned "never matches", which
// silently drops a live arm and sends the program down the wildcard instead.
// All three now panic. These tests pin down that the front end gets there
// first.

/// Perspective 29: an unknown class in a downcast pattern is rejected with
/// E0350 before the backend can look up a type id that does not exist.
#[test]
fn unknown_downcast_class_never_reaches_codegen() {
    let stderr = compile_error_stderr(
        r#"
interface Shape {
    fn area(self) -> i64;
}

class Square implements Shape {
    pub side: i64;

    pub fn area(self) -> i64 {
        return self.side * self.side;
    }
}

fn describe(s: Shape) -> i64 {
    match s {
        Circle(c) => 0,
        _ => 1,
    }
}

fn main() {
    println(describe(new Square(2)));
}
"#,
    );
    assert!(
        !stderr.contains("compiler invariant violated"),
        "reached the backend instead of the checker:\n{stderr}"
    );
    assert!(
        stderr.contains("error[E0350]") && stderr.contains("Circle"),
        "expected an unknown-class diagnostic, got:\n{stderr}"
    );
}

/// Perspective 30: a downcast pattern on a non-interface scrutinee. There is no
/// interface box to read a type id out of, and E1205 says so.
#[test]
fn downcast_on_non_interface_scrutinee_never_reaches_codegen() {
    assert_rejected_before_codegen(
        r#"
class Square {
    pub side: i64;
}

fn main() {
    let s = new Square(2);
    match s {
        Square(sq) => println(1),
        _ => println(0),
    }
}
"#,
    );
}

/// Perspective 31: a class that does not implement the scrutinee's interface.
/// The arm could never match at run time, so the checker rejects it rather than
/// letting the backend emit a comparison against an unrelated type id.
#[test]
fn downcast_to_unrelated_class_never_reaches_codegen() {
    assert_rejected_before_codegen(
        r#"
interface Shape {
    fn area(self) -> i64;
}

class Square implements Shape {
    pub side: i64;

    pub fn area(self) -> i64 {
        return self.side * self.side;
    }
}

class Bag {
    pub count: i64;
}

fn describe(s: Shape) -> i64 {
    match s {
        Bag(b) => 0,
        _ => 1,
    }
}

fn main() {
    println(describe(new Square(2)));
}
"#,
    );
}

/// Perspective 32: construction through the LIR generator, which is the emitter
/// that used to stamp type id 0. (The object-literal form of the same bug is
/// now unreachable from source at all: named-field construction is rejected
/// with E0847.)
#[test]
fn lir_path_constructs_objects_without_tripping_an_invariant() {
    let (out, ok) = super::support::compile_with_env_and_run(
        r#"
interface Shape {
    fn area(self) -> i64;
}

class Square implements Shape {
    pub side: i64;

    pub fn area(self) -> i64 {
        return self.side * self.side;
    }
}

class Circle implements Shape {
    pub r: i64;

    pub fn area(self) -> i64 {
        return 3 * self.r * self.r;
    }
}

fn main() {
    let sq = new Square(4);
    let ci = new Circle(2);
    println(sq.area());
    println(ci.area());
}
"#,
        &[],
    );
    assert!(ok, "the program failed to run: {out}");
    assert!(
        !out.contains("compiler invariant violated"),
        "valid program tripped an invariant panic: {out}"
    );
    assert_eq!(out.trim(), "16\n12");
}

/// Perspective 33: the type ids the emitter stamps at construction are the ones
/// the downcast lowering reads back. `main` builds the objects and `which`
/// matches on them, so an id of 0 — what the old `unwrap_or(0)` produced —
/// makes both arms miss and prints -1 twice.
#[test]
fn constructed_type_ids_are_the_ones_a_downcast_reads_back() {
    let source = r#"
interface Shape {
    fn area(self) -> i64;
}

class Square implements Shape {
    pub side: i64;

    pub fn area(self) -> i64 {
        return self.side * self.side;
    }
}

class Circle implements Shape {
    pub r: i64;

    pub fn area(self) -> i64 {
        return 3 * self.r * self.r;
    }
}

fn which(s: Shape) -> i64 {
    return match s {
        Square(sq) => sq.side,
        Circle(c) => c.r * 100,
        _ => -1,
    };
}

fn main() {
    println(which(new Square(4)));
    println(which(new Circle(2)));
}
"#;

    let (out, ok) = compile_and_run(source);
    assert!(ok, "program failed to run: {out}");
    assert_eq!(out.trim(), "4\n200");
}
