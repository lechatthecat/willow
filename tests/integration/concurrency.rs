use super::support::*;

#[test]
fn test_async_await_mvp_compiles_and_runs() {
    let (stdout, ok) = compile_and_run(
        r#"
async fn work() -> i64 {
    return 42;
}

async fn main() {
    let value = await work();
    println(value);
}
"#,
    );
    assert!(ok);
    assert_eq!(stdout, "42\n");
}

// ---------------------------------------------------------------------------
// Async state machines + async stack traces — willow-9lw acceptance.
// ---------------------------------------------------------------------------

// WILLOW_WORKERS contract (willow-gyaa.4): runs use at least five workers;
// WILLOW_WORKERS=N may request more. These pin result correctness for default,
// clamped-low, and larger explicit values.
const WORKERS_CONCURRENT_SRC: &str = r#"
async fn compute(n: i64) -> i64 {
    await sleep(1);
    return n * n;
}
async fn main() {
    let a = compute(1);
    let b = compute(2);
    let c = compute(3);
    println(a.join() + b.join() + c.join());
}
"#;

#[test]
fn test_workers_default_runs_concurrent_program() {
    let (out, ok) = compile_and_run(WORKERS_CONCURRENT_SRC);
    assert!(ok);
    assert_eq!(out, "14\n"); // 1 + 4 + 9
}

#[test]
fn test_workers_env_does_not_change_result() {
    // 1 and 4 clamp to five; 0 and garbage use the five-worker default.
    for value in ["1", "4", "0", "not-a-number"] {
        let (out, ok) =
            compile_and_run_with_env(WORKERS_CONCURRENT_SRC, &[("WILLOW_WORKERS", value)]);
        assert!(ok, "WILLOW_WORKERS={value} should run");
        assert_eq!(out, "14\n", "WILLOW_WORKERS={value} changed the result");
    }
}

#[test]
fn test_workers_high_count_still_correct_under_gc_stress() {
    let (out, ok) = compile_and_run_with_env(
        WORKERS_CONCURRENT_SRC,
        &[("WILLOW_WORKERS", "8"), ("WILLOW_GC_STRESS", "alloc")],
    );
    assert!(ok, "high worker count under GC stress should run");
    assert_eq!(out, "14\n");
}

#[test]
fn async_frame_large_warning_reports_function_and_size() {
    let mut source = String::from("async fn oversized() {\n");
    for index in 0..1020 {
        source.push_str(&format!("    let value_{index}: i64 = {index};\n"));
    }
    source.push_str("}\nfn main() {}\n");

    let (ok, stderr) = compile_with_compiler_env(&source, &[]);
    assert!(
        ok,
        "large async frame warning must not fail compilation: {stderr}"
    );
    assert!(stderr.contains("warning[W0801]"), "stderr: {stderr}");
    assert!(
        stderr.contains("async frame for `oversized` is large: 8192 bytes"),
        "stderr: {stderr}"
    );
    assert!(
        stderr.contains("avoid keeping large arrays or objects live across await points"),
        "stderr: {stderr}"
    );
}

#[test]
fn preempt_loop_backedge_allows_ready_task_to_run() {
    let source = r#"
async fn cpu_bound() -> i64 {
    let mut i = 0;
    while i < 100 {
        i = i + 1;
    }
    println(1);
    return i;
}
async fn quick() -> i64 {
    println(2);
    return 0;
}
async fn main() {
    let slow = cpu_bound();
    let fast = quick();
    slow.join();
    fast.join();
}
"#;

    let (out, ok) = compile_and_run_with_env(source, &[("WILLOW_TASK_BUDGET", "1")]);
    assert!(ok, "tiny-budget preemption program should run");
    assert_eq!(out, "2\n1\n");
}

#[test]
fn preempt_async_spin_does_not_starve_timer_task() {
    let source = r#"
async fn spin(done: AtomicBool) -> i64 {
    while !done.load() {
    }
    return 0;
}
async fn delayed(done: AtomicBool) -> i64 {
    await sleep(1);
    println(42);
    done.store(true);
    return 0;
}
fn main() {
    let done = AtomicBool::new(false);
    let background = spin(done);
    delayed(done).join();
    background.join();
}
"#;

    let (out, ok) = compile_and_run_with_env(source, &[("WILLOW_TASK_BUDGET", "1")]);
    assert!(ok, "background CPU spin must not starve the timer task");
    assert_eq!(out, "42\n");
}

#[test]
fn preempt_range_for_resumes_with_frame_backed_index() {
    let source = r#"
async fn sum() -> i64 {
    let mut total = 0;
    for i in 0..100 {
        total = total + i;
    }
    return total;
}
async fn main() {
    println(sum().join());
}
"#;

    let (out, ok) = compile_and_run_with_env(source, &[("WILLOW_TASK_BUDGET", "1")]);
    assert!(ok, "range-for should resume after every preemption");
    assert_eq!(out, "4950\n");
}

#[test]
fn preempt_array_for_keeps_gc_values_live() {
    let source = r#"
import std::collections::Array;
async fn concatenate() -> String {
    let values: Array<String> = ["a", "b", "c"];
    let mut out = "";
    for value in values {
        out = out + value;
    }
    return out;
}
async fn main() {
    println(concatenate().join());
}
"#;

    let (out, ok) = compile_and_run_with_env(
        source,
        &[("WILLOW_TASK_BUDGET", "1"), ("WILLOW_GC_STRESS", "alloc")],
    );
    assert!(ok, "array-for GC values must survive preemption");
    assert_eq!(out, "abc\n");
}

#[test]
fn preempt_statement_boundaries_interleave_straight_line_tasks() {
    let source = r#"
async fn first() -> i64 {
    println(1);
    println(3);
    return 0;
}
async fn second() -> i64 {
    println(2);
    return 0;
}
fn main() {
    let a = first();
    let b = second();
    a.join();
    b.join();
}
"#;

    let (out, ok) = compile_and_run_with_env(source, &[("WILLOW_TASK_BUDGET", "1")]);
    assert!(
        ok,
        "straight-line tasks should resume after statement safepoints"
    );
    let lines: Vec<_> = out.lines().collect();
    assert_eq!(lines.len(), 3, "{out}");
    for expected in ["1", "2", "3"] {
        assert!(lines.contains(&expected), "{out}");
    }
    assert!(
        lines.iter().position(|line| *line == "1") < lines.iter().position(|line| *line == "3"),
        "first task must preserve its own statement order: {out}"
    );
}

#[test]
fn preempt_await_channel_and_allocation_statements_resume() {
    let source = r#"
async fn producer(ch: Channel<String>) -> i64 {
    await sleep(1);
    let value = "pre" + "empt";
    ch.send(value);
    return 0;
}
async fn consumer(ch: Channel<String>) -> String {
    let value = ch.recv();
    return value;
}
async fn main() {
    let ch = Channel<String>::new();
    let p = producer(ch);
    let c = consumer(ch);
    println(c.join());
    p.join();
}
"#;

    let (out, ok) = compile_and_run_with_env(
        source,
        &[("WILLOW_TASK_BUDGET", "1"), ("WILLOW_GC_STRESS", "alloc")],
    );
    assert!(ok, "await/channel/allocation statements must resume safely");
    assert_eq!(out, "preempt\n");
}

// Concurrency unification (willow-h2vf Stage 1): an async fn call returns an
// eager Task that is joinable directly — no `spawn` needed.
// ── Send / Sync marker interfaces (willow-dgwo.1) ────────────────────────────
//
// 20 test perspectives for the compiler-known Send/Sync markers:
//  1. `class C implements Send` is rejected (E2401).
//  2. `class C implements Sync` is rejected (E2401).
//  3. The diagnostic names it a "compiler-known marker interface".
//  4. The help points at Mutex/RwLock/Atomic/Channel/frozen.
//  5. `implements Send` is rejected even with no fields.
//  6. `implements Sync` is rejected even with only immutable fields.
//  7. Markers are in scope with NO import (prelude).
//  8. `interface I extends Send` is allowed.
//  9. `interface I extends Sync` is allowed.
// 10. A class implementing a Send-extending interface compiles and runs.
// 11. A chained `extends` (Pet→Named→Sync) does not produce a false E2401.
// 12. The transitive marker is not mistaken for a manual impl.
// 13. `implements Animal, Send` still flags the Send (manual impl).
// 14. A Send-extending interface value dispatches correctly at runtime.
// 15. Normal programs (no markers) are unaffected by the prelude additions.
// 16. `implements Send` reports at the offending class.
// 17. One bad class does not suppress other valid classes.
// 18. A class can implement a real interface AND not be forced to name markers.
// 19. Markers work as an interface bound across module-free single files.
// 20. Existing interface conformance/dispatch is unchanged (regression suite).
#[test]
fn test_send_marker_manual_impl_rejected_e2401() {
    assert_compile_error_contains(
        "class Bad implements Send { value: i64; pub init(self, value: i64) { self.value = value; } }\nfn main() {}\n",
        &[
            "error[E2401]",
            "`Send` is a compiler-known marker interface",
            "cannot be implemented manually",
        ],
    );
}

#[test]
fn test_sync_marker_manual_impl_rejected_e2401() {
    assert_compile_error_contains(
        "class Bad implements Sync { value: i64; pub init(self, value: i64) { self.value = value; } }\nfn main() {}\n",
        &["error[E2401]", "`Sync`", "cannot be implemented manually"],
    );
}

#[test]
fn test_send_marker_e2401_help_mentions_safe_wrappers() {
    assert_compile_error_contains(
        "class Bad implements Sync {}\nfn main() {}\n",
        &["error[E2401]", "Mutex", "Channel"],
    );
}

#[test]
fn test_send_marker_rejected_even_with_no_fields() {
    assert_compile_error_contains(
        "class Empty implements Send {}\nfn main() {}\n",
        &["error[E2401]"],
    );
}

#[test]
fn test_marker_alongside_real_interface_still_flags_marker() {
    assert_compile_error_contains(
        r#"
interface Animal { fn speak(self) -> String; }
class Dog implements Animal, Send {
    pub fn speak(self) -> String { return "woof"; }
}
fn main() {}
"#,
        &["error[E2401]", "`Send`"],
    );
}

#[test]
fn test_interface_extends_send_is_allowed_and_runs() {
    let (out, ok) = compile_and_run(
        r#"
interface Job extends Send { fn run(self) -> i64; }
class Square implements Job {
    pub value: i64;
    pub fn run(self) -> i64 { return self.value * self.value; }
}
fn use_job(j: Job) -> i64 { return j.run(); }
fn main() { println(use_job(new Square(6))); }
"#,
    );
    assert!(ok);
    assert_eq!(out, "36\n");
}

#[test]
fn test_interface_extends_sync_is_allowed() {
    let (out, ok) = compile_and_run(
        r#"
interface Shared extends Sync { fn tag(self) -> i64; }
class Tag implements Shared {
    pub fn tag(self) -> i64 { return 7; }
}
fn main() { println(new Tag().tag()); }
"#,
    );
    assert!(ok);
    assert_eq!(out, "7\n");
}

#[test]
fn test_chained_extends_marker_no_false_e2401() {
    // Pet -> Named -> Sync; a class implementing Pet transitively "has" Sync but
    // must NOT be flagged as manually implementing it.
    let (out, ok) = compile_and_run(
        r#"
interface Named extends Sync { fn name(self) -> String; }
interface Pet extends Named { fn owner(self) -> String; }
class Dog implements Pet {
    pub fn name(self) -> String { return "Rex"; }
    pub fn owner(self) -> String { return "Sam"; }
}
fn main() { println(new Dog().name()); }
"#,
    );
    assert!(ok);
    assert_eq!(out, "Rex\n");
}

#[test]
fn test_markers_available_without_import() {
    // No `import` line — Send/Sync come from the prelude.
    let (out, ok) = compile_and_run(
        r#"
interface Task2 extends Send { fn go(self) -> i64; }
class Go implements Task2 { pub fn go(self) -> i64 { return 1; } }
fn main() { println(new Go().go()); }
"#,
    );
    assert!(ok);
    assert_eq!(out, "1\n");
}

#[test]
fn test_send_extending_interface_dispatches_at_runtime() {
    let (out, ok) = compile_and_run(
        r#"
interface Job extends Send { fn run(self) -> i64; }
class A implements Job { pub fn run(self) -> i64 { return 10; } }
class B implements Job { pub fn run(self) -> i64 { return 20; } }
fn run_it(j: Job) -> i64 { return j.run(); }
fn main() {
    println(run_it(new A()));
    println(run_it(new B()));
}
"#,
    );
    assert!(ok);
    assert_eq!(out, "10\n20\n");
}

#[test]
fn test_prelude_markers_do_not_break_normal_program() {
    let (out, ok) = compile_and_run("fn main() { println(42); }\n");
    assert!(ok);
    assert_eq!(out, "42\n");
}

// ── Async-call capture checking (willow-dgwo.4) ──────────────────────────────
// Enforced by default because Willow always runs with at least five workers, or
// explicitly via WILLOW_DATA_RACE_CHECK.
//
// 20 perspectives (this block + the dgwo.2 classifier unit tests in
// type_checker/send_sync.rs cover the underlying type rules):
//  1. non-Sync GC arg (Array) rejected under the check (E2402)
//  2. a low worker override is clamped and still rejected
//  3. E2402 help names the safe wrappers
//  4. Map arg rejected
//  5. Option<Array> arg rejected
//  6. class with an Array field rejected
//  7. Sync class (all-i64 fields) accepted
//  8. Mutex / Channel / Atomic / i64 / String args accepted together
//  9. RwLock<i64> arg accepted
// 10. RwLock<Array<i64>> arg rejected (inner not Sync)
// 11. Mutex<Array<i64>> arg accepted (Mutex needs only inner Send)
// 12. AtomicBool arg accepted
// 13. fieldless enum arg accepted (scalar tag, Send+Sync)
// 14. payload enum carrying an Array rejected
// 15. only the offending arg is flagged (good args alongside)
// 16. a NON-async call passing an Array is NOT checked (no Task boundary)
// 17. passing a Task<T> handle as an arg is rejected (Task is not Sync)
// 18. multiple non-Sync args each report E2402
// 19. scalar-only async fn accepted
// 20. nested async call forwarding a Sync arg is accepted
const NONSYNC_ARG_SRC: &str = r#"
import std::collections::Array;
async fn use_xs(xs: Array<i64>) -> i64 { await sleep(1); return xs[0]; }
async fn main() { let xs: Array<i64> = [1, 2, 3]; println(await use_xs(xs)); }
"#;

#[test]
fn test_dgwo4_nonsync_gc_arg_rejected_under_check() {
    let (ok, stderr) = compile_with_data_race_check(NONSYNC_ARG_SRC);
    assert!(!ok, "non-Sync Array arg should be rejected");
    assert!(stderr.contains("error[E2402]"), "{stderr}");
    assert!(stderr.contains("not `Sync`"), "{stderr}");
}

#[test]
fn test_dgwo4_low_worker_override_still_rejects_nonsync_arg() {
    let (ok, stderr) = compile_with_compiler_env(NONSYNC_ARG_SRC, &[("WILLOW_WORKERS", "1")]);
    assert!(!ok, "WILLOW_WORKERS=1 must be clamped to five");
    assert!(stderr.contains("error[E2402]"), "{stderr}");
}

#[test]
fn test_dgwo4_e2402_help_mentions_safe_wrappers() {
    let (_ok, stderr) = compile_with_data_race_check(NONSYNC_ARG_SRC);
    assert!(
        stderr.contains("Mutex") && stderr.contains("Channel"),
        "{stderr}"
    );
}

#[test]
fn test_dgwo4_map_arg_rejected() {
    let (ok, stderr) = compile_with_data_race_check(
        r#"
import std::collections::Map;
async fn use_m(m: Map<String, i64>) -> i64 { await sleep(1); return 0; }
async fn main() { let m: Map<String, i64> = Map::new(); println(await use_m(m)); }
"#,
    );
    assert!(!ok);
    assert!(stderr.contains("error[E2402]"), "{stderr}");
}

#[test]
fn test_dgwo4_option_of_array_rejected() {
    let (ok, _stderr) = compile_with_data_race_check(
        r#"
import std::collections::Array;
async fn use_o(o: Option<Array<i64>>) -> i64 { await sleep(1); return 0; }
async fn main() { let o: Option<Array<i64>> = Option::None; println(await use_o(o)); }
"#,
    );
    assert!(!ok, "Option<Array> is not Sync, should be rejected");
}

#[test]
fn test_dgwo4_sync_and_scalar_args_accepted() {
    // Mutex/Channel/AtomicI64 (Sync) + i64/String (Send/Sync) all pass.
    let (ok, stderr) = compile_with_data_race_check(
        r#"
async fn worker(m: Mutex<i64>, ch: Channel<i64>, a: AtomicI64, n: i64, s: String) -> i64 {
    await sleep(1);
    return n;
}
async fn main() {
    let m = Mutex::new(0);
    let ch = Channel<i64>::new();
    let a = AtomicI64::new(0);
    println(await worker(m, ch, a, 7, "hi"));
}
"#,
    );
    assert!(ok, "Sync/Send args should be accepted: {stderr}");
}

#[test]
fn test_dgwo4_class_with_array_field_rejected() {
    // A class with a (non-Sync) Array field is not Sync.
    let (ok, _stderr) = compile_with_data_race_check(
        r#"
import std::collections::Array;
class Bag { pub xs: Array<i64>; }
async fn use_b(b: Bag) -> i64 { await sleep(1); return 0; }
async fn main() { let b = new Bag([1, 2]); println(await use_b(b)); }
"#,
    );
    assert!(!ok, "class with Array field is not Sync");
}

#[test]
fn test_dgwo4_sync_class_accepted() {
    // A class whose fields are all Sync (i64) is Sync and accepted.
    let (ok, stderr) = compile_with_data_race_check(
        r#"
class Point { pub x: i64; pub y: i64; }
async fn use_p(p: Point) -> i64 { await sleep(1); return p.x; }
async fn main() { let p = new Point(1, 2); println(await use_p(p)); }
"#,
    );
    assert!(ok, "all-i64-field class is Sync: {stderr}");
}

#[test]
fn test_dgwo4_rwlock_inner_sync_accepted_else_rejected() {
    // 9: RwLock<i64> accepted (i64 is Send+Sync).
    let (ok, _) = compile_with_data_race_check(
        r#"
async fn r(x: RwLock<i64>) -> i64 { await sleep(1); return x.read(); }
async fn main() { let x = RwLock::new(1); println(await r(x)); }
"#,
    );
    assert!(ok);
    // 10: RwLock<Array<i64>> rejected (Array is not Sync, RwLock needs Sync).
    let (ok2, _) = compile_with_data_race_check(
        r#"
import std::collections::Array;
async fn r(x: RwLock<Array<i64>>) -> i64 { await sleep(1); return 0; }
async fn main() { let x = RwLock<Array<i64>>::new([1]); println(await r(x)); }
"#,
    );
    assert!(!ok2);
}

#[test]
fn test_dgwo4_mutex_of_array_accepted_and_atomicbool() {
    // 11: Mutex<Array<i64>> accepted (Mutex only needs inner Send).
    // 12: AtomicBool accepted.
    let (ok, stderr) = compile_with_data_race_check(
        r#"
import std::collections::Array;
async fn w(m: Mutex<Array<i64>>, f: AtomicBool) -> i64 { await sleep(1); return 0; }
async fn main() {
    let m = Mutex<Array<i64>>::new([1, 2]);
    let f = AtomicBool::new(false);
    println(await w(m, f));
}
"#,
    );
    assert!(ok, "Mutex<Array> + AtomicBool should be accepted: {stderr}");
}

#[test]
fn test_dgwo4_fieldless_enum_accepted_payload_enum_with_array_rejected() {
    // 13: fieldless enum is a scalar tag (Send+Sync) — accepted.
    let (ok, _) = compile_with_data_race_check(
        r#"
enum Color { Red, Green, Blue }
async fn c(x: Color) -> i64 { await sleep(1); return 0; }
async fn main() { println(await c(Color::Red)); }
"#,
    );
    assert!(ok);
    // 14: payload enum carrying an Array is not Sync — rejected.
    let (ok2, _) = compile_with_data_race_check(
        r#"
import std::collections::Array;
enum Holder { Of(Array<i64>) }
async fn h(x: Holder) -> i64 { await sleep(1); return 0; }
async fn main() { println(await h(Holder::Of([1]))); }
"#,
    );
    assert!(!ok2);
}

#[test]
fn test_dgwo4_non_async_call_is_not_checked() {
    // 16: a synchronous call passing an Array crosses no task boundary — never
    // checked, even with the data-race check on.
    let (ok, stderr) = compile_with_data_race_check(
        r#"
import std::collections::Array;
fn use_xs(xs: Array<i64>) -> i64 { return xs[0]; }
fn main() { let xs: Array<i64> = [7, 8]; println(use_xs(xs)); }
"#,
    );
    assert!(ok, "sync call must not be capture-checked: {stderr}");
}

#[test]
fn test_dgwo4_only_offending_args_flagged() {
    // 15 + 18: with several args, exactly the non-Sync ones report E2402.
    let (ok, stderr) = compile_with_data_race_check(
        r#"
import std::collections::Array;
async fn f(a: i64, xs: Array<i64>, m: Mutex<i64>, ys: Array<i64>) -> i64 { await sleep(1); return a; }
async fn main() {
    let xs: Array<i64> = [1];
    let ys: Array<i64> = [2];
    let m = Mutex::new(0);
    println(await f(9, xs, m, ys));
}
"#,
    );
    assert!(!ok);
    assert_eq!(stderr.matches("error[E2402]").count(), 2, "{stderr}");
}

// ── FrozenArray<T> (willow-dgwo.7) ───────────────────────────────────────────
// Perspectives: 1 freeze+len; 2 indexing read; 3 independent copy (original
// mutation does not leak); 4 push rejected; 5 pop rejected; 6 index-assign
// rejected; 7 unknown method rejected; 8 freeze takes no args; 9 FrozenArray<i64>
// is Sync (passable to async under the check); 10 FrozenArray<Array<i64>> is not
// Sync (rejected); 11 FrozenArray<String> ok; 12 low worker values still check.
#[test]
fn test_frozen_array_freeze_len_index_and_independent_copy() {
    let (out, ok) = compile_and_run(
        r#"
import std::collections::Array;
fn main() {
    let xs: Array<i64> = [10, 20, 30];
    let fa = xs.freeze();
    println(fa.len());   // 3
    println(fa[0]);      // 10
    println(fa[2]);      // 30
    xs.push(40);
    println(fa.len());   // 3 (independent of the original)
    println(xs.len());   // 4
}
"#,
    );
    assert!(ok);
    assert_eq!(out, "3\n10\n30\n3\n4\n");
}

#[test]
fn test_frozen_array_push_rejected() {
    assert_compile_error_contains(
        "import std::collections::Array;\nfn main() { let f = [1, 2].freeze(); f.push(3); }\n",
        &["error[E0201]", "immutable"],
    );
}

#[test]
fn test_frozen_array_pop_rejected() {
    assert_compile_error_contains(
        "import std::collections::Array;\nfn main() { let f = [1, 2].freeze(); f.pop(); }\n",
        &["error[E0201]", "immutable"],
    );
}

#[test]
fn test_frozen_array_index_assign_rejected() {
    assert_compile_error_contains(
        "import std::collections::Array;\nfn main() { let f = [1, 2].freeze(); f[0] = 9; }\n",
        &["error[E0201]"],
    );
}

#[test]
fn test_frozen_array_unknown_method_rejected() {
    assert_compile_error_contains(
        "import std::collections::Array;\nfn main() { let f = [1, 2].freeze(); f.frob(); }\n",
        &["error[E0201]", "no method `frob`"],
    );
}

#[test]
fn test_frozen_array_freeze_takes_no_args() {
    assert_compile_error_contains(
        "import std::collections::Array;\nfn main() { let xs: Array<i64> = [1]; let f = xs.freeze(7); }\n",
        &["error[E0201]"],
    );
}

#[test]
fn test_frozen_array_is_sync_passable_to_async() {
    // FrozenArray<i64> is Sync, so it is accepted by the data-race check.
    let (ok, stderr) = compile_with_data_race_check(
        r#"
import std::collections::Array;
async fn t(fa: FrozenArray<i64>) -> i64 { await sleep(1); return fa.len(); }
async fn main() { let fa = [1, 2, 3].freeze(); println(await t(fa)); }
"#,
    );
    assert!(ok, "FrozenArray<i64> should be Sync: {stderr}");
}

#[test]
fn test_frozen_array_string_is_sync() {
    let (ok, _) = compile_with_data_race_check(
        r#"
import std::collections::Array;
async fn t(fa: FrozenArray<String>) -> i64 { await sleep(1); return fa.len(); }
async fn main() { let fa: Array<String> = ["a", "b"]; println(await t(fa.freeze())); }
"#,
    );
    assert!(ok);
}

#[test]
fn test_frozen_array_of_array_not_sync_rejected() {
    // FrozenArray<Array<i64>> follows its element: inner Array is not Sync.
    let (ok, _) = compile_with_data_race_check(
        r#"
import std::collections::Array;
async fn t(fa: FrozenArray<Array<i64>>) -> i64 { await sleep(1); return fa.len(); }
async fn main() {
    let inner: Array<i64> = [1];
    let outer: Array<Array<i64>> = [inner];
    println(await t(outer.freeze()));
}
"#,
    );
    assert!(!ok, "FrozenArray<Array<i64>> is not Sync");
}

// ── FrozenMap<K,V> (willow-dgwo.10) ──────────────────────────────────────────
// Perspectives: 1 freeze+len; 2 contains; 3 get->Option<V>; 4 independent copy;
// 5 insert rejected; 6 remove rejected; 7 unknown method rejected; 8 freeze no
// args; 9 FrozenMap<String,i64> is Sync (passable to async under the check).
#[test]
fn test_frozen_map_reads_and_independent_copy() {
    let (out, ok) = compile_and_run(
        r#"
import std::collections::Map;
fn main() {
    let m: Map<String, i64> = Map::new();
    m.insert("a", 1);
    m.insert("b", 2);
    let fm = m.freeze();
    println(fm.len());           // 2
    println(fm.contains("a"));   // true
    println(fm.contains("z"));   // false
    println(match fm.get("b") { Option::Some(v) => v, Option::None => -1 });  // 2
    m.insert("c", 3);
    println(m.len());            // 3
    println(fm.len());           // 2 (independent)
}
"#,
    );
    assert!(ok);
    assert_eq!(out, "2\ntrue\nfalse\n2\n3\n2\n");
}

#[test]
fn test_frozen_map_insert_rejected() {
    assert_compile_error_contains(
        "import std::collections::Map;\nfn main() { let f = Map<String, i64>::new().freeze(); f.insert(\"x\", 1); }\n",
        &["error[E0201]", "immutable"],
    );
}

#[test]
fn test_frozen_map_unknown_method_rejected() {
    assert_compile_error_contains(
        "import std::collections::Map;\nfn main() { let f = Map<String, i64>::new().freeze(); f.frob(); }\n",
        &["error[E0201]", "no method `frob`"],
    );
}

#[test]
fn test_frozen_map_freeze_takes_no_args() {
    assert_compile_error_contains(
        "import std::collections::Map;\nfn main() { let m: Map<String, i64> = Map::new(); let f = m.freeze(7); }\n",
        &["error[E0201]"],
    );
}

#[test]
fn test_frozen_map_is_sync_passable_to_async() {
    let (ok, stderr) = compile_with_data_race_check(
        r#"
import std::collections::Map;
async fn t(fm: FrozenMap<String, i64>) -> i64 { await sleep(1); return fm.len(); }
async fn main() {
    let m: Map<String, i64> = Map::new();
    m.insert("a", 1);
    println(await t(m.freeze()));
}
"#,
    );
    assert!(ok, "FrozenMap<String,i64> should be Sync: {stderr}");
}

// ── Task<T> Send: interface-value capture diagnostics (willow-dgwo.5) ────────
// An interface value passed to an async fn follows the interface contract, so a
// plain interface (neither Send nor Sync) → E2404; an interface that is Send but
// not Sync → E2405; an interface that `extends Sync` is accepted. Gated by the
// the always-on multi-worker data-race check.
#[test]
fn test_dgwo5_plain_interface_arg_e2404() {
    let (ok, stderr) = compile_with_data_race_check(
        r#"
interface Animal { fn speak(self) -> String; }
class Dog implements Animal { pub fn speak(self) -> String { return "woof"; } }
async fn run(a: Animal) -> i64 { await sleep(1); return 0; }
async fn main() { println(await run(new Dog())); }
"#,
    );
    assert!(!ok);
    assert!(stderr.contains("error[E2404]"), "{stderr}");
    assert!(stderr.contains("is not `Send`"), "{stderr}");
}

#[test]
fn test_dgwo5_send_only_interface_arg_e2405() {
    // `extends Send` makes it Send but not Sync; a captured GC ref needs Sync.
    let (ok, stderr) = compile_with_data_race_check(
        r#"
interface Job extends Send { fn go(self) -> i64; }
class J implements Job { pub fn go(self) -> i64 { return 1; } }
async fn run(j: Job) -> i64 { await sleep(1); return j.go(); }
async fn main() { println(await run(new J())); }
"#,
    );
    assert!(!ok);
    assert!(stderr.contains("error[E2405]"), "{stderr}");
    assert!(stderr.contains("is not `Sync`"), "{stderr}");
}

#[test]
fn test_dgwo5_sync_interface_arg_accepted() {
    let (ok, stderr) = compile_with_data_race_check(
        r#"
interface Job extends Sync { fn go(self) -> i64; }
class J implements Job { pub fn go(self) -> i64 { return 9; } }
async fn run(j: Job) -> i64 { await sleep(1); return j.go(); }
async fn main() { println(await run(new J())); }
"#,
    );
    assert!(ok, "interface extends Sync should be accepted: {stderr}");
}

#[test]
fn test_dgwo5_interface_arg_rejected_by_default() {
    let (ok, stderr) = compile_with_compiler_env(
        r#"
interface Animal { fn speak(self) -> i64; }
class Dog implements Animal { pub fn speak(self) -> i64 { return 7; } }
async fn run(a: Animal) -> i64 { await sleep(1); return a.speak(); }
async fn main() { println(await run(new Dog())); }
"#,
        &[],
    );
    assert!(!ok);
    assert!(stderr.contains("error[E2404]"), "{stderr}");
}

// ── Happens-before guarantees + Channel item Send (willow-dgwo.6) ────────────
// Perspectives: 1 channel send->recv value visible; 2 channel order preserved;
// 3 Mutex counter no lost updates; 4 AtomicI64 counter no lost updates; 5 join
// makes a task's result visible; 6 Channel<Fn> send rejected under the check
// (E2403); 7 low worker overrides still reject it; 8 Channel<i64> is accepted.
#[test]
fn test_dgwo6_channel_send_recv_value_and_order() {
    let (out, ok) = compile_and_run(
        r#"
async fn producer(ch: Channel<i64>) -> i64 {
    let mut x = 0;
    x = 41;
    x = x + 1;        // write happens-before the send
    ch.send(x);
    ch.send(100);
    ch.close();
    return 0;
}
async fn main() {
    let ch = Channel<i64>::new();
    let p = producer(ch);
    println(ch.recv());   // 42 — the pre-send write is visible
    println(ch.recv());   // 100 — order preserved
    p.join();
}
"#,
    );
    assert!(ok);
    assert_eq!(out, "42\n100\n");
}

#[test]
fn test_dgwo6_mutex_value_visible_across_tasks() {
    let (out, ok) = compile_and_run(
        r#"
async fn store(m: Mutex<i64>, value: i64) -> i64 {
    await sleep(1);
    m.set(value);
    return value;
}
async fn main() {
    let m = Mutex::new(0);
    let writer = store(m, 10);
    writer.join();
    println(m.get());   // the joined task's write is visible
}
"#,
    );
    assert!(ok);
    assert_eq!(out, "10\n");
}

#[test]
fn test_dgwo6_atomic_counter_no_lost_updates() {
    let (out, ok) = compile_and_run(
        r#"
async fn inc(c: AtomicI64, n: i64) -> i64 {
    let mut i = 0;
    while i < n { c.add(1); await sleep(1); i = i + 1; }
    return n;
}
async fn main() {
    let c = AtomicI64::new(0);
    let a = inc(c, 5);
    let b = inc(c, 7);
    a.join();
    b.join();
    println(c.load());   // 12
}
"#,
    );
    assert!(ok);
    assert_eq!(out, "12\n");
}

#[test]
fn test_dgwo6_join_makes_task_result_visible() {
    let (out, ok) = compile_and_run(
        r#"
async fn compute() -> i64 { await sleep(1); return 7 * 6; }
async fn main() {
    let t = compute();
    println(t.join());   // 42 — the task's writes are visible after join
}
"#,
    );
    assert!(ok);
    assert_eq!(out, "42\n");
}

#[test]
fn test_dgwo6_channel_item_must_be_send_e2403() {
    let (ok, stderr) = compile_with_data_race_check(
        r#"
fn dbl(x: i64) -> i64 { return x * 2; }
fn main() {
    let ch = Channel<fn(i64) -> i64>::new();
    let f = dbl;
    ch.send(f);
}
"#,
    );
    assert!(!ok);
    assert!(stderr.contains("error[E2403]"), "{stderr}");
    assert!(stderr.contains("must be `Send`"), "{stderr}");
}

#[test]
fn test_dgwo6_channel_fn_send_rejected_by_default() {
    let (ok, stderr) = compile_with_compiler_env(
        r#"
fn dbl(x: i64) -> i64 { return x * 2; }
fn main() {
    let ch = Channel<fn(i64) -> i64>::new();
    ch.send(dbl);
    let g = ch.recv();
    println(g(21));   // 42
}
"#,
        &[],
    );
    assert!(!ok);
    assert!(stderr.contains("error[E2403]"), "{stderr}");
}

#[test]
fn test_dgwo6_channel_send_send_value_ok_under_check() {
    let (ok, stderr) = compile_with_data_race_check(
        r#"
fn main() {
    let ch = Channel<i64>::new();
    ch.send(5);
    println(ch.recv());
}
"#,
    );
    assert!(ok, "Channel<i64> send should be accepted: {stderr}");
}

#[test]
fn test_dgwo4_scalar_only_and_nested_forwarding_accepted() {
    // 19 + 20: scalar-only async fn, and a nested async call forwarding a Sync
    // argument, are both accepted.
    let (ok, stderr) = compile_with_data_race_check(
        r#"
async fn inner(m: Mutex<i64>) -> i64 { await sleep(1); return m.get(); }
async fn outer(m: Mutex<i64>, n: i64) -> i64 { return await inner(m) + n; }
async fn main() {
    let m = Mutex::new(5);
    println(await outer(m, 1));
}
"#,
    );
    assert!(
        ok,
        "scalar + nested Sync forwarding should be accepted: {stderr}"
    );
}

// ── Multi-worker capstone (willow-dgwo.9) ─────────────────────────────────────
// The five-worker minimum enables the Send/Sync checks that make task migration
// sound. WILLOW_WORKERS values below five are clamped; invalid values use five.
const NONSEND_ASYNC_FRAME_SRC: &str = r#"
fn inc(x: i64) -> i64 { return x + 1; }
async fn run() -> i64 {
    let op: fn(i64) -> i64 = inc;
    await sleep(1);
    return op(41);
}
async fn main() { println(await run()); }
"#;

#[test]
fn test_dgwo9_workers_env_enables_data_race_check() {
    let (ok, stderr) = compile_with_compiler_env(NONSYNC_ARG_SRC, &[("WILLOW_WORKERS", "4")]);
    assert!(!ok, "WILLOW_WORKERS>1 should enable Send/Sync checks");
    assert!(stderr.contains("error[E2402]"), "{stderr}");
    assert!(stderr.contains("not `Sync`"), "{stderr}");
}

#[test]
fn test_dgwo9_default_five_workers_enable_data_race_check() {
    let (ok, stderr) = compile_with_compiler_env(NONSYNC_ARG_SRC, &[]);
    assert!(
        !ok,
        "the five-worker default should enable Send/Sync checks"
    );
    assert!(stderr.contains("error[E2402]"), "{stderr}");
}

#[test]
fn test_dgwo9_workers_one_is_clamped_and_keeps_checks_on() {
    let (ok, stderr) = compile_with_compiler_env(NONSYNC_ARG_SRC, &[("WILLOW_WORKERS", "1")]);
    assert!(!ok, "WILLOW_WORKERS=1 must be clamped to five");
    assert!(stderr.contains("error[E2402]"), "{stderr}");
}

#[test]
fn test_dgwo9_invalid_workers_fall_back_to_default_checks() {
    let (ok, stderr) =
        compile_with_compiler_env(NONSYNC_ARG_SRC, &[("WILLOW_WORKERS", "not-a-number")]);
    assert!(
        !ok,
        "invalid WILLOW_WORKERS should fall back to five-worker checks: {stderr}"
    );
    assert!(stderr.contains("error[E2402]"), "{stderr}");
}

#[test]
fn test_dgwo9_async_task_frame_must_be_send_under_workers() {
    let (ok, stderr) =
        compile_with_compiler_env(NONSEND_ASYNC_FRAME_SRC, &[("WILLOW_WORKERS", "4")]);
    assert!(!ok, "non-Send async frame should be rejected");
    assert!(stderr.contains("error[E2402]"), "{stderr}");
    assert!(
        stderr.contains("async task frame is not `Send`"),
        "{stderr}"
    );
    assert!(stderr.contains("fn(i64) -> i64"), "{stderr}");
}

#[test]
fn test_dgwo9_async_task_frame_must_be_send_under_explicit_check() {
    let (ok, stderr) = compile_with_data_race_check(NONSEND_ASYNC_FRAME_SRC);
    assert!(
        !ok,
        "explicit data-race check should reject non-Send frames"
    );
    assert!(
        stderr.contains("async task frame is not `Send`"),
        "{stderr}"
    );
}

#[test]
fn test_dgwo9_async_task_frame_rejected_by_default() {
    let (ok, stderr) = compile_with_compiler_env(NONSEND_ASYNC_FRAME_SRC, &[]);
    assert!(!ok);
    assert!(
        stderr.contains("async task frame is not `Send`"),
        "{stderr}"
    );
}

// ── Atomic primitives AtomicI64 / AtomicBool (willow-dgwo.3) ──────────────────
//
// 20 test perspectives:
//  1. AtomicI64::new + load reads the initial value.
//  2. store then load.
//  3. add returns the PREVIOUS value and updates.
//  4. sub returns the PREVIOUS value and updates.
//  5. swap returns the PREVIOUS value and updates.
//  6. AtomicBool::new(false) + load.
//  7. AtomicBool store + load.
//  8. AtomicBool swap returns previous.
//  9. load() result is an i64 usable in arithmetic.
// 10. AtomicBool load() is a bool usable as a condition.
// 11. An atomic shared across async tasks accumulates exactly.
// 12. Atomics survive GC (they are GC-allocated cells).
// 13. Multiple atomics are independent.
// 14. Atomic passed as a function parameter works.
// 15. AtomicI64::new with wrong arg count is rejected.
// 16. AtomicI64::new with a bool arg is rejected.
// 17. AtomicBool::new with an i64 arg is rejected.
// 18. An unknown atomic method is rejected (E0806).
// 19. AtomicBool has no add/sub (E0806).
// 20. Atomics are in scope with no import (compiler-known).
#[test]
fn test_atomic_i64_basic_ops() {
    let (out, ok) = compile_and_run(
        r#"
fn main() {
    let c = AtomicI64::new(0);
    c.store(10);
    println(c.add(5));    // 10 (previous)
    println(c.load());    // 15
    println(c.sub(3));    // 15 (previous)
    println(c.load());    // 12
    println(c.swap(99));  // 12 (previous)
    println(c.load());    // 99
}
"#,
    );
    assert!(ok);
    assert_eq!(out, "10\n15\n15\n12\n12\n99\n");
}

#[test]
fn test_atomic_bool_basic_ops() {
    let (out, ok) = compile_and_run(
        r#"
fn main() {
    let f = AtomicBool::new(false);
    println(f.load());      // false
    f.store(true);
    println(f.load());      // true
    println(f.swap(false)); // true
    println(f.load());      // false
}
"#,
    );
    assert!(ok);
    assert_eq!(out, "false\ntrue\ntrue\nfalse\n");
}

#[test]
fn test_atomic_load_is_i64_in_arithmetic() {
    let (out, ok) = compile_and_run(
        r#"
fn main() {
    let c = AtomicI64::new(20);
    println(c.load() + 22);   // 42
}
"#,
    );
    assert!(ok);
    assert_eq!(out, "42\n");
}

#[test]
fn test_atomic_bool_load_is_bool_condition() {
    let (out, ok) = compile_and_run(
        r#"
fn main() {
    let f = AtomicBool::new(true);
    if f.load() { println(1); } else { println(0); }
}
"#,
    );
    assert!(ok);
    assert_eq!(out, "1\n");
}

#[test]
fn test_atomic_shared_across_async_tasks() {
    let (out, ok) = compile_and_run(
        r#"
async fn bump(c: AtomicI64, n: i64) -> i64 {
    let mut i = 0;
    while i < n { c.add(1); await sleep(1); i = i + 1; }
    return n;
}
async fn main() {
    let c = AtomicI64::new(0);
    let a = bump(c, 2);
    let b = bump(c, 5);
    a.join();
    b.join();
    println(c.load());   // 7
}
"#,
    );
    assert!(ok);
    assert_eq!(out, "7\n");
}

#[test]
fn test_atomic_survives_gc_stress() {
    let (out, ok) = compile_and_run_gc_stress(
        r#"
fn main() {
    let c = AtomicI64::new(1);
    let mut i = 0;
    while i < 40 {
        let junk = AtomicI64::new(i);
        c.add(1);
        i = i + 1;
    }
    println(c.load());   // 41
}
"#,
    );
    assert!(ok);
    assert_eq!(out, "41\n");
}

#[test]
fn test_atomics_independent_and_param_passing() {
    let (out, ok) = compile_and_run(
        r#"
fn add_to(a: AtomicI64, n: i64) {
    a.add(n);
}
fn main() {
    let x = AtomicI64::new(0);
    let y = AtomicI64::new(0);
    add_to(x, 3);
    add_to(y, 100);
    println(x.load());   // 3
    println(y.load());   // 100
}
"#,
    );
    assert!(ok);
    assert_eq!(out, "3\n100\n");
}

#[test]
fn test_atomic_i64_new_wrong_arg_count_rejected() {
    assert_compile_error_contains(
        "fn main() { let c = AtomicI64::new(); }\n",
        &["error[E0201]", "expects 1 argument"],
    );
}

#[test]
fn test_atomic_i64_new_bool_arg_rejected() {
    assert_compile_error_contains(
        "fn main() { let c = AtomicI64::new(true); }\n",
        &["error[E0201]", "expects `i64`"],
    );
}

#[test]
fn test_atomic_bool_new_i64_arg_rejected() {
    assert_compile_error_contains(
        "fn main() { let c = AtomicBool::new(1); }\n",
        &["error[E0201]", "expects `bool`"],
    );
}

#[test]
fn test_atomic_unknown_method_rejected() {
    assert_compile_error_contains(
        "fn main() { let c = AtomicI64::new(0); c.frobnicate(); }\n",
        &["error[E0806]", "no method `frobnicate`"],
    );
}

#[test]
fn test_atomic_bool_has_no_add() {
    assert_compile_error_contains(
        "fn main() { let f = AtomicBool::new(false); f.add(1); }\n",
        &["error[E0806]", "no method `add`"],
    );
}

// ── Mutex<T> / RwLock<T> (willow-dgwo.3) ─────────────────────────────────────
//
// 20 test perspectives:
//  1. Mutex<i64> get reads the initial value.
//  2. Mutex set then get.
//  3. RwLock<bool> read reads initial.
//  4. RwLock write then read.
//  5. Element type inferred from the constructor argument (i64).
//  6. Element type inferred as bool.
//  7. Element type inferred as f64 (word coercion round-trips).
//  8. Explicit type argument `Mutex<i64>::new(0)`.
//  9. Mutex<String> (GC element) round-trips a value.
// 10. A GC element survives collection (traced via the lock registry).
// 11. Mutex shared across async tasks accumulates correctly.
// 12. Mutex passed as a function parameter.
// 13. get() result usable in arithmetic.
// 14. RwLock<i64> read/write with numbers.
// 15. Mutex::new wrong arg count rejected.
// 16. Explicit type arg mismatch rejected.
// 17. Unknown Mutex method rejected (E0806).
// 18. RwLock has no get/set (only read/write) — unknown method rejected.
// 19. Compiler-known with no import.
// 20. Multiple independent locks.
#[test]
fn test_mutex_get_set() {
    let (out, ok) = compile_and_run(
        r#"
fn main() {
    let m = Mutex::new(10);
    println(m.get());   // 10
    m.set(25);
    println(m.get());   // 25
}
"#,
    );
    assert!(ok);
    assert_eq!(out, "10\n25\n");
}

#[test]
fn test_rwlock_read_write_bool() {
    let (out, ok) = compile_and_run(
        r#"
fn main() {
    let r = RwLock::new(true);
    println(r.read());
    r.write(false);
    println(r.read());
}
"#,
    );
    assert!(ok);
    assert_eq!(out, "true\nfalse\n");
}

#[test]
fn test_mutex_f64_word_coercion() {
    let (out, ok) = compile_and_run(
        r#"
fn main() {
    let m = Mutex::new(2.5);
    m.set(3.5);
    println(m.get());
}
"#,
    );
    assert!(ok);
    assert_eq!(out, "3.5\n");
}

#[test]
fn test_mutex_explicit_type_arg() {
    let (out, ok) = compile_and_run(
        r#"
fn main() {
    let m = Mutex<i64>::new(7);
    println(m.get());
}
"#,
    );
    assert!(ok);
    assert_eq!(out, "7\n");
}

#[test]
fn test_mutex_string_survives_gc() {
    let (out, ok) = compile_and_run_gc_stress(
        r#"
fn main() {
    let m = Mutex::new("hello");
    let mut i = 0;
    while i < 30 { let junk = Mutex::new(i); i = i + 1; }
    gc_collect();
    println(m.get());
    m.set("world");
    println(m.get());
}
"#,
    );
    assert!(ok);
    assert_eq!(out, "hello\nworld\n");
}

#[test]
fn test_mutex_passed_across_async_tasks() {
    let (out, ok) = compile_and_run(
        r#"
async fn bump(m: Mutex<i64>, n: i64) -> i64 {
    let mut i = 0;
    while i < n { m.set(m.get() + 1); await sleep(1); i = i + 1; }
    return n;
}
async fn main() {
    let m = Mutex::new(0);
    let a = bump(m, 3);
    a.join();
    let b = bump(m, 4);
    b.join();
    println(m.get());   // 7
}
"#,
    );
    assert!(ok);
    assert_eq!(out, "7\n");
}

#[test]
fn test_mutex_param_and_independent_cells() {
    let (out, ok) = compile_and_run(
        r#"
fn add_to(m: Mutex<i64>, n: i64) { m.set(m.get() + n); }
fn main() {
    let x = Mutex::new(0);
    let y = Mutex::new(0);
    add_to(x, 3);
    add_to(y, 100);
    println(x.get() + 1);   // 4
    println(y.get());       // 100
}
"#,
    );
    assert!(ok);
    assert_eq!(out, "4\n100\n");
}

#[test]
fn test_mutex_new_wrong_arg_count_rejected() {
    assert_compile_error_contains(
        "fn main() { let m = Mutex::new(); }\n",
        &["error[E0201]", "expects 1 argument"],
    );
}

#[test]
fn test_mutex_explicit_type_arg_mismatch_rejected() {
    assert_compile_error_contains(
        "fn main() { let m = Mutex<i64>::new(true); }\n",
        &["error[E0201]"],
    );
}

#[test]
fn test_mutex_unknown_method_rejected() {
    assert_compile_error_contains(
        "fn main() { let m = Mutex::new(0); m.lock(); }\n",
        &["error[E0806]", "no method `lock`"],
    );
}

#[test]
fn test_rwlock_has_no_get() {
    assert_compile_error_contains(
        "fn main() { let r = RwLock::new(0); r.get(); }\n",
        &["error[E0806]", "no method `get`"],
    );
}

// Case A (willow-h2vf.5): an async fn already returns Task<ReturnType>, so its
// declared return type must be the awaited value, not a task handle (E0809).
#[test]
fn test_async_return_task_handle_rejected_task() {
    assert_compile_error_contains(
        "async fn f() -> Task<i64> { return 1; }\nfn main() {}\n",
        &[
            "error[E0809]",
            "async fn return type must be the awaited value",
        ],
    );
}

#[test]
fn test_async_return_task_handle_rejected_future() {
    assert_compile_error_contains(
        "async fn f() -> Future<i64> { return 1; }\nfn main() {}\n",
        &["error[E0809]"],
    );
}

#[test]
fn test_async_return_task_handle_rejected_join_handle() {
    assert_compile_error_contains(
        "async fn f() -> JoinHandle<i64> { return 1; }\nfn main() {}\n",
        &["error[E0809]"],
    );
}

#[test]
fn test_async_return_plain_value_allowed() {
    // The awaited-value annotation (`-> i64`) is fine and yields a joinable task.
    let (out, ok) = compile_and_run(
        r#"
async fn f() -> i64 { await sleep(1); return 7; }
async fn main() { println(f().join()); }
"#,
    );
    assert!(ok);
    assert_eq!(out, "7\n");
}

#[test]
fn test_async_call_is_joinable_without_spawn() {
    let (out, ok) = compile_and_run(
        r#"
async fn work(x: i64) -> i64 { await sleep(1); return x * 2; }
async fn main() {
    let t = work(21);
    println(t.join());
}
"#,
    );
    assert!(ok);
    assert_eq!(out, "42\n");
}

#[test]
fn test_async_call_concurrent_joins_without_spawn() {
    let (out, ok) = compile_and_run(
        r#"
async fn work(id: i64, ticks: i64) -> i64 {
    let mut i = 0;
    while i < ticks { await sleep(1); i = i + 1; }
    return id * 100 + i;
}
async fn main() {
    let a = work(1, 2);
    let b = work(2, 3);
    println(a.join());
    println(b.join());
}
"#,
    );
    assert!(ok);
    assert_eq!(out, "102\n203\n");
}

#[test]
fn test_async_call_join_inline_without_spawn() {
    let (out, ok) = compile_and_run(
        r#"
async fn square(x: i64) -> i64 { await sleep(1); return x * x; }
async fn main() {
    println(square(5).join());
}
"#,
    );
    assert!(ok);
    assert_eq!(out, "25\n");
}

// join()/await resume when the TARGET task completes, not when the whole
// scheduler drains (willow-bsqy).
#[test]
fn test_join_returns_when_target_completes_not_draining_all() {
    // a completes immediately; b is unrelated and never joined. With five
    // workers b may start concurrently, but join must not wait for its timer.
    let (out, ok) = compile_and_run(
        r#"
async fn a_task() -> i64 { return 1; }
async fn b_task() -> i64 { println(91); await sleep(200); println(92); return 2; }
async fn main() {
    let a = a_task();
    let b = b_task();
    println(a.join());
    println(99);
}
"#,
    );
    assert!(ok);
    let lines: Vec<_> = out.lines().collect();
    assert!(lines.contains(&"1"), "{out}");
    assert!(lines.contains(&"99"), "{out}");
    assert!(
        !lines.contains(&"92"),
        "join drained an unrelated task: {out}"
    );
}

#[test]
fn test_join_unrelated_task_is_still_joinable_afterwards() {
    // Explicitly joining b finishes it (its side effects happen at b.join()).
    let (out, ok) = compile_and_run(
        r#"
async fn a_task() -> i64 { return 1; }
async fn b_task() -> i64 { println(91); await sleep(1); println(92); return 2; }
async fn main() {
    let a = a_task();
    let b = b_task();
    println(a.join());
    println(b.join());
    println(99);
}
"#,
    );
    assert!(ok);
    let lines: Vec<_> = out.lines().collect();
    for expected in ["1", "91", "92", "2", "99"] {
        assert_eq!(
            lines.iter().filter(|line| **line == expected).count(),
            1,
            "{out}"
        );
    }
    let position = |value| lines.iter().position(|line| *line == value).unwrap();
    assert!(position("91") < position("92"), "{out}");
    assert!(position("92") < position("2"), "{out}");
    assert!(position("2") < position("99"), "{out}");
}

#[test]
fn test_join_drives_target_dependencies() {
    // a awaits c, so joining a must still drive c to completion.
    let (out, ok) = compile_and_run(
        r#"
async fn c_task() -> i64 { await sleep(1); return 5; }
async fn a_task() -> i64 { let c = c_task(); return await c + 1; }
async fn main() { let a = a_task(); println(a.join()); }
"#,
    );
    assert!(ok);
    assert_eq!(out, "6\n");
}

#[test]
fn test_join_does_not_hang_on_unrelated_long_task() {
    // b would run far longer than a; a.join() must return promptly and the
    // program must exit (main joined only a) rather than draining b.
    let (out, ok) = compile_and_run(
        r#"
async fn quick() -> i64 { await sleep(1); return 42; }
async fn slow() -> i64 {
    let mut i = 0;
    while i < 100000 { await sleep(1); i = i + 1; }
    return i;
}
async fn main() {
    let a = quick();
    let b = slow();
    println(a.join());
    println(777);
}
"#,
    );
    assert!(ok);
    assert_eq!(out, "42\n777\n");
}

// Many concurrent tasks: start 30 async workers, collect handles in an array,
// join them all. Verifies the scheduler + array-of-Task + run-until join scale
// and that each task keeps its own identity/result (willow-9lw/h2vf/bsqy).
const THIRTY_WORKERS_SRC: &str = r#"
import std::collections::Array;
async fn worker(id: i64) -> i64 {
    let mut i = 0;
    let ticks = id % 5 + 1;   // vary awaits so the 30 tasks interleave
    while i < ticks { await sleep(1); i = i + 1; }
    return id * 10;
}
async fn main() {
    let tasks: Array<Task<i64>> = [];
    let mut id = 1;
    while id <= 30 { tasks.push(worker(id)); id = id + 1; }
    let mut k = 0;
    let mut mismatches = 0;
    let mut total = 0;
    while k < tasks.len() {
        let r = tasks[k].join();
        if r != (k + 1) * 10 { mismatches = mismatches + 1; }
        total = total + r;
        k = k + 1;
    }
    println(mismatches);       // 0 — every task matched its expected result
    println(total);            // (1+..+30)*10 = 4650
    println(tasks.len());      // 30
}
"#;

#[test]
fn test_thirty_concurrent_tasks_each_returns_own_value() {
    let (out, ok) = compile_and_run(THIRTY_WORKERS_SRC);
    assert!(ok);
    assert_eq!(out, "0\n4650\n30\n");
}

#[test]
fn test_thirty_concurrent_tasks_under_gc_stress() {
    let (out, ok) = compile_and_run_gc_stress(THIRTY_WORKERS_SRC);
    assert!(ok);
    assert_eq!(out, "0\n4650\n30\n");
}

#[test]
fn test_thirty_concurrent_tasks_sum_465() {
    // Mirrors example/async_concurrent.wi (worker returns id, sum 1..30 = 465).
    let (out, ok) = compile_and_run(
        r#"
import std::collections::Array;
async fn worker(id: i64) -> i64 {
    let mut i = 0;
    let ticks = id % 5 + 1;
    while i < ticks { await sleep(1); i = i + 1; }
    return id;
}
async fn main() {
    let tasks: Array<Task<i64>> = [];
    let mut id = 1;
    while id <= 30 { tasks.push(worker(id)); id = id + 1; }
    let mut total = 0;
    let mut k = 0;
    while k < tasks.len() { total = total + tasks[k].join(); k = k + 1; }
    println(total);   // 465
}
"#,
    );
    assert!(ok);
    assert_eq!(out, "465\n");
}

#[test]
fn test_async_9lw_two_concurrent_timers() {
    // Two spawned async workers each loop awaiting sleep; the single-threaded
    // executor drives both concurrently to completion.
    let (stdout, ok) = compile_and_run(
        r#"
async fn worker(id: i64, ticks: i64) -> i64 {
    let mut i = 0;
    while i < ticks {
        await sleep(1);
        i = i + 1;
    }
    return id * 100 + i;
}
async fn main() {
    let a = worker(1, 2);
    let b = worker(2, 3);
    println(a.join());
    println(b.join());
}
"#,
    );
    assert!(ok);
    assert_eq!(stdout, "102\n203\n");
}

#[test]
fn test_async_9lw_locals_live_across_await() {
    let (stdout, ok) = compile_and_run(
        r#"
async fn main() {
    let mut sum = 0;
    let mut i = 1;
    while i <= 3 {
        await sleep(1);
        sum = sum + i;
        i = i + 1;
    }
    println(sum);
}
"#,
    );
    assert!(ok);
    assert_eq!(stdout, "6\n");
}

#[test]
fn test_async_9lw_nested_await_passes_values() {
    let (stdout, ok) = compile_and_run(
        r#"
async fn inner(x: i64) -> i64 {
    await sleep(1);
    return x + 1;
}
async fn outer(x: i64) -> i64 {
    let a = await inner(x);
    let b = await inner(a);
    return b;
}
async fn main() {
    println(await outer(10));
}
"#,
    );
    assert!(ok);
    assert_eq!(stdout, "12\n");
}

#[test]
fn test_async_9lw_panic_renders_async_chain() {
    // A panic inside a suspended async fn renders the async future chain
    // (current task first), not just the immediate location — the cooperative
    // scheduler flattens the OS call stack, so this comes from runtime state.
    let (out, ok) = compile_and_run_check_exit(
        r#"
async fn inner(x: i64) -> i64 {
    await sleep(1);
    panic("boom in inner");
    return x;
}
async fn main() {
    let r = await inner(5);
    println(r);
}
"#,
    );
    assert!(!ok, "panic must make the program exit non-zero");
    assert!(out.contains("boom in inner"), "panic message: {out}");
    assert!(
        out.contains("async stack"),
        "expected an async stack trace: {out}"
    );
    assert!(out.contains("inner"), "chain should name `inner`: {out}");
    assert!(out.contains("main"), "chain should name `main`: {out}");
}

#[test]
fn test_async_sleep_mvp_compiles_and_runs() {
    let (stdout, ok) = compile_and_run(
        r#"
async fn wait_value() -> i64 {
    await sleep(0);
    return 42;
}

async fn main() {
    let value = await wait_value();
    println(value);
}
"#,
    );
    assert!(ok);
    assert_eq!(stdout, "42\n");
}

#[test]
fn test_async_task_values_are_awaitable() {
    let (stdout, ok) = compile_and_run(
        r#"
async fn number() -> i64 {
    return 7;
}

async fn flag() -> bool {
    return true;
}

async fn ratio() -> f64 {
    return 2.5;
}

async fn word() -> String {
    return "ok";
}

async fn main() {
    let number_task = number();
    let value = await number_task;
    println(value);
    println(await flag());
    println(await ratio());
    println(await word());
}
"#,
    );
    assert!(ok);
    assert_eq!(stdout, "7\ntrue\n2.5\nok\n");
}

#[test]
fn test_async_mut_reference_parameter_reports_e1707() {
    assert_compile_error_contains(
        r#"
async fn update(x: &mut i64) {
    x = x + 1;
}

fn main() {
    println(1);
}
"#,
        &[
            "error[E1707]",
            "reference parameter `x` is not supported in async function",
            "`&mut` parameter may live across suspension points",
        ],
    );
}

#[test]
fn test_async_immutable_reference_parameter_reports_e1707() {
    assert_compile_error_contains(
        r#"
async fn read(x: & i64) -> i64 {
    return x;
}

fn main() {
    println(1);
}
"#,
        &[
            "error[E1707]",
            "reference parameter `x` is not supported in async function",
            "`&` parameter may live across suspension points",
        ],
    );
}

#[test]
fn test_spawn_join_mvp_compiles_and_runs() {
    let (stdout, ok) = compile_and_run(
        r#"
async fn work(x: i64) -> i64 {
    return x * 2;
}

fn main() {
    let h = work(21);
    println(h.join());
}
"#,
    );
    assert!(ok);
    assert_eq!(stdout, "42\n");
}

#[test]
fn test_spawn_multiple_parallel_tasks_compile_and_run() {
    let (stdout, ok) = compile_and_run(
        r#"
async fn square(x: i64) -> i64 {
    return x * x;
}

fn main() {
    let a = square(3);
    let b = square(4);
    let c = square(5);
    println(a.join());
    println(b.join());
    println(c.join());
}
"#,
    );
    assert!(ok);
    assert_eq!(stdout, "9\n16\n25\n");
}

#[test]
fn test_await_outside_async_reports_e0801() {
    assert_compile_error_contains(
        r#"
fn value() -> i64 {
    return 1;
}

fn main() {
    await value();
}
"#,
        &[
            "error[E0801]",
            "`await` can only be used inside an async function",
            "`await` used in a non-async function",
            "help: make the enclosing function `async`",
        ],
    );
}

#[test]
fn test_select_block_is_supported() {
    // `select` is implemented (willow-7aj): a ready recv case runs its body.
    let (out, ok) = compile_and_run(
        r#"
fn main() {
    let ch = Channel<i64>::new();
    ch.send(5);
    select {
        let v = ch.recv() => { println(v); }
        default => { println(0); }
    }
}
"#,
    );
    assert!(ok, "{out}");
    assert_eq!(out, "5\n");
}

#[test]
fn test_await_non_future_reports_e0803() {
    assert_compile_error_contains(
        r#"
async fn main() {
    let value = await 1;
}
"#,
        &[
            "error[E0803]",
            "cannot await value of type `i64`",
            "expected an awaitable",
        ],
    );
}

#[test]
fn test_looping_sync_helper_in_task_context_reports_e0810() {
    assert_compile_error_contains(
        r#"
fn heavy(n: i64) -> i64 {
    let mut i = 0;
    while i < n {
        i = i + 1;
    }
    return i;
}

async fn run() -> i64 {
    return heavy(10);
}

fn main() {
    run().join();
}
"#,
        &[
            "error[E0810]",
            "sync helper `heavy` with a loop is not preemptible in task context",
            "this call can monopolize the scheduler worker",
            "help: make the helper async so its loop can use resumable safepoints",
        ],
    );
}

// ───────────────────────────────────────────────────────────────────────────
// E0810 for a looping method reached through a typed NON-`self` receiver
// (`obj.heavy()`), resolved by the type checker since the AST-only
// ConcurrencyAnalyzer cannot type the receiver (willow-0a6k.2).
// ───────────────────────────────────────────────────────────────────────────

#[test]
fn test_typed_receiver_looping_method_reports_e0810() {
    assert_compile_error_contains(
        r#"
class Work {
    pub fn heavy(self, n: i64) -> i64 {
        let mut i = 0;
        while i < n {
            i = i + 1;
        }
        return i;
    }
}

async fn run(w: Work) -> i64 {
    return w.heavy(10);
}
"#,
        &[
            "error[E0810]",
            "sync helper `Work::heavy` with a loop is not preemptible in task context",
            "this call can monopolize the scheduler worker",
        ],
    );
}

#[test]
fn test_typed_receiver_transitive_looping_method_reports_e0810() {
    assert_compile_error_contains(
        r#"
class Work {
    pub fn heavy(self, n: i64) -> i64 {
        let mut i = 0;
        while i < n {
            i = i + 1;
        }
        return i;
    }
    pub fn wrapper(self, n: i64) -> i64 {
        return self.heavy(n);
    }
}

async fn run(w: Work) -> i64 {
    return w.wrapper(10);
}
"#,
        &[
            "error[E0810]",
            "sync helper `Work::wrapper` with a loop is not preemptible in task context",
        ],
    );
}

#[test]
fn test_typed_receiver_looping_method_via_local_reports_e0810() {
    assert_compile_error_contains(
        r#"
class Work {
    pub init(self) {}
    pub fn heavy(self, n: i64) -> i64 {
        let mut i = 0;
        while i < n {
            i = i + 1;
        }
        return i;
    }
}

async fn run() -> i64 {
    let w = new Work();
    return w.heavy(10);
}
"#,
        &[
            "error[E0810]",
            "sync helper `Work::heavy` with a loop is not preemptible in task context",
        ],
    );
}

#[test]
fn test_typed_receiver_loop_free_method_is_allowed() {
    let (out, ok) = compile_and_run(
        r#"
class Work {
    pub fn light(self, n: i64) -> i64 {
        return n + 1;
    }
}

async fn run(w: Work) -> i64 {
    return w.light(41);
}

fn main() {
    println(run(new Work()).join());
}
"#,
    );
    assert!(ok, "loop-free typed-receiver method should compile and run");
    assert_eq!(out, "42\n");
}

#[test]
fn test_typed_receiver_looping_method_in_sync_context_is_allowed() {
    // Preemption only matters in a task context; the same call from a plain fn
    // must not warn.
    let (out, ok) = compile_and_run(
        r#"
class Work {
    pub fn heavy(self, n: i64) -> i64 {
        let mut i = 0;
        while i < n {
            i = i + 1;
        }
        return i;
    }
}

fn run(w: Work) -> i64 {
    return w.heavy(3);
}

fn main() {
    println(run(new Work()));
}
"#,
    );
    assert!(
        ok,
        "looping typed-receiver call in sync context should be allowed"
    );
    assert_eq!(out, "3\n");
}

#[test]
fn test_self_looping_method_reports_single_e0810() {
    // `self.heavy()` is handled by the AST-level ConcurrencyAnalyzer; the
    // type-checker typed-receiver path must skip `self` so it is not reported
    // twice.
    let stderr = compile_error_stderr(
        r#"
class Work {
    pub fn heavy(self, n: i64) -> i64 {
        let mut i = 0;
        while i < n {
            i = i + 1;
        }
        return i;
    }
    pub async fn run(self) -> i64 {
        return self.heavy(10);
    }
}
"#,
    );
    let count = stderr.matches("error[E0810]").count();
    assert_eq!(
        count, 1,
        "expected exactly one E0810, got {count}:\n{stderr}"
    );
}

#[test]
fn test_typed_receiver_inherited_looping_method_reports_e0810() {
    // The looping method is INHERITED from `Base`; calling it through a
    // `Derived` receiver must still be flagged, attributed to `Base::heavy`.
    assert_compile_error_contains(
        r#"
open class Base {
    pub fn heavy(self, n: i64) -> i64 {
        let mut i = 0;
        while i < n {
            i = i + 1;
        }
        return i;
    }
}

class Derived extends Base {
}

async fn run(d: Derived) -> i64 {
    return d.heavy(10);
}
"#,
        &[
            "error[E0810]",
            "sync helper `Base::heavy` with a loop is not preemptible in task context",
        ],
    );
}

#[test]
fn test_typed_receiver_loop_free_override_is_allowed() {
    // `Derived` overrides the base's looping method with a loop-free body; the
    // call resolves to the override, so it must NOT inherit the base's E0810.
    let (out, ok) = compile_and_run(
        r#"
open class Base {
    pub open fn heavy(self, n: i64) -> i64 {
        let mut i = 0;
        while i < n {
            i = i + 1;
        }
        return i;
    }
}

class Derived extends Base {
    pub override fn heavy(self, n: i64) -> i64 {
        return n + 1;
    }
}

async fn run(d: Derived) -> i64 {
    return d.heavy(41);
}

fn main() {
    println(run(new Derived()).join());
}
"#,
    );
    assert!(ok, "loop-free override should not inherit the base's E0810");
    assert_eq!(out, "42\n");
}

// Cross-module typed receiver: a looping method of an IMPORTED class, called
// through a typed receiver in a task context, is flagged with a module note
// (willow-0a6k.2). The receiver-class key differs by import style.

#[test]
fn test_cross_module_typed_receiver_item_import_reports_e0810() {
    let m = r#"
pub class Work {
    pub init(self) {}
    pub fn heavy(self, n: i64) -> i64 {
        let mut i = 0;
        while i < n {
            i = i + 1;
        }
        return i;
    }
}
"#;
    let main = r#"
import m::Work;

async fn run(w: Work) -> i64 {
    return w.heavy(10);
}

fn main() {
    println(1);
}
"#;
    let stderr = compile_temp_project_error_stderr(&[("m.wi", m), ("main.wi", main)], "main.wi");
    for expected in [
        "error[E0810]",
        "sync helper `Work::heavy` with a loop is not preemptible in task context",
        "imported module `m`",
    ] {
        assert!(
            stderr.contains(expected),
            "stderr did not contain `{expected}`:\n{stderr}"
        );
    }
}

#[test]
fn test_cross_module_typed_receiver_whole_module_import_reports_e0810() {
    let m = r#"
pub class Work {
    pub init(self) {}
    pub fn heavy(self, n: i64) -> i64 {
        let mut i = 0;
        while i < n {
            i = i + 1;
        }
        return i;
    }
}
"#;
    let main = r#"
import m;

async fn run(w: m::Work) -> i64 {
    return w.heavy(10);
}

fn main() {
    println(1);
}
"#;
    let stderr = compile_temp_project_error_stderr(&[("m.wi", m), ("main.wi", main)], "main.wi");
    assert!(
        stderr.contains("error[E0810]")
            && stderr.contains("sync helper `m::Work::heavy`")
            && stderr.contains("imported module `m`"),
        "expected whole-module cross-module typed-receiver E0810:\n{stderr}"
    );
}

#[test]
fn test_cross_module_typed_receiver_loop_free_is_allowed() {
    let m = r#"
pub class Work {
    pub init(self) {}
    pub fn light(self, n: i64) -> i64 {
        return n + 1;
    }
}
"#;
    let main = r#"
import m::Work;

async fn run(w: Work) -> i64 {
    return w.light(41);
}

fn main() {
    println(run(new Work()).join());
}
"#;
    let (out, ok) = compile_temp_project_and_run(&[("m.wi", m), ("main.wi", main)], "main.wi");
    assert!(ok, "loop-free cross-module typed-receiver call should run");
    assert_eq!(out, "42\n");
}

// ---------------------------------------------------------------------------
// Task-aware preemption analysis (E0810) across imported modules
// (willow-0a6k.2). The analyzer previously ran only on the entry program, so a
// looping synchronous helper called from an async fn *inside an imported
// module* slipped through. These cover the per-module analysis, the resolved
// module file in the diagnostic, transitive reachability inside a module, and
// the absence of false positives for loop-free module helpers.
// ---------------------------------------------------------------------------

#[test]
fn test_module_async_fn_calling_looping_helper_reports_e0810() {
    let worker = r#"
fn heavy(n: i64) -> i64 {
    let mut i = 0;
    while i < n {
        i = i + 1;
    }
    return i;
}

pub async fn run() -> i64 {
    return heavy(10);
}

pub fn ping() -> i64 {
    return 1;
}
"#;
    let main = r#"
import worker;

fn main() {
    println(worker::ping());
}
"#;
    let stderr =
        compile_temp_project_error_stderr(&[("worker.wi", worker), ("main.wi", main)], "main.wi");
    for expected in [
        "error[E0810]",
        "sync helper `heavy` with a loop is not preemptible in task context",
        // The diagnostic must resolve to the module file, not the entry file.
        "worker.wi",
    ] {
        assert!(
            stderr.contains(expected),
            "stderr did not contain `{expected}`:\n{stderr}"
        );
    }
}

#[test]
fn test_module_transitive_looping_helper_reports_e0810() {
    let worker = r#"
fn heavy(n: i64) -> i64 {
    let mut i = 0;
    while i < n {
        i = i + 1;
    }
    return i;
}

fn wrapper(n: i64) -> i64 {
    return heavy(n);
}

pub async fn run() -> i64 {
    return wrapper(10);
}

pub fn ping() -> i64 {
    return 1;
}
"#;
    let main = r#"
import worker;

fn main() {
    println(worker::ping());
}
"#;
    let stderr =
        compile_temp_project_error_stderr(&[("worker.wi", worker), ("main.wi", main)], "main.wi");
    assert!(
        stderr.contains("error[E0810]")
            && stderr.contains("sync helper `wrapper` with a loop is not preemptible"),
        "expected transitive module E0810 for `wrapper`:\n{stderr}"
    );
}

#[test]
fn test_module_loop_free_helper_in_async_compiles() {
    let worker = r#"
fn add_one(n: i64) -> i64 {
    return n + 1;
}

pub async fn run() -> i64 {
    return add_one(41);
}

pub fn ping() -> i64 {
    return 1;
}
"#;
    let main = r#"
import worker;

fn main() {
    println(worker::ping());
}
"#;
    let (out, ok) =
        compile_temp_project_and_run(&[("worker.wi", worker), ("main.wi", main)], "main.wi");
    assert!(ok, "loop-free module async helper should compile and run");
    assert_eq!(out, "1\n");
}

#[test]
fn test_entry_async_calling_module_looping_helper_reports_e0810() {
    let worker = r#"
pub fn heavy(n: i64) -> i64 {
    let mut i = 0;
    while i < n {
        i = i + 1;
    }
    return i;
}
"#;
    let main = r#"
import worker;

async fn run() -> i64 {
    return worker::heavy(10);
}

fn main() {
    run().join();
}
"#;
    let stderr =
        compile_temp_project_error_stderr(&[("worker.wi", worker), ("main.wi", main)], "main.wi");
    for expected in [
        "error[E0810]",
        "sync helper `worker::heavy` with a loop is not preemptible in task context",
        // Cross-module helper described via a note, not a secondary source label.
        "imported module `worker`",
        "help: make the helper async so its loop can use resumable safepoints",
    ] {
        assert!(
            stderr.contains(expected),
            "stderr did not contain `{expected}`:\n{stderr}"
        );
    }
}

#[test]
fn test_entry_async_calling_module_transitive_helper_reports_e0810() {
    let worker = r#"
pub fn heavy(n: i64) -> i64 {
    let mut i = 0;
    while i < n {
        i = i + 1;
    }
    return i;
}

pub fn wrapper(n: i64) -> i64 {
    return heavy(n);
}
"#;
    let main = r#"
import worker;

async fn run() -> i64 {
    return worker::wrapper(10);
}

fn main() {
    run().join();
}
"#;
    let stderr =
        compile_temp_project_error_stderr(&[("worker.wi", worker), ("main.wi", main)], "main.wi");
    assert!(
        stderr.contains("error[E0810]")
            && stderr.contains("sync helper `worker::wrapper`")
            && stderr.contains("imported module `worker`"),
        "expected cross-module transitive E0810 for `worker::wrapper`:\n{stderr}"
    );
}

#[test]
fn test_entry_async_calling_module_loop_free_helper_compiles() {
    let worker = r#"
pub fn add_one(n: i64) -> i64 {
    return n + 1;
}
"#;
    let main = r#"
import worker;

async fn run() -> i64 {
    return worker::add_one(41);
}

fn main() {
    println(run().join());
}
"#;
    let (out, ok) =
        compile_temp_project_and_run(&[("worker.wi", worker), ("main.wi", main)], "main.wi");
    assert!(
        ok,
        "loop-free cross-module async call should compile and run"
    );
    assert_eq!(out, "42\n");
}

#[test]
fn test_item_imported_looping_helper_from_async_reports_e0810() {
    let worker = r#"
pub fn heavy(n: i64) -> i64 {
    let mut i = 0;
    while i < n {
        i = i + 1;
    }
    return i;
}
"#;
    let main = r#"
import worker::heavy;

async fn run() -> i64 {
    return heavy(10);
}

fn main() {
    run().join();
}
"#;
    let stderr =
        compile_temp_project_error_stderr(&[("worker.wi", worker), ("main.wi", main)], "main.wi");
    for expected in [
        "error[E0810]",
        "sync helper `heavy` with a loop is not preemptible in task context",
        "imported module `worker`",
    ] {
        assert!(
            stderr.contains(expected),
            "stderr did not contain `{expected}`:\n{stderr}"
        );
    }
}

#[test]
fn test_item_imported_loop_free_helper_from_async_compiles() {
    let worker = r#"
pub fn add_one(n: i64) -> i64 {
    return n + 1;
}
"#;
    let main = r#"
import worker::add_one;

async fn run() -> i64 {
    return add_one(41);
}

fn main() {
    println(run().join());
}
"#;
    let (out, ok) =
        compile_temp_project_and_run(&[("worker.wi", worker), ("main.wi", main)], "main.wi");
    assert!(
        ok,
        "loop-free item-imported async call should compile and run"
    );
    assert_eq!(out, "42\n");
}

#[test]
fn test_module_to_module_looping_call_from_async_reports_e0810() {
    // main -> a (async) -> b::heavy (looping). The call lives in module `a`, so
    // module `a` must be seeded with module `b`'s helpers.
    let b = r#"
pub fn heavy(n: i64) -> i64 {
    let mut i = 0;
    while i < n {
        i = i + 1;
    }
    return i;
}
"#;
    let a = r#"
import b;

pub async fn run() -> i64 {
    return b::heavy(10);
}
"#;
    let main = r#"
import a;

fn main() {
    println(1);
}
"#;
    let stderr = compile_temp_project_error_stderr(
        &[("b.wi", b), ("a.wi", a), ("main.wi", main)],
        "main.wi",
    );
    for expected in [
        "error[E0810]",
        "sync helper `b::heavy` with a loop is not preemptible in task context",
        "imported module `b`",
        // The offending call is in module a, so the diagnostic resolves there.
        "a.wi",
    ] {
        assert!(
            stderr.contains(expected),
            "stderr did not contain `{expected}`:\n{stderr}"
        );
    }
}

#[test]
fn test_module_to_module_loop_free_call_from_async_compiles() {
    let b = r#"
pub fn add_one(n: i64) -> i64 {
    return n + 1;
}
"#;
    // Module `a`'s async fn calls `b`'s loop-free helper — no E0810. (`run` is
    // exercised internally; `main` only needs the modules to compile.)
    let a = r#"
import b;

pub async fn run() -> i64 {
    return b::add_one(41);
}

pub fn ping() -> i64 {
    return 7;
}
"#;
    let main = r#"
import a;

fn main() {
    println(a::ping());
}
"#;
    let (out, ok) =
        compile_temp_project_and_run(&[("b.wi", b), ("a.wi", a), ("main.wi", main)], "main.wi");
    assert!(
        ok,
        "loop-free module-to-module async call should compile and run"
    );
    assert_eq!(out, "7\n");
}

// ---------------------------------------------------------------------------
// Cross-module async fn call types as `Task<T>` at the call site (willow-887c).
// A module-qualified call to an async fn must yield a task so `.join()`/`await`
// type-check, exactly like a local async call.
// ---------------------------------------------------------------------------

#[test]
fn test_cross_module_async_fn_join_returns_value() {
    let worker = r#"
pub async fn make_value() -> i64 {
    return 42;
}
"#;
    let main = r#"
import worker;

fn main() {
    println(worker::make_value().join());
}
"#;
    let (out, ok) =
        compile_temp_project_and_run(&[("worker.wi", worker), ("main.wi", main)], "main.wi");
    assert!(ok, "cross-module async fn `.join()` should compile and run");
    assert_eq!(out, "42\n");
}

#[test]
fn test_cross_module_async_fn_await_returns_value() {
    let worker = r#"
pub async fn make_value() -> i64 {
    await sleep(1);
    return 42;
}
"#;
    let main = r#"
import worker;

async fn main() {
    println(await worker::make_value());
}
"#;
    let (out, ok) =
        compile_temp_project_and_run(&[("worker.wi", worker), ("main.wi", main)], "main.wi");
    assert!(
        ok,
        "cross-module `await` of an async fn should compile and run"
    );
    assert_eq!(out, "42\n");
}

#[test]
fn test_item_imported_async_fn_join_returns_value() {
    // Item-imported async fn called by its bare local name already wraps to
    // `Task<T>`; guard against regressing that alongside the module-qualified fix.
    let worker = r#"
pub async fn make_value() -> i64 {
    return 42;
}
"#;
    let main = r#"
import worker::make_value;

fn main() {
    println(make_value().join());
}
"#;
    let (out, ok) =
        compile_temp_project_and_run(&[("worker.wi", worker), ("main.wi", main)], "main.wi");
    assert!(
        ok,
        "item-imported async fn `.join()` should compile and run"
    );
    assert_eq!(out, "42\n");
}

// ---------------------------------------------------------------------------
// Awaiting a NON-leaf (item-imported) async fn from a cooperative poll fn must
// suspend cooperatively, not block-drive the scheduler (willow-0a6k.6). Output
// correctness here also guards the frame-slot reload on resume: without a
// reserved callee-frame slot the resume path would re-run the call.
// ---------------------------------------------------------------------------

#[test]
fn test_await_item_imported_async_from_cooperative_fn_runs_once() {
    let worker = r#"
pub async fn make_value() -> i64 {
    await sleep(1);
    println(99);
    return 42;
}
"#;
    let main = r#"
import worker::make_value;

async fn run() -> i64 {
    let x = await make_value();
    return x;
}

async fn main() {
    println(await run());
}
"#;
    let (out, ok) =
        compile_temp_project_and_run(&[("worker.wi", worker), ("main.wi", main)], "main.wi");
    assert!(
        ok,
        "awaiting an item-imported async fn should compile and run"
    );
    // `99` printed exactly once (single call), then the awaited result.
    assert_eq!(out, "99\n42\n");
}

#[test]
fn test_await_item_imported_async_in_loop_reuses_slot() {
    let worker = r#"
pub async fn inc(n: i64) -> i64 {
    await sleep(1);
    return n + 1;
}
"#;
    let main = r#"
import worker::inc;

async fn run() -> i64 {
    let mut total = 0;
    let mut i = 0;
    while i < 3 {
        total = total + await inc(i);
        i = i + 1;
    }
    return total;
}

async fn main() {
    println(await run());
}
"#;
    let (out, ok) =
        compile_temp_project_and_run(&[("worker.wi", worker), ("main.wi", main)], "main.wi");
    assert!(
        ok,
        "awaiting an item-imported async fn in a loop should run"
    );
    // inc(0)+inc(1)+inc(2) = 1+2+3 = 6, with the suspend/resume slot reused each
    // iteration.
    assert_eq!(out, "6\n");
}

// ---------------------------------------------------------------------------
// Function-pointer spawn (willow-spawn-fptr).
//
// `spawn f(args)` where `f` is a function VALUE (a `fn(...)` local — a named
// function reference or a lambda) used to run the call INLINE at the spawn site
// and merely wrap the result in a frame. It now compiles a `call_indirect` poll
// trampoline and schedules the task on the cooperative scheduler, exactly like
// `spawn named_fn(args)`. The 20 perspectives below cover that behavior.
//
//  1. named fn in a `fn` local, single i64 arg → join returns the result
//  2. lambda value spawned → join returns the result
//  3. two-arg fptr spawn → correct combined result
//  4. zero-arg fptr spawn
//  5. bool-returning fptr spawn
//  6. f64-returning fptr spawn
//  7. String-returning fptr spawn (GC-managed result slot in the frame mask)
//  8. String args through the indirect trampoline (GC-managed arg slots)
//  9. result usable in arithmetic after join
// 10. multiple fptr spawns joined in spawn order
// 11. multiple fptr spawns joined OUT of spawn order
// 12. fptr spawn is DEFERRED, not inline: a print after spawn precedes the
//     task's print (the observable behavior change vs. the old inline fallback)
// 13. fptr spawn matches named-fn spawn ordering (same scheduled semantics)
// 14. fptr passed in as a `fn` PARAMETER, then spawned
// 15. the same fptr local spawned twice → two independent tasks
// 16. two DIFFERENT fptr signatures in one program → distinct trampolines
// 17. fptr spawn result equals the equivalent direct call
// 18. four-arg fptr spawn → arg slot offsets stay correct
// 19. mixed arg types (i64 + bool) through one indirect trampoline
// 20. GC stress: String-returning + String-arg fptr spawn survives collection
//     during scheduling/join (frame + arg rooting correctness)
// ---------------------------------------------------------------------------

#[test]
fn test_join_on_non_handle_reports_e0805() {
    assert_compile_error_contains(
        r#"
fn main() {
    let value = 1;
    value.join();
}
"#,
        &[
            "error[E0805]",
            "cannot call `join` on `i64`",
            "expected a task",
        ],
    );
}

#[test]
fn test_channel_send_type_mismatch_reports_e0802() {
    assert_compile_error_contains(
        r#"
fn send_bool(ch: Channel<i64>) {
    ch.send(true);
}

fn main() {
    println(1);
}
"#,
        &[
            "error[E0802]",
            "cannot send `bool` into `Channel<i64>`",
            "expected `i64`, found `bool`",
        ],
    );
}

#[test]
fn test_channel_operation_on_non_channel_reports_e0806() {
    assert_compile_error_contains(
        r#"
fn main() {
    let value = 1;
    value.recv();
}
"#,
        &[
            "error[E0806]",
            "cannot call `recv` on `i64`",
            "expected `Channel<T>`",
        ],
    );
}

#[test]
fn test_channel_i64_mvp_send_recv_compiles_and_runs() {
    let (out, ok) = compile_and_run(
        r#"
fn main() {
    let ch: Channel<i64> = Channel::new();
    ch.send(10);
    ch.send(32);
    println(ch.recv() + ch.recv());
}
"#,
    );
    assert!(ok, "Channel<i64> send/recv MVP should compile and run");
    assert_eq!(out, "42\n");
}

#[test]
fn test_channel_recv_empty_open_panics_instead_of_defaulting() {
    let (out, ok) = compile_and_run_check_exit(
        r#"
fn main() {
    let ch: Channel<i64> = Channel::new();
    println(ch.recv());
}
"#,
    );
    assert!(!ok, "empty open recv must fail instead of returning 0");
    assert!(
        out.contains("runtime panic: recv on empty open channel would block"),
        "{out}"
    );
}

#[test]
fn test_channel_recv_closed_empty_still_returns_default() {
    let (out, ok) = compile_and_run(
        r#"
fn main() {
    let ch: Channel<i64> = Channel::new();
    ch.close();
    println(ch.recv());
}
"#,
    );
    assert!(ok, "closed empty recv keeps the existing default behavior");
    assert_eq!(out, "0\n");
}

#[test]
fn test_channel_target_producer_spawn_example_compiles_and_runs() {
    let (out, ok) = compile_and_run(
        r#"
async fn producer(ch: Channel<i64>) {
    ch.send(10);
    ch.send(20);
    ch.close();
}

fn main() {
    let ch = Channel<i64>::new();
    let h = producer(ch);
    println(ch.recv());
    println(ch.recv());
    h.join();
}
"#,
    );
    assert!(
        ok,
        "target Channel producer/spawn example should compile and run"
    );
    assert_eq!(out, "10\n20\n");
}

#[test]
fn test_concurrency_generic_types_parse_and_type_check() {
    let (out, ok) = compile_and_run(
        r#"
fn takes_join(h: JoinHandle<i64>) {
}

fn takes_future(f: Future<String>) {
}

fn takes_channel(c: Channel<i64>) {
}

fn main() {
    println(1);
}
"#,
    );
    assert!(ok, "concurrency generic type annotations should compile");
    assert_eq!(out, "1\n");
}

#[test]
fn test_concurrency_generic_type_mismatch_is_reported() {
    assert_compile_error_contains(
        r#"
fn takes_join(h: JoinHandle<i64>) {
}

fn main() {
    takes_join(1);
}
"#,
        &[
            "error[E0201]",
            "mismatched types: expected `JoinHandle<i64>`, found `i64`",
            "expected `JoinHandle<i64>`",
        ],
    );
}

// ── Spawn / task: additional type and behaviour coverage ────────────────────

/// Void-return function can be spawned and joined; join completes without a value.
#[test]
fn test_spawn_void_function_join_completes() {
    let (out, ok) = compile_and_run(
        r#"
async fn say() {
    println("hi");
}

fn main() {
    let h = say();
    h.join();
    println("done");
}
"#,
    );
    assert!(ok, "void spawn/join should compile and run");
    assert_eq!(out, "hi\ndone\n");
}

/// Spawned function returning bool produces the correct bool value on join.
#[test]
fn test_spawn_bool_return_join_value() {
    let (out, ok) = compile_and_run(
        r#"
async fn is_even(x: i64) -> bool {
    return x % 2 == 0;
}

fn main() {
    let h1 = is_even(4);
    let h2 = is_even(7);
    println(h1.join());
    println(h2.join());
}
"#,
    );
    assert!(ok, "bool-return spawn/join should compile and run");
    assert_eq!(out, "true\nfalse\n");
}

/// Spawned function returning f64 produces the correct value on join.
#[test]
fn test_spawn_f64_return_join_value() {
    let (out, ok) = compile_and_run(
        r#"
async fn half(x: f64) -> f64 {
    return x / 2.0;
}

fn main() {
    let h = half(10.0);
    let r = h.join();
    println(r);
}
"#,
    );
    assert!(ok, "f64-return spawn/join should compile and run");
    assert_eq!(out.trim(), "5");
}

/// Function with three i64 parameters can be spawned; all args are forwarded.
#[test]
fn test_spawn_three_argument_function() {
    let (out, ok) = compile_and_run(
        r#"
async fn sum3(a: i64, b: i64, c: i64) -> i64 {
    return a + b + c;
}

fn main() {
    let h = sum3(10, 20, 30);
    println(h.join());
}
"#,
    );
    assert!(ok, "three-arg spawn should compile and run");
    assert_eq!(out, "60\n");
}

/// The result of join() can be used directly inside an arithmetic expression.
#[test]
fn test_spawn_join_result_used_in_expression() {
    let (out, ok) = compile_and_run(
        r#"
async fn square(x: i64) -> i64 {
    return x * x;
}

fn main() {
    let a = square(3);
    let b = square(4);
    println(a.join() + b.join());
}
"#,
    );
    assert!(ok, "join result in expression should compile and run");
    assert_eq!(out, "25\n");
}

/// The same function can be spawned multiple times; each task is independent.
#[test]
fn test_spawn_same_function_twice_produces_independent_results() {
    let (out, ok) = compile_and_run(
        r#"
async fn double(x: i64) -> i64 {
    return x * 2;
}

fn main() {
    let h1 = double(5);
    let h2 = double(6);
    println(h1.join());
    println(h2.join());
}
"#,
    );
    assert!(ok, "two spawns of same function should compile and run");
    assert_eq!(out, "10\n12\n");
}

/// Release-mode spawn/join produces the same output as debug mode.
#[test]
fn test_spawn_in_release_mode_produces_correct_output() {
    let id = unique_test_id();
    let src_path = temp_path(format!("willow_spawn_rel_{}.wi", id));
    let bin_path = temp_path(format!("willow_spawn_rel_{}", id));

    let source = r#"
async fn square(x: i64) -> i64 { return x * x; }
fn main() {
    let h = square(7);
    println(h.join());
}
"#;
    std::fs::write(&src_path, source).unwrap();

    let compiler = env!("CARGO_BIN_EXE_willowc");
    let output = std::process::Command::new(compiler)
        .args(["build", &src_path, "-o", &bin_path, "--release"])
        .output()
        .expect("failed to compile");

    assert!(
        output.status.success(),
        "release spawn build should succeed"
    );

    let run = std::process::Command::new(&bin_path)
        .output()
        .expect("failed to run binary");

    let _ = std::fs::remove_file(&src_path);
    let _ = std::fs::remove_file(&bin_path);
    let _ = std::fs::remove_file(format!("{bin_path}.wsmap"));

    assert!(run.status.success(), "release spawn binary should run");
    assert_eq!(
        String::from_utf8_lossy(&run.stdout).trim(),
        "49",
        "release spawn should produce correct output"
    );
}

/// Calling join() on a non-JoinHandle type (e.g. i64) must be a compile error.
#[test]
fn test_join_on_non_join_handle_reports_e0805() {
    assert_compile_error_contains(
        r#"
fn main() {
    let x: i64 = 42;
    println(x.join());
}
"#,
        &[
            "error[E0805]",
            "cannot call `join` on `i64`",
            "expected a task",
        ],
    );
}

// ── Arithmetic ───────────────────────────────────────────────────────────────

// ── Cooperative task cancellation (willow-0a6k.7) ───────────────────────────
// 20 perspectives: 1 cancel request visible immediately on an in-flight task, 2 cancel while parked on a
// timer (wakes + finalizes promptly), 3 is_cancelled true after request,
// 4 is_cancelled false for untouched task, 5 cancel is idempotent, 6 cancel
// after completion is a no-op (join still returns the value), 7 join on a
// cancelled task panics with the task id, 8 the panic aborts (non-zero exit),
// 9 other tasks are unaffected, 10 cancelled task's post-await side effects
// never run, 11 fan-out with one cancelled member, 12 cancel in a loop over
// handles, 13 program exits cleanly with a cancelled task never joined,
// 14 is_cancelled after finalization stays true, 15 sleeping 10s task
// cancelled -> program finishes fast (parked-wake path), 16 cancel + GC
// stress (frame root released for Cancelled), 17 void-returning task cancel,
// 18 is_cancelled on completed-then-cancel-requested stays false-ish (no-op),
// 19 checker rejects cancel with arguments, 20 checker rejects cancel on a
// non-task receiver.

#[test]
fn cancel_01_request_visible_immediately() {
    // The task must still be in flight when cancel() lands, so it sleeps; a
    // no-await task can legitimately COMPLETE before main cancels under the
    // multi-worker scheduler (that no-op case is cancel_06/cancel_18).
    let (out, ok) = compile_and_run(
        "async fn t() -> i64 { await sleep(200); return 1; }\nasync fn main() { let h = t(); h.cancel(); println(h.is_cancelled()); }",
    );
    assert!(ok, "{out}");
    assert_eq!(out, "true\n");
}

#[test]
fn cancel_02_while_parked_on_timer() {
    let (out, ok) = compile_and_run(
        "async fn t() -> i64 { await sleep(30); return 1; }\nasync fn main() { let h = t(); await sleep(1); h.cancel(); println(h.is_cancelled()); }",
    );
    assert!(ok, "{out}");
    assert_eq!(out, "true\n");
}

#[test]
fn cancel_03_is_cancelled_after_request() {
    let (out, ok) = compile_and_run(
        "async fn t() -> i64 { await sleep(20); return 1; }\nasync fn main() { let h = t(); h.cancel(); println(h.is_cancelled()); }",
    );
    assert!(ok, "{out}");
    assert_eq!(out, "true\n");
}

#[test]
fn cancel_04_untouched_task_not_cancelled() {
    let (out, ok) = compile_and_run(
        "async fn t() -> i64 { return 1; }\nasync fn main() { let h = t(); println(h.is_cancelled()); println(h.join()); }",
    );
    assert!(ok, "{out}");
    assert_eq!(out, "false\n1\n");
}

#[test]
fn cancel_05_idempotent() {
    let (out, ok) = compile_and_run(
        "async fn t() -> i64 { await sleep(20); return 1; }\nasync fn main() { let h = t(); h.cancel(); h.cancel(); h.cancel(); println(h.is_cancelled()); }",
    );
    assert!(ok, "{out}");
    assert_eq!(out, "true\n");
}

#[test]
fn cancel_06_after_completion_noop() {
    let (out, ok) = compile_and_run(
        "async fn t() -> i64 { return 42; }\nasync fn main() { let h = t(); let v = h.join(); h.cancel(); println(v); println(h.is_cancelled()); }",
    );
    assert!(ok, "{out}");
    assert_eq!(out, "42\nfalse\n");
}

#[test]
fn cancel_07_join_cancelled_panics_with_id() {
    let (out, ok) = compile_and_run_check_exit(
        "async fn t() -> i64 { await sleep(30); return 1; }\nasync fn main() { let h = t(); h.cancel(); println(h.join()); }",
    );
    assert!(!ok);
    assert!(out.contains("joined a cancelled task (task"), "{out}");
}

#[test]
fn cancel_08_join_cancelled_aborts() {
    let (_, ok) = compile_and_run_check_exit(
        "async fn t() -> i64 { await sleep(30); return 1; }\nasync fn main() { let h = t(); h.cancel(); h.join(); }",
    );
    assert!(!ok);
}

#[test]
fn cancel_09_other_tasks_unaffected() {
    let (out, ok) = compile_and_run(
        "async fn t(n: i64) -> i64 { await sleep(5); return n * 10; }\nasync fn main() { let a = t(1); let b = t(2); let c = t(3); b.cancel(); println(a.join()); println(c.join()); }",
    );
    assert!(ok, "{out}");
    assert_eq!(out, "10\n30\n");
}

#[test]
fn cancel_10_post_await_side_effects_never_run() {
    // The cancelled task must NOT print after its sleep resumes.
    let (out, ok) = compile_and_run(
        "async fn noisy() -> i64 { await sleep(10); println(999); return 1; }\nasync fn main() { let h = noisy(); h.cancel(); await sleep(40); println(0); }",
    );
    assert!(ok, "{out}");
    assert_eq!(out, "0\n");
}

#[test]
fn cancel_11_fan_out_with_one_cancelled() {
    let (out, ok) = compile_and_run(
        "async fn t(n: i64) -> i64 { await sleep(5); return n; }\nasync fn main() { let a = t(1); let b = t(2); let c = t(3); let d = t(4); c.cancel(); println(a.join() + b.join() + d.join()); }",
    );
    assert!(ok, "{out}");
    assert_eq!(out, "7\n");
}

#[test]
fn cancel_12_loop_over_handles() {
    let (out, ok) = compile_and_run(
        "async fn t(n: i64) -> i64 { await sleep(20); return n; }\nasync fn main() { let a = t(1); let b = t(2); a.cancel(); b.cancel(); println(a.is_cancelled()); println(b.is_cancelled()); }",
    );
    assert!(ok, "{out}");
    assert_eq!(out, "true\ntrue\n");
}

#[test]
fn cancel_13_program_exits_with_unjoined_cancelled() {
    let (out, ok) = compile_and_run(
        "async fn t() -> i64 { await sleep(30); return 1; }\nasync fn main() { let h = t(); h.cancel(); println(7); }",
    );
    assert!(ok, "{out}");
    assert_eq!(out, "7\n");
}

#[test]
fn cancel_14_stays_cancelled_after_finalization() {
    let (out, ok) = compile_and_run(
        "async fn t() -> i64 { await sleep(5); return 1; }\nasync fn main() { let h = t(); h.cancel(); await sleep(30); println(h.is_cancelled()); }",
    );
    assert!(ok, "{out}");
    assert_eq!(out, "true\n");
}

#[test]
fn cancel_15_long_sleeper_finishes_fast() {
    // A 10-second sleeper cancelled immediately: the run must finish well
    // within the harness timeout because cancel re-queues the parked task.
    let (out, ok) = compile_and_run(
        "async fn t() -> i64 { await sleep(10000); return 1; }\nasync fn main() { let h = t(); h.cancel(); println(h.is_cancelled()); }",
    );
    assert!(ok, "{out}");
    assert_eq!(out, "true\n");
}

#[test]
fn cancel_16_gc_stress() {
    let (out, ok) = compile_and_run_gc_stress(
        "async fn t() -> String { await sleep(20); return \"kept\"; }\nasync fn main() { let h = t(); h.cancel(); await sleep(40); println(h.is_cancelled()); }",
    );
    assert!(ok, "{out}");
    assert_eq!(out, "true\n");
}

#[test]
fn cancel_17_void_task() {
    let (out, ok) = compile_and_run(
        "async fn t() { await sleep(20); }\nasync fn main() { let h = t(); h.cancel(); println(h.is_cancelled()); }",
    );
    assert!(ok, "{out}");
    assert_eq!(out, "true\n");
}

#[test]
fn cancel_18_completed_then_request_reports_false() {
    let (out, ok) = compile_and_run(
        "async fn t() -> i64 { return 1; }\nasync fn main() { let h = t(); h.join(); h.cancel(); println(h.is_cancelled()); }",
    );
    assert!(ok, "{out}");
    assert_eq!(out, "false\n");
}

#[test]
fn cancel_19_arguments_rejected() {
    let (ok, stderr) = compile_with_compiler_env(
        "async fn t() -> i64 { return 1; }\nasync fn main() { let h = t(); h.cancel(1); }",
        &[],
    );
    assert!(!ok);
    assert!(stderr.contains("0 arguments"), "{stderr}");
}

#[test]
fn cancel_20_non_task_receiver_rejected() {
    let (ok, stderr) = compile_with_compiler_env("fn main() { let x = 5; x.cancel(); }", &[]);
    assert!(!ok);
    assert!(!stderr.is_empty());
}

// ── Task-id + spawn-site debug traces (willow-0a6k.7 slice 2) ───────────────
// 10 perspectives on top of cancel_01-20: 1 panic trace carries task id,
// 2 panic trace carries spawn file:line, 3 spawn line is the CALL site (not
// the async fn definition), 4 fire-and-forget spawn (`let h = f();`) records
// a site, 5 await-driven spawn records a site, 6 the runtime-spawned main
// task has an id but no spawn site, 7 two tasks get distinct ids, 8 nested
// spawn chain shows a line per awaiter with ids, 9 cancelled-join panic
// reports the cancelled task's id, 10 chain text keeps the fn name.

#[test]
fn trace_01_panic_has_task_id() {
    let (out, ok) = compile_and_run_check_exit(
        "async fn t() -> i64 { await sleep(1); panic(\"boom\"); }\nasync fn main() { let h = t(); println(h.join()); }",
    );
    assert!(!ok);
    assert!(out.contains("[task "), "{out}");
}

#[test]
fn trace_02_panic_has_spawn_site() {
    let (out, ok) = compile_and_run_check_exit(
        "async fn t() -> i64 { await sleep(1); panic(\"boom\"); }\nasync fn main() { let h = t(); println(h.join()); }",
    );
    assert!(!ok);
    assert!(out.contains("spawned at "), "{out}");
    assert!(out.contains(".wi:2"), "{out}");
}

#[test]
fn trace_03_spawn_line_is_call_site() {
    // The spawn happens on line 4, the async fn is defined on line 1.
    let (out, ok) = compile_and_run_check_exit(
        "async fn t() -> i64 { await sleep(1); panic(\"boom\"); }\nfn pad1() {}\nfn pad2() {}\nasync fn main() { let h = t(); println(h.join()); }",
    );
    assert!(!ok);
    assert!(out.contains(".wi:4"), "{out}");
    assert!(
        !out.contains(".wi:1]"),
        "must not point at the definition line: {out}"
    );
}

#[test]
fn trace_04_fire_and_forget_records_site() {
    // No await between spawn and panic-join: the plain-call spawn path.
    let (out, ok) = compile_and_run_check_exit(
        "async fn t() -> i64 { panic(\"boom\"); }\nasync fn main() { let h = t(); println(h.join()); }",
    );
    assert!(!ok);
    assert!(out.contains("spawned at "), "{out}");
}

#[test]
fn trace_05_awaited_spawn_records_site() {
    let (out, ok) = compile_and_run_check_exit(
        "async fn t() -> i64 { await sleep(1); panic(\"boom\"); }\nasync fn main() { let v = await t(); println(v); }",
    );
    assert!(!ok);
    assert!(out.contains("spawned at "), "{out}");
}

#[test]
fn trace_06_main_task_has_id_but_no_site() {
    let (out, ok) =
        compile_and_run_check_exit("async fn main() { await sleep(1); panic(\"boom\"); }");
    assert!(!ok);
    assert!(out.contains("async main [task "), "{out}");
    assert!(!out.contains("async main [task 1, spawned"), "{out}");
}

#[test]
fn trace_07_distinct_task_ids() {
    let (out, ok) = compile_and_run(
        "async fn t() -> i64 { await sleep(1); return 1; }\nasync fn main() { let a = t(); let b = t(); println(a.join() + b.join()); }",
    );
    assert!(ok, "{out}");
    assert_eq!(out, "2\n");
}

#[test]
fn trace_08_nested_chain_ids_per_frame() {
    let (out, ok) = compile_and_run_check_exit(
        "async fn inner() -> i64 { await sleep(1); panic(\"boom\"); }\nasync fn outer() -> i64 { let v = await inner(); return v; }\nasync fn main() { println(await outer()); }",
    );
    assert!(!ok);
    assert!(out.contains("0: async inner [task "), "{out}");
    assert!(out.contains("1: async outer [task "), "{out}");
}

#[test]
fn trace_09_cancelled_join_reports_task_id() {
    let (out, ok) = compile_and_run_check_exit(
        "async fn t() -> i64 { await sleep(30); return 1; }\nasync fn main() { let h = t(); h.cancel(); println(h.join()); }",
    );
    assert!(!ok);
    assert!(out.contains("joined a cancelled task (task"), "{out}");
}

#[test]
fn trace_10_chain_keeps_fn_name() {
    let (out, ok) = compile_and_run_check_exit(
        "async fn my_worker() -> i64 { await sleep(1); panic(\"boom\"); }\nasync fn main() { println(my_worker().join()); }",
    );
    assert!(!ok);
    assert!(out.contains("async my_worker"), "{out}");
}

// ── Top-level panic policy (willow-0a6k.7, spec §18) ────────────────────────
// Willow has no recover: the FINAL policy is abort-the-program. A panic
// anywhere — any task, any worker thread, any depth — prints the located
// `runtime panic:` message, the call stack, and the async chain (task id +
// spawn site), then aborts. The Panicked task state stays RESERVED for a
// possible future recoverable policy; nothing sets it today.
// 10 perspectives: 1 panic in a joined task aborts with message, 2 exit is
// abnormal (abort), 3 panic in main task, 4 panic in a deep await chain
// reports the full chain, 5 panic with format args interpolates, 6 panic in
// a fire-and-forget task that is never joined still aborts the program when
// it runs, 7 panic inside a loop inside async, 8 panic in a class method
// called from a task, 9 the message appears exactly once (no double
// report from scheduler re-entry), 10 panic in one of several workers'
// tasks aborts even while siblings are mid-sleep.

#[test]
fn ppol_01_joined_task_aborts_with_message() {
    let (out, ok) = compile_and_run_check_exit(
        "async fn t() -> i64 { await sleep(1); panic(\"boom\"); }\nasync fn main() { let h = t(); println(h.join()); }",
    );
    assert!(!ok);
    assert!(out.contains("runtime panic: boom"), "{out}");
}

#[test]
fn ppol_02_abnormal_exit() {
    let (_, ok) = compile_and_run_check_exit(
        "async fn t() { await sleep(1); panic(\"x\"); }\nasync fn main() { let h = t(); h.join(); println(1); }",
    );
    assert!(!ok);
}

#[test]
fn ppol_03_panic_in_main_task() {
    let (out, ok) =
        compile_and_run_check_exit("async fn main() { await sleep(1); panic(\"main-boom\"); }");
    assert!(!ok);
    assert!(out.contains("runtime panic: main-boom"), "{out}");
}

#[test]
fn ppol_04_deep_chain_reported() {
    let (out, ok) = compile_and_run_check_exit(
        "async fn a() -> i64 { await sleep(1); panic(\"deep\"); }\nasync fn b() -> i64 { return await a(); }\nasync fn c() -> i64 { return await b(); }\nasync fn main() { println(await c()); }",
    );
    assert!(!ok);
    assert!(out.contains("async a"), "{out}");
    assert!(out.contains("async b"), "{out}");
    assert!(out.contains("async c"), "{out}");
}

#[test]
fn ppol_05_format_args() {
    let (out, ok) = compile_and_run_check_exit(
        "async fn t(n: i64) { await sleep(1); panic(\"worker {} died\", n); }\nasync fn main() { let h = t(42); h.join(); }",
    );
    assert!(!ok);
    assert!(out.contains("worker 42 died"), "{out}");
}

#[test]
fn ppol_06_unjoined_task_panic_aborts() {
    // Fire-and-forget panicking task: main sleeps long enough for it to run.
    let (out, ok) = compile_and_run_check_exit(
        "async fn t() { await sleep(1); panic(\"orphan\"); }\nasync fn main() { let h = t(); await sleep(200); println(9); }",
    );
    assert!(!ok);
    assert!(out.contains("orphan"), "{out}");
}

#[test]
fn ppol_07_panic_inside_async_loop() {
    let (out, ok) = compile_and_run_check_exit(
        "async fn t() { for i in 0..10 { await sleep(1); if i == 3 { panic(\"at {}\", i); } } }\nasync fn main() { let h = t(); h.join(); }",
    );
    assert!(!ok);
    assert!(out.contains("at 3"), "{out}");
}

#[test]
fn ppol_08_panic_in_method_from_task() {
    let (out, ok) = compile_and_run_check_exit(
        "class W { pub fn go(self) -> i64 { panic(\"method-boom\"); } }\nasync fn t() -> i64 { await sleep(1); let w = new W(); return w.go(); }\nasync fn main() { let h = t(); println(h.join()); }",
    );
    assert!(!ok);
    assert!(out.contains("method-boom"), "{out}");
}

#[test]
fn ppol_09_message_exactly_once() {
    let (out, ok) = compile_and_run_check_exit(
        "async fn t() { await sleep(1); panic(\"once\"); }\nasync fn main() { let h = t(); h.join(); }",
    );
    assert!(!ok);
    assert_eq!(out.matches("runtime panic: once").count(), 1, "{out}");
}

#[test]
fn ppol_10_panic_while_siblings_sleep() {
    let (out, ok) = compile_and_run_check_exit(
        "async fn sleeper() { await sleep(5000); }\nasync fn bad() { await sleep(5); panic(\"among-workers\"); }\nasync fn main() { let a = sleeper(); let b = sleeper(); let c = bad(); c.join(); }",
    );
    assert!(!ok);
    assert!(out.contains("among-workers"), "{out}");
}

// ── Cancel runtime integrity (willow-vynv.1, defer Phase 1) ─────────────────
// Integration half; unit half lives in the runtime crate (finalize wakes
// parked awaiters + claims them, wake no-ops on Cancelled, stale timer entry
// revalidated away, netpoll purge_task own/shared-fd, channel send drains all
// waiters). 6 e2e perspectives: 1 cancelled recv-parked consumer does not
// swallow the wake — a live consumer still receives (the lost-wakeup bug),
// 2 await on a cancelled task is a located panic (not garbage from the
// result slot), 3 program with a task cancelled while parked on recv exits
// cleanly, 4 close() after a cancel still releases live consumers,
// 5 cancelled consumer's post-recv side effects never run, 6 send to a
// queue holding ONLY a cancelled waiter keeps the value for a later recv.

#[test]
fn cnl2_01_cancelled_consumer_no_lost_wakeup() {
    let (out, ok) = compile_and_run(
        "async fn consumer(ch: Channel<i64>, tag: i64) -> i64 { let v = ch.recv(); println(tag); return v; }\nasync fn main() { let ch = Channel<i64>::new(); let dead = consumer(ch, 1); await sleep(20); dead.cancel(); await sleep(20); let live = consumer(ch, 2); await sleep(20); ch.send(42); println(live.join()); }",
    );
    assert!(ok, "{out}");
    assert_eq!(out, "2\n42\n");
}

#[test]
fn cnl2_02_await_cancelled_is_located_panic() {
    let (out, ok) = compile_and_run_check_exit(
        "async fn slow() -> i64 { await sleep(100); return 5; }\nasync fn main() { let t = slow(); t.cancel(); await sleep(20); println(t.join()); }",
    );
    assert!(!ok);
    assert!(out.contains("awaited/joined a cancelled task"), "{out}");
}

#[test]
fn cnl2_03_cancel_recv_parked_exits_cleanly() {
    let (out, ok) = compile_and_run(
        "async fn consumer(ch: Channel<i64>) -> i64 { let v = ch.recv(); return v; }\nasync fn main() { let ch = Channel<i64>::new(); let h = consumer(ch); await sleep(20); h.cancel(); await sleep(20); println(h.is_cancelled()); }",
    );
    assert!(ok, "{out}");
    assert_eq!(out, "true\n");
}

#[test]
fn cnl2_04_close_after_cancel_releases_live() {
    let (out, ok) = compile_and_run(
        "async fn consumer(ch: Channel<i64>, tag: i64) -> i64 { let v = ch.recv(); println(tag); return v; }\nasync fn main() { let ch = Channel<i64>::new(); let dead = consumer(ch, 1); await sleep(20); dead.cancel(); await sleep(20); let live = consumer(ch, 2); await sleep(20); ch.close(); println(live.join()); }",
    );
    assert!(ok, "{out}");
    assert_eq!(out, "2\n0\n");
}

#[test]
fn cnl2_05_cancelled_consumer_side_effects_never_run() {
    let (out, ok) = compile_and_run(
        "async fn consumer(ch: Channel<i64>) { let v = ch.recv(); println(999); }\nasync fn main() { let ch = Channel<i64>::new(); let h = consumer(ch); await sleep(20); h.cancel(); await sleep(20); ch.send(1); await sleep(50); println(0); }",
    );
    assert!(ok, "{out}");
    assert_eq!(out, "0\n");
}

#[test]
fn cnl2_06_value_survives_for_later_recv() {
    let (out, ok) = compile_and_run(
        "async fn consumer(ch: Channel<i64>) { ch.recv(); println(9); }\nasync fn main() { let ch = Channel<i64>::new(); let h = consumer(ch); await sleep(20); h.cancel(); await sleep(20); ch.send(77); await sleep(20); println(ch.recv()); }",
    );
    assert!(ok, "{out}");
    assert_eq!(out, "77\n");
}

// ── Async defer + cancellation cleanup (willow-vynv.3) ──────────────────────
// v1 semantics: in an async fn, defers scope to the FUNCTION (the coop path
// has no block-scope machinery); registration sets a frame FLAG + stashes
// operands into GC-masked frame slots; every normal exit (return/fallthrough)
// flushes LIFO and clears flags; cancellation runs the compiler-generated
// __coop_cancel entry on a worker WITHOUT the scheduler lock (Cancelling
// state), executing only still-flagged sites in reverse lexical order.
// 20 perspectives: 1 normal return flushes LIFO, 2 fallthrough (void) exit
// flushes, 3 return-position call-await flushes, 4 cancel runs pending
// defers, 5 cancel runs ONLY sites registered before the suspension point,
// 6 cancel before any registration runs nothing, 7 defers do not run twice
// when cancelled after completion, 8 operand stashed before await survives
// suspension into the cancel path, 9 receiver stash for method defer,
// 10 async METHOD defer, 11 string operand + GC stress across cancel,
// 12 print form, 13 two flagged sites run reverse-lexically on cancel,
// 14 join after cancel still panics AFTER cleanup ran, 15 cancel of a
// sleep-parked task runs defers promptly (10s sleeper), 16 defer in async
// loop body registers per iteration and flushes once at exit with the last
// operands (documented v1 function scoping), 17 await inside defer rejected,
// 18 async callee inside defer rejected (async context too), 19 args
// evaluated at registration in async, 20 unjoined cancelled task's defers
// still run before program exit.

#[test]
fn adfr_01_normal_return_lifo() {
    let (out, ok) = compile_and_run(
        "fn c(n: i64) { println(n); }\nasync fn w() -> i64 { defer c(1); defer c(2); await sleep(1); return 7; }\nasync fn main() { println(w().join()); }",
    );
    assert!(ok, "{out}");
    assert_eq!(out, "2\n1\n7\n");
}

#[test]
fn adfr_02_fallthrough_flushes() {
    let (out, ok) = compile_and_run(
        "fn c() { println(3); }\nasync fn w() { defer c(); await sleep(1); println(1); }\nasync fn main() { w().join(); println(9); }",
    );
    assert!(ok, "{out}");
    assert_eq!(out, "1\n3\n9\n");
}

#[test]
fn adfr_03_return_position_await_flushes() {
    let (out, ok) = compile_and_run(
        "fn c() { println(5); }\nasync fn inner() -> i64 { await sleep(1); return 4; }\nasync fn w() -> i64 { defer c(); return await inner(); }\nasync fn main() { println(w().join()); }",
    );
    assert!(ok, "{out}");
    assert_eq!(out, "5\n4\n");
}

#[test]
fn adfr_04_cancel_runs_pending() {
    let (out, ok) = compile_and_run(
        "fn c() { println(42); }\nasync fn w() { defer c(); await sleep(5000); }\nasync fn main() { let h = w(); await sleep(20); h.cancel(); await sleep(50); println(h.is_cancelled()); }",
    );
    assert!(ok, "{out}");
    assert_eq!(out, "42\ntrue\n");
}

#[test]
fn adfr_05_cancel_only_registered_sites() {
    // Site 2 sits AFTER the suspension the cancel interrupts: never registered.
    let (out, ok) = compile_and_run(
        "fn c(n: i64) { println(n); }\nasync fn w() { defer c(10); await sleep(5000); defer c(20); await sleep(1); }\nasync fn main() { let h = w(); await sleep(20); h.cancel(); await sleep(50); println(0); }",
    );
    assert!(ok, "{out}");
    assert_eq!(out, "10\n0\n");
}

#[test]
fn adfr_06_cancel_before_registration() {
    // Cancelled before its first poll: no defer registered, none run.
    let (out, ok) = compile_and_run(
        "fn c() { println(99); }\nasync fn w() { await sleep(200); defer c(); await sleep(200); }\nasync fn main() { let h = w(); await sleep(20); h.cancel(); await sleep(50); println(0); }",
    );
    assert!(ok, "{out}");
    assert_eq!(out, "0\n");
}

#[test]
fn adfr_07_no_double_run_after_completion() {
    let (out, ok) = compile_and_run(
        "fn c() { println(8); }\nasync fn w() -> i64 { defer c(); await sleep(1); return 1; }\nasync fn main() { let h = w(); println(h.join()); h.cancel(); await sleep(30); println(0); }",
    );
    assert!(ok, "{out}");
    assert_eq!(out, "8\n1\n0\n");
}

#[test]
fn adfr_08_operand_survives_suspension() {
    let (out, ok) = compile_and_run(
        "fn c(n: i64) { println(n); }\nasync fn w(x: i64) { defer c(x * 7); await sleep(5000); }\nasync fn main() { let h = w(6); await sleep(20); h.cancel(); await sleep(50); println(0); }",
    );
    assert!(ok, "{out}");
    assert_eq!(out, "42\n0\n");
}

#[test]
fn adfr_09_method_receiver_stash() {
    let (out, ok) = compile_and_run(
        "class R { pub v: i64; pub fn show(self) { println(self.v); } }\nasync fn w() { let r = new R(5); defer r.show(); await sleep(5000); }\nasync fn main() { let h = w(); await sleep(20); h.cancel(); await sleep(50); println(0); }",
    );
    assert!(ok, "{out}");
    assert_eq!(out, "5\n0\n");
}

#[test]
fn adfr_10_async_method_defer() {
    let (out, ok) = compile_and_run(
        "fn c() { println(4); }\nclass W { pub async fn go(self) -> i64 { defer c(); await sleep(1); return 6; } }\nasync fn main() { let w = new W(); println((w.go()).join()); }",
    );
    assert!(ok, "{out}");
    assert_eq!(out, "4\n6\n");
}

#[test]
fn adfr_11_string_operand_gc_stress_cancel() {
    let (out, ok) = compile_and_run_gc_stress(
        "fn c(s: String) { println(s); }\nasync fn w() { let name = \"a\" + \"b\"; defer c(name + \"!\"); await sleep(5000); }\nasync fn main() { let h = w(); await sleep(20); h.cancel(); await sleep(80); println(\"end\"); }",
    );
    assert!(ok, "{out}");
    assert_eq!(out, "ab!\nend\n");
}

#[test]
fn adfr_12_print_form() {
    let (out, ok) = compile_and_run(
        "async fn w() -> i64 { let x = 3; defer println(x); await sleep(1); return 1; }\nasync fn main() { println(w().join()); }",
    );
    assert!(ok, "{out}");
    assert_eq!(out, "3\n1\n");
}

#[test]
fn adfr_13_cancel_two_sites_reverse() {
    let (out, ok) = compile_and_run(
        "fn c(n: i64) { println(n); }\nasync fn w() { defer c(1); defer c(2); await sleep(5000); }\nasync fn main() { let h = w(); await sleep(20); h.cancel(); await sleep(50); println(0); }",
    );
    assert!(ok, "{out}");
    assert_eq!(out, "2\n1\n0\n");
}

#[test]
fn adfr_14_join_after_cancel_panics_after_cleanup() {
    let (out, ok) = compile_and_run_check_exit(
        "fn c() { println(7); }\nasync fn w() -> i64 { defer c(); await sleep(5000); return 1; }\nasync fn main() { let h = w(); await sleep(20); h.cancel(); await sleep(50); println(h.join()); }",
    );
    assert!(!ok);
    assert!(out.contains('7'), "{out}");
    assert!(out.contains("cancelled task"), "{out}");
}

#[test]
fn adfr_15_long_sleeper_cleanup_prompt() {
    let (out, ok) = compile_and_run(
        "fn c() { println(1); }\nasync fn w() { defer c(); await sleep(10000); }\nasync fn main() { let h = w(); await sleep(20); h.cancel(); await sleep(50); println(2); }",
    );
    assert!(ok, "{out}");
    assert_eq!(out, "1\n2\n");
}

#[test]
fn adfr_16_loop_defer_function_scoped_v1() {
    // v1: in async fns the defer scopes to the FUNCTION — one pending
    // instance per site, flushed at exit with the LAST registration's operand.
    let (out, ok) = compile_and_run(
        "fn c(n: i64) { println(n); }\nasync fn w() { for i in 0..3 { defer c(i); await sleep(1); } println(8); }\nasync fn main() { w().join(); }",
    );
    assert!(ok, "{out}");
    assert_eq!(out, "8\n2\n");
}

#[test]
fn adfr_17_await_inside_defer_rejected() {
    let (ok, stderr) = compile_with_compiler_env(
        "async fn g() -> i64 { return 1; }\nfn c(n: i64) {}\nasync fn w() { defer c(await g()); }\nasync fn main() { w().join(); }",
        &[],
    );
    assert!(!ok);
    assert!(stderr.contains("E0905"), "{stderr}");
}

#[test]
fn adfr_18_async_callee_rejected_in_async_too() {
    let (ok, stderr) = compile_with_compiler_env(
        "async fn cleanup() {}\nasync fn w() { defer cleanup(); }\nasync fn main() { w().join(); }",
        &[],
    );
    assert!(!ok);
    assert!(stderr.contains("async call"), "{stderr}");
}

#[test]
fn adfr_19_args_registration_time_async() {
    let (out, ok) = compile_and_run(
        "fn c(n: i64) { println(n); }\nasync fn w() -> i64 { let mut x = 1; defer c(x); x = 50; await sleep(1); return x; }\nasync fn main() { println(w().join()); }",
    );
    assert!(ok, "{out}");
    assert_eq!(out, "1\n50\n");
}

#[test]
fn adfr_20_unjoined_cancelled_defers_run() {
    let (out, ok) = compile_and_run(
        "fn c() { println(3); }\nasync fn w() { defer c(); await sleep(5000); }\nasync fn main() { let h = w(); await sleep(20); h.cancel(); await sleep(60); println(4); }",
    );
    assert!(ok, "{out}");
    assert_eq!(out, "3\n4\n");
}

// ── Task::try_join -> Result<T, Cancelled> (willow-vynv.4) ──────────────────
// Cancellation is normal control flow: try_join never panics — a cancelled
// task yields Err(Cancelled), a completed one Ok(value). join() keeps the
// panic form until the Result migration (willow-aff) makes it the default.
// 20 perspectives: 1 Ok on completion, 2 Err on cancelled, 3 no panic /
// clean exit on the Err path, 4 void task Ok, 5 void task Err, 6 f64
// payload, 7 bool payload, 8 String payload (GC), 9 try_join drives the
// task (no prior join needed), 10 try_join after join returns Ok again
// (idempotent read), 11 cancel AFTER completion still Ok, 12 defer cleanup
// runs before Err observed, 13 ? propagation on the result inside a
// Result-returning fn, 14 match binding the error value type-checks,
// 15 arguments rejected, 16 non-task receiver rejected, 17 GC stress on
// Ok(String), 18 GC stress on Err path, 19 try_join in async fn (coop
// context), 20 two tasks one cancelled one not — both observed correctly.

#[test]
fn tjoin_01_ok_on_completion() {
    let (out, ok) = compile_and_run(
        "async fn w() -> i64 { await sleep(1); return 7; }\nasync fn main() { match w().try_join() { Ok(v) => println(v), Err(e) => println(0), } }",
    );
    assert!(ok, "{out}");
    assert_eq!(out, "7\n");
}

#[test]
fn tjoin_02_err_on_cancelled() {
    let (out, ok) = compile_and_run(
        "async fn w() -> i64 { await sleep(5000); return 7; }\nasync fn main() { let h = w(); await sleep(20); h.cancel(); await sleep(30); match h.try_join() { Ok(v) => println(v), Err(e) => println(9), } }",
    );
    assert!(ok, "{out}");
    assert_eq!(out, "9\n");
}

#[test]
fn tjoin_03_no_panic_clean_exit() {
    let (out, ok) = compile_and_run(
        "async fn w() -> i64 { await sleep(5000); return 7; }\nasync fn main() { let h = w(); h.cancel(); match h.try_join() { Ok(v) => println(v), Err(e) => println(1), } println(2); }",
    );
    assert!(ok, "{out}");
    assert_eq!(out, "1\n2\n");
}

#[test]
fn tjoin_04_void_ok() {
    let (out, ok) = compile_and_run(
        "async fn w() { await sleep(1); }\nasync fn main() { match w().try_join() { Ok(v) => println(1), Err(e) => println(0), } }",
    );
    assert!(ok, "{out}");
    assert_eq!(out, "1\n");
}

#[test]
fn tjoin_05_void_err() {
    let (out, ok) = compile_and_run(
        "async fn w() { await sleep(5000); }\nasync fn main() { let h = w(); await sleep(20); h.cancel(); await sleep(30); match h.try_join() { Ok(v) => println(0), Err(e) => println(1), } }",
    );
    assert!(ok, "{out}");
    assert_eq!(out, "1\n");
}

#[test]
fn tjoin_06_f64_payload() {
    let (out, ok) = compile_and_run(
        "async fn w() -> f64 { await sleep(1); return 2.5; }\nasync fn main() { match w().try_join() { Ok(v) => println(v), Err(e) => println(0), } }",
    );
    assert!(ok, "{out}");
    assert_eq!(out, "2.5\n");
}

#[test]
fn tjoin_07_bool_payload() {
    let (out, ok) = compile_and_run(
        "async fn w() -> bool { await sleep(1); return true; }\nasync fn main() { match w().try_join() { Ok(v) => println(v), Err(e) => println(false), } }",
    );
    assert!(ok, "{out}");
    assert_eq!(out, "true\n");
}

#[test]
fn tjoin_08_string_payload() {
    let (out, ok) = compile_and_run(
        "async fn w() -> String { await sleep(1); return \"hi\" + \"!\"; }\nasync fn main() { match w().try_join() { Ok(v) => println(v), Err(e) => println(\"no\"), } }",
    );
    assert!(ok, "{out}");
    assert_eq!(out, "hi!\n");
}

#[test]
fn tjoin_09_drives_task() {
    // No sleep in main: try_join itself must drive the scheduler.
    let (out, ok) = compile_and_run(
        "async fn w() -> i64 { await sleep(10); return 3; }\nasync fn main() { match w().try_join() { Ok(v) => println(v), Err(e) => println(0), } }",
    );
    assert!(ok, "{out}");
    assert_eq!(out, "3\n");
}

#[test]
fn tjoin_10_after_join() {
    let (out, ok) = compile_and_run(
        "async fn w() -> i64 { await sleep(1); return 4; }\nasync fn main() { let h = w(); println(h.join()); match h.try_join() { Ok(v) => println(v), Err(e) => println(0), } }",
    );
    assert!(ok, "{out}");
    assert_eq!(out, "4\n4\n");
}

#[test]
fn tjoin_11_cancel_after_completion_ok() {
    let (out, ok) = compile_and_run(
        "async fn w() -> i64 { return 5; }\nasync fn main() { let h = w(); h.join(); h.cancel(); match h.try_join() { Ok(v) => println(v), Err(e) => println(0), } }",
    );
    assert!(ok, "{out}");
    assert_eq!(out, "5\n");
}

#[test]
fn tjoin_12_defer_before_err() {
    let (out, ok) = compile_and_run(
        "fn c() { println(1); }\nasync fn w() { defer c(); await sleep(5000); }\nasync fn main() { let h = w(); await sleep(20); h.cancel(); await sleep(30); match h.try_join() { Ok(v) => println(0), Err(e) => println(2), } }",
    );
    assert!(ok, "{out}");
    assert_eq!(out, "1\n2\n");
}

#[test]
fn tjoin_13_question_mark_propagation() {
    let (out, ok) = compile_and_run(
        "async fn w() -> i64 { await sleep(5000); return 7; }\nasync fn main() { let h = w(); h.cancel(); match run(h) { Ok(v) => println(v), Err(e) => println(8), } }\nfn run(h: Task<i64>) -> Result<i64, Cancelled> { let v = h.try_join()?; return Ok(v + 1); }",
    );
    assert!(ok, "{out}");
    assert_eq!(out, "8\n");
}

#[test]
fn tjoin_14_error_binding_typechecks() {
    let (out, ok) = compile_and_run(
        "async fn w() -> i64 { await sleep(5000); return 7; }\nasync fn main() { let h = w(); h.cancel(); match h.try_join() { Ok(v) => println(v), Err(c) => { match c { Cancelled => println(6), } } } }",
    );
    assert!(ok, "{out}");
    assert_eq!(out, "6\n");
}

#[test]
fn tjoin_15_arguments_rejected() {
    let (ok, stderr) = compile_with_compiler_env(
        "async fn w() -> i64 { return 1; }\nasync fn main() { w().try_join(2); }",
        &[],
    );
    assert!(!ok);
    assert!(stderr.contains("0 arguments"), "{stderr}");
}

#[test]
fn tjoin_16_non_task_rejected() {
    let (ok, stderr) = compile_with_compiler_env("fn main() { let x = 1; x.try_join(); }", &[]);
    assert!(!ok);
    assert!(!stderr.is_empty());
}

#[test]
fn tjoin_17_gc_stress_ok_string() {
    let (out, ok) = compile_and_run_gc_stress(
        "async fn w() -> String { await sleep(1); return \"a\" + \"b\"; }\nasync fn main() { match w().try_join() { Ok(v) => println(v), Err(e) => println(\"no\"), } }",
    );
    assert!(ok, "{out}");
    assert_eq!(out, "ab\n");
}

#[test]
fn tjoin_18_gc_stress_err_path() {
    let (out, ok) = compile_and_run_gc_stress(
        "async fn w() -> String { await sleep(5000); return \"x\"; }\nasync fn main() { let h = w(); await sleep(20); h.cancel(); await sleep(40); match h.try_join() { Ok(v) => println(v), Err(e) => println(\"c\" + \"!\"), } }",
    );
    assert!(ok, "{out}");
    assert_eq!(out, "c!\n");
}

#[test]
fn tjoin_19_inside_async_fn() {
    let (out, ok) = compile_and_run(
        "async fn w() -> i64 { await sleep(1); return 2; }\nasync fn outer() -> i64 { let h = w(); match h.try_join() { Ok(v) => { return v * 10; } Err(e) => { return 0; } } }\nasync fn main() { println(outer().join()); }",
    );
    assert!(ok, "{out}");
    assert_eq!(out, "20\n");
}

#[test]
fn tjoin_20_mixed_pair() {
    let (out, ok) = compile_and_run(
        "async fn w(n: i64) -> i64 { await sleep(n); return n; }\nasync fn main() { let a = w(5); let b = w(5000); await sleep(20); b.cancel(); await sleep(30); match a.try_join() { Ok(v) => println(v), Err(e) => println(0), } match b.try_join() { Ok(v) => println(v), Err(e) => println(9), } }",
    );
    assert!(ok, "{out}");
    assert_eq!(out, "5\n9\n");
}

// ── Return-position channel recv is a real suspend point (willow-0a6k.6) ────
// `return ch.recv();` used to fall into the SYNC recv path: it block-drove
// the scheduler from inside its own poll (nested run), could not park, could
// not be cancelled, and aborted 'recv would block' at idle. Now it suspends
// like let/assign/expr-position recv. 20 perspectives: 1 value delivery,
// 2 parks (no abort) + cancellable, 3 clean exit unjoined, 4 close() ->
// type default, 5 String channel, 6 f64 channel, 7 bool channel, 8 two
// sequential return-recv consumers, 9 return-recv task awaited through an
// async chain, 10 defer flushes on the return-recv exit, 11 mixed let+return
// positions in one fn, 12 producer/consumer roundtrip, 13 GC stress,
// 14 is_cancelled true after cancel while parked, 15 10s-idle cancel is
// prompt (no block-drive hang), 16 sync-fn return recv unchanged (value),
// 17 try_join Ok on delivered value, 18 try_join Err on cancelled parked
// consumer, 19 send BEFORE first poll (buffered) still returns, 20 two
// channels, inner selected by arg.

#[test]
fn rrecv_01_value() {
    let (out, ok) = compile_and_run(
        "async fn c(ch: Channel<i64>) -> i64 { return ch.recv(); }\nasync fn main() { let ch = Channel<i64>::new(); let h = c(ch); await sleep(10); ch.send(42); println(h.join()); }",
    );
    assert!(ok, "{out}");
    assert_eq!(out, "42\n");
}

#[test]
fn rrecv_02_parks_and_cancellable() {
    let (out, ok) = compile_and_run(
        "async fn c(ch: Channel<i64>) -> i64 { return ch.recv(); }\nasync fn main() { let ch = Channel<i64>::new(); let h = c(ch); await sleep(20); h.cancel(); await sleep(20); println(h.is_cancelled()); }",
    );
    assert!(ok, "{out}");
    assert_eq!(out, "true\n");
}

#[test]
fn rrecv_03_clean_exit_unjoined() {
    let (out, ok) = compile_and_run(
        "async fn c(ch: Channel<i64>) -> i64 { return ch.recv(); }\nasync fn main() { let ch = Channel<i64>::new(); let h = c(ch); await sleep(20); h.cancel(); await sleep(20); println(7); }",
    );
    assert!(ok, "{out}");
    assert_eq!(out, "7\n");
}

#[test]
fn rrecv_04_close_default() {
    let (out, ok) = compile_and_run(
        "async fn c(ch: Channel<i64>) -> i64 { return ch.recv(); }\nasync fn main() { let ch = Channel<i64>::new(); let h = c(ch); await sleep(10); ch.close(); println(h.join()); }",
    );
    assert!(ok, "{out}");
    assert_eq!(out, "0\n");
}

#[test]
fn rrecv_05_string_channel() {
    let (out, ok) = compile_and_run(
        "async fn c(ch: Channel<String>) -> String { return ch.recv(); }\nasync fn main() { let ch = Channel<String>::new(); let h = c(ch); await sleep(10); ch.send(\"hi\"); println(h.join()); }",
    );
    assert!(ok, "{out}");
    assert_eq!(out, "hi\n");
}

#[test]
fn rrecv_06_f64_channel() {
    let (out, ok) = compile_and_run(
        "async fn c(ch: Channel<f64>) -> f64 { return ch.recv(); }\nasync fn main() { let ch = Channel<f64>::new(); let h = c(ch); await sleep(10); ch.send(2.5); println(h.join()); }",
    );
    assert!(ok, "{out}");
    assert_eq!(out, "2.5\n");
}

#[test]
fn rrecv_07_bool_channel() {
    let (out, ok) = compile_and_run(
        "async fn c(ch: Channel<bool>) -> bool { return ch.recv(); }\nasync fn main() { let ch = Channel<bool>::new(); let h = c(ch); await sleep(10); ch.send(true); println(h.join()); }",
    );
    assert!(ok, "{out}");
    assert_eq!(out, "true\n");
}

#[test]
fn rrecv_08_two_sequential_consumers() {
    let (out, ok) = compile_and_run(
        "async fn c(ch: Channel<i64>) -> i64 { return ch.recv(); }\nasync fn main() { let ch = Channel<i64>::new(); let a = c(ch); let b = c(ch); await sleep(10); ch.send(1); ch.send(2); println(a.join() + b.join()); }",
    );
    assert!(ok, "{out}");
    assert_eq!(out, "3\n");
}

#[test]
fn rrecv_09_through_async_chain() {
    let (out, ok) = compile_and_run(
        "async fn c(ch: Channel<i64>) -> i64 { return ch.recv(); }\nasync fn outer(ch: Channel<i64>) -> i64 { let v = await c(ch); return v * 2; }\nasync fn main() { let ch = Channel<i64>::new(); let h = outer(ch); await sleep(10); ch.send(21); println(h.join()); }",
    );
    assert!(ok, "{out}");
    assert_eq!(out, "42\n");
}

#[test]
fn rrecv_10_defer_flushes() {
    let (out, ok) = compile_and_run(
        "fn cl() { println(1); }\nasync fn c(ch: Channel<i64>) -> i64 { defer cl(); return ch.recv(); }\nasync fn main() { let ch = Channel<i64>::new(); let h = c(ch); await sleep(10); ch.send(5); println(h.join()); }",
    );
    assert!(ok, "{out}");
    assert_eq!(out, "1\n5\n");
}

#[test]
fn rrecv_11_mixed_positions() {
    let (out, ok) = compile_and_run(
        "async fn c(ch: Channel<i64>) -> i64 { let a = ch.recv(); return ch.recv(); }\nasync fn main() { let ch = Channel<i64>::new(); let h = c(ch); await sleep(10); ch.send(1); ch.send(9); println(h.join()); }",
    );
    assert!(ok, "{out}");
    assert_eq!(out, "9\n");
}

#[test]
fn rrecv_12_producer_roundtrip() {
    let (out, ok) = compile_and_run(
        "async fn produce(ch: Channel<i64>) { ch.send(3); ch.send(4); ch.close(); }\nasync fn consume(ch: Channel<i64>) -> i64 { return ch.recv(); }\nasync fn main() { let ch = Channel<i64>::new(); let p = produce(ch); let a = consume(ch); let b = consume(ch); println(a.join() + b.join()); p.join(); }",
    );
    assert!(ok, "{out}");
    assert_eq!(out, "7\n");
}

#[test]
fn rrecv_13_gc_stress() {
    let (out, ok) = compile_and_run_gc_stress(
        "async fn c(ch: Channel<String>) -> String { return ch.recv(); }\nasync fn main() { let ch = Channel<String>::new(); let h = c(ch); await sleep(10); ch.send(\"a\" + \"b\"); println(h.join()); }",
    );
    assert!(ok, "{out}");
    assert_eq!(out, "ab\n");
}

#[test]
fn rrecv_14_cancel_while_parked() {
    let (out, ok) = compile_and_run(
        "async fn c(ch: Channel<i64>) -> i64 { return ch.recv(); }\nasync fn main() { let ch = Channel<i64>::new(); let h = c(ch); await sleep(30); h.cancel(); await sleep(30); println(h.is_cancelled()); }",
    );
    assert!(ok, "{out}");
    assert_eq!(out, "true\n");
}

#[test]
fn rrecv_15_idle_cancel_prompt() {
    // Nothing else runs: a block-driving recv would hang or abort here.
    let (out, ok) = compile_and_run(
        "async fn c(ch: Channel<i64>) -> i64 { return ch.recv(); }\nasync fn main() { let ch = Channel<i64>::new(); let h = c(ch); await sleep(10); h.cancel(); await sleep(10); println(2); }",
    );
    assert!(ok, "{out}");
    assert_eq!(out, "2\n");
}

#[test]
fn rrecv_16_sync_fn_unchanged() {
    let (out, ok) = compile_and_run(
        "async fn produce(ch: Channel<i64>) { ch.send(6); }\nfn take(ch: Channel<i64>) -> i64 { return ch.recv(); }\nfn main() { let ch = Channel<i64>::new(); let p = produce(ch); println(take(ch)); p.join(); }",
    );
    assert!(ok, "{out}");
    assert_eq!(out, "6\n");
}

#[test]
fn rrecv_17_try_join_ok() {
    let (out, ok) = compile_and_run(
        "async fn c(ch: Channel<i64>) -> i64 { return ch.recv(); }\nasync fn main() { let ch = Channel<i64>::new(); let h = c(ch); await sleep(10); ch.send(8); match h.try_join() { Ok(v) => println(v), Err(e) => println(0), } }",
    );
    assert!(ok, "{out}");
    assert_eq!(out, "8\n");
}

#[test]
fn rrecv_18_try_join_err_on_cancel() {
    let (out, ok) = compile_and_run(
        "async fn c(ch: Channel<i64>) -> i64 { return ch.recv(); }\nasync fn main() { let ch = Channel<i64>::new(); let h = c(ch); await sleep(20); h.cancel(); await sleep(20); match h.try_join() { Ok(v) => println(v), Err(e) => println(9), } }",
    );
    assert!(ok, "{out}");
    assert_eq!(out, "9\n");
}

#[test]
fn rrecv_19_send_before_first_poll() {
    let (out, ok) = compile_and_run(
        "async fn c(ch: Channel<i64>) -> i64 { return ch.recv(); }\nasync fn main() { let ch = Channel<i64>::new(); ch.send(55); let h = c(ch); println(h.join()); }",
    );
    assert!(ok, "{out}");
    assert_eq!(out, "55\n");
}

#[test]
fn rrecv_20_two_channels() {
    let (out, ok) = compile_and_run(
        "async fn c(ch: Channel<i64>) -> i64 { return ch.recv(); }\nasync fn main() { let x = Channel<i64>::new(); let y = Channel<i64>::new(); let a = c(x); let b = c(y); await sleep(10); x.send(1); y.send(2); println(a.join()); println(b.join()); }",
    );
    assert!(ok, "{out}");
    assert_eq!(out, "1\n2\n");
}

// Review fixes on the recv suspend arms (willow-0a6k.6): is_channel_recv
// matched by NAME only, so a user-defined `recv()` method was hijacked into
// the channel runtime (with an i64 element-type fallback). The arms are now
// gated on the receiver actually typing as Channel<T>. 21 user recv() in
// return position dispatches to the class method, 22 user recv() in let
// position, 23 user recv() in expr-stmt position, 24 channel recv unchanged
// alongside a user recv in the same program.

#[test]
fn rrecv_21_user_recv_return_position() {
    let (out, ok, timed_out) = compile_and_run_with_env_timeout(
        "class Reader { pub base: i64; pub fn recv(self) -> i64 { return self.base + 1; } }\nasync fn f(x: Reader) -> i64 { return x.recv(); }\nasync fn main() { println(f(new Reader(41)).join()); }",
        &[("WILLOW_WORKERS", "5")],
        std::time::Duration::from_secs(3),
    );
    assert!(
        !timed_out,
        "user-defined recv() was treated as a channel:\n{out}"
    );
    assert!(ok, "{out}");
    assert_eq!(out, "42\n");
}

#[test]
fn rrecv_22_user_recv_let_position() {
    let (out, ok, timed_out) = compile_and_run_with_env_timeout(
        "class Reader { pub base: i64; pub fn recv(self) -> i64 { return self.base * 2; } }\nasync fn f(x: Reader) -> i64 { let v = x.recv(); return v; }\nasync fn main() { println(f(new Reader(5)).join()); }",
        &[("WILLOW_WORKERS", "5")],
        std::time::Duration::from_secs(3),
    );
    assert!(
        !timed_out,
        "user-defined recv() was treated as a channel:\n{out}"
    );
    assert!(ok, "{out}");
    assert_eq!(out, "10\n");
}

#[test]
fn rrecv_23_user_recv_expr_position() {
    let (out, ok, timed_out) = compile_and_run_with_env_timeout(
        "class Reader { pub n: i64; pub fn recv(self) -> i64 { println(self.n); return self.n; } }\nasync fn f(x: Reader) { x.recv(); }\nasync fn main() { f(new Reader(7)).join(); }",
        &[("WILLOW_WORKERS", "5")],
        std::time::Duration::from_secs(3),
    );
    assert!(
        !timed_out,
        "user-defined recv() was treated as a channel:\n{out}"
    );
    assert!(ok, "{out}");
    assert_eq!(out, "7\n");
}

#[test]
fn rrecv_24_channel_and_user_recv_coexist() {
    let (out, ok, timed_out) = compile_and_run_with_env_timeout(
        "class Reader { pub b: i64; pub fn recv(self) -> i64 { return self.b; } }\nasync fn c(ch: Channel<i64>, r: Reader) -> i64 { let a = ch.recv(); return a + r.recv(); }\nasync fn main() { let ch = Channel<i64>::new(); let h = c(ch, new Reader(2)); await sleep(10); ch.send(40); println(h.join()); }",
        &[("WILLOW_WORKERS", "5")],
        std::time::Duration::from_secs(3),
    );
    assert!(!timed_out, "mixed channel/user recv program hung:\n{out}");
    assert!(ok, "{out}");
    assert_eq!(out, "42\n");
}

#[test]
fn rrecv_25_channel_consumer_example_is_repeatable_with_five_workers() {
    let source = fs::read_to_string("example/channel_consumer.wi").unwrap();
    let expected = "consumer 1 done\n42\nconsumer 2 done\ntrue\n";

    // The old example started both consumers before sending one value. With
    // five workers either waiter could consume it, leaving `served.join()`
    // parked forever. Repeat enough times to exercise parallel scheduling,
    // while bounding every run so a regression fails instead of hanging CI.
    for iteration in 1..=20 {
        let (out, ok, timed_out) = compile_and_run_with_env_timeout(
            &source,
            &[("WILLOW_WORKERS", "5")],
            std::time::Duration::from_secs(3),
        );
        assert!(
            !timed_out,
            "channel_consumer hung on iteration {iteration}:\n{out}"
        );
        assert!(
            ok,
            "channel_consumer failed on iteration {iteration}:\n{out}"
        );
        assert_eq!(out, expected, "iteration {iteration}");
    }
}
