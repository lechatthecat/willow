//! Scheduler scaling and footprint measurements (willow-ezs.2, willow-ezs.3).
//!
//! These are measurements, not assertions about wall-clock time: they print a
//! CSV table and only assert the invariants that must hold regardless of the
//! machine. They are `#[ignore]`d so a slow, allocation-heavy sweep never runs
//! in the normal gate, matching the GC stress suite's convention:
//!
//! ```shell
//! cargo test -p willow_runtime --release scaling_ -- --ignored --nocapture
//! ```
//!
//! Columns:
//!
//! ```text
//! case,tasks,elapsed_ms,ns_per_task,bytes_per_task
//! ```
//!
//! Read scaling down the `ns_per_task` column of one case: a flat column is the
//! expected O(1)-per-operation behavior, a column that grows with the task
//! count is the quadratic behavior this work removed. `bytes_per_task` reports
//! the inline `RuntimeTask` size plus the heap its metadata owns.
//!
//! ## Checked-in footprint record (willow-ezs.3)
//!
//! Requested heap bytes per task at 10,000 simultaneously active tasks, release
//! build, **measured through a counting global allocator** rather than estimated
//! (`scaling_07_measured_heap_per_active_task`). This already includes the
//! inline `RuntimeTask` values stored in the scheduler's `HashMap` buckets, so
//! `size_of::<RuntimeTask>()` must not be added a second time. The "before"
//! column was produced by running the same measurement against the pre-change
//! layout.
//! `size_of::<RuntimeTask>()` itself went from 264 to 80 bytes.
//!
//! ```text
//! workload                    before  after   change
//! active_ready                  461    159     -66%
//! active_completion_waiting     493    412     -16%
//! active_channel_waiting        480    338     -30%
//! active_debug_tagged           469    223     -52%
//! ```
//!
//! Total allocated bytes fall for every workload, which is the acceptance
//! criterion: the split is never a loss. What it does cost is one extra
//! allocation per task for the workloads that use the cold data —
//! allocations/task go 2.00 -> 3.00 for the waiting workloads and 3.00 -> 4.00
//! for the tagged one — traded against ~180 inline bytes that every task used
//! to carry whether or not it ever waited or was tagged.
//!
//! Where the 184 inline bytes went:
//!
//! ```text
//! roots: GcRootSet                24   dead — frames are rooted through the
//!                                      runtime root registry, never here
//! spawned_from: Option<Trace>     24   dead — never read or written
//! stack_trace: RuntimeStackTrace  24   dead — never read or written
//! waiters + awaiting +            72   moved behind Option<Box<TaskWaitLinks>>
//!   wait_channels
//! name + spawn_site               56   moved behind Option<Box<TaskDebugInfo>>
//! ```
//!
//! (200 bytes of fields, against a 184-byte drop in `size_of`: the rest is
//! padding the remaining bools now share.)
//!
//! The wait box is allocated on the first registration and released with the
//! last, and the debug box is independent of it — a tagged task that never
//! waits allocates no wait box, and vice versa.
//!
//! `scaling_05_active_task_footprint_by_workload` reports a cheaper structural
//! *estimate* of the same thing. Keep it for spotting which field group grew,
//! but do not quote it as a footprint number: it counts live entries rather
//! than reserved hash capacity and cannot see bucket arrays, `String` capacity,
//! boxed allocations, or the scheduler's own task table.

use super::*;
use crate::gc::runtime_test_guard;

const TASK_COUNTS: [usize; 4] = [1_000, 2_500, 5_000, 10_000];

/// A counting global allocator, so the footprint measurements report bytes the
/// allocator was actually asked for rather than a hand-rolled estimate.
///
/// The estimate in [`task_metadata_bytes`] cannot see what it does not model:
/// hash control bytes and power-of-two bucket arrays, `String` capacity beyond
/// its length, the scheduler's own task table, or the `Box` around each lazy
/// section. This wrapper sees all of it, because every one of those goes
/// through `GlobalAlloc`.
///
/// It counts requested bytes, not the size class the platform allocator rounds
/// a request up to, so it is a lower bound on RSS and it is deterministic
/// across platforms — which is what makes before/after numbers comparable at
/// all. It is compiled only into this crate's test binary.
#[cfg(test)]
mod counting_allocator {
    use std::alloc::{GlobalAlloc, Layout, System};
    use std::sync::atomic::{AtomicUsize, Ordering};

    static LIVE_BYTES: AtomicUsize = AtomicUsize::new(0);
    static ALLOCATIONS: AtomicUsize = AtomicUsize::new(0);

    pub struct Counting;

    // SAFETY: every method forwards to `System` unchanged and only adds
    // relaxed counter arithmetic, which allocates nothing and so cannot
    // re-enter the allocator.
    unsafe impl GlobalAlloc for Counting {
        unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
            let ptr = unsafe { System.alloc(layout) };
            if !ptr.is_null() {
                LIVE_BYTES.fetch_add(layout.size(), Ordering::Relaxed);
                ALLOCATIONS.fetch_add(1, Ordering::Relaxed);
            }
            ptr
        }

        unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
            LIVE_BYTES.fetch_sub(layout.size(), Ordering::Relaxed);
            unsafe { System.dealloc(ptr, layout) }
        }

        unsafe fn alloc_zeroed(&self, layout: Layout) -> *mut u8 {
            let ptr = unsafe { System.alloc_zeroed(layout) };
            if !ptr.is_null() {
                LIVE_BYTES.fetch_add(layout.size(), Ordering::Relaxed);
                ALLOCATIONS.fetch_add(1, Ordering::Relaxed);
            }
            ptr
        }

        unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
            let new_ptr = unsafe { System.realloc(ptr, layout, new_size) };
            if !new_ptr.is_null() {
                LIVE_BYTES.fetch_add(new_size, Ordering::Relaxed);
                LIVE_BYTES.fetch_sub(layout.size(), Ordering::Relaxed);
                ALLOCATIONS.fetch_add(1, Ordering::Relaxed);
            }
            new_ptr
        }
    }

    #[global_allocator]
    static GLOBAL: Counting = Counting;

    /// Bytes currently requested from the allocator and not yet freed.
    pub fn live_bytes() -> usize {
        LIVE_BYTES.load(Ordering::Relaxed)
    }

    /// Allocation calls since process start.
    pub fn allocations() -> usize {
        ALLOCATIONS.load(Ordering::Relaxed)
    }
}

fn header() {
    println!("case,tasks,elapsed_ms,ns_per_task,bytes_per_task");
}

fn report(case: &str, tasks: usize, elapsed: std::time::Duration, bytes_per_task: usize) {
    let nanos = elapsed.as_nanos();
    println!(
        "{case},{tasks},{:.3},{:.1},{bytes_per_task}",
        nanos as f64 / 1_000_000.0,
        nanos as f64 / tasks as f64
    );
}

/// A structural *estimate* of the bytes this task owns beyond the scheduler's
/// inline `RuntimeTask` slot: the wait relationships, the boxed preemption
/// flag, and the optional debug metadata.
///
/// It counts live entries and string lengths, not reserved hash capacity or
/// `String` capacity, and it knows nothing about bucket arrays, allocator
/// rounding, or the scheduler's task table. It is useful for attributing a
/// change to a field group; `scaling_07_measured_heap_per_active_task` is the
/// number to quote for footprint.
fn task_metadata_bytes(task: &RuntimeTask) -> usize {
    let id = std::mem::size_of::<RuntimeTaskId>();
    let entry = id + std::mem::size_of::<u64>();
    // The lazy wait/debug boxes themselves (willow-ezs.3): a task that never
    // waits and is never tagged pays for neither.
    let wait_box = if task.owns_wait_links() {
        std::mem::size_of::<crate::task::TaskWaitLinks>()
    } else {
        0
    };
    let debug_box = if task.owns_debug_info() {
        std::mem::size_of::<crate::task::TaskDebugInfo>()
    } else {
        0
    };
    let waiters = task.queued_waiter_entries() * entry + task.waiter_count() * entry;
    let awaiting = task.awaiting_count() * id;
    let channels = std::mem::size_of_val(task.wait_channels());
    let preempt = std::mem::size_of::<std::sync::atomic::AtomicBool>();
    let name = task.name().map(|name| name.len()).unwrap_or(0);
    let spawn_site = task.spawn_site().map(|(file, _)| file.len()).unwrap_or(0);
    wait_box + debug_box + waiters + awaiting + channels + preempt + name + spawn_site
}

fn average_task_bytes(scheduler: &RuntimeScheduler) -> usize {
    let inline = std::mem::size_of::<RuntimeTask>();
    if scheduler.task_count() == 0 {
        return inline;
    }
    let metadata: usize = scheduler
        .tasks()
        .map(|task| task_metadata_bytes(&task))
        .sum();
    inline + metadata / scheduler.task_count()
}

/// Fan-in registration: every task awaits the same task (willow-ezs.2).
#[test]
#[ignore = "scheduler scaling measurement"]
fn scaling_01_fan_in_registration() {
    // Serialize against the other measurement runs so the timings are not
    // taken while another test is competing for the same allocator.
    let _guard = runtime_test_guard();
    header();
    for tasks in TASK_COUNTS {
        let mut scheduler = RuntimeScheduler::with_worker_count(1);
        let awaitee = scheduler.spawn_parked_placeholder();
        let waiters: Vec<RuntimeTaskId> = (0..tasks)
            .map(|_| scheduler.spawn_parked_placeholder())
            .collect();

        let start = std::time::Instant::now();
        for &waiter in &waiters {
            scheduler.register_waiter(awaitee, waiter);
        }
        let elapsed = start.elapsed();

        assert_eq!(scheduler.task(awaitee).unwrap().waiter_count(), tasks);
        report("fan_in_registration", tasks, elapsed, 0);
    }
}

/// Completing the awaited task detaches and wakes every waiter.
#[test]
#[ignore = "scheduler scaling measurement"]
fn scaling_02_fan_in_completion() {
    // Serialize against the other measurement runs so the timings are not
    // taken while another test is competing for the same allocator.
    let _guard = runtime_test_guard();
    header();
    for tasks in TASK_COUNTS {
        let mut scheduler = RuntimeScheduler::with_worker_count(1);
        let awaitee = scheduler.spawn_parked_placeholder();
        for _ in 0..tasks {
            let waiter = scheduler.spawn_parked_placeholder();
            scheduler.register_waiter(awaitee, waiter);
        }

        let start = std::time::Instant::now();
        scheduler.complete(awaitee);
        let elapsed = start.elapsed();

        assert!(scheduler.task(awaitee).is_none());
        report("fan_in_completion", tasks, elapsed, 0);
    }
}

/// Losing `select` join arms register and unregister forever. This is the path
/// where waiter tombstones could otherwise accumulate.
#[test]
#[ignore = "scheduler scaling measurement"]
fn scaling_03_losing_select_arm_churn() {
    // Serialize against the other measurement runs so the timings are not
    // taken while another test is competing for the same allocator.
    let _guard = runtime_test_guard();
    header();
    for tasks in TASK_COUNTS {
        let mut scheduler = RuntimeScheduler::with_worker_count(1);
        let awaitee = scheduler.spawn_parked_placeholder();
        let resident = scheduler.spawn_parked_placeholder();
        scheduler.register_waiter(awaitee, resident);
        let churner = scheduler.spawn_parked_placeholder();

        let start = std::time::Instant::now();
        for _ in 0..tasks {
            scheduler.register_waiter(awaitee, churner);
            scheduler.unregister_waiter(awaitee, churner);
        }
        let elapsed = start.elapsed();

        let awaitee_task = scheduler.task(awaitee).unwrap();
        assert_eq!(awaitee_task.waiter_count(), 1);
        assert!(awaitee_task.queued_waiter_entries() <= 16);
        report("select_arm_churn", tasks, elapsed, 0);
    }
}

/// Terminating waiters one at a time, each deregistering itself from the task
/// it parked on.
#[test]
#[ignore = "scheduler scaling measurement"]
fn scaling_04_waiter_teardown() {
    // Serialize against the other measurement runs so the timings are not
    // taken while another test is competing for the same allocator.
    let _guard = runtime_test_guard();
    header();
    for tasks in TASK_COUNTS {
        let mut scheduler = RuntimeScheduler::with_worker_count(1);
        let awaitee = scheduler.spawn_parked_placeholder();
        let waiters: Vec<RuntimeTaskId> = (0..tasks)
            .map(|_| {
                let waiter = scheduler.spawn_parked_placeholder();
                scheduler.register_waiter(awaitee, waiter);
                waiter
            })
            .collect();

        let start = std::time::Instant::now();
        for &waiter in &waiters {
            scheduler.finalize_cancelled(waiter);
        }
        let elapsed = start.elapsed();

        assert_eq!(scheduler.task(awaitee).unwrap().waiter_count(), 0);
        report("waiter_teardown", tasks, elapsed, 0);
    }
}

/// Metadata footprint of simultaneously active tasks, by workload
/// (willow-ezs.3). The `bytes_per_task` column is the number to beat.
#[test]
#[ignore = "scheduler scaling measurement"]
fn scaling_05_active_task_footprint_by_workload() {
    // Serialize against the other measurement runs so the timings are not
    // taken while another test is competing for the same allocator.
    let _guard = runtime_test_guard();
    header();
    for tasks in TASK_COUNTS {
        // Ready: the cheapest active task, no wait relationships at all.
        let mut scheduler = RuntimeScheduler::with_worker_count(1);
        let start = std::time::Instant::now();
        for _ in 0..tasks {
            scheduler.spawn_placeholder();
        }
        report(
            "active_ready_tasks",
            tasks,
            start.elapsed(),
            average_task_bytes(&scheduler),
        );

        // Completion-waiting: every task carries a waiter relationship.
        let mut scheduler = RuntimeScheduler::with_worker_count(1);
        let awaitee = scheduler.spawn_parked_placeholder();
        let start = std::time::Instant::now();
        for _ in 0..tasks {
            let waiter = scheduler.spawn_parked_placeholder();
            scheduler.register_waiter(awaitee, waiter);
        }
        report(
            "active_completion_waiting_tasks",
            tasks,
            start.elapsed(),
            average_task_bytes(&scheduler),
        );

        // Channel-waiting: every task carries a channel reverse reference.
        let mut scheduler = RuntimeScheduler::with_worker_count(1);
        let start = std::time::Instant::now();
        for index in 0..tasks {
            let id = scheduler.spawn_parked_placeholder();
            scheduler.with_task_mut(id, |task| {
                task.add_wait_channel(0x1000 + index);
            });
        }
        report(
            "active_channel_waiting_tasks",
            tasks,
            start.elapsed(),
            average_task_bytes(&scheduler),
        );

        // Debug-generated: name and spawn site are populated, as they are for
        // async fns compiled in debug mode.
        let mut scheduler = RuntimeScheduler::with_worker_count(1);
        let start = std::time::Instant::now();
        for index in 0..tasks {
            let id = scheduler.spawn_parked_placeholder();
            scheduler.with_task_mut(id, |task| {
                task.set_name(format!("task_{index}"));
                task.set_spawn_site(String::from("src/main.wi"), index as u32);
            });
        }
        report(
            "active_debug_tagged_tasks",
            tasks,
            start.elapsed(),
            average_task_bytes(&scheduler),
        );
    }
}

/// Heap the allocator was actually asked for by N simultaneously active tasks
/// (willow-ezs.3).
///
/// [`scaling_05_active_task_footprint_by_workload`] estimates a task's metadata
/// from what the task structurally owns; this one measures. The two disagree by
/// design — the estimate cannot see hash bucket arrays, `String` capacity,
/// boxed allocations, or the scheduler's own task table — and the measured number
/// is the one the acceptance criterion is written against, because "the split
/// lowers total allocated bytes" is a claim about the allocator, not about
/// `size_of`.
///
/// Run alone (`--test-threads=1`): the counter is process-global, so a test
/// allocating on another thread would land inside the measurement window.
#[test]
#[ignore = "scheduler footprint measurement"]
fn scaling_07_measured_heap_per_active_task() {
    let _guard = runtime_test_guard();
    println!("case,tasks,heap_bytes,heap_bytes_per_task,allocs_per_task,inline_bytes_per_task");

    /// Build a scheduler and report (heap bytes, allocation count) that were
    /// live while it was, measured across the whole build.
    fn measure(build: impl FnOnce() -> RuntimeScheduler) -> (usize, usize) {
        let bytes_before = counting_allocator::live_bytes();
        let allocs_before = counting_allocator::allocations();
        let scheduler = build();
        let bytes = counting_allocator::live_bytes().saturating_sub(bytes_before);
        let allocs = counting_allocator::allocations() - allocs_before;
        drop(scheduler);
        (bytes, allocs)
    }

    fn report(case: &str, tasks: usize, measured: (usize, usize)) -> usize {
        let (bytes, allocs) = measured;
        let inline = std::mem::size_of::<RuntimeTask>();
        let per_task = bytes / tasks;
        println!(
            "{case},{tasks},{bytes},{per_task},{:.2},{inline}",
            allocs as f64 / tasks as f64
        );
        per_task
    }

    for tasks in TASK_COUNTS {
        let ready = report(
            "measured_active_ready",
            tasks,
            measure(|| {
                let mut scheduler = RuntimeScheduler::with_worker_count(1);
                for _ in 0..tasks {
                    scheduler.spawn_placeholder();
                }
                scheduler
            }),
        );

        let waiting = report(
            "measured_active_completion_waiting",
            tasks,
            measure(|| {
                let mut scheduler = RuntimeScheduler::with_worker_count(1);
                let awaitee = scheduler.spawn_parked_placeholder();
                for _ in 0..tasks {
                    let waiter = scheduler.spawn_parked_placeholder();
                    scheduler.register_waiter(awaitee, waiter);
                }
                scheduler
            }),
        );

        let channels = report(
            "measured_active_channel_waiting",
            tasks,
            measure(|| {
                let mut scheduler = RuntimeScheduler::with_worker_count(1);
                for index in 0..tasks {
                    let id = scheduler.spawn_parked_placeholder();
                    scheduler.with_task_mut(id, |task| {
                        task.add_wait_channel(0x1000 + index);
                    });
                }
                scheduler
            }),
        );

        let tagged = report(
            "measured_active_debug_tagged",
            tasks,
            measure(|| {
                let mut scheduler = RuntimeScheduler::with_worker_count(1);
                for index in 0..tasks {
                    let id = scheduler.spawn_parked_placeholder();
                    scheduler.with_task_mut(id, |task| {
                        task.set_name(format!("task_{index}"));
                        task.set_spawn_site(String::from("src/main.wi"), index as u32);
                    });
                }
                scheduler
            }),
        );

        // Machine-independent: a task with no relationships and no tags must
        // allocate strictly less than one that has them, which is exactly the
        // property the lazy split buys and the old inline layout did not have.
        assert!(
            ready < waiting,
            "a ready task must not pay for wait links it does not own ({ready} vs {waiting})"
        );
        assert!(
            ready < channels,
            "a ready task must not pay for a channel wait it does not own ({ready} vs {channels})"
        );
        assert!(
            ready < tagged,
            "a ready task must not pay for debug metadata it does not own ({ready} vs {tagged})"
        );
    }
}

/// Sustained throughput of short tasks: spawn, claim, complete, reap.
#[test]
#[ignore = "scheduler scaling measurement"]
fn scaling_06_short_task_throughput() {
    // Serialize against the other measurement runs so the timings are not
    // taken while another test is competing for the same allocator.
    let _guard = runtime_test_guard();
    header();
    for tasks in TASK_COUNTS {
        let mut scheduler = RuntimeScheduler::with_worker_count(1);
        let start = std::time::Instant::now();
        for _ in 0..tasks {
            scheduler.spawn_placeholder();
        }
        while let Some(id) = scheduler.pop_ready() {
            scheduler.complete(id);
            scheduler.clear_running();
        }
        let elapsed = start.elapsed();

        assert!(
            scheduler.take_pending_terminal_cleanups().is_empty(),
            "bookkeeping placeholders have no external registrations to purge"
        );
        assert_eq!(scheduler.metadata_snapshot().heavy_tasks, 0);
        report("short_task_throughput", tasks, elapsed, 0);
    }
}

// ─── Global-lock contention profiles (willow-ezs.4) ─────────────────────────
//
// These profiles were introduced while every scheduler operation ran inside
// one process-global `Mutex<RuntimeScheduler>`. They now exercise the extracted
// run queues and task-table shards directly, so the before/after record shows
// what the lock decomposition changed without wrapping the new paths in a
// synthetic outer mutex.
//
// Each case reports operations per second for 1, 2, 4, and 8 threads sharing
// one scheduler. Perfect scaling would multiply the single-thread rate by the
// thread count; a flat or falling column is contention. The `ns_per_task`
// column is per-operation latency including lock acquisition.
//
// Run:
//
// ```shell
// cargo test -p willow_runtime --release contention_ -- --ignored --nocapture --test-threads=1
// ```
//
// Checked-in 8-thread scaling record (aggregate throughput / 1-thread
// throughput, 20,000 operations per thread):
//
// ```text
// case                    before global mutex   after queue/table shards
// spawn_only                         0.57x                 0.88x
// spawn_claim_complete               0.35x                 1.05x
// wake_only                          0.29x                 0.67x
// private_scheduler_control          5.68x                 2.50x
// ```
//
// The after column was recorded on 2026-07-26 with the command above. It
// profiles the extracted queue/task-table operations directly: no synthetic
// outer `Mutex<RuntimeScheduler>` remains around those cases.

const CONTENTION_THREADS: [usize; 4] = [1, 2, 4, 8];
const OPS_PER_THREAD: usize = 20_000;

fn contention_header() {
    println!("case,threads,total_ops,elapsed_ms,ns_per_op,ops_per_sec,scaling_vs_1_thread");
}

fn contention_report(
    case: &str,
    threads: usize,
    ops: usize,
    elapsed: std::time::Duration,
    base: f64,
) -> f64 {
    let seconds = elapsed.as_secs_f64();
    let per_sec = ops as f64 / seconds;
    let scaling = if base > 0.0 { per_sec / base } else { 1.0 };
    println!(
        "{case},{threads},{ops},{:.3},{:.1},{:.0},{scaling:.2}",
        seconds * 1_000.0,
        elapsed.as_nanos() as f64 / ops as f64,
        per_sec
    );
    per_sec
}

/// Threads that only spawn into the sharded table and shared overflow queue.
#[test]
#[ignore = "scheduler lock contention profile"]
fn contention_01_spawn_only() {
    let _guard = runtime_test_guard();
    contention_header();
    let mut base = 0.0;
    for threads in CONTENTION_THREADS {
        let tasks = ShardedTaskTable::new();
        let queues = RunQueues::new(8);
        let next_id = std::sync::atomic::AtomicU64::new(1);
        let start = std::time::Instant::now();
        std::thread::scope(|scope| {
            for _ in 0..threads {
                scope.spawn(|| {
                    for _ in 0..OPS_PER_THREAD {
                        let id = next_id.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                        let task = RuntimeTask::new(id);
                        assert!(task.state.claim_queue_slot());
                        tasks.insert(id, task);
                        queues.push_global(id);
                    }
                });
            }
        });
        let ops = threads * OPS_PER_THREAD;
        let rate = contention_report("spawn_only", threads, ops, start.elapsed(), base);
        if threads == 1 {
            base = rate;
        }
    }
}

/// Threads that insert, publish locally, atomically claim, and reap through
/// independent task/queue shards.
#[test]
#[ignore = "scheduler lock contention profile"]
fn contention_02_spawn_claim_complete() {
    let _guard = runtime_test_guard();
    contention_header();
    let mut base = 0.0;
    for threads in CONTENTION_THREADS {
        let tasks = ShardedTaskTable::new();
        let queues = RunQueues::new(8);
        let next_id = std::sync::atomic::AtomicU64::new(1);
        let start = std::time::Instant::now();
        std::thread::scope(|scope| {
            for worker in 0..threads {
                let tasks = &tasks;
                let queues = &queues;
                let next_id = &next_id;
                scope.spawn(move || {
                    for _ in 0..OPS_PER_THREAD {
                        let id = next_id.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                        let task = RuntimeTask::new(id);
                        assert!(task.state.claim_queue_slot());
                        tasks.insert(id, task);
                        queues.push_local(worker, id);
                        let popped = queues.pop_for_worker(worker).unwrap();
                        tasks.with_mut(popped, |task| {
                            assert_eq!(task.state.claim_for_poll(), ClaimOutcome::Poll);
                            assert!(task.state.finish_terminal());
                        });
                        tasks.remove(popped);
                    }
                });
            }
        });
        let ops = threads * OPS_PER_THREAD;
        let rate = contention_report("spawn_claim_complete", threads, ops, start.elapsed(), base);
        if threads == 1 {
            base = rate;
        }
    }
}

/// Threads that wake/claim/park through the real sharded state + independent
/// queue locks, with no outer scheduler mutex.
#[test]
#[ignore = "scheduler lock contention profile"]
fn contention_03_wake_only() {
    let _guard = runtime_test_guard();
    contention_header();
    let mut base = 0.0;
    for threads in CONTENTION_THREADS {
        let tasks = ShardedTaskTable::new();
        let queues = RunQueues::new(8);
        let ids: Vec<RuntimeTaskId> = (1..=threads as u64).collect();
        for &id in &ids {
            let task = RuntimeTask::new(id);
            assert!(task.state.claim_queue_slot());
            assert_eq!(task.state.claim_for_poll(), ClaimOutcome::Poll);
            assert_eq!(task.state.park_after_poll(), BoundaryOutcome::Suspended);
            tasks.insert(id, task);
        }
        let start = std::time::Instant::now();
        std::thread::scope(|scope| {
            for (worker, &id) in ids.iter().enumerate() {
                let tasks = &tasks;
                let queues = &queues;
                scope.spawn(move || {
                    for _ in 0..OPS_PER_THREAD {
                        let outcome = tasks.with_mut(id, |task| task.state.wake()).unwrap();
                        if outcome == WakeOutcome::Enqueue {
                            queues.push_local(worker, id);
                        }
                        let Some(popped) = queues.pop_for_worker(worker) else {
                            continue;
                        };
                        tasks.with_mut(popped, |task| {
                            assert_eq!(task.state.claim_for_poll(), ClaimOutcome::Poll);
                            assert_eq!(task.state.park_after_poll(), BoundaryOutcome::Suspended);
                        });
                    }
                });
            }
        });
        let ops = threads * OPS_PER_THREAD;
        let rate = contention_report("wake_only", threads, ops, start.elapsed(), base);
        if threads == 1 {
            base = rate;
        }
    }
}

/// A control measurement: the same operation count against a per-thread
/// scheduler, i.e. what the workload could reach if the global lock were not in
/// the way. The gap between this and `contention_01` is the headroom the lock
/// decomposition is competing for.
#[test]
#[ignore = "scheduler lock contention profile"]
fn contention_04_uncontended_control() {
    let _guard = runtime_test_guard();
    contention_header();
    let mut base = 0.0;
    for threads in CONTENTION_THREADS {
        let start = std::time::Instant::now();
        std::thread::scope(|scope| {
            for _ in 0..threads {
                scope.spawn(|| {
                    let mut sched = RuntimeScheduler::with_worker_count(8);
                    for _ in 0..OPS_PER_THREAD {
                        sched.spawn_parked_placeholder();
                    }
                });
            }
        });
        let ops = threads * OPS_PER_THREAD;
        let rate = contention_report(
            "private_scheduler_control",
            threads,
            ops,
            start.elapsed(),
            base,
        );
        if threads == 1 {
            base = rate;
        }
    }
}
