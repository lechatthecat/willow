//! Where a channel's element type comes from, and what the runtime does with it
//! (willow-nk3g).
//!
//! `Channel::new()` written without a type argument carries no element of its
//! own: the checker types it `Channel<void>` and a `let` annotation supplies the
//! real element. That placeholder used to survive into code generation, and both
//! emitters read the element from the wrong place — the AST one from the
//! (empty) call type arguments, the LIR walker from the `Channel<void>` node —
//! so an annotated `Channel<String>` was built with `is_ref = 0`.
//!
//! `is_ref` is not cosmetic. It tells the runtime whether the channel's BUFFER
//! holds GC references, and a buffer that is not traced loses whatever is queued
//! in it at the next collection. So most of what follows queues values, allocates
//! hard enough to force a collection under `WILLOW_GC_STRESS=alloc`, and only
//! then drains — a channel whose element resolved to a scalar cannot survive
//! that.
//!
//! The inference is deliberately narrow: it applies to a `let` annotation and
//! nowhere else. Perspectives 16-18 hold that line, because widening it silently
//! would turn a type error into a wrong `is_ref`.
//!
//! 20 perspectives:
//!   1 annotated String round-trip      11 two element types side by side
//!   2 explicit type argument, control  12 element reachable only from the queue
//!   3 annotated i64 stays a scalar     13 annotated channel in a class field
//!   4 annotated bool                   14 buffer reused after a drain
//!   5 annotated f64                    15 annotated Array element
//!   6 annotated class element          16 no annotation: still `Channel<void>`
//!   7 annotated enum element           17 constructor arg: still `Channel<void>`
//!   8 a deep buffer across a collection 18 `with_capacity` infers too
//!   9 annotated channel across a task  19 bounded channel traces its buffer
//!  10 annotated channel in a `select`  20 the example is fully LIR

use super::support::{compile_and_run_with_env, compile_error_stderr, compile_with_compiler_env};

/// No extra compiler environment: the ordinary build.
const PLAIN: [(&str, &str); 0] = [];
const STRESS: [(&str, &str); 1] = [("WILLOW_GC_STRESS", "alloc")];

/// The padding loop every buffering perspective runs between queueing and
/// draining. It allocates a fresh String per turn and keeps none of them, so
/// under `WILLOW_GC_STRESS=alloc` the collector runs repeatedly with the queued
/// values reachable only through the channel.
const PAD: &str = "    let mut padding = \"\";
    let mut p = 0;
    while p < 120 {
        padding = \"pad \" + p.toString();
        p = p + 1;
    }
";

/// `expected` must come out with and without GC stress, and `functions` must
/// each be named in the walker's selection log — otherwise a coverage
/// regression could leave a function unlowered while the program still printed
/// the right answer.
fn assert_channels(source: &str, expected: &str, functions: &[&str]) {
    for env in [&PLAIN[..], &STRESS[..]] {
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

// 1. The repro. Strings are queued, everything else is dropped, the collector
//    runs, and the queue is drained: this passes only if the annotation reached
//    the channel's `is_ref` flag.
#[test]
fn channel_element_01_annotated_string_round_trip() {
    assert_channels(
        &format!(
            "async fn relay(count: i64) -> String {{
    let ch: Channel<String> = Channel::new();
    let mut i = 0;
    while i < count {{
        ch.send(\"value \" + i.toString());
        i = i + 1;
    }}
{PAD}    let mut out = \"\";
    let mut k = 0;
    while k < count {{
        out = out + ch.recv() + \";\";
        k = k + 1;
    }}
    return out;
}}
async fn main() {{ println(await relay(4)); }}
"
        ),
        "value 0;value 1;value 2;value 3;\n",
        &["relay", "main"],
    );
}

// 2. The spelling that always carried its element. It has to keep producing
//    exactly the same channel, which is what makes perspective 1 a fix rather
//    than a second code path.
#[test]
fn channel_element_02_explicit_type_argument_unchanged() {
    assert_channels(
        &format!(
            "async fn relay(count: i64) -> String {{
    let ch = Channel<String>::new();
    let mut i = 0;
    while i < count {{
        ch.send(\"value \" + i.toString());
        i = i + 1;
    }}
{PAD}    let mut out = \"\";
    let mut k = 0;
    while k < count {{
        out = out + ch.recv() + \";\";
        k = k + 1;
    }}
    return out;
}}
async fn main() {{ println(await relay(4)); }}
"
        ),
        "value 0;value 1;value 2;value 3;\n",
        &["relay", "main"],
    );
}

// 3. The other half of the flag: an `i64` element must NOT be traced. A buffer
//    of plain words walked as if it held pointers is the mirror-image fault.
#[test]
fn channel_element_03_annotated_i64_stays_a_scalar() {
    assert_channels(
        &format!(
            "async fn total(count: i64) -> i64 {{
    let ch: Channel<i64> = Channel::new();
    let mut i = 0;
    while i < count {{
        ch.send(i * i);
        i = i + 1;
    }}
{PAD}    let mut sum = 0;
    let mut k = 0;
    while k < count {{
        sum = sum + ch.recv();
        k = k + 1;
    }}
    return sum;
}}
async fn main() {{ println((await total(5)).toString()); }}
"
        ),
        "30\n",
        &["total", "main"],
    );
}

// 4. `bool` is a scalar element too, and its false values are the word zero —
//    the same bit pattern as a null reference, so a traced buffer would read
//    them as objects.
#[test]
fn channel_element_04_annotated_bool() {
    assert_channels(
        &format!(
            "async fn flags() -> String {{
    let ch: Channel<bool> = Channel::new();
    ch.send(true);
    ch.send(false);
    ch.send(true);
{PAD}    let mut out = \"\";
    let mut k = 0;
    while k < 3 {{
        out = out + ch.recv().toString() + \";\";
        k = k + 1;
    }}
    return out;
}}
async fn main() {{ println(await flags()); }}
"
        ),
        "true;false;true;\n",
        &["flags", "main"],
    );
}

// 5. `f64` is a scalar whose bits are not a valid pointer at all.
#[test]
fn channel_element_05_annotated_f64() {
    assert_channels(
        &format!(
            "async fn ratios() -> String {{
    let ch: Channel<f64> = Channel::new();
    ch.send(1.5);
    ch.send(2.25);
{PAD}    let mut out = \"\";
    let mut k = 0;
    while k < 2 {{
        out = out + ch.recv().toString() + \";\";
        k = k + 1;
    }}
    return out;
}}
async fn main() {{ println(await ratios()); }}
"
        ),
        "1.5;2.25;\n",
        &["ratios", "main"],
    );
}

// 6. A class element: the buffer holds object references, and each object owns
//    a String that has to be reached THROUGH it. Two levels of tracing, both
//    rooted in the channel.
#[test]
fn channel_element_06_annotated_class_element() {
    assert_channels(
        &format!(
            "class Note {{
    pub text: String;
    pub weight: i64;
}}
async fn notes(count: i64) -> String {{
    let ch: Channel<Note> = Channel::new();
    let mut i = 0;
    while i < count {{
        ch.send(new Note(\"note \" + i.toString(), i));
        i = i + 1;
    }}
{PAD}    let mut out = \"\";
    let mut sum = 0;
    let mut k = 0;
    while k < count {{
        let note = ch.recv();
        sum = sum + note.weight;
        out = out + note.text + \";\";
        k = k + 1;
    }}
    return sum.toString() + \" \" + out;
}}
async fn main() {{ println(await notes(4)); }}
"
        ),
        "6 note 0;note 1;note 2;note 3;\n",
        &["notes", "main"],
    );
}

// 7. An enum element. A payload-carrying variant is a heap object; a payloadless
//    one may be a bare tag, so the same buffer holds both shapes.
#[test]
fn channel_element_07_annotated_enum_element() {
    assert_channels(
        &format!(
            "enum Tone {{ Flat, Sharp(String) }}
async fn tones() -> String {{
    let ch: Channel<Tone> = Channel::new();
    ch.send(Tone::Sharp(\"high \" + \"c\"));
    ch.send(Tone::Flat);
    ch.send(Tone::Sharp(\"low \" + \"g\"));
{PAD}    let mut out = \"\";
    let mut k = 0;
    while k < 3 {{
        let tone = ch.recv();
        out = out + match tone {{ Flat => \"flat\", Sharp(name) => name }} + \";\";
        k = k + 1;
    }}
    return out;
}}
async fn main() {{ println(await tones()); }}
"
        ),
        "high c;flat;low g;\n",
        &["tones", "main"],
    );
}

// 8. A deep buffer. An unbounded channel grows its storage as it fills, so a
//    queue long enough to be reallocated must stay traced across the move.
#[test]
fn channel_element_08_deep_buffer_across_a_collection() {
    assert_channels(
        &format!(
            "async fn relay(count: i64) -> String {{
    let ch: Channel<String> = Channel::new();
    let mut i = 0;
    while i < count {{
        ch.send(\"v\" + i.toString());
        i = i + 1;
    }}
{PAD}    let mut first = \"\";
    let mut last = \"\";
    let mut k = 0;
    while k < count {{
        last = ch.recv();
        if k == 0 {{
            first = last;
        }}
        k = k + 1;
    }}
    return first + \"..\" + last;
}}
async fn main() {{ println(await relay(64)); }}
"
        ),
        "v0..v63\n",
        &["relay", "main"],
    );
}

// 9. The annotated channel handed to another task. `Channel<String>` is `Sync`
//    only because its element is, so this also checks the resolved element
//    reaches the send/sync analysis and not just code generation.
#[test]
fn channel_element_09_annotated_channel_across_a_task() {
    assert_channels(
        "async fn consume(ch: Channel<String>, count: i64) -> String {
    let mut out = \"\";
    let mut i = 0;
    while i < count {
        out = out + ch.recv() + \";\";
        i = i + 1;
    }
    return out;
}
async fn main() {
    let ch: Channel<String> = Channel::new();
    ch.send(\"a\" + \"lpha\");
    ch.send(\"b\" + \"eta\");
    println(await consume(ch, 2));
}
",
        "alpha;beta;\n",
        &["consume", "main"],
    );
}

// 10. Read back through a `select` recv case rather than a plain `recv`. The
//     case binds the element type, so a channel still typed `Channel<void>`
//     would bind nothing usable here.
#[test]
fn channel_element_10_annotated_channel_in_a_select() {
    assert_channels(
        "fn take(ch: Channel<String>) -> String {
    let mut got = \"none\";
    select {
        let v = ch.recv() => { got = v; }
        default => { got = \"idle\"; }
    }
    return got;
}
async fn main() {
    let ch: Channel<String> = Channel::new();
    println(take(ch));
    ch.send(\"que\" + \"ued\");
    println(take(ch));
}
",
        "idle\nqueued\n",
        &["take", "main"],
    );
}

// 11. Two annotated channels of different elements in one function: each has to
//     resolve from its OWN annotation, not from whichever was seen last.
#[test]
fn channel_element_11_two_element_types_side_by_side() {
    assert_channels(
        &format!(
            "async fn both() -> String {{
    let words: Channel<String> = Channel::new();
    let counts: Channel<i64> = Channel::new();
    words.send(\"one \" + \"two\");
    counts.send(7);
    words.send(\"three\");
    counts.send(9);
{PAD}    let a = words.recv();
    let b = counts.recv();
    let c = words.recv();
    let d = counts.recv();
    return a + \"/\" + b.toString() + \"/\" + c + \"/\" + d.toString();
}}
async fn main() {{ println(await both()); }}
"
        ),
        "one two/7/three/9\n",
        &["both", "main"],
    );
}

// 12. The queued value's ONLY reference is the channel: it is built inline in
//     the send, so nothing on the stack keeps it alive.
#[test]
fn channel_element_12_element_reachable_only_from_the_queue() {
    assert_channels(
        &format!(
            "class Payload {{ pub body: String; }}
async fn hold() -> String {{
    let ch: Channel<Payload> = Channel::new();
    ch.send(new Payload(\"held \" + \"value\"));
{PAD}    return ch.recv().body;
}}
async fn main() {{ println(await hold()); }}
"
        ),
        "held value\n",
        &["hold", "main"],
    );
}

// 13. The channel stored in a class field and read back through it. The field
//     is annotated `Channel<String>`, and the value put into it came from an
//     annotated `let`.
#[test]
fn channel_element_13_annotated_channel_in_a_class_field() {
    assert_channels(
        &format!(
            "class Hub {{
    pub inbox: Channel<String>;
}}
async fn route() -> String {{
    let ch: Channel<String> = Channel::new();
    let hub = new Hub(ch);
    hub.inbox.send(\"routed \" + \"message\");
{PAD}    return hub.inbox.recv();
}}
async fn main() {{ println(await route()); }}
"
        ),
        "routed message\n",
        &["route", "main"],
    );
}

// 14. Drain the channel empty, then fill it again. An unbounded channel reuses
//     its storage, so the slots a traced buffer already walked are written a
//     second time.
#[test]
fn channel_element_14_buffer_reused_after_a_drain() {
    assert_channels(
        "async fn twice() -> String {
    let ch: Channel<String> = Channel::new();
    ch.send(\"first \" + \"round\");
    let mut padding = \"\";
    let mut p = 0;
    while p < 60 {
        padding = \"pad \" + p.toString();
        p = p + 1;
    }
    let a = ch.recv();
    ch.send(\"second \" + \"round\");
    p = 0;
    while p < 60 {
        padding = \"pad \" + p.toString();
        p = p + 1;
    }
    let b = ch.recv();
    return a + \"/\" + b;
}
async fn main() { println(await twice()); }
",
        "first round/second round\n",
        &["twice", "main"],
    );
}

// 15. A collection element. The buffer holds array references, and the arrays
//     hold their own storage, so the annotation has to survive through a
//     generic element type as well as a plain one.
#[test]
fn channel_element_15_annotated_array_element() {
    assert_channels(
        &format!(
            "import std::collections::Array;
async fn rows() -> String {{
    let ch: Channel<Array<i64>> = Channel::new();
    let first: Array<i64> = [1, 2, 3];
    let second: Array<i64> = [4, 5];
    ch.send(first);
    ch.send(second);
{PAD}    let a = ch.recv();
    let b = ch.recv();
    return a.len().toString() + \"/\" + b.len().toString() + \"/\" + a[2].toString();
}}
async fn main() {{ println(await rows()); }}
"
        ),
        "3/2/3\n",
        &["rows", "main"],
    );
}

// 16. The inference is the `let` annotation's, not the call's. Without one the
//     element stays `void` and the first send is a type error — the placeholder
//     must keep being visible rather than silently becoming `i64`.
#[test]
fn channel_element_16_no_annotation_stays_void() {
    let stderr = compile_error_stderr(
        "async fn main() {
    let ch = Channel::new();
    ch.send(1);
}
",
    );
    assert!(
        stderr.contains("E0802") && stderr.contains("Channel<void>"),
        "expected a send-into-`Channel<void>` error, got: {stderr}"
    );
}

// 17. A constructor argument does not infer either, even though the field it
//     fills names the element. Only the `let` form was ever accepted, and the
//     fix records a type rather than widening what is accepted.
#[test]
fn channel_element_17_constructor_argument_stays_void() {
    let stderr = compile_error_stderr(
        "class Hub { pub inbox: Channel<String>; }
async fn main() {
    let hub = new Hub(Channel::new());
    hub.inbox.send(\"x\");
}
",
    );
    assert!(
        stderr.contains("Channel<void>"),
        "expected the constructor argument to stay `Channel<void>`, got: {stderr}"
    );
}

// 18. The annotation supplies the element for the BOUNDED constructor too. Both
//     `Channel::new()` and `Channel::with_capacity(n)` are typed `Channel<void>`
//     from the call alone and carry the identical hazard, so they infer through
//     one predicate. This used to be an E0201 on an accepted-looking program:
//     `Channel::new()` inferred and `with_capacity` did not, for no reason a
//     caller could see. The buffering shape is what makes the assertion real —
//     a `with_capacity` that resolved to a scalar element would be untraced, and
//     the padding loop below collects while the values are queued.
#[test]
fn channel_element_18_with_capacity_infers_from_the_annotation() {
    assert_channels(
        &format!(
            "async fn main() {{
    let ch: Channel<String> = Channel::with_capacity(8);
    let mut i = 0;
    while i < 4 {{
        ch.send(\"held \" + i.toString());
        i = i + 1;
    }}
{PAD}    let mut out = \"\";
    let mut k = 0;
    while k < 4 {{
        out = out + ch.recv() + \";\";
        k = k + 1;
    }}
    println(out);
}}
"
        ),
        "held 0;held 1;held 2;held 3;\n",
        &["main"],
    );
}

// 18b. The line perspective 17 draws holds for the bounded constructor as well:
//      inference is a `let` annotation and nothing else, so `with_capacity` in a
//      constructor argument still stays `Channel<void>` rather than picking up
//      the field's element. Widening 18 must not have widened this.
#[test]
fn channel_element_18b_with_capacity_outside_a_let_stays_void() {
    let stderr = compile_error_stderr(
        "class Hub { pub inbox: Channel<String>; }
async fn main() {
    let hub = new Hub(Channel::with_capacity(4));
    hub.inbox.send(\"x\");
}
",
    );
    assert!(
        stderr.contains("Channel<void>"),
        "expected a constructor argument to stay `Channel<void>`, got: {stderr}"
    );
}

// 19. The bounded constructor reads its element the same way, so a
//     `Channel<String>::with_capacity(n)` buffer is traced too — the fix touches
//     one shared accessor rather than the unbounded path alone.
#[test]
fn channel_element_19_bounded_channel_traces_its_buffer() {
    assert_channels(
        &format!(
            "async fn relay(count: i64) -> String {{
    let ch = Channel<String>::with_capacity(64);
    let mut i = 0;
    while i < count {{
        ch.send(\"value \" + i.toString());
        i = i + 1;
    }}
{PAD}    let mut out = \"\";
    let mut k = 0;
    while k < count {{
        out = out + ch.recv() + \";\";
        k = k + 1;
    }}
    return out;
}}
async fn main() {{ println(await relay(4)); }}
"
        ),
        "value 0;value 1;value 2;value 3;\n",
        &["relay", "main"],
    );
}

// 20. The shipped example, which is the annotated form end to end. Reading the
//     file keeps this honest: if the example stops exercising the annotation,
//     the assertions above are all that is left and this test says so.
#[test]
fn channel_element_20_example_is_fully_lir() {
    let path = concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/example/channel_element_inference.wi"
    );
    let source = std::fs::read_to_string(path).expect("the example is present");
    assert!(
        source.contains("let ch: Channel<String> = Channel::new();"),
        "the example no longer exercises the annotated form"
    );
    let (ok, stderr) = compile_with_compiler_env(&source, &[("WILLOW_LIR_LOG", "1")]);
    assert!(ok, "the example fell back to the AST backend: {stderr}");
    for function in [
        "buffered_strings",
        "buffered_notes",
        "buffered_numbers",
        "buffered_strings_explicit",
        "consume",
        "main",
    ] {
        let line = format!("[lir] compiling async `{function}` from lowered IR");
        assert!(
            stderr.contains(&line),
            "`{function}` did not use the LIR walker: {stderr}"
        );
    }
}
