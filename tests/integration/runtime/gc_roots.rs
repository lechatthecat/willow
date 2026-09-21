use super::*;

// ---------------------------------------------------------------------------
// GC rooting under allocation stress (WILLOW_GC_STRESS=alloc).
//
// These guard codegen GC-root soundness: every live value must survive a
// collection that fires *during* a subsequent allocation.  Without the fixes
// these exercise, each crashes or prints wrong output only when a collection
// happens to land mid-expression — invisible to normal threshold-based GC.
// ---------------------------------------------------------------------------

// Enum-variant construction must root the half-built enum across argument
// evaluation: `Option::Some(Node { .. })` allocates the Node after allocating
// the Option, and that allocation can collect the unrooted Option.
#[test]
fn gc_stress_01_option_some_class_payload() {
    let (out, ok) = compile_and_run_gc_stress(
        r#"
class Node {
    pub init(self, v: i64) {
        self.v = v;
    } v: i64; pub fn get(self) -> i64 { return self.v; } }
fn main() {
    let opt = Option::Some(new Node(8));
    gc_collect();
    let v = opt.unwrap();
    println(v.get());
}
"#,
    );
    assert!(ok, "should not crash under GC stress: {out}");
    assert_eq!(out, "8\n");
}

// Result::Ok with a String payload through the same construction path.
#[test]
fn gc_stress_02_result_ok_string_payload() {
    let (out, ok) = compile_and_run_gc_stress(
        r#"
fn main() {
    let r: Result<String, i64> = Result::Ok("alpha");
    gc_collect();
    println(r.unwrap());
}
"#,
    );
    assert!(ok, "should not crash under GC stress: {out}");
    assert_eq!(out, "alpha\n");
}

// Option<String> built and matched after a collection.
#[test]
fn gc_stress_03_option_string_survives() {
    let (out, ok) = compile_and_run_gc_stress(
        r#"
fn main() {
    let s = Option::Some("hello");
    gc_collect();
    println(s.unwrap());
}
"#,
    );
    assert!(ok, "should not crash under GC stress: {out}");
    assert_eq!(out, "hello\n");
}

// Fieldless (C-like) enums are immediate tags, not heap pointers, so a value of
// such an enum type must NOT be rooted/traced as a GC reference.  Passing one
// to a function that then allocates (the String literal) used to crash the
// collector by dereferencing the tag as an object header.
#[test]
fn gc_stress_04_fieldless_enum_not_rooted() {
    let (out, ok) = compile_and_run_gc_stress(
        r#"
enum Color { Red, Green, Blue, }
fn name(c: Color) -> String {
    return match c {
        Color::Red => "red",
        Color::Green => "green",
        Color::Blue => "blue",
    };
}
fn main() {
    println(name(Color::Red));
    println(name(Color::Green));
    println(name(Color::Blue));
}
"#,
    );
    assert!(ok, "should not crash under GC stress: {out}");
    assert_eq!(out, "red\ngreen\nblue\n");
}

// A class method returning Option, called twice.  Regression for the
// gc_root_count bookkeeping bug: the enum-construction root inside the method
// must decrement the root counter so the method epilogue does not over-pop the
// shared runtime root stack and strip the caller's live roots.
#[test]
fn gc_stress_05_class_method_returns_option_twice() {
    let (out, ok) = compile_and_run_gc_stress(
        r#"
class Lookup {
    pub init(self, key: i64, value: i64) {
        self.key = key;
        self.value = value;
    }
    key: i64;
    value: i64;
    pub fn find(self, k: i64) -> Option<i64> {
        if self.key == k {
            return Option::Some(self.value);
        }
        return Option::None;
    }
}
fn main() {
    let l = new Lookup(5, 100);
    println(l.find(5).unwrap());
    println(l.find(9).is_none());
}
"#,
    );
    assert!(ok, "should not crash under GC stress: {out}");
    assert_eq!(out, "100\ntrue\n");
}

// Enum with a payload-carrying variant IS heap-allocated and must survive a
// collection when held, including a fieldless variant (None) of the same enum.
#[test]
fn gc_stress_06_mixed_enum_variants_round_trip() {
    let (out, ok) = compile_and_run_gc_stress(
        r#"
fn pick(n: i64) -> Option<i64> {
    if n > 0 { return Option::Some(n * 2); }
    return Option::None;
}
fn main() {
    let mut i = 0;
    let mut total = 0;
    while i < 5 {
        let o = pick(i);
        gc_collect();
        total = total + o.unwrap_or(0);
        i = i + 1;
    }
    println(total);
}
"#,
    );
    assert!(ok, "should not crash under GC stress: {out}");
    assert_eq!(out, "20\n");
}

// Channel/Future locals are opaque RUNTIME pointers with no GC header, so
// is_gc_managed must NOT root them on the shadow stack — otherwise the collector
// reads a bogus header at payload_to_header and crashes once a collection scans
// the root (willow-lpn.9). Task/JoinHandle are GC async frames in the cooperative
// scheduler path, so it is safe and necessary to trace them.

// A void task awaited while collections fire on every allocation.
// The JoinHandle local is a GC frame and remains valid across collection.
#[test]
fn gc_stress_07_spawn_await_void() {
    let (out, ok) = compile_and_run_gc_stress(
        r#"
async fn say() {
    println("hi");
}
async fn main() {
    let h = say();
    gc_collect();
    await h;
    println("done");
}
"#,
    );
    assert!(ok, "task await must not crash under GC stress: {out}");
    assert_eq!(out, "hi\ndone\n");
}

// Awaiting task values of scalar types under stress. Task locals are async frame
// pointers and must remain traced across collection.
#[test]
fn gc_stress_08_task_await_scalars() {
    let (out, ok) = compile_and_run_gc_stress(
        r#"
async fn number() -> i64 {
    return 7;
}
async fn ratio() -> f64 {
    return 2.5;
}
async fn main() {
    let f = number();
    gc_collect();
    println(await f);
    println(await ratio());
}
"#,
    );
    assert!(ok, "await must not crash under GC stress: {out}");
    assert_eq!(out, "7\n2.5\n");
}

// A cancellation-aware await loads a GC reference from the completed task
// frame and allocates `Result::Ok(String)`. Collection at that allocation must
// see the String through the rooted task frame; after matching, the extracted
// binding must remain rooted too.
#[test]
fn gc_stress_09_task_result_string_payload() {
    let (out, ok) = compile_and_run_gc_stress(
        r#"
async fn work() -> String {
    await sleep(1);
    return "hel" + "lo";
}

async fn main() {
    match await work().result() {
        Ok(value) => {
            gc_collect();
            println(value);
        }
        Err(Cancelled) => println("cancelled"),
    }
}
"#,
    );
    assert!(ok, "TaskResult<String> must survive GC stress: {out}");
    assert_eq!(out, "hello\n");
}

// A channel produced by a spawned task, drained on the main task, with a
// collection between operations. The Channel local must not be traced.
#[test]
fn gc_stress_10_channel_spawn_producer() {
    let (out, ok) = compile_and_run_gc_stress(
        r#"
async fn producer(ch: Channel<i64>) {
    ch.send(10);
    ch.send(20);
    ch.close();
}
async fn main() {
    let ch = Channel<i64>::new();
    let h = producer(ch);
    gc_collect();
    println(ch.recv());
    println(ch.recv());
    await h;
}
"#,
    );
    assert!(ok, "channel/spawn must not crash under GC stress: {out}");
    assert_eq!(out, "10\n20\n");
}
