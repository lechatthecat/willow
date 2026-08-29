//! Enum variant payloads must be rooted across their own allocation
//! (willow-vz5f).
//!
//! Building an enum value with a payload is two allocations in sequence: the
//! payload, then the `[tag | payload…]` GC object that will hold it. Between
//! the two the payload is LIVE — it is about to be stored — but it is named by
//! nothing except an SSA register, so it is not in the GC root graph. Any
//! collection triggered by the second allocation is free to reclaim it, and the
//! store then writes a dangling pointer into a fresh object. The failure is
//! silent: the enum reads back whatever the allocator put in that memory next.
//!
//! Two shapes reach it, and both backends share the emitters:
//!
//! * `emit_enum_variant_alloc` / `emit_lir_enum_construction` build the object
//!   FIRST and evaluate the arguments into it, so what has to survive is the
//!   half-built enum — rooted whenever there is any argument to evaluate. The
//!   payload area is `alloc_zeroed`, so tracing it before every slot is stored
//!   is safe: an unwritten ref slot reads as null and is skipped.
//! * `emit_alloc_enum_variant` and `emit_alloc_enum_variant_raw` take a payload
//!   that already exists — the `Option`/`Result` combinators rewrapping a value
//!   they just computed — so what has to survive is the payload. Only reference
//!   payloads are rooted; rooting a scalar word would have the GC follow it as
//!   a bogus object pointer.
//!
//! Every test runs the same program three ways — plain, AST emitter under
//! `WILLOW_GC_STRESS=alloc`, LIR walker under the same — and requires identical
//! output. Each loops a few hundred times so a collection lands inside the
//! window rather than only near it. Removing either rooting makes 17 of the 20
//! programs below crash outright or print garbage; the three that survive are
//! the ones whose payload never needed a root, and they are kept because that
//! is worth pinning too.
//!
//! 20 perspectives, plus a check that the walker really compiled them:
//!   1 `map_err` rewraps a produced String  11 two String payloads
//!   2 `map` rewraps a produced String      12 an `Array` payload
//!   3 `map` passes the Err String through  13 a class payload
//!   4 `map_err` passes the Ok through      14 an interface-typed payload
//!   5 `Option<String>::map`, niche repr    15 a nested `Result` payload
//!   6 `Option::and_then`                   16 a payload only in a register
//!   7 `Result::and_then`                   17 a payload built by two allocs
//!   8 `?` builds an Err String             18 `Result<void, E>`'s empty Ok
//!   9 one String payload                   19 async construction
//!  10 a String beside a scalar             20 payload and payload-free variants

use super::support::{compile_and_run_with_env, compile_with_compiler_env};

const PLAIN: [(&str, &str); 1] = [("WILLOW_LIR_BACKEND", "0")];
const AST_STRESS: [(&str, &str); 2] = [("WILLOW_LIR_BACKEND", "0"), ("WILLOW_GC_STRESS", "alloc")];
const LIR_STRESS: [(&str, &str); 2] = [("WILLOW_LIR_BACKEND", "1"), ("WILLOW_GC_STRESS", "alloc")];

/// Run `source` plain and under allocation stress on both emitters, and require
/// all three to print `expected`. Stress collects on every allocation, so an
/// unrooted payload is reclaimed the moment the enum object is allocated.
fn assert_rooted(source: &str, expected: &str) {
    for env in [&PLAIN[..], &AST_STRESS[..], &LIR_STRESS[..]] {
        let (out, ok) = compile_and_run_with_env(source, env);
        assert!(ok, "program failed under {env:?}: {out}");
        assert_eq!(out, expected, "wrong output under {env:?}");
    }
}

/// Require the walker to have taken each named function, so the LIR half of the
/// differential is not quietly the AST emitter again.
fn assert_walker_compiled(source: &str, functions: &[&str]) {
    let (ok, stderr) = compile_with_compiler_env(
        source,
        &[("WILLOW_LIR_BACKEND", "1"), ("WILLOW_LIR_LOG", "1")],
    );
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

// 1. The reported repro. `map_err` calls the function, gets a fresh String back,
//    then allocates the `Err` box to rewrap it — `emit_alloc_enum_variant` with
//    tag 1 and a GC payload. Unrooted, this prints a fragment of whatever the
//    allocator handed out next.
#[test]
fn gc_enum_payload_01_map_err_rewraps_a_produced_string() {
    assert_rooted(
        "fn bang(e: String) -> String { return \"!\" + e; }
fn main() {
    let mut i = 0;
    let mut last = \"\";
    while i < 300 {
        let b: Result<i64, String> = Err(\"bad\" + i.toString());
        last = match b.map_err(bang) { Ok(v) => v.toString(), Err(e) => e };
        i = i + 1;
    }
    println(last);
}
",
        "!bad299\n",
    );
}

// 2. The same site with tag 0: `map` on an `Ok` rewraps the mapped value.
#[test]
fn gc_enum_payload_02_map_rewraps_a_produced_string() {
    assert_rooted(
        "fn tag(n: i64) -> String { return \"v\" + n.toString(); }
fn main() {
    let mut i = 0;
    let mut last = \"\";
    while i < 300 {
        let a: Result<i64, String> = Ok(i);
        last = match a.map(tag) { Ok(s) => s, Err(e) => e };
        i = i + 1;
    }
    println(last);
}
",
        "v299\n",
    );
}

// 3. `emit_alloc_enum_variant_raw` instead: `map` on an `Err` does not call the
//    function, it re-boxes the existing error word. That word is a String here,
//    so the raw path needs the same root.
#[test]
fn gc_enum_payload_03_map_passes_the_err_string_through() {
    assert_rooted(
        "fn tag(n: i64) -> String { return \"v\" + n.toString(); }
fn main() {
    let mut i = 0;
    let mut last = \"\";
    while i < 300 {
        let c: Result<i64, String> = Err(\"keep\" + i.toString());
        last = match c.map(tag) { Ok(s) => s, Err(e) => e };
        i = i + 1;
    }
    println(last);
}
",
        "keep299\n",
    );
}

// 4. The mirror of 3: `map_err` re-boxes an untouched `Ok`, whose payload is the
//    GC-managed side this time.
#[test]
fn gc_enum_payload_04_map_err_passes_the_ok_through() {
    assert_rooted(
        "fn code(e: i64) -> i64 { return e + 1; }
fn main() {
    let mut i = 0;
    let mut last = \"\";
    while i < 300 {
        let d: Result<String, i64> = Ok(\"held\" + i.toString());
        last = match d.map_err(code) { Ok(s) => s, Err(e) => e.toString() };
        i = i + 1;
    }
    println(last);
}
",
        "held299\n",
    );
}

// 5. `Option<String>` uses the pointer niche, so `Some` IS the payload and
//    nothing is allocated to wrap it. Nothing to root, and nothing to get
//    wrong — pinned so a later representation change cannot quietly start
//    allocating here without this file noticing.
#[test]
fn gc_enum_payload_05_option_map_over_the_pointer_niche() {
    assert_rooted(
        "fn upper(s: String) -> String { return s + \"?\"; }
fn main() {
    let mut i = 0;
    let mut last = \"\";
    while i < 300 {
        let o: Option<String> = Some(\"opt\" + i.toString());
        last = match o.map(upper) { Some(s) => s, None => \"none\" };
        i = i + 1;
    }
    println(last);
}
",
        "opt299?\n",
    );
}

// 6. `and_then` on an `Option`: the function returns the wrapped value itself,
//    so the combinator forwards rather than rewraps.
#[test]
fn gc_enum_payload_06_option_and_then() {
    assert_rooted(
        "fn tag(n: i64) -> Option<String> { return Some(\"v\" + n.toString()); }
fn main() {
    let mut i = 0;
    let mut last = \"\";
    while i < 300 {
        let o: Option<i64> = Some(i);
        last = match o.and_then(tag) { Some(s) => s, None => \"none\" };
        i = i + 1;
    }
    println(last);
}
",
        "v299\n",
    );
}

// 7. `and_then` on a `Result`, where the inner `Ok(...)` is itself a variant
//    allocation over a String the callee just built.
#[test]
fn gc_enum_payload_07_result_and_then() {
    assert_rooted(
        "fn tag(n: i64) -> String { return \"v\" + n.toString(); }
fn wrap(n: i64) -> Result<String, String> { return Ok(tag(n)); }
fn main() {
    let mut i = 0;
    let mut last = \"\";
    while i < 300 {
        let r: Result<i64, String> = Ok(i);
        last = match r.and_then(wrap) { Ok(s) => s, Err(e) => e };
        i = i + 1;
    }
    println(last);
}
",
        "v299\n",
    );
}

// 8. A propagating `?`, which rebuilds the `Err` in the caller's return type
//    around a String the callee allocated.
#[test]
fn gc_enum_payload_08_try_propagation_builds_an_err_string() {
    assert_rooted(
        "fn split(n: i64) -> Result<i64, String> {
    if n < 0 { return Err(\"neg\" + n.toString()); }
    return Ok(n);
}
fn chain(n: i64) -> Result<String, String> {
    let v = split(n)?;
    return Ok(\"ok\" + v.toString());
}
fn main() {
    let mut i = 0;
    let mut good = \"\";
    let mut bad = \"\";
    while i < 300 {
        good = match chain(i) { Ok(s) => s, Err(e) => e };
        bad = match chain(0 - i - 1) { Ok(s) => s, Err(e) => e };
        i = i + 1;
    }
    println(good);
    println(bad);
}
",
        "ok299\nneg-300\n",
    );
}

// 9. A user-declared enum rather than a builtin one, which goes through
//    `emit_enum_variant_alloc` and its root-the-half-built-object strategy.
#[test]
fn gc_enum_payload_09_a_user_enum_with_one_string_payload() {
    assert_rooted(
        "enum Note { Text(String), Code(i64), Empty }
fn build(i: i64) -> Note { return Note::Text(\"n\" + i.toString()); }
fn main() {
    let mut i = 0;
    let mut last = \"\";
    while i < 300 {
        last = match build(i) {
            Note::Text(s) => s,
            Note::Code(c) => c.toString(),
            Note::Empty => \"empty\",
        };
        i = i + 1;
    }
    println(last);
}
",
        "n299\n",
    );
}

// 10. A GC slot beside a scalar one. The variant's `gc_ref_mask` must describe
//     exactly the reference slot, or tracing walks the integer as a pointer.
#[test]
fn gc_enum_payload_10_a_string_beside_a_scalar() {
    assert_rooted(
        "enum Pair { Both(String, i64), None }
fn build(i: i64) -> Pair { return Pair::Both(\"p\" + i.toString(), i); }
fn main() {
    let mut i = 0;
    let mut last = \"\";
    while i < 300 {
        last = match build(i) {
            Pair::Both(s, n) => s + \"/\" + n.toString(),
            Pair::None => \"none\",
        };
        i = i + 1;
    }
    println(last);
}
",
        "p299/299\n",
    );
}

// 11. Two reference slots. The first is already stored when the second's
//     expression allocates, so the enum has to be traceable half-written.
#[test]
fn gc_enum_payload_11_two_string_payloads() {
    assert_rooted(
        "enum Two { Strings(String, String), None }
fn build(i: i64) -> Two { return Two::Strings(\"a\" + i.toString(), \"b\" + i.toString()); }
fn main() {
    let mut i = 0;
    let mut last = \"\";
    while i < 300 {
        last = match build(i) {
            Two::Strings(a, b) => a + b,
            Two::None => \"none\",
        };
        i = i + 1;
    }
    println(last);
}
",
        "a299b299\n",
    );
}

// 12. An `Array<i64>` payload: a GC handle rather than a String, and one whose
//     buffer moves independently of the handle.
#[test]
fn gc_enum_payload_12_an_array_payload() {
    assert_rooted(
        "import std::collections::Array;
enum Holder { Items(Array<i64>), None }
fn build(i: i64) -> Holder {
    let mut a = [i];
    a.push(i + 1);
    return Holder::Items(a);
}
fn main() {
    let mut i = 0;
    let mut last = 0;
    while i < 300 {
        last = match build(i) {
            Holder::Items(a) => a.len() + a[1],
            Holder::None => -1,
        };
        i = i + 1;
    }
    println(last);
}
",
        "302\n",
    );
}

// 13. A class instance payload, allocated by the argument expression itself.
#[test]
fn gc_enum_payload_13_a_class_payload() {
    assert_rooted(
        "class Node { pub v: i64; }
enum Wrap { Held(Node), None }
fn build(i: i64) -> Wrap { return Wrap::Held(new Node(i)); }
fn main() {
    let mut i = 0;
    let mut last = 0;
    while i < 300 {
        last = match build(i) {
            Wrap::Held(n) => n.v,
            Wrap::None => -1,
        };
        i = i + 1;
    }
    println(last);
}
",
        "299\n",
    );
}

// 14. An interface-typed slot. The class value is boxed on the way in, so the
//     payload actually stored is a second allocation made after the enum
//     object already exists.
#[test]
fn gc_enum_payload_14_an_interface_typed_payload() {
    assert_rooted(
        "interface Greeter { fn hello() -> String; }
class Dog implements Greeter { pub fn hello() -> String { return \"woof\"; } }
enum Held { One(Greeter), None }
fn build(i: i64) -> Held { return Held::One(new Dog()); }
fn main() {
    let mut i = 0;
    let mut last = \"\";
    while i < 300 {
        last = match build(i) { Held::One(g) => g.hello(), Held::None => \"none\" };
        i = i + 1;
    }
    println(last);
}
",
        "woof\n",
    );
}

// 15. An enum inside an enum: the payload is itself a `[tag | payload]` object
//     built by the allocation immediately before.
#[test]
fn gc_enum_payload_15_a_nested_result_payload() {
    assert_rooted(
        "enum Outer { Inner(Result<i64, String>), None }
fn build(i: i64) -> Outer {
    if i % 2 == 0 { return Outer::Inner(Ok(i)); }
    return Outer::Inner(Err(\"e\" + i.toString()));
}
fn main() {
    let mut i = 0;
    let mut last = \"\";
    while i < 300 {
        last = match build(i) {
            Outer::Inner(r) => match r { Ok(v) => v.toString(), Err(e) => e },
            Outer::None => \"none\",
        };
        i = i + 1;
    }
    println(last);
}
",
        "e299\n",
    );
}

// 16. The payload is a call result that no binding ever names, which is the
//     case the root has to cover — a named local is already in the frame's
//     root graph.
#[test]
fn gc_enum_payload_16_a_payload_only_in_a_register() {
    assert_rooted(
        "fn make(n: i64) -> String { return \"m\" + n.toString(); }
enum Boxed { Held(String), None }
fn build(i: i64) -> Boxed { return Boxed::Held(make(i)); }
fn main() {
    let mut i = 0;
    let mut last = \"\";
    while i < 300 {
        last = match build(i) { Boxed::Held(s) => s, Boxed::None => \"none\" };
        i = i + 1;
    }
    println(last);
}
",
        "m299\n",
    );
}

// 17. The argument expression allocates twice: the call, then the concatenation
//     of its result. The intermediate must survive the second allocation as
//     well as the enum's own.
#[test]
fn gc_enum_payload_17_a_payload_built_by_two_allocations() {
    assert_rooted(
        "fn make(n: i64) -> String { return \"m\" + n.toString(); }
enum Note { Text(String), None }
fn build(i: i64) -> Note { return Note::Text(make(i) + \"x\"); }
fn main() {
    let mut i = 0;
    let mut last = \"\";
    while i < 300 {
        last = match build(i) { Note::Text(s) => s, Note::None => \"none\" };
        i = i + 1;
    }
    println(last);
}
",
        "m299x\n",
    );
}

// 18. `Result<void, E>::Ok()` carries a substituted `void` payload and no
//     argument at all, so it must root nothing and still describe the same
//     object on both paths.
#[test]
fn gc_enum_payload_18_a_result_with_an_empty_ok() {
    assert_rooted(
        "fn ping(n: i64) -> Result<void, String> {
    if n < 0 { return Err(\"neg\"); }
    return Ok();
}
fn main() {
    let mut i = 0;
    let mut last = \"\";
    while i < 300 {
        last = match ping(i) { Ok(_) => \"fine\", Err(e) => e };
        last = match ping(0 - i - 1) { Ok(_) => \"fine\", Err(e) => e };
        i = i + 1;
    }
    println(last);
}
",
        "neg\n",
    );
}

// 19. Construction inside an async function, where the frame rather than the
//     machine stack holds the live values across a suspension point.
#[test]
fn gc_enum_payload_19_construction_in_an_async_function() {
    assert_rooted(
        "enum Note { Text(String), None }
async fn build(i: i64) -> Note { return Note::Text(\"a\" + i.toString()); }
async fn main() {
    let mut i = 0;
    let mut last = \"\";
    while i < 200 {
        last = match await build(i) { Note::Text(s) => s, Note::None => \"none\" };
        i = i + 1;
    }
    println(last);
}
",
        "a199\n",
    );
}

// 20. One enum with both a payload-carrying and a payload-free variant. The
//     payload-free one still allocates a `[tag]` object, and must not root a
//     payload it does not have.
#[test]
fn gc_enum_payload_20_payload_and_payload_free_variants() {
    assert_rooted(
        "fn label(n: i64) -> String { return \"L\" + n.toString(); }
enum Step { Next(String, i64), Stop }
fn build(i: i64) -> Step {
    if i > 250 { return Step::Stop; }
    return Step::Next(label(i), i);
}
fn main() {
    let mut i = 0;
    let mut last = \"\";
    while i < 300 {
        last = match build(i) { Step::Next(s, n) => s + \"/\" + n.toString(), Step::Stop => \"stop\" };
        i = i + 1;
    }
    println(last);
    println(match build(10) { Step::Next(s, n) => s + \"/\" + n.toString(), Step::Stop => \"stop\" });
}
",
        "stop\nL10/10\n",
    );
}

// The walker really compiled the shapes above, so the LIR half of every
// differential is the LIR emitter and not a silent fallback to the AST one.
#[test]
fn gc_enum_payload_21_the_walker_really_compiled_these() {
    assert_walker_compiled(
        "enum Note { Text(String), Code(i64), Empty }
fn bang(e: String) -> String { return \"!\" + e; }
fn build(i: i64) -> Note { return Note::Text(\"n\" + i.toString()); }
fn main() {
    let b: Result<i64, String> = Err(\"bad\");
    println(match b.map_err(bang) { Ok(v) => v.toString(), Err(e) => e });
    println(match build(1) {
        Note::Text(s) => s,
        Note::Code(c) => c.toString(),
        Note::Empty => \"empty\",
    });
}
",
        &["bang", "build", "main"],
    );
}
