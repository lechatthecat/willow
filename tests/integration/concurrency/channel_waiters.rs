use super::*;

// ── Many waiters on one channel (willow-ezs.1.2) ─────────────────────────────
//
// End-to-end coverage the unit tests cannot give: real generated code parking
// hundreds of tasks on a single channel, cancelling some of them, and mixing
// recv and send waiters. Perspectives 29-32 of willow-ezs.1.2 (1-15 cover
// `WaiterQueue`, 16-28 the scheduler's blocked-syscall counter).

/// 29. hundreds of consumers park on ONE channel and every value is taken
///     exactly once, whichever consumer wins it.
#[test]
fn manywait_29_hundreds_of_consumers_share_one_channel() {
    let (out, ok, timed_out) = compile_and_run_with_env_timeout(
        r#"
async fn consume(work: Channel<i64>, done: Channel<i64>) {
    let mut total = 0;
    let mut value = work.recv();
    while value != 0 {
        total = total + value;
        value = work.recv();
    }
    done.send(total);
}

async fn main() {
    let work = Channel<i64>::new();
    let done = Channel<i64>::new();

    let mut spawned = 0;
    while spawned < 200 {
        consume(work, done);
        spawned = spawned + 1;
    }

    let mut i = 1;
    while i <= 500 {
        work.send(i);
        i = i + 1;
    }
    let mut stops = 0;
    while stops < 200 {
        work.send(0);
        stops = stops + 1;
    }
    work.close();

    let mut total = 0;
    let mut collected = 0;
    while collected < 200 {
        total = total + done.recv();
        collected = collected + 1;
    }
    println(total);
}
"#,
        &[("WILLOW_WORKERS", "5")],
        std::time::Duration::from_secs(30),
    );
    assert!(!timed_out, "many-consumer fan-out hung:\n{out}");
    assert!(ok, "{out}");
    assert_eq!(out, "125250\n"); // 1 + 2 + ... + 500
}

/// 30. cancelling waiters parked on a shared channel deregisters them, so the
///     survivors still receive every value (a stale entry would swallow a
///     wake).
#[test]
fn manywait_30_cancelled_waiters_do_not_swallow_wakes() {
    let (out, ok, timed_out) = compile_and_run_with_env_timeout(
        r#"
async fn park_forever(work: Channel<i64>) {
    work.recv();
}

async fn take_one(work: Channel<i64>, done: Channel<i64>) {
    done.send(work.recv());
}

async fn main() {
    let work = Channel<i64>::new();
    let done = Channel<i64>::new();

    // 100 tasks park on `work` and are then cancelled while still parked.
    let mut parked = [park_forever(work)];
    let mut spawned = 1;
    while spawned < 100 {
        parked.push(park_forever(work));
        spawned = spawned + 1;
    }
    await sleep(5);
    let mut i = 0;
    while i < 100 {
        parked[i].cancel();
        i = i + 1;
    }

    // One live consumer must still be woken by the send.
    let live = take_one(work, done);
    await sleep(5);
    work.send(7);
    println(done.recv());
    await live;
}
"#,
        &[("WILLOW_WORKERS", "5")],
        std::time::Duration::from_secs(30),
    );
    assert!(!timed_out, "cancelled-waiter fan-out hung:\n{out}");
    assert!(ok, "{out}");
    assert_eq!(out, "7\n");
}

/// 31. many producers park on ONE full bounded channel; each freed slot wakes
///     one producer until the queue drains, with no loss or duplication.
#[test]
fn manywait_31_many_producers_park_on_one_bounded_channel() {
    let (out, ok, timed_out) = compile_and_run_with_env_timeout(
        r#"
async fn produce(work: Channel<i64>, value: i64) {
    work.send(value);
}

async fn main() {
    let work = Channel<i64>::with_capacity(2);

    let mut spawned = 0;
    while spawned < 150 {
        produce(work, 1);
        spawned = spawned + 1;
    }

    let mut total = 0;
    let mut received = 0;
    while received < 150 {
        total = total + work.recv();
        received = received + 1;
    }
    println(total);
}
"#,
        &[("WILLOW_WORKERS", "5")],
        std::time::Duration::from_secs(30),
    );
    assert!(!timed_out, "bounded producer fan-in hung:\n{out}");
    assert!(ok, "{out}");
    assert_eq!(out, "150\n");
}

/// 32. a select loop repeatedly registers and unregisters on the same
///     channels; the waiter queues must not grow without bound or start
///     dropping wakes.
#[test]
fn manywait_32_repeated_select_registration_stays_correct() {
    let (out, ok, timed_out) = compile_and_run_with_env_timeout(
        r#"
async fn feed(ch: Channel<i64>, count: i64) {
    let mut i = 0;
    while i < count {
        ch.send(1);
        await sleep(1);
        i = i + 1;
    }
    ch.close();
}

async fn main() {
    let live = Channel<i64>::new();
    let idle = Channel<i64>::new();

    let feeder = feed(live, 200);

    // Every loser iteration re-registers on `idle` and unregisters again.
    let mut taken = 0;
    let mut spins = 0;
    while taken < 200 {
        select {
            let v = live.recv() => { taken = taken + v; }
            let v = idle.recv() => { spins = spins + v; }
        }
    }
    await feeder;
    println(taken);
    println(spins);
}
"#,
        &[("WILLOW_WORKERS", "5")],
        std::time::Duration::from_secs(30),
    );
    assert!(!timed_out, "select registration loop hung:\n{out}");
    assert!(ok, "{out}");
    assert_eq!(out, "200\n0\n");
}
