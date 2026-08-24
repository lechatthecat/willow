//! `AtomicI64` / `AtomicBool` compiled from Lowered IR (willow-0g8j.2.13).
//!
//! Both cells are a GC-allocated word the runtime reads and writes atomically.
//! Admitting them to the walker is three pieces: the type itself, the
//! `Atomic*::new` constructor, and the five method intrinsics. Nothing here
//! suspends, so the same code has to work identically in a synchronous function
//! and inside a poll function.
//!
//! Every test asserts the same output from the AST emitter and the walker. The
//! walker is confirmed to be the path that ran, so a coverage regression that
//! sent a function back to the AST emitter would fail here rather than pass
//! vacuously by comparing the AST path against itself.
//!
//! 20 perspectives:
//!   1 i64 load                       11 cell stored in a class field
//!   2 i64 store                      12 cell in an array
//!   3 i64 add returns the PREVIOUS   13 cell passed to a function
//!   4 i64 sub returns the PREVIOUS   14 cell returned from a function
//!   5 i64 swap returns the previous  15 cell captured across a suspension
//!   6 bool load/store/swap           16 two tasks incrementing one counter
//!   7 operand is a computed expr     17 a bool cell as a completion flag
//!   8 receiver is a call result      18 GC stress with a live cell
//!   9 cell used in a loop            19 scheduler pressure
//!  10 several cells side by side     20 the cell survives a collection

use super::support::{compile_and_run_with_env, compile_with_compiler_env};

const AST: [(&str, &str); 1] = [("WILLOW_LIR_BACKEND", "0")];
const LIR: [(&str, &str); 2] = [("WILLOW_LIR_BACKEND", "1"), ("WILLOW_LIR_REQUIRE", "1")];
const LIR_BUDGET: [(&str, &str); 3] = [
    ("WILLOW_LIR_BACKEND", "1"),
    ("WILLOW_LIR_REQUIRE", "1"),
    ("WILLOW_TASK_BUDGET", "1"),
];
const LIR_STRESS: [(&str, &str); 3] = [
    ("WILLOW_LIR_BACKEND", "1"),
    ("WILLOW_LIR_REQUIRE", "1"),
    ("WILLOW_GC_STRESS", "alloc"),
];

/// `expected` must come out of all four configurations, and `functions` must
/// each be named in the walker's selection log — otherwise a function that
/// quietly fell back would make the AST/LIR comparison compare the AST path
/// with itself.
fn assert_atomics(source: &str, expected: &str, functions: &[&str]) {
    for env in [&AST[..], &LIR[..], &LIR_BUDGET[..], &LIR_STRESS[..]] {
        let (out, ok) = compile_and_run_with_env(source, env);
        assert!(ok, "atomics run failed under {env:?}: {out}");
        assert_eq!(out, expected, "wrong output under {env:?}");
    }
    let (ok, stderr) = compile_with_compiler_env(
        source,
        &[
            ("WILLOW_LIR_BACKEND", "1"),
            ("WILLOW_LIR_REQUIRE", "1"),
            ("WILLOW_LIR_LOG", "1"),
        ],
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

#[test]
fn lir_atomics_01_i64_load_reads_the_initial_value() {
    assert_atomics(
        r#"
fn main() {
    let c = AtomicI64::new(10);
    println(c.load());
}
"#,
        "10\n",
        &["main"],
    );
}

#[test]
fn lir_atomics_02_i64_store_replaces_the_value() {
    assert_atomics(
        r#"
fn main() {
    let c = AtomicI64::new(10);
    c.store(42);
    println(c.load());
}
"#,
        "42\n",
        &["main"],
    );
}

#[test]
fn lir_atomics_03_i64_add_returns_the_previous_value() {
    // `add` is a fetch-add: the RESULT is what was in the cell before, not
    // after. Getting this backwards is invisible to a test that only checks
    // `load()` afterwards, so both are printed.
    assert_atomics(
        r#"
fn main() {
    let c = AtomicI64::new(10);
    println(c.add(5));
    println(c.load());
}
"#,
        "10\n15\n",
        &["main"],
    );
}

#[test]
fn lir_atomics_04_i64_sub_returns_the_previous_value() {
    assert_atomics(
        r#"
fn main() {
    let c = AtomicI64::new(10);
    println(c.sub(4));
    println(c.load());
}
"#,
        "10\n6\n",
        &["main"],
    );
}

#[test]
fn lir_atomics_05_i64_swap_returns_the_previous_value() {
    assert_atomics(
        r#"
fn main() {
    let c = AtomicI64::new(1);
    println(c.swap(9));
    println(c.load());
}
"#,
        "1\n9\n",
        &["main"],
    );
}

#[test]
fn lir_atomics_06_bool_cell_loads_stores_and_swaps() {
    // The bool cell is a different runtime symbol and a different result width;
    // `AtomicBool` deliberately has no `add`/`sub`.
    assert_atomics(
        r#"
fn main() {
    let b = AtomicBool::new(false);
    println(b.load());
    b.store(true);
    println(b.load());
    println(b.swap(false));
    println(b.load());
}
"#,
        "false\ntrue\ntrue\nfalse\n",
        &["main"],
    );
}

#[test]
fn lir_atomics_07_the_operand_may_be_a_computed_expression() {
    // The receiver is only an SSA temporary while the operand is evaluated, and
    // a call in that operand can allocate and collect.
    assert_atomics(
        r#"
fn weight(n: i64) -> i64 { return n * 3; }
fn main() {
    let c = AtomicI64::new(0);
    c.store(weight(4) + 1);
    println(c.load());
    println(c.add(weight(2)));
    println(c.load());
}
"#,
        "13\n13\n19\n",
        &["weight", "main"],
    );
}

#[test]
fn lir_atomics_08_the_receiver_may_be_a_call_result() {
    assert_atomics(
        r#"
fn fresh(start: i64) -> AtomicI64 { return AtomicI64::new(start); }
fn main() {
    println(fresh(7).load());
    println(fresh(7).add(1));
}
"#,
        "7\n7\n",
        &["fresh", "main"],
    );
}

#[test]
fn lir_atomics_09_a_cell_accumulates_across_a_loop() {
    assert_atomics(
        r#"
fn main() {
    let total = AtomicI64::new(0);
    let mut i = 1;
    while i <= 5 {
        total.add(i);
        i = i + 1;
    }
    println(total.load());
}
"#,
        "15\n",
        &["main"],
    );
}

#[test]
fn lir_atomics_10_several_cells_stay_independent() {
    assert_atomics(
        r#"
fn main() {
    let a = AtomicI64::new(1);
    let b = AtomicI64::new(2);
    let flag = AtomicBool::new(true);
    a.add(10);
    b.sub(10);
    flag.store(false);
    println(a.load());
    println(b.load());
    println(flag.load());
}
"#,
        "11\n-8\nfalse\n",
        &["main"],
    );
}

#[test]
fn lir_atomics_11_a_cell_lives_in_a_class_field() {
    // The field's declared type has to pass `supported_type` for the class
    // itself to be in the subset, so this is the type admission seen from the
    // class-layout side rather than the local-binding side.
    assert_atomics(
        r#"
class Meter { pub hits: AtomicI64; }
fn main() {
    let m = new Meter(AtomicI64::new(0));
    m.hits.add(3);
    m.hits.add(4);
    println(m.hits.load());
}
"#,
        "7\n",
        &["main"],
    );
}

#[test]
fn lir_atomics_12_cells_live_in_an_array() {
    assert_atomics(
        r#"
import std::collections::Array;
fn main() {
    let cells: Array<AtomicI64> = [AtomicI64::new(1), AtomicI64::new(2)];
    let mut i = 0;
    while i < cells.len() {
        cells[i].add(10);
        i = i + 1;
    }
    println(cells[0].load());
    println(cells[1].load());
}
"#,
        "11\n12\n",
        &["main"],
    );
}

#[test]
fn lir_atomics_13_a_cell_passes_through_a_parameter() {
    assert_atomics(
        r#"
fn bump(c: AtomicI64, by: i64) { c.add(by); }
fn main() {
    let c = AtomicI64::new(0);
    bump(c, 5);
    bump(c, 6);
    println(c.load());
}
"#,
        "11\n",
        &["bump", "main"],
    );
}

#[test]
fn lir_atomics_14_a_cell_is_returned_and_kept() {
    assert_atomics(
        r#"
fn make() -> AtomicBool { return AtomicBool::new(true); }
fn main() {
    let flag = make();
    println(flag.load());
    flag.store(false);
    println(flag.load());
}
"#,
        "true\nfalse\n",
        &["make", "main"],
    );
}

#[test]
fn lir_atomics_15_a_cell_stays_live_across_a_suspension() {
    // In a poll function the cell is a frame slot, and the async frame layout
    // is what has to keep it alive across the park.
    assert_atomics(
        r#"
async fn main() {
    let c = AtomicI64::new(1);
    c.add(1);
    await sleep(1);
    c.add(1);
    await sleep(1);
    println(c.load());
}
"#,
        "3\n",
        &["main"],
    );
}

#[test]
fn lir_atomics_16_two_tasks_share_one_counter() {
    // The point of the type: the increments are atomic, so the total is exact
    // no matter how the two tasks interleave.
    assert_atomics(
        r#"
async fn bump(c: AtomicI64, times: i64) -> i64 {
    let mut i = 0;
    while i < times {
        c.add(1);
        await yield();
        i = i + 1;
    }
    return times;
}
async fn main() {
    let c = AtomicI64::new(0);
    let a = bump(c, 20);
    let b = bump(c, 30);
    await a;
    await b;
    println(c.load());
}
"#,
        "50\n",
        &["bump", "main"],
    );
}

#[test]
fn lir_atomics_17_a_bool_cell_signals_completion_between_tasks() {
    // The first read is taken before the task is spawned. Taking it afterwards
    // would race: `sleep` widens the window but orders nothing, so a worker can
    // run the whole body between the spawn and the read.
    assert_atomics(
        r#"
async fn worker(done: AtomicBool) -> i64 {
    await sleep(1);
    done.store(true);
    return 1;
}
async fn main() {
    let done = AtomicBool::new(false);
    println(done.load());
    let t = worker(done);
    await t;
    println(done.load());
}
"#,
        "false\ntrue\n",
        &["worker", "main"],
    );
}

#[test]
fn lir_atomics_18_a_live_cell_survives_repeated_allocation() {
    // Under `WILLOW_GC_STRESS=alloc` every allocation in the loop collects; the
    // cell is reachable only through the local, so losing its root would read
    // back a freed word.
    assert_atomics(
        r#"
import std::collections::Array;
fn main() {
    let c = AtomicI64::new(100);
    let mut i = 0;
    while i < 20 {
        let junk: Array<i64> = [i, i + 1, i + 2];
        c.add(junk.len());
        i = i + 1;
    }
    println(c.load());
}
"#,
        "160\n",
        &["main"],
    );
}

#[test]
fn lir_atomics_19_a_cell_is_correct_under_scheduler_pressure() {
    // `WILLOW_TASK_BUDGET=1` parks at every safepoint, so each task resumes
    // between its own read and write as often as the scheduler allows.
    assert_atomics(
        r#"
async fn tick(c: AtomicI64) -> i64 {
    let mut i = 0;
    while i < 10 {
        c.add(1);
        await yield();
        i = i + 1;
    }
    return 0;
}
async fn main() {
    let c = AtomicI64::new(0);
    let a = tick(c);
    let b = tick(c);
    let d = tick(c);
    await a;
    await b;
    await d;
    println(c.load());
}
"#,
        "30\n",
        &["tick", "main"],
    );
}

#[test]
fn lir_atomics_20_a_cell_reached_only_through_an_object_survives_a_collection() {
    // The only path to the cell is object -> field, so it stays alive only if
    // the collector traces the field. Nothing here holds the cell directly.
    assert_atomics(
        r#"
import std::collections::Array;
class Box { pub cell: AtomicI64; }
fn main() {
    let b = new Box(AtomicI64::new(5));
    let mut i = 0;
    while i < 20 {
        let junk: Array<String> = ["a", "b", "c"];
        b.cell.add(junk.len());
        i = i + 1;
    }
    println(b.cell.load());
}
"#,
        "65\n",
        &["main"],
    );
}

#[test]
fn lir_atomics_21_the_example_is_fully_lir_compiled() {
    // The example's header claims every function it declares is compiled from
    // the lowered IR; `WILLOW_LIR_REQUIRE=1` is what keeps that claim honest,
    // and the log check names them so a silent fallback cannot pass by leaving
    // the run's output unchanged.
    let source = include_str!("../../example/lir_atomics.wi");
    assert_atomics(
        source,
        "10\n15\n12\n19\ntrue\n48\n23\n18\n18\n100\nfalse\n50\ntrue\ntrue\nfalse\n",
        &[
            "weight",
            "bump",
            "fresh",
            "previous_values",
            "computed_operand",
            "through_fields",
            "in_an_array",
            "tick",
            "main",
        ],
    );
}

#[test]
fn lir_atomics_22_a_user_static_new_returning_a_cell_is_not_hijacked() {
    // `AtomicI64::new` is recognized by result type AND class name. A user
    // `static fn new` that returns a cell matches the result type and the
    // method name, so keying on those alone would silently replace its body
    // with the runtime constructor: the seed would reach the cell unscaled and
    // the `println` would never run.
    assert_atomics(
        r#"
class Factory {
    pub static fn new(seed: i64) -> AtomicI64 {
        println("factory ran");
        return AtomicI64::new(seed * 10);
    }
}
fn main() {
    let c = Factory::new(4);
    println(c.load());
}
"#,
        "factory ran\n40\n",
        &["main"],
    );
}

#[test]
fn lir_atomics_23_a_user_static_new_returning_a_bool_cell_is_not_hijacked() {
    // Same hole on the other width, where the substituted constructor would
    // also flip the value: the runtime `new` would store the argument, and the
    // user's method inverts it.
    assert_atomics(
        r#"
class Switch {
    pub static fn new(start: bool) -> AtomicBool {
        println("switch built");
        return AtomicBool::new(!start);
    }
}
fn main() {
    let b = Switch::new(true);
    println(b.load());
}
"#,
        "switch built\nfalse\n",
        &["main"],
    );
}
