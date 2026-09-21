use super::*;

// ── V1 restrictions, as the compiler driver reports them ─────────────────────

/// Perspective 28: E2603 — a park needs an async frame to resume into, so a
/// `lock` in a sync function is refused rather than silently blocking a worker.
#[test]
fn lock_lower_28_sync_function_rejected() {
    assert_compile_error_contains(
        "fn helper(m: Mutex<i64>) { lock m as value { println(value); } }\nfn main() { }\n",
        &[
            "error[E2603]",
            "only allowed in an async function",
            "async fn",
        ],
    );
}

/// Perspective 29: E2604 — suspending inside the body would hold the lock
/// across an unbounded wait. This is the rule that keeps sections short and the
/// reason the lowering never has to handle a suspension mid-body.
#[test]
fn lock_lower_29_await_in_body_rejected() {
    assert_compile_error_contains(
        "async fn main() { let m = Mutex::new(0); lock m as value { await sleep(1); } }\n",
        &[
            "error[E2604]",
            "cannot suspend or block while holding a Willow lock",
        ],
    );
}

/// Perspective 30: E2605 — no lock-order analysis exists, so lexical nesting is
/// refused outright rather than risking a deadlock cycle.
#[test]
fn lock_lower_30_nested_acquisition_rejected() {
    assert_compile_error_contains(
        "async fn main() { let a = Mutex::new(0); let b = Mutex::new(0); \
         lock a as x { lock b as y { println(x + y); } } }\n",
        &["error[E2605]", "nested lock acquisition"],
    );
}

/// Perspective 31: E2602 — the bare form is Mutex-only, and the diagnostic
/// points an `RwLock<T>` target at `lock read`/`lock write`.
#[test]
fn lock_lower_31_wrong_lock_type_rejected() {
    assert_compile_error_contains(
        "async fn main() { let r = RwLock::new(0); lock r as value { println(value); } }\n",
        &["error[E2602]", "`lock` requires `Mutex<T>`", "lock read"],
    );
}

/// Perspective 32: E2601 — a shared view cannot be written through.
#[test]
fn lock_lower_32_read_binding_cannot_be_mut() {
    assert_compile_error_contains(
        "async fn main() { let r = RwLock::new(0); lock read r as mut value { } }\n",
        &["error[E2601]", "cannot be `mut`"],
    );
}

/// Perspective 33: `lock`, `read` and `write` are contextual keywords. A program
/// written before the statement existed must keep compiling and running
/// unchanged — including one that declares a FUNCTION named `lock`.
#[test]
fn lock_lower_33_lock_read_write_remain_ordinary_identifiers() {
    let (out, ok) = compile_and_run(
        r#"
fn lock(n: i64) -> i64 { return n * 2; }
fn main() {
    let read = 1;
    let write = read + 1;
    let mut counter = lock(read + write);
    counter = counter + 1;
    println(counter);
    println(read + write);
}
"#,
    );
    assert!(ok, "contextual keywords broke a valid program: {out}");
    assert_eq!(out, "7\n3\n");
}

/// Perspective 34: cancelling a task that already loaded the protected value
/// uses the same cleanup order as every ordinary exit. The inner defer mutates
/// the frame-backed binding while ownership is held; cancellation must then
/// commit and release before the outer defer runs. The outer defer deliberately
/// waits for a contender to acquire the mutex, so the old
/// `inner -> outer -> release` ordering times out instead of being hidden by
/// nondeterministic output.
#[test]
fn lock_lower_34_held_cancellation_commits_releases_then_runs_outer_defer() {
    let (out, ok, timed_out) = compile_and_run_with_env_timeout(
        r#"
async fn victim(m: Mutex<i64>, entered: AtomicBool, contender_entered: AtomicBool) {
    defer {
        while !contender_entered.load() { }
        println("outer");
    }
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

async fn contender(m: Mutex<i64>, entered: AtomicBool) {
    lock m as value {
        entered.store(true);
    }
}

async fn main() {
    let m = Mutex::new(0);
    let victim_entered = AtomicBool::new(false);
    let contender_entered = AtomicBool::new(false);
    let stopped = victim(m, victim_entered, contender_entered);
    while !victim_entered.load() {
        await yield();
    }
    let next = contender(m, contender_entered);
    stopped.cancel();
    match await stopped.result() {
        Ok(_) => println("unexpected completion"),
        Err(Cancelled) => { }
    }
    await next;
    lock m as value {
        println(value);
    }
}
"#,
        &[("WILLOW_TASK_BUDGET", "1"), ("WILLOW_WORKERS", "5")],
        std::time::Duration::from_secs(15),
    );
    assert!(!timed_out, "cancel cleanup deadlocked: {out}");
    assert!(ok, "held cancellation cleanup failed: {out}");
    assert_eq!(out, "inner\nouter\n11\n");
}

/// Perspective 35: a GC-managed protected value is rooted by the lock binding
/// only until release. Keep the reader Task alive after its section, replace
/// the mutex value, then collect. If the old binding pointer remains in the
/// reader's async-frame slot, no object is reclaimable and `after < peak`
/// becomes false.
#[test]
fn lock_lower_35_release_clears_the_protected_gc_frame_slot() {
    let (out, ok, timed_out) = compile_and_run_with_env_timeout(
        r#"
class Box { pub value: i64; }

async fn read_then_stay_alive(m: Mutex<Box>, read_done: AtomicBool, stop: AtomicBool) {
    lock m as value {
        let observed = value.value;
    }
    read_done.store(true);
    let mut turns = 0;
    while !stop.load() {
        turns = turns + 1;
    }
}

async fn main() {
    let m = Mutex::new(new Box(1));
    let read_done = AtomicBool::new(false);
    let stop = AtomicBool::new(false);
    let reader = read_then_stay_alive(m, read_done, stop);
    while !read_done.load() {
        await yield();
    }

    lock m as mut value {
        value = new Box(2);
    }
    let peak = gc_allocated_bytes();
    gc_collect();
    let after = gc_allocated_bytes();
    println(after < peak);

    stop.store(true);
    await reader;
}
"#,
        &[("WILLOW_TASK_BUDGET", "1")],
        std::time::Duration::from_secs(15),
    );
    assert!(!timed_out, "GC frame-slot lifetime test timed out: {out}");
    assert!(ok, "GC frame-slot lifetime test failed: {out}");
    assert_eq!(out, "true\n");
}

/// Perspective 36: cancellation can be requested while the contender's poll
/// is already running, immediately before a contended acquire. The synchronous
/// wait helper creates that ordering without a cooperative safepoint. Runtime
/// must return CANCELLED (no registration), and generated code must yield to
/// cancellation cleanup; treating it as LOST retries forever while the holder
/// waits for main and therefore makes this test time out.
#[test]
fn lock_lower_36_cancel_requested_before_contended_acquire_does_not_spin() {
    let (out, ok, timed_out) = compile_and_run_with_env_timeout(
        r#"
fn wait_until_acquire(started: AtomicBool, proceed: Channel<i64>) {
    started.store(true);
    proceed.recv();
}

async fn holder(m: Mutex<i64>, holding: AtomicBool, release: AtomicBool) {
    lock m as value {
        holding.store(true);
        while !release.load() { }
    }
}

async fn contender(m: Mutex<i64>, started: AtomicBool, proceed: Channel<i64>) {
    wait_until_acquire(started, proceed);
    lock m as value {
        println("unexpected acquisition");
    }
}

async fn main() {
    let m = Mutex::new(0);
    let holding = AtomicBool::new(false);
    let release = AtomicBool::new(false);
    let started = AtomicBool::new(false);
    let proceed = Channel<i64>::new();

    let owner = holder(m, holding, release);
    while !holding.load() {
        await yield();
    }
    let stopped = contender(m, started, proceed);
    while !started.load() {
        await yield();
    }

    stopped.cancel();
    proceed.send(1);
    match await stopped.result() {
        Ok(_) => println("unexpected completion"),
        Err(Cancelled) => println("cancelled"),
    }

    release.store(true);
    await owner;
    println("done");
}
"#,
        &[("WILLOW_WORKERS", "5")],
        std::time::Duration::from_secs(15),
    );
    assert!(
        !timed_out,
        "cancelled acquire retried instead of yielding: {out}"
    );
    assert!(ok, "cancel-before-acquire handling failed: {out}");
    assert_eq!(out, "cancelled\ndone\n");
}

/// Perspective 37: the E2604 gate follows synchronous user calls instead of
/// looking only for wait syntax physically inside the critical section. This
/// is the minimal helper-hidden Channel.recv shape that previously compiled
/// and could park while retaining the mutex.
#[test]
fn lock_lower_37_transitive_helper_wait_is_rejected() {
    assert_compile_error_contains(
        r#"
fn receive(ch: Channel<i64>) -> i64 {
    return ch.recv();
}

async fn main() {
    let m = Mutex::new(0);
    let ch: Channel<i64> = Channel::new();
    lock m as value {
        let got = receive(ch);
    }
}
"#,
        &[
            "error[E2604]",
            "cannot call a waiting helper while holding a Willow lock",
            "Channel.recv",
        ],
    );
}

/// Perspective 38: a default interface body is the implementation selected
/// for a class that does not override it. Its wait effect must reach the
/// interface-dispatch callsite inside the critical section.
#[test]
fn lock_lower_38_default_interface_body_wait_is_rejected() {
    assert_compile_error_contains(
        r#"
interface Receiver extends Send {
    fn run(self, ch: Channel<i64>) {
        let value = ch.recv();
    }
}

class Impl implements Receiver {}

async fn main() {
    let m = Mutex::new(0);
    let ch: Channel<i64> = Channel::new();
    let receiver: Receiver = new Impl();
    lock m as value {
        receiver.run(ch);
    }
}
"#,
        &[
            "error[E2604]",
            "cannot call a waiting helper while holding a Willow lock",
            "Channel.recv",
        ],
    );
}

/// Perspective 39: imported implementation bodies are not part of the entry
/// TypeChecker's call graph yet. Interface dispatch therefore fails closed
/// instead of assuming that an unavailable implementation is pure.
#[test]
fn lock_lower_39_imported_interface_implementation_fails_closed() {
    let project = TestProject::new(
        "lock_interface_effect_import",
        &[
            (
                "receiver.wi",
                r#"
pub interface Receiver extends Send {
    fn run(self);
}

pub class Impl implements Receiver {
    pub channel: Channel<i64>;

    pub fn run(self) {
        let value = self.channel.recv();
    }
}
"#,
            ),
            (
                "main.wi",
                r#"
import receiver;

async fn main() {
    let m = Mutex::new(0);
    let ch: Channel<i64> = Channel::new();
    let target: receiver::Receiver = new receiver::Impl(ch);
    lock m as value {
        target.run();
    }
}
"#,
            ),
        ],
    );
    let output = project.compile("main.wi");
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(!output.status.success(), "unexpectedly compiled: {stderr}");
    assert!(stderr.contains("error[E2604]"), "{stderr}");
    assert!(
        stderr.contains("interface dispatch to an imported implementation"),
        "{stderr}"
    );
}

/// Perspective 40: a default body's effect belongs only to implementations
/// that actually inherit it. If every reachable implementation overrides the
/// method with a pure body, interface dispatch remains legal under the lock.
#[test]
fn lock_lower_40_unused_waiting_default_does_not_taint_override() {
    let (out, ok) = compile_and_run(
        r#"
interface Receiver extends Send {
    fn run(self, ch: Channel<i64>) {
        let value = ch.recv();
    }
}

class Impl implements Receiver {
    pub fn run(self, ch: Channel<i64>) {
        println(7);
    }
}

async fn main() {
    let m = Mutex::new(0);
    let ch: Channel<i64> = Channel::new();
    let receiver: Receiver = new Impl();
    lock m as value {
        receiver.run(ch);
    }
}
"#,
    );
    assert!(ok, "{out}");
    assert_eq!(out, "7\n");
}

/// Perspective 41: a class receiver's declared type only bounds the dynamic
/// type from above, so E2604 must consider the whole override union. This
/// program compiled clean before willow-uqzx.1.2: `Base::run` is pure, and the
/// waiting body lives in a subclass the call site never names.
#[test]
fn lock_lower_41_subclass_override_wait_is_rejected_through_a_base_receiver() {
    assert_compile_error_contains(
        r#"
open class Base {
    pub open fn run(self, ch: Channel<i64>) {}
}

class Derived extends Base {
    pub override fn run(self, ch: Channel<i64>) {
        let value = ch.recv();
    }
}

async fn main() {
    let m = Mutex::new(0);
    let ch: Channel<i64> = Channel::new();
    let base: Base = new Derived();
    lock m as value {
        base.run(ch);
    }
}
"#,
        &[
            "error[E2604]",
            "cannot call a waiting helper while holding a Willow lock",
            "Derived::run",
        ],
    );
}

/// Perspective 42: the union is bounded below by the declared type, so it is
/// not a blanket taint. Naming the pure sibling keeps the call legal and the
/// program still runs — the widening cannot be "report every class that
/// declares the name".
#[test]
fn lock_lower_42_pure_sibling_receiver_stays_legal() {
    let (out, ok) = compile_and_run(
        r#"
open class Base {
    pub open fn run(self, ch: Channel<i64>) {}
}

class Quiet extends Base {
    pub override fn run(self, ch: Channel<i64>) {
        println(7);
    }
}

class Loud extends Base {
    pub override fn run(self, ch: Channel<i64>) {
        let value = ch.recv();
    }
}

async fn main() {
    let m = Mutex::new(0);
    let ch: Channel<i64> = Channel::new();
    let quiet = new Quiet();
    lock m as value {
        quiet.run(ch);
    }
}
"#,
    );
    assert!(ok, "{out}");
    assert_eq!(out, "7\n");
}

/// Perspective 43: one call site is one diagnostic. The union records an edge
/// per member, and three waiting overrides must not become three errors on the
/// same span.
#[test]
fn lock_lower_43_override_union_reports_one_diagnostic_per_callsite() {
    let stderr = compile_error_stderr(
        r#"
open class Base {
    pub open fn run(self, ch: Channel<i64>) {}
}

class One extends Base {
    pub override fn run(self, ch: Channel<i64>) {
        let value = ch.recv();
    }
}

class Two extends Base {
    pub override fn run(self, ch: Channel<i64>) {
        let value = ch.recv();
    }
}

class Three extends Base {
    pub override fn run(self, ch: Channel<i64>) {
        let value = ch.recv();
    }
}

async fn main() {
    let m = Mutex::new(0);
    let ch: Channel<i64> = Channel::new();
    let base: Base = new One();
    lock m as value {
        base.run(ch);
    }
}
"#,
    );
    assert_eq!(
        stderr.matches("error[E2604]").count(),
        1,
        "expected one E2604, got: {stderr}"
    );
}

/// Perspective 44 (willow-uqzx.1.3): an `async` callee is an edge in the shared
/// graph, but a masked one. Calling one eagerly creates a Task; the waiting
/// happens on the Task, not on the caller's stack, so a helper that only spawns
/// stays wait-free and remains legal inside a critical section.
#[test]
fn lock_lower_44_eager_async_call_does_not_transmit_the_wait() {
    let (out, ok) = compile_and_run(
        r#"
async fn waiter(ch: Channel<i64>) -> i64 {
    return ch.recv();
}

async fn main() {
    let m = Mutex::new(0);
    let ch: Channel<i64> = Channel::new();
    ch.send(4);
    lock m as mut value {
        // Eager: a Task is created here, nothing waits here.
        let pending = waiter(ch);
        value = value + 1;
    }
    println(1);
}
"#,
    );
    assert!(ok, "{out}");
    assert_eq!(out, "1\n");
}

/// Perspective 45: awaiting the same callee inside the section is still
/// rejected. The mask suppresses the effect across the *call* edge only; the
/// `await` is a direct suspension of this body.
#[test]
fn lock_lower_45_awaiting_inside_the_section_is_still_rejected() {
    assert_compile_error_contains(
        r#"
async fn waiter(ch: Channel<i64>) -> i64 {
    return ch.recv();
}

async fn main() {
    let m = Mutex::new(0);
    let ch: Channel<i64> = Channel::new();
    lock m as mut value {
        value = await waiter(ch);
    }
}
"#,
        &["error[E2604]"],
    );
}

/// Perspective 46: the mask is one hop deep, not transitive laundering. A
/// *synchronous* helper that itself waits still taints its caller, even when a
/// sibling async callee in the same body is masked.
#[test]
fn lock_lower_46_a_sync_waiting_helper_still_taints_through_a_masked_sibling() {
    assert_compile_error_contains(
        r#"
async fn spawned(ch: Channel<i64>) -> i64 {
    return ch.recv();
}

fn blocking(ch: Channel<i64>) -> i64 {
    return ch.recv();
}

fn mixed(ch: Channel<i64>) -> i64 {
    let pending = spawned(ch);
    return blocking(ch);
}

async fn main() {
    let m = Mutex::new(0);
    let ch: Channel<i64> = Channel::new();
    lock m as mut value {
        value = mixed(ch);
    }
}
"#,
        &["error[E2604]", "mixed"],
    );
}

/// Perspective 47: a purely recursive synchronous cycle carries no wait seed,
/// so the shared fixpoint must still prove it wait-free rather than giving up
/// on the cycle. Recursion alone is not a wait.
///
/// The program is still rejected, by E0810, because an unpreemptible sync
/// helper cannot run in task context — and that is the point: the two analyses
/// now share one fixpoint and must still reach *different* conclusions about
/// the same cycle.
#[cfg(not(any(
    all(
        target_os = "linux",
        target_env = "gnu",
        any(target_arch = "x86_64", target_arch = "aarch64")
    ),
    all(
        target_os = "macos",
        any(target_arch = "x86_64", target_arch = "aarch64")
    ),
    all(target_os = "windows", target_env = "msvc", target_arch = "x86_64")
)))]
#[test]
fn lock_lower_47_a_pure_recursive_cycle_is_still_provably_wait_free() {
    let stderr = compile_error_stderr(
        r#"
fn even(n: i64) -> bool {
    if n == 0 {
        return true;
    }
    return odd(n - 1);
}

fn odd(n: i64) -> bool {
    if n == 0 {
        return false;
    }
    return even(n - 1);
}

async fn main() {
    let m = Mutex::new(0);
    lock m as mut value {
        if even(6) {
            value = value + 1;
        }
    }
}
"#,
    );
    assert!(stderr.contains("error[E0810]"), "{stderr}");
    assert!(
        !stderr.contains("error[E2604]"),
        "the cycle has no wait seed, so it must not be reported as waiting: {stderr}"
    );
}

/// Perspective 48: a recursive cycle that *does* reach a wait taints every
/// member, so entering the cycle from either end is rejected. The fixpoint has
/// to keep iterating a cycle rather than settling on first visit.
#[test]
fn lock_lower_48_a_waiting_recursive_cycle_taints_every_member() {
    assert_compile_error_contains(
        r#"
fn even(n: i64, ch: Channel<i64>) -> bool {
    if n == 0 {
        let value = ch.recv();
        return true;
    }
    return odd(n - 1, ch);
}

fn odd(n: i64, ch: Channel<i64>) -> bool {
    if n == 0 {
        return false;
    }
    return even(n - 1, ch);
}

async fn main() {
    let m = Mutex::new(0);
    let ch: Channel<i64> = Channel::new();
    lock m as mut value {
        if odd(6, ch) {
            value = value + 1;
        }
    }
}
"#,
        &["error[E2604]"],
    );
}

/// Perspective 49: E0810 still fires for a recursive synchronous helper called
/// from task context, and still names recursion rather than a loop. The witness
/// is the `min` of the reasons, and `Loop` sorting first must not turn a purely
/// recursive helper into a phantom `while`.
#[test]
fn lock_lower_49_recursion_witness_survives_the_shared_fixpoint() {
    assert_sync_preemption_capability(
        r#"
fn fib(n: i64) -> i64 {
    if n <= 1 {
        return n;
    }
    return fib(n - 1) + fib(n - 2);
}

async fn main() {
    println(fib(10));
}
"#,
        &["error[E0810]", "unbounded recursive work"],
    );
}

/// Perspective 50: a helper that both loops and reaches recursion is reported
/// as looping. `Loop` sorts before `Recursion`, and the shared witness join is
/// `min`, which is what preserves the long-standing wording.
#[test]
fn lock_lower_50_loop_dominates_recursion_in_the_witness_join() {
    assert_sync_preemption_capability(
        r#"
fn spin(n: i64) -> i64 {
    if n <= 0 {
        return 0;
    }
    return spin(n - 1);
}

fn drive(n: i64) -> i64 {
    let mut total = 0;
    let mut i = 0;
    while i < n {
        total = total + spin(i);
        i = i + 1;
    }
    return total;
}

async fn main() {
    println(drive(3));
}
"#,
        &["error[E0810]", "with a loop is not preemptible"],
    );
}

#[test]
fn rwlock_lower_01_writers_preserve_all_updates() {
    let (out, ok) = compile_and_run(RWLOCK_CONTENDED_COUNTER_SRC);
    assert!(ok, "{out}");
    assert_eq!(out, "1000\n");
}

#[test]
fn rwlock_lower_02_one_worker_waits_without_blocking_it() {
    let (out, ok) = compile_and_run_with_env(
        RWLOCK_CONTENDED_COUNTER_SRC,
        &[("WILLOW_WORKERS", "1"), ("WILLOW_TASK_BUDGET", "1")],
    );
    assert!(ok, "single-worker RwLock contention must progress: {out}");
    assert_eq!(out, "1000\n");
}

#[test]
fn rwlock_lower_03_sixteen_workers_keep_exclusive_writes_exact() {
    let (out, ok) = compile_and_run_with_env(
        RWLOCK_CONTENDED_COUNTER_SRC,
        &[("WILLOW_WORKERS", "16"), ("WILLOW_TASK_BUDGET", "1")],
    );
    assert!(ok, "{out}");
    assert_eq!(out, "1000\n");
}

#[test]
fn rwlock_lower_04_reference_commit_survives_gc_stress() {
    let source = r#"
async fn main() {
    let r = RwLock::new("");
    lock write r as mut value { value = "wil" + "low"; }
    gc_collect();
    lock read r as value { println(value); }
}
"#;
    let (out, ok) = compile_and_run_with_env(source, &[("WILLOW_GC_STRESS", "1")]);
    assert!(ok, "{out}");
    assert_eq!(out, "willow\n");
}

#[test]
fn rwlock_lower_05_early_return_releases_reader_and_writer() {
    let (out, ok) = compile_and_run(
        r#"
async fn set(r: RwLock<i64>) -> i64 {
    lock write r as mut value { value = 9; return value; }
}
async fn get(r: RwLock<i64>) -> i64 {
    lock read r as value { return value; }
}
async fn main() {
    let r = RwLock::new(1);
    println(await set(r));
    println(await get(r));
}
"#,
    );
    assert!(ok, "{out}");
    assert_eq!(out, "9\n9\n");
}

#[test]
fn rwlock_lower_06_read_binding_is_immutable() {
    assert_compile_error_contains(
        "async fn main() { let r = RwLock::new(0); lock read r as value { value = 1; } }",
        &["error[E0301]", "immutable"],
    );
}

#[test]
fn rwlock_lower_07_accessors_are_replaced_atomically() {
    for method in ["read()", "write(1)"] {
        assert_compile_error_contains(
            &format!("async fn main() {{ let r = RwLock::new(0); r.{method}; }}"),
            &["lock read", "lock write", "BlockingRwCell<T>"],
        );
    }
}

#[test]
fn rwlock_lower_08_blocking_compatibility_type_is_explicit() {
    let (out, ok) = compile_and_run(
        "fn main() { let r = BlockingRwCell::new(1); r.write(2); println(r.read()); }",
    );
    assert!(ok, "{out}");
    assert_eq!(out, "2\n");
}

#[test]
fn rwlock_lower_09_multiple_reader_tasks_complete() {
    let (out, ok) = compile_and_run_with_env(
        r#"
async fn read(r: RwLock<i64>) -> i64 {
    lock read r as value { return value; }
}
async fn main() {
    let r = RwLock::new(7);
    let a = read(r); let b = read(r); let c = read(r); let d = read(r);
    println(await a + await b + await c + await d);
}
"#,
        &[("WILLOW_TASK_BUDGET", "1")],
    );
    assert!(ok, "{out}");
    assert_eq!(out, "28\n");
}

#[test]
fn rwlock_lower_10_one_worker_progresses_unrelated_task_while_waiter_is_parked() {
    let (out, ok) = compile_and_run_with_env(
        r#"
async fn hold(r: RwLock<i64>, entered: AtomicI64) {
    lock write r as mut value {
        entered.store(1);
        let mut i = 0;
        while i < 5000 { i = i + 1; }
        value = 9;
    }
}
async fn wait_for_read(r: RwLock<i64>, entered: AtomicI64) -> i64 {
    while entered.load() == 0 { await yield(); }
    lock read r as value { return value; }
}
async fn unrelated(entered: AtomicI64, progressed: AtomicI64) {
    while entered.load() == 0 { await yield(); }
    progressed.store(1);
}
async fn main() {
    let r = RwLock::new(0);
    let entered = AtomicI64::new(0);
    let progressed = AtomicI64::new(0);
    let owner = hold(r, entered);
    let waiter = wait_for_read(r, entered);
    let bystander = unrelated(entered, progressed);
    await owner;
    await bystander;
    println(progressed.load());
    println(await waiter);
}
"#,
        &[("WILLOW_WORKERS", "1"), ("WILLOW_TASK_BUDGET", "1")],
    );
    assert!(ok, "{out}");
    assert_eq!(out, "1\n9\n");
}

#[test]
fn lock_gc_handle_01_mutex_constructor_roots_reference_argument() {
    let (out, ok) = compile_and_run_with_env(
        r#"
class Node { pub value: i64; }
async fn main() {
    let m = Mutex::new(new Node(7));
    gc_collect();
    lock m as node { println(node.value); }
}
"#,
        &[("WILLOW_GC_STRESS", "alloc")],
    );
    assert!(ok, "{out}");
    assert_eq!(out, "7\n");
}

#[test]
fn lock_gc_handle_02_rwlock_constructor_and_trace_update_reference() {
    let (out, ok) = compile_and_run_with_env(
        r#"
class Node { pub value: i64; }
async fn main() {
    let r = RwLock::new(new Node(8));
    gc_collect();
    lock read r as node { println(node.value); }
}
"#,
        &[("WILLOW_GC_STRESS", "alloc")],
    );
    assert!(ok, "{out}");
    assert_eq!(out, "8\n");
}

#[test]
fn lock_gc_handle_03_mutex_constructor_remembers_young_value_through_holder() {
    let (out, ok) = compile_and_run_with_env(
        r#"
class Payload { pub tag: i64; }
class Holder { pub lock: Mutex<Payload>; }

async fn main() {
    // Runtime lock handles are old-generation objects. Keep this handle reachable
    // only through another object so a direct stack root cannot mask a missing
    // old-to-young remembered-set edge for its initial protected value.
    let holder = new Holder(Mutex::new(new Payload(1234567)));
    gc_minor_collect();

    let mut i = 0;
    while i < 4096 {
        let junk = new Payload(-1);
        i = i + 1;
    }

    lock holder.lock as value { println(value.tag); }
}
"#,
        &[("WILLOW_GC_VERIFY_BARRIER", "1")],
    );
    assert!(ok, "{out}");
    assert_eq!(out, "1234567\n");
}

#[test]
fn lock_gc_handle_04_rwlock_constructor_remembers_young_value_through_holder() {
    let (out, ok) = compile_and_run_with_env(
        r#"
class Payload { pub tag: i64; }
class Holder { pub lock: RwLock<Payload>; }

async fn main() {
    // Deliberately do not enable WILLOW_GC_STRESS=alloc: stress allocation sends
    // the payload directly to old generation and makes this edge vacuous.
    let holder = new Holder(RwLock::new(new Payload(7654321)));
    gc_minor_collect();

    let mut i = 0;
    while i < 4096 {
        let junk = new Payload(-1);
        i = i + 1;
    }

    lock read holder.lock as value { println(value.tag); }
}
"#,
        &[("WILLOW_GC_VERIFY_BARRIER", "1")],
    );
    assert!(ok, "{out}");
    assert_eq!(out, "7654321\n");
}

#[test]
fn lock_gc_handle_05_waiting_and_owned_frames_root_rwlock_during_collection() {
    let (out, ok) = compile_and_run_with_env(
        r#"
class Node { pub value: i64; }
async fn hold(r: RwLock<Node>, started: AtomicI64) {
    lock write r as mut node {
        started.store(1);
        let mut i = 0;
        while i < 20000 { i = i + 1; }
        node = new Node(node.value + 1);
    }
}
async fn wait_read(r: RwLock<Node>) -> i64 {
    lock read r as node { return node.value; }
}
async fn collect_when_held(started: AtomicI64) {
    while started.load() == 0 { await yield(); }
    gc_collect();
}
async fn main() {
    let r = RwLock::new(new Node(10));
    let started = AtomicI64::new(0);
    let owner = hold(r, started);
    let waiter = wait_read(r);
    let collector = collect_when_held(started);
    await owner;
    await collector;
    println(await waiter);
}
"#,
        &[
            ("WILLOW_WORKERS", "1"),
            ("WILLOW_TASK_BUDGET", "1"),
            ("WILLOW_GC_STRESS", "alloc"),
            ("WILLOW_GC_VERIFY_BARRIER", "1"),
        ],
    );
    assert!(ok, "{out}");
    assert_eq!(out, "11\n");
}

#[test]
fn async_reference_escape_reports_e1708_after_e1707() {
    let (ok, diagnostics) = compile_with_compiler_env(
        "async fn read(x: &i64) -> i64 { return x; } async fn main() { let x = 1; let task = read(&x); }",
        &[],
    );
    assert!(!ok, "escaping async borrow unexpectedly compiled");
    let declaration = diagnostics.find("error[E1707]").expect(&diagnostics);
    let escape = diagnostics.find("error[E1708]").expect(&diagnostics);
    assert!(declaration < escape, "{diagnostics}");
    assert!(
        diagnostics.contains("async reference borrow escapes its discharge block"),
        "{diagnostics}"
    );
}

#[test]
fn imported_synchronous_reference_call_returns_independent_task() {
    let (out, ok) = compile_temp_project_and_run(
        &[
            (
                "factory.wi",
                "async fn other() -> i64 { return 42; } pub fn make(x: &i64) -> Task<i64> { return other(); }",
            ),
            (
                "main.wi",
                "import factory; fn consume(task: Task<i64>) -> Task<i64> { return task; } async fn main() { let x = 1; let task = factory::make(&x); println(await consume(task)); }",
            ),
        ],
        "main.wi",
    );
    assert!(ok, "{out}");
    assert_eq!(out, "42\n");
}

#[test]
fn imported_asynchronous_reference_call_still_rejects_escape() {
    let project = TestProject::new(
        "imported_async_reference_escape",
        &[
            (
                "factory.wi",
                "pub async fn make(x: &i64) -> i64 { return x; }",
            ),
            (
                "main.wi",
                "import factory; fn consume(task: Task<i64>) {} fn main() { let x = 1; let task = factory::make(&x); consume(task); }",
            ),
        ],
    );
    let output = project.compile("main.wi");
    assert!(!output.status.success());
    let diagnostics = String::from_utf8_lossy(&output.stderr);
    assert!(diagnostics.contains("error[E1708]"), "{diagnostics}");
}
