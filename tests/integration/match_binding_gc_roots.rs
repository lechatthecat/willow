//! A `match` pattern binding is a GC root of its own (willow-10zt).
//!
//! Willow's collector is generational, and only half of it stands still. The
//! old generation never moves and a *direct* root is promoted in place -- that
//! is what lets generated code keep an SSA alias of a rooted local. A young
//! object the collector reaches only through a heap slot is a different case
//! entirely: it is copied into the old generation, the owning slot is fixed up,
//! and the source is reclaimed.
//!
//! An enum payload and the object inside an interface box are exactly that
//! second case. Rooting the scrutinee keeps the payload *reachable*, but the
//! scrutinee is what stays put while its child moves, so a binding that copied
//! the payload word into a Cranelift variable named freed memory as soon as the
//! arm allocated. A GC-managed binding gets a rooted stack slot instead, which
//! makes it a direct root the collector pins.
//!
//! Each test mutates through the binding after a collection and reads the value
//! back through the scrutinee, so a stale binding shows up as the old number
//! rather than the new one -- a silent wrong answer, not a crash. Every program
//! runs under `WILLOW_GC_STRESS`. Note that the stress
//! mode that matters here is `minor`: `alloc` both skips the minor collector and
//! allocates every object straight into the old generation, so it can never move
//! anything. `gc_minor_collect()` is the deterministic trigger.
//!
//! 25 perspectives:
//!   1 enum payload written after a GC   14 a non-GC-only match is unchanged
//!   2 two allocations after the bind    15 sibling arms bind different types
//!   3 a nested enum payload             16 one arm falls through, one returns
//!   4 a class downcast binding          17 an arm panics under a binding
//!   5 an enum String payload            18 the collect is inside a callee
//!   6 a String payload returned         19 a `_` arm beside a binding arm
//!   7 GC and non-GC in one variant      20 two matches on one scrutinee
//!   8 the binding stored into an object 21 a downcast across two collects
//!   9 a match in a loop, 200 collects   22 the binding handed to a callee
//!  10 a Void match, non-Void arm tail   23 an `if` collects on one branch
//!  11 a binding held across two awaits  24 the example runs the same
//!  12 two allocations, nested enum      25 the example is fully LIR
//!  13 a nested match mutates the inner

use super::support::{compile_and_run_with_env, compile_with_compiler_env};

/// No extra compiler environment: the ordinary build.
const PLAIN: [(&str, &str); 0] = [];
const MINOR_STRESS: [(&str, &str); 1] = [("WILLOW_GC_STRESS", "minor")];
const ALLOC_STRESS: [(&str, &str); 1] = [("WILLOW_GC_STRESS", "alloc")];
const LIR_LOG: [(&str, &str); 1] = [("WILLOW_LIR_LOG", "1")];

/// Declarations every program shares.
///
/// Mutation goes through `bump`, `set_side` and `put` rather than a field store
/// written in the arm itself, because a field store inside a `match` arm is not
/// yet in the walker's statement subset, and since willow-0g8j.3 that is a
/// compile error rather than a fallback.
const PRELUDE: &str = r#"class Node { pub v: i64; }
class Cell { pub n: Node; }
enum Boxed { One(Node), Pair(Node, i64), Empty }
enum Wrap { Inner(Boxed), Nil }
enum Note { Text(String), Num(i64) }
enum Either { Left(Node), Right(Cell) }
interface Shape { fn area(self) -> i64; }
class Square implements Shape { pub side: i64; pub fn area(self) -> i64 { return self.side * self.side; } }
class Circle implements Shape { pub r: i64; pub fn area(self) -> i64 { return 3 * self.r * self.r; } }
fn bump(n: Node, v: i64) -> i64 { n.v = v; return 0; }
fn set_side(q: Square, v: i64) -> i64 { q.side = v; return 0; }
fn put(c: Cell, n: Node) -> i64 { c.n = n; return 0; }
fn note(v: i64) -> i64 { return v; }
fn collect_then(v: i64) -> i64 { gc_minor_collect(); return v; }
fn keep(n: Node) -> i64 { let c = new Cell(n); gc_minor_collect(); return c.n.v; }
fn peek(x: Boxed) -> i64 {
    match x {
        Boxed::One(n) => { return n.v; }
        Boxed::Pair(n, k) => { return n.v + k; }
        Boxed::Empty => { return -1; }
    }
}
fn read_either(e: Either) -> i64 {
    match e {
        Either::Left(n) => { return n.v; }
        Either::Right(c) => { return c.n.v; }
    }
}
fn side_of(s: Shape) -> i64 {
    match s {
        Square(q) => { return q.side; }
        Circle(c) => { return c.r; }
    }
}
"#;

fn program(body: &str) -> String {
    format!("{PRELUDE}{body}")
}

/// `expected` must come out plain and under both GC stress modes, and
/// `functions` must each be named in the
/// walker's selection log.
fn assert_binding(body: &str, expected: &str, functions: &[&str]) {
    let source = program(body);
    for env in [&PLAIN[..], &MINOR_STRESS[..], &ALLOC_STRESS[..]] {
        let (out, ok) = compile_and_run_with_env(&source, env);
        assert!(ok, "run failed under {env:?}: {out}");
        assert_eq!(out, expected, "wrong output under {env:?}");
    }
    assert_walker_compiled(&source, functions);
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

// 1. The bead's own repro. The payload is young and reachable only through the
//    enum, so the minor collection moves it; the write has to land on the copy
//    the scrutinee now points at. Before the fix this printed the original 7.
#[test]
fn binding_roots_01_enum_payload_written_after_a_collection() {
    assert_binding(
        r#"fn relabel(x: Boxed) -> i64 {
    match x {
        Boxed::One(n) => { gc_minor_collect(); bump(n, 99); }
        Boxed::Pair(n, k) => { bump(n, k); }
        Boxed::Empty => {}
    }
    return peek(x);
}
fn main() { println(relabel(Boxed::One(new Node(7)))); }
"#,
        "99\n",
        &["relabel", "peek", "main"],
    );
}

// 2. Allocation is what makes the hazard bite, so more than one of it: the
//    binding is read after two fresh objects and a collection.
#[test]
fn binding_roots_02_two_allocations_after_the_binding() {
    assert_binding(
        r#"fn allocate_then_read(x: Boxed) -> i64 {
    match x {
        Boxed::One(n) => {
            let a = new Node(1);
            let b = new Node(2);
            gc_minor_collect();
            return n.v + a.v + b.v;
        }
        Boxed::Pair(n, k) => { return -2; }
        Boxed::Empty => { return -1; }
    }
}
fn main() { println(allocate_then_read(Boxed::One(new Node(7)))); }
"#,
        "10\n",
        &["allocate_then_read", "main"],
    );
}

// 3. A payload that is itself an enum. Both bindings are roots and the inner
//    one moves out from under the outer one; before the fix the collector
//    walked a root graph holding the freed address and aborted the process
//    outright with `invalid root ... during minor collection`.
#[test]
fn binding_roots_03_a_nested_enum_payload() {
    assert_binding(
        r#"fn nested(w: Wrap) -> i64 {
    match w {
        Wrap::Inner(b) => {
            gc_minor_collect();
            match b {
                Boxed::One(n) => { gc_minor_collect(); bump(n, 55); }
                Boxed::Pair(n, k) => { bump(n, k); }
                Boxed::Empty => {}
            }
            return peek(b);
        }
        Wrap::Nil => { return -1; }
    }
}
fn main() { println(nested(Wrap::Inner(Boxed::One(new Node(7))))); }
"#,
        "55\n",
        &["nested", "main"],
    );
}

// 4. The same shape through an interface box: the box is the rooted scrutinee
//    and the object it holds is the child that moves, so a downcast binding is
//    stale for exactly the same reason. Read back through a fresh downcast,
//    which loads the fixed-up pointer.
#[test]
fn binding_roots_04_a_class_downcast_binding() {
    assert_binding(
        r#"fn regrow(s: Shape) -> i64 {
    match s {
        Square(q) => { gc_minor_collect(); set_side(q, 9); return side_of(s); }
        Circle(c) => { return c.r; }
    }
}
fn main() { println(regrow(new Square(2))); }
"#,
        "9\n",
        &["regrow", "side_of", "main"],
    );
}

// 5. A String payload across two allocations. Runtime strings are born in the
//    old generation, so this one cannot go stale -- it is here because the
//    rooting rule now covers it and a rooted String has to still read back as
//    itself.
#[test]
fn binding_roots_05_an_enum_string_payload() {
    assert_binding(
        r#"fn caption(m: Note) -> String {
    match m {
        Note::Text(s) => {
            let head = "[" + "note] ";
            gc_minor_collect();
            let tail = "!" + "";
            return head + s + tail;
        }
        Note::Num(k) => { return "none"; }
    }
}
fn main() { println(caption(Note::Text("kept"))); }
"#,
        "[note] kept!\n",
        &["caption", "main"],
    );
}

// 6. The narrowest String case: bind it, collect, hand it straight back. The
//    binding is now a stack slot, so the return has to reload from the slot
//    rather than from a register that no longer exists.
#[test]
fn binding_roots_06_a_string_payload_returned() {
    assert_binding(
        r#"fn text(m: Note) -> String {
    match m {
        Note::Text(s) => { gc_minor_collect(); return s; }
        Note::Num(k) => { return "none"; }
    }
}
fn main() { println(text(Note::Text("kept"))); }
"#,
        "kept\n",
        &["text", "main"],
    );
}

// 7. One variant with both kinds of payload. The `Node` gets a rooted slot and
//    the `i64` must NOT: rooting a scalar word would have the collector trace
//    it as an object pointer.
#[test]
fn binding_roots_07_gc_and_non_gc_in_one_variant() {
    assert_binding(
        r#"fn mixed(x: Boxed) -> i64 {
    match x {
        Boxed::Pair(n, k) => { gc_minor_collect(); bump(n, k * 2); return peek(x); }
        Boxed::One(n) => { return -2; }
        Boxed::Empty => { return -1; }
    }
}
fn main() { println(mixed(Boxed::Pair(new Node(7), 20))); }
"#,
        "60\n",
        &["mixed", "peek", "main"],
    );
}

// 8. The binding is written into a fresh object, which makes it heap-reachable
//    a second way, and both survive two more collections.
#[test]
fn binding_roots_08_the_binding_stored_into_an_object() {
    assert_binding(
        r#"fn cellwise(x: Boxed) -> i64 {
    match x {
        Boxed::One(n) => {
            let c = new Cell(n);
            gc_minor_collect();
            put(c, new Node(3));
            gc_minor_collect();
            return n.v + c.n.v;
        }
        Boxed::Pair(n, k) => { return -2; }
        Boxed::Empty => { return -1; }
    }
}
fn main() { println(cellwise(Boxed::One(new Node(7)))); }
"#,
        "10\n",
        &["cellwise", "main"],
    );
}

// 9. Two hundred iterations, each pushing an arm root and collecting. The arm
//    has to pop what it pushed or the root stack grows without bound.
#[test]
fn binding_roots_09_a_match_in_a_loop() {
    assert_binding(
        r#"fn total(x: Boxed) -> i64 {
    let mut sum = 0;
    let mut i = 0;
    while i < 200 {
        match x {
            Boxed::One(n) => { gc_minor_collect(); sum = sum + n.v; }
            Boxed::Pair(n, k) => { sum = sum + k; }
            Boxed::Empty => {}
        }
        i = i + 1;
    }
    return sum;
}
fn main() { println(total(Boxed::One(new Node(2)))); }
"#,
        "400\n",
        &["total", "main"],
    );
}

// 10. A `match` in statement position is typed `Void`, and its result variable
//     is an `I8`, but an arm may still end in an expression statement that has
//     an `i64` value -- `bump(n, 41);` here. Handing that word to the result
//     variable is a Cranelift type error that aborted the compiler; the arm
//     value is dropped now, the way the AST emitter already dropped it
//     (willow-vdfj).
#[test]
fn binding_roots_10_a_void_match_with_a_non_void_arm_tail() {
    assert_binding(
        r#"fn drop_tail(x: Boxed) -> i64 {
    match x {
        Boxed::One(n) => { gc_minor_collect(); bump(n, 41); }
        Boxed::Pair(n, k) => { bump(n, k); }
        Boxed::Empty => {}
    }
    return peek(x);
}
fn main() { println(drop_tail(Boxed::One(new Node(1)))); }
"#,
        "41\n",
        &["drop_tail", "main"],
    );
}

// 11. An async arm parks with the binding live. A poll return pops every active
//     binding root and re-registers it on the next poll, so an arm root the
//     cooperative shadow list did not know about would be left pointing into a
//     dead native frame -- and the unwind asserts the two depths agree.
#[test]
fn binding_roots_11_a_binding_held_across_two_awaits() {
    assert_binding(
        r#"async fn later(x: Boxed) -> i64 {
    match x {
        Boxed::One(n) => {
            await yield();
            gc_minor_collect();
            await yield();
            bump(n, 88);
            return peek(x);
        }
        Boxed::Pair(n, k) => { return -2; }
        Boxed::Empty => { return -1; }
    }
}
async fn main() { println(await later(Boxed::One(new Node(4)))); }
"#,
        "88\n",
        &["later", "main"],
    );
}

// 12. The outer binding of a nested enum, read after two allocations and two
//     collections without the inner one ever being bound.
#[test]
fn binding_roots_12_two_allocations_under_a_nested_enum() {
    assert_binding(
        r#"fn twice(w: Wrap) -> i64 {
    match w {
        Wrap::Inner(b) => {
            let a = new Node(1);
            gc_minor_collect();
            let c = new Node(2);
            gc_minor_collect();
            return peek(b) + a.v + c.v;
        }
        Wrap::Nil => { return -1; }
    }
}
fn main() { println(twice(Wrap::Inner(Boxed::One(new Node(7))))); }
"#,
        "10\n",
        &["twice", "main"],
    );
}

// 13. Two levels deep: the inner binding is mutated and the result is read back
//     through the OUTER binding, so both have to have survived.
#[test]
fn binding_roots_13_a_nested_match_mutates_the_inner_binding() {
    assert_binding(
        r#"fn deep(w: Wrap) -> i64 {
    match w {
        Wrap::Inner(b) => {
            match b {
                Boxed::One(n) => {
                    let junk = new Node(1);
                    gc_minor_collect();
                    bump(n, 33);
                    return peek(b) + junk.v;
                }
                Boxed::Pair(n, k) => { return -2; }
                Boxed::Empty => { return -1; }
            }
        }
        Wrap::Nil => { return -1; }
    }
}
fn main() { println(deep(Wrap::Inner(Boxed::One(new Node(7))))); }
"#,
        "34\n",
        &["deep", "main"],
    );
}

// 14. The willow-vdfj crash on its own, with no GC-managed binding anywhere:
//     a `Void` match whose arms end in `i64` calls still has to compile.
#[test]
fn binding_roots_14_a_non_gc_match_is_unchanged() {
    assert_binding(
        r#"fn tally(x: Boxed) -> i64 {
    let mut out = 0;
    match x {
        Boxed::One(n) => { note(5); out = 1; note(6); }
        Boxed::Pair(n, k) => { note(k); out = 2; note(k); }
        Boxed::Empty => { note(0); }
    }
    return out;
}
fn main() { println(tally(Boxed::Pair(new Node(1), 3))); }
"#,
        "2\n",
        &["tally", "main"],
    );
}

// 15. Sibling arms binding different GC types. Each arm pushes its own root and
//     pops it again, so the two paths reach the merge at the same depth.
#[test]
fn binding_roots_15_sibling_arms_bind_different_types() {
    assert_binding(
        r#"fn pick(e: Either) -> i64 {
    match e {
        Either::Left(n) => { gc_minor_collect(); bump(n, 11); return read_either(e); }
        Either::Right(c) => { gc_minor_collect(); put(c, new Node(22)); return read_either(e); }
    }
}
fn main() {
    println(pick(Either::Left(new Node(1))));
    println(pick(Either::Right(new Cell(new Node(2)))));
}
"#,
        "11\n22\n",
        &["pick", "read_either", "main"],
    );
}

// 16. One arm falls through to the merge and one leaves through a `return`.
//     The falling arm has to pop its root before the jump; the returning one
//     must not pop twice, having already unwound the whole depth itself.
#[test]
fn binding_roots_16_one_arm_falls_through_and_one_returns() {
    assert_binding(
        r#"fn mix(x: Boxed) -> i64 {
    match x {
        Boxed::One(n) => { gc_minor_collect(); bump(n, 12); }
        Boxed::Pair(n, k) => { gc_minor_collect(); return peek(x) + k; }
        Boxed::Empty => { return 0; }
    }
    return peek(x);
}
fn main() {
    println(mix(Boxed::One(new Node(1))));
    println(mix(Boxed::Pair(new Node(3), 4)));
}
"#,
        "12\n11\n",
        &["mix", "main"],
    );
}

// 17. An arm that panics with a rooted binding live. The panic unwinds the
//     whole root depth by itself, so the arm's own pop must not run.
#[test]
fn binding_roots_17_an_arm_panics_under_a_binding() {
    let source = program(
        r#"fn guard(x: Boxed) -> i64 {
    match x {
        Boxed::One(n) => { gc_minor_collect(); panic("a node is not allowed here"); }
        Boxed::Pair(n, k) => { return k; }
        Boxed::Empty => { return 0; }
    }
}
fn main() { println(guard(Boxed::One(new Node(1)))); }
"#,
    );
    let (out, ok) = compile_and_run_with_env(&source, &PLAIN);
    assert!(!ok, "expected a panic: {out}");
    assert!(
        out.contains("a node is not allowed here"),
        "report is missing the message:\n{out}"
    );
}

// 18. The collection happens inside a callee rather than in the arm, which is
//     the ordinary case: any call can collect.
#[test]
fn binding_roots_18_the_collect_is_inside_a_callee() {
    assert_binding(
        r#"fn indirect(x: Boxed) -> i64 {
    match x {
        Boxed::One(n) => { let k = collect_then(5); bump(n, k); return peek(x); }
        Boxed::Pair(n, k) => { return -2; }
        Boxed::Empty => { return -1; }
    }
}
fn main() { println(indirect(Boxed::One(new Node(1)))); }
"#,
        "5\n",
        &["indirect", "collect_then", "main"],
    );
}

// 19. A `_` arm beside a binding arm: the wildcard pushes nothing, so the two
//     arms only agree at the merge if the binding arm pops what it pushed.
#[test]
fn binding_roots_19_a_wildcard_arm_beside_a_binding_arm() {
    assert_binding(
        r#"fn wild(x: Boxed) -> i64 {
    match x {
        Boxed::One(n) => { gc_minor_collect(); bump(n, 13); }
        _ => {}
    }
    return peek(x);
}
fn main() {
    println(wild(Boxed::One(new Node(1))));
    println(wild(Boxed::Empty));
}
"#,
        "13\n-1\n",
        &["wild", "main"],
    );
}

// 20. The same scrutinee matched twice in a row. The second arm reads the
//     payload through its own binding after a collection, which only gives the
//     value the first match wrote if that binding is a root too.
#[test]
fn binding_roots_20_two_matches_on_one_scrutinee() {
    assert_binding(
        r#"fn twice_match(x: Boxed) -> i64 {
    match x { Boxed::One(n) => { gc_minor_collect(); bump(n, 21); } _ => {} }
    match x { Boxed::One(n) => { gc_minor_collect(); bump(n, n.v + 1); } _ => {} }
    return peek(x);
}
fn main() { println(twice_match(Boxed::One(new Node(1)))); }
"#,
        "22\n",
        &["twice_match", "main"],
    );
}

// 21. A downcast binding across two collections with an allocation between
//     them, so the object is given every chance to move.
#[test]
fn binding_roots_21_a_downcast_across_two_collects() {
    assert_binding(
        r#"fn twicecast(s: Shape) -> i64 {
    match s {
        Square(q) => {
            gc_minor_collect();
            let junk = new Node(1);
            gc_minor_collect();
            set_side(q, 6);
            return side_of(s) + junk.v;
        }
        Circle(c) => { return c.r; }
    }
}
fn main() { println(twicecast(new Square(2))); }
"#,
        "7\n",
        &["twicecast", "main"],
    );
}

// 22. The binding is handed to a function that allocates before reading it, so
//     the value has to still be right one frame down. Before the fix the callee
//     rooted the stale address and the collector aborted on it.
#[test]
fn binding_roots_22_the_binding_handed_to_a_callee() {
    assert_binding(
        r#"fn feed(x: Boxed) -> i64 {
    match x {
        Boxed::One(n) => { gc_minor_collect(); return keep(n); }
        _ => { return -1; }
    }
}
fn main() { println(feed(Boxed::One(new Node(7)))); }
"#,
        "7\n",
        &["feed", "keep", "main"],
    );
}

// 23. An `if` inside the arm that collects on one branch only: the binding is
//     read on the merge of the two, where one predecessor has collected and the
//     other has not.
#[test]
fn binding_roots_23_an_if_collects_on_one_branch() {
    assert_binding(
        r#"fn branchy(x: Boxed, flag: bool) -> i64 {
    match x {
        Boxed::One(n) => {
            if flag { gc_minor_collect(); }
            bump(n, 31);
            return peek(x);
        }
        _ => { return -1; }
    }
}
fn main() {
    println(branchy(Boxed::One(new Node(1)), true));
    println(branchy(Boxed::One(new Node(1)), false));
}
"#,
        "31\n31\n",
        &["branchy", "main"],
    );
}

// 24. The runnable example, plain and under both stress modes.
#[test]
fn binding_roots_24_the_example_runs_the_same() {
    let source = std::fs::read_to_string("example/match_binding_gc_roots.wi")
        .expect("example/match_binding_gc_roots.wi");
    for env in [&PLAIN[..], &MINOR_STRESS[..], &ALLOC_STRESS[..]] {
        let (out, ok) = compile_and_run_with_env(&source, env);
        assert!(ok, "example failed under {env:?}: {out}");
        assert_eq!(
            out, "99\n10\n55\n9\n[note] kept!\n300\n7\n",
            "wrong example output under {env:?}"
        );
    }
}

// 25. And every function in it goes through the walker, so the LIR column of
//     this file is really testing the LIR emitter.
#[test]
fn binding_roots_25_the_example_is_fully_lir() {
    let source = std::fs::read_to_string("example/match_binding_gc_roots.wi")
        .expect("example/match_binding_gc_roots.wi");
    assert_walker_compiled(
        &source,
        &[
            "relabel",
            "allocate_then_read",
            "nested",
            "regrow",
            "caption",
            "total",
            "handed_off",
            "peek",
            "side_of",
            "keep",
            "main",
        ],
    );
}
