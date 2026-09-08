//! Collections that race scheduler mutator registration (willow-v6k0).
//!
//! Every thread that can hold GC roots registers as a MUTATOR: the thread
//! driving `main`, and each worker the scheduler spins up for a
//! `parallel::map` or a parallel drive. Registration is therefore a moving
//! target — a worker appears when the pool starts and disappears when it
//! drains, all while the collector runs.
//!
//! The collector used to ask `multi_mutator_active()` and, when it was the only
//! registered mutator, scan its own root stack and sweep the shared heap
//! without stopping the world. That answer is a SNAPSHOT: a worker registering
//! after the read runs unseen for the rest of the cycle, so the sweep frees the
//! objects it has just allocated and rooted. The program then runs on freed
//! memory — a wrong answer now, an "invalid GC pointer in GC root graph" abort
//! one cycle later, or a segfault through a dangling async-frame slot.
//!
//! The narrow window belongs to the runtime, and `gc.rs`'s `coord_*` tests hit
//! it directly. These perspectives cover the shapes that reach it from WILLOW
//! code: pool workers appearing and leaving around collections, under the
//! stress modes where every allocation collects. Each asserts an exact output,
//! so a freed-and-reused object shows up as a wrong line rather than a flake.
//!
//! 21 perspectives:
//!   1 a parallel map under alloc stress   12 a class field across the pool
//!   2 two maps with churn between them    13 a `defer` after the pool drains
//!   3 a parallel map under minor stress   14 a string built by the mapper
//!   4 four workers under alloc stress     15 strings churned by the workers
//!   5 one worker under alloc stress       16 a one-poll task budget
//!   6 many async tasks and collections    17 a short time quantum
//!   7 explicit collects between drives    18 the whole matrix, one program
//!   8 a channel across the worker pool    19 two net exchanges under stress
//!   9 a lambda mapper under stress        20 net across parked sleeps
//!  10 the mapped array outlives a collect 21 the example program
//!  11 release build, alloc stress

use super::support::{
    compile_and_run_gc_stress_mode, compile_and_run_release, compile_and_run_with_env,
};

/// Sources shared by several perspectives, so a perspective states only the
/// shape it adds.
const MAP_AND_COLLECT: &str = r#"
import std::collections::Array;
import std::parallel;

fn double(value: i64) -> i64 {
    return value * 2;
}

async fn total(values: FrozenArray<i64>) -> i64 {
    let mapped = await parallel::map(values, double);
    let mut sum = 0;
    let mut index = 0;
    while index < mapped.len() {
        sum = sum + mapped[index];
        index = index + 1;
    }
    return sum;
}

async fn main() {
    let values: Array<i64> = [1, 2, 3, 4, 5, 6, 7, 8];
    let frozen = values.freeze();
    let mut round = 0;
    let mut sum = 0;
    while round < 4 {
        sum = sum + await total(frozen);
        gc_collect();
        round = round + 1;
    }
    println(sum);
}
"#;

const NET_TWO_EXCHANGES: &str = r#"
import std::net;

async fn exchange(message: String) -> String {
    match await attempt(message) {
        Ok(text) => return text,
        Err(error) => return "net failed",
    }
}

async fn attempt(message: String) -> Result<String, IoError> {
    let listener = net::bind("127.0.0.1:0")?;
    let address = net::local_addr(listener)?;
    let accepting = net::accept_async(listener);
    let client = (await net::connect_async(address))?;
    (await net::write_async(client, message))?;
    let server = (await accepting)?;
    let got = (await net::read_async(server, 4096))?;
    return Ok(got);
}

async fn main() {
    let first = exchange("first");
    let second = exchange("second");
    println(await first);
    println(await second);
}
"#;

fn assert_stress(source: &str, mode: &str, expected: &str) {
    let (output, ok) = compile_and_run_gc_stress_mode(source, mode);
    assert!(
        ok,
        "expected the program to run under {mode} stress:\n{output}"
    );
    assert_eq!(output, expected);
}

fn assert_env(source: &str, env: &[(&str, &str)], expected: &str) {
    let (output, ok) = compile_and_run_with_env(source, env);
    assert!(ok, "expected the program to run under {env:?}:\n{output}");
    assert_eq!(output, expected);
}

#[test]
fn gcreg_01_a_parallel_map_under_alloc_stress() {
    // Every allocation collects, so the pool's workers register and unregister
    // across many cycles rather than one.
    assert_stress(MAP_AND_COLLECT, "alloc", "288\n");
}

#[test]
fn gcreg_02_two_maps_with_churn_between_them() {
    // Garbage between the maps gives the sweep something to reclaim while the
    // pool is spinning back up.
    assert_stress(
        r#"
import std::collections::Array;
import std::parallel;

fn triple(value: i64) -> i64 {
    return value * 3;
}

async fn churn(count: i64) -> i64 {
    let mut scratch: Array<String> = [];
    let mut index = 0;
    while index < count {
        scratch.push("scratch");
        index = index + 1;
    }
    return scratch.len();
}

async fn main() {
    let values: Array<i64> = [1, 2, 3];
    let frozen = values.freeze();
    let first = await parallel::map(frozen, triple);
    println(await churn(48));
    let second = await parallel::map(frozen, triple);
    println(first.toString());
    println(second.toString());
}
"#,
        "alloc",
        "48\n[3, 6, 9]\n[3, 6, 9]\n",
    );
}

#[test]
fn gcreg_03_a_parallel_map_under_minor_stress() {
    // The minor collector took the same fast path, and its sweep frees young
    // objects — exactly what a freshly registered worker has just allocated.
    assert_stress(MAP_AND_COLLECT, "minor", "288\n");
}

#[test]
fn gcreg_04_four_workers_under_alloc_stress() {
    assert_env(
        MAP_AND_COLLECT,
        &[("WILLOW_WORKERS", "4"), ("WILLOW_GC_STRESS", "alloc")],
        "288\n",
    );
}

#[test]
fn gcreg_05_one_worker_under_alloc_stress() {
    // A single worker still REGISTERS, so the driving thread is not alone even
    // when the pool cannot overlap anything.
    assert_env(
        MAP_AND_COLLECT,
        &[("WILLOW_WORKERS", "1"), ("WILLOW_GC_STRESS", "alloc")],
        "288\n",
    );
}

#[test]
fn gcreg_06_many_async_tasks_and_collections() {
    assert_env(
        r#"
import std::collections::Array;

async fn worker(id: i64) -> i64 {
    let mut ticks = 0;
    while ticks < id % 3 + 1 {
        await sleep(1);
        gc_collect();
        ticks = ticks + 1;
    }
    return id;
}

async fn main() {
    let tasks: Array<Task<i64>> = [];
    let mut id = 1;
    while id <= 12 {
        tasks.push(worker(id));
        id = id + 1;
    }
    let mut total = 0;
    let mut index = 0;
    while index < tasks.len() {
        total = total + await tasks[index];
        index = index + 1;
    }
    println(total);
}
"#,
        &[("WILLOW_WORKERS", "4"), ("WILLOW_GC_STRESS", "alloc")],
        "78\n",
    );
}

#[test]
fn gcreg_07_explicit_collects_between_drives() {
    // Both collectors, back to back, with the pool in play.
    assert_env(
        r#"
import std::collections::Array;
import std::parallel;

fn negate(value: i64) -> i64 {
    return 0 - value;
}

async fn main() {
    let values: Array<i64> = [4, 5, 6];
    let frozen = values.freeze();
    let mut round = 0;
    while round < 6 {
        let mapped = await parallel::map(frozen, negate);
        gc_collect();
        gc_minor_collect();
        if round == 5 {
            println(mapped.toString());
        }
        round = round + 1;
    }
}
"#,
        &[("WILLOW_WORKERS", "4"), ("WILLOW_GC_STRESS", "alloc")],
        "[-4, -5, -6]\n",
    );
}

#[test]
fn gcreg_08_a_channel_across_the_worker_pool() {
    // The channel buffer holds GC references that only the shared root set
    // reaches — a single-mutator scan never sees them.
    assert_env(
        r#"
async fn producer(ch: Channel<String>) {
    let mut index = 0;
    while index < 3 {
        ch.send("item");
        gc_collect();
        index = index + 1;
    }
    ch.close();
}

async fn main() {
    let ch = Channel<String>::new();
    let handle = producer(ch);
    println(ch.recv());
    println(ch.recv());
    println(ch.recv());
    await handle;
}
"#,
        &[("WILLOW_WORKERS", "4"), ("WILLOW_GC_STRESS", "alloc")],
        "item\nitem\nitem\n",
    );
}

#[test]
fn gcreg_09_a_lambda_mapper_under_stress() {
    assert_stress(
        r#"
import std::collections::Array;
import std::parallel;

async fn main() {
    let values: Array<i64> = [2, 4, 6, 8];
    let mapped = await parallel::map(values.freeze(), |value| value + 1);
    println(mapped.toString());
}
"#,
        "alloc",
        "[3, 5, 7, 9]\n",
    );
}

#[test]
fn gcreg_10_the_mapped_array_outlives_a_collection() {
    // The result was allocated by a worker. If the sweep missed that worker's
    // registration the array is freed, and reading it back prints garbage.
    assert_env(
        r#"
import std::collections::Array;
import std::parallel;

fn square(value: i64) -> i64 {
    return value * value;
}

async fn main() {
    let values: Array<i64> = [3, 5, 7];
    let mapped = await parallel::map(values.freeze(), square);
    let mut round = 0;
    while round < 8 {
        gc_collect();
        round = round + 1;
    }
    println(mapped.toString());
    println(mapped[0] + mapped[1] + mapped[2]);
}
"#,
        &[("WILLOW_WORKERS", "4"), ("WILLOW_GC_STRESS", "alloc")],
        "[9, 25, 49]\n83\n",
    );
}

#[test]
fn gcreg_11_release_build_under_alloc_stress() {
    // Release drops the debug pointer validation, so a freed object shows up as
    // a wrong total instead of an abort.
    let (output, ok) = compile_and_run_release(MAP_AND_COLLECT);
    assert!(ok, "expected the release program to run:\n{output}");
    assert_eq!(output, "288\n");
}

#[test]
fn gcreg_12_a_class_field_across_the_worker_pool() {
    assert_env(
        r#"
import std::collections::Array;
import std::parallel;

class Ledger {
    pub total: i64;
    pub label: String;
}

fn double(value: i64) -> i64 {
    return value * 2;
}

async fn main() {
    let ledger = new Ledger(0, "ledger");
    let values: Array<i64> = [1, 2, 3, 4];
    let frozen = values.freeze();
    let mut round = 0;
    while round < 3 {
        let mapped = await parallel::map(frozen, double);
        let mut index = 0;
        while index < mapped.len() {
            ledger.total = ledger.total + mapped[index];
            index = index + 1;
        }
        gc_collect();
        round = round + 1;
    }
    println(ledger.label);
    println(ledger.total);
}
"#,
        &[("WILLOW_WORKERS", "4"), ("WILLOW_GC_STRESS", "alloc")],
        "ledger\n60\n",
    );
}

#[test]
fn gcreg_13_a_defer_after_the_pool_drains() {
    assert_env(
        r#"
import std::collections::Array;
import std::parallel;

fn double(value: i64) -> i64 {
    return value * 2;
}

async fn mapped_sum(values: FrozenArray<i64>) -> i64 {
    defer println("done");
    let mapped = await parallel::map(values, double);
    gc_collect();
    return mapped[0] + mapped[1];
}

async fn main() {
    let values: Array<i64> = [10, 20];
    println(await mapped_sum(values.freeze()));
}
"#,
        &[("WILLOW_WORKERS", "4"), ("WILLOW_GC_STRESS", "alloc")],
        "done\n60\n",
    );
}

#[test]
fn gcreg_14_a_string_built_inside_the_mapper() {
    // The mapper allocates on the worker's own TLAB, which the cycle retires.
    assert_env(
        r#"
import std::collections::Array;
import std::parallel;

fn tag(value: i64) -> i64 {
    let text = "value-" + value.toString();
    if text == "value-22" {
        return 1;
    }
    return 0;
}

async fn main() {
    let values: Array<i64> = [1, 22, 333];
    let mapped = await parallel::map(values.freeze(), tag);
    println(mapped.toString());
}
"#,
        &[("WILLOW_WORKERS", "4"), ("WILLOW_GC_STRESS", "alloc")],
        "[0, 1, 0]\n",
    );
}

#[test]
fn gcreg_15_strings_churned_by_the_workers() {
    assert_env(
        r#"
import std::collections::Array;
import std::parallel;

fn width(value: i64) -> i64 {
    let mut text = "";
    let mut index = 0;
    while index < value {
        text = text + "x";
        index = index + 1;
    }
    if text == "xxx" {
        return 3;
    }
    return 0;
}

async fn main() {
    let values: Array<i64> = [2, 3, 4];
    let mapped = await parallel::map(values.freeze(), width);
    gc_collect();
    println(mapped.toString());
}
"#,
        &[("WILLOW_WORKERS", "4"), ("WILLOW_GC_STRESS", "alloc")],
        "[0, 3, 0]\n",
    );
}

#[test]
fn gcreg_16_a_one_poll_task_budget() {
    // A one-poll budget makes every task park after a single step, so the pool
    // hands work around constantly while the collector runs.
    assert_env(
        MAP_AND_COLLECT,
        &[
            ("WILLOW_WORKERS", "4"),
            ("WILLOW_TASK_BUDGET", "1"),
            ("WILLOW_GC_STRESS", "alloc"),
        ],
        "288\n",
    );
}

#[test]
fn gcreg_17_a_short_time_quantum() {
    assert_env(
        MAP_AND_COLLECT,
        &[
            ("WILLOW_WORKERS", "4"),
            ("WILLOW_TIME_QUANTUM_MS", "1"),
            ("WILLOW_GC_STRESS", "alloc"),
        ],
        "288\n",
    );
}

#[test]
fn gcreg_18_the_whole_matrix_in_one_program() {
    // Tasks, a channel, a parallel map and both collectors in one drive, so the
    // registered set changes several times per cycle.
    assert_env(
        r#"
import std::collections::Array;
import std::parallel;

fn double(value: i64) -> i64 {
    return value * 2;
}

async fn feed(ch: Channel<i64>, values: FrozenArray<i64>) {
    let mapped = await parallel::map(values, double);
    let mut index = 0;
    while index < mapped.len() {
        ch.send(mapped[index]);
        gc_minor_collect();
        index = index + 1;
    }
    ch.close();
}

async fn main() {
    let values: Array<i64> = [1, 2, 3];
    let ch = Channel<i64>::new();
    let handle = feed(ch, values.freeze());
    let mut total = 0;
    let mut taken = 0;
    while taken < 3 {
        total = total + ch.recv();
        gc_collect();
        taken = taken + 1;
    }
    await handle;
    println(total);
}
"#,
        &[
            ("WILLOW_WORKERS", "4"),
            ("WILLOW_TASK_BUDGET", "1"),
            ("WILLOW_GC_STRESS", "alloc"),
        ],
        "12\n",
    );
}

#[test]
fn gcreg_19_two_net_exchanges_under_stress() {
    // The reported failure (willow-v6k0): two async exchanges whose frames hold
    // sockets and buffers across parks, under the stress mode where every
    // allocation collects.
    assert_env(
        NET_TWO_EXCHANGES,
        &[
            ("WILLOW_LIR_BACKEND", "1"),
            ("WILLOW_LIR_REQUIRE", "1"),
            ("WILLOW_GC_STRESS", "alloc"),
        ],
        "first\nsecond\n",
    );
}

#[test]
fn gcreg_20_net_across_parked_sleeps() {
    // Parking between every I/O step widens the window in which the driving
    // thread collects while a worker owns the frame.
    assert_env(
        r#"
import std::net;

async fn exchange(message: String) -> Result<String, IoError> {
    let listener = net::bind("127.0.0.1:0")?;
    let address = net::local_addr(listener)?;
    await sleep(1);
    let accepting = net::accept_async(listener);
    let client = (await net::connect_async(address))?;
    await sleep(1);
    (await net::write_async(client, message))?;
    let server = (await accepting)?;
    let got = (await net::read_async(server, 4096))?;
    return Ok(got);
}

async fn main() {
    match await exchange("after parking") {
        Ok(text) => println(text),
        Err(error) => println("net failed"),
    }
}
"#,
        &[("WILLOW_WORKERS", "4"), ("WILLOW_GC_STRESS", "alloc")],
        "after parking\n",
    );
}

#[test]
fn gcreg_21_the_example_program_under_stress() {
    let source = include_str!("../../example/gc_mutator_registration.wi");
    assert_env(
        source,
        &[("WILLOW_WORKERS", "4"), ("WILLOW_GC_STRESS", "alloc")],
        "ledger\n288\n",
    );
}
