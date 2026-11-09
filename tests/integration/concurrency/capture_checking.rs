use super::*;

#[test]
fn test_dgwo4_nonsync_gc_arg_rejected_under_check() {
    let (ok, stderr) = compile_with_data_race_check(NONSYNC_ARG_SRC);
    assert!(!ok, "non-Sync Array arg should be rejected");
    assert!(stderr.contains("error[E2402]"), "{stderr}");
    assert!(stderr.contains("not `Sync`"), "{stderr}");
    assert!(stderr.contains("immediately awaited"), "{stderr}");
    assert!(stderr.contains("exclusive access"), "{stderr}");
}

#[test]
fn test_dgwo4_low_worker_override_still_rejects_nonsync_arg() {
    let (ok, stderr) = compile_with_compiler_env(NONSYNC_ARG_SRC, &[("WILLOW_WORKERS", "1")]);
    assert!(
        !ok,
        "single-worker scheduling must preserve Send/Sync checks"
    );
    assert!(stderr.contains("error[E2402]"), "{stderr}");
    assert!(stderr.contains("immediately awaited"), "{stderr}");
}

#[test]
fn test_dgwo4_e2402_help_names_array_remedies() {
    let (_ok, stderr) = compile_with_data_race_check(NONSYNC_ARG_SRC);
    assert!(
        stderr.contains("Mutex<Array<T>>") && stderr.contains("FrozenArray"),
        "{stderr}"
    );
}

#[test]
fn test_dgwo4_map_arg_rejected() {
    let (ok, stderr) = compile_with_data_race_check(
        r#"
import std::collections::Map;
async fn use_m(m: Map<String, i64>) -> i64 { await sleep(1); return 0; }
async fn forward(m: Map<String, i64>) { println(await use_m(m)); }
async fn main() {}
"#,
    );
    assert!(!ok);
    assert!(stderr.contains("error[E2402]"), "{stderr}");
    assert!(stderr.contains("immediately awaited"), "{stderr}");
    assert!(
        stderr.contains("FrozenMap") && stderr.contains("Mutex<Map"),
        "{stderr}"
    );
}

#[test]
fn test_immediate_await_fresh_array_is_exclusively_owned() {
    let (ok, stderr) = compile_with_data_race_check(
        "import std::collections::Array; async fn take(xs: Array<i64>) {} async fn main() { await take([1, 2]); }",
    );
    assert!(ok, "{stderr}");
}

#[test]
fn test_immediate_await_sheet_reports_field_and_remedies() {
    let (ok, stderr) = compile_with_data_race_check(
        r#"
import std::collections::Map;
class Sheet { pub cells: Map<i64, i64>; }
async fn evaluate(sheet: Sheet) -> i64 { return sheet.cells.len(); }
async fn forward(sheet: Sheet) { println(await evaluate(sheet)); }
async fn main() {}
"#,
    );
    assert!(!ok, "{stderr}");
    for text in [
        "error[E2402]",
        "field `cells:",
        "immediately awaited",
        "FrozenMap",
        "Mutex<Map",
    ] {
        assert!(stderr.contains(text), "{stderr}");
    }
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
fn owned_class_with_array_field_accepted() {
    // A class with a (non-Sync) Array field is not Sync.
    let (ok, _stderr) = compile_with_data_race_check(
        r#"
import std::collections::Array;
class Bag { pub xs: Array<i64>; }
async fn use_b(b: Bag) -> i64 { await sleep(1); return 0; }
async fn main() { let b = new Bag([1, 2]); println(await use_b(b)); }
"#,
    );
    assert!(
        ok,
        "a fresh class can be exclusively lent despite being non-Sync"
    );
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
async fn r(x: RwLock<i64>) -> i64 { await sleep(1); lock read x as value { return value; } }
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
async fn forward(xs: Array<i64>, ys: Array<i64>) {
    let m = Mutex::new(0);
    println(await f(9, xs, m, ys));
}
async fn main() {}
"#,
    );
    assert!(!ok);
    assert_eq!(stderr.matches("error[E2402]").count(), 2, "{stderr}");
}

#[test]
fn test_fn_pointer_send_sync_async_example() {
    let (out, ok) = compile_and_run(include_str!("../../../example/async_fn_pointer.wi"));
    assert!(ok, "{out}");
    assert_eq!(out, "42\n14\n");
}

#[test]
fn test_closure_async_e2402_help() {
    for source in [
        "async fn apply(f: closure(i64) -> i64) -> i64 { await sleep(1); return f(1); } fn main() {}",
        "async fn apply(f: closure(i64) -> i64) -> i64 { return f(1); } fn main() { let n = 2; let f: closure(i64) -> i64 = |x| x + n; let t = apply(f); }",
        "async fn main() { let f: closure(i64) -> i64 = |x| x + 1; await sleep(1); println(f(1)); }",
    ] {
        let (ok, stderr) = compile_with_data_race_check(source);
        assert!(!ok, "{stderr}");
        assert!(stderr.contains("E2402"), "{stderr}");
        assert!(
            stderr.contains("closure values have unknown captures"),
            "{stderr}"
        );
        assert!(stderr.contains("environment-free fn pointer"), "{stderr}");
        assert!(!stderr.contains("function and closure values"), "{stderr}");
    }
}

#[test]
fn test_fn_pointer_send_sync_return_and_field_gc_release() {
    let source = "fn twice(x: i64) -> i64 { return x * 2; }
        class Callback { pub f: fn(i64) -> i64; }
        async fn get() -> fn(i64) -> i64 { await sleep(1); return twice; }
        async fn apply(c: Callback) -> i64 { let f = c.f; await sleep(1); return f(21); }
        async fn main() { let f = await get(); println(f(7)); println(await apply(new Callback(f))); }";
    for run in [compile_and_run_gc_stress, compile_and_run_release] {
        let (out, ok) = run(source);
        assert!(ok, "{out}");
        assert_eq!(out, "14\n42\n");
    }
}

#[test]
fn owned_task_loans_run_with_relocation_and_preemption() {
    let source = include_str!("../../../example/owned_task_loans.wi");
    for workers in ["1", "4"] {
        let (output, ok) = compile_and_run_with_runtime_env(
            source,
            &[
                ("WILLOW_WORKERS", workers),
                ("WILLOW_TASK_BUDGET", "1"),
                ("WILLOW_GC_STRESS", "relocate"),
            ],
            std::time::Duration::from_secs(30),
        );
        assert!(ok, "{output}");
        assert_eq!(output, "11\n13\n23\n1\n");
    }
    let (output, ok) = compile_and_run_release(source);
    assert!(ok, "{output}");
    assert_eq!(output, "11\n13\n23\n1\n");
}

#[test]
fn owned_task_loans_resolved_call_forms() {
    let source = r#"
import std::collections::Array;
class Worker {
    pub async fn add(self, a: Array<i64>) { a[0] = a[0] + 1; }
    pub static async fn twice(a: Array<i64>) { a[0] = a[0] * 2; }
}
async fn main() {
    let a = [20];
    let worker = new Worker();
    await worker.add(a);
    let t = Worker::twice(a);
    await t;
    println(a[0]);
}
"#;
    let (output, ok) = compile_and_run(source);
    assert!(ok, "{output}");
    assert_eq!(output, "42\n");
}

#[test]
fn owned_task_loans_imported_callee_and_escape_contract() {
    let worker = "import std::collections::Array; pub async fn add(a: Array<i64>) { await sleep(0); a[0] = a[0] + 1; }";
    let (output, ok) = compile_temp_project_and_run(
        &[
            ("worker.wi", worker),
            (
                "main.wi",
                "import worker; async fn main() { let a = [41]; await worker::add(a); println(a[0]); }",
            ),
        ],
        "main.wi",
    );
    assert!(ok, "{output}");
    assert_eq!(output, "42\n");
    let errors = compile_temp_project_error_stderr(
        &[
            (
                "worker.wi",
                "import std::collections::Array; pub async fn add(a: Array<i64>) { let m = Mutex::new(a); }",
            ),
            (
                "main.wi",
                "import worker; async fn main() { let a = [41]; await worker::add(a); println(a[0]); }",
            ),
        ],
        "main.wi",
    );
    assert!(errors.contains("E2402"), "{errors}");
}

#[test]
fn owned_task_loans_reject_later_argument_mutex_race() {
    let source = r#"
import std::collections::Array;
async fn mutate(m: Mutex<Array<i64>>) {
    lock m as values { values[0] = 2; }
}
async fn take(a: Array<i64>, m: Mutex<Array<i64>>) {
    let child = mutate(m);
    a[0] = 3;
    await child;
}
async fn main() {
    let a = [1];
    let t = take(a, Mutex::new(a));
    await t;
}
"#;
    let (ok, errors) = compile_with_data_race_check(source);
    assert!(
        !ok && errors.contains("E2402") && errors.contains("after permanent transfer"),
        "{errors}"
    );
    // The same callee is valid when the mutex owns an independent collection.
    let independent = source.replace("take(a, Mutex::new(a))", "take(a, Mutex::new([9]))");
    let (ok, errors) = compile_with_data_race_check(&independent);
    assert!(ok, "{errors}");
}

#[test]
fn owned_class_task_loans_run_with_relocation_and_preemption() {
    let source = include_str!("../../../example/owned_class_task_loans.wi");
    for workers in ["1", "4"] {
        let (output, ok) = compile_and_run_with_runtime_env(
            source,
            &[
                ("WILLOW_WORKERS", workers),
                ("WILLOW_TASK_BUDGET", "1"),
                ("WILLOW_GC_STRESS", "relocate"),
            ],
            std::time::Duration::from_secs(30),
        );
        assert!(ok, "{output}");
        assert_eq!(output, "11\n13\n23\n14\n");
    }
    let (output, ok) = compile_and_run_release(source);
    assert!(ok, "{output}");
    assert_eq!(output, "11\n13\n23\n14\n");
}
