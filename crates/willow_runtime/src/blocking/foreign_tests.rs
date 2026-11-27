use super::*;
use crate::scheduler::*;
use crate::test_sync::{SchedulerClock, Watchdog, drive_until, until};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Condvar, mpsc};
use std::time::Duration;

#[derive(Clone, Default)]
struct Fixture {
    gate: Arc<(Mutex<bool>, Condvar)>,
    started: Arc<AtomicUsize>,
    input_drops: Arc<AtomicUsize>,
    output_drops: Arc<AtomicUsize>,
    returned: Arc<AtomicUsize>,
}
static FIXTURE: crate::runtime::PerRuntime<Mutex<Fixture>> =
    crate::runtime::PerRuntime::new(|| Mutex::new(Fixture::default()));
struct Probe(Arc<AtomicUsize>);
impl Drop for Probe {
    fn drop(&mut self) {
        assert!(
            std::thread::current()
                .name()
                .unwrap_or("")
                .starts_with("willow-rust-blocking")
        );
        self.0.fetch_add(1, Ordering::SeqCst);
    }
}
unsafe extern "C" fn call(_: *mut std::ffi::c_void) -> i32 {
    let f = FIXTURE.lock().unwrap().clone();
    let input = Probe(f.input_drops.clone());
    let returned = f.returned.clone();
    let value = run(move |cancelled| {
        if cancelled {
            drop(input);
            return None;
        }
        f.started.fetch_add(1, Ordering::SeqCst);
        let (lock, ready) = &*f.gate;
        let mut open = lock.lock().unwrap();
        while !*open {
            let (next, timeout) = ready.wait_timeout(open, Duration::from_secs(30)).unwrap();
            open = next;
            assert!(!timeout.timed_out(), "foreign test gate hung");
        }
        drop(input);
        Some(Probe(f.output_drops.clone()))
    });
    assert!(value.is_none(), "cancelled task received a native result");
    returned.fetch_add(1, Ordering::SeqCst);
    crate::task::RUNTIME_POLL_READY
}
fn open(f: &Fixture) {
    *f.gate.0.lock().unwrap() = true;
    f.gate.1.notify_all();
}
fn stall(pool: &BlockingPool) -> Vec<mpsc::Sender<()>> {
    let mut gates = Vec::new();
    for _ in 0..super::super::test_support::pool_threads() {
        let (send, recv) = mpsc::channel();
        let (started, running) = mpsc::channel();
        let mut pending = Some(Box::new(move || {
            started.send(()).unwrap();
            let _ = recv.recv();
        }) as BlockingWork);
        until("foreign stall admission", || {
            match pool.try_submit(pending.take().unwrap()) {
                Ok(()) => true,
                Err(job) => {
                    pending = Some(job);
                    false
                }
            }
        });
        running.recv_timeout(Duration::from_secs(30)).unwrap();
        gates.push(send);
    }
    gates
}

#[test]
fn foreign_cancel_at_every_ownership_state_and_io_isolation() {
    let _guard = crate::gc::isolated_runtime();
    let _watchdog = Watchdog::new("foreign cancellation states");
    let clock = SchedulerClock::new();
    let pool = PoolKind::Foreign.pool();
    for mode in ["waiting", "queued", "running", "completed"] {
        reset_global_scheduler_for_test();
        let f = Fixture::default();
        *FIXTURE.lock().unwrap() = f.clone();
        let stalled = if matches!(mode, "waiting" | "queued") {
            stall(pool)
        } else {
            Vec::new()
        };
        if mode == "waiting" {
            for _ in 0..pool.capacity {
                assert!(pool.try_submit(Box::new(|| {})).is_ok());
            }
        }
        // SAFETY: this callback uses no managed frame and the fixture remains live.
        let task = unsafe { willow_sched_spawn(call, std::ptr::null_mut()) };
        drive_until("foreign task parks", &clock, || {
            willow_sched_task_state(task) == 7
                && match mode {
                    "waiting" => pool.metrics.waiters.load(Ordering::Acquire) == 1,
                    "queued" => pool.metrics.queued.load(Ordering::Acquire) == 1,
                    _ => f.started.load(Ordering::Acquire) == 1,
                }
        });
        assert!(pool.metrics.queued.load(Ordering::Acquire) <= pool.capacity);
        // A saturated Rust pool never occupies the general file-I/O pool.
        let (sent, received) = mpsc::channel();
        assert!(super::super::try_submit(Box::new(move || sent.send(()).unwrap())).is_ok());
        received.recv_timeout(Duration::from_secs(30)).unwrap();
        if mode == "completed" {
            open(&f);
            until("foreign completion published", || {
                pool.metrics.active.load(Ordering::Acquire) == 0
            });
            assert_eq!(f.input_drops.load(Ordering::Acquire), 1);
            assert_eq!(f.output_drops.load(Ordering::Acquire), 0);
        }
        let before = crate::test_sync::count("foreign cancellation observed");
        willow_sched_cancel(task);
        if mode != "completed" {
            drive_until("foreign cancellation observed", &clock, || {
                crate::test_sync::count("foreign cancellation observed") > before
            });
        }
        open(&f);
        drop(stalled);
        willow_sched_run_until(task);
        assert_eq!(f.returned.load(Ordering::Acquire), 1);
        assert_eq!(f.input_drops.load(Ordering::Acquire), 1);
        let expected = usize::from(matches!(mode, "running" | "completed"));
        assert_eq!(f.started.load(Ordering::Acquire), expected);
        assert_eq!(f.output_drops.load(Ordering::Acquire), expected);
        until("foreign pool drained", || {
            pool.metrics.active.load(Ordering::Acquire) == 0
                && pool.metrics.queued.load(Ordering::Acquire) == 0
        });
        assert_eq!(pool.metrics.waiters.load(Ordering::Acquire), 0);
        eprintln!(
            "state={mode} input_drops=1 body_runs={expected} output_drops={expected} waiters=0"
        );
    }
    reset_global_scheduler_for_test();
}

#[test]
fn foreign_backpressure_cancellation_counts_are_linear() {
    let _guard = crate::gc::isolated_runtime();
    let _watchdog = Watchdog::new("foreign cancellation scaling");
    let clock = SchedulerClock::new();
    let pool = PoolKind::Foreign.pool();
    for count in [1, 16, 64] {
        reset_global_scheduler_for_test();
        let f = Fixture::default();
        *FIXTURE.lock().unwrap() = f.clone();
        let stalled = stall(pool);
        for _ in 0..pool.capacity {
            assert!(pool.try_submit(Box::new(|| {})).is_ok());
        }
        let ids: Vec<_> = (0..count)
            .map(|_| {
                // SAFETY: call uses the runtime-local fixture and no frame storage.
                unsafe { willow_sched_spawn(call, std::ptr::null_mut()) }
            })
            .collect();
        drive_until("all foreign callers backpressured", &clock, || {
            pool.metrics.waiters.load(Ordering::Acquire) == count
                && ids.iter().all(|id| willow_sched_task_state(*id) == 7)
        });
        assert_eq!(pool.metrics.queued.load(Ordering::Acquire), pool.capacity);
        for &id in ids.iter().rev() {
            willow_sched_cancel(id);
        }
        drop(stalled);
        for id in ids {
            willow_sched_run_until(id);
        }
        assert_eq!(f.returned.load(Ordering::Acquire), count);
        assert_eq!(f.input_drops.load(Ordering::Acquire), count);
        assert_eq!(f.started.load(Ordering::Acquire), 0);
        assert_eq!(f.output_drops.load(Ordering::Acquire), 0);
        until("foreign scaling drain", || {
            pool.metrics.active.load(Ordering::Acquire) == 0
                && pool.metrics.queued.load(Ordering::Acquire) == 0
        });
        assert_eq!(pool.metrics.waiters.load(Ordering::Acquire), 0);
        eprintln!(
            "foreign callers={count} input_drops={count} body_runs=0 output_drops=0 waiters=0 queue_bound={}",
            pool.capacity
        );
    }
    reset_global_scheduler_for_test();
}
