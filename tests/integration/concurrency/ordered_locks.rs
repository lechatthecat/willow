use super::*;

fn assert_ordered_run(source: &str, expected: &str) {
    let (out, ok, timed_out) =
        compile_and_run_with_env_timeout(source, &[], std::time::Duration::from_secs(15));
    assert!(!timed_out, "ordered acquisition or cleanup hung: {out}");
    assert!(ok, "{out}");
    assert_eq!(out, expected);
}

#[test]
fn ordered_locks_same_class_parameters_preserve_source_bindings() {
    assert_ordered_run(
        r#"
async fn combine(a: Mutex<i64>, b: Mutex<i64>) -> i64 {
    lock b as mut right, a as mut left {
        right = right + 2;
        left = left + 1;
        return left * 100 + right;
    }
}
async fn main() {
    let a = Mutex::new(10);
    let b = Mutex::new(20);
    println(await combine(a, b));
    lock a as left, b as right { println(left * 100 + right); }
}
"#,
        "1122\n1122\n",
    );
}

#[test]
fn ordered_locks_heterogeneous_mutex_and_rwlock_modes() {
    assert_ordered_run(
        r#"
async fn main() {
    let number = Mutex::new(10);
    let flag = Mutex::new(false);
    let source = RwLock::new(4);
    let destination = RwLock::new(0);
    lock write destination as mut out, flag as mut enabled,
         read source as input, number as mut value {
        enabled = true;
        value = value + input;
        out = value;
    }
    lock number as value, flag as enabled, read destination as out {
        println(value);
        println(enabled);
        println(out);
    }
}
"#,
        "14\ntrue\n14\n",
    );
}

#[test]
fn ordered_locks_targets_run_once_left_to_right_before_bindings() {
    assert_ordered_run(
        r#"
fn pick(m: Mutex<i64>, calls: BlockingCell<i64>, digit: i64) -> Mutex<i64> {
    calls.set(calls.get() * 10 + digit);
    return m;
}
async fn main() {
    let a = Mutex::new(10);
    let b = Mutex::new(20);
    let calls = BlockingCell::new(0);
    lock pick(b, calls, 2) as a, pick(a, calls, 1) as b {
        println(a);
        println(b);
    }
    println(calls.get());
}
"#,
        "20\n10\n21\n",
    );
}

#[test]
fn ordered_locks_break_and_continue_commit_all_bindings() {
    assert_ordered_run(
        r#"
async fn main() {
    let a = Mutex::new(0);
    let b = Mutex::new(0);
    let mut n = 0;
    while n < 3 {
        n = n + 1;
        lock b as mut y, a as mut x {
            x = x + 1;
            y = y + 10;
            if n < 3 { continue; }
            break;
        }
    }
    lock a as x, b as y { println(x); println(y); }
}
"#,
        "3\n30\n",
    );
}

#[test]
fn ordered_locks_recovered_panic_commits_and_releases_all() {
    assert_ordered_run(
        r#"
async fn risky(a: Mutex<i64>, b: Mutex<i64>, fail: bool) {
    defer match recover() {
        Some(_) => println("recovered"),
        None => { }
    }
    lock b as mut y, a as mut x {
        x = x + 1;
        y = y + 10;
        if fail { panic("group body"); }
    }
}
async fn main() {
    let a = Mutex::new(0);
    let b = Mutex::new(0);
    await risky(a, b, true);
    await risky(a, b, false);
    lock a as x, b as y { println(x); println(y); }
}
"#,
        "recovered\n2\n20\n",
    );
}

#[test]
fn ordered_locks_distinct_nested_acquisitions_commit_on_return() {
    assert_ordered_run(
        r#"
async fn nested() -> i64 {
    let a = Mutex::new(10);
    let b = Mutex::new(20);
    lock a as x {
        lock b as y { return x + y; }
    }
}
async fn main() { println(await nested()); }
"#,
        "30\n",
    );
}

#[test]
fn ordered_locks_static_alias_is_rejected() {
    assert_compile_error_contains(
        r#"
async fn main() {
    let a = Mutex::new(1);
    let alias = a;
    lock a as x, alias as y { println(x + y); }
}
"#,
        &["E2605"],
    );
}

#[test]
fn ordered_locks_unknown_duplicate_recovers_without_leaking_prefix() {
    assert_ordered_run(
        r#"
async fn duplicate(prefix: Mutex<i64>, a: Mutex<i64>, b: Mutex<i64>) {
    defer match recover() {
        Some(_) => println("duplicate rejected"),
        None => println("unexpected success"),
    }
    lock prefix as p, a as x, b as y { println("unexpected entry"); }
}
async fn main() {
    let prefix = Mutex::new(1);
    let a = Mutex::new(2);
    await duplicate(prefix, a, a);
    lock prefix as p, a as x { println(p + x); }
}
"#,
        "duplicate rejected\n3\n",
    );
}

#[test]
fn ordered_locks_wait_in_group_remains_rejected() {
    assert_compile_error_contains(
        r#"
async fn main() {
    let a = Mutex::new(1);
    let b = Mutex::new(2);
    lock a as x, b as y { await sleep(1); }
}
"#,
        &["E2604"],
    );
}

#[test]
fn ordered_locks_wait_in_nested_section_remains_rejected() {
    assert_compile_error_contains(
        r#"
async fn main() {
    let a = Mutex::new(1);
    let b = Mutex::new(2);
    lock a as x { lock b as y { await yield(); } }
}
"#,
        &["E2604"],
    );
}

#[test]
fn ordered_locks_held_cancellation_commits_before_outer_cleanup() {
    let (out, ok, timed_out) = compile_and_run_with_env_timeout(
        r#"
async fn victim(a: Mutex<i64>, b: Mutex<i64>, entered: AtomicBool,
                next_entered: AtomicBool) {
    defer {
        while !next_entered.load() { }
        println("outer");
    }
    lock b as mut y, a as mut x {
        defer { x = x + 10; y = y + 20; println("inner"); }
        x = 1;
        y = 2;
        entered.store(true);
        while true { x = x; }
    }
}
async fn contender(a: Mutex<i64>, b: Mutex<i64>, entered: AtomicBool) {
    lock a as x, b as y { entered.store(true); }
}
async fn main() {
    let a = Mutex::new(0);
    let b = Mutex::new(0);
    let entered = AtomicBool::new(false);
    let next_entered = AtomicBool::new(false);
    let stopped = victim(a, b, entered, next_entered);
    while !entered.load() { await yield(); }
    let next = contender(a, b, next_entered);
    stopped.cancel();
    match await stopped.result() {
        Ok(_) => println("unexpected completion"),
        Err(Cancelled) => { }
    }
    await next;
    lock a as x, b as y { println(x); println(y); }
}
"#,
        &[("WILLOW_TASK_BUDGET", "1"), ("WILLOW_WORKERS", "5")],
        std::time::Duration::from_secs(15),
    );
    assert!(!timed_out, "group cancellation cleanup hung: {out}");
    assert!(ok, "{out}");
    assert_eq!(out, "inner\nouter\n11\n22\n");
}

#[test]
fn ordered_locks_calls_under_locks_do_not_need_a_lock_proof() {
    assert_ordered_run(
        r#"
interface Step extends Sync { fn step(self, n: i64) -> i64; }
class Add implements Step {
    pub fn step(self, n: i64) -> i64 { return n + 1; }
}
fn helper(n: i64) -> i64 { return n * 2; }
fn twice(n: i64) -> i64 { return n * 2; }
async fn update(m: Mutex<i64>, flag: AtomicBool, s: Step, f: fn(i64) -> i64) {
    lock m as mut x {
        flag.store(true);
        x = f(s.step(helper(x)));
    }
}
async fn main() {
    let text = Mutex::new("abc");
    let number = Mutex::new(1);
    let flag = AtomicBool::new(false);
    await update(number, flag, new Add(), twice);
    lock text as t, number as n {
        println(t.len());
        println(n);
    }
    println(flag.load());
}
"#,
        "3\n6\ntrue\n",
    );
}

#[test]
fn ordered_locks_panic_commits_live_gc_member_value() {
    assert_ordered_run(
        r#"
async fn risky(a: Mutex<i64>, b: Mutex<String>, fail: bool) {
    defer match recover() {
        Some(_) => println("recovered"),
        None => { }
    }
    lock a as x, b as mut y {
        println(y);
        if fail { panic("group body"); }
        y = "new";
    }
}
async fn main() {
    let a = Mutex::new(1);
    let b = Mutex::new("old");
    await risky(a, b, true);
    lock b as y { println(y); }
    await risky(a, b, false);
    lock b as y { println(y); }
}
"#,
        "old\nrecovered\nold\nold\nnew\n",
    );
}
