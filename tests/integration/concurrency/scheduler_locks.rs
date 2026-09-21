use super::*;

// ── Scheduler-aware `lock` statement, end to end (willow-38w.1.1 … .1.4) ─────
//
// The unit tests in `src/parser` and `src/semantic/type_checker` cover the
// grammar and the rule set (`lock_check_01` … `lock_check_30`). The suite below
// covers the STAGE 4 LOWERING: what the generated code actually does at run
// time. The lowering's central claim is that the release is exactly-once on
// every exit from the body, so most perspectives here are structured the same
// way — take the lock, leave the body through one specific edge, then take the
// lock AGAIN. A leaked release shows up as a hang (the test times out); a
// double release shows up as corruption or an abort.

/// Perspective 1: the canonical Mutex form compiles and runs end to end. No
/// staged gate survives for it — before Stage 4 this reported E2502.
#[test]
fn lock_lower_01_mutex_form_runs() {
    let (out, ok) = compile_and_run(
        r#"
async fn main() {
    let m = Mutex::new(41);
    lock m as mut value {
        value = value + 1;
    }
    lock m as value {
        println(value);
    }
}
"#,
    );
    assert!(ok, "lock lowering should compile and run: {out}");
    assert_eq!(out, "42\n");
}

/// Perspective 2: the write through a `mut` binding is COMMITTED when the
/// section ends. A binding that behaved like a local copy would print the old
/// value here, which is the exact bug `Mutex.get`/`.set` could not avoid.
#[test]
fn lock_lower_02_mut_binding_write_is_published() {
    let (out, ok) = compile_and_run(
        r#"
async fn write_it(m: Mutex<i64>) {
    lock m as mut value { value = 7; }
}
async fn main() {
    let m = Mutex::new(0);
    await write_it(m);
    lock m as value { println(value); }
}
"#,
    );
    assert!(ok, "{out}");
    assert_eq!(out, "7\n");
}

/// Perspective 3: a non-`mut` binding still observes the CURRENT value, not the
/// value captured when the mutex was constructed.
#[test]
fn lock_lower_03_shared_binding_reads_current_value() {
    let (out, ok) = compile_and_run(
        r#"
async fn main() {
    let m = Mutex::new(1);
    lock m as mut value { value = 2; }
    lock m as first { println(first); }
    lock m as mut value { value = 3; }
    lock m as second { println(second); }
}
"#,
    );
    assert!(ok, "{out}");
    assert_eq!(out, "2\n3\n");
}

/// Perspective 4: the lock target is evaluated EXACTLY ONCE. `next()` bumps a
/// counter and returns the same mutex either way, so a target re-evaluated on
/// the acquisition retry — or evaluated once for acquire and again for
/// release — changes the printed count.
#[test]
fn lock_lower_04_target_is_evaluated_exactly_once() {
    let (out, ok) = compile_and_run(
        r#"
class Holder {
    pub cell: Mutex<i64>;
    pub calls: BlockingCell<i64>;
    pub fn pick(self) -> Mutex<i64> {
        self.calls.set(self.calls.get() + 1);
        return self.cell;
    }
}
async fn main() {
    let h = new Holder(Mutex::new(0), BlockingCell::new(0));
    lock h.pick() as mut value { value = value + 1; }
    println(h.calls.get());
    lock h.cell as value { println(value); }
}
"#,
    );
    assert!(ok, "{out}");
    assert_eq!(out, "1\n1\n");
}

/// Perspective 5: mutual exclusion under contention. Four tasks each run 250
/// read-modify-writes on one mutex; every increment must survive. This is the
/// property the removed `get`/`set` pair could not provide.
#[test]
fn lock_lower_05_no_lost_updates_under_contention() {
    let (out, ok) = compile_and_run(CONTENDED_COUNTER_SRC);
    assert!(ok, "{out}");
    assert_eq!(out, "1000\n");
}

/// Perspective 6: the same program on a SINGLE worker. A contended acquisition
/// must park the task, not the worker thread — if it blocked the thread the one
/// worker would never run the owner, and this would deadlock rather than
/// merely serialize.
#[test]
fn lock_lower_06_no_lost_updates_on_one_worker() {
    let (out, ok) = compile_and_run_with_env(CONTENDED_COUNTER_SRC, &[("WILLOW_WORKERS", "1")]);
    assert!(ok, "single-worker run must not deadlock: {out}");
    assert_eq!(out, "1000\n");
}

/// Perspective 7: and with more workers than tasks, where real parallel
/// contention on the cell is most likely.
#[test]
fn lock_lower_07_no_lost_updates_on_sixteen_workers() {
    let (out, ok) = compile_and_run_with_env(CONTENDED_COUNTER_SRC, &[("WILLOW_WORKERS", "16")]);
    assert!(ok, "{out}");
    assert_eq!(out, "1000\n");
}

/// Perspective 8: `return` out of the body releases. The second acquisition
/// hangs if it does not.
#[test]
fn lock_lower_08_return_from_body_releases() {
    let (out, ok) = compile_and_run(
        r#"
async fn take(m: Mutex<i64>) -> i64 {
    lock m as mut value {
        value = value + 1;
        return value;
    }
}
async fn main() {
    let m = Mutex::new(0);
    println(await take(m));
    println(await take(m));
}
"#,
    );
    assert!(ok, "early return must release the lock: {out}");
    assert_eq!(out, "1\n2\n");
}

/// Perspective 9: `break` out of an enclosing loop from inside the body
/// releases, and the committed write survives the jump.
#[test]
fn lock_lower_09_break_from_body_releases() {
    let (out, ok) = compile_and_run(
        r#"
async fn main() {
    let m = Mutex::new(0);
    let mut i = 0;
    while i < 10 {
        lock m as mut value {
            value = value + 1;
            if value == 3 {
                break;
            }
        }
        i = i + 1;
    }
    lock m as value { println(value); }
}
"#,
    );
    assert!(ok, "break out of a lock body must release: {out}");
    assert_eq!(out, "3\n");
}

/// Perspective 10: `continue` releases too, and the loop keeps making progress
/// — a leaked release would wedge on the next iteration's acquisition.
#[test]
fn lock_lower_10_continue_from_body_releases() {
    let (out, ok) = compile_and_run(
        r#"
async fn main() {
    let m = Mutex::new(0);
    let mut i = 0;
    while i < 5 {
        i = i + 1;
        lock m as mut value {
            value = value + 1;
            continue;
        }
    }
    lock m as value { println(value); }
}
"#,
    );
    assert!(ok, "continue out of a lock body must release: {out}");
    assert_eq!(out, "5\n");
}

/// Perspective 11: a propagated `?` is an exit edge with no `return` statement
/// of its own. The write made before it must still be committed, and the lock
/// must still be released — the second call hangs otherwise.
#[test]
fn lock_lower_11_try_propagate_from_body_releases() {
    let (out, ok) = compile_and_run(
        r#"
class Boom { pub code: i64; }
fn maybe(fail: bool) -> Result<i64, Boom> {
    if fail { return Result::Err(new Boom(9)); }
    return Result::Ok(5);
}
async fn bump(m: Mutex<i64>, fail: bool) -> Result<i64, Boom> {
    lock m as mut value {
        value = value + 1;
        let extra = maybe(fail)?;
        value = value + extra;
        return Result::Ok(value);
    }
}
async fn main() {
    let m = Mutex::new(0);
    match await bump(m, true) {
        Result::Ok(v) => println(v),
        Result::Err(e) => println(e.code),
    }
    match await bump(m, false) {
        Result::Ok(v) => println(v),
        Result::Err(e) => println(e.code),
    }
    lock m as value { println(value); }
}
"#,
    );
    assert!(ok, "`?` out of a lock body must release: {out}");
    // 9  — Err path, after `value = value + 1` committed 1
    // 7  — 1 + 1 + 5
    assert_eq!(out, "9\n7\n7\n");
}

/// Perspective 12: a panic that unwinds out of the body releases, and the
/// pre-panic write is still committed. The recovering task runs again on the
/// same mutex, so a leaked release would hang instead of printing.
#[test]
fn lock_lower_12_recovered_panic_from_body_releases() {
    let (out, ok) = compile_and_run(
        r#"
async fn risky(m: Mutex<i64>, boom: bool) {
    defer match recover() {
        Some(info) => println("recovered"),
        None => println("clean"),
    }
    lock m as mut value {
        value = value + 1;
        if boom {
            panic("inside the section");
        }
    }
}
async fn main() {
    let m = Mutex::new(0);
    await risky(m, true);
    await risky(m, false);
    lock m as value { println(value); }
}
"#,
    );
    assert!(ok, "a recovered panic must release the lock: {out}");
    assert_eq!(out, "recovered\nclean\n2\n");
}

/// Perspective 13: defer ordering. Defers declared INSIDE the body run LIFO
/// while ownership is still held, then the value is committed, then the lock is
/// released, then the enclosing scope's defers run. That ordering is what makes
/// "cleanup that must see the protected value" expressible.
#[test]
fn lock_lower_13_defer_ordering_around_the_section() {
    let (out, ok) = compile_and_run(
        r#"
async fn traced(m: Mutex<i64>) {
    defer println("5. outside the section");
    lock m as mut value {
        defer println("3. second defer, still holding");
        defer println("2. first defer, still holding");
        println("1. inside the section");
        value = value + 1;
    }
    println("4. released, write published");
}
async fn main() {
    await traced(Mutex::new(0));
}
"#,
    );
    assert!(ok, "{out}");
    assert_eq!(
        out,
        "1. inside the section\n\
         2. first defer, still holding\n\
         3. second defer, still holding\n\
         4. released, write published\n\
         5. outside the section\n"
    );
}

/// Perspective 14: a defer inside the body still runs — and the lock is still
/// released exactly once — when the body exits through a `return` rather than
/// falling through. Two exit paths must not both flush the same defer.
#[test]
fn lock_lower_14_defer_runs_once_on_the_return_edge() {
    let (out, ok) = compile_and_run(
        r#"
async fn traced(m: Mutex<i64>) -> i64 {
    lock m as mut value {
        defer println("cleanup");
        value = value + 1;
        return value;
    }
}
async fn main() {
    let m = Mutex::new(0);
    println(await traced(m));
    println(await traced(m));
}
"#,
    );
    assert!(ok, "{out}");
    assert_eq!(out, "cleanup\n1\ncleanup\n2\n");
}

/// Perspective 15: sequential critical sections in one function are fine — only
/// NESTING is refused (E2605). Two different mutexes, one after the other.
#[test]
fn lock_lower_15_sequential_sections_on_distinct_mutexes() {
    let (out, ok) = compile_and_run(
        r#"
async fn main() {
    let left = Mutex::new(1);
    let right = Mutex::new(2);
    lock left as mut a { a = a + 1; }
    lock right as mut b { b = b + 1; }
    lock left as a { println(a); }
    lock right as b { println(b); }
}
"#,
    );
    assert!(ok, "{out}");
    assert_eq!(out, "2\n3\n");
}

/// Perspective 16: re-acquiring the SAME mutex sequentially in one function is
/// also fine — the release from the first section is what makes the second
/// acquisition succeed rather than trip the recursive-acquisition guard.
#[test]
fn lock_lower_16_same_mutex_reacquired_sequentially() {
    let (out, ok) = compile_and_run(
        r#"
async fn main() {
    let m = Mutex::new(0);
    lock m as mut a { a = a + 1; }
    lock m as mut b { b = b + 1; }
    lock m as mut c { c = c + 1; }
    lock m as value { println(value); }
}
"#,
    );
    assert!(
        ok,
        "sequential re-acquisition must not self-deadlock: {out}"
    );
    assert_eq!(out, "3\n");
}

/// Perspective 17: a mutex passed across a task boundary is the SAME cell, not
/// a copy — otherwise each task would increment its own and the total would be
/// wrong even with perfect mutual exclusion.
#[test]
fn lock_lower_17_mutex_is_shared_across_tasks_not_copied() {
    let (out, ok) = compile_and_run(
        r#"
async fn once(m: Mutex<i64>) {
    lock m as mut value { value = value + 1; }
}
async fn main() {
    let m = Mutex::new(0);
    let a = once(m);
    let b = once(m);
    let c = once(m);
    await a;
    await b;
    await c;
    lock m as value { println(value); }
}
"#,
    );
    assert!(ok, "{out}");
    assert_eq!(out, "3\n");
}

/// Perspective 18: two independent mutexes do not interfere. A task holding one
/// must not exclude a task holding the other, and each total must be exact.
#[test]
fn lock_lower_18_independent_mutexes_do_not_interfere() {
    let (out, ok) = compile_and_run(
        r#"
async fn bump(m: Mutex<i64>, times: i64) {
    let mut i = 0;
    while i < times {
        lock m as mut value { value = value + 1; }
        i = i + 1;
    }
}
async fn main() {
    let a = Mutex::new(0);
    let b = Mutex::new(100);
    let t0 = bump(a, 200);
    let t1 = bump(b, 200);
    let t2 = bump(a, 200);
    let t3 = bump(b, 200);
    await t0;
    await t1;
    await t2;
    await t3;
    lock a as va { println(va); }
    lock b as vb { println(vb); }
}
"#,
    );
    assert!(ok, "{out}");
    assert_eq!(out, "400\n500\n");
}

/// Perspective 19: a GC-managed protected value. The cell is a GC root and the
/// commit goes through the write barrier, so a String published from inside a
/// section survives an explicit collection.
#[test]
fn lock_lower_19_gc_managed_value_survives_collection() {
    let (out, ok) = compile_and_run(GC_MANAGED_LOCK_SRC);
    assert!(ok, "{out}");
    assert_eq!(out, "willow\n");
}

/// Perspective 20: the same program under GC stress, where a collection can
/// land between the commit and the next acquisition. A missing root or a
/// missing barrier shows up here as a corrupted read or a crash.
#[test]
fn lock_lower_20_gc_managed_value_survives_gc_stress() {
    let (out, ok) = compile_and_run_with_env(GC_MANAGED_LOCK_SRC, &[("WILLOW_GC_STRESS", "1")]);
    assert!(ok, "GC stress must not lose the protected reference: {out}");
    assert_eq!(out, "willow\n");
}

/// Perspective 21: non-i64 protected types go through the same word-based ABI.
/// `bool` and `f64` must round-trip through the cell unchanged.
#[test]
fn lock_lower_21_bool_and_float_values_round_trip() {
    let (out, ok) = compile_and_run(
        r#"
async fn main() {
    let flag = Mutex::new(false);
    let ratio = Mutex::new(1.5);
    lock flag as mut f { f = true; }
    lock ratio as mut r { r = r + 0.25; }
    lock flag as f { println(f); }
    lock ratio as r { println(r); }
}
"#,
    );
    assert!(ok, "{out}");
    assert_eq!(out, "true\n1.75\n");
}

/// Perspective 22: an empty body is a valid section. Acquire then release with
/// nothing in between must not confuse the exactly-once cleanup.
#[test]
fn lock_lower_22_empty_body_acquires_and_releases() {
    let (out, ok) = compile_and_run(
        r#"
async fn main() {
    let m = Mutex::new(5);
    lock m as value { }
    lock m as mut value { }
    lock m as value { println(value); }
}
"#,
    );
    assert!(ok, "an empty section must still release: {out}");
    assert_eq!(out, "5\n");
}

/// Perspective 23: a section held across many statements still yields to the
/// preemption safepoint the backend plants before each one. Preemption re-polls
/// the SAME task, so ownership is retained across it; a task that lost the lock
/// to a preemption would lose updates here.
#[test]
fn lock_lower_23_long_body_survives_preemption() {
    let (out, ok) = compile_and_run_with_env(
        r#"
async fn work(m: Mutex<i64>) {
    lock m as mut value {
        let mut i = 0;
        while i < 2000 {
            value = value + 1;
            i = i + 1;
        }
    }
}
async fn main() {
    let m = Mutex::new(0);
    let a = work(m);
    let b = work(m);
    await a;
    await b;
    lock m as value { println(value); }
}
"#,
        &[("WILLOW_WORKERS", "4")],
    );
    assert!(ok, "{out}");
    assert_eq!(out, "4000\n");
}

/// Perspective 24: cancelling a task that is contending for the lock must not
/// wedge the mutex. The surviving tasks still finish and the final count is
/// exact for the work that was not cancelled.
#[test]
fn lock_lower_24_cancelled_waiter_does_not_wedge_the_mutex() {
    let (out, ok) = compile_and_run(
        r#"
async fn bump(m: Mutex<i64>, times: i64) {
    let mut i = 0;
    while i < times {
        lock m as mut value { value = value + 1; }
        i = i + 1;
    }
}
async fn main() {
    let m = Mutex::new(0);
    let doomed = bump(m, 500);
    let keep = bump(m, 100);
    doomed.cancel();
    await keep;
    lock m as value {
        println(value >= 100);
    }
    // The mutex is still usable after a waiter was cancelled. Cancellation is
    // observed at a suspension point, so a straggler increment from `doomed`
    // may still land between these sections — assert only what is monotone.
    lock m as mut value { value = value + 1000000; }
    lock m as value { println(value >= 1000100); }
}
"#,
    );
    assert!(ok, "a cancelled waiter must not wedge the mutex: {out}");
    assert_eq!(out, "true\ntrue\n");
}

/// Perspective 25: a mutex stored in a class field, reached through `self`, is
/// the same cell for every task holding that object.
#[test]
fn lock_lower_25_mutex_in_a_class_field() {
    let (out, ok) = compile_and_run(
        r#"
class Account {
    pub balance: Mutex<i64>;
    pub async fn deposit(self, amount: i64) {
        lock self.balance as mut value { value = value + amount; }
    }
    pub async fn read_balance(self) -> i64 {
        lock self.balance as value { return value; }
    }
}
async fn main() {
    let account = new Account(Mutex::new(100));
    let a = account.deposit(10);
    let b = account.deposit(20);
    await a;
    await b;
    println(await account.read_balance());
}
"#,
    );
    assert!(ok, "{out}");
    assert_eq!(out, "130\n");
}

/// Perspective 26: Stage 5 lowers both RwLock forms and publishes writes to
/// later shared readers.
#[test]
fn lock_lower_26_rwlock_read_and_write_forms_run() {
    let (out, ok) = compile_and_run(
        r#"
async fn main() {
    let r = RwLock::new(0);
    lock read r as value { println(value); }
    lock write r as mut value { value = 1; }
    lock read r as value { println(value); }
}
"#,
    );
    assert!(ok, "{out}");
    assert_eq!(out, "0\n1\n");
}

/// Perspective 27: `Mutex<T>` has no accessors after Stage 4. The diagnostic
/// must say WHY (a `get`/`set` pair loses updates) and name both replacements,
/// or the removal is just a breakage.
#[test]
fn lock_lower_27_mutex_accessors_are_gone_with_a_migration_help() {
    for method in ["get()", "set(1)"] {
        assert_compile_error_contains(
            &format!("async fn main() {{ let m = Mutex::new(0); m.{method}; }}\n"),
            &[
                "lock <mutex> as [mut] value",
                "BlockingCell<T>",
                "read-modify-write",
            ],
        );
    }
}
