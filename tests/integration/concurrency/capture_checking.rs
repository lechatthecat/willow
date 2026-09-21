use super::*;

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
    assert!(
        !ok,
        "single-worker scheduling must preserve Send/Sync checks"
    );
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
