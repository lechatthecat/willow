//! Relocatable root slots (willow-9tls.9): a minor collection copies young
//! survivors held by generated-code root slots and rewrites those slots
//! instead of pinning their nursery chunks. `WILLOW_GC_STRESS=relocate` moves
//! objects at every generated allocation and poisons moved payloads, so a
//! holder that kept a pre-move address prints garbage or crashes.
//! `WILLOW_GC_VERIFY_NO_PIN=1` aborts on any young pin, so the stress modes
//! below also prove that generated code never asks for one.

use super::support::*;

const NO_PIN: (&str, &str) = ("WILLOW_GC_VERIFY_NO_PIN", "1");
const DEBUG_MODES: &[&[(&str, &str)]] = &[
    &[],
    &[("WILLOW_GC_STRESS", "minor"), NO_PIN],
    &[("WILLOW_GC_STRESS", "relocate"), NO_PIN],
    &[
        ("WILLOW_GC_STRESS", "relocate"),
        ("WILLOW_GC_VERIFY_BARRIER", "1"),
        ("WILLOW_GC_VERIFY_REGIONS", "1"),
        NO_PIN,
    ],
];
const RELEASE_MODES: &[&[(&str, &str)]] = &[&[], &[("WILLOW_GC_STRESS", "relocate"), NO_PIN]];
/// Generated allocations are old under `alloc` stress, so nothing can pin;
/// only programs whose output does not report movement run in it.
const ALLOC_NO_PIN: &[(&str, &str)] = &[("WILLOW_GC_STRESS", "alloc"), NO_PIN];

fn assert_runs(project: &TestProject, modes: &[&[(&str, &str)]], expected: &str) {
    for env in modes {
        let output = project.run_with_env(env);
        let stdout = String::from_utf8_lossy(&output.stdout);
        assert!(
            output.status.success(),
            "{env:?} failed: {stdout}{}",
            String::from_utf8_lossy(&output.stderr)
        );
        assert_eq!(stdout, expected, "{env:?}");
    }
}

/// Run `source` in debug and release builds under the normal, minor and
/// relocate stress modes with pinning forbidden (debug also with barrier and
/// region verification).
fn assert_relocating_modes(name: &str, source: &str, expected: &str) -> TestProject {
    let project = TestProject::new(name, &[("main.wi", source)]);
    for release in [false, true] {
        let compiled = if release {
            project.compile_release("main.wi")
        } else {
            project.compile("main.wi")
        };
        assert!(
            compiled.status.success(),
            "{}",
            String::from_utf8_lossy(&compiled.stderr)
        );
        let modes = if release { RELEASE_MODES } else { DEBUG_MODES };
        assert_runs(&project, modes, expected);
    }
    project
}

#[test]
fn relocatable_roots_example_in_every_mode() {
    let source = include_str!("../../example/relocatable_roots.wi");
    assert_relocating_modes(
        "relocatable_example",
        source,
        "42\n7\n1225\n7\n25\n820\n36\n9\n6\n200\n45\n0\n0\ntrue\n",
    );
}

#[test]
fn rooted_survivor_benchmark_leaves_no_pinned_regions() {
    // Acceptance benchmark: survivors held only by locals while garbage churns
    // through many minor collections are copied, never pinned.
    let source = r#"
class Node {
    pub value: i64;
    pub next: Option<Node>;
}
fn churn(n: i64) -> i64 {
    let mut total = 0;
    let mut i = 0;
    while i < n {
        let t = new Node(i, Option::None);
        total = total + t.value;
        i = i + 1;
    }
    return total;
}
fn main() {
    let keep = new Node(7, Option::None);
    let other = new Node(8, Option::Some(keep));
    let pinned = gc_pinned_promotions();
    let regions = gc_pinned_region_count();
    let moved = gc_moved_objects();
    let minors = gc_minor_collections();
    let s = churn(20000);
    gc_minor_collect();
    gc_minor_collect();
    println(s);
    println(keep.value + other.value);
    println(gc_pinned_promotions() - pinned);
    println(gc_pinned_region_count() - regions);
    println(gc_moved_objects() > moved);
    println(gc_minor_collections() > minors + 1);
}
"#;
    assert_relocating_modes(
        "relocatable_survivor",
        source,
        "199990000\n15\n0\n0\ntrue\ntrue\n",
    );
}

#[test]
fn aliased_locals_and_heap_edges_follow_one_copy() {
    // Two slots and a field naming one object observe each other's writes
    // after the object moves; a moved holder's field is rewritten as well.
    let source = r#"
class Node { pub value: i64; }
class Holder { pub child: Node; }
fn main() {
    let a = new Node(1);
    let b = a;
    let h = new Holder(a);
    gc_minor_collect();
    b.value = 2;
    println(a.value);
    gc_minor_collect();
    h.child.value = 3;
    println(b.value);
    gc_minor_collect();
    gc_minor_collect();
    a.value = 4;
    println(h.child.value);
}
"#;
    assert_relocating_modes("relocatable_alias", source, "2\n3\n4\n");
}

#[test]
fn call_arguments_and_receivers_survive_callee_collections() {
    // Argument and receiver bridge roots are relocatable: the callee roots its
    // own copies, and no pinned region is left behind.
    let source = r#"
class Node {
    pub value: i64;
    pub fn churn_then_read(other: Node) -> i64 {
        let mut i = 0;
        while i < 64 {
            let garbage = new Node(i);
            i = i + 1 + garbage.value - garbage.value;
        }
        gc_minor_collect();
        return self.value * 10 + other.value;
    }
}
fn make(v: i64) -> Node { return new Node(v); }
fn pair(a: Node, b: Node) -> i64 {
    gc_minor_collect();
    let c = new Node(5);
    gc_minor_collect();
    return a.value + b.value + c.value;
}
fn main() {
    let pinned = gc_pinned_promotions();
    println(pair(new Node(1), make(2)));
    println(make(3).churn_then_read(make(4)));
    let n = new Node(6);
    println(n.churn_then_read(n));
    println(gc_pinned_promotions() - pinned);
}
"#;
    assert_relocating_modes("relocatable_calls", source, "8\n34\n66\n0\n");
}

#[test]
fn deep_recursion_frames_are_all_rewritten() {
    let source = r#"
class Node { pub value: i64; }
fn depth(n: i64) -> i64 {
    let here = new Node(n);
    if n == 0 {
        gc_minor_collect();
        return here.value;
    }
    let below = depth(n - 1);
    gc_minor_collect();
    return here.value + below;
}
fn main() {
    println(depth(200));
}
"#;
    assert_relocating_modes("relocatable_recursion", source, "20100\n");
}

#[test]
fn reassigned_loop_locals_tenure_through_survivor_copies() {
    // Survivors copy at age 1 and tenure at age 2 while the loop keeps
    // reassigning the slot; the list stays intact across both moves.
    let source = r#"
class Node {
    pub value: i64;
    pub next: Option<Node>;
}
fn main() {
    let mut head = new Node(0, Option::None);
    let mut i = 1;
    while i <= 300 {
        head = new Node(i, Option::Some(head));
        if i % 7 == 0 {
            gc_minor_collect();
        }
        i = i + 1;
    }
    let mut total = 0;
    let mut cursor = Option::Some(head);
    let mut done = false;
    while !done {
        match cursor {
            Option::Some(node) => {
                total = total + node.value;
                cursor = node.next;
            }
            Option::None => { done = true; }
        }
    }
    println(total);
}
"#;
    assert_relocating_modes("relocatable_tenure", source, "45150\n");
}

#[test]
fn interface_enum_option_and_closure_slots_move() {
    let source = r#"
interface Shape { fn area() -> i64; }
class Square implements Shape {
    pub side: i64;
    pub fn area() -> i64 { return self.side * self.side; }
}
class Node { pub value: i64; }
enum Slot { Empty, Full(Node) }
fn apply(f: closure(i64) -> i64, v: i64) -> i64 { return f(v); }
fn main() {
    let shape: Shape = new Square(4);
    let slot = Slot::Full(new Node(9));
    let maybe: Option<Node> = Option::Some(new Node(11));
    let captured = new Node(100);
    gc_minor_collect();
    gc_minor_collect();
    println(shape.area());
    match slot {
        Slot::Full(n) => println(n.value),
        Slot::Empty => println("empty"),
    }
    match maybe {
        Option::Some(n) => println(n.value),
        Option::None => println("none"),
    }
    println(apply(|x| x + captured.value, 5));
}
"#;
    assert_relocating_modes("relocatable_shapes", source, "16\n9\n11\n105\n");
}

#[test]
fn array_elements_and_strings_mix_with_moved_locals() {
    // Runtime-allocated buffers and strings never move; their young elements
    // move and the old buffer's remembered slots are rewritten.
    let source = r#"
import std::collections::Array;
class Node { pub value: i64; pub label: String; }
fn main() {
    let xs: Array<Node> = [];
    let mut i = 0;
    while i < 40 {
        let mut label = "odd";
        if i % 2 == 0 {
            label = "even";
        }
        xs.push(new Node(i, label + "-"));
        if i % 9 == 0 {
            gc_minor_collect();
        }
        i = i + 1;
    }
    let first = xs[0];
    gc_minor_collect();
    xs[1] = first;
    gc_minor_collect();
    let mut total = 0;
    let mut j = 0;
    while j < xs.len() {
        total = total + xs[j].value;
        j = j + 1;
    }
    println(total);
    println(xs[1].label + xs[39].label);
}
"#;
    assert_relocating_modes("relocatable_arrays", source, "779\neven-odd-\n");
}

#[test]
fn panic_recovery_and_defer_keep_moved_locals() {
    let source = r#"
class Node { pub value: i64; }
fn risky(n: Node) -> i64 {
    let local = new Node(n.value + 1);
    defer println(local.value);
    gc_minor_collect();
    if n.value > 0 {
        panic("boom");
    }
    return local.value;
}
fn guarded(n: Node) {
    defer match recover() {
        Some(_) => println("recovered"),
        None => println("clean"),
    }
    risky(n);
}
fn main() {
    let keep = new Node(41);
    guarded(new Node(0));
    guarded(keep);
    gc_minor_collect();
    println(keep.value);
}
"#;
    assert_relocating_modes("relocatable_panic", source, "1\nclean\n42\nrecovered\n41\n");
}

#[test]
fn async_tasks_and_worker_threads_relocate_safely() {
    // Several workers allocate and collect concurrently. Threads parked inside
    // runtime code publish their roots as in-place values; generated-code
    // safepoints publish rewritable slots.
    let source = r#"
import std::collections::Array;
import std::parallel;
class Node { pub value: i64; pub next: Option<Node>; }
fn build(seed: i64) -> i64 {
    let mut head = new Node(seed, Option::None);
    let mut i = 0;
    while i < 500 {
        head = new Node(i, Option::Some(head));
        if i % 100 == 0 {
            gc_minor_collect();
        }
        i = i + 1;
    }
    let mut total = 0;
    let mut cursor = Option::Some(head);
    let mut done = false;
    while !done {
        match cursor {
            Option::Some(node) => {
                total = total + node.value;
                cursor = node.next;
            }
            Option::None => { done = true; }
        }
    }
    return total;
}
async fn held(seed: i64) -> i64 {
    let keep = new Node(seed, Option::None);
    let mut i = 0;
    while i < 20 {
        let garbage = new Node(i, Option::None);
        await sleep(1);
        gc_minor_collect();
        i = i + 1 + garbage.value - garbage.value;
    }
    return keep.value;
}
async fn main() {
    let seeds: Array<i64> = [1, 2, 3, 4, 5, 6, 7, 8];
    let sums = await parallel::map(seeds.freeze(), build);
    let mut total = 0;
    let mut i = 0;
    while i < sums.len() {
        total = total + sums[i];
        i = i + 1;
    }
    println(total);
    let a = held(10);
    let b = held(20);
    let x = await a;
    let y = await b;
    println(x + y);
}
"#;
    let project = TestProject::new("relocatable_threads", &[("main.wi", source)]);
    let compiled = project.compile("main.wi");
    assert!(
        compiled.status.success(),
        "{}",
        String::from_utf8_lossy(&compiled.stderr)
    );
    for workers in ["1", "4"] {
        for budget in [None, Some("1")] {
            let mut env = vec![
                ("WILLOW_WORKERS", workers),
                ("WILLOW_GC_STRESS", "relocate"),
            ];
            if let Some(budget) = budget {
                env.push(("WILLOW_TASK_BUDGET", budget));
            }
            assert_runs(&project, &[&env], "998036\n30\n");
        }
    }
}

#[test]
fn alloc_stress_keeps_objects_in_place() {
    // Under `alloc` stress generated allocations are old, so nothing moves.
    let source = r#"
class Node { pub value: i64; }
fn main() {
    let moved = gc_moved_objects();
    let a = new Node(5);
    gc_minor_collect();
    println(a.value);
    println(gc_moved_objects() == moved);
}
"#;
    let project = TestProject::new("relocatable_alloc", &[("main.wi", source)]);
    assert!(project.compile("main.wi").status.success());
    assert_runs(&project, &[ALLOC_NO_PIN], "5\ntrue\n");
    assert_runs(&project, &[&[]], "5\nfalse\n");
}

#[test]
fn locals_and_call_bridges_use_relocatable_roots() {
    // Locals and direct-call argument/receiver bridges register rewritable
    // slots; no pinned alias root is emitted for them.
    let source = r#"
class Node {
    pub value: i64;
    pub fn get() -> i64 { gc_minor_collect(); return self.value; }
}
fn take(n: Node) -> i64 { gc_minor_collect(); return n.value; }
fn main() {
    let a = new Node(1);
    println(take(new Node(2)) + a.get() + new Node(3).get());
}
"#;
    let names = compile_and_collect_relocation_targets_all(source, &[]);
    assert!(
        names.iter().any(|name| name == "willow_push_root"),
        "{names:?}"
    );
    assert_no_pinned_entries(source);
}

#[test]
fn coercing_constructor_arguments_relocate() {
    // Constructor arguments are coerced after evaluation; their roots only
    // bridge to the constructor call, so they relocate without pinning.
    let source = r#"
interface Shape { fn area() -> i64; }
class Square implements Shape {
    pub side: i64;
    pub fn area() -> i64 { return self.side * self.side; }
}
class Holder {
    pub shape: Shape;
    pub count: i64;
}
fn main() {
    let h = new Holder(new Square(3), 2);
    gc_minor_collect();
    println(h.shape.area() * h.count);
}
"#;
    assert_relocating_modes("relocatable_ctor", source, "18\n");
}

#[test]
fn pair_boxing_stores_reload_moved_owners() {
    // Boxing an inline pair into a storage word allocates, so the owner the
    // word is stored into is reloaded from its relocatable root. Interface
    // coercions build inline pairs without allocating, so owners used across
    // them stay valid.
    let source = r#"
import std::collections::Array;
import std::collections::Map;

interface Shape { fn area(self) -> i64; }
class Square implements Shape {
    pub side: i64;
    pub fn area(self) -> i64 { return self.side * self.side; }
}
class Holder {
    pub shape: Shape;
    pub count: i64;
}
class Maker {
    pub static fn area_of(shape: Shape) -> i64 { return shape.area(); }
}

fn main() {
    let mut options: Array<Option<i64>> = [Option::None, Option::None];
    options[1] = Option::Some(5);
    options.push(Option::Some(6));
    println(options[1].unwrap() + options[2].unwrap());
    let mut shapes: Array<Shape> = [new Square(1)];
    shapes[0] = new Square(2);
    shapes.push(new Square(3));
    println(shapes[0].area() + shapes[1].area());
    let mut holder = new Holder(new Square(4), 2);
    holder.shape = new Square(5);
    println(holder.shape.area() * holder.count);
    let mut by_name: Map<String, Option<i64>> = Map::new();
    by_name.insert("k" + "1", Option::Some(7));
    let mut shapes_by_name: Map<String, Shape> = Map::new();
    shapes_by_name.insert("s" + "1", new Square(6));
    println(by_name.get("k1").unwrap().unwrap() + shapes_by_name.get("s1").unwrap().area());
    let channel: Channel<Option<i64>> = Channel::with_capacity(1);
    channel.send(Option::Some(8));
    println(channel.recv().unwrap());
    println(Maker::area_of(new Square(7)));
}
"#;
    assert_relocating_modes("relocatable_pair_boxing", source, "11\n13\n50\n43\n8\n49\n");
}

const FORMER_PIN_SITES: &str = r#"
import std::collections::Array;
import std::collections::Map;

interface Value extends Sync { fn get(self) -> i64; }
class Number implements Value {
    pub n: i64;
    pub fn get(self) -> i64 { return self.n; }
}
class Node { pub value: i64; }
enum Slot { Empty, Full(Node, String) }
class Problem { pub at: i64; }
class Wrapped implements Into<Problem> {
    pub at: i64;
    pub fn into(self) -> Problem { return new Problem(self.at + 100); }
}

fn fails(flag: bool) -> Result<i64, Wrapped> {
    if flag { return Result::Err(new Wrapped(5)); }
    return Result::Ok(3);
}
fn convert(flag: bool) -> Result<i64, Problem> {
    let v = fails(flag)?;
    return Result::Ok(v);
}
fn slot_value(slot: Slot) -> i64 {
    return match slot {
        Slot::Empty => 0,
        Slot::Full(node, label) => node.value + label.len(),
    };
}

fn main() -> Result<void, String> {
    defer { println("defer " + "done"); }
    let mut values: Map<String, Value> = Map::new();
    values.insert("a" + "1", new Number(4));
    values.insert("b" + "2", new Number(6));
    let mut list: Array<Value> = [];
    list.push(new Number(8));
    list.push(new Number(9));
    let base = new Node(1);
    let add = |a: Node, b: Node| a.value + b.value + base.value;
    let total = add(new Node(10), new Node(20));
    let start: Option<Node> = Option::Some(new Node(21));
    let doubled = start.map(|n: Node| new Node(n.value * 2));
    let slot = Slot::Full(new Node(30), "x" + "yz");
    let lock = Mutex::new(new Node(50));
    let problem = match convert(true) { Result::Ok(v) => v, Result::Err(p) => p.at };
    println(values.get("a1").unwrap().get() + values.get("b2").unwrap().get());
    println(list[0].get() + list[1].get());
    println(total);
    println(doubled.unwrap().value);
    println(slot_value(slot));
    println(problem);
    println("con" + "cat");
    return Result::Ok();
}
"#;

/// The pinned root entry point is gone from the runtime ABI (willow-9tls.9);
/// this guards against generated code reintroducing such a call.
fn assert_no_pinned_entries(source: &str) {
    let names = compile_and_collect_relocation_targets_all(source, &[]);
    assert!(
        !names.iter().any(|name| name == "willow_push_pinned_root"),
        "{names:?}"
    );
}

#[test]
fn temporaries_at_former_pin_sites_never_pin() {
    // Former pinned-temporary sites: map and array stores of boxed pairs,
    // closure calls, Option combinators, string concat, enum construction,
    // Mutex::new, `?` with Into and a `main` Result kept across an allocating
    // defer. None emits a pinned root entry, and every stress mode runs with
    // pinning forbidden.
    assert_no_pinned_entries(FORMER_PIN_SITES);
    let expected = "10\n17\n31\n42\n33\n105\nconcat\ndefer done\n";
    let project = assert_relocating_modes("relocatable_former_pins", FORMER_PIN_SITES, expected);
    assert_runs(&project, &[ALLOC_NO_PIN], expected);
}

#[test]
fn channel_and_select_sends_of_boxed_pairs_never_pin() {
    // Channel and select sends box inline pairs, then reload the channel.
    let source = r#"
interface Value extends Sync { fn get(self) -> i64; }
class Number implements Value {
    pub n: i64;
    pub fn get(self) -> i64 { return self.n; }
}

async fn main() {
    let ch: Channel<Value> = Channel::new();
    ch.send(new Number(3));
    let fresh: Value = new Number(4);
    select {
        ch.send(fresh) => { println(1); }
        default => { println(2); }
    }
    let first = ch.recv();
    let second = ch.recv();
    println(first.get() * 10 + second.get());
    let maybe: Channel<Option<i64>> = Channel::new();
    maybe.send(Option::Some(5));
    println(maybe.recv().unwrap());
}
"#;
    assert_no_pinned_entries(source);
    let project = assert_relocating_modes("relocatable_channel_pairs", source, "1\n34\n5\n");
    assert_runs(&project, &[ALLOC_NO_PIN], "1\n34\n5\n");
}

/// Interior `&`/`&mut` arguments pass a caller-owned `{base, offset}` cell
/// whose base slot is a relocatable root; the callee re-derives the address
/// at every access (docs/decisions/0011-reference-cell-abi.md). Every callee
/// here collects and allocates before and after using its reference.
const INTERIOR_REFERENCES: &str = r#"
import std::collections::Array;

class Node { pub value: i64; }
class Holder {
    pub count: i64;
    pub label: String;
    pub node: Node;
    pub lo: i64;
    pub hi: i64;
    pub items: Array<Node>;

    // A method passes its own (young, moving) receiver's field.
    pub fn bump_own(self) -> i64 {
        add_after_churn(&self.count, 3);
        return self.count;
    }
}
interface Stepper { fn nudge(self, value: &mut i64); }
class Step implements Stepper {
    pub by: i64;
    pub fn nudge(self, value: &mut i64) {
        churn(4);
        value = value + self.by;
    }
}
class Counter {
    pub start: i64;
    pub init(self, seed: &mut i64) {
        churn(4);
        seed = seed + 1;
        self.start = seed;
    }
}

fn churn(n: i64) -> i64 {
    let mut i = 0;
    let mut sum = 0;
    while i < n {
        sum = sum + new Node(i).value;
        i = i + 1;
    }
    gc_minor_collect();
    return sum;
}
fn add_after_churn(x: &mut i64, by: i64) {
    churn(8);
    x = x + by;
    churn(2);
}
fn read_after_churn(x: & i64) -> i64 {
    churn(8);
    return x;
}
fn rename(label: &mut String) {
    churn(4);
    label = label + "-" + "moved";
    churn(4);
}
fn replace_node(node: &mut Node, v: i64) {
    churn(4);
    node = new Node(v);
    churn(4);
}
fn read_node(node: & Node) -> i64 {
    churn(4);
    return node.value;
}
fn forward(x: &mut i64, depth: i64) {
    churn(2);
    if depth == 0 {
        x = x + 100;
        return;
    }
    forward(&x, depth - 1);
    churn(1);
}
fn spread(lo: &mut i64, hi: &mut i64) {
    churn(4);
    lo = lo - 1;
    hi = hi + 1;
}
fn pop_then_store(xs: Array<Node>, slot: &mut Node) -> i64 {
    let popped = xs.pop();
    churn(4);
    // The slot is past the logical length now; the store and the referent
    // must survive later collections through the still-traced buffer.
    slot = new Node(popped.value + 40);
    churn(8);
    return slot.value;
}
fn set_opt(slot: &mut Option<i64>, v: i64) {
    churn(4);
    slot = Option::Some(v);
    churn(2);
}
fn twice(a: &mut i64, b: &mut i64, c: & i64) -> i64 {
    churn(3);
    a = a + c;
    b = b + c;
    return a + b;
}

fn fresh() -> Holder {
    return new Holder(1, "h" + "x", new Node(5), 10, 20, [new Node(7), new Node(8)]);
}

fn main() {
    // 1. i64 field of a young object.
    let h = fresh();
    add_after_churn(&h.count, 1);
    println(h.count);
    // 2. Managed String field rebound to a young string.
    rename(&h.label);
    println(h.label);
    // 3. Object field replaced through `&mut`.
    replace_node(&h.node, 55);
    println(h.node.value);
    // 4. Shared object-field reference read after collections.
    println(read_node(&h.node));
    // 5. Two fields of one owner in one call.
    spread(&h.lo, &h.hi);
    println(h.lo * 100 + h.hi);
    // 6. Scalar array element.
    let mut nums: Array<i64> = [1, 2, 3];
    add_after_churn(&nums[2], 30);
    println(nums[2]);
    // 7. Managed array element replaced.
    let mut nodes: Array<Node> = [new Node(1), new Node(2)];
    replace_node(&nodes[1], 22);
    println(nodes[1].value);
    // 8. String array element.
    let mut names: Array<String> = ["a" + "b", "c" + "d"];
    rename(&names[0]);
    println(names[0]);
    // 9. Element popped through an alias, then stored through the reference.
    let mut stack: Array<Node> = [new Node(1), new Node(2), new Node(3)];
    println(pop_then_store(stack, &stack[2]));
    println(stack.len());
    stack.push(new Node(4));
    println(stack[2].value);
    // 10. Inline scalar pair element.
    let mut opts: Array<Option<i64>> = [Option::None, Option::Some(1)];
    set_opt(&opts[0], 9);
    println(opts[0].unwrap());
    // 11. Plain stack local.
    let mut n = 5;
    add_after_churn(&n, 2);
    println(n);
    // 12. Managed stack local.
    let mut text = "lo" + "cal";
    rename(&text);
    println(text);
    // 13. Forwarding chain and recursion.
    let k = fresh();
    forward(&k.count, 12);
    println(k.count);
    // 14. Interface (virtual) call with a field reference.
    let stepper: Stepper = new Step(6);
    stepper.nudge(&k.lo);
    println(k.lo);
    // 15. Constructor reference argument on a young field.
    let c = new Counter(&k.hi);
    println(c.start + k.hi);
    // 16. Method passing its own receiver's field.
    println(k.bump_own());
    // 17. Several references, shared and mutable, into different owners.
    let a = fresh();
    let b = fresh();
    println(twice(&a.lo, &b.hi, &k.count));
    println(a.lo + b.hi);
    // 18. Element of an array held in a field.
    replace_node(&h.items[0], 77);
    println(h.items[0].value);
    // 19. Shared scalar reference read after collections.
    println(read_after_churn(&h.items[1].value));
    // 20. Same call repeated in a loop: roots stay balanced.
    let mut i = 0;
    while i < 50 {
        add_after_churn(&h.count, 1);
        i = i + 1;
    }
    println(h.count);
    // 21. Reference into an object created in this statement's operands.
    let mut total = 0;
    let fresh_holder = fresh();
    total = total + twice(&fresh_holder.lo, &fresh_holder.hi, &n);
    println(total);
}
"#;

const INTERIOR_REFERENCES_OUTPUT: &str = "2\nhx-moved\n55\n55\n921\n33\n22\nab-moved\n43\n2\n4\n9\n7\n\
                                          local-moved\n101\n16\n42\n104\n238\n238\n77\n8\n52\n44\n";

#[test]
fn interior_reference_arguments_relocate_without_pinning() {
    // Perspectives (in program order): i64, String and object fields of a
    // young owner; a shared object-field read; two fields of one owner;
    // scalar, object and String array elements; an element popped through an
    // alias and then stored through the reference (the buffer's traced
    // high-water prefix keeps the referent); an inline scalar pair element;
    // scalar and managed stack locals; a 12-deep forwarding recursion; an
    // interface call; a constructor argument; a method passing its own
    // receiver's field; three references into three owners; an element of an
    // array held in a field; a field of an array element; 50 calls in a loop;
    // and references into an owner created just before the call. All run
    // with pinning forbidden under minor, relocate (with barrier and region
    // verification) and alloc stress, in debug and release builds.
    let project = assert_relocating_modes(
        "relocatable_interior_references",
        INTERIOR_REFERENCES,
        INTERIOR_REFERENCES_OUTPUT,
    );
    assert_runs(&project, &[ALLOC_NO_PIN], INTERIOR_REFERENCES_OUTPUT);
}

#[test]
fn async_frame_and_heap_references_relocate_without_pinning() {
    // A reference to an async frame local uses the frame as its cell base;
    // field and object references held across awaits by concurrent tasks.
    let source = r#"
class Node { pub value: i64; }
fn churn(n: i64) -> i64 {
    let mut i = 0;
    let mut sum = 0;
    while i < n {
        sum = sum + new Node(i).value;
        i = i + 1;
    }
    gc_minor_collect();
    return sum;
}
fn bump(x: &mut i64, by: i64) {
    churn(6);
    x = x + by;
}
fn replace(node: &mut Node, v: i64) {
    churn(6);
    node = new Node(v);
    churn(2);
}
async fn step(base: i64) -> i64 {
    // Frame locals of an async function live in its heap frame.
    let mut local = base;
    let holder = new Node(base);
    let mut slot = new Node(1);
    bump(&local, 1);
    await sleep(0);
    bump(&holder.value, 2);
    replace(&slot, base * 3);
    await sleep(0);
    return local + holder.value + slot.value;
}
async fn main() {
    let mut n = 10;
    bump(&n, 5);
    await sleep(0);
    println(n);
    let a = step(1);
    let b = step(100);
    println(await a);
    println(await b);
    let mut kept = new Node(7);
    replace(&kept, 70);
    await sleep(0);
    println(kept.value);
}
"#;
    let project =
        assert_relocating_modes("relocatable_async_references", source, "15\n8\n503\n70\n");
    assert_runs(&project, &[ALLOC_NO_PIN], "15\n8\n503\n70\n");
}

#[test]
fn async_main_args_survive_frame_allocation_collection() {
    // The `async fn main(args)` entry built the frame, then allocated the args
    // array while the frame lived only in a register. Under `alloc` stress
    // that collection freed the frame. The array is now built first and
    // rooted across the frame allocation.
    let source = r#"
import std::collections::Array;
class Node { pub value: i64; }
async fn main(args: Array<String>) {
    let node = new Node(args.len() + 7);
    await sleep(0);
    println(node.value);
}
"#;
    let project = assert_relocating_modes("relocatable_async_main_args", source, "7\n");
    assert_runs(&project, &[ALLOC_NO_PIN], "7\n");
}

#[test]
fn option_result_combinators_read_receivers_before_gc_points() {
    // `emit_flat_enum_method` roots the receiver and arguments relocatably
    // without reloading them: every combinator must read them before its
    // first GC point. Each callback here allocates, and `relocate` moves and
    // poisons survivors at every allocation, so a receiver read after the
    // callback (or after a result allocation) prints garbage or crashes.
    let source = r#"
class Node { pub value: i64; }
class Fail { pub code: i64; }

fn churn(n: i64) -> i64 {
    let mut i = 0;
    let mut sum = 0;
    while i < 8 {
        sum = sum + new Node(n + i).value;
        i = i + 1;
    }
    return sum;
}

fn some(v: i64) -> Option<Node> { return Option::Some(new Node(v)); }
fn none() -> Option<Node> { return Option::None; }
fn ok(v: i64) -> Result<Node, Fail> { return Result::Ok(new Node(v)); }
fn err(v: i64) -> Result<Node, Fail> { return Result::Err(new Fail(v)); }

fn main() {
    let fallback = new Node(-1);
    println(some(1).map(|n: Node| new Node(n.value + churn(1))).unwrap().value);
    println(some(2).and_then(|n: Node| some(n.value + churn(2))).unwrap().value);
    println(none().or_else(|| some(churn(3))).unwrap().value);
    println(some(4).or_else(|| some(churn(4))).unwrap().value);
    println(none().map(|n: Node| new Node(churn(n.value))).is_none());
    println(ok(5).map(|n: Node| new Node(n.value + churn(5))).unwrap().value);
    println(err(6).map_err(|f: Fail| new Fail(f.code + churn(6))).unwrap_err().code);
    println(ok(7).map_err(|f: Fail| new Fail(churn(f.code))).unwrap().value);
    println(err(8).map(|n: Node| new Node(churn(n.value))).unwrap_err().code);
    println(ok(9).and_then(|n: Node| ok(n.value + churn(9))).unwrap().value);
    println(err(10).and_then(|n: Node| ok(churn(n.value))).unwrap_err().code);
    println(err(11).or_else(|f: Fail| ok(f.code + churn(11))).unwrap().value);
    println(ok(12).or_else(|f: Fail| ok(churn(f.code))).unwrap().value);
    println(some(13).expect("present" + "!").value);
    println(none().unwrap_or(fallback).value);
    println(err(14).unwrap_or(new Node(churn(14))).value);
    println(ok(15).is_ok() && err(15).is_err() && some(15).is_some());
}
"#;
    assert_no_pinned_entries(source);
    assert_relocating_modes(
        "relocatable_combinators",
        source,
        "37\n46\n52\n4\ntrue\n73\n82\n7\n8\n109\n10\n127\n12\n13\n-1\n140\ntrue\n",
    );
}
