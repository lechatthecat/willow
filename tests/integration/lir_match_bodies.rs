//! Statements inside `match` arm bodies, compiled from Lowered IR
//! (willow-0g8j.2.13).
//!
//! A `match` used as a STATEMENT is how Willow spells a multi-way store, and the
//! walker previously admitted only arms whose body was a single expression. A
//! `let` naming an intermediate, or an assignment to a variable declared outside
//! the match, took the whole function back to the AST emitter — which is what
//! kept `example/panic_recover_service.wi`'s `defer match recover()` off the
//! walker.
//!
//! The walker now brackets each arm: `vars` and the GC root depth are
//! snapshotted before the body and restored after. That bracket is what these
//! tests are really about, so several of them put a GC-managed binding in an arm
//! and then allocate, under `WILLOW_GC_STRESS=alloc`, where an unrooted slot is
//! collected rather than merely at risk.
//!
//! A `let` is admitted only where the bracket exists. A `select` case has one of
//! its own, so it takes a `let` too, and as of willow-0g8j.3 so does a `defer`
//! block: `emit_deferred_action` brackets a replayed body the same way, so the
//! rooted slot a binding takes is popped at every exit the registration is live
//! for. Perspectives 18, 19 and 20 pin that down from all three sides.
//!
//! Tests assert runtime output and use the selection log to confirm that
//! each named function was compiled from lowered IR.
//!
//! 25 perspectives:
//!   1 arm assigns an i64 outward         13 arm-local holds a new object
//!   2 arm assigns a String outward       14 payload binding feeds the next stmt
//!   3 arm declares and uses a local      15 arm-local under GC stress
//!   4 arm declares a GC local            16 arm bodies in a poll function
//!   5 statements run in order            17 deferred match assigns (the census)
//!   6 sibling arms reuse a source name   18 `let` in a `defer` block admitted
//!   7 arm-local shadows an outer name    19 `let` in a `select` case admitted
//!   8 diverging arm: `let` then return   20 `select` case assigns outward
//!   9 arm body inside a loop             21 empty arm body
//!  10 nested match inside an arm         22 wildcard arm with statements
//!  11 arm assigns to a block-scoped name  23 one arm assigns, another returns
//!  12 arm assigns through an interface   24 the example is fully LIR
//!  25 field stores inside match arms

use super::support::{compile_and_run_with_env, compile_with_compiler_env};

/// No extra compiler environment: the ordinary build.
const PLAIN: [(&str, &str); 0] = [];
const BUDGET: [(&str, &str); 1] = [("WILLOW_TASK_BUDGET", "1")];
const STRESS: [(&str, &str); 1] = [("WILLOW_GC_STRESS", "alloc")];

const SIGNAL: &str = "enum Signal { Halt, Step(i64), Label(String) }\n";

/// `expected` must come out of all four configurations, and `functions` must
/// each be named in the walker's selection log.
fn assert_bodies(source: &str, expected: &str, functions: &[&str]) {
    for env in [&PLAIN[..], &BUDGET[..], &STRESS[..]] {
        let (out, ok) = compile_and_run_with_env(source, env);
        assert!(ok, "run failed under {env:?}: {out}");
        assert_eq!(out, expected, "wrong output under {env:?}");
    }
    let (ok, stderr) = compile_with_compiler_env(source, &[("WILLOW_LIR_LOG", "1")]);
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

// 1. The core shape: an arm assigns to a variable the match does not own. The
//    walker used to have no emitter for a statement here at all.
#[test]
fn lir_bodies_01_arm_assigns_an_i64_outward() {
    assert_bodies(
        &format!(
            "{SIGNAL}
fn weight(signal: Signal) -> i64 {{
    let mut out = 0;
    match signal {{
        Halt => {{ out = 7; }},
        Step(n) => {{ out = n; }},
        Label(name) => {{ out = 1; }},
    }}
    return out;
}}
fn main() {{
    println(weight(Signal::Halt).toString());
    println(weight(Signal::Step(5)).toString());
    println(weight(Signal::Label(\"x\")).toString());
}}
"
        ),
        "7\n5\n1\n",
        &["weight", "main"],
    );
}

// 2. The same store to a GC-managed local. The target's slot is already rooted,
//    so this is a write through the existing root rather than a new one.
#[test]
fn lir_bodies_02_arm_assigns_a_string_outward() {
    assert_bodies(
        &format!(
            "{SIGNAL}
fn describe(signal: Signal) -> String {{
    let mut out = \"\";
    match signal {{
        Halt => {{ out = \"halt\"; }},
        Step(n) => {{ out = \"step \" + n.toString(); }},
        Label(name) => {{ out = \"label \" + name; }},
    }}
    return out;
}}
fn main() {{
    println(describe(Signal::Halt));
    println(describe(Signal::Step(3)));
    println(describe(Signal::Label(\"here\")));
}}
"
        ),
        "halt\nstep 3\nlabel here\n",
        &["describe", "main"],
    );
}

// 3. A `let` naming an intermediate, read by a later statement of the same arm.
#[test]
fn lir_bodies_03_arm_declares_and_uses_a_local() {
    assert_bodies(
        &format!(
            "{SIGNAL}
fn doubled(signal: Signal) -> i64 {{
    let mut out = 0;
    match signal {{
        Step(n) => {{
            let twice = n * 2;
            out = twice + 1;
        }},
        Halt => {{ out = 0; }},
        Label(name) => {{ out = 0; }},
    }}
    return out;
}}
fn main() {{ println(doubled(Signal::Step(4)).toString()); }}
"
        ),
        "9\n",
        &["doubled", "main"],
    );
}

// 4. A GC-managed arm-local. Its rooted slot is pushed inside the arm and popped
//    on the way to the merge block, so the depth the match started at is the
//    depth it continues at.
#[test]
fn lir_bodies_04_arm_declares_a_gc_local() {
    assert_bodies(
        &format!(
            "{SIGNAL}
fn describe(signal: Signal) -> String {{
    let mut out = \"\";
    match signal {{
        Step(n) => {{
            let text = \"step \" + n.toString();
            out = text + \"!\";
        }},
        Halt => {{ out = \"halt\"; }},
        Label(name) => {{ out = name; }},
    }}
    return out;
}}
fn main() {{
    println(describe(Signal::Step(2)));
    println(describe(Signal::Halt));
}}
"
        ),
        "step 2!\nhalt\n",
        &["describe", "main"],
    );
}

// 5. Statements run in source order, and each sees what the previous one wrote.
#[test]
fn lir_bodies_05_statements_run_in_order() {
    assert_bodies(
        &format!(
            "{SIGNAL}
fn trace(signal: Signal) -> String {{
    let mut log = \"\";
    match signal {{
        Step(n) => {{
            log = log + \"a\";
            let mid = n + 1;
            log = log + mid.toString();
            log = log + \"b\";
        }},
        Halt => {{ log = \"halt\"; }},
        Label(name) => {{ log = name; }},
    }}
    return log;
}}
fn main() {{ println(trace(Signal::Step(7))); }}
"
        ),
        "a8b\n",
        &["trace", "main"],
    );
}

// 6. Two arms declaring the same SOURCE name. HIR lowering renames the second,
//    so the flat map never sees a collision — and the bracket keeps neither
//    binding visible outside its own arm.
#[test]
fn lir_bodies_06_sibling_arms_reuse_a_source_name() {
    assert_bodies(
        &format!(
            "{SIGNAL}
fn pick(signal: Signal) -> i64 {{
    let mut out = 0;
    match signal {{
        Halt => {{
            let value = 11;
            out = value;
        }},
        Step(n) => {{
            let value = n * 100;
            out = value;
        }},
        Label(name) => {{
            let value = 3;
            out = value;
        }},
    }}
    return out;
}}
fn main() {{
    println(pick(Signal::Halt).toString());
    println(pick(Signal::Step(2)).toString());
    println(pick(Signal::Label(\"z\")).toString());
}}
"
        ),
        "11\n200\n3\n",
        &["pick", "main"],
    );
}

// 7. An arm-local that shadows a name already in scope. The outer binding must
//    be intact after the match, which is exactly what restoring `vars` buys.
#[test]
fn lir_bodies_07_arm_local_shadows_an_outer_name() {
    assert_bodies(
        &format!(
            "{SIGNAL}
fn shadowed(signal: Signal) -> String {{
    let value = \"outer\";
    let mut seen = \"\";
    match signal {{
        Halt => {{
            let value = \"inner\";
            seen = value;
        }},
        Step(n) => {{ seen = n.toString(); }},
        Label(name) => {{ seen = name; }},
    }}
    return seen + \"/\" + value;
}}
fn main() {{ println(shadowed(Signal::Halt)); }}
"
        ),
        "inner/outer\n",
        &["shadowed", "main"],
    );
}

// 8. A diverging arm may declare a local and leave through it. The `return` pops
//    the full runtime root depth itself, so the arm's own root goes with it.
#[test]
fn lir_bodies_08_diverging_arm_declares_then_returns() {
    assert_bodies(
        &format!(
            "{SIGNAL}
fn cost(signal: Signal) -> String {{
    match signal {{
        Halt => {{
            let base = \"halt\";
            return base + \"!\";
        }},
        Step(n) => {{
            let scaled = n * 3;
            return scaled.toString();
        }},
        Label(name) => {{
            let padded = name + \"?\";
            return padded;
        }},
    }}
}}
fn main() {{
    println(cost(Signal::Halt));
    println(cost(Signal::Step(4)));
    println(cost(Signal::Label(\"q\")));
}}
"
        ),
        "halt!\n12\nq?\n",
        &["cost", "main"],
    );
}

// 9. The arm body inside a loop. The slot is allocated once in the frame; the
//    root is pushed and popped every iteration, so a hundred passes end at the
//    depth the first one started at.
#[test]
fn lir_bodies_09_arm_body_inside_a_loop() {
    assert_bodies(
        &format!(
            "{SIGNAL}
fn pick(i: i64) -> Signal {{
    return match i % 3 {{
        0 => Signal::Halt,
        1 => Signal::Step(i),
        _ => Signal::Label(i.toString()),
    }};
}}
fn run(count: i64) -> String {{
    let mut sum = 0;
    let mut tail = \"\";
    let mut i = 0;
    while i < count {{
        let signal = pick(i);
        match signal {{
            Halt => {{
                let stop = 7;
                sum = sum + stop;
            }},
            Step(n) => {{
                let weighted = n * n;
                sum = sum + weighted;
            }},
            Label(name) => {{
                let marked = name + \".\";
                tail = tail + marked;
            }},
        }}
        i = i + 1;
    }}
    return sum.toString() + \" \" + tail;
}}
fn main() {{ println(run(100)); }}
"
        ),
        // 34 Halts at 7 each (238), plus n*n over i = 1, 4, 7, .. 97 (106161).
        "106399 2.5.8.11.14.17.20.23.26.29.32.35.38.41.44.47.50.53.56.59.62.65.68.\
71.74.77.80.83.86.89.92.95.98.\n",
        &["pick", "run", "main"],
    );
}

// 10. A match nested inside an arm body: the inner bracket has to nest with the
//     outer one, so the inner arm's local does not survive into the outer arm.
#[test]
fn lir_bodies_10_nested_match_inside_an_arm() {
    assert_bodies(
        &format!(
            "{SIGNAL}
fn nested(signal: Signal, verbose: bool) -> String {{
    let mut label = \"\";
    match signal {{
        Step(n) => {{
            match verbose {{
                true => {{
                    let detail = \"step(\" + n.toString() + \")\";
                    label = detail;
                }},
                false => {{
                    let brief = \"step\";
                    label = brief;
                }},
            }}
        }},
        Halt => {{ label = \"halt\"; }},
        Label(name) => {{ label = name; }},
    }}
    return label;
}}
fn main() {{
    println(nested(Signal::Step(2), true));
    println(nested(Signal::Step(2), false));
}}
"
        ),
        "step(2)\nstep\n",
        &["nested", "main"],
    );
}

// 11. Assignment to a name declared in an ENCLOSING block rather than in the
//     function's own top-level scope. The map the walker checks against is flat,
//     so the arm resolves it the same way either time.
#[test]
fn lir_bodies_11_arm_assigns_to_an_enclosing_block_local() {
    assert_bodies(
        &format!(
            "{SIGNAL}
fn adjust(signal: Signal) -> i64 {{
    let mut reported = 0;
    if true {{
        let mut base = 10;
        match signal {{
            Halt => {{ base = base + 1; }},
            Step(n) => {{
                let bump = n * 2;
                base = base + bump;
            }},
            Label(name) => {{ base = 0; }},
        }}
        reported = base;
    }}
    return reported;
}}
fn main() {{
    println(adjust(Signal::Halt).toString());
    println(adjust(Signal::Step(3)).toString());
    println(adjust(Signal::Label(\"z\")).toString());
}}
"
        ),
        "11\n16\n0\n",
        &["adjust", "main"],
    );
}

// 12. An arm assigning an interface box outward. The DECLARED type of the
//     target decides the store, exactly as it does for a function-level
//     assignment, so moving a concrete arm-local into an interface-typed
//     variable boxes it on the way.
#[test]
fn lir_bodies_12_arm_assigns_an_interface_box() {
    assert_bodies(
        &format!(
            "{SIGNAL}
interface Named {{ fn label(self) -> String; }}
class Item implements Named {{
    pub tag: String;
    pub fn label(self) -> String {{ return \"item \" + self.tag; }}
}}
class Blank implements Named {{
    pub fn label(self) -> String {{ return \"blank\"; }}
}}
fn shown(who: Named) -> String {{ return who.label(); }}
fn boxed(signal: Signal) -> String {{
    let mut who: Named = new Blank();
    match signal {{
        Step(n) => {{
            let chosen = new Item(\"tagged\");
            who = chosen;
        }},
        Halt => {{ who = new Blank(); }},
        Label(name) => {{ who = new Item(name); }},
    }}
    return shown(who);
}}
fn main() {{
    println(boxed(Signal::Step(8)));
    println(boxed(Signal::Halt));
    println(boxed(Signal::Label(\"named\")));
}}
"
        ),
        "item tagged\nblank\nitem named\n",
        &["shown", "boxed", "main"],
    );
}

// 13. An arm-local holding a freshly allocated object, read after a further
//     allocation in the same arm. Without the root the object is unreachable
//     from the collector's point of view for the length of that allocation.
#[test]
fn lir_bodies_13_arm_local_holds_a_new_object() {
    assert_bodies(
        &format!(
            "{SIGNAL}
class Route {{
    pub name: String;
    pub cost: i64;
}}
fn route_for(signal: Signal) -> String {{
    let mut summary = \"none\";
    match signal {{
        Step(n) => {{
            let route = new Route(\"step\", n);
            let tag = route.name + \"/\" + route.cost.toString();
            summary = tag;
        }},
        Halt => {{ summary = \"halt\"; }},
        Label(name) => {{ summary = name; }},
    }}
    return summary;
}}
fn main() {{ println(route_for(Signal::Step(9))); }}
"
        ),
        "step/9\n",
        &["route_for", "main"],
    );
}

// 14. A pattern's payload binding is in scope for the arm's statements, and a
//     `let` declared after it does not displace it.
#[test]
fn lir_bodies_14_payload_binding_feeds_the_next_statement() {
    assert_bodies(
        &format!(
            "{SIGNAL}
fn combine(signal: Signal) -> String {{
    let mut out = \"\";
    match signal {{
        Label(name) => {{
            let prefix = \"<\";
            let wrapped = prefix + name + \">\";
            out = wrapped + name;
        }},
        Halt => {{ out = \"halt\"; }},
        Step(n) => {{ out = n.toString(); }},
    }}
    return out;
}}
fn main() {{ println(combine(Signal::Label(\"ab\"))); }}
"
        ),
        "<ab>ab\n",
        &["combine", "main"],
    );
}

// 15. GC stress with a long chain of arm-locals, each allocating while the
//     previous one is still live. `STRESS` collects at every allocation, so
//     a missing root shows up as a wrong string rather than as luck.
#[test]
fn lir_bodies_15_arm_local_under_gc_stress() {
    assert_bodies(
        &format!(
            "{SIGNAL}
fn chain(signal: Signal) -> String {{
    let mut out = \"\";
    match signal {{
        Step(n) => {{
            let a = \"a\" + n.toString();
            let b = a + \"b\";
            let c = b + \"c\";
            let d = c + \"d\";
            out = a + \"|\" + b + \"|\" + c + \"|\" + d;
        }},
        Halt => {{ out = \"halt\"; }},
        Label(name) => {{ out = name; }},
    }}
    return out;
}}
fn main() {{ println(chain(Signal::Step(1))); }}
"
        ),
        "a1|a1b|a1bc|a1bcd\n",
        &["chain", "main"],
    );
}

// 16. The same bodies inside a poll function. Nothing in an arm may suspend — an
//     arm body has no cooperative await split of its own — so the parks sit on
//     either side of the match.
#[test]
fn lir_bodies_16_arm_bodies_in_a_poll_function() {
    assert_bodies(
        &format!(
            "{SIGNAL}
async fn describe(signal: Signal) -> String {{
    await yield();
    let mut out = \"\";
    match signal {{
        Step(n) => {{
            let doubled = n + n;
            out = \"async step \" + doubled.toString();
        }},
        Halt => {{ out = \"async halt\"; }},
        Label(name) => {{ out = \"async \" + name; }},
    }}
    await yield();
    return out;
}}
async fn main() {{
    let a = await describe(Signal::Step(6));
    println(a);
    let b = await describe(Signal::Halt);
    println(b);
}}
"
        ),
        "async step 12\nasync halt\n",
        &["describe", "main"],
    );
}

// 17. The census shape this slice was opened for: a deferred match whose arm
//     assigns to the enclosing variable. `example/panic_recover_service.wi` is
//     written this way, and it stayed on the AST emitter until now.
#[test]
fn lir_bodies_17_deferred_match_assigns_outward() {
    assert_bodies(
        "fn handle(payload: i64) -> i64 {
    if payload < 0 {
        panic(\"bad payload\");
    }
    return payload * 2;
}
fn serve(payload: i64) -> String {
    let mut status = \"500\";
    if true {
        defer match recover() {
            Some(info) => {
                status = \"500 recovered: \" + info.message;
            },
            None => {}
        }
        let body = handle(payload);
        status = \"200 body=\" + body.toString();
    }
    return status;
}
fn main() {
    println(serve(21));
    println(serve(0 - 1));
}
",
        "200 body=42\n500 recovered: bad payload\n",
        &["handle", "serve", "main"],
    );
}

// 18. A `let` in a deferred block is admitted (willow-0g8j.3). The unwinder
//     replays that body inside the bracket `emit_deferred_action` opens for it,
//     so the GC-managed binding's rooted slot is popped again on the way out and
//     the code after the flush stays at the depth it was emitted for. Run under
//     stress like the arm cases, where a leaked slot is collected rather than
//     merely at risk.
#[test]
fn lir_bodies_18_let_in_a_defer_block_admitted() {
    assert_bodies(
        "fn serve() -> String {
    let mut status = \"ok\";
    if true {
        defer {
            let note = \"cleaned\";
            println(note);
        }
        status = \"ran\";
    }
    return status;
}
fn main() { println(serve()); }
",
        "cleaned\nran\n",
        &["serve", "main"],
    );
}

// 19. A `select` case, by contrast, IS bracketed: its emitter snapshots `vars`
//     and the root depth around each case exactly as the match emitter does
//     around an arm, so a `let` is admitted there — including a GC-managed one,
//     which is why this runs under stress like the arm cases above.
#[test]
fn lir_bodies_19_let_in_a_select_case_admitted() {
    assert_bodies(
        "fn drain(ch: Channel<i64>) -> String {
    let mut label = \"empty\";
    select {
        let v = ch.recv() => {
            let tag = \"got \" + v.toString();
            label = tag;
        }
        default => {
            let idle = \"nothing \" + \"queued\";
            label = idle;
        }
    }
    return label;
}
async fn main() {
    let ch = Channel<i64>::new();
    println(drain(ch));
    ch.send(21);
    println(drain(ch));
}
",
        "nothing queued\ngot 21\n",
        &["drain", "main"],
    );
}

// 20. Assignment out of a `select` case, the form that was already emittable.
//     It shares the statement emitter with the `let` above, so both paths of
//     that emitter are covered.
#[test]
fn lir_bodies_20_select_case_assigns_outward() {
    assert_bodies(
        "fn drain(ch: Channel<i64>) -> i64 {
    let mut seen = 0;
    select {
        let v = ch.recv() => { seen = v * 2; }
        default => { seen = 0 - 1; }
    }
    return seen;
}
async fn main() {
    let ch = Channel<i64>::new();
    println(drain(ch).toString());
    ch.send(21);
    println(drain(ch).toString());
}
",
        "-1\n42\n",
        &["drain", "main"],
    );
}

// 21. An empty arm body. Nothing runs and nothing is handed to the merge block,
//     which is a shape `defer match recover()` relies on for its `None` arm.
#[test]
fn lir_bodies_21_empty_arm_body() {
    assert_bodies(
        &format!(
            "{SIGNAL}
fn only_steps(signal: Signal) -> i64 {{
    let mut out = 0;
    match signal {{
        Step(n) => {{ out = n; }},
        Halt => {{}},
        Label(name) => {{}},
    }}
    return out;
}}
fn main() {{
    println(only_steps(Signal::Step(4)).toString());
    println(only_steps(Signal::Halt).toString());
}}
"
        ),
        "4\n0\n",
        &["only_steps", "main"],
    );
}

// 22. A wildcard arm with statements. It always matches, so the emitter breaks
//     out of the arm loop after it — the bracket still has to be unwound.
#[test]
fn lir_bodies_22_wildcard_arm_with_statements() {
    assert_bodies(
        "fn bucket(n: i64) -> String {
    let mut out = \"\";
    match n {
        0 => { out = \"zero\"; },
        _ => {
            let doubled = n * 2;
            let text = doubled.toString();
            out = \"other \" + text;
        },
    }
    return out;
}
fn main() {
    println(bucket(0));
    println(bucket(5));
}
",
        "zero\nother 10\n",
        &["bucket", "main"],
    );
}

// 23. One arm assigns and reaches the merge block; another returns and does not.
//     The assigned variable must be defined on the reaching path only.
#[test]
fn lir_bodies_23_one_arm_assigns_another_returns() {
    assert_bodies(
        &format!(
            "{SIGNAL}
fn resolve(signal: Signal) -> String {{
    let mut out = \"start\";
    match signal {{
        Halt => {{
            let early = \"early\";
            return early;
        }},
        Step(n) => {{
            let text = n.toString();
            out = out + \"/\" + text;
        }},
        Label(name) => {{ out = name; }},
    }}
    return out + \"/end\";
}}
fn main() {{
    println(resolve(Signal::Halt));
    println(resolve(Signal::Step(3)));
}}
"
        ),
        "early\nstart/3/end\n",
        &["resolve", "main"],
    );
}

// 24. The runnable example is fully walker-compiled, function by function. Its
//     output is asserted by the `example/*.wi` table in `runtime.rs`; this test
//     is about the selection, so a coverage regression names the function it
//     lost rather than showing up as a silent AST run.
#[test]
fn lir_bodies_24_the_example_is_fully_lir() {
    let source = std::fs::read_to_string(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/example/lir_match_bodies.wi"
    ))
    .expect("the example is checked in");
    let (ok, stderr) = compile_with_compiler_env(&source, &[("WILLOW_LIR_LOG", "1")]);
    assert!(ok, "the example fell back somewhere: {stderr}");
    for function in [
        "describe",
        "cost_of",
        "total",
        "pick",
        "nested",
        "route_for",
        "describe_async",
        "guarded",
        "main",
    ] {
        let sync = format!("[lir] compiling `{function}` from lowered IR");
        let coop = format!("[lir] compiling async `{function}` from lowered IR");
        assert!(
            stderr.contains(&sync) || stderr.contains(&coop),
            "`{function}` did not use the LIR walker: {stderr}"
        );
    }
}

// 25. A field store is an ordinary effect statement inside an arm. Cover both
//     binding shapes that used to reject it: an enum payload and an interface
//     downcast. Either refusal is a compile error, so the run is the check.
#[test]
fn lir_bodies_25_field_stores_inside_match_arms() {
    assert_bodies(
        "class Node { pub value: i64; }
enum Boxed { One(Node), Empty }
interface Shape { fn area(self) -> i64; }
class Square implements Shape {
    pub side: i64;
    pub fn area(self) -> i64 { return self.side * self.side; }
}
fn update_box(x: Boxed) -> i64 {
    match x {
        Boxed::One(n) => { n.value = 5; return n.value; }
        Boxed::Empty => { return -1; }
    }
}
fn update_shape(shape: Shape) -> i64 {
    match shape {
        Square(square) => { square.side = 9; return square.side; }
        _ => { return -1; }
    }
}
fn main() {
    println(update_box(Boxed::One(new Node(1))).toString());
    println(update_shape(new Square(2)).toString());
}
",
        "5\n9\n",
        &["update_box", "update_shape", "main"],
    );
}
