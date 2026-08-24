//! `lock` / `read` / `write` critical sections compiled from Lowered IR
//! (willow-0g8j.2.13).
//!
//! A critical section is two edges the compiler owns rather than the program:
//! an acquisition that may PARK the task, and a release on every way out of the
//! body. On the AST path both came from the statement tree; the walker builds
//! them out of a `SuspendOp::LockAcquire` terminator, a forced defer scope, and
//! a `LirInst::ReleaseLock` on each exit that leaves the section under its own
//! power. The panic exit has no instruction — an unwind releases from the
//! section's own cleanup block, which is why the scope is opened even for a
//! body that defers nothing.
//!
//! Every test asserts the same output from the AST emitter and from the walker,
//! and confirms in the selection log that the walker is the path that actually
//! ran. Without that confirmation a coverage regression would send the function
//! back to the AST emitter and the comparison would pass vacuously by comparing
//! the AST path with itself.
//!
//! 20 perspectives:
//!   1 read-modify-write commits       11 `?` out of the section releases
//!   2 fallthrough releases            12 RwLock write section
//!   3 `return` out of the section     13 RwLock read section is shared
//!   4 `break` out of the section      14 concurrent readers overlap
//!   5 `continue` out of the section   15 contention loses no update
//!   6 a loop INSIDE keeps the lock    16 the handle is a class field
//!   7 defer ordering across release   17 a GC-managed protected value
//!   8 panic recovered outside         18 the protected value is an object
//!   9 panic recovered inside          19 cancellation while parked
//!  10 sections run back to back       20 cancellation while held
//!
//! Plus three the type coverage brought with it: the two blocking cells, which
//! are single runtime calls rather than sections, and the runnable example.

use std::time::Duration;

use super::support::{
    compile_and_run_with_env, compile_and_run_with_env_timeout, compile_with_compiler_env,
};

const AST: [(&str, &str); 1] = [("WILLOW_LIR_BACKEND", "0")];
const LIR: [(&str, &str); 2] = [("WILLOW_LIR_BACKEND", "1"), ("WILLOW_LIR_REQUIRE", "1")];
const LIR_BUDGET: [(&str, &str); 3] = [
    ("WILLOW_LIR_BACKEND", "1"),
    ("WILLOW_LIR_REQUIRE", "1"),
    ("WILLOW_TASK_BUDGET", "1"),
];
const LIR_STRESS: [(&str, &str); 3] = [
    ("WILLOW_LIR_BACKEND", "1"),
    ("WILLOW_LIR_REQUIRE", "1"),
    ("WILLOW_GC_STRESS", "alloc"),
];

/// `expected` must come out of all four configurations, and `functions` must
/// each be named in the walker's selection log.
///
/// The preemption budget and the GC stress modes are not decoration here: a
/// section's frame slots have to survive a re-poll, and the handle and the
/// protected value are both traced, so a collection between acquisition and
/// release is exactly the case a wrong barrier would break.
fn assert_locks(source: &str, expected: &str, functions: &[&str]) {
    for env in [&AST[..], &LIR[..], &LIR_BUDGET[..], &LIR_STRESS[..]] {
        let (out, ok) = compile_and_run_with_env(source, env);
        assert!(ok, "lock run failed under {env:?}: {out}");
        assert_eq!(out, expected, "wrong output under {env:?}");
    }
    assert_walker_owns(source, functions);
}

/// Assert the walker compiled each named function, without running anything.
/// Used on its own by the tests whose runs need a deadline.
fn assert_walker_owns(source: &str, functions: &[&str]) {
    let (ok, stderr) = compile_with_compiler_env(
        source,
        &[
            ("WILLOW_LIR_BACKEND", "1"),
            ("WILLOW_LIR_REQUIRE", "1"),
            ("WILLOW_LIR_LOG", "1"),
        ],
    );
    assert!(ok, "logged LIR compile failed: {stderr}");
    for function in functions {
        let sync = format!("[lir] compiling `{function}` from lowered IR");
        let coop = format!("[lir] compiling async `{function}` from lowered IR");
        assert!(
            stderr.contains(&sync) || stderr.contains(&coop),
            "`{function}` did not use the LIR walker: {stderr}"
        );
    }
}

/// Perspective 1: the read-modify-write a `get`/`set` pair cannot express. The
/// binding is loaded out of the handle on acquisition and committed back on
/// release, so the write is visible to the next section.
#[test]
fn lir_locks_01_read_modify_write_commits() {
    assert_locks(
        r#"
async fn bump(m: Mutex<i64>, by: i64) {
    lock m as mut value {
        value = value + by;
    }
}
async fn main() {
    let m = Mutex::new(0);
    await bump(m, 5);
    await bump(m, 7);
    lock m as value { println(value); }
}
"#,
        "12\n",
        &["bump", "main"],
    );
}

/// Perspective 2: falling off the end of the body releases. Two sections in one
/// task prove it — a leaked release would park the second one forever, and the
/// scheduler would never resume it.
#[test]
fn lir_locks_02_fallthrough_releases() {
    assert_locks(
        r#"
async fn main() {
    let m = Mutex::new(1);
    lock m as mut value { value = value + 1; }
    lock m as mut value { value = value + 1; }
    lock m as value { println(value); }
}
"#,
        "3\n",
        &["main"],
    );
}

/// Perspective 3: a `return` from the middle of the body. The release is
/// emitted in front of the jump, so the value written before it is committed.
#[test]
fn lir_locks_03_return_out_of_the_section_commits_and_releases() {
    assert_locks(
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
        &["take", "main"],
    );
}

/// Perspective 4: `break` leaves the section AND the loop, so the release goes
/// in front of the jump to the loop exit.
#[test]
fn lir_locks_04_break_out_of_the_section_releases() {
    assert_locks(
        r#"
async fn until(m: Mutex<i64>, limit: i64) -> i64 {
    let mut rounds = 0;
    while true {
        lock m as mut value {
            if value >= limit {
                break;
            }
            value = value * 2;
        }
        rounds = rounds + 1;
    }
    return rounds;
}
async fn main() {
    let m = Mutex::new(3);
    println(await until(m, 40));
    lock m as value { println(value); }
}
"#,
        "4\n48\n",
        &["until", "main"],
    );
}

/// Perspective 5: `continue` is the same exit with a different destination.
#[test]
fn lir_locks_05_continue_out_of_the_section_releases() {
    assert_locks(
        r#"
async fn skipping(m: Mutex<i64>, times: i64) -> i64 {
    let mut i = 0;
    let mut skipped = 0;
    while i < times {
        i = i + 1;
        lock m as mut value {
            if i % 2 == 0 {
                skipped = skipped + 1;
                continue;
            }
            value = value + i;
        }
    }
    return skipped;
}
async fn main() {
    let m = Mutex::new(0);
    println(await skipping(m, 6));
    lock m as value { println(value); }
}
"#,
        "3\n9\n",
        &["skipping", "main"],
    );
}

/// Perspective 6: a loop written INSIDE the section. Its `break` stays in the
/// critical section, so nothing is released there — only falling off the end
/// of the body releases. The predicate is the defer depth: the loop's scope is
/// deeper than the section's, so an exit to it is not an exit from the section.
#[test]
fn lir_locks_06_loop_inside_the_section_keeps_the_lock() {
    assert_locks(
        r#"
async fn saturate(m: Mutex<i64>) {
    lock m as mut value {
        let mut i = 0;
        while i < 100 {
            if value > 40 {
                break;
            }
            value = value + 7;
            i = i + 1;
        }
    }
}
async fn main() {
    let m = Mutex::new(0);
    await saturate(m);
    lock m as value { println(value); }
}
"#,
        "42\n",
        &["saturate", "main"],
    );
}

/// Perspective 7: defer ordering across the release. The section's own defers
/// run while ownership is still held — they are entitled to see and change the
/// protected value — then the value is committed and the lock released, and
/// only then do the enclosing scope's defers run.
#[test]
fn lir_locks_07_defers_run_inside_then_the_lock_goes_back() {
    assert_locks(
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
        &["traced", "main"],
    );
}

/// Perspective 8: a panic recovered by an ENCLOSING scope. This is the exit
/// with no `ReleaseLock` instruction — the unwind releases from the section's
/// own cleanup block, which is why the walker opens a defer scope for a body
/// that defers nothing. A leaked release parks the next acquisition forever.
#[test]
fn lir_locks_08_panic_recovered_outside_the_section_releases() {
    assert_locks(
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
        &["risky", "main"],
    );
}

/// Perspective 9: a panic recovered by the section's OWN defer. Recovery
/// resumes at the block right after the body, which is where the fallthrough
/// release sits — so this path releases through the instruction while
/// perspective 8 releases through the cleanup, and both must commit once.
#[test]
fn lir_locks_09_panic_recovered_inside_the_section_releases() {
    assert_locks(
        r#"
async fn risky(m: Mutex<i64>) {
    lock m as mut value {
        defer match recover() {
            Some(info) => println("recovered"),
            None => println("clean"),
        }
        value = value + 1;
        panic("inside the section");
    }
}
async fn main() {
    let m = Mutex::new(0);
    await risky(m);
    lock m as mut value { value = value + 10; }
    lock m as value { println(value); }
}
"#,
        "recovered\n11\n",
        &["risky", "main"],
    );
}

/// Perspective 10: sections run back to back on two different handles. Each
/// acquisition reuses the same four frame slots in a loop, and every release
/// clears the handle slot, so the next one starts from zero.
#[test]
fn lir_locks_10_repeated_sections_reuse_the_frame_slots() {
    assert_locks(
        r#"
async fn bump_many(m: Mutex<i64>, times: i64) {
    let mut i = 0;
    while i < times {
        lock m as mut value { value = value + 1; }
        i = i + 1;
    }
}
async fn main() {
    let left = Mutex::new(0);
    let right = Mutex::new(100);
    await bump_many(left, 30);
    await bump_many(right, 4);
    lock left as value { println(value); }
    lock right as value { println(value); }
}
"#,
        "30\n104\n",
        &["bump_many", "main"],
    );
}

/// Perspective 11: `?` propagates out of the section. It is an early exit like
/// `return`, so the write made before it is still committed.
#[test]
fn lir_locks_11_try_propagation_out_of_the_section_releases() {
    assert_locks(
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
        "9\n7\n7\n",
        &["bump", "main"],
    );
}

/// Perspective 12: `lock write` over an `RwLock<T>`. A different runtime state
/// machine — readers and a writer rather than one owner — reached through the
/// same acquisition, so the walker keys the entry points on the lock mode.
#[test]
fn lir_locks_12_rwlock_write_section_commits() {
    assert_locks(
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
        &["publish", "main"],
    );
}

/// Perspective 13: `lock read` hands out a shared view, so the binding is never
/// writable. Two sequential readers see the same committed value.
#[test]
fn lir_locks_13_rwlock_read_section_is_shared() {
    assert_locks(
        r#"
async fn observe(l: RwLock<i64>) -> i64 {
    lock read l as value {
        return value;
    }
}
async fn main() {
    let l = RwLock::new(7);
    println(await observe(l));
    println(await observe(l));
}
"#,
        "7\n7\n",
        &["observe", "main"],
    );
}

/// Perspective 14: readers overlap. Each task parks inside its own read section
/// until the other has entered, which can only finish if both are admitted at
/// once — a write-exclusive acquisition would deadlock and time out.
#[test]
fn lir_locks_14_concurrent_readers_are_admitted_together() {
    let source = r#"
async fn reader(l: RwLock<i64>, mine: AtomicBool, theirs: AtomicBool) -> i64 {
    lock read l as value {
        mine.store(true);
        while !theirs.load() { }
        return value;
    }
}
async fn main() {
    let l = RwLock::new(4);
    let first = AtomicBool::new(false);
    let second = AtomicBool::new(false);
    let a = reader(l, first, second);
    let b = reader(l, second, first);
    println(await a + await b);
}
"#;
    let (out, ok, timed_out) = compile_and_run_with_env_timeout(
        source,
        &[
            ("WILLOW_LIR_BACKEND", "1"),
            ("WILLOW_LIR_REQUIRE", "1"),
            ("WILLOW_WORKERS", "4"),
        ],
        Duration::from_secs(20),
    );
    assert!(!timed_out, "concurrent readers did not overlap: {out}");
    assert!(ok, "reader run failed: {out}");
    assert_eq!(out, "8\n");
    assert_walker_owns(source, &["reader", "main"]);
}

/// Perspective 15: contention. Four tasks increment one counter 200 times each;
/// every increment survives, which is only true if each section is exclusive
/// and every release publishes.
#[test]
fn lir_locks_15_contended_increments_lose_nothing() {
    assert_locks(
        r#"
async fn bump_many(m: Mutex<i64>, times: i64) {
    let mut i = 0;
    while i < times {
        lock m as mut value { value = value + 1; }
        i = i + 1;
    }
}
async fn main() {
    let m = Mutex::new(0);
    let t0 = bump_many(m, 200);
    let t1 = bump_many(m, 200);
    let t2 = bump_many(m, 200);
    let t3 = bump_many(m, 200);
    await t0;
    await t1;
    await t2;
    await t3;
    lock m as value { println(value); }
}
"#,
        "800\n",
        &["bump_many", "main"],
    );
}

/// Perspective 16: the handle is a class field, so the target is a field load
/// rather than a name. The lowerer hoists it into a `let` first — it must be
/// evaluated exactly once, before anything can park.
#[test]
fn lir_locks_16_handle_in_a_class_field() {
    assert_locks(
        r#"
class Account {
    pub balance: Mutex<i64>;
    pub async fn deposit(self, amount: i64) {
        lock self.balance as mut value {
            value = value + amount;
        }
    }
    pub async fn read_balance(self) -> i64 {
        lock self.balance as value {
            return value;
        }
    }
}
async fn main() {
    let account = new Account(Mutex::new(100));
    await account.deposit(15);
    println(await account.read_balance());
}
"#,
        "115\n",
        &["main"],
    );
}

/// Perspective 17: a GC-managed protected value. The handle is a traced object
/// and the commit goes through the write barrier, so a String published from
/// inside a section is still reachable after a collection.
#[test]
fn lir_locks_17_gc_managed_protected_value_survives_collection() {
    assert_locks(
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
        &["rename", "main"],
    );
}

/// Perspective 18: the protected value is a class instance, mutated through the
/// binding. The binding is a reference, so the field write is visible outside
/// the section without being committed by value.
#[test]
fn lir_locks_18_protected_object_is_mutated_through_the_binding() {
    assert_locks(
        r#"
class Meter {
    pub reads: i64;
}
async fn touch(m: Mutex<Meter>) {
    lock m as mut value {
        value.reads = value.reads + 1;
    }
}
async fn main() {
    let m = Mutex::new(new Meter(0));
    await touch(m);
    await touch(m);
    gc_collect();
    lock m as value { println(value.reads); }
}
"#,
        "2\n",
        &["touch", "main"],
    );
}

/// Perspective 19: cancelling a task that is PARKED on a contended acquisition.
/// The waiter has to unregister rather than inherit the lock, or the holder's
/// release hands ownership to a task that will never poll again and every later
/// acquisition wedges.
#[test]
fn lir_locks_19_cancelled_waiter_does_not_wedge_the_lock() {
    let source = r#"
async fn holder(m: Mutex<i64>, entered: AtomicBool, release: AtomicBool) {
    lock m as mut value {
        entered.store(true);
        while !release.load() { }
        value = value + 1;
    }
}
async fn waiter(m: Mutex<i64>) {
    lock m as mut value {
        value = value + 100;
    }
}
async fn main() {
    let m = Mutex::new(0);
    let entered = AtomicBool::new(false);
    let release = AtomicBool::new(false);
    let held = holder(m, entered, release);
    while !entered.load() {
        await yield();
    }
    let parked = waiter(m);
    parked.cancel();
    release.store(true);
    await held;
    match await parked.result() {
        Ok(_) => { }
        Err(Cancelled) => { }
    }
    lock m as mut value { value = value + 1000; }
    lock m as value { println(value); }
}
"#;
    let (out, ok, timed_out) = compile_and_run_with_env_timeout(
        source,
        &[
            ("WILLOW_LIR_BACKEND", "1"),
            ("WILLOW_LIR_REQUIRE", "1"),
            ("WILLOW_WORKERS", "4"),
        ],
        Duration::from_secs(20),
    );
    assert!(!timed_out, "a cancelled waiter wedged the mutex: {out}");
    assert!(ok, "cancelled waiter run failed: {out}");
    assert!(
        out == "1001\n" || out == "1101\n",
        "the holder's increment and the later section must both land: {out}"
    );
    assert_walker_owns(source, &["holder", "waiter", "main"]);
}

/// Perspective 20: cancelling a task that is HOLDING the section. The cancel
/// entry commits the protected value and releases, in the same order an
/// ordinary exit uses: the section's own defers, then the release, then the
/// enclosing scope's defers.
///
/// A contender is already parked on the mutex when the cancel lands, so a
/// release that never happened parks it forever and the deadline fires instead
/// of the assertion. The printed order pins the defers around it, and the final
/// value pins the commit: the cancelled section's `+ 10` and the contender's
/// `+ 1` both have to be in it.
#[test]
fn lir_locks_20_cancelled_holder_commits_then_releases_then_defers() {
    let source = r#"
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
"#;
    let (out, ok, timed_out) = compile_and_run_with_env_timeout(
        source,
        &[
            ("WILLOW_LIR_BACKEND", "1"),
            ("WILLOW_LIR_REQUIRE", "1"),
            ("WILLOW_TASK_BUDGET", "1"),
            ("WILLOW_WORKERS", "5"),
        ],
        Duration::from_secs(20),
    );
    assert!(!timed_out, "cancel cleanup deadlocked: {out}");
    assert!(ok, "held cancellation cleanup failed: {out}");
    assert_eq!(out, "inner\nouter\n12\n");
    assert_walker_owns(source, &["victim", "contender", "main"]);
}

/// The two native-blocking cells came in with the same type coverage. They are
/// single runtime calls with no critical section, which is exactly why they are
/// callable from a SYNCHRONOUS function while a `lock` is not (E2603).
#[test]
fn lir_locks_21_blocking_cells_are_callable_from_sync_code() {
    assert_locks(
        r#"
fn flag_round_trip(initial: bool) -> bool {
    let cell = BlockingCell<bool>::new(initial);
    cell.set(!cell.get());
    return cell.get();
}
fn stage_round_trip(initial: String) -> String {
    let cell = BlockingRwCell<String>::new(initial);
    cell.write(cell.read() + "/done");
    return cell.read();
}
fn main() {
    println(flag_round_trip(false));
    println(stage_round_trip("build"));
}
"#,
        "true\nbuild/done\n",
        &["flag_round_trip", "stage_round_trip", "main"],
    );
}

/// A blocking cell is a leaked raw runtime pointer rather than a GC object, so
/// the walker must NOT root its receiver — handing the collector one would let
/// it trace a pointer it does not own. Allocating around every access is what
/// makes a wrong decision here visible.
#[test]
fn lir_locks_22_blocking_cell_receiver_is_not_traced() {
    assert_locks(
        r#"
class Holder {
    pub cell: BlockingCell<i64>;
}
fn main() {
    let holder = new Holder(BlockingCell<i64>::new(1));
    let mut i = 0;
    while i < 50 {
        let filler = "pad" + i.toString();
        holder.cell.set(holder.cell.get() + 1);
        i = i + 1;
    }
    gc_collect();
    println(holder.cell.get());
}
"#,
        "51\n",
        &["main"],
    );
}

/// The runnable example compiles with no fallback anywhere and prints the same
/// thing on both backends. It is the one place every exit path out of a section
/// appears in a single program.
#[test]
fn lir_locks_23_example_compiles_from_lir_end_to_end() {
    let source = include_str!("../../example/lir_locks.wi");
    let (ast, ast_ok) = compile_and_run_with_env(source, &AST);
    assert!(ast_ok, "AST run of the example failed: {ast}");
    let (lir, lir_ok) = compile_and_run_with_env(source, &LIR);
    assert!(lir_ok, "LIR run of the example failed: {lir}");
    assert_eq!(ast, lir, "the two backends must agree on the example");
    assert_walker_owns(
        source,
        &[
            "bump",
            "take",
            "bump_many",
            "until",
            "skipping",
            "inner_loop",
            "traced",
            "guarded",
            "rename",
            "label_of",
            "credit",
            "flag_round_trip",
            "stage_round_trip",
            "main",
        ],
    );
}
