//! A lexical scope's GC roots end where the scope does (willow-0g8j.3.3).
//!
//! The LIR back end gives every GC-managed local ONE stack slot, allocated and
//! rooted once at function entry: the lowered IR is a flat basic-block graph, so
//! a push/pop pair around each `let` would grow the shadow root stack once per
//! loop iteration. That makes the slot itself the root, and nothing freed a
//! scope's last value before the function returned — a loop body pinned one
//! object per call, where the AST emitter, which pops the root as the scope
//! closes, let the collector take it.
//!
//! `LirInst::ClearScopeRoots` is the fix: lowering marks the end of each lexical
//! scope with the source locals it declared — it cannot know which of them are
//! GC-managed, that needs the back end's enum table — and the emitter nulls the
//! slots that actually hold a root. Early exits need it stated again, because
//! `break` and `continue` jump out without passing the fallthrough close, the
//! same reason `FlushDefers` exists beside `LeaveDeferScope`.
//!
//! Every measurement below is a DELTA across a collection, so it asserts only
//! "the objects that scope bound are gone" and never anything about object sizes
//! or about what the rest of the program happens to be holding. The classes the
//! delta tests allocate are all-scalar on purpose: a `String` field would also
//! allocate its contents, which the string tables keep alive, and that would land
//! in the delta having nothing to do with roots.
//!
//! A `for` needs a second close of its own: the desugaring hoists the iterable
//! into a synthetic `let` that outlives every iteration, and no source scope
//! holds it, so the loop itself has to drop that root when it ends.
//!
//! A scope has more than one way out. A recovered panic resumes AFTER the scope,
//! having run its `defer`s, so the close belongs in the block both that path and
//! the fallthrough reach. And two constructs own roots the source never named:
//! a `for` holds its hoisted iterable, and a `match` lowered as a block graph
//! holds its scrutinee temp and each arm's pattern bindings.
//!
//! 38 perspectives:
//!   1 a `while` body                     20 an `Array` binding
//!   2 a `for` body over a range          21 an enum payload binding
//!   3 a `for` body over an array         22 an async body's frame locals
//!   4 an `if` branch                     23 a `lock` section in a loop
//!   5 an `else` branch                   24 the shapes are still lowered
//!   6 an inner scope leaves the outer    25 alloc stress
//!   7 `break` drops what it abandons     26 minor stress
//!   8 `continue` drops it each time      27 a release build
//!   9 `return` hands the value back      28 the example runs
//!  10 a value that escapes survives      29 the example is fully lowered
//!  11 a shadowed outer binding lives     30 the example under GC stress
//!  12 an array still reaches it          31 the `for` iterable temp
//!  13 a `match` arm's scope              32 `break` out of a `for`
//!  14 the scope's `defer` reads it       33 a `for` over a range value
//!  15 two bindings, both dropped         34 a recovered panic drops it too
//!  16 nested loops, both levels          35 a `match` scrutinee temp
//!  17 a scalar local is untouched        36 an arm's pattern binding
//!  18 an address-taken scalar too        37 an async frame word
//!  19 a `String` binding                 38 those shapes under alloc stress

use super::support::{
    compile_and_run_gc_stress_mode, compile_and_run_release, compile_and_run_with_env,
    compile_file_and_run, compile_with_compiler_env,
};

const AST: [(&str, &str); 1] = [("WILLOW_LIR_BACKEND", "0")];
const LIR: [(&str, &str); 2] = [("WILLOW_LIR_BACKEND", "1"), ("WILLOW_LIR_REQUIRE", "1")];
const LOG: [(&str, &str); 3] = [
    ("WILLOW_LIR_BACKEND", "1"),
    ("WILLOW_LIR_REQUIRE", "1"),
    ("WILLOW_LIR_LOG", "1"),
];

/// The class the delta tests allocate. All-scalar, so allocating one adds
/// exactly one object and nothing else to the heap.
const CELL: &str = "class Cell {
    pub init(self, id: i64) { self.id = id; }
    id: i64;
    pub fn id(self) -> i64 { return self.id; }
}
";

/// A class with a heap field, for the perspectives that watch a value SURVIVE:
/// the text it prints is the evidence that the object was not collected.
const LABEL: &str = "class Label {
    pub init(self, text: String) { self.text = text; }
    text: String;
    pub fn text(self) -> String { return self.text; }
}
";

/// The same program under the AST emitter and under the walker must print
/// `expected`, and the walker side runs with `WILLOW_LIR_REQUIRE=1` so a silent
/// fallback is a compile error rather than a comparison of the AST emitter
/// against itself.
fn assert_both_backends(source: &str, expected: &str) {
    for env in [&AST[..], &LIR[..]] {
        let (out, ok) = compile_and_run_with_env(source, env);
        assert!(ok, "run failed under {env:?}: {out}");
        assert_eq!(out, expected, "wrong output under {env:?}");
    }
}

/// For a measurement whose exact value depends on an allocator detail rather
/// than on rooting: the two emitters still have to answer identically.
fn assert_backends_agree(source: &str) -> String {
    let (ast, ok) = compile_and_run_with_env(source, &AST);
    assert!(ok, "run failed under the AST emitter: {ast}");
    let (lir, ok) = compile_and_run_with_env(source, &LIR);
    assert!(ok, "run failed under the walker: {lir}");
    assert_eq!(ast, lir, "the two emitters disagree");
    lir
}

/// Compile once with the selection log on and require the walker to have taken
/// each named function. Without this a coverage regression would still print the
/// right answer — from the AST emitter.
fn assert_walker_compiled(source: &str, functions: &[&str]) {
    let (ok, stderr) = compile_with_compiler_env(source, &LOG);
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

/// Every scheduling the stress modes can produce must give the same answer: a
/// slot that is rooted when it should be empty aborts the collector, and one
/// that is empty when it should be rooted frees a live object.
fn assert_survives_gc_stress(source: &str, expected: &str) {
    for mode in ["alloc", "minor"] {
        let (out, ok) = compile_and_run_gc_stress_mode(source, mode);
        assert!(ok, "run failed under WILLOW_GC_STRESS={mode}: {out}");
        assert_eq!(out, expected, "wrong output under WILLOW_GC_STRESS={mode}");
    }
}

// 1. The shape the bug was found in: a `while` body binding one object per
//    iteration. Before the clear the last one stayed reachable for the rest of
//    the function, so this printed a nonzero delta under the walker alone.
#[test]
fn p01_a_while_body_binding_is_dropped_each_iteration() {
    let source = format!(
        "{CELL}fn run(rounds: i64) -> i64 {{
    gc_collect();
    let before = gc_allocated_bytes();
    let mut i = 0;
    while i < rounds {{
        let cell = new Cell(i);
        i = i + 1 + cell.id() - cell.id();
    }}
    gc_collect();
    return gc_allocated_bytes() - before;
}}
fn main() {{
    println(run(50));
}}
"
    );
    assert_both_backends(&source, "0\n");
}

// 2. A `for` over a range is a separate lowering path — its own header,
//    increment and back edge — so its body scope needs its own close.
#[test]
fn p02_a_for_body_over_a_range_is_dropped() {
    let source = format!(
        "{CELL}fn run(rounds: i64) -> i64 {{
    gc_collect();
    let before = gc_allocated_bytes();
    let mut seen = 0;
    for n in 0..rounds {{
        let cell = new Cell(n);
        seen = seen + cell.id();
    }}
    gc_collect();
    return gc_allocated_bytes() - before + seen - seen;
}}
fn main() {{
    println(run(50));
}}
"
    );
    assert_both_backends(&source, "0\n");
}

// 3. A `for` over an array, where the loop also holds a live collection: the
//    array stays rooted, the body's own binding does not.
#[test]
fn p03_a_for_body_over_an_array_is_dropped() {
    let source = format!(
        "import std::collections::Array;
{CELL}fn run(rounds: i64) -> i64 {{
    let seeds: Array<i64> = [1, 2, 3, 4];
    gc_collect();
    let before = gc_allocated_bytes();
    let mut i = 0;
    while i < rounds {{
        for seed in seeds {{
            let cell = new Cell(seed);
            i = i + cell.id() - cell.id();
        }}
        i = i + 1;
    }}
    gc_collect();
    return gc_allocated_bytes() - before;
}}
fn main() {{
    println(run(3));
}}
"
    );
    assert_both_backends(&source, "0\n");
}

// 4. A conditional branch is a lexical scope of its own, entered on some paths
//    and not on others.
#[test]
fn p04_an_if_branch_scope_is_dropped() {
    let source = format!(
        "{CELL}fn run(rounds: i64) -> i64 {{
    gc_collect();
    let before = gc_allocated_bytes();
    let mut i = 0;
    while i < rounds {{
        if i % 2 == 0 {{
            let cell = new Cell(i);
            i = i + cell.id() - cell.id();
        }}
        i = i + 1;
    }}
    gc_collect();
    return gc_allocated_bytes() - before;
}}
fn main() {{
    println(run(50));
}}
"
    );
    assert_both_backends(&source, "0\n");
}

// 5. And the `else` side, which lowering reaches through a different block.
#[test]
fn p05_an_else_branch_scope_is_dropped() {
    let source = format!(
        "{CELL}fn run(rounds: i64) -> i64 {{
    gc_collect();
    let before = gc_allocated_bytes();
    let mut i = 0;
    while i < rounds {{
        if i > 1000 {{
            let hot = new Cell(i);
            i = i + hot.id() - hot.id();
        }} else {{
            let cold = new Cell(i);
            i = i + cold.id() - cold.id();
        }}
        i = i + 1;
    }}
    gc_collect();
    return gc_allocated_bytes() - before;
}}
fn main() {{
    println(run(50));
}}
"
    );
    assert_both_backends(&source, "0\n");
}

// 6. Scopes nest, and the marks nest with them: closing the inner one must not
//    drop the root of a binding the outer one still owns. The outer object is
//    read after a collection that happens once the inner scope has closed.
#[test]
fn p06_an_inner_scope_leaves_the_one_around_it_rooted() {
    let source = format!(
        "{LABEL}fn run() -> String {{
    let outer = new Label(\"outer\");
    if true {{
        let inner = new Label(\"inner\");
        println(inner.text());
    }}
    gc_collect();
    return outer.text();
}}
fn main() {{
    println(run());
}}
"
    );
    assert_both_backends(&source, "inner\nouter\n");
}

// 7. `break` jumps straight out of the loop body scope without passing its
//    fallthrough close, so the exit has to drop the roots itself.
#[test]
fn p07_break_drops_the_binding_it_abandons() {
    let source = format!(
        "{CELL}fn run(rounds: i64) -> i64 {{
    gc_collect();
    let before = gc_allocated_bytes();
    let mut i = 0;
    while i < rounds {{
        let cell = new Cell(i);
        if cell.id() == 1 {{
            break;
        }}
        i = i + 1;
    }}
    gc_collect();
    return gc_allocated_bytes() - before;
}}
fn main() {{
    println(run(50));
}}
"
    );
    assert_both_backends(&source, "0\n");
}

// 8. `continue` leaves the same way, once per iteration rather than once.
#[test]
fn p08_continue_drops_the_binding_every_iteration() {
    let source = format!(
        "{CELL}fn run(rounds: i64) -> i64 {{
    gc_collect();
    let before = gc_allocated_bytes();
    let mut i = 0;
    while i < rounds {{
        i = i + 1;
        let cell = new Cell(i);
        if cell.id() % 2 == 0 {{
            continue;
        }}
    }}
    gc_collect();
    return gc_allocated_bytes() - before;
}}
fn main() {{
    println(run(50));
}}
"
    );
    assert_both_backends(&source, "0\n");
}

// 9. `return` needs no clear of its own — the emitter pops every root there —
//    and the value it hands back has to survive a collection in the caller.
#[test]
fn p09_return_hands_the_value_back_alive() {
    let source = format!(
        "{CELL}fn make(id: i64) -> Cell {{
    let cell = new Cell(id);
    return cell;
}}
fn main() {{
    let kept = make(7);
    gc_collect();
    println(kept.id());
}}
"
    );
    assert_both_backends(&source, "7\n");
}

// 10. A value assigned out to an enclosing binding is still rooted — by the
//     slot of the variable that now holds it — so the clear must not free it.
#[test]
fn p10_a_value_that_escapes_the_scope_survives() {
    let source = format!(
        "{LABEL}fn run(rounds: i64) -> String {{
    let mut kept = new Label(\"replaced\");
    let mut i = 0;
    while i < rounds {{
        let label = new Label(\"escaped\");
        kept = label;
        i = i + 1;
    }}
    gc_collect();
    return kept.text();
}}
fn main() {{
    println(run(3));
}}
"
    );
    assert_both_backends(&source, "escaped\n");
}

// 11. A shadowing inner binding is alpha-renamed by HIR lowering, so it has a
//     slot of its own: clearing it leaves the outer name's object rooted.
#[test]
fn p11_a_shadowing_inner_binding_leaves_the_outer_one_rooted() {
    let source = format!(
        "{LABEL}fn run(rounds: i64) -> String {{
    let label = new Label(\"outer\");
    let mut i = 0;
    while i < rounds {{
        let label = new Label(\"inner\");
        println(label.text());
        i = i + 1;
    }}
    gc_collect();
    return label.text();
}}
fn main() {{
    println(run(2));
}}
"
    );
    assert_both_backends(&source, "inner\ninner\nouter\n");
}

// 12. Dropping the binding's root does not free an object something else still
//     reaches: the array keeps every element it was handed.
#[test]
fn p12_an_object_the_array_still_reaches_survives() {
    let source = format!(
        "import std::collections::Array;
{CELL}fn run(rounds: i64) -> i64 {{
    let kept: Array<Cell> = [];
    let mut i = 0;
    while i < rounds {{
        let cell = new Cell(i);
        kept.push(cell);
        i = i + 1;
    }}
    gc_collect();
    let mut total = 0;
    for cell in kept {{
        total = total + cell.id();
    }}
    return total;
}}
fn main() {{
    println(run(4));
}}
"
    );
    assert_both_backends(&source, "6\n");
}

// 13. A `match` arm body is its own scope, lowered into its own block, and its
//     pattern binding lives only inside it.
#[test]
fn p13_a_match_arm_scope_is_dropped() {
    let source = format!(
        "{CELL}enum Tag {{ Bare, Boxed(Cell) }}
fn tag(n: i64) -> Tag {{
    if n % 2 == 0 {{
        return Tag::Bare;
    }}
    return Tag::Boxed(new Cell(n));
}}
fn run(rounds: i64) -> i64 {{
    gc_collect();
    let before = gc_allocated_bytes();
    let mut i = 0;
    let mut seen = 0;
    while i < rounds {{
        match tag(i) {{
            Tag::Bare => {{
                let cell = new Cell(i);
                seen = seen + cell.id();
            }}
            Tag::Boxed(cell) => {{
                seen = seen + cell.id();
            }}
        }}
        i = i + 1;
    }}
    gc_collect();
    return gc_allocated_bytes() - before + seen - seen;
}}
fn main() {{
    println(run(6));
}}
"
    );
    assert_both_backends(&source, "0\n");
}

// 14. The scope's own `defer`s run BEFORE its roots are dropped: a deferred body
//     may read exactly the bindings the close is about to release.
#[test]
fn p14_the_scopes_own_defer_still_reads_the_binding() {
    let source = format!(
        "{CELL}fn run(rounds: i64) -> i64 {{
    gc_collect();
    let before = gc_allocated_bytes();
    let mut i = 0;
    while i < rounds {{
        let cell = new Cell(i);
        defer println(100 + cell.id());
        i = i + 1;
    }}
    gc_collect();
    return gc_allocated_bytes() - before;
}}
fn main() {{
    println(run(3));
}}
"
    );
    assert_both_backends(&source, "100\n101\n102\n0\n");
}

// 15. One close names every local the scope declared, not just the last one.
#[test]
fn p15_two_bindings_in_one_scope_are_both_dropped() {
    let source = format!(
        "{CELL}fn run(rounds: i64) -> i64 {{
    gc_collect();
    let before = gc_allocated_bytes();
    let mut i = 0;
    while i < rounds {{
        let first = new Cell(i);
        let second = new Cell(i + 1);
        i = i + 1 + second.id() - first.id() - 1;
    }}
    gc_collect();
    return gc_allocated_bytes() - before;
}}
fn main() {{
    println(run(20));
}}
"
    );
    assert_both_backends(&source, "0\n");
}

// 16. Nested loops: the inner body closes once per inner iteration, the outer
//     once per outer iteration, and neither leaves the other's binding pinned.
#[test]
fn p16_nested_loops_drop_at_both_levels() {
    let source = format!(
        "{CELL}fn run(rounds: i64) -> i64 {{
    gc_collect();
    let before = gc_allocated_bytes();
    let mut i = 0;
    while i < rounds {{
        let outer = new Cell(i);
        let mut j = 0;
        while j < rounds {{
            let inner = new Cell(j);
            j = j + 1 + inner.id() - inner.id();
        }}
        i = i + 1 + outer.id() - outer.id();
    }}
    gc_collect();
    return gc_allocated_bytes() - before;
}}
fn main() {{
    println(run(8));
}}
"
    );
    assert_both_backends(&source, "0\n");
}

// 17. The close names every SOURCE local the scope declared, GC-managed or not.
//     A scalar holds no root, so the emitter has to leave its storage alone
//     rather than zero it.
#[test]
fn p17_a_scalar_local_in_the_scope_is_untouched() {
    let source = format!(
        "{CELL}fn run(rounds: i64) -> i64 {{
    let mut total = 0;
    let mut i = 0;
    while i < rounds {{
        let step = i * 2;
        let cell = new Cell(step);
        total = total + step + cell.id();
        i = i + 1;
    }}
    return total;
}}
fn main() {{
    println(run(4));
}}
"
    );
    assert_both_backends(&source, "24\n");
}

// 18. An address-taken scalar gets a real stack slot, which is what a rooted
//     binding gets too. Only the GC-managed ones may be cleared.
#[test]
fn p18_an_address_taken_scalar_is_not_zeroed() {
    let source = format!(
        "{CELL}fn bump(slot: &mut i64, by: i64) {{
    slot = slot + by;
}}
fn run(rounds: i64) -> i64 {{
    let mut total = 0;
    let mut i = 0;
    while i < rounds {{
        let mut step = i;
        bump(&step, 10);
        let cell = new Cell(step);
        total = total + cell.id();
        i = i + 1;
    }}
    return total;
}}
fn main() {{
    println(run(4));
}}
"
    );
    assert_both_backends(&source, "46\n");
}

// 19. A `String` binding is GC-managed too. The delta is not zero here — the
//     string tables keep the contents of what was built — but it is an
//     allocator detail, so what this pins down is that both emitters answer the
//     same way.
#[test]
fn p19_a_string_binding_agrees_between_emitters() {
    let source = "fn run(rounds: i64) -> i64 {
    gc_collect();
    let before = gc_allocated_bytes();
    let mut i = 0;
    while i < rounds {
        let text = \"row \" + i.toString();
        println(text);
        i = i + 1;
    }
    gc_collect();
    return gc_allocated_bytes() - before;
}
fn main() {
    println(run(3));
}
";
    let out = assert_backends_agree(source);
    assert!(
        out.starts_with("row 0\nrow 1\nrow 2\n"),
        "unexpected output: {out}"
    );
}

// 20. An `Array` binding is one object plus its storage, and both go.
#[test]
fn p20_an_array_binding_is_dropped() {
    let source = "import std::collections::Array;
fn run(rounds: i64) -> i64 {
    gc_collect();
    let before = gc_allocated_bytes();
    let mut i = 0;
    while i < rounds {
        let xs: Array<i64> = [i, i + 1, i + 2];
        i = i + xs.len() - 2;
    }
    gc_collect();
    return gc_allocated_bytes() - before;
}
fn main() {
    println(run(20));
}
";
    assert_both_backends(source, "0\n");
}

// 21. A payload-carrying enum value is a heap object, so its binding is a root
//     like any other.
#[test]
fn p21_an_enum_payload_binding_is_dropped() {
    let source = format!(
        "{CELL}enum Tag {{ Bare, Boxed(Cell) }}
fn run(rounds: i64) -> i64 {{
    gc_collect();
    let before = gc_allocated_bytes();
    let mut i = 0;
    while i < rounds {{
        let tag = Tag::Boxed(new Cell(i));
        match tag {{
            Tag::Bare => {{ i = i + 1; }}
            Tag::Boxed(cell) => {{ i = i + 1 + cell.id() - cell.id(); }}
        }}
    }}
    gc_collect();
    return gc_allocated_bytes() - before;
}}
fn main() {{
    println(run(20));
}}
"
    );
    assert_both_backends(&source, "0\n");
}

// 22. An async body's locals live in the heap async frame, which the frame's own
//     map traces — not in a stack slot on the shadow stack. The close must pass
//     over them rather than write into the frame.
#[test]
fn p22_an_async_bodys_frame_locals_are_left_alone() {
    let source = format!(
        "{CELL}async fn work(rounds: i64) -> i64 {{
    let mut i = 0;
    let mut total = 0;
    while i < rounds {{
        let cell = new Cell(i);
        await sleep(1);
        total = total + cell.id();
        i = i + 1;
    }}
    gc_collect();
    return total;
}}
async fn main() {{
    println(await work(4));
}}
"
    );
    assert_both_backends(&source, "6\n");
}

// 23. A `lock` section is a scope with a lock to hand back, so its close already
//     carried a release. The root drop joins it without disturbing the section.
#[test]
fn p23_a_lock_section_in_a_loop_is_dropped() {
    let source = format!(
        "{CELL}async fn run(m: Mutex<i64>, rounds: i64) -> i64 {{
    let mut total = 0;
    let mut i = 0;
    while i < rounds {{
        lock m as mut value {{
            let cell = new Cell(i);
            value = value + cell.id();
            total = value;
        }}
        i = i + 1;
    }}
    gc_collect();
    return total;
}}
async fn main() {{
    let m: Mutex<i64> = Mutex::new(0);
    println(await run(m, 5));
}}
"
    );
    assert_both_backends(&source, "10\n");
}

// 24. The new instruction must not take any of these shapes out of the walker's
//     subset: an instruction it cannot emit would be a silent fallback, and the
//     answers above would then be the AST emitter's twice over.
#[test]
fn p24_the_shapes_are_still_lowered() {
    let source = format!(
        "{CELL}fn looped(rounds: i64) -> i64 {{
    let mut total = 0;
    let mut i = 0;
    while i < rounds {{
        let cell = new Cell(i);
        if cell.id() == 3 {{
            break;
        }}
        if cell.id() % 2 == 0 {{
            i = i + 1;
            continue;
        }}
        total = total + cell.id();
        i = i + 1;
    }}
    return total;
}}
fn ranged(rounds: i64) -> i64 {{
    let mut total = 0;
    for n in 0..rounds {{
        let cell = new Cell(n);
        total = total + cell.id();
    }}
    return total;
}}
fn main() {{
    println(looped(10));
    println(ranged(4));
}}
"
    );
    assert_both_backends(&source, "1\n6\n");
    assert_walker_compiled(&source, &["looped", "ranged", "main"]);
}

// 25. Under allocation stress every allocation collects, so a slot that is
//     cleared too early frees a live object and a slot that keeps a stale
//     pointer is a dangling root.
#[test]
fn p25_alloc_stress() {
    let source = format!(
        "{CELL}{LABEL}fn run(rounds: i64) -> String {{
    let kept = new Label(\"survivor\");
    let mut i = 0;
    while i < rounds {{
        let cell = new Cell(i);
        if cell.id() == 5 {{
            i = i + 1;
            continue;
        }}
        let label = new Label(\"transient\");
        println(label.text());
        i = i + 1;
    }}
    gc_collect();
    return kept.text();
}}
fn main() {{
    println(run(3));
}}
"
    );
    assert_survives_gc_stress(&source, "transient\ntransient\ntransient\nsurvivor\n");
}

// 26. And under minor-collection stress, where surviving objects move.
#[test]
fn p26_minor_stress() {
    let source = format!(
        "import std::collections::Array;
{CELL}fn run(rounds: i64) -> i64 {{
    let kept: Array<Cell> = [];
    let mut i = 0;
    while i < rounds {{
        let cell = new Cell(i);
        if cell.id() % 2 == 0 {{
            kept.push(cell);
        }}
        i = i + 1;
    }}
    gc_collect();
    let mut total = 0;
    for cell in kept {{
        total = total + cell.id();
    }}
    return total;
}}
fn main() {{
    println(run(10));
}}
"
    );
    assert_survives_gc_stress(&source, "20\n");
}

// 27. A release build, where the optimizer sees the slot stores: a null store
//     into a slot the collector reads must not be sunk past the collection.
#[test]
fn p27_release_build() {
    let source = format!(
        "{CELL}fn run(rounds: i64) -> i64 {{
    gc_collect();
    let before = gc_allocated_bytes();
    let mut i = 0;
    while i < rounds {{
        let cell = new Cell(i);
        i = i + 1 + cell.id() - cell.id();
    }}
    gc_collect();
    return gc_allocated_bytes() - before;
}}
fn main() {{
    println(run(50));
}}
"
    );
    let (out, ok) = compile_and_run_release(&source);
    assert!(ok, "release run failed: {out}");
    assert_eq!(out, "0\n");
}

const EXAMPLE_OUTPUT: &str = "0\n0\n0\n0\n0\n0\n0\n100\n101\n0\n6\nescapes into `kept`\ninner: the inner label\ninner: the inner label\nthe outer label\n";

// 28. The example program runs and prints what its comments claim.
#[test]
fn p28_the_example_runs() {
    let (out, ok) = compile_file_and_run("example/lir_scope_roots.wi");
    assert!(ok, "example run failed: {out}");
    assert_eq!(out, EXAMPLE_OUTPUT);
}

// 29. ...and every function in it is compiled from lowered IR, so the example
//     documents the walker's behaviour rather than the AST emitter's.
#[test]
fn p29_the_example_is_fully_lowered() {
    let source =
        std::fs::read_to_string("example/lir_scope_roots.wi").expect("example is readable");
    assert_both_backends(&source, EXAMPLE_OUTPUT);
    assert_walker_compiled(
        &source,
        &[
            "while_body",
            "for_body",
            "hoisted_iterable",
            "early_break",
            "early_continue",
            "matched",
            "divide",
            "warm",
            "recovered",
            "deferred",
            "reachable_from_an_array",
            "escapes",
            "shadowed",
            "main",
        ],
    );
}

// 30. And it holds under both stress schedules.
#[test]
fn p30_the_example_under_gc_stress() {
    let source =
        std::fs::read_to_string("example/lir_scope_roots.wi").expect("example is readable");
    assert_survives_gc_stress(&source, EXAMPLE_OUTPUT);
}

// 31. The iterable a `for` hoists. `for x in arr` desugars to a synthetic `let`
//     holding the whole array plus an index, and that `let` belongs to no source
//     scope: only the loop's own close drops it, and until it does the array —
//     and every object in it — outlives the scope that built it.
#[test]
fn p31_the_iterable_a_for_hoists_is_dropped() {
    let source = format!(
        "import std::collections::Array;
{CELL}fn run(rounds: i64) -> i64 {{
    gc_collect();
    let before = gc_allocated_bytes();
    let mut i = 0;
    while i < rounds {{
        let src: Array<Cell> = [new Cell(1), new Cell(2)];
        let mut seen = 0;
        for cell in src {{
            seen = seen + cell.id();
        }}
        i = i + 1 + seen - seen;
    }}
    gc_collect();
    return gc_allocated_bytes() - before;
}}
fn main() {{
    println(run(4));
}}
"
    );
    assert_both_backends(&source, "0\n");
}

// 32. `break` leaves the loop by the same block the header falls through to, so
//     the iterable's root is dropped once, on the one path out — not twice, and
//     not never.
#[test]
fn p32_break_out_of_a_for_drops_the_iterable() {
    let source = format!(
        "import std::collections::Array;
{CELL}fn run(rounds: i64) -> i64 {{
    gc_collect();
    let before = gc_allocated_bytes();
    let mut i = 0;
    while i < rounds {{
        let src: Array<Cell> = [new Cell(1), new Cell(2), new Cell(3)];
        for cell in src {{
            if cell.id() == 2 {{
                break;
            }}
        }}
        i = i + 1;
    }}
    gc_collect();
    return gc_allocated_bytes() - before;
}}
fn main() {{
    println(run(4));
}}
"
    );
    assert_both_backends(&source, "0\n");
}

// 33. A range held as a value is hoisted the same way, so the close has to reach
//     that temp too — and the body's own binding still goes with the iteration.
#[test]
fn p33_a_for_over_a_range_value_drops_both() {
    let source = format!(
        "{CELL}fn run(rounds: i64) -> i64 {{
    gc_collect();
    let before = gc_allocated_bytes();
    let mut i = 0;
    while i < rounds {{
        let r = 0..3;
        let mut seen = 0;
        for k in r {{
            let cell = new Cell(k);
            seen = seen + cell.id();
        }}
        i = i + 1 + seen - seen;
    }}
    gc_collect();
    return gc_allocated_bytes() - before;
}}
fn main() {{
    println(run(4));
}}
"
    );
    assert_both_backends(&source, "0\n");
}

// 34. A recovered panic is a SECOND way out of a scope: the runtime resumes
//     after the scope, having run its `defer`s, without ever passing the
//     fallthrough close. A clear placed on the fallthrough alone would leave the
//     binding rooted on exactly the path that took it.
#[test]
fn p34_a_recovered_panic_drops_the_scopes_binding() {
    let source = format!(
        "{CELL}fn divide(a: i64, b: i64) -> i64 {{
    return a / b;
}}
fn warm() {{
    defer match recover() {{
        Some(info) => {{}}
        None => {{}}
    }}
    println(divide(1, 0));
}}
fn run(rounds: i64) -> i64 {{
    gc_collect();
    let before = gc_allocated_bytes();
    let mut i = 0;
    while i < rounds {{
        defer match recover() {{
            Some(info) => {{}}
            None => {{}}
        }}
        i = i + 1;
        let cell = new Cell(i);
        println(divide(cell.id(), cell.id() - i));
    }}
    gc_collect();
    return gc_allocated_bytes() - before;
}}
fn main() {{
    warm();
    println(run(6));
}}
"
    );
    assert_both_backends(&source, "0\n");
}

// 35. The scrutinee of a `match` lowered as a block graph lives in a temp that
//     outlives every arm and that no source scope declared. The construct closes
//     at the merge every arm reaches, which is the one place all of them meet.
#[test]
fn p35_a_match_scrutinee_temp_is_dropped_at_the_merge() {
    let source = format!(
        "{CELL}enum Tag {{
    Bare,
    Boxed(Cell)
}}
fn run(rounds: i64) -> i64 {{
    gc_collect();
    let before = gc_allocated_bytes();
    let mut i = 0;
    while i < rounds {{
        let mut acc = 0;
        match Tag::Boxed(new Cell(i)) {{
            Tag::Boxed(c) => {{
                let mut k = 0;
                while k < 1 {{
                    acc = acc + c.id();
                    k = k + 1;
                }}
            }}
            Tag::Bare => {{
                acc = acc + 1;
            }}
        }}
        i = i + 1 + acc - acc;
    }}
    gc_collect();
    return gc_allocated_bytes() - before;
}}
fn main() {{
    println(run(8));
}}
"
    );
    assert_both_backends(&source, "0\n");
}

// 36. An arm's pattern bindings are declared ahead of the arm body's own scope,
//     so the body's close does not reach them: the arm is a scope of its own and
//     drops them where it ends.
#[test]
fn p36_a_match_arms_pattern_binding_is_dropped() {
    let source = format!(
        "{CELL}enum Tag {{
    Bare,
    Boxed(Cell)
}}
fn run(rounds: i64) -> i64 {{
    gc_collect();
    let before = gc_allocated_bytes();
    let mut i = 0;
    let mut acc = 0;
    while i < rounds {{
        let tag = Tag::Boxed(new Cell(i));
        match tag {{
            Tag::Boxed(held) => {{
                acc = acc + held.id();
            }}
            Tag::Bare => {{
                acc = acc + 1;
            }}
        }}
        i = i + 1;
    }}
    gc_collect();
    return gc_allocated_bytes() - before + acc - acc;
}}
fn main() {{
    println(run(8));
}}
"
    );
    assert_both_backends(&source, "0\n");
}

// 37. An async local that is live across an `await` is not on the stack at all:
//     it lives in the task's heap frame, which the frame's own map traces. The
//     close has to null that frame word too, or the object stays traced until
//     the whole task completes. Asserted under the walker alone — the AST
//     emitter keeps frame words live for the task's lifetime, which is the
//     behaviour the walker is replacing.
#[test]
fn p37_an_async_frame_local_is_dropped_at_the_scope_end() {
    let source = format!(
        "{CELL}async fn run(rounds: i64) -> i64 {{
    gc_collect();
    let before = gc_allocated_bytes();
    let mut i = 0;
    let mut total = 0;
    while i < rounds {{
        let cell = new Cell(i);
        await sleep(1);
        total = total + cell.id();
        i = i + 1;
    }}
    gc_collect();
    return gc_allocated_bytes() - before + total - total;
}}
async fn main() {{
    println(await run(6));
}}
"
    );
    let (out, ok) = compile_and_run_with_env(&source, &LIR);
    assert!(ok, "run failed under the walker: {out}");
    assert_eq!(out, "0\n");
}

// 38. The three shapes above under allocation stress, where a slot cleared too
//     early frees a live object and a stale one is a dangling root — and where
//     the recovery path allocates on every round.
#[test]
fn p38_the_match_and_recovery_shapes_survive_alloc_stress() {
    let source = format!(
        "{CELL}{LABEL}enum Tag {{
    Bare,
    Boxed(Cell)
}}
fn divide(a: i64, b: i64) -> i64 {{
    return a / b;
}}
fn run(rounds: i64) -> String {{
    let kept = new Label(\"survivor\");
    let mut i = 0;
    let mut acc = 0;
    while i < rounds {{
        defer match recover() {{
            Some(info) => {{}}
            None => {{}}
        }}
        let tag = Tag::Boxed(new Cell(i));
        match tag {{
            Tag::Boxed(held) => {{
                acc = acc + held.id();
            }}
            Tag::Bare => {{
                acc = acc + 1;
            }}
        }}
        i = i + 1;
        println(divide(acc, i - i));
    }}
    gc_collect();
    return kept.text();
}}
fn main() {{
    println(run(4));
}}
"
    );
    let (out, ok) = compile_and_run_gc_stress_mode(&source, "alloc");
    assert!(ok, "run failed under WILLOW_GC_STRESS=alloc: {out}");
    assert_eq!(out, "survivor\n");
}
