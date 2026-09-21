use super::*;

// ----------------------------------------------------------------------------
// Cooperative channels (willow-dsw): channel `recv` is a cooperative suspend
// point — an empty `recv` parks the consuming task as a channel waiter, and
// `send`/`close` wake it. This makes a recv-consumer a real cooperative task
// (task await works) and lets producer/consumer tasks interleave correctly.
// ----------------------------------------------------------------------------

// Producer and consumer tasks interleave; awaiting the consumer returns its result.
#[test]
fn coop_chan_01_task_producer_consumer() {
    let (out, ok) = compile_and_run(
        r#"
async fn producer(ch: Channel<i64>) -> i64 {
    let mut i = 1;
    while i <= 3 {
        await sleep(1);
        ch.send(i * 10);
        i = i + 1;
    }
    ch.send(0);
    ch.close();
    return 0;
}
async fn consumer(ch: Channel<i64>) -> i64 {
    let mut total = 0;
    let mut v = ch.recv();
    while v != 0 {
        println(v);
        total = total + v;
        v = ch.recv();
    }
    return total;
}
async fn main() {
    let ch = Channel<i64>::new();
    let p = producer(ch);
    let c = consumer(ch);
    println(await c);
    await p;
}
"#,
    );
    assert!(ok, "{out}");
    assert_eq!(out, "10\n20\n30\n60\n");
}

// Same, under GC stress (the channel value queue + frame slots survive).
#[test]
fn coop_chan_02_task_producer_consumer_gc() {
    let (out, ok) = compile_and_run_gc_stress(
        r#"
async fn producer(ch: Channel<i64>) -> i64 {
    let mut i = 1;
    while i <= 3 {
        await sleep(1);
        ch.send(i);
        i = i + 1;
    }
    ch.send(0);
    ch.close();
    return 0;
}
async fn consumer(ch: Channel<i64>) -> i64 {
    let mut total = 0;
    let mut v = ch.recv();
    while v != 0 {
        total = total + v;
        v = ch.recv();
    }
    return total;
}
async fn main() {
    let ch = Channel<i64>::new();
    let p = producer(ch);
    let c = consumer(ch);
    println(await c);
    await p;
}
"#,
    );
    assert!(ok, "{out}");
    assert_eq!(out, "6\n");
}

// A consumer that recvs in a `let` binding (first value) then loops with assign.
#[test]
fn coop_chan_03_recv_let_and_assign() {
    let (out, ok) = compile_and_run(
        r#"
async fn producer(ch: Channel<i64>) -> i64 {
    await sleep(1);
    ch.send(7);
    ch.send(8);
    ch.close();
    return 0;
}
async fn main() {
    let ch = Channel<i64>::new();
    let p = producer(ch);
    let a = await consume_first(ch);
    println(a);
    await p;
}
async fn consume_first(ch: Channel<i64>) -> i64 {
    let x = ch.recv();
    let y = ch.recv();
    return x + y;
}
"#,
    );
    assert!(ok, "{out}");
    assert_eq!(out, "15\n");
}

// Channel<GC-type> buffers are GC-traced: computed (non-literal) string values
// queued in a channel survive collection until received (willow-dsw GC tracing).
#[test]
fn coop_chan_04_gc_element_channel_traced() {
    let (out, ok) = compile_and_run_gc_stress(
        r#"
async fn producer(ch: Channel<String>, tag: String) -> i64 {
    await sleep(1);
    ch.send(tag + "-1");
    ch.send(tag + "-2");
    ch.close();
    return 0;
}
async fn consumer(ch: Channel<String>) -> i64 {
    let a = ch.recv();
    let b = ch.recv();
    println(a);
    println(b);
    return 0;
}
async fn main() {
    let ch = Channel<String>::new();
    let p = producer(ch, "x");
    let c = consumer(ch);
    await c;
    await p;
}
"#,
    );
    assert!(ok, "{out}");
    assert_eq!(out, "x-1\nx-2\n");
}

#[test]
fn coop_chan_05_parked_receiver_frame_survives_gc_before_send() {
    let (out, ok) = compile_and_run_gc_stress(
        r#"
async fn producer(ch: Channel<String>) -> i64 {
    await sleep(1);
    gc_collect();
    ch.send("done");
    ch.close();
    return 0;
}
async fn consumer(ch: Channel<String>, prefix: String) -> String {
    let kept = prefix + "-keep";
    let v = ch.recv();
    gc_collect();
    return kept + ":" + v;
}
async fn main() {
    let ch = Channel<String>::new();
    let p = producer(ch);
    let c = consumer(ch, "rx");
    gc_collect();
    println(await c);
    await p;
}
"#,
    );
    assert!(ok, "{out}");
    assert_eq!(out, "rx-keep:done\n");
}

#[test]
fn coop_chan_06_gc_stress_all_scheduler_boundaries() {
    let (out, ok) = compile_and_run_gc_stress_all(
        r#"
class Box { pub text: String; }
async fn producer(ch: Channel<Box>) -> i64 {
    await sleep(1);
    ch.send(new Box("v" + "1"));
    ch.close();
    return 0;
}
async fn consumer(ch: Channel<Box>, prefix: String) -> String {
    let kept = prefix + "-keep";
    let b = ch.recv();
    return kept + ":" + b.text;
}
async fn main() {
    let ch = Channel<Box>::new();
    let p = producer(ch);
    let c = consumer(ch, "rx");
    println(await c);
    await p;
}
"#,
    );
    assert!(ok, "{out}");
    assert_eq!(out, "rx-keep:v1\n");
}

#[test]
fn async_catalog_50_cases() {
    let (out, ok) = compile_and_run_gc_stress(
        r#"
import std::collections::Array;

async fn id_i64(x: i64) -> i64 { await sleep(1); return x; }
async fn plus(a: i64, b: i64) -> i64 { await sleep(1); return a + b; }
async fn flag(value: bool) -> bool { await sleep(1); return value; }
async fn half(value: f64) -> f64 { await sleep(1); return value / 2.0; }
async fn mark(value: String) -> String { await sleep(1); return value + "!"; }
async fn wrap(value: String) -> String { return await mark(value); }
async fn delayed_sum(a: i64, b: i64, c: i64) -> i64 {
    let values: Array<i64> = [a, b, c];
    let mut total = 0;
    for value in values { await sleep(1); total = total + value; }
    return total;
}
async fn range_sum(end: i64) -> i64 {
    let mut total = 0;
    for value in 1..end { await sleep(1); total = total + value; }
    return total;
}
async fn while_sum(end: i64) -> i64 {
    let mut total = 0;
    let mut value = 1;
    while value <= end { await sleep(1); total = total + value; value = value + 1; }
    return total;
}
async fn choose(cond: bool, a: i64, b: i64) -> i64 { await sleep(1); return cond ? a : b; }
async fn mutate_local(seed: i64) -> i64 {
    let mut value = seed;
    value = await plus(value, 2);
    await sleep(1);
    return value;
}
async fn producer(ch: Channel<i64>) -> i64 {
    await sleep(1);
    ch.send(10);
    ch.send(20);
    ch.close();
    return 0;
}
async fn consumer(ch: Channel<i64>) -> i64 {
    let a = ch.recv();
    let b = ch.recv();
    return a + b;
}
async fn string_producer(ch: Channel<String>, prefix: String) -> i64 {
    await sleep(1);
    ch.send(prefix + "-a");
    ch.send(prefix + "-b");
    ch.close();
    return 0;
}
async fn string_consumer(ch: Channel<String>) -> String {
    let a = ch.recv();
    let b = ch.recv();
    return a + b;
}
async fn square(x: i64) -> i64 { return x * x; }
async fn async_square(x: i64) -> i64 { await sleep(1); return x * x; }
async fn async_bool(value: i64) -> bool { await sleep(1); return value > 0; }
async fn async_text(value: String) -> String { await sleep(1); return value + "?"; }
async fn nested_left(x: i64) -> i64 {
    let y = await plus(x, 1);
    await sleep(1);
    return y + 1;
}
async fn nested_right(x: i64) -> i64 {
    let y = await nested_left(x);
    await sleep(1);
    return y + 1;
}
async fn count_down(seed: i64) -> i64 {
    let mut value = seed;
    while value > 0 { await sleep(1); value = value - 1; }
    return value;
}
async fn maybe_sleep(flag_value: bool) -> i64 {
    if flag_value { await sleep(1); return 31; } else { await sleep(1); return 32; }
}
async fn array_pick(a: i64, b: i64, c: i64, index: i64) -> i64 { let values: Array<i64> = [a, b, c]; await sleep(1); return values[index]; }
async fn array_update() -> i64 {
    let mut values: Array<i64> = [1, 2, 3];
    values[1] = await plus(values[0], values[2]);
    await sleep(1);
    return values[1];
}
async fn gc_string(value: String) -> String {
    gc_collect();
    await sleep(1);
    gc_collect();
    return value + "*";
}
async fn return_array() -> Array<i64> { await sleep(1); return [4, 5, 6]; }
async fn await_after_sleep(value: i64) -> i64 { await sleep(1); return value; }

// Split into parts: one async fn holding all 50 cases would need more
// GC-managed frame slots than the frame's reference mask can describe.
async fn main() {
    await part1();
    await part2();
    await part3();
    await part4();
}

async fn part1() {
    println(await id_i64(1));
    println(await plus(1, 1));
    println(await flag(true));
    println(await flag(false));
    println(await half(5.0));
    println(await mark("hello"));
    println(await wrap("wrap"));
    let s1 = await id_i64(3);
    let s2 = await id_i64(4);
    println(s1 + s2);
    let mut assigned = 0;
    assigned = await plus(5, 5);
    println(assigned);
    await id_i64(10);
    println(11);
    if true { await sleep(1); println(12); }
    if false { println(0); } else { await sleep(1); println(13); }
    println(await while_sum(3));
    println(await delayed_sum(1, 2, 3));
    println(await range_sum(4));
    let h1 = square(4);
    println(await h1);
    let h2 = async_square(5);
    println(await h2);
    let ha = async_square(2);
    let hb = async_square(3);
    println(await ha + await hb);
    let hc = await_after_sleep(21);
    await sleep(1);
    println(await hc);
}

async fn part2() {
    let ch = Channel<i64>::new();
    let p = producer(ch);
    let c = consumer(ch);
    println(await c);
    await p;
    let sch = Channel<String>::new();
    let sp = string_producer(sch, "m");
    let sc = string_consumer(sch);
    println(await sc);
    await sp;
    let buffered = Channel<i64>::new();
    buffered.send(0);
    buffered.close();
    println(buffered.recv());
    println(await gc_string("live"));
    let array_value: Array<i64> = [4, 5];
    println(await delayed_sum(array_value[0], array_value[1], 0));
}

async fn part3() {
    println(await choose(true, 27, 0));
    println(await choose(false, 0, 28));
    println(await plus(14, 15));
    println(await plus(15, 16));
    println(await maybe_sleep(true));
    println(await maybe_sleep(false));
    println(await nested_right(30));
    println(await count_down(3));
    println(await array_pick(40, 41, 42, 1));
    println(await array_update());
    let returned = await return_array();
    println(returned[2]);
    println(await async_bool(1));
    println(await async_bool(-1));
    println(await async_text("text"));
    let j1 = async_bool(2);
    println(await j1);
    let j2 = async_text("await");
    println(await j2);
    let j3 = half(3.0);
    println(await j3);
}

async fn part4() {
    let mut loop_total = 0;
    for n in 1..5 { await sleep(1); loop_total = loop_total + n; }
    println(loop_total);
    let mut while_total = 0;
    let mut wi = 0;
    while wi < 3 { await sleep(1); while_total = while_total + wi; wi = wi + 1; }
    println(while_total);
    await sleep(0);
    println(48);
    await sleep(-1);
    println(49);
    println(await mutate_local(40));
    let j4 = async_square(6);
    println(await j4);
    println(await delayed_sum(7, 8, 0));
    println(await mark("last"));
    println(await plus(25, 25));
}
"#,
    );
    assert!(ok, "{out}");
    assert_catalog_lines(
        &out,
        &[
            ("await_i64", "1"),
            ("await_add", "2"),
            ("await_bool_true", "true"),
            ("await_bool_false", "false"),
            ("await_f64", "2.5"),
            ("await_string", "hello!"),
            ("return_call_await", "wrap!"),
            ("sequential_awaits", "7"),
            ("assign_await", "10"),
            ("discard_await", "11"),
            ("await_in_if", "12"),
            ("await_in_else", "13"),
            ("await_in_while", "6"),
            ("await_in_array_for", "6"),
            ("await_in_range_for", "6"),
            ("spawn_sync_await", "16"),
            ("spawn_async_await", "25"),
            ("multiple_async_awaits", "13"),
            ("sleep_before_task_await", "21"),
            ("channel_i64", "30"),
            ("channel_string", "m-am-b"),
            ("closed_channel_buffered_value", "0"),
            ("gc_string_across_await", "live*"),
            ("array_param_across_await", "9"),
            ("ternary_true_after_await", "27"),
            ("ternary_false_after_await", "28"),
            ("await_add_again", "29"),
            ("await_add_second", "31"),
            ("if_true_return", "31"),
            ("if_false_return", "32"),
            ("nested_call_await", "33"),
            ("countdown_loop", "0"),
            ("array_index_after_await", "41"),
            ("array_assignment_await", "4"),
            ("async_return_array", "6"),
            ("spawn_bool_true", "true"),
            ("spawn_bool_false", "false"),
            ("async_text", "text?"),
            ("await_bool", "true"),
            ("await_string", "await?"),
            ("await_f64", "1.5"),
            ("main_range_loop", "10"),
            ("main_while_loop", "3"),
            ("zero_sleep", "48"),
            ("negative_sleep", "49"),
            ("mutate_local_after_await", "42"),
            ("spawn_square_again", "36"),
            ("array_sum_again", "15"),
            ("string_mark_again", "last!"),
            ("final_add", "50"),
        ],
    );
}

#[test]
fn async_object_catalog_50_cases() {
    let (out, ok) = compile_and_run_gc_stress(
        r#"
import std::collections::Array;

class Box {
    pub v: i64;
    pub fn get(self) -> i64 { return self.v; }
    pub fn add(self, n: i64) { self.v = self.v + n; }
    pub fn set(self, n: i64) { self.v = n; }
    pub fn copy(self) -> Box { return new Box(self.v); }
    pub static fn new(v: i64) -> Box { return new Box(v); }
}
class Holder { pub text: String; pub child: Option<Box>; }
class Pair { pub left: Box; pub right: Box; }
class FlagBox { pub ok: bool; }
class FloatBox { pub v: f64; }
class Node { pub v: i64; pub next: Option<Node>; }
interface Named extends Sync { fn name(self) -> String; }
interface Greeter extends Sync { fn name(self) -> String; fn greet(self) -> String { return "hi " + self.name(); } }
class User implements Named, Greeter { pub label: String; pub fn name(self) -> String { return self.label; } }
open class Animal { pub open fn score(self) -> i64 { return 1; } }
class Dog extends Animal { pub bonus: i64; pub override fn score(self) -> i64 { return self.bonus + 2; } }

async fn read_value(b: Box) -> i64 { await sleep(1); return b.v; }
async fn read_method(b: Box) -> i64 { await sleep(1); return b.get(); }
async fn add_after(b: Box, n: i64) -> i64 { await sleep(1); b.add(n); return b.v; }
async fn set_after(b: Box, n: i64) -> i64 { await sleep(1); b.set(n); return b.v; }
async fn make_box(v: i64) -> Box { await sleep(1); return new Box(v); }
async fn same_box(b: Box) -> Box { await sleep(1); return b; }
async fn copy_after(b: Box) -> Box { await sleep(1); return b.copy(); }
async fn plus_i64(a: i64, b: i64) -> i64 { await sleep(1); return a + b; }
async fn holder_text(h: Holder) -> String { await sleep(1); return h.text; }
async fn update_holder(h: Holder, suffix: String) -> String { await sleep(1); h.text = h.text + suffix; return h.text; }
async fn child_value(h: Holder) -> i64 { await sleep(1); return match h.child { Some(child) => child.v, None => 0 }; }
async fn pair_sum(p: Pair) -> i64 { await sleep(1); return p.left.v + p.right.v; }
async fn array_sum(a: Box, b: Box, c: Box) -> i64 { let xs: Array<Box> = [a, b, c]; let mut total = 0; for x in xs { await sleep(1); total = total + x.v; } return total; }
async fn array_sum_gc(a: Box, b: Box) -> i64 { let xs: Array<Box> = [a, b]; gc_collect(); let mut total = 0; for x in xs { await sleep(1); gc_collect(); total = total + x.v; } return total; }
async fn box_producer(ch: Channel<Box>) -> i64 { await sleep(1); ch.send(new Box(9)); ch.send(new Box(10)); ch.close(); return 0; }
async fn box_consumer(ch: Channel<Box>) -> i64 { let a = ch.recv(); let b = ch.recv(); return a.v + b.v; }
async fn return_boxes() -> Array<Box> { await sleep(1); return [new Box(9), new Box(11)]; }
async fn gc_box_value(b: Box) -> i64 { gc_collect(); await sleep(1); gc_collect(); return b.v; }
async fn gc_holder_text(h: Holder) -> String { gc_collect(); await sleep(1); gc_collect(); return h.text; }
async fn named_name(n: Named) -> String { await sleep(1); return n.name(); }
async fn greet_text(g: Greeter) -> String { await sleep(1); return g.greet(); }
async fn animal_score(a: Animal) -> i64 { await sleep(1); return a.score(); }
async fn option_box(opt: Option<Box>) -> i64 { await sleep(1); return match opt { Option::Some(b) => b.v, Option::None => 0 }; }
async fn result_box(r: Result<Box, String>) -> i64 { await sleep(1); return match r { Result::Ok(b) => b.v, Result::Err(e) => 0 }; }
fn sound(n: Named) -> String { return match n { User(u) => u.name() + "!", _ => "?" }; }
async fn named_sound(n: Named) -> String { await sleep(1); return sound(n); }
async fn async_sum_nodes(node: Option<Node>) -> i64 { await sleep(1); let mut total = 0; let mut current = node; while current.is_some() { let value = current.unwrap(); total = total + value.v; current = value.next; } return total; }
async fn choose_box(cond: bool, a: Box, b: Box) -> Box { await sleep(1); return cond ? a : b; }
async fn make_from_static(v: i64) -> Box { await sleep(1); return Box::new(v); }
async fn flag_value(f: FlagBox) -> bool { await sleep(1); return f.ok; }
async fn float_half(f: FloatBox) -> f64 { await sleep(1); return f.v / 2.0; }
async fn make_holder(text: String, value: i64) -> Holder { await sleep(1); return new Holder(text, Some(new Box(value))); }
async fn holder_child_copy_value(h: Holder) -> i64 { await sleep(1); return match h.child { Some(child) => child.copy().v, None => 0 }; }
async fn user_producer(ch: Channel<User>) -> i64 { await sleep(1); ch.send(new User("chan")); ch.close(); return 0; }
async fn user_consumer(ch: Channel<User>) -> String { let u = ch.recv(); return u.name(); }
async fn nested_box(v: i64) -> Box { return await make_box(v); }

// Split into parts: one async fn holding all 50 cases would need more
// GC-managed frame slots than the frame's reference mask can describe.
async fn main() {
    await part1();
    await part2();
    await part3();
    await part4();
}

async fn part1() {
    println(await read_value(new Box(1)));
    println(await read_method(new Box(2)));
    let b3 = new Box(3);
    println(await add_after(b3, 1));
    println(b3.v);
    let b5 = await make_box(5);
    println(b5.v);
    let b6 = await same_box(b5);
    println(b6.v);
    let alias = b3;
    println(await add_after(alias, 3));
    println(b3.v);
    println(await set_after(b3, 9));
    println(b3.v);
    let h = new Holder("a", Some(b3));
    println(await holder_text(h));
    println(await update_holder(h, "b"));
    println(h.text);
    println(await child_value(h));
    let empty = new Holder("empty", None);
    println(await child_value(empty));
    let pair = new Pair(new Box(7), new Box(8));
    println(await pair_sum(pair));
    println(await array_sum(new Box(1), new Box(2), new Box(3)));
}

async fn part2() {
    let mut arr: Array<Box> = [new Box(4), new Box(5)];
    arr[1] = await make_box(18);
    println(arr[1].v);
    let ch = Channel<Box>::new();
    let p = box_producer(ch);
    let c = box_consumer(ch);
    println(await c);
    await p;
    let boxes = await return_boxes();
    println(boxes[0].v + boxes[1].v);
    let j = make_box(21);
    println((await j).v);
    let jr = read_value(new Box(22));
    println(await jr);
    let shared = new Box(20);
    let r1 = read_value(shared);
    let r2 = read_method(shared);
    println(await r1 + await r2);
    println(await gc_box_value(new Box(24)));
    println(await gc_holder_text(new Holder("gc", Some(new Box(1)))));
}

async fn part3() {
    let u = new User("Ada");
    println(await named_name(u));
    println(await greet_text(u));
    println(await animal_score(new Dog(26)));
    println(await option_box(Option::Some(new Box(29))));
    println(await option_box(Option::None));
    println(await result_box(Result::Ok(new Box(31))));
    println(await result_box(Result::Err("bad")));
    println(await named_sound(new User("Rex")));
    let n3 = new Node(3, None);
    let n2 = new Node(2, Some(n3));
    let n1 = new Node(1, Some(n2));
    println(await async_sum_nodes(Some(n1)));
    println((await choose_box(true, new Box(35), new Box(0))).v);
    println((await choose_box(false, new Box(0), new Box(36))).v);
    let copied = await copy_after(new Box(37));
    println(copied.v);
    let b38 = await make_from_static(38);
    println(b38.get());
    let h39 = new Holder("h", None);
    h39.child = Some(await make_box(39));
    println(await child_value(h39));
    let b40 = new Box(0);
    b40.v = await plus_i64(20, 20);
    println(b40.v);
    println(await flag_value(new FlagBox(true)));
    println(await float_half(new FloatBox(84.0)));
    println(await array_sum_gc(new Box(20), new Box(23)));
}

async fn part4() {
    let h44 = await make_holder("n", 44);
    println(await child_value(h44));
    println(await holder_child_copy_value(h44));
    let user_ch = Channel<User>::new();
    let up = user_producer(user_ch);
    let uc = user_consumer(user_ch);
    println(await uc);
    await up;
    let jh = make_holder("j", 47);
    println(await child_value(await jh));
    println((await nested_box(48)).v);
    println(await named_name(new User("last")));
    println(await read_value(new Box(50)));
}
"#,
    );
    assert!(ok, "{out}");
    assert_catalog_lines(
        &out,
        &[
            ("object_param_field", "1"),
            ("object_method_after_await", "2"),
            ("object_mutation_return", "4"),
            ("object_mutation_visible", "4"),
            ("async_returns_object", "5"),
            ("same_object_return", "5"),
            ("alias_mutation_return", "7"),
            ("alias_mutation_visible", "7"),
            ("set_after_await_return", "9"),
            ("set_after_await_visible", "9"),
            ("string_field_read", "a"),
            ("string_field_update", "ab"),
            ("string_field_visible", "ab"),
            ("option_child_some", "9"),
            ("option_child_none", "0"),
            ("nested_pair_sum", "15"),
            ("object_array_sum", "6"),
            ("object_array_assignment", "18"),
            ("object_channel_sum", "19"),
            ("async_returns_object_array", "20"),
            ("spawn_returns_object", "21"),
            ("spawn_reads_object", "22"),
            ("two_tasks_read_same_object", "40"),
            ("gc_object_across_await", "24"),
            ("gc_string_field_across_await", "gc"),
            ("interface_dispatch_after_await", "Ada"),
            ("interface_default_after_await", "hi Ada"),
            ("virtual_dispatch_after_await", "28"),
            ("option_some_object", "29"),
            ("option_none_object", "0"),
            ("result_ok_object", "31"),
            ("result_err_object", "0"),
            ("interface_downcast_after_await", "Rex!"),
            ("nullable_chain_sum", "6"),
            ("ternary_object_true", "35"),
            ("ternary_object_false", "36"),
            ("copy_method_after_await", "37"),
            ("static_constructor_after_await", "38"),
            ("nullable_field_assignment_await", "39"),
            ("field_assignment_await_scalar", "40"),
            ("bool_field_after_await", "true"),
            ("f64_field_after_await", "42"),
            ("gc_object_array_after_await", "43"),
            ("async_returns_nested_holder", "44"),
            ("copy_nullable_child", "44"),
            ("channel_user_object", "chan"),
            ("await_holder_then_continue", "47"),
            ("nested_async_object_return", "48"),
            ("interface_gc_final", "last"),
            ("final_object_read", "50"),
        ],
    );
}

#[test]
fn async_method_instance_static_and_gc_values() {
    let (out, ok) = compile_and_run_gc_stress(
        r#"
class Counter {
    pub value: i64;
    pub async fn add_after(self, n: i64) -> i64 {
        await sleep(1);
        self.value = self.value + n;
        return self.value;
    }
    pub static async fn twice(n: i64) -> i64 {
        await sleep(1);
        return n * 2;
    }
}
class Label {
    pub text: String;
    pub async fn suffix(self, s: String) -> String {
        await sleep(1);
        gc_collect();
        return self.text + s;
    }
}
async fn main() {
    let c = new Counter(10);
    let first = await c.add_after(5);
    println(first);
    let task = c.add_after(7);
    println(await task);
    println(c.value);
    let doubled = await Counter::twice(4);
    println(doubled);
    c.value = await Counter::twice(6);
    println(c.value);
    let label = new Label("async");
    let text = await label.suffix("-method");
    println(text);
}
"#,
    );
    assert!(ok, "{out}");
    assert_eq!(out, "15\n22\n22\n8\n12\nasync-method\n");
}

#[test]
fn async_method_dispatch_and_interface_task_surface() {
    let (out, ok) = compile_and_run_gc_stress(
        r#"
open class Base {
    pub open async fn score(self) -> i64 {
        await sleep(1);
        return 1;
    }
}
class Child extends Base {
    pub override async fn score(self) -> i64 {
        await sleep(1);
        return 9;
    }
}
interface AsyncGetter extends Sync {
    fn get(self) -> Task<i64>;
}
class Box implements AsyncGetter {
    pub v: i64;
    pub async fn get(self) -> i64 {
        await sleep(1);
        return self.v;
    }
}
async fn main() {
    let b: Base = new Child();
    let score = await b.score();
    println(score);
    let g: AsyncGetter = new Box(6);
    let value = await g.get();
    println(value);
}
"#,
    );
    assert!(ok, "{out}");
    assert_eq!(out, "9\n6\n");
}

#[test]
fn async_method_return_task_handle_annotation_is_rejected() {
    assert_compile_error_contains(
        r#"
class Bad {
    async fn work(self) -> Task<i64> {
        return 1;
    }
}
fn main() {}
"#,
        &[
            "error[E0809]",
            "async method return type must be the awaited value",
        ],
    );
}
