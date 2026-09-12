//! Receiver-root reuse (willow-dcre): stack snapshots pin SSA receivers;
//! async-frame snapshots must retain their additional direct root.
use super::support::{compile_and_collect_relocation_targets_mode, compile_and_run_gc_stress_all};

const CLASSES: &str = r#"
class Item {
    pub value: String;
    pub fn read(self) -> String { gc_collect(); return self.value + "!"; }
    pub fn with_arg(self, n: i64) -> String { gc_collect(); return self.value + n.toString(); }
    pub fn nested(self) -> String { return self.read(); }
    pub fn same(self) -> Item { gc_collect(); return self; }
    pub fn replace(self, slot: &mut Item) -> String {
        slot = new Item("new"); gc_collect(); return self.read();
    }
    pub async fn later(self) -> String { await sleep(1); gc_collect(); return self.read(); }
}
class Holder { pub item: Item; }
fn make() -> Item { return new Item("ok"); }
fn allocate() -> i64 { let x = make(); gc_collect(); return 7; }
fn replace(slot: &mut Item) -> i64 { slot = new Item("new"); gc_collect(); return 7; }
async fn wait_arg() -> i64 { await sleep(1); gc_collect(); return 7; }
"#;

macro_rules! stress {
    ($name:ident, $body:literal, $expected:literal) => {
        #[test]
        fn $name() {
            let source = format!("{CLASSES} {}", $body);
            let (out, ok) = compile_and_run_gc_stress_all(&source);
            assert!(ok, "{out}");
            assert_eq!(out, $expected);
        }
    };
}

stress!(
    local,
    "fn main() { let x = make(); println(x.read()); }",
    "ok!\n"
);
stress!(
    self_receiver,
    "fn main() { println(make().nested()); }",
    "ok!\n"
);
stress!(
    loop_binding,
    "fn main() { for x in [make(), make()] { println(x.read()); } }",
    "ok!\nok!\n"
);
stress!(
    temporary,
    "fn main() { println(new Item(\"ok\").read()); }",
    "ok!\n"
);
stress!(
    call_result,
    "fn main() { println(make().read()); }",
    "ok!\n"
);
stress!(
    field,
    "fn main() { let h = new Holder(make()); println(h.item.read()); }",
    "ok!\n"
);
stress!(
    array_element,
    "fn main() { let xs = [make()]; println(xs[0].read()); }",
    "ok!\n"
);
stress!(
    chained,
    "fn main() { println(make().same().same().read()); }",
    "ok!\n"
);
stress!(
    allocating_argument,
    "fn main() { let x = make(); println(x.with_arg(allocate())); }",
    "ok7\n"
);
stress!(
    temporary_allocating_argument,
    "fn main() { println(make().with_arg(allocate())); }",
    "ok7\n"
);
stress!(
    reassign_during_argument,
    "fn main() { let mut x = make(); println(x.with_arg(replace(&x))); println(x.read()); }",
    "ok7\nnew!\n"
);
stress!(
    reassign_during_callee,
    "fn main() { let mut x = make(); println(x.replace(&x)); println(x.read()); }",
    "ok!\nnew!\n"
);
stress!(
    deferred_call,
    "fn main() { let x = make(); defer { println(x.read()); } gc_collect(); }",
    "ok!\n"
);
stress!(
    loop_backedge,
    "fn main() { let x = make(); let mut i = 0; while i < 3 { println(x.read()); i = i + 1; } }",
    "ok!\nok!\nok!\n"
);
stress!(
    conditional_receiver,
    "fn main() { let yes = true; println((yes ? make() : new Item(\"bad\")).read()); }",
    "ok!\n"
);
stress!(
    parameter,
    "fn run(x: Item) { println(x.read()); } fn main() { run(make()); }",
    "ok!\n"
);
stress!(
    async_local,
    "async fn main() { let x = make(); await sleep(1); println(x.read()); }",
    "ok!\n"
);
stress!(
    async_frame_receiver,
    "async fn main() { let x = make(); println(x.with_arg(await wait_arg())); }",
    "ok7\n"
);
stress!(
    async_frame_temporary,
    "async fn main() { println(make().with_arg(await wait_arg())); }",
    "ok7\n"
);
stress!(
    async_method,
    "async fn main() { let x = make(); println(await x.later()); }",
    "ok!\n"
);

#[test]
fn runnable_example() {
    let (out, ok) =
        compile_and_run_gc_stress_all(include_str!("../../example/method_receiver_roots.wi"));
    assert!(ok, "{out}");
    assert_eq!(out, "original!\nreplacement!\n");
}

#[test]
fn repeated_stack_calls_add_only_entry_roots() {
    for release in [false, true] {
        let roots = |calls: usize| {
            let body = "total = total + x.step();".repeat(calls);
            let source = format!(
                "class C {{ pub n: i64; pub fn step(self) -> i64 {{ return self.n; }} }} fn main() {{ let x = new C(1); let mut total = 0; {body} println(total); }}"
            );
            compile_and_collect_relocation_targets_mode(&source, &[], release)
                .iter()
                .filter(|name| name.as_str() == "willow_push_root")
                .count()
        };
        // Lowering keeps both the operand and prepared receiver snapshots in
        // entry-rooted slots. Dispatch must not add a third root per call.
        assert_eq!(roots(8) - roots(1), 14, "release={release}");
    }
}
