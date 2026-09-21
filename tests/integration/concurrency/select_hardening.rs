use super::*;

// ---------------------------------------------------------------------------
// select hardening (willow-o038 review). Perspectives covered here:
//   1. `t.join(x)` inside select is an arity error, not a silently dropped arg
//   2. same for the `let v = t.join(x)` form
//   3. `ch.recv(x)` inside select is an error
//   4. same for the `let v = ch.recv(x)` form
//   5. the argument's side effects are not silently skipped (the program is
//      rejected instead of running with the call erased)
//   6. argument-free `recv()` and task-await forms still parse
//   7. a near timeout beats a far-off task instead of waiting for it
//   8. a task that finishes first still beats a far-off timeout
//   9. the NEAREST of several timeouts fires
//  10. an already-ready channel beats a timeout without waiting
//  11. a timeout fires when the scheduler has no other work at all
//  12. a timeout still fires while an unrelated task keeps making progress
//  13. a select send value survives a GC that runs during the probe loop
//  14. a select recv binding survives a collection inside the case body
//  15. a select task-await binding survives collection inside the case body
//  16. select-awaiting a TEMPORARY task handle keeps its frame alive
//  17. the cooperative task-await binding survives allocation pressure after
//      store (write barrier on the async-frame slot)
//  18. send/recv/task-await bindings of reference type round-trip their values
//  19. everything above holds under GC stress at every safepoint
//  20. cancelling a task that a select is awaiting does not strand the waiter
// ---------------------------------------------------------------------------

#[test]
fn sel_arg_01_join_with_argument_is_rejected() {
    assert_compile_error_contains(
        r#"
async fn work() -> i64 {
    return 1;
}

fn side() -> i64 {
    println(99);
    return 2;
}

fn main() {
    let t = work();
    select {
        t.join(side()) => { println(0); }
        default => { println(1); }
    }
}
"#,
        &["error[E0103]", "select case must be"],
    );
}

#[test]
fn sel_arg_02_let_join_with_argument_is_rejected() {
    assert_compile_error_contains(
        r#"
async fn work() -> i64 {
    return 1;
}

fn main() {
    let t = work();
    select {
        let v = t.join(7) => { println(v); }
        default => { println(1); }
    }
}
"#,
        &["error[E0103]", "select `let` case must bind"],
    );
}

#[test]
fn sel_arg_03_recv_with_argument_is_rejected() {
    assert_compile_error_contains(
        r#"
fn main() {
    let ch = Channel<i64>::new();
    select {
        ch.recv(1) => { println(0); }
        default => { println(1); }
    }
}
"#,
        &["error[E0103]", "select case must be"],
    );
}

#[test]
fn sel_arg_04_let_recv_with_argument_is_rejected() {
    assert_compile_error_contains(
        r#"
fn main() {
    let ch = Channel<i64>::new();
    select {
        let v = ch.recv(1) => { println(v); }
        default => { println(1); }
    }
}
"#,
        &["error[E0103]", "select `let` case must bind"],
    );
}

#[test]
fn sel_arg_05_argument_free_forms_still_parse() {
    let (out, ok) = compile_and_run(
        r#"
async fn work() -> i64 {
    return 11;
}

async fn main() {
    let ch = Channel<i64>::new();
    ch.send(5);
    select {
        let v = ch.recv() => { println(v); }
        default => { println(0); }
    }
    let t = work();
    select {
        let v = await t => { println(v); }
        sleep(5000) => { println(0); }
    }
}
"#,
    );
    assert!(ok, "{out}");
    assert_eq!(out, "5\n11\n");
}

#[test]
fn sel_to_01_near_timeout_beats_far_off_task() {
    // The drive inside a sync select must be bounded by the nearest deadline.
    // An unbounded `willow_sched_run` runs the 800ms task to completion first,
    // so the task-await arm would win a 30ms timeout.
    let (out, ok) = compile_and_run(
        r#"
async fn slow() -> i64 {
    await sleep(800);
    return 1;
}

async fn main() {
    let t = slow();
    select {
        let v = await t => { println(v); }
        sleep(30) => { println("timeout"); }
    }
}
"#,
    );
    assert!(ok, "{out}");
    assert_eq!(out, "timeout\n");
}

#[test]
fn sel_to_02_task_that_finishes_first_beats_the_timeout() {
    let (out, ok) = compile_and_run(
        r#"
async fn quick() -> i64 {
    await sleep(5);
    return 42;
}

async fn main() {
    let t = quick();
    select {
        let v = await t => { println(v); }
        sleep(5000) => { println("timeout"); }
    }
}
"#,
    );
    assert!(ok, "{out}");
    assert_eq!(out, "42\n");
}

#[test]
fn sel_to_03_nearest_of_several_timeouts_fires() {
    let (out, ok) = compile_and_run(
        r#"
async fn slow() -> i64 {
    await sleep(800);
    return 1;
}

async fn main() {
    let t = slow();
    select {
        let v = await t => { println(v); }
        sleep(3000) => { println("late"); }
        sleep(30) => { println("early"); }
    }
}
"#,
    );
    assert!(ok, "{out}");
    assert_eq!(out, "early\n");
}

#[test]
fn sel_to_04_ready_channel_beats_a_timeout_without_waiting() {
    let (out, ok) = compile_and_run(
        r#"
fn main() {
    let ch = Channel<i64>::new();
    ch.send(9);
    select {
        let v = ch.recv() => { println(v); }
        sleep(5000) => { println("timeout"); }
    }
}
"#,
    );
    assert!(ok, "{out}");
    assert_eq!(out, "9\n");
}

#[test]
fn sel_to_05_timeout_fires_with_no_other_work() {
    // Nothing else is runnable: the deadline placeholder is the only timer,
    // so the idle wait must block on it and return.
    let (out, ok) = compile_and_run(
        r#"
fn main() {
    let ch = Channel<i64>::new();
    select {
        let v = ch.recv() => { println(v); }
        sleep(20) => { println("timeout"); }
    }
}
"#,
    );
    assert!(ok, "{out}");
    assert_eq!(out, "timeout\n");
}

#[test]
fn sel_to_06_timeout_fires_while_other_tasks_keep_progressing() {
    // A task that keeps waking (short sleeps) makes every drive "progress", so
    // the loop re-probes repeatedly; the deadline must still win.
    let (out, ok) = compile_and_run(
        r#"
async fn chatty() -> i64 {
    let mut i = 0;
    while i < 200 {
        await sleep(3);
        i = i + 1;
    }
    return i;
}

async fn main() {
    let t = chatty();
    select {
        let v = await t => { println(v); }
        sleep(40) => { println("timeout"); }
    }
}
"#,
    );
    assert!(ok, "{out}");
    assert_eq!(out, "timeout\n");
}

#[test]
fn sel_gc_01_send_value_survives_collection_during_the_probe_loop() {
    // The send value is evaluated ONCE at select entry and then held across
    // every probe iteration — including the scheduler drives that collect.
    let (out, ok) = compile_and_run_gc_stress(
        r#"
async fn drainer(ch: Channel<String>) -> String {
    await sleep(10);
    return ch.recv();
}

async fn main() {
    let ch = Channel<String>::with_capacity(1);
    ch.send("first");
    let d = drainer(ch);
    select {
        ch.send("second" + "!") => { println("sent"); }
    }
    println(await d);
    println(ch.recv());
}
"#,
    );
    assert!(ok, "{out}");
    assert_eq!(out, "sent\nfirst\nsecond!\n");
}

#[test]
fn sel_gc_02_recv_binding_survives_collection_in_the_body() {
    // The received string has just left the channel: the case binding is its
    // only reference while the body runs.
    let (out, ok) = compile_and_run(
        r#"
fn main() {
    let ch = Channel<String>::new();
    ch.send("hello" + "-world");
    select {
        let v = ch.recv() => {
            gc_collect();
            println(v);
        }
        default => { println("empty"); }
    }
}
"#,
    );
    assert!(ok, "{out}");
    assert_eq!(out, "hello-world\n");
}

#[test]
fn sel_gc_03_task_await_binding_survives_collection_in_the_body() {
    let (out, ok) = compile_and_run(
        r#"
async fn work() -> String {
    return "res" + "ult";
}

async fn main() {
    let t = work();
    select {
        let v = await t => {
            gc_collect();
            println(v);
        }
        sleep(5000) => { println("timeout"); }
    }
}
"#,
    );
    assert!(ok, "{out}");
    assert_eq!(out, "result\n");
}

#[test]
fn sel_gc_04_temporary_task_handle_stays_alive_across_the_probe_loop() {
    // The handle is never bound to a local, so the select's own stash is the
    // only thing keeping the async frame (and its result) reachable.
    let (out, ok) = compile_and_run_gc_stress(
        r#"
async fn slow() -> String {
    await sleep(15);
    return "late" + "-value";
}

async fn main() {
    select {
        let v = await slow() => { println(v); }
        sleep(5000) => { println("timeout"); }
    }
}
"#,
    );
    assert!(ok, "{out}");
    assert_eq!(out, "late-value\n");
}

#[test]
fn sel_gc_05_cooperative_await_binding_survives_allocation_pressure() {
    // Cooperative select stores the awaited result INTO the async frame: an old
    // frame pointing at a young string needs the write barrier, or a young
    // collection reclaims the result before it is printed.
    let (out, ok) = compile_and_run_gc_stress_all(
        r#"
async fn work() -> String {
    await sleep(1);
    return "awaited" + "-ok";
}

async fn main() {
    let t = work();
    select {
        let v = await t => {
            let mut i = 0;
            while i < 200 {
                let junk = "x" + "y";
                i = i + 1;
            }
            gc_collect();
            println(v);
        }
        sleep(5000) => { println("timeout"); }
    }
}
"#,
    );
    assert!(ok, "{out}");
    assert_eq!(out, "awaited-ok\n");
}

#[test]
fn sel_gc_06_reference_bindings_round_trip_under_gc_stress() {
    let (out, ok) = compile_and_run_gc_stress_all(
        r#"
async fn producer(ch: Channel<String>) -> String {
    await sleep(2);
    ch.send("payload" + "-1");
    return "produced";
}

async fn main() {
    let ch = Channel<String>::with_capacity(2);
    let p = producer(ch);
    select {
        let v = ch.recv() => { println(v); }
        sleep(5000) => { println("timeout"); }
    }
    println(await p);
}
"#,
    );
    assert!(ok, "{out}");
    assert_eq!(out, "payload-1\nproduced\n");
}

#[test]
fn sel_cancel_01_cancelled_task_waiter_does_not_strand_the_awaitee() {
    // A cancelled task that was registered as a select task waiter must be
    // removed from its awaitee's waiter list; the awaitee still completes and
    // its work is still observed.
    let (out, ok) = compile_and_run(
        r#"
async fn target(ch: Channel<i64>) -> i64 {
    await sleep(60);
    ch.send(7);
    return 7;
}

async fn watcher(ch: Channel<i64>) -> i64 {
    let t = target(ch);
    select {
        let v = await t => { return v; }
        sleep(5000) => { return 0 - 1; }
    }
}

async fn main() {
    let ch = Channel<i64>::new();
    let w = watcher(ch);
    await sleep(10);
    w.cancel();
    println(ch.recv());
}
"#,
    );
    assert!(ok, "{out}");
    assert_eq!(out, "7\n");
}

// ---------------------------------------------------------------------------
// Follow-up review of the bounded-channel work: (a) a sync `select` stashed its
// channel operand without rooting it, (b) a sync `send`/`recv` passed channel
// and value to a runtime call that drives the scheduler (and therefore
// allocates) with neither rooted, (c) the select case forms and the builtin
// static constructors dropped `CallArgMode`, silently accepting `&arg`.
//
// Perspectives covered below:
//   1. select `ch.send(&v)` is E1703, not a silent value send
//   2. select `sleep(&ms)` is E1703
//   3. `Channel::with_capacity(&n)` is E1703
//   4. `Mutex::new(&v)` is E1703
//   5. `RwLock::new(&v)` is E1703
//   6. `AtomicI64::new(&n)` is E1703
//   7. `AtomicBool::new(&b)` is E1703
//   8. the same forms without `&` still compile and run
//   9. `&` on a user function called INSIDE a select send value is still legal
//  10. a temporary (factory-returned) channel survives a sync select probe loop
//  11. ... and still delivers the value a task sends into it
//  12. a temporary channel in a SEND case survives the probe loop
//  13. two temporary channels in one select both stay live
//  14. the timeout exit path leaves the root stack balanced
//  15. the default exit path leaves the root stack balanced
//  16. `return` out of a select case body leaves the root stack balanced
//  17. repeated selects over temporaries do not leak roots
//  18. a sync send of a COMPUTED string into a full bounded channel keeps the
//      value alive while the runtime parks and drives the scheduler
//  19. ... and the channel itself, when it is a temporary
//  20. a sync recv that blocks on an empty temporary channel keeps it alive
//  21. an i64 (non-GC) element still round-trips through the same path
//  22. a class instance round-trips through a full bounded channel
//  23. a long sync send/recv loop under GC stress stays correct

#[test]
fn selref_01_select_send_reference_argument_is_rejected() {
    assert_compile_error_contains(
        r#"
fn main() {
    let ch = Channel<i64>::new();
    let v = 1;
    select {
        ch.send(&v) => { println(0); }
        default => { println(1); }
    }
}
"#,
        &["E1703", "unexpected reference argument"],
    );
}

#[test]
fn selref_02_select_sleep_reference_argument_is_rejected() {
    assert_compile_error_contains(
        r#"
fn main() {
    let ch = Channel<i64>::new();
    let ms = 5;
    select {
        let v = ch.recv() => { println(v); }
        sleep(&ms) => { println(1); }
    }
}
"#,
        &["E1703", "unexpected reference argument"],
    );
}

#[test]
fn selref_03_channel_with_capacity_reference_argument_is_rejected() {
    assert_compile_error_contains(
        r#"
fn main() {
    let n = 2;
    let ch = Channel<i64>::with_capacity(&n);
    ch.close();
}
"#,
        &["E1703", "unexpected reference argument"],
    );
}

#[test]
fn selref_04_mutex_new_reference_argument_is_rejected() {
    assert_compile_error_contains(
        r#"
fn main() {
    let v = 1;
    let m = Mutex<i64>::new(&v);
    println(m.get());
}
"#,
        &["E1703", "unexpected reference argument"],
    );
}

#[test]
fn selref_05_rwlock_new_reference_argument_is_rejected() {
    assert_compile_error_contains(
        r#"
fn main() {
    let v = 1;
    let l = RwLock<i64>::new(&v);
}
"#,
        &["E1703", "unexpected reference argument"],
    );
}

#[test]
fn selref_06_atomic_i64_new_reference_argument_is_rejected() {
    assert_compile_error_contains(
        r#"
fn main() {
    let v = 1;
    let a = AtomicI64::new(&v);
    println(a.load());
}
"#,
        &["E1703", "unexpected reference argument"],
    );
}

#[test]
fn selref_07_atomic_bool_new_reference_argument_is_rejected() {
    assert_compile_error_contains(
        r#"
fn main() {
    let v = true;
    let a = AtomicBool::new(&v);
    println(a.load());
}
"#,
        &["E1703", "unexpected reference argument"],
    );
}

#[test]
fn selref_08_value_forms_still_compile_and_run() {
    let (out, ok) = compile_and_run(
        r#"
fn main() {
    let ch = Channel<i64>::with_capacity(2);
    let v = 7;
    let ms = 5;
    select {
        ch.send(v) => { println("sent"); }
        sleep(ms) => { println("timeout"); }
    }
    println(ch.recv());
}
"#,
    );
    assert!(ok, "{out}");
    assert_eq!(out, "sent\n7\n");
}

#[test]
fn selref_09_reference_argument_to_a_user_call_inside_a_send_value_is_allowed() {
    // Only the select case's OWN argument is by value; a nested call keeps its
    // normal parameter modes.
    let (out, ok) = compile_and_run(
        r#"
fn twice(n: &i64) -> i64 {
    return n * 2;
}

fn main() {
    let ch = Channel<i64>::with_capacity(1);
    let base = 21;
    select {
        ch.send(twice(&base)) => { println("sent"); }
        default => { println("full"); }
    }
    println(ch.recv());
}
"#,
    );
    assert!(ok, "{out}");
    assert_eq!(out, "sent\n42\n");
}

#[test]
fn selroot_01_temporary_channel_survives_the_sync_select_probe_loop() {
    // The channel is never bound to a local: the select's stash is its only
    // reference while the probe loop drives the scheduler (which allocates).
    let (out, ok) = compile_and_run_gc_stress_all(
        r#"
fn make() -> Channel<i64> {
    return Channel<i64>::new();
}

fn main() {
    select {
        let v = make().recv() => { println(v); }
        sleep(30) => { println("timeout"); }
    }
}
"#,
    );
    assert!(ok, "{out}");
    assert_eq!(out, "timeout\n");
}

#[test]
fn selroot_02_temporary_channel_still_delivers_its_value() {
    // The factory keeps the channel reachable through the feeder task too, but
    // the select must probe the SAME object it stashed for the value to arrive.
    let (out, ok) = compile_and_run_gc_stress_all(
        r#"
async fn feed(ch: Channel<String>) -> i64 {
    await sleep(5);
    ch.send("fed" + "-value");
    return 0;
}

fn make() -> Channel<String> {
    let ch = Channel<String>::with_capacity(2);
    feed(ch);
    return ch;
}

fn main() {
    select {
        let v = make().recv() => { println(v); }
        sleep(5000) => { println("timeout"); }
    }
}
"#,
    );
    assert!(ok, "{out}");
    assert_eq!(out, "fed-value\n");
}

#[test]
fn selroot_03_temporary_channel_in_a_send_case_survives() {
    let (out, ok) = compile_and_run_gc_stress_all(
        r#"
fn make() -> Channel<String> {
    return Channel<String>::with_capacity(1);
}

fn main() {
    select {
        make().send("payload" + "-1") => { println("sent"); }
        sleep(5000) => { println("timeout"); }
    }
}
"#,
    );
    assert!(ok, "{out}");
    assert_eq!(out, "sent\n");
}

#[test]
fn selroot_04_two_temporary_channels_both_stay_live() {
    let (out, ok) = compile_and_run_gc_stress_all(
        r#"
fn empty() -> Channel<i64> {
    return Channel<i64>::new();
}

fn make() -> Channel<i64> {
    return Channel<i64>::with_capacity(1);
}

fn main() {
    select {
        let v = empty().recv() => { println(v); }
        make().send(5) => { println("sent"); }
        sleep(5000) => { println("timeout"); }
    }
}
"#,
    );
    assert!(ok, "{out}");
    assert_eq!(out, "sent\n");
}

#[test]
fn selroot_05_timeout_exit_path_keeps_the_root_stack_balanced() {
    // A leaked root would keep every later allocation alive; a double pop would
    // corrupt the shadow stack. Both show up as a wrong result or a crash once
    // more work runs after the select.
    let (out, ok) = compile_and_run_gc_stress_all(
        r#"
fn make() -> Channel<String> {
    return Channel<String>::new();
}

fn main() {
    select {
        let v = make().recv() => { println(v); }
        sleep(10) => { println("timeout"); }
    }
    let s = "after" + "-select";
    gc_collect();
    println(s);
}
"#,
    );
    assert!(ok, "{out}");
    assert_eq!(out, "timeout\nafter-select\n");
}

#[test]
fn selroot_06_default_exit_path_keeps_the_root_stack_balanced() {
    let (out, ok) = compile_and_run_gc_stress_all(
        r#"
fn make() -> Channel<String> {
    return Channel<String>::new();
}

fn main() {
    select {
        let v = make().recv() => { println(v); }
        default => { println("nothing ready"); }
    }
    let s = "after" + "-default";
    gc_collect();
    println(s);
}
"#,
    );
    assert!(ok, "{out}");
    assert_eq!(out, "nothing ready\nafter-default\n");
}

#[test]
fn selroot_07_return_out_of_a_select_case_pops_its_roots() {
    let (out, ok) = compile_and_run_gc_stress_all(
        r#"
fn make() -> Channel<String> {
    return Channel<String>::with_capacity(1);
}

fn pick() -> String {
    select {
        make().send("early" + "-exit") => { return "sent"; }
        sleep(5000) => { return "timeout"; }
    }
    return "fell through";
}

fn main() {
    println(pick());
    gc_collect();
    println("done" + "!");
}
"#,
    );
    assert!(ok, "{out}");
    assert_eq!(out, "sent\ndone!\n");
}

#[test]
fn selroot_08_repeated_selects_over_temporaries_do_not_leak_roots() {
    let (out, ok) = compile_and_run(
        r#"
fn make() -> Channel<i64> {
    return Channel<i64>::with_capacity(1);
}

fn main() {
    let mut i = 0;
    let mut sent = 0;
    while i < 50 {
        select {
            make().send(i) => { sent = sent + 1; }
            default => { }
        }
        i = i + 1;
    }
    gc_collect();
    println(sent);
}
"#,
    );
    assert!(ok, "{out}");
    assert_eq!(out, "50\n");
}

#[test]
fn chroot_01_sync_send_of_a_computed_string_into_a_full_bounded_channel() {
    // Capacity 1 with a slow consumer: every send after the first parks and
    // drives the scheduler, so the freshly built string must be rooted across
    // the runtime call.
    let (out, ok) = compile_and_run_gc_stress_all(
        r#"
async fn drain(ch: Channel<String>, done: Channel<i64>) -> i64 {
    let mut n = 0;
    while n < 3 {
        await sleep(5);
        println(ch.recv());
        n = n + 1;
    }
    done.send(n);
    return n;
}

fn main() {
    let ch = Channel<String>::with_capacity(1);
    let done = Channel<i64>::with_capacity(1);
    drain(ch, done);
    let mut i = 0;
    while i < 3 {
        ch.send("item" + "-x");
        i = i + 1;
    }
    println(done.recv());
}
"#,
    );
    assert!(ok, "{out}");
    assert_eq!(out, "item-x\nitem-x\nitem-x\n3\n");
}

#[test]
fn chroot_02_sync_send_on_a_temporary_channel_keeps_the_channel_alive() {
    let (out, ok) = compile_and_run_gc_stress_all(
        r#"
fn make() -> Channel<String> {
    return Channel<String>::with_capacity(2);
}

fn main() {
    make().send("temp" + "-channel");
    println("sent");
}
"#,
    );
    assert!(ok, "{out}");
    assert_eq!(out, "sent\n");
}

#[test]
fn chroot_03_sync_recv_that_blocks_keeps_a_temporary_channel_alive() {
    let (out, ok) = compile_and_run_gc_stress_all(
        r#"
async fn feed(ch: Channel<String>) -> i64 {
    await sleep(10);
    ch.send("late" + "-payload");
    return 0;
}

fn make() -> Channel<String> {
    let ch = Channel<String>::with_capacity(1);
    feed(ch);
    return ch;
}

fn main() {
    println(make().recv());
}
"#,
    );
    assert!(ok, "{out}");
    assert_eq!(out, "late-payload\n");
}

#[test]
fn chroot_04_i64_elements_round_trip_through_the_same_blocking_path() {
    // The non-GC element path pushes one root (the channel) instead of two:
    // an unbalanced pop here would corrupt the shadow stack.
    let (out, ok) = compile_and_run_gc_stress_all(
        r#"
async fn drain(ch: Channel<i64>, done: Channel<i64>) -> i64 {
    let mut total = 0;
    let mut n = 0;
    while n < 4 {
        await sleep(2);
        total = total + ch.recv();
        n = n + 1;
    }
    done.send(total);
    return total;
}

fn main() {
    let ch = Channel<i64>::with_capacity(1);
    let done = Channel<i64>::with_capacity(1);
    drain(ch, done);
    let mut i = 1;
    while i < 5 {
        ch.send(i);
        i = i + 1;
    }
    println(done.recv());
    let s = "after" + "-loop";
    gc_collect();
    println(s);
}
"#,
    );
    assert!(ok, "{out}");
    assert_eq!(out, "10\nafter-loop\n");
}

#[test]
fn chroot_05_class_instances_round_trip_through_a_full_bounded_channel() {
    let (out, ok) = compile_and_run_gc_stress_all(
        r#"
class Item {
    pub label: String;

    pub static fn new(label: String) -> Item {
        return new Item(label);
    }
}

async fn drain(ch: Channel<Item>, done: Channel<i64>) -> i64 {
    let mut n = 0;
    while n < 2 {
        await sleep(5);
        let item = ch.recv();
        println(item.label);
        n = n + 1;
    }
    done.send(n);
    return n;
}

fn main() {
    let ch = Channel<Item>::with_capacity(1);
    let done = Channel<i64>::with_capacity(1);
    drain(ch, done);
    let mut i = 0;
    while i < 2 {
        ch.send(Item::new("boxed" + "-item"));
        i = i + 1;
    }
    println(done.recv());
}
"#,
    );
    assert!(ok, "{out}");
    assert_eq!(out, "boxed-item\nboxed-item\n2\n");
}

#[test]
fn chroot_06_long_sync_send_recv_loop_stays_correct_under_allocation_pressure() {
    let (out, ok) = compile_and_run_gc_stress(
        r#"
fn main() {
    let ch = Channel<String>::with_capacity(4);
    let mut i = 0;
    let mut seen = 0;
    while i < 30 {
        ch.send("value" + "-tag");
        let got = ch.recv();
        if got == "value-tag" {
            seen = seen + 1;
        }
        i = i + 1;
    }
    println(seen);
}
"#,
    );
    assert!(ok, "{out}");
    assert_eq!(out, "30\n");
}
