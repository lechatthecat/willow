//! A store into a class's static property from inside an HIR island — a
//! `defer` block, a `match` arm, a `select` case — on the LIR-walking backend
//! (willow-0g8j.15).
//!
//! A function-level `Class::prop = value;` is `LirInst::StaticFieldAssign` and
//! has always been walked and emitted. An island is different: it is not
//! lowered into the LIR block graph at all, the walker keeps it as HIR and
//! emits it statement by statement, and that statement subset — an expression,
//! an assignment to a local, a `let`, an `if` — did not include the static
//! store. So a single
//!
//!     defer { Registry::count = Registry::count + n; }
//!
//! sent the whole enclosing function back to the AST emitter with the reason
//! "it registers an unsupported `defer` body". No miscompile — a subset gap,
//! and a silent one, since the fallback prints the same answer.
//!
//! The store is admitted in BOTH island scopes: its destination is a data
//! segment, so it needs no storage of its own at all. A `let` beside it is
//! admitted too as of willow-0g8j.3 — `emit_deferred_action` brackets a
//! deferred body's `vars` and GC root depth the way the match emitter brackets
//! an arm — and perspective 23 is the pair of them together, a `let` a
//! deferred static store reads.
//!
//! Since willow-0g8j.3 a body outside the walker's subset is a compile error,
//! so a run that prints the right answer is proof the walker produced it.
//!
//! 24 perspectives:
//!   1 every island body is logged     13 a subclass value into a base slot
//!   2 the bead's repro                14 two defers run last-in-first-out
//!   3 the store reads its own slot    15 a defer inside a loop body
//!   4 an `if` picks the value         16 a nested `if` in a defer
//!   5 the `else` branch stores        17 another class's property
//!   6 a `match` arm stores            18 a defer on the recover path
//!   7 an arm with a payload binding   19 an async function's defer
//!   8 a `select` case stores          20 a String property under GC stress
//!   9 `Self::prop` in a defer         21 an Array property under GC stress
//!  10 `Self::prop` in a match arm     22 an untaken branch stores nothing
//!  11 a String property               23 a `let` in a defer is still refused
//!  12 an Array property               24 the example, with no fallback at all

use super::support::{
    compile_with_compiler_env, compile_with_env_and_run, compile_with_env_and_run_under,
};

/// No extra compiler environment: the ordinary build.
const PLAIN: [(&str, &str); 0] = [];
const LIR_LOG: [(&str, &str); 1] = [("WILLOW_LIR_LOG", "1")];
const ALLOC_STRESS: [(&str, &str); 1] = [("WILLOW_GC_STRESS", "alloc")];
const MINOR_STRESS: [(&str, &str); 1] = [("WILLOW_GC_STRESS", "minor")];

/// The build runs and prints `expected`. Since willow-0g8j.3 every body is
/// compiled from lowered IR, so a body the walker cannot take is a compile
/// error here rather than a second emitter's answer.
#[track_caller]
fn assert_project_output(source: &str, expected: &str) {
    let (out, ok) = compile_with_env_and_run(source, &PLAIN);
    assert!(ok, "run failed: {out}");
    assert_eq!(out, expected, "wrong output");
}

/// The class every perspective stores into.
const REGISTRY: &str = "class Registry {
    pub static mut count: i64 = 0;
    pub static mut label: String = \"none\";
    pub static mut seen: Array<i64> = [];
}
";

/// [`REGISTRY`] followed by `body`, as one program.
fn with_registry(body: &str) -> String {
    format!("import std::collections::Array;\n\n{REGISTRY}\n{body}")
}

// 1. The selection log explicitly records each lowered island body.
#[test]
fn lir_static_body_01_every_island_body_is_logged() {
    let src = with_registry(
        "fn deferred(n: i64) -> i64 {
    defer {
        Registry::count = n;
    }
    return n;
}

fn matched(n: i64) -> i64 {
    match n {
        0 => {
            Registry::count = 1;
        }
        _ => {
            Registry::count = 2;
        }
    }
    return Registry::count;
}

fn selected() -> i64 {
    let ch = Channel<i64>::with_capacity(1);
    ch.send(7);
    select {
        let v = ch.recv() => {
            Registry::count = v;
        }
    }
    return Registry::count;
}

fn main() { println(deferred(3) + matched(0) + selected()); }",
    );
    let (ok, stderr) = compile_with_compiler_env(&src, &LIR_LOG);
    assert!(ok, "the program must compile: {stderr}");
    for name in ["deferred", "matched", "selected", "main"] {
        assert!(
            stderr.contains(&format!("[lir] compiling `{name}` from lowered IR")),
            "`{name}` was not walker-compiled:\n{stderr}"
        );
    }
}

// 2. The bead's own repro: the store is the deferred body's only statement.
#[test]
fn lir_static_body_02_a_defer_block_stores() {
    assert_project_output(
        &with_registry(
            "fn bump(n: i64) -> i64 {
    defer {
        Registry::count = n;
    }
    return n * 2;
}

fn main() {
    println(bump(3));
    println(Registry::count);
}",
        ),
        "6\n3\n",
    );
}

// 3. The value expression reads the same property the store writes, so the
// load and the store must both resolve to the one data segment.
#[test]
fn lir_static_body_03_the_store_reads_its_own_slot() {
    assert_project_output(
        &with_registry(
            "fn add(n: i64) -> i64 {
    defer {
        Registry::count = Registry::count + n;
    }
    return Registry::count;
}

fn main() {
    println(add(4));
    println(add(5));
    println(Registry::count);
}",
        ),
        "0\n4\n9\n",
    );
}

// 4. An `if` inside the deferred body was already emittable; only the store
// under it was not.
#[test]
fn lir_static_body_04_an_if_picks_the_stored_value() {
    assert_project_output(
        &with_registry(
            "fn classify(n: i64) -> i64 {
    defer {
        if n > 10 {
            Registry::label = \"big\";
        } else {
            Registry::label = \"small\";
        }
    }
    return n;
}

fn main() {
    println(classify(20));
    println(Registry::label);
}",
        ),
        "20\nbig\n",
    );
}

// 5. The other branch of the same `if`.
#[test]
fn lir_static_body_05_the_else_branch_stores() {
    assert_project_output(
        &with_registry(
            "fn classify(n: i64) -> i64 {
    defer {
        if n > 10 {
            Registry::label = \"big\";
        } else {
            Registry::label = \"small\";
        }
    }
    return n;
}

fn main() {
    println(classify(2));
    println(Registry::label);
}",
        ),
        "2\nsmall\n",
    );
}

// 6. The bracketed island: a `match` used as a statement is how the source
// spells a multi-way store.
#[test]
fn lir_static_body_06_a_match_arm_stores() {
    assert_project_output(
        &with_registry(
            "fn name(n: i64) -> String {
    match n {
        0 => {
            Registry::label = \"zero\";
        }
        1 => {
            Registry::label = \"one\";
        }
        _ => {
            Registry::label = \"many\";
        }
    }
    return Registry::label;
}

fn main() {
    println(name(1));
    println(name(4));
}",
        ),
        "one\nmany\n",
    );
}

// 7. An arm that binds a payload: the binding is the arm's own, and the store
// runs inside the same bracket that holds it.
#[test]
fn lir_static_body_07_an_arm_with_a_payload_binding_stores() {
    assert_project_output(
        &with_registry(
            "enum Reading {
    Value(i64),
    Missing,
}

fn record(r: Reading) -> i64 {
    match r {
        Value(v) => {
            Registry::count = v * 2;
        }
        Missing => {
            Registry::count = -1;
        }
    }
    return Registry::count;
}

fn main() {
    println(record(Value(6)));
    println(record(Missing));
}",
        ),
        "12\n-1\n",
    );
}

// 8. A `select` case body is bracketed exactly as a `match` arm is.
#[test]
fn lir_static_body_08_a_select_case_stores() {
    assert_project_output(
        &with_registry(
            "fn selected() -> i64 {
    let ch = Channel<i64>::with_capacity(1);
    ch.send(7);
    select {
        let v = ch.recv() => {
            Registry::count = v;
        }
    }
    return Registry::count;
}

fn main() { println(selected()); }",
        ),
        "7\n",
    );
}

// 9. `Self::` inside a deferred body resolves to the class the body is
// DECLARED in (willow-0g8j.13), and the store is the same store.
#[test]
fn lir_static_body_09_self_prop_in_a_defer() {
    assert_project_output(
        "class Meter {
    pub static mut issued: i64 = 0;

    pub static fn arm(n: i64) -> i64 {
        defer {
            Self::issued = Self::issued + n;
        }
        return n;
    }
}

fn main() {
    println(Meter::arm(2));
    println(Meter::arm(3));
    println(Meter::issued);
}",
        "2\n3\n5\n",
    );
}

// 10. The same resolution inside the other island shape.
#[test]
fn lir_static_body_10_self_prop_in_a_match_arm() {
    assert_project_output(
        "class Meter {
    pub static mut issued: i64 = 0;

    pub static fn note(n: i64) -> i64 {
        match n {
            0 => {
                Self::issued = 10;
            }
            _ => {
                Self::issued = Self::issued + 1;
            }
        }
        return Self::issued;
    }
}

fn main() {
    println(Meter::note(0));
    println(Meter::note(9));
}",
        "10\n11\n",
    );
}

// 11. A heap-valued property: the store goes through the write barrier a
// global static needs, not a plain word store.
#[test]
fn lir_static_body_11_a_string_property() {
    assert_project_output(
        &with_registry(
            "fn tag(a: String, b: String) -> i64 {
    defer {
        Registry::label = a + \"-\" + b;
    }
    return 1;
}

fn main() {
    println(tag(\"left\", \"right\"));
    println(Registry::label);
}",
        ),
        "1\nleft-right\n",
    );
}

// 12. An array-typed property, stored from a deferred body and read after the
// function it was deferred in has returned.
#[test]
fn lir_static_body_12_an_array_property() {
    assert_project_output(
        &with_registry(
            "fn fill() -> i64 {
    defer {
        Registry::seen = [4, 6];
    }
    return 9;
}

fn main() {
    println(fill());
    println(Registry::seen[0] + Registry::seen[1]);
}",
        ),
        "9\n10\n",
    );
}

// 13. The DECLARED type of the property decides the store, so a subclass value
// is stored as its base — the same rule the function-level store follows.
#[test]
fn lir_static_body_13_a_subclass_value_into_a_base_slot() {
    assert_project_output(
        "open class Animal {
    pub legs: i64;
    pub open fn legs_of(self) -> i64 { return self.legs; }
}

class Dog extends Animal {
    pub override fn legs_of(self) -> i64 { return self.legs + 100; }
}

class Pen {
    pub static mut resident: Animal = new Animal(0);
}

fn stock(n: i64) -> i64 {
    defer {
        Pen::resident = new Dog(n);
    }
    return n;
}

fn main() {
    println(stock(4));
    println(Pen::resident.legs_of());
}",
        "4\n104\n",
    );
}

// 14. Deferred bodies run last-in-first-out. The ordering belongs to the
// emitter, and admitting the statement must not disturb it.
#[test]
fn lir_static_body_14_two_defers_run_lifo() {
    assert_project_output(
        &with_registry(
            "fn ordered() -> i64 {
    defer {
        Registry::count = Registry::count * 2;
    }
    defer {
        Registry::count = 5;
    }
    return 0;
}

fn main() {
    println(ordered());
    println(Registry::count);
}",
        ),
        "0\n10\n",
    );
}

// 15. A `defer` inside a loop body registers once per iteration, and every
// registration runs at scope exit.
#[test]
fn lir_static_body_15_a_defer_inside_a_loop() {
    assert_project_output(
        &with_registry(
            "fn counted(n: i64) -> i64 {
    let mut i = 0;
    while i < n {
        defer {
            Registry::count = Registry::count + 1;
        }
        i = i + 1;
    }
    return Registry::count;
}

fn main() {
    println(counted(3));
    println(Registry::count);
}",
        ),
        "3\n3\n",
    );
}

// 16. `if` inside `if` inside the deferred body: the island's statement walk
// is recursive, so the store has to be admitted at every depth.
#[test]
fn lir_static_body_16_a_nested_if_in_a_defer() {
    assert_project_output(
        &with_registry(
            "fn grade(n: i64) -> i64 {
    defer {
        if n > 0 {
            if n > 10 {
                Registry::label = \"high\";
            } else {
                Registry::label = \"low\";
            }
        }
    }
    return n;
}

fn main() {
    println(grade(5));
    println(Registry::label);
    println(grade(50));
    println(Registry::label);
}",
        ),
        "5\nlow\n50\nhigh\n",
    );
}

// 17. The property need not belong to the enclosing class, or to any class the
// function is inside.
#[test]
fn lir_static_body_17_another_classes_property() {
    assert_project_output(
        "class Left {
    pub static mut n: i64 = 0;
}

class Right {
    pub static mut n: i64 = 0;

    pub static fn touch(v: i64) -> i64 {
        defer {
            Left::n = v;
            Self::n = v * 2;
        }
        return v;
    }
}

fn main() {
    println(Right::touch(3));
    println(Left::n);
    println(Right::n);
}",
        "3\n3\n6\n",
    );
}

// 18. The unwinder replays deferred bodies on the panic path too, which is the
// path with no root bracket at all — the reason a `let` is refused there and
// this store is not.
#[test]
fn lir_static_body_18_a_defer_on_the_recover_path() {
    assert_project_output(
        &with_registry(
            "fn boom() {
    defer match recover() {
        Some(info) => {
            Registry::label = \"caught\";
        }
        None => {}
    }
    defer {
        Registry::count = 42;
    }
    panic(\"boom\");
}

fn main() {
    boom();
    println(Registry::count);
    println(Registry::label);
}",
        ),
        "42\ncaught\n",
    );
}

// 19. An async body is walked by the same eligibility and emitted through the
// poll ABI, so its deferred island had the same gap.
#[test]
fn lir_static_body_19_an_async_functions_defer() {
    assert_project_output(
        &with_registry(
            "async fn work(n: i64) -> i64 {
    defer {
        Registry::count = n;
    }
    return n * 2;
}

async fn main() {
    println(await work(4));
    println(Registry::count);
}",
        ),
        "8\n4\n",
    );
}

// 20. A collection at every allocation, with the stored value itself freshly
// allocated: the String must survive from the deferred body to the read.
#[test]
fn lir_static_body_20_a_string_property_under_allocation_stress() {
    let src = with_registry(
        "fn tag(n: i64) -> i64 {
    defer {
        Registry::label = \"tag-\" + n.toString();
    }
    return n;
}

fn main() {
    println(tag(7));
    println(Registry::label);
    println(Registry::label + \"!\");
}",
    );
    let (out, ok) = compile_with_env_and_run_under(&src, &PLAIN, &ALLOC_STRESS);
    assert!(ok, "allocation-stress run failed: {out}");
    assert_eq!(out, "7\ntag-7\ntag-7!\n");
}

// 21. The same for an array, across a minor collection: a global static is a
// root, and the store has to tell the collector so.
#[test]
fn lir_static_body_21_an_array_property_under_minor_stress() {
    let src = with_registry(
        "fn fill(n: i64) -> i64 {
    defer {
        Registry::seen = [n, n + 1, n + 2];
    }
    return n;
}

fn main() {
    println(fill(1));
    println(Registry::seen[0] + Registry::seen[1] + Registry::seen[2]);
    println(fill(10));
    println(Registry::seen[2]);
}",
    );
    let (out, ok) = compile_with_env_and_run_under(&src, &PLAIN, &MINOR_STRESS);
    assert!(ok, "minor-collection-stress run failed: {out}");
    assert_eq!(out, "1\n6\n10\n12\n");
}

// 22. A branch that is not taken stores nothing: admitting the statement must
// not make it unconditional.
#[test]
fn lir_static_body_22_an_untaken_branch_stores_nothing() {
    assert_project_output(
        &with_registry(
            "fn maybe(n: i64) -> i64 {
    defer {
        if n > 100 {
            Registry::count = 999;
        }
    }
    return n;
}

fn main() {
    println(maybe(1));
    println(Registry::count);
}",
        ),
        "1\n0\n",
    );
}

// 23. The boundary that MOVED (willow-0g8j.3): a `let` in a deferred body is
// admitted, and a deferred static store can read it. `emit_deferred_action`
// replays the body inside a bracket of its own — `vars` and the GC root depth
// snapshotted before and restored after — so the binding's rooted slot is
// popped at every exit the registration is live for, and the code after the
// flush stays at the depth it was emitted for. The String property makes that
// concrete: `doubled` is GC-managed, so a leaked slot would show up here.
#[test]
fn lir_static_body_23_a_let_feeds_a_deferred_store() {
    let src = with_registry(
        "fn bump(n: i64) -> i64 {
    defer {
        let doubled = n * 2;
        let tag = \"x\";
        Registry::count = doubled;
        Registry::label = tag;
    }
    return n;
}

fn main() {
    println(bump(3));
    println(Registry::count);
    println(Registry::label);
}",
    );
    assert_project_output(&src, "3\n6\nx\n");
    let (ok, stderr) = compile_with_compiler_env(&src, &LIR_LOG);
    assert!(ok, "the program must compile: {stderr}");
    assert!(
        stderr.contains("[lir] compiling `bump` from lowered IR"),
        "`bump` was not walker-compiled:\n{stderr}"
    );
    for stress in [&ALLOC_STRESS[..], &MINOR_STRESS[..]] {
        let (out, ran) = compile_with_env_and_run_under(&src, &PLAIN, stress);
        assert!(ran, "stressed run failed: {out}");
        assert_eq!(out, "3\n6\nx\n", "wrong output under {stress:?}");
    }
}

// 24. The example compiles entirely through lowered IR.
#[test]
fn lir_static_body_24_the_example_has_no_fallback() {
    let src = std::fs::read_to_string("example/lir_static_property_bodies.wi")
        .expect("example/lir_static_property_bodies.wi must exist");
    assert_project_output(&src, "6\n3\n20\nbig\none\nmany\n2\n5\n0\n10\n7\n9\n10\n");
}
