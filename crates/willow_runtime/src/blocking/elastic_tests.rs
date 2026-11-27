use super::test_support::wait_until;
use super::*;
use std::sync::mpsc;

// Unlike recycled scheduler fixtures, each configuration needs a fresh OnceLock.
struct FreshRuntime(&'static crate::runtime::Runtime);
impl Drop for FreshRuntime {
    fn drop(&mut self) {
        self.0.select_for_thread();
    }
}
fn fresh_runtime() -> FreshRuntime {
    let previous = crate::runtime::current();
    let owner = Box::leak(Box::new(crate::runtime::Runtime::new(
        crate::scheduler::environment_worker_config,
    )));
    owner.select_for_thread();
    FreshRuntime(previous)
}

fn pool(limit: usize, capacity: usize, timeout: Duration) -> &'static BlockingPool {
    let owner = crate::runtime::current();
    let mut pool =
        BlockingPool::with_workers(limit, capacity, "willow-blocking", owner, PoolKind::General);
    pool.idle_timeout = timeout;
    assert!(owner.blocking.general.set(pool).is_ok());
    owner.blocking.general.get().unwrap()
}

fn stall(pool: &BlockingPool) -> mpsc::Sender<()> {
    let (release, gate) = mpsc::channel();
    let (started, running) = mpsc::channel();
    assert!(
        pool.try_submit(Box::new(move || {
            started.send(()).unwrap();
            let _ = gate.recv();
        }))
        .is_ok()
    );
    running.recv_timeout(Duration::from_secs(5)).unwrap();
    release
}

#[test]
fn elastic_growth_retirement_and_restart_counts() {
    for limit in [1, 4, 8, 32] {
        let _runtime = fresh_runtime();
        let timeout = Duration::from_millis(20);
        // One queued slot must still permit reaching every configured limit.
        let pool = pool(limit, 1, timeout);
        assert_eq!(willow_blocking_current_threads(), 0);
        for round in 1..=3 {
            let mut gates = Vec::new();
            for expected in 1..=limit {
                gates.push(stall(pool));
                assert_eq!(willow_blocking_current_threads(), expected as i64);
                assert_eq!(willow_blocking_active_jobs(), expected as i64);
            }
            assert_eq!(pool.metrics.started.load(Ordering::Acquire), round * limit);
            let released = Instant::now();
            drop(gates);
            wait_until("all elastic workers retire", || {
                willow_blocking_current_threads() == 0
            });
            // The timer is the behavior under test; assert only a lower bound.
            assert!(released.elapsed() >= timeout);
            assert_eq!(willow_blocking_active_jobs(), 0);
            assert_eq!(willow_blocking_queued_jobs(), 0);
            assert_eq!(willow_blocking_completed_jobs(), (round * limit) as i64);
            assert_eq!(pool.metrics.retired.load(Ordering::Acquire), round * limit);
        }
        println!(
            "elastic limit={limit} rounds=3 starts={} retirements={} completed={}",
            pool.metrics.started.load(Ordering::Acquire),
            pool.metrics.retired.load(Ordering::Acquire),
            willow_blocking_completed_jobs()
        );
    }
}

#[test]
fn elastic_reuses_idle_workers_and_does_not_retire_busy_workers() {
    let _runtime = fresh_runtime();
    let pool = pool(4, 1, Duration::from_secs(30));
    let gate = stall(pool);
    drop(gate);
    wait_until("first job completes", || {
        willow_blocking_completed_jobs() == 1
    });
    let gate = stall(pool);
    assert_eq!(pool.metrics.started.load(Ordering::Acquire), 1);
    assert_eq!(willow_blocking_current_threads(), 1);
    drop(gate);

    let _runtime = fresh_runtime();
    let pool = self::pool(4, 1, Duration::from_millis(20));
    let mut gates: Vec<_> = (0..4).map(|_| stall(pool)).collect();
    let busy = gates.pop().unwrap();
    drop(gates);
    wait_until("only busy worker remains", || {
        willow_blocking_current_threads() == 1
    });
    assert_eq!(willow_blocking_active_jobs(), 1);
    assert_eq!(willow_blocking_completed_jobs(), 3);
    // Idle peers have exited; new work must grow while the old job stays blocked.
    let new = stall(pool);
    assert_eq!(willow_blocking_current_threads(), 2);
    drop((busy, new));
    wait_until("remaining workers retire", || {
        willow_blocking_current_threads() == 0
    });
    assert_eq!(willow_blocking_completed_jobs(), 5);
}

#[test]
fn elastic_idle_notifications_do_not_extend_retirement_deadline() {
    let _runtime = fresh_runtime();
    let timeout = Duration::from_millis(20);
    let pool = pool(1, 1, timeout);
    let gate = stall(pool);
    let released = Instant::now();
    drop(gate);
    wait_until("retirement despite idle notifications", || {
        pool.work_ready.notify_all();
        willow_blocking_current_threads() == 0
    });
    assert!(released.elapsed() >= timeout);
    assert_eq!(willow_blocking_completed_jobs(), 1);
}

#[test]
fn elastic_concurrent_admission_and_retirement_never_strand_work() {
    for jobs in [32, 128, 512] {
        let _runtime = fresh_runtime();
        // Force the retirement/admission race whenever the queue empties.
        let pool = pool(8, 2, Duration::ZERO);
        let (done, results) = mpsc::channel();
        std::thread::scope(|scope| {
            for producer in 0..8 {
                let done = done.clone();
                scope.spawn(move || {
                    for index in 0..jobs / 8 {
                        let done = done.clone();
                        let mut pending: BlockingWork = Box::new(move || {
                            done.send(producer * (jobs / 8) + index).unwrap();
                        });
                        let deadline = Instant::now() + Duration::from_secs(10);
                        while let Err(work) = pool.try_submit(pending) {
                            assert!(Instant::now() < deadline, "admission stalled");
                            pending = work;
                            std::thread::yield_now();
                        }
                        let state = pool.lock();
                        assert!(state.busy <= state.threads && state.threads <= 8);
                        assert!(state.queue.len() <= 2);
                    }
                });
            }
        });
        let mut seen = vec![false; jobs];
        for _ in 0..jobs {
            let index = results.recv_timeout(Duration::from_secs(5)).unwrap();
            assert!(!std::mem::replace(&mut seen[index], true), "job ran twice");
        }
        wait_until("racing workers retire", || {
            willow_blocking_current_threads() == 0
        });
        assert_eq!(willow_blocking_completed_jobs(), jobs as i64);
        assert_eq!(pool.metrics.growth_checks.load(Ordering::Acquire), jobs);
        let starts = pool.metrics.started.load(Ordering::Acquire);
        assert!((1..=jobs).contains(&starts));
        assert_eq!(pool.metrics.retired.load(Ordering::Acquire), starts);
        println!(
            "elastic racing jobs={jobs} growth_checks={jobs} starts={starts} retirements={starts} completed={jobs}"
        );
    }
}

#[test]
fn elastic_configuration_is_a_limit_and_gauge_is_lazy() {
    let _runtime = fresh_runtime();
    assert_eq!(willow_blocking_current_threads(), 0);
    assert!(PoolKind::General.started().is_none());
    let pool = PoolKind::General.pool();
    assert_eq!(
        pool.thread_limit,
        willow_abi::settings::blocking_threads().unwrap_or(DEFAULT_BLOCKING_THREADS)
    );
    assert_eq!(willow_blocking_current_threads(), 0);
    assert_eq!(PoolKind::Stdout.pool().thread_limit, 1);
    assert_eq!(PoolKind::Stdout.pool().capacity, 1);
    assert_eq!(PoolKind::Foreign.pool().thread_limit, pool.thread_limit);
}

#[test]
fn elastic_retirement_preserves_reservations_and_reserved_submit_restarts() {
    use crate::scheduler::*;
    let _runtime = fresh_runtime();
    let pool = pool(1, 1, Duration::from_millis(20));
    unsafe extern "C" fn parked(_: *mut std::ffi::c_void) -> i32 {
        crate::task::RUNTIME_POLL_BLOCKED_SYSCALL
    }
    // SAFETY: the callback ignores its null frame and only parks.
    let task = unsafe { willow_sched_spawn(parked, std::ptr::null_mut()) };
    let gate = stall(pool);
    assert!(pool.try_submit(Box::new(|| {})).is_ok());
    let (done, result) = mpsc::channel();
    let pending = pool
        .try_submit_or_wait(task, Box::new(move || done.send(73).unwrap()))
        .expect_err("queue full");
    drop(gate);
    wait_until("worker retires with an unconsumed reservation", || {
        willow_blocking_current_threads() == 0
    });
    assert_eq!(willow_blocking_slot_waiters(), 1);
    assert!(
        pool.try_submit(Box::new(|| panic!("newcomer stole reservation")))
            .is_err()
    );
    assert!(pool.try_submit_or_wait(task, pending).is_ok());
    assert_eq!(result.recv_timeout(Duration::from_secs(5)).unwrap(), 73);
    wait_until("reserved job's replacement retires", || {
        willow_blocking_current_threads() == 0
    });
    assert_eq!(willow_blocking_slot_waiters(), 0);
    assert_eq!(willow_blocking_completed_jobs(), 3);
    willow_sched_cancel(task);
    willow_sched_run_until(task);
}
