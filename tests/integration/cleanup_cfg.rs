//! Native cleanup CFG execution under three actual backend block orders.
//! Each named fixture below is a separate explicit test perspective, adapted
//! from the established integration fixture named alongside it. Every order
//! must produce the same explicit stdout under allocation GC stress.
use super::support::TestProject;
use std::time::Duration;

fn check_orders(label: &str, source: &str, expected: &str) {
    for order in ["graph", "index", "reverse"] {
        let project = TestProject::new("cleanup_cfg", &[("main.wi", source)]);
        let compiled =
            project.compile_with_env("main.wi", &[("WILLOW_TEST_LIR_BLOCK_ORDER", order)]);
        assert!(
            compiled.status.success(),
            "{label} order={order} compile failed:\n{}",
            String::from_utf8_lossy(&compiled.stderr)
        );
        let output = project.run_with_env_timeout(
            &[
                ("WILLOW_GC_STRESS", "alloc"),
                ("WILLOW_WORKERS", "5"),
                ("WILLOW_TASK_BUDGET", "1"),
            ],
            Duration::from_secs(30),
        );
        assert!(
            output.status.success(),
            "{label} order={order} run failed:\n{}",
            String::from_utf8_lossy(&output.stderr)
        );
        assert_eq!(
            String::from_utf8_lossy(&output.stdout),
            expected,
            "{label} order={order}"
        );
    }
}

#[test]
fn cleanup_cfg_sync_emission_orders() {
    // lir_defer_block_order_01_a_merge_reached_only_from_the_else_arm
    check_orders(
        "lir_defer_block_order_01_a_merge_reached_only_from_the_else_arm",
        "fn f(flag: bool) -> i64 {
    defer println(\"done\");
    if flag { return 1; } else { println(\"else\"); }
    return 2;
}
fn main() { println(f(false)); println(f(true)); }
",
        "else\ndone\n2\ndone\n1\n",
    );
    // lir_defer_block_order_04_the_merge_is_what_leaves_the_scope
    check_orders(
        "lir_defer_block_order_04_the_merge_is_what_leaves_the_scope",
        "fn f(flag: bool) -> i64 {
    defer println(\"outer\");
    for i in 0..1 {
        defer println(\"body\");
        if flag { return 1; } else { println(\"else\"); }
    }
    return 2;
}
fn main() { println(f(false)); println(f(true)); }
",
        "else\nbody\nouter\n2\nbody\nouter\n1\n",
    );
    // lir_defer_block_order_05_two_registrations_still_unwind_lifo
    check_orders(
        "lir_defer_block_order_05_two_registrations_still_unwind_lifo",
        "fn f(flag: bool) -> i64 {
    defer println(\"a\");
    defer println(\"b\");
    if flag { return 1; } else { println(\"else\"); }
    return 2;
}
fn main() { println(f(false)); println(f(true)); }
",
        "else\nb\na\n2\nb\na\n1\n",
    );
    // lir_defer_block_order_09_continue_in_the_then_arm
    check_orders(
        "lir_defer_block_order_09_continue_in_the_then_arm",
        "fn f(limit: i64) -> i64 {
    defer println(\"fn\");
    let mut total = 0;
    for i in 0..limit {
        defer println(i);
        if i == 1 { continue; } else { total = total + i; }
    }
    return total;
}
fn main() { println(f(4)); }
",
        "0\n1\n2\n3\nfn\n5\n",
    );
    // lir_defer_block_order_10_break_in_the_then_arm
    check_orders(
        "lir_defer_block_order_10_break_in_the_then_arm",
        "fn f(limit: i64) -> i64 {
    defer println(\"fn\");
    let mut total = 0;
    for i in 0..limit {
        defer println(i * 10);
        if i == 2 { break; } else { total = total + i; }
    }
    return total;
}
fn main() { println(f(5)); }
",
        "0\n10\n20\nfn\n1\n",
    );
    // lir_defer_block_order_13_a_defer_registered_in_the_else_arm
    check_orders(
        "lir_defer_block_order_13_a_defer_registered_in_the_else_arm",
        "fn f(flag: bool) -> i64 {
    defer println(\"fn\");
    if flag { return 1; } else { defer println(\"arm\"); println(\"else\"); }
    return 2;
}
fn main() { println(f(false)); println(f(true)); }
",
        "else\narm\nfn\n2\nfn\n1\n",
    );
}

#[test]
fn cleanup_cfg_panic_emission_orders() {
    // panic_recover_02_lifo_continues_with_older_defers_after_recovery
    check_orders(
        "panic_recover_02_lifo_continues_with_older_defers_after_recovery",
        r#"
fn main() {
    if true {
        defer println("oldest");
        defer match recover() {
            Some(_) => println("recovered"),
            None => {}
        }
        defer println("newest");
        panic("boom");
    }
    println("after");
}
"#,
        "newest\nrecovered\noldest\nafter\n",
    );
    // panic_recover_04_same_panic_is_consumed_once
    check_orders(
        "panic_recover_04_same_panic_is_consumed_once",
        r#"
fn main() {
    if true {
        defer {
            match recover() {
                Some(info) => println(info.message),
                None => println("first-none")
            }
            match recover() {
                Some(_) => println("twice"),
                None => println("second-none")
            }
        }
        panic("once");
    }
    println("after");
}
"#,
        "once\nsecond-none\nafter\n",
    );
    // panic_recover_12_nested_panic_abandons_current_defer_without_rerun
    check_orders(
        "panic_recover_12_nested_panic_abandons_current_defer_without_rerun",
        r#"
fn main() {
    if true {
        defer match recover() {
            Some(info) => println("outer:" + info.message),
            None => {}
        }
        if true {
            defer match recover() {
                Some(info) => println("inner:" + info.message),
                None => {}
            }
            defer { panic("B"); }
            panic("A");
        }
    }
    println("after");
}
"#,
        "inner:B\nouter:A\nafter\n",
    );
    // panic_recover_16_later_unregistered_defer_is_not_run
    check_orders(
        "panic_recover_16_later_unregistered_defer_is_not_run",
        r#"
fn main() {
    if true {
        defer match recover() {
            Some(_) => println("recovered"),
            None => {}
        }
        if true { panic("boom"); }
        defer println("registered-too-late");
    }
    println("after");
}
"#,
        "recovered\nafter\n",
    );
}

#[test]
fn cleanup_cfg_async_emission_orders() {
    // Registration both before and after a completed suspension, then LIFO return.
    check_orders(
        "await_before_and_after_registration",
        "fn c(n: i64) { println(n); }\nasync fn w() -> i64 { await sleep(1); defer c(1); await sleep(1); defer c(2); await sleep(1); return 7; }\nasync fn main() { println(await w()); }",
        "2\n1\n7\n",
    );

    // adfr_03_return_position_await_flushes
    check_orders(
        "adfr_03_return_position_await_flushes",
        "fn c() { println(5); }\nasync fn inner() -> i64 { await sleep(1); return 4; }\nasync fn w() -> i64 { defer c(); return await inner(); }\nasync fn main() { println(await w()); }",
        "5\n4\n",
    );
    // adfr_05_cancel_only_registered_sites
    check_orders(
        "adfr_05_cancel_only_registered_sites",
        "fn c(n: i64) { println(n); }\nasync fn w(ready: AtomicBool) { defer c(10); ready.store(true); await sleep(5000); defer c(20); await sleep(1); }\nasync fn main() { let ready = AtomicBool::new(false); let h = w(ready); while !ready.load() { await yield(); } h.cancel(); await h.result(); println(0); }",
        "10\n0\n",
    );
    // adfr_06_cancel_before_registration
    check_orders(
        "adfr_06_cancel_before_registration",
        "fn c() { println(99); }\nasync fn w(ready: AtomicBool, proceed: AtomicBool) { ready.store(true); while !proceed.load() { await yield(); } defer c(); }\nasync fn main() { let ready = AtomicBool::new(false); let proceed = AtomicBool::new(false); let h = w(ready, proceed); while !ready.load() { await yield(); } h.cancel(); await h.result(); println(0); }",
        "0\n",
    );
    // adfr_11_string_operand_gc_stress_cancel
    check_orders(
        "adfr_11_string_operand_gc_stress_cancel",
        "fn c(s: String) { println(s); }\nasync fn w(ready: AtomicBool) { let name = \"a\" + \"b\"; defer c(name + \"!\"); ready.store(true); await sleep(5000); }\nasync fn main() { let ready = AtomicBool::new(false); let h = w(ready); while !ready.load() { await yield(); } h.cancel(); await h.result(); println(\"end\"); }",
        "ab!\nend\n",
    );
    // adfr_30_return_unwinds_nested_scopes
    check_orders(
        "adfr_30_return_unwinds_nested_scopes",
        "async fn w() -> i64 { defer println(9); if true { defer println(1); await sleep(1); return 7; } return 0; }\nasync fn main() { println(await w()); }",
        "1\n9\n7\n",
    );
    // adfr_31_nested_await_return_is_fixed_before_defer
    check_orders(
        "adfr_31_nested_await_return_is_fixed_before_defer",
        "async fn one() -> i64 { return 1; }\nasync fn w() -> i64 { let mut x = 1; defer { x = 9; println(x); } return (await one()) + x; }\nasync fn main() { println(await w()); }",
        "9\n2\n",
    );
}

#[test]
fn cleanup_cfg_cancel_during_sync_panic_defer_emission_orders() {
    // Combine the established AtomicBool cancellation handshake with a native
    // synchronous callee: cancellation must consume runtime registration flags
    // even while the panic cleanup graph contains every lexical defer site.
    check_orders(
        "cancel_during_sync_panic_defer_skips_consumed_and_future_sites",
        r#"
fn victim(entered: AtomicBool, released: AtomicBool, finished: AtomicBool) {
    defer { println("older"); finished.store(true); }
    defer {
        println("current");
        match recover() {
            Some(_) => { },
            None => { }
        }
        entered.store(true);
        while !released.load() { }
    }
    if true { panic("begin cleanup"); }
    defer println("future");
}
async fn worker(entered: AtomicBool, released: AtomicBool, finished: AtomicBool) {
    victim(entered, released, finished);
}
async fn main() {
    let entered = AtomicBool::new(false);
    let released = AtomicBool::new(false);
    let finished = AtomicBool::new(false);
    let task = worker(entered, released, finished);
    while !entered.load() { await yield(); }
    task.cancel();
    while !finished.load() { await yield(); }
    released.store(true);
    await task.result();
    println(task.is_cancelled());
}
"#,
        "current\nolder\ntrue\n",
    );
}

#[test]
fn cleanup_cfg_lock_emission_orders() {
    // lir_locks_03_return_out_of_the_section_commits_and_releases
    check_orders(
        "lir_locks_03_return_out_of_the_section_commits_and_releases",
        r#"
async fn take(m: Mutex<i64>, amount: i64) -> bool {
    lock m as mut value {
        if value < amount {
            return false;
        }
        value = value - amount;
        return true;
    }
}
async fn main() {
    let m = Mutex::new(10);
    println(await take(m, 3));
    println(await take(m, 100));
    lock m as value { println(value); }
}
"#,
        "true\nfalse\n7\n",
    );
    // lir_locks_07_defers_run_inside_then_the_lock_goes_back
    check_orders(
        "lir_locks_07_defers_run_inside_then_the_lock_goes_back",
        r#"
async fn traced(m: Mutex<i64>) {
    defer println("4. outside the section");
    lock m as mut value {
        defer println("2. still holding the lock");
        println("1. inside the section");
        value = value + 1;
    }
    println("3. lock released");
}
async fn main() {
    let m = Mutex::new(0);
    await traced(m);
    lock m as value { println(value); }
}
"#,
        "1. inside the section\n2. still holding the lock\n3. lock released\n\
         4. outside the section\n1\n",
    );
    // lir_locks_08_panic_recovered_outside_the_section_releases
    check_orders(
        "lir_locks_08_panic_recovered_outside_the_section_releases",
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
        "recovered\nclean\n2\n",
    );
    // lir_locks_12_rwlock_write_section_commits
    check_orders(
        "lir_locks_12_rwlock_write_section_commits",
        r#"
async fn publish(l: RwLock<i64>, next: i64) {
    lock write l as mut value {
        value = value + next;
    }
}
async fn main() {
    let l = RwLock::new(1);
    await publish(l, 41);
    lock read l as value { println(value); }
}
"#,
        "42\n",
    );
    // lir_locks_17_gc_managed_protected_value_survives_collection
    check_orders(
        "lir_locks_17_gc_managed_protected_value_survives_collection",
        r#"
async fn rename(l: RwLock<String>, name: String) {
    lock write l as mut value {
        value = name + "!";
    }
}
async fn main() {
    let l = RwLock::new("draft");
    await rename(l, "published");
    gc_collect();
    lock read l as value { println(value); }
}
"#,
        "published!\n",
    );
    // lir_locks_20_cancelled_holder_commits_then_releases_then_defers
    check_orders(
        "lir_locks_20_cancelled_holder_commits_then_releases_then_defers",
        r#"
async fn victim(m: Mutex<i64>, entered: AtomicBool) {
    defer println("outer");
    lock m as mut value {
        defer {
            value = value + 10;
            println("inner");
        }
        value = 1;
        entered.store(true);
        while true {
            value = value;
        }
    }
}
async fn contender(m: Mutex<i64>) {
    lock m as mut value {
        value = value + 1;
    }
}
async fn main() {
    let m = Mutex::new(0);
    let victim_entered = AtomicBool::new(false);
    let stopped = victim(m, victim_entered);
    while !victim_entered.load() {
        await yield();
    }
    let next = contender(m);
    stopped.cancel();
    match await stopped.result() {
        Ok(_) => println("unexpected completion"),
        Err(Cancelled) => { }
    }
    await next;
    lock m as value { println(value); }
}
"#,
        "inner\nouter\n12\n",
    );
}
