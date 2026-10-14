use super::*;

// ── Reassigned fields break `Sync` (willow-jz15.78) ──────────────────────────
//
// Willow fields have no readonly spelling, so a class is `Sync` only when all
// of its fields are `Sync` AND no field is written after construction.
// Perspectives:
//  1. The ticket repro (8 tasks racing on `c.n = c.n + 1`) is rejected.
//  2. The diagnostic names the field, the race, and the remedies, and labels
//     the field declaration.
//  3. Fields assigned only in `init` keep the class `Sync`.
//  4. A compound `self.f += …` write in a method breaks `Sync`.
//  5. A write through an immutable binding in a free function breaks `Sync`.
//  6. A subclass method writing an inherited field breaks the base's `Sync`.
//  7. A `&obj.f` reference argument counts as a write.
//  8. A nested `outer.inner.f = …` write breaks the inner AND the outer class.
//  9. A write to an object aliased from `self` inside an `init` lambda counts.
// 10. A lambda in `init` that does not write fields keeps the class `Sync`.
// 11. AtomicI64 / AtomicBool / Mutex / RwLock / FrozenArray fields that are
//     never reassigned stay `Sync`, and the program runs.
// 12. Reassigning a Mutex field itself breaks `Sync`.
// 13. `RwLock<C>` with a reassigned field in `C` is rejected (needs `Sync`).
// 14. `Mutex<C>` with a reassigned field in `C` is accepted (needs `Send`).
// 15. A class implementing a `Sync`-extending interface reports E2406.
// 16. Generic classes are covered (`Box<T>` with a setter).
// 17. Static field writes do not affect instance-field `Sync`.
// 18. A same-named field written via `self` in ANOTHER class is unaffected.
// 19. A same-named field written via an unresolved receiver is conservative.
// 20. Synchronous (non-async) calls are not affected by the rule.
// 21. A write in another module makes an imported class non-`Sync`.
// 22. A write in the entry module makes an imported class non-`Sync`.
// 23. An imported class that is never written stays `Sync` and runs.
// 24. The `Mutex`-shared counter runs with the exact total.
// 25. A diagnostic for a field declared in another module still names it.
// 26. Generic programs (template phase first) still see cross-module writes.
// 27. The runnable example compiles, runs, and prints exact totals.
// 28. An `init` write after `self` escaped to a task breaks `Sync`; writes
//     before the escape stay construction.
// 29. A subclass `init` write after `super.init()` whose base constructor let
//     `self` escape breaks `Sync`; a non-escaping base does not.

const REPRO: &str = r#"
import std::collections::Array;
class Counter { pub n: i64; }
async fn bump(c: Counter, times: i64) -> i64 {
    let mut i = 0;
    while i < times { c.n = c.n + 1; i = i + 1; }
    return 0;
}
async fn main() {
    let c = new Counter(0);
    let ts: Array<Task<i64>> = [];
    for i in 0..8 { ts.push(bump(c, 1000)); }
    for t in ts { await t; }
    println(c.n);
}
"#;

fn assert_not_sync(source: &str, needles: &[&str]) {
    let (ok, stderr) = compile_with_data_race_check(source);
    assert!(!ok, "expected rejection:\n{source}");
    for needle in needles {
        assert!(stderr.contains(needle), "missing `{needle}`:\n{stderr}");
    }
}

fn assert_accepted(source: &str) {
    let (ok, stderr) = compile_with_data_race_check(source);
    assert!(ok, "expected acceptance:\n{source}\n{stderr}");
}

const REASSIGNED: &str = "is reassigned after construction";

#[test]
fn test_jz15_78_repro_rejected() {
    assert_not_sync(
        REPRO,
        &[
            "error[E2402]",
            "cannot pass `Counter` to an async call: it is not `Sync`",
            "field `n: i64` is reassigned after construction, so tasks sharing a `Counter` would race on it",
        ],
    );
}

#[test]
fn test_jz15_78_diagnostic_points_at_field_and_names_remedies() {
    assert_not_sync(
        REPRO,
        &[
            "this field is reassigned after construction",
            "assign the field only in `init`",
            "AtomicI64",
            "Mutex<T>",
        ],
    );
}

#[test]
fn test_jz15_78_init_only_writes_stay_sync() {
    assert_accepted(
        r#"
class Config {
    pub n: i64;
    pub name: String;
    pub init(self, n: i64) { self.n = n; self.name = "cfg"; }
}
async fn read(c: Config) -> i64 { await sleep(1); return c.n; }
async fn main() { let c = new Config(3); println(await read(c)); }
"#,
    );
}

#[test]
fn test_jz15_78_method_compound_write_rejected() {
    assert_not_sync(
        r#"
class C { pub n: i64; pub fn bump(self) { self.n += 1; } }
async fn read(c: C) -> i64 { await sleep(1); return c.n; }
async fn main() { let c = new C(3); println(await read(c)); }
"#,
        &["error[E2402]", REASSIGNED],
    );
}

#[test]
fn test_jz15_78_write_through_immutable_binding_rejected() {
    assert_not_sync(
        r#"
class C { pub n: i64; }
fn reset(c: C) { c.n = 0; }
async fn read(c: C) -> i64 { await sleep(1); return c.n; }
async fn main() { let c = new C(3); reset(c); println(await read(c)); }
"#,
        &["error[E2402]", "field `n: i64` is reassigned"],
    );
}

#[test]
fn test_jz15_78_subclass_write_to_inherited_field_rejected() {
    assert_not_sync(
        r#"
open class Base { pub n: i64; }
class Sub extends Base { pub fn bump(self) { self.n = self.n + 1; } }
async fn read(b: Base) -> i64 { await sleep(1); return b.n; }
async fn main() { let b = new Base(3); println(await read(b)); }
"#,
        &["cannot pass `Base`", REASSIGNED],
    );
}

#[test]
fn test_jz15_78_reference_argument_counts_as_write() {
    assert_not_sync(
        r#"
class C { pub n: i64; }
fn inc(x: &mut i64) { x = x + 1; }
async fn read(c: C) -> i64 { await sleep(1); return c.n; }
async fn main() { let c = new C(1); inc(&c.n); println(await read(c)); }
"#,
        &["error[E2402]", REASSIGNED],
    );
}

#[test]
fn test_jz15_78_nested_write_breaks_inner_and_outer() {
    let decls = r#"
class Inner { pub v: i64; }
class Outer { pub inner: Inner; }
fn poke(o: Outer) { o.inner.v = 2; }
"#;
    assert_not_sync(
        &format!(
            "{decls}async fn read(o: Outer) -> i64 {{ await sleep(1); return o.inner.v; }}\nasync fn main() {{ let o = new Outer(new Inner(1)); poke(o); println(await read(o)); }}"
        ),
        &[
            "cannot pass `Outer`",
            "field `inner: Inner` -> field `v: i64`",
        ],
    );
    assert_not_sync(
        &format!(
            "{decls}async fn read(i: Inner) -> i64 {{ await sleep(1); return i.v; }}\nasync fn main() {{ println(await read(new Inner(1))); }}"
        ),
        &["cannot pass `Inner`", REASSIGNED],
    );
}

#[test]
fn test_jz15_78_init_lambda_write_through_alias_counts() {
    assert_not_sync(
        r#"
class C {
    pub n: i64;
    pub reset: fn() -> i64;
    pub init(self) {
        self.n = 0;
        let me = self;
        self.reset = || { me.n = 0; return 0; };
    }
}
async fn read(c: C) -> i64 { await sleep(1); return c.n; }
async fn main() { println(await read(new C())); }
"#,
        &["field `n: i64` is reassigned"],
    );
}

#[test]
fn test_jz15_78_init_write_after_self_escapes_rejected() {
    assert_not_sync(
        r#"
class C {
    pub n: i64;
    pub init(self) { self.n = 0; let t = reader(self); self.n = 5; }
}
async fn reader(c: C) -> i64 { await sleep(1); return c.n; }
async fn main() { println(new C().n); }
"#,
        &["cannot pass `C`", "field `n: i64` is reassigned"],
    );
    assert_accepted(
        r#"
class C {
    pub n: i64;
    pub init(self) { self.n = 0; self.n = self.n + 5; let t = reader(self); }
}
async fn reader(c: C) -> i64 { await sleep(1); return c.n; }
async fn main() { println(new C().n); }
"#,
    );
}

#[test]
fn test_jz15_78_write_after_escaping_super_init_rejected() {
    let base = r#"
open class P {
    pub x: i64;
    pub init(self) { self.x = 0; let t = reader(self); }
}
async fn reader(p: P) -> i64 { await sleep(1); return p.x; }
"#;
    assert_not_sync(
        &format!(
            "{base}class C extends P {{ pub init(self) {{ super.init(); self.x = 5; }} }}\nasync fn main() {{ println(new C().x); }}"
        ),
        &["cannot pass `P`", "field `x: i64` is reassigned"],
    );
    assert_accepted(
        r#"
open class P { pub x: i64; pub init(self) { self.x = 0; } }
class C extends P { pub y: i64; pub init(self) { super.init(); self.y = 1; } }
async fn read(c: C) -> i64 { await sleep(1); return c.x + c.y; }
async fn main() { println(await read(new C())); }
"#,
    );
}

#[test]
fn test_jz15_78_init_lambda_without_writes_stays_sync() {
    assert_accepted(
        r#"
class C {
    pub n: i64;
    pub init(self) { let k = |x: i64| { return x + 1; }; self.n = k(1); }
}
async fn read(c: C) -> i64 { await sleep(1); return c.n; }
async fn main() { println(await read(new C())); }
"#,
    );
}

#[test]
fn test_jz15_78_atomic_lock_and_frozen_fields_stay_sync_and_run() {
    let (out, ok) = compile_and_run(
        r#"
import std::collections::Array;
import std::collections::FrozenArray;
class Shared {
    pub hits: AtomicI64;
    pub done: AtomicBool;
    pub total: Mutex<i64>;
    pub limit: RwLock<i64>;
    pub weights: FrozenArray<i64>;
}
async fn work(s: Shared) -> i64 {
    await sleep(1);
    s.hits.add(1);
    lock s.total as mut t { t = t + s.weights[1]; }
    lock read s.limit as l { return l; }
}
async fn main() {
    let s = new Shared(AtomicI64::new(0), AtomicBool::new(false), Mutex::new(0), RwLock::new(9), [1, 2].freeze());
    let ts: Array<Task<i64>> = [];
    for i in 0..8 { ts.push(work(s)); }
    for t in ts { await t; }
    s.done.store(true);
    println(s.hits.load());
    lock s.total as t { println(t); }
}
"#,
    );
    assert!(ok, "{out}");
    assert_eq!(out, "8\n16\n");
}

#[test]
fn test_jz15_78_reassigned_mutex_field_rejected() {
    assert_not_sync(
        r#"
class Shared { pub m: Mutex<i64>; pub fn reset(self) { self.m = Mutex::new(0); } }
async fn work(s: Shared) -> i64 { await sleep(1); return 0; }
async fn main() { await work(new Shared(Mutex::new(0))); }
"#,
        &["field `m: Mutex<i64>` is reassigned"],
    );
}

#[test]
fn test_jz15_78_rwlock_of_mutable_class_rejected() {
    assert_not_sync(
        r#"
class C { pub n: i64; pub fn bump(self) { self.n += 1; } }
async fn r(x: RwLock<C>) -> i64 { await sleep(1); return 0; }
async fn main() { println(await r(RwLock::new(new C(1)))); }
"#,
        &["`RwLock<C>`", REASSIGNED],
    );
}

#[test]
fn test_jz15_78_mutex_of_mutable_class_accepted() {
    assert_accepted(
        r#"
class C { pub n: i64; pub fn bump(self) { self.n += 1; } }
async fn r(x: Mutex<C>) -> i64 { await sleep(1); lock x as c { c.bump(); return c.n; } }
async fn main() { println(await r(Mutex::new(new C(1)))); }
"#,
    );
}

#[test]
fn test_jz15_78_sync_interface_contract_rejects_reassigned_field() {
    assert_not_sync(
        r#"
interface Shareable extends Sync {}
class C implements Shareable { pub n: i64; pub fn bump(self) { self.n += 1; } }
fn main() {}
"#,
        &["error[E2406]", "field `n: i64` is reassigned"],
    );
    assert_accepted(
        r#"
interface Shareable extends Sync {}
class C implements Shareable { pub n: i64; }
fn main() { println(new C(1).n); }
"#,
    );
}

#[test]
fn test_jz15_78_generic_class_setter_rejected() {
    assert_not_sync(
        r#"
class Box<T> { pub v: T; pub fn set(self, v: T) { self.v = v; } }
async fn read(b: Box<i64>) -> i64 { await sleep(1); return b.v; }
async fn main() { println(await read(new Box<i64>(1))); }
"#,
        &["error[E2402]", "field `v: i64` is reassigned"],
    );
}

#[test]
fn test_jz15_78_static_field_writes_do_not_affect_instances() {
    let (ok, stderr) = compile_with_data_race_check(
        r#"
class C { pub static mut total: i64 = 0; pub n: i64; }
async fn read(c: C) -> i64 { await sleep(1); return c.n; }
async fn main() { C::total = 5; println(await read(new C(1))); }
"#,
    );
    assert!(ok, "{stderr}");
    assert!(!stderr.contains(REASSIGNED), "{stderr}");
}

#[test]
fn test_jz15_78_self_write_in_other_class_is_resolved() {
    assert_accepted(
        r#"
class A { pub n: i64; }
class B { pub n: i64; pub fn bump(self) { self.n += 1; } }
async fn read(a: A) -> i64 { await sleep(1); return a.n; }
async fn main() { println(await read(new A(1))); }
"#,
    );
}

#[test]
fn test_jz15_78_unresolved_receiver_is_conservative() {
    // Without types, `b.n = …` marks every instance field named `n`.
    assert_not_sync(
        r#"
class A { pub n: i64; }
class B { pub n: i64; }
async fn read(a: A) -> i64 { await sleep(1); return a.n; }
async fn main() { let b = new B(1); b.n = 2; println(await read(new A(1))); }
"#,
        &["cannot pass `A`", REASSIGNED],
    );
}

#[test]
fn test_jz15_78_synchronous_calls_unaffected() {
    let (out, ok) = compile_and_run(
        r#"
class C { pub n: i64; }
fn read(c: C) -> i64 { return c.n; }
fn main() { let c = new C(1); c.n = 2; println(read(c)); }
"#,
    );
    assert!(ok, "{out}");
    assert_eq!(out, "2\n");
}

const COUNTER_MODULE: &str = "pub class Counter { pub n: i64; }\n";

#[test]
fn test_jz15_78_write_in_other_module_rejects_imported_class() {
    let writer = "import counter::Counter;\npub fn bump(c: Counter) { c.n = c.n + 1; }\n";
    let main = r#"
import counter::Counter;
import writer;
async fn read(c: Counter) -> i64 { await sleep(1); return c.n; }
async fn main() { let c = new Counter(1); writer::bump(c); println(await read(c)); }
"#;
    let stderr = compile_temp_project_error_stderr(
        &[
            ("counter.wi", COUNTER_MODULE),
            ("writer.wi", writer),
            ("main.wi", main),
        ],
        "main.wi",
    );
    assert!(stderr.contains("error[E2402]"), "{stderr}");
    assert!(stderr.contains("field `n: i64` is reassigned"), "{stderr}");
}

#[test]
fn test_jz15_78_write_in_entry_rejects_imported_class() {
    let main = r#"
import counter::Counter;
async fn read(c: Counter) -> i64 { await sleep(1); return c.n; }
async fn main() { let c = new Counter(1); c.n = 5; println(await read(c)); }
"#;
    let stderr = compile_temp_project_error_stderr(
        &[("counter.wi", COUNTER_MODULE), ("main.wi", main)],
        "main.wi",
    );
    assert!(stderr.contains("error[E2402]"), "{stderr}");
    // The field lives in another file: the note names it instead of a label.
    assert!(stderr.contains("field `n: i64` is reassigned"), "{stderr}");
}

#[test]
fn test_jz15_78_aliased_escaping_base_init_rejected() {
    // The base is spelled through an import alias, so the subclass names a
    // class that is not declared under that name anywhere.
    let base = r#"
pub open class Base {
    pub x: i64;
    pub init(self) { self.x = 0; let t = reader(self); }
}
pub async fn reader(p: Base) -> i64 { await sleep(1); return p.x; }
"#;
    let main = r#"
import base::Base as B;
class C extends B { pub init(self) { super.init(); self.x = 5; } }
async fn main() { println(new C().x); }
"#;
    let stderr =
        compile_temp_project_error_stderr(&[("base.wi", base), ("main.wi", main)], "main.wi");
    assert!(stderr.contains("error[E2402]"), "{stderr}");
    assert!(stderr.contains("field `x: i64` is reassigned"), "{stderr}");
}

#[test]
fn test_jz15_78_generic_program_sees_cross_module_writes() {
    // Generic code routes the build through the template phase first; the
    // whole-program write summary must still reach the final check.
    let writer = "import counter::Counter;\npub fn bump(c: Counter) { c.n = c.n + 1; }\n";
    let main = r#"
import counter::Counter;
import writer;
fn id<T>(x: T) -> T { return x; }
async fn read(c: Counter) -> i64 { await sleep(1); return c.n; }
async fn main() { let c = id(new Counter(1)); writer::bump(c); println(await read(c)); }
"#;
    let stderr = compile_temp_project_error_stderr(
        &[
            ("counter.wi", COUNTER_MODULE),
            ("writer.wi", writer),
            ("main.wi", main),
        ],
        "main.wi",
    );
    assert!(stderr.contains("error[E2402]"), "{stderr}");
    assert!(stderr.contains("field `n: i64` is reassigned"), "{stderr}");
}

#[test]
fn test_jz15_78_unwritten_imported_class_stays_sync_and_runs() {
    let main = r#"
import counter::Counter;
async fn read(c: Counter) -> i64 { await sleep(1); return c.n; }
async fn main() { println(await read(new Counter(7))); }
"#;
    let (out, ok) = compile_temp_project_and_run(
        &[("counter.wi", COUNTER_MODULE), ("main.wi", main)],
        "main.wi",
    );
    assert!(ok, "{out}");
    assert_eq!(out, "7\n");
}

#[test]
fn test_jz15_78_mutex_shared_counter_runs_exactly() {
    let (out, ok) = compile_and_run(
        r#"
import std::collections::Array;
class Counter { pub n: i64; pub fn bump(self) { self.n = self.n + 1; } }
async fn bump(c: Mutex<Counter>, times: i64) -> i64 {
    let mut i = 0;
    while i < times { lock c as counter { counter.bump(); } i = i + 1; }
    return 0;
}
async fn main() {
    let c = Mutex::new(new Counter(0));
    let ts: Array<Task<i64>> = [];
    for i in 0..8 { ts.push(bump(c, 1000)); }
    for t in ts { await t; }
    lock c as counter { println(counter.n); }
}
"#,
    );
    assert!(ok, "{out}");
    assert_eq!(out, "8000\n");
}

#[test]
fn test_jz15_78_runnable_example() {
    let (out, ok) = compile_and_run(include_str!("../../../example/sync_field_reassignment.wi"));
    assert!(ok, "{out}");
    assert_eq!(out, "hits 800\ntotal 1600\nboxed 1600\n");
}
