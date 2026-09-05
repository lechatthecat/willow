//! The last fallback categories the LIR walker turned down, closed for the
//! Stage 5 cutover (willow-0g8j.3).
//!
//! Every one of these programs was correct already — the AST emitter compiled
//! it — so nothing here is a bug fix in the usual sense. What each perspective
//! pins down is WHICH emitter runs, because the cutover deletes the AST one:
//! a category still falling back at that point stops compiling altogether.
//!
//! Five categories, each with its own reason for having been out:
//!
//!   * `PanicInfo` fields. `PanicInfo` is the one nominal type no source file
//!     declares. The checker defines it and the back end lays it out, but HIR
//!     lowering knew nothing about it, so `info.line` found no field on its
//!     receiver and lowering produced no HIR for the whole function, which the
//!     compile path could only report as "it has no lowered IR".
//!   * A `let` in a `defer` body. Admitted now that
//!     `emit_deferred_action` brackets each replay's `vars` and GC root depth.
//!   * `Result::and_then` on a receiver with no error type. `Result::Ok(5)`
//!     written with nothing to infer `E` from records `Result<i64, void>`, and
//!     eligibility demanded a non-void `E` the emitter never reads.
//!   * A `match` on a GENERIC interface instantiation. A downcast arm compares
//!     word 0's `type_id` and never reads the type arguments, so `Box<i64>` is
//!     the same box `Shape` is.
//!   * An interface a module IMPORTED. The interface table is one flat
//!     build-wide namespace keyed by the canonical name, so a module body that
//!     named `Describable` found nothing and both back ends read the value as a
//!     class — a silent wrong dispatch with one implementation, and a compiler
//!     abort with two. This is the one category that was also an AST-path
//!     MISCOMPILE, so its perspectives check the answer, not just the emitter.
//!
//! The evidence is the build itself: with the fallback gone, a category still
//! outside the walker's subset does not compile, so a runtime perspective that
//! runs at all has already proved the walker took the body — and the value it
//! prints is what proves the walker got it right.
//!
//! 27 perspectives:
//!   1 `info.line` through the walker      14 `and_then` twice in a chain
//!   2 every `PanicInfo` field at once     15 a String ok payload
//!   3 the String field under GC stress    16 a declared `E` still works
//!   4 a field into a `&i64` parameter     17 `or_else` still needs its `E`
//!   5 the field feeds a `match` arm       18 downcast on `Box<i64>`
//!   6 `recover()` then a field read       19 the untaken arm's wildcard
//!   7 a field read inside the `defer`     20 a generic interface, String arg
//!   8 `let` in a `defer` body             21 a binding pattern on `Box<i64>`
//!   9 a GC-managed `let` under stress     22 an imported interface dispatches
//!  10 two defers, two bindings            23 two implementations, no abort
//!  11 a `let` in a defer inside a loop    24 the module calls it itself
//!  12 a defer `let` on the panic path     25 the example program
//!  13 `and_then` with no error type       26 a divergent deferred expression
//!                                         27 range/array loops in a defer

use super::support::*;

/// No extra compiler environment: the ordinary build.
const PLAIN: [(&str, &str); 0] = [];
const LIR_LOG: &[(&str, &str)] = &[("WILLOW_LIR_LOG", "1")];
const ALLOC_STRESS: &[(&str, &str)] = &[("WILLOW_GC_STRESS", "alloc")];
const MINOR_STRESS: &[(&str, &str)] = &[("WILLOW_GC_STRESS", "minor")];

/// The program builds and prints `expected`. A body outside the walker's subset
/// no longer compiles at all, so a passing run is the proof the walker took it.
#[track_caller]
fn assert_project_output(source: &str, expected: &str) {
    let (out, ok) = compile_with_env_and_run(source, &PLAIN[..]);
    assert!(ok, "the walker must claim every function here: {out}");
    assert_eq!(out, expected, "wrong output");
}

/// Each named function appears in the walker's selection log, so a perspective
/// cannot pass by having its subject inlined away or never compiled.
#[track_caller]
fn assert_logged(source: &str, functions: &[&str]) {
    let (ok, stderr) = compile_with_compiler_env(source, LIR_LOG);
    assert!(ok, "the program must compile: {stderr}");
    for name in functions {
        let sync = format!("[lir] compiling `{name}` from lowered IR");
        let coop = format!("[lir] compiling async `{name}` from lowered IR");
        assert!(
            stderr.contains(&sync) || stderr.contains(&coop),
            "`{name}` was not walker-compiled:\n{stderr}"
        );
    }
}

/// The same program under both collector stress modes, on the walker.
#[track_caller]
fn assert_under_stress(source: &str, expected: &str) {
    for stress in [ALLOC_STRESS, MINOR_STRESS] {
        let (out, ok) = compile_with_env_and_run_under(source, &PLAIN[..], stress);
        assert!(ok, "stressed run failed under {stress:?}: {out}");
        assert_eq!(out, expected, "wrong output under {stress:?}");
    }
}

// ── `PanicInfo` fields in lowered IR ────────────────────────────────────────

// 1. The core shape. `info.line` is an ordinary field read of an ordinary
// object; only the class it reads from is one no source file declares.
#[test]
fn cutover_01_panic_info_line_walks() {
    let src = r#"
fn line_of(info: PanicInfo) -> i64 { return info.line; }

fn main() {
    defer match recover() {
        Some(info) => println(line_of(info)),
        None => {}
    }
    panic("boom");
}
"#;
    assert_project_output(src, "9\n");
    assert_logged(src, &["line_of", "main"]);
}

// 2. All four fields, so the layout the walker reads is the layout the runtime
// wrote — a swapped pair prints the wrong two values rather than failing.
#[test]
fn cutover_02_every_panic_info_field() {
    let src = r#"
fn describe(info: PanicInfo) {
    println(info.message);
    println(info.line);
    println(info.column);
}

fn main() {
    defer match recover() {
        Some(info) => describe(info),
        None => {}
    }
    panic("detail");
}
"#;
    assert_project_output(src, "detail\n13\n5\n");
    assert_logged(src, &["describe", "main"]);
}

// 3. `message` is a `String` — a GC-managed field, read while the collector is
// under stress, so a reference the walker failed to keep alive would be
// collected rather than merely at risk.
#[test]
fn cutover_03_panic_info_string_field_under_stress() {
    let src = r#"
import std::collections::Array;

fn shout(info: PanicInfo) -> String {
    let filler: Array<String> = [];
    filler.push("a");
    filler.push("b");
    return info.message;
}

fn main() {
    defer match recover() {
        Some(info) => println(shout(info)),
        None => {}
    }
    panic("loud");
}
"#;
    assert_project_output(src, "loud\n");
    assert_under_stress(src, "loud\n");
}

// 4. The bead's own repro: a field taken by reference into a `&i64` parameter.
// The reference ARGUMENT already lowered; the receiver's field did not, and the
// gap took the enclosing function with it.
#[test]
fn cutover_04_panic_info_field_into_a_reference_param() {
    let src = r#"
fn peek(value: &i64) { println(value); }

fn show(info: PanicInfo) { peek(&info.line); }

fn main() {
    defer match recover() {
        Some(info) => show(info),
        None => {}
    }
    panic("boom");
}
"#;
    assert_project_output(src, "11\n");
    assert_logged(src, &["peek", "show", "main"]);
}

// 5. The field feeds a `match` arm rather than a call, so the value crosses an
// HIR island boundary on its way out.
#[test]
fn cutover_05_panic_info_field_in_a_match_arm() {
    let src = r#"
fn kind(info: PanicInfo) -> String {
    return match info.column {
        0 => "start",
        _ => "inside",
    };
}

fn main() {
    defer match recover() {
        Some(info) => println(kind(info)),
        None => {}
    }
    panic("k");
}
"#;
    assert_project_output(src, "inside\n");
    assert_logged(src, &["kind", "main"]);
}

// 6. `recover()` hands back an `Option<PanicInfo>`; unwrapping it and reading a
// field is the shape a handler that does not want a `match` writes.
#[test]
fn cutover_06_recover_unwrapped_then_read() {
    let src = r#"
fn main() {
    defer {
        let caught = recover();
        if caught.is_some() {
            let info = caught.unwrap();
            println(info.message);
        }
    }
    panic("unwrapped");
}
"#;
    assert_project_output(src, "unwrapped\n");
    assert_logged(src, &["main"]);
}

// 7. The read happens INSIDE the deferred body, which the unwinder replays —
// so the field access is emitted in the island, not in the function around it.
#[test]
fn cutover_07_panic_info_field_read_inside_the_defer() {
    let src = r#"
fn main() {
    defer match recover() {
        Some(info) => println(info.message),
        None => println("none"),
    }
    panic("inside");
}
"#;
    assert_project_output(src, "inside\n");
    assert_logged(src, &["main"]);
}

// ── `let` in a deferred body ────────────────────────────────────────────────

// 8. The plain case. `emit_deferred_action` snapshots `vars` and the GC root
// depth around each replay, which is what makes the binding safe to declare.
#[test]
fn cutover_08_let_in_a_defer_body() {
    let src = r#"
fn serve() -> String {
    let mut status = "ok";
    if true {
        defer {
            let note = "cleaned";
            println(note);
        }
        status = "ran";
    }
    return status;
}

fn main() { println(serve()); }
"#;
    assert_project_output(src, "cleaned\nran\n");
    assert_logged(src, &["serve", "main"]);
}

// 9. A GC-managed binding in the deferred body, under stress: a slot the
// bracket failed to pop would leave the shadow stack deeper than the code after
// the flush was emitted for.
#[test]
fn cutover_09_gc_let_in_a_defer_under_stress() {
    let src = r#"
import std::collections::Array;

fn build(n: i64) -> i64 {
    defer {
        let scratch: Array<i64> = [];
        scratch.push(n);
        scratch.push(n * 2);
        println(scratch[0] + scratch[1]);
    }
    return n;
}

fn main() { println(build(4)); }
"#;
    assert_project_output(src, "12\n4\n");
    assert_under_stress(src, "12\n4\n");
}

// 10. Two deferred bodies, each with a binding of its own, run last-in-first-out.
// Each replay gets its own bracket, so the second body's slot cannot outlive it.
#[test]
fn cutover_10_two_defers_two_bindings() {
    let src = r#"
fn ordered() -> i64 {
    defer {
        let first = "outer";
        println(first);
    }
    defer {
        let second = "inner";
        println(second);
    }
    return 1;
}

fn main() { println(ordered()); }
"#;
    assert_project_output(src, "inner\nouter\n1\n");
    assert_logged(src, &["ordered", "main"]);
}

// 11. A `defer` inside a loop body registers once per iteration and flushes at
// the end of each. Its body observes `i` after the increment, so the same
// binding is declared and popped three times with values 10, 20 and 30.
#[test]
fn cutover_11_let_in_a_defer_inside_a_loop() {
    let src = r#"
fn counted() -> i64 {
    let mut total = 0;
    let mut i = 0;
    while i < 3 {
        defer {
            let step = i * 10;
            println(step);
        }
        total = total + i;
        i = i + 1;
    }
    return total;
}

fn main() { println(counted()); }
"#;
    assert_project_output(src, "10\n20\n30\n3\n");
    assert_logged(src, &["counted", "main"]);
}

// 12. The other exit: the body is replayed on the PANIC path, where the pop is
// deliberately skipped because the trap does not unwind this stack. The
// recovered program must still run on.
#[test]
fn cutover_12_defer_let_on_the_panic_path() {
    let src = r#"
fn risky() {
    defer {
        let tag = "cleanup";
        println(tag);
    }
    panic("fail");
}

fn main() {
    defer match recover() {
        Some(info) => println(info.message),
        None => {}
    }
    risky();
}
"#;
    assert_project_output(src, "cleanup\nfail\n");
    assert_logged(src, &["risky", "main"]);
}

// ── `Result::and_then` with no error type on the receiver ───────────────────

// 13. `Result::Ok(10)` has nothing to infer `E` from, so the checker records
// `Result<i64, void>`. The emitter reads only the ok type, and the `Err` arm
// passes the receiver's own box through unchanged.
#[test]
fn cutover_13_and_then_on_a_void_error_receiver() {
    let src = r#"
fn add_five(v: i64) -> Result<i64, String> {
    return Result::Ok(v + 5);
}

fn main() {
    let chained = Result::Ok(10).and_then(add_five);
    println(chained.unwrap());
}
"#;
    assert_project_output(src, "15\n");
    assert_logged(src, &["main"]);
}

// 14. Twice in one chain, and down the `Err` arm as well, so the pass-through
// really is the receiver's box and not a rebuilt one.
#[test]
fn cutover_14_and_then_chained_both_ways() {
    let src = r#"
fn parse_positive(n: i64) -> Result<i64, String> {
    if n > 0 {
        return Result::Ok(n);
    }
    return Result::Err("not positive");
}

fn main() {
    let a = Result::Ok(5).and_then(parse_positive).and_then(parse_positive);
    let b = Result::Ok(0 - 3).and_then(parse_positive);
    println(a.unwrap());
    println(b.unwrap_err());
}
"#;
    assert_project_output(src, "5\nnot positive\n");
    assert_logged(src, &["parse_positive", "main"]);
}

// 15. A GC-managed ok payload through the same chain, so the value the
// combinator moves into the new box is one the collector tracks.
#[test]
fn cutover_15_and_then_with_a_string_payload() {
    let src = r#"
fn shout(s: String) -> Result<String, String> {
    return Result::Ok(s + "!");
}

fn main() {
    let r = Result::Ok("hi").and_then(shout);
    println(r.unwrap());
}
"#;
    assert_project_output(src, "hi!\n");
    assert_under_stress(src, "hi!\n");
}

// 16. The receiver that always worked still does: a declared error type is not
// what the rule turned on, so widening it must not have changed this case.
#[test]
fn cutover_16_and_then_with_a_declared_error_type() {
    let src = r#"
fn half(n: i64) -> Result<i64, String> {
    if n % 2 == 0 {
        return Result::Ok(n / 2);
    }
    return Result::Err("odd");
}

fn source(ok: bool) -> Result<i64, String> {
    if ok {
        return Result::Ok(8);
    }
    return Result::Err("no source");
}

fn main() {
    println(source(true).and_then(half).unwrap());
    println(source(false).and_then(half).unwrap_err());
}
"#;
    assert_project_output(src, "4\nno source\n");
    assert_logged(src, &["half", "source", "main"]);
}

// 17. `or_else` is the mirror image and its `E` IS read — the callable takes
// it — so it keeps the requirement `and_then` gave up.
#[test]
fn cutover_17_or_else_still_takes_the_error() {
    let src = r#"
fn recover_from(e: String) -> Result<i64, String> {
    println(e);
    return Result::Ok(0);
}

fn failing() -> Result<i64, String> {
    return Result::Err("gone");
}

fn main() {
    println(failing().or_else(recover_from).unwrap());
}
"#;
    assert_project_output(src, "gone\n0\n");
    assert_logged(src, &["recover_from", "failing", "main"]);
}

// ── `match` on a generic interface instantiation ────────────────────────────

// 18. The downcast arm compares word 0's runtime `type_id`; the interface's
// type arguments say what its methods produce and are not part of that test.
#[test]
fn cutover_18_downcast_on_a_generic_interface() {
    let src = r#"
interface Box<T> { fn get(self) -> T; }

class IntBox implements Box<i64> {
    pub fn get(self) -> i64 { return 7; }
    pub fn extra(self) -> i64 { return 99; }
}

class OtherBox implements Box<i64> {
    pub fn get(self) -> i64 { return 1; }
}

fn probe(b: Box<i64>) -> i64 {
    return match b {
        IntBox(x) => x.extra(),
        _ => b.get(),
    };
}

fn main() {
    println(probe(new IntBox()));
    println(probe(new OtherBox()));
}
"#;
    assert_project_output(src, "99\n1\n");
    assert_logged(src, &["probe", "main"]);
}

// 19. Exactness: a class that implements the same instantiation does NOT match
// another class's arm: matching compares the concrete class identity.
#[test]
fn cutover_19_generic_downcast_is_exact() {
    let src = r#"
interface Holder<T> { fn get(self) -> T; }

class A implements Holder<i64> { pub fn get(self) -> i64 { return 1; } }
class B implements Holder<i64> { pub fn get(self) -> i64 { return 2; } }

fn which(h: Holder<i64>) -> String {
    return match h {
        A(_) => "a",
        B(_) => "b",
        _ => "other",
    };
}

fn main() {
    println(which(new A()));
    println(which(new B()));
}
"#;
    assert_project_output(src, "a\nb\n");
    assert_logged(src, &["which", "main"]);
}

// 20. A GC-managed type argument, matched under stress: the scrutinee box is
// rooted for the whole match, and the unboxed object the arm binds stays
// reachable through it.
#[test]
fn cutover_20_generic_downcast_with_a_string_argument() {
    let src = r#"
import std::collections::Array;

interface Named<T> { fn name(self) -> T; }

class Tag implements Named<String> {
    pub fn name(self) -> String { return "tag"; }
    pub fn loud(self) -> String {
        let filler: Array<String> = [];
        filler.push("x");
        return "TAG";
    }
}

class Plain implements Named<String> {
    pub fn name(self) -> String { return "plain"; }
}

fn label(n: Named<String>) -> String {
    return match n {
        Tag(t) => t.loud(),
        _ => n.name(),
    };
}

fn main() {
    println(label(new Tag()));
    println(label(new Plain()));
}
"#;
    assert_project_output(src, "TAG\nplain\n");
    assert_under_stress(src, "TAG\nplain\n");
}

// 21. A whole-scrutinee binding rather than a downcast, so the arm aliases the
// box itself and calls through the vtable it carries.
#[test]
fn cutover_21_generic_interface_binding_pattern() {
    let src = r#"
interface Cell<T> { fn read(self) -> T; }

class One implements Cell<i64> { pub fn read(self) -> i64 { return 41; } }

fn value(c: Cell<i64>) -> i64 {
    return match c {
        other => other.read() + 1,
    };
}

fn main() { println(value(new One())); }
"#;
    assert_project_output(src, "42\n");
    assert_logged(src, &["value", "main"]);
}

// ── An interface a module imported ─────────────────────────────────────────

/// The interface, in a module of its own, so every user of it below reaches it
/// by a single-item import.
const PROTO: &str = r#"
module proto;

pub interface Describable {
    fn label(self) -> String;
}
"#;

// 22. One implementation. The class is declared in a module that IMPORTED the
// interface, so the `implements` name is not the module's own to qualify. It
// used to be qualified anyway, into a type nothing declares — the vtable was
// silently skipped and the box fell back to the raw object, which dispatched
// straight to the one candidate and printed the right answer by luck.
#[test]
fn cutover_22_an_imported_interface_dispatches() {
    let files = [
        ("proto.wi", PROTO),
        (
            "impls.wi",
            r#"
module impls;

import proto::Describable;

pub class Item implements Describable {
    pub fn label(self) -> String { return "item"; }
}

pub fn make() -> Describable { return new Item(); }
"#,
        ),
        (
            "main.wi",
            r#"
import proto::Describable;
import impls;

fn describe(d: Describable) { println(d.label()); }

fn main() { describe(impls::make()); }
"#,
        ),
    ];
    let (lir, ok) = compile_temp_project_with_env_and_run(&files, "main.wi", &PLAIN[..]);
    assert!(ok, "LIR build failed: {lir}");
    assert_eq!(lir, "item\n");
    let (ast, ok) = compile_temp_project_with_env_and_run(&files, "main.wi", &PLAIN[..]);
    assert!(ok, "AST build failed: {ast}");
    assert_eq!(ast, "item\n");
}

// 23. Two implementations of the same imported interface. With no vtable to
// dispatch through, the emitter reached its "no virtual slot but 2 candidate
// implementations" invariant and aborted the compile — so this perspective is
// about the program building at all, and then about each box picking its own.
#[test]
fn cutover_23_two_implementations_of_an_imported_interface() {
    let files = [
        ("proto.wi", PROTO),
        (
            "impls.wi",
            r#"
module impls;

import proto::Describable;

pub class Item implements Describable {
    pub fn label(self) -> String { return "item"; }
}

pub class Crate implements Describable {
    pub fn label(self) -> String { return "crate"; }
}

pub fn item() -> Describable { return new Item(); }
pub fn crated() -> Describable { return new Crate(); }
"#,
        ),
        (
            "main.wi",
            r#"
import proto::Describable;
import impls;

fn describe(d: Describable) { println(d.label()); }

fn main() {
    describe(impls::item());
    describe(impls::crated());
}
"#,
        ),
    ];
    let (lir, ok) = compile_temp_project_with_env_and_run(&files, "main.wi", &PLAIN[..]);
    assert!(ok, "LIR build failed: {lir}");
    assert_eq!(lir, "item\ncrate\n");
    let (ast, ok) = compile_temp_project_with_env_and_run(&files, "main.wi", &PLAIN[..]);
    assert!(ok, "AST build failed: {ast}");
    assert_eq!(ast, "item\ncrate\n");
}

// 24. The dispatch happens INSIDE the module that imported the interface, not
// in the entry file, so the alias has to be installed for that unit's own
// bodies as well as for its declarations.
#[test]
fn cutover_24_the_module_dispatches_through_its_import() {
    let files = [
        ("proto.wi", PROTO),
        (
            "impls.wi",
            r#"
module impls;

import proto::Describable;

pub class Item implements Describable {
    pub fn label(self) -> String { return "item"; }
}

pub class Crate implements Describable {
    pub fn label(self) -> String { return "crate"; }
}

fn describe(d: Describable) -> String { return d.label(); }

pub fn both() -> String {
    return describe(new Item()) + "," + describe(new Crate());
}
"#,
        ),
        (
            "main.wi",
            r#"
import impls;

fn main() { println(impls::both()); }
"#,
        ),
    ];
    let (lir, ok) = compile_temp_project_with_env_and_run(&files, "main.wi", &PLAIN[..]);
    assert!(ok, "LIR build failed: {lir}");
    assert_eq!(lir, "item,crate\n");
    let (ast, ok) = compile_temp_project_with_env_and_run(&files, "main.wi", &PLAIN[..]);
    assert!(ok, "AST build failed: {ast}");
    assert_eq!(ast, "item,crate\n");
}

// 25. The example program compiles entirely through lowered IR.
#[test]
fn cutover_25_the_example_has_no_fallback() {
    let src = std::fs::read_to_string("example/lir_stage5_cutover.wi")
        .expect("example/lir_stage5_cutover.wi must exist");
    assert_project_output(
        &src,
        "42\nboom at line 33\ncleaned up 8\n15\nnot positive\n99\n1\n",
    );
}

// 26. A deferred expression may itself diverge. It is replayed in a position
// where nothing follows it, so the same panic rule as a divergent arm applies.
#[test]
fn cutover_26_deferred_panic_expression() {
    let src = r#"
fn main() {
    defer match recover() {
        Some(info) => println(info.message),
        None => {}
    }
    if true {
        defer panic("cleanup panic");
    }
}
"#;
    assert_project_output(src, "cleanup panic\n");
    assert_logged(src, &["main"]);
}

// 27. Deferred blocks retain HIR statements, so their `for` loops need the
// same one-evaluation and per-iteration root discipline as graph-lowered loops.
#[test]
fn cutover_27_range_and_array_for_in_a_defer() {
    let src = r#"
import std::collections::Array;

fn main() {
    let values: Array<String> = ["a", "b"];
    defer {
        let mut joined = "";
        for value in values { joined = joined + value; }
        for i in 0..3 { joined = joined + i.toString(); }
        println(joined);
    }
}
"#;
    assert_project_output(src, "ab012\n");
    assert_under_stress(src, "ab012\n");
    assert_logged(src, &["main"]);
}
