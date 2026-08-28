//! A suspension inside a `match` arm, compiled from Lowered IR
//! (willow-0g8j.2.11.1).
//!
//! A `match` lowered to ONE LIR instruction holding its arms as an HIR tree, so
//! an `await` or a channel operation inside an arm had nowhere to become a block
//! boundary. The cooperative pass builds resume points out of the block graph,
//! found none, and the whole function fell back to the AST async emitter — which
//! has no `match` handling at all, and so could not compile the shapes below
//! either. Lowering now splits such a `match` into a dispatch chain of pattern
//! tests and two-way branches, one block per arm, all joining at a merge.
//!
//! The split is taken only when an arm actually suspends: `lir_match_bodies.rs`
//! covers the single-instruction form that everything else still uses, and
//! perspectives 20 and 21 here pin down that it stays that way.
//!
//! Every test is differential — the same program under the AST emitter and under
//! the walker must print the same thing — and the walker side sets
//! `WILLOW_LIR_REQUIRE=1`, so a silent fallback is a compile error rather than a
//! comparison of the AST emitter against itself. Several also run under a
//! one-safepoint task budget (preemption between every safepoint) and under
//! `WILLOW_GC_STRESS=alloc` (a collection at every allocation), because an arm
//! binding that the frame forgot is only *at risk* under the default settings
//! and is actually lost under those.
//!
//! 25 perspectives:
//!   1 arm awaits and returns              14 suspending scrutinee AND arm
//!   2 only one arm suspends               15 defer inside a suspending arm
//!   3 first matching arm wins             16 match with await inside a loop
//!   4 wildcard arm suspends               17 sibling arms reuse a name
//!   5 catch-all binding arm suspends      18 arm-local shadows an outer name
//!   6 payload binding feeds the await     19 class downcast arm suspends
//!   7 two awaits, binding live across     20 non-suspending match unchanged
//!   8 Option niche payload                21 sync match unchanged
//!   9 GC binding read after the park      22 the arm really parks (rendezvous)
//!  10 GC value allocated in the arm       23 E0811/E2606 still reject
//!  11 void statement-position match       24 suspending ternary condition
//!  12 nested match inside an arm          25 the example is fully LIR
//!  13 channel send/recv inside an arm

use super::support::{compile_and_run_with_env, compile_error_stderr, compile_with_compiler_env};

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

const SHAPE: &str = "enum Shape { Circle(i64), Rect(i64, i64), Empty }\n";
const SCALED: &str = "async fn scaled(n: i64) -> i64 { await yield(); return n * 3; }\n";

/// `expected` must come out of all four configurations, and each of `functions`
/// must be named in the walker's selection log.
fn assert_suspends(source: &str, expected: &str, functions: &[&str]) {
    for env in [&AST[..], &LIR[..], &LIR_BUDGET[..], &LIR_STRESS[..]] {
        let (out, ok) = compile_and_run_with_env(source, env);
        assert!(ok, "run failed under {env:?}: {out}");
        assert_eq!(out, expected, "wrong output under {env:?}");
    }
    assert_walker_compiled(source, functions);
}

/// Compile once with the selection log on and require the walker to have taken
/// each named function. Without this a coverage regression would still print the
/// right answer — from the AST emitter.
fn assert_walker_compiled(source: &str, functions: &[&str]) {
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

// 1. The core shape. Each arm is a block that awaits and returns; before the
//    split, the `await` had no block boundary to become and `route` went to the
//    AST emitter, which cannot compile a `match` at all.
#[test]
fn lir_suspend_01_arm_awaits_and_returns() {
    assert_suspends(
        &format!(
            "{SCALED}
async fn route(which: i64) -> i64 {{
    match which {{
        1 => {{ return await scaled(10); }}
        2 => {{ return await scaled(20); }}
        _ => {{ return 0; }}
    }}
}}
async fn main() {{
    println(await route(1));
    println(await route(2));
    println(await route(9));
}}
"
        ),
        "30\n60\n0\n",
        &["route", "main"],
    );
}

// 2. Only one arm suspends. The others must stay straight-line rather than
//    growing suspend edges of their own, and the merge must be reachable from
//    all three.
#[test]
fn lir_suspend_02_one_arm_of_three_suspends() {
    assert_suspends(
        &format!(
            "{SCALED}
async fn pick(which: i64) -> i64 {{
    let mut out = 0;
    match which {{
        1 => {{ out = 1; }}
        2 => {{ out = await scaled(5); }}
        _ => {{ out = 3; }}
    }}
    return out;
}}
async fn main() {{
    println(await pick(1));
    println(await pick(2));
    println(await pick(7));
}}
"
        ),
        "1\n15\n3\n",
        &["pick", "main"],
    );
}

// 3. Arm order. The dispatch chain tests arms in source order and stops at the
//    first match, so an overlapping later arm must never run.
#[test]
fn lir_suspend_03_first_matching_arm_wins() {
    assert_suspends(
        &format!(
            "{SCALED}
async fn first(n: i64) -> i64 {{
    match n {{
        1 => {{ return await scaled(1); }}
        1 => {{ return await scaled(99); }}
        _ => {{ return await scaled(2); }}
    }}
}}
async fn main() {{
    println(await first(1));
    println(await first(4));
}}
"
        ),
        "3\n6\n",
        &["first", "main"],
    );
}

// 4. A trailing wildcard arm is the else edge of the last test. It suspends
//    here, so the chain's fall-through target has to be a real arm block rather
//    than the merge.
#[test]
fn lir_suspend_04_wildcard_arm_suspends() {
    assert_suspends(
        &format!(
            "{SCALED}
async fn fallback(n: i64) -> i64 {{
    match n {{
        1 => {{ return 100; }}
        _ => {{ return await scaled(n); }}
    }}
}}
async fn main() {{
    println(await fallback(1));
    println(await fallback(4));
}}
"
        ),
        "100\n12\n",
        &["fallback", "main"],
    );
}

// 5. A catch-all BINDING arm always matches, so no test is emitted at all --
//    just a jump into the arm and a bind of the whole scrutinee, which the arm
//    then carries across its suspension.
#[test]
fn lir_suspend_05_catch_all_binding_arm_suspends() {
    assert_suspends(
        &format!(
            "{SCALED}
async fn passthrough(n: i64) -> i64 {{
    match n {{
        bound => {{
            let result = await scaled(bound);
            return result + bound;
        }}
    }}
}}
async fn main() {{
    println(await passthrough(5));
    println(await passthrough(0));
}}
"
        ),
        "20\n0\n",
        &["passthrough", "main"],
    );
}

// 6. An enum payload binding feeds the suspension. The bind happens in the arm's
//    own block, after the test that proved the variant.
#[test]
fn lir_suspend_06_payload_binding_feeds_the_await() {
    assert_suspends(
        &format!(
            "{SHAPE}{SCALED}
async fn area(shape: Shape) -> i64 {{
    match shape {{
        Shape::Circle(r) => {{ return await scaled(r); }}
        Shape::Rect(w, h) => {{ return await scaled(w + h); }}
        Shape::Empty => {{ return 0; }}
    }}
}}
async fn main() {{
    println(await area(Shape::Circle(4)));
    println(await area(Shape::Rect(2, 5)));
    println(await area(Shape::Empty));
}}
"
        ),
        "12\n21\n0\n",
        &["area", "main"],
    );
}

// 7. Two suspensions in one arm, with the second payload binding read only after
//    the first. `h` is live across a park and has to reach the async frame; a
//    binding left in a register would come back as whatever the resume left
//    there.
#[test]
fn lir_suspend_07_binding_live_across_a_suspension() {
    assert_suspends(
        &format!(
            "{SHAPE}{SCALED}
async fn area(shape: Shape) -> i64 {{
    match shape {{
        Shape::Rect(w, h) => {{
            let a = await scaled(w);
            let b = await scaled(h);
            return a + b;
        }}
        _ => {{ return 0; }}
    }}
}}
async fn main() {{
    println(await area(Shape::Rect(2, 5)));
    println(await area(Shape::Circle(1)));
}}
"
        ),
        "21\n0\n",
        &["area", "main"],
    );
}

// 8. `Option<i64>` uses a niche representation, so `Some(v)` is a bit-pattern
//    test rather than a tag load. The payload extraction has to read the same
//    words the single-instruction form reads.
#[test]
fn lir_suspend_08_option_niche_payload() {
    assert_suspends(
        &format!(
            "{SCALED}
async fn doubled_or(value: Option<i64>) -> i64 {{
    match value {{
        Some(v) => {{ return await scaled(v); }}
        None => {{ return -1; }}
    }}
}}
async fn main() {{
    println(await doubled_or(Some(7)));
    println(await doubled_or(None));
}}
"
        ),
        "21\n-1\n",
        &["doubled_or", "main"],
    );
}

// 9. A GC-managed binding read AFTER the park. Under `WILLOW_GC_STRESS=alloc`
//    the concatenation below collects, so a `text` the frame did not root is
//    freed rather than merely unrooted.
#[test]
fn lir_suspend_09_gc_binding_read_after_the_park() {
    assert_suspends(
        "async fn greet(name: Option<String>) -> String {
    match name {
        Some(text) => {
            await yield();
            return \"hello, \" + text;
        }
        None => { return \"hello, stranger\"; }
    }
}
async fn main() {
    println(await greet(Some(\"willow\")));
    println(await greet(None));
}
",
        "hello, willow\nhello, stranger\n",
        &["greet", "main"],
    );
}

// 10. A GC value ALLOCATED inside the arm and read after the park. The binding
//     is neither a parameter nor a payload, so it exercises the arm block's own
//     rooting rather than the scrutinee's.
#[test]
fn lir_suspend_10_gc_value_allocated_in_the_arm() {
    assert_suspends(
        "async fn label(n: i64) -> String {
    match n {
        1 => {
            let tag = \"tag-\" + n.toString();
            await yield();
            return tag + \"!\";
        }
        _ => { return \"none\"; }
    }
}
async fn main() {
    println(await label(1));
    println(await label(2));
}
",
        "tag-1!\nnone\n",
        &["label", "main"],
    );
}

// 11. A `void` match in statement position: no destination local, so the arms
//     merge with nothing to write. The merge block still has to exist, because
//     the statements after the match live in it.
#[test]
fn lir_suspend_11_void_statement_position_match() {
    assert_suspends(
        &format!(
            "{SCALED}
async fn report(n: i64) {{
    match n {{
        1 => {{
            let doubled = await scaled(2);
            println(doubled);
        }}
        _ => {{ println(0); }}
    }}
    println(\"done\");
}}
async fn main() {{
    await report(1);
    await report(5);
}}
"
        ),
        "6\ndone\n0\ndone\n",
        &["report", "main"],
    );
}

// 12. A match inside an arm of another match. The inner chain is built in the
//     outer arm's block, so both levels need resume points of their own.
#[test]
fn lir_suspend_12_nested_match_inside_an_arm() {
    assert_suspends(
        &format!(
            "{SCALED}
async fn nested(outer: i64, inner: i64) -> i64 {{
    match outer {{
        1 => {{
            match inner {{
                2 => {{ return await scaled(7); }}
                _ => {{ return -2; }}
            }}
        }}
        _ => {{ return -1; }}
    }}
}}
async fn main() {{
    println(await nested(1, 2));
    println(await nested(1, 3));
    println(await nested(9, 9));
}}
"
        ),
        "21\n-2\n-1\n",
        &["nested", "main"],
    );
}

// 13. Channel operations park exactly as an `await` does, and they are the shape
//     E0811 rejects in an arm EXPRESSION -- so an arm BLOCK has to compile them.
#[test]
fn lir_suspend_13_channel_ops_inside_an_arm() {
    assert_suspends(
        "async fn through(n: i64) -> i64 {
    let channel = Channel<i64>::with_capacity(1);
    match n {
        1 => {
            channel.send(41);
            let received = channel.recv();
            return received + 1;
        }
        _ => { return 0; }
    }
}
async fn main() {
    println(await through(1));
    println(await through(2));
}
",
        "42\n0\n",
        &["through", "main"],
    );
}

// 14. The SCRUTINEE suspends as well. It is evaluated into its own local before
//     the first test, so its suspension is cut ahead of the chain and the arm's
//     inside it.
#[test]
fn lir_suspend_14_suspending_scrutinee_and_arm() {
    assert_suspends(
        &format!(
            "{SCALED}
async fn maybe(n: i64) -> Option<i64> {{
    await yield();
    if n > 0 {{ return Some(n); }}
    return None;
}}
async fn both_sides(n: i64) -> i64 {{
    match await maybe(n) {{
        Some(v) => {{ return await scaled(v); }}
        None => {{ return -1; }}
    }}
}}
async fn main() {{
    println(await both_sides(5));
    println(await both_sides(-3));
}}
"
        ),
        "15\n-1\n",
        &["both_sides", "main"],
    );
}

// 15. A `defer` registered inside an arm still runs on the way out, even though
//     the arm parked in between: the registration and the flush are now in
//     different blocks of the same function.
#[test]
fn lir_suspend_15_defer_inside_a_suspending_arm() {
    assert_suspends(
        &format!(
            "{SCALED}
async fn with_cleanup(n: i64) -> i64 {{
    match n {{
        1 => {{
            defer println(\"cleanup\");
            let value = await scaled(4);
            return value;
        }}
        _ => {{ return 0; }}
    }}
}}
async fn main() {{
    println(await with_cleanup(1));
    println(await with_cleanup(2));
}}
"
        ),
        "cleanup\n12\n0\n",
        &["with_cleanup", "main"],
    );
}

// 16. The whole chain inside a loop. The merge block is re-entered once per
//     iteration, and the loop-carried accumulator crosses every suspension.
#[test]
fn lir_suspend_16_match_with_await_inside_a_loop() {
    assert_suspends(
        &format!(
            "{SCALED}
async fn accumulate(rounds: i64) -> i64 {{
    let mut total = 0;
    let mut i = 0;
    while i < rounds {{
        match i {{
            0 => {{
                let value = await scaled(1);
                total = total + value;
            }}
            _ => {{
                let value = await scaled(i);
                total = total + value * 2;
            }}
        }}
        i = i + 1;
    }}
    return total;
}}
async fn main() {{ println(await accumulate(3)); }}
"
        ),
        "21\n",
        &["accumulate", "main"],
    );
}

// 17. Sibling arms binding the same source name. Each arm declares its own LIR
//     local, so the second must not read the first's storage.
#[test]
fn lir_suspend_17_sibling_arms_reuse_a_name() {
    assert_suspends(
        &format!(
            "{SHAPE}{SCALED}
async fn measure(shape: Shape) -> i64 {{
    match shape {{
        Shape::Circle(v) => {{ return await scaled(v); }}
        Shape::Rect(v, other) => {{ return (await scaled(v)) + other; }}
        Shape::Empty => {{ return 0; }}
    }}
}}
async fn main() {{
    println(await measure(Shape::Circle(2)));
    println(await measure(Shape::Rect(3, 1)));
}}
"
        ),
        "6\n10\n",
        &["measure", "main"],
    );
}

// 18. An arm-local shadowing an outer name across a suspension. The outer value
//     has to be intact in the merge block, which means the two names cannot have
//     collapsed onto one frame slot.
#[test]
fn lir_suspend_18_arm_local_shadows_an_outer_name() {
    assert_suspends(
        &format!(
            "{SCALED}
async fn shadowed(n: i64) -> i64 {{
    let value = 100;
    let mut out = 0;
    match n {{
        1 => {{
            let value = await scaled(2);
            out = value;
        }}
        _ => {{ out = -1; }}
    }}
    return out + value;
}}
async fn main() {{
    println(await shadowed(1));
    println(await shadowed(2));
}}
"
        ),
        "106\n99\n",
        &["shadowed", "main"],
    );
}

// 19. A class downcast arm. Its test is a `type_id` comparison and its bind is a
//     pointer the arm then holds across the park, so it must stay rooted as well
//     as correct.
#[test]
fn lir_suspend_19_class_downcast_arm_suspends() {
    assert_suspends(
        "interface Animal extends Sync { fn name(self) -> String; }
class Dog implements Animal {
    pub fn name(self) -> String { return \"rex\"; }
    pub fn bark(self) -> String { return \"woof\"; }
}
class Cat implements Animal {
    pub fn name(self) -> String { return \"mia\"; }
    pub fn meow(self) -> String { return \"meow\"; }
}
class Fish implements Animal { pub fn name(self) -> String { return \"nemo\"; } }
async fn speak(animal: Animal) -> String {
    match animal {
        Dog(d) => {
            await yield();
            return d.name() + \" says \" + d.bark();
        }
        Cat(c) => { return c.name() + \" says \" + c.meow(); }
        _ => { return animal.name() + \" is quiet\"; }
    }
}
async fn main() {
    println(await speak(new Dog()));
    println(await speak(new Cat()));
    println(await speak(new Fish()));
}
",
        "rex says woof\nmia says meow\nnemo is quiet\n",
        &["speak", "main"],
    );
}

// 20. A `match` no arm of which suspends must keep the single-instruction form,
//     even in an async function that suspends elsewhere. The split costs blocks
//     and only pays for itself where a resume point is needed.
#[test]
fn lir_suspend_20_non_suspending_match_unchanged() {
    assert_suspends(
        &format!(
            "{SHAPE}
async fn weigh(shape: Shape) -> i64 {{
    let mut out = 0;
    match shape {{
        Shape::Circle(r) => {{ out = r; }}
        Shape::Rect(w, h) => {{ out = w * h; }}
        Shape::Empty => {{ out = 0; }}
    }}
    await yield();
    return out;
}}
async fn main() {{
    println(await weigh(Shape::Circle(4)));
    println(await weigh(Shape::Rect(2, 5)));
    println(await weigh(Shape::Empty));
}}
"
        ),
        "4\n10\n0\n",
        &["weigh", "main"],
    );
}

// 21. The synchronous form is untouched: a `fn` never suspends, so no `match` in
//     one is ever split however its arms are written.
#[test]
fn lir_suspend_21_sync_match_unchanged() {
    assert_suspends(
        &format!(
            "{SHAPE}
fn weigh(shape: Shape) -> i64 {{
    match shape {{
        Shape::Circle(r) => {{ return r; }}
        Shape::Rect(w, h) => {{ return w * h; }}
        Shape::Empty => {{ return 0; }}
    }}
}}
fn main() {{
    println(weigh(Shape::Circle(4)));
    println(weigh(Shape::Rect(2, 5)));
    println(weigh(Shape::Empty));
}}
"
        ),
        "4\n10\n0\n",
        &["weigh", "main"],
    );
}

// 22. The arm really PARKS rather than blocking its worker. The two tasks
//     rendezvous on an unbuffered channel from inside their arms, which only
//     completes if the sender's arm suspends and lets the receiver run; a
//     blocking arm pins the worker and the program never finishes.
#[test]
fn lir_suspend_22_the_arm_really_parks() {
    assert_suspends(
        "async fn produce(channel: Channel<i64>, n: i64) {
    match n {
        1 => {
            channel.send(7);
            println(\"sent\");
        }
        _ => { println(\"skipped\"); }
    }
}
async fn consume(channel: Channel<i64>, n: i64) -> i64 {
    match n {
        1 => {
            let value = channel.recv();
            return value;
        }
        _ => { return 0; }
    }
}
async fn main() {
    let channel = Channel<i64>::new();
    let producer = produce(channel, 1);
    let consumer = consume(channel, 1);
    await producer;
    println(await consumer);
}
",
        "sent\n7\n",
        &["produce", "consume", "main"],
    );
}

// 23. The two checker rules the split does NOT lift. Both exist because the AST
//     emitter is still the fallback and cannot compile either shape; lifting
//     them belongs with the Stage 5 cutover, not here.
#[test]
fn lir_suspend_23_arm_expression_and_lock_still_rejected() {
    let await_in_arm_expression = "async fn leaf(n: i64) -> i64 { return n; }
async fn pick(n: i64) -> i64 {
    return match n {
        1 => await leaf(10),
        _ => 0,
    };
}
async fn main() { println(await pick(1)); }
";
    let stderr = compile_error_stderr(await_in_arm_expression);
    assert!(
        stderr.contains("E0811"),
        "a suspending arm EXPRESSION must still be rejected: {stderr}"
    );

    let lock_in_arm = "async fn apply(account: Mutex<i64>, n: i64) -> i64 {
    match n {
        1 => { lock account as mut balance { balance = balance + 1; } return 1; }
        _ => { return 0; }
    }
}
async fn main() { println(await apply(Mutex::new(0), 1)); }
";
    let stderr = compile_error_stderr(lock_in_arm);
    assert!(
        stderr.contains("E2606"),
        "`lock` in an arm must still be rejected: {stderr}"
    );
}

// 24. The sibling shape that the same block split exposed (willow-ht1h): a
//     ternary whose CONDITION suspends while its branches do not. The merge
//     local is written by an `Assign` in each branch and never by a `let`, so
//     neither existing binding pass gave it storage and it reached codegen
//     unbound -- an internal compiler error, not a fallback.
#[test]
fn lir_suspend_24_suspending_ternary_condition() {
    assert_suspends(
        "async fn flag(n: i64) -> bool { await yield(); return n > 0; }
async fn pick(n: i64) -> i64 {
    let value: i64 = (await flag(n)) ? 11 : 22;
    return value;
}
async fn main() {
    println(await pick(1));
    println(await pick(-1));
}
",
        "11\n22\n",
        &["pick", "main"],
    );
}

// 25. The runnable example is walker-compiled function by function. Its output
//     is asserted by the `example/*.wi` table in `runtime.rs`; this test is about
//     the selection, so a coverage regression names the function it lost instead
//     of passing as a silent AST run.
#[test]
fn lir_suspend_25_the_example_is_fully_lir() {
    let source = std::fs::read_to_string(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/example/lir_match_suspend.wi"
    ))
    .expect("the example is checked in");
    assert_walker_compiled(
        &source,
        &[
            "scaled",
            "route",
            "area",
            "doubled_or",
            "greet",
            "report",
            "passthrough",
            "nested",
            "through_channel",
            "maybe",
            "both_sides",
            "with_cleanup",
            "accumulate",
            "main",
        ],
    );
}
