use super::*;
use std::time::{Duration, Instant};

#[test]
fn sweep_allocation_runway_boundaries_and_saturating_clock() {
    let _guard = runtime_test_guard();
    reset_internal_for_test();
    let mut state = runtime().heap.lock().unwrap();
    for start in [0, 17, u64::MAX - sweep::ALLOCATION_RUNWAY] {
        state.sweep_allocation_start = start;
        for debt in [0, 1, sweep::ALLOCATION_RUNWAY - 1, sweep::ALLOCATION_RUNWAY] {
            state.total_allocated_bytes = start + debt;
            state.sweeping = None;
            assert!(!sweep::allocation_budget_exhausted(&state));
            state.sweeping = Some(std::thread::current().id());
            assert_eq!(
                sweep::allocation_budget_exhausted(&state),
                debt >= sweep::ALLOCATION_RUNWAY
            );
        }
    }
    state.sweeping = None;
    drop(state);
    reset_internal_for_test();
}

#[test]
fn sweep_allocation_waits_cooperatively_and_collector_is_reentrant() {
    let _guard = runtime_test_guard();
    for count in [1, 4, 16] {
        reset_internal_for_test();
        cycle_wait::SLEEPS.store(0, Ordering::Relaxed);
        cycle_wait::CHECKS.store(0, Ordering::Relaxed);
        let owner = cycle_wait::CollectorGuard::try_acquire().unwrap();
        {
            let mut state = runtime().heap.lock().unwrap();
            state.sweeping = Some(std::thread::current().id());
            state.sweep_allocation_start = 0;
            state.total_allocated_bytes = sweep::ALLOCATION_RUNWAY;
        }
        // Native collector callbacks retain the non-waiting allocation path.
        assert!(!allocate_old(0, 0, 8192, 0).is_null());
        let workers: Vec<_> = (0..count)
            .map(|i| {
                std::thread::spawn(move || {
                    let _registration = MutatorRegistration::new();
                    let size = if i % 2 == 0 {
                        8192
                    } else {
                        GC_LARGE_OBJECT_THRESHOLD + 1
                    };
                    let object = allocate_old(0, 0, size as i64, 0);
                    assert!(!object.is_null());
                    unsafe {
                        *object = 42;
                    }
                })
            })
            .collect();
        let deadline = Instant::now() + Duration::from_secs(10);
        while cycle_wait::SLEEPS.load(Ordering::Acquire) < count {
            assert!(Instant::now() < deadline, "allocators did not join sweep");
            std::thread::yield_now();
        }
        assert!(workers.iter().all(|worker| !worker.is_finished()));
        // A sleeping allocation still participates in subsequent stop rounds.
        with_stw(crate::gc_telemetry::stops::StopReason::Minor, |coord, _| {
            assert_eq!(coord.parked.len(), count);
        });
        runtime().heap.lock().unwrap().sweeping = None;
        drop(owner);
        for worker in workers {
            worker.join().unwrap();
        }
        println!(
            "sweep waiters={count} condition_checks={} sleeps={}",
            cycle_wait::CHECKS.load(Ordering::Relaxed),
            cycle_wait::SLEEPS.load(Ordering::Relaxed)
        );
        willow_gc_collect();
        assert_eq!(willow_gc_allocated_bytes(), 0);
    }
    reset_internal_for_test();
}

#[test]
fn large_runtime_allocations_service_initial_handshake_without_loop_polls() {
    let _guard = runtime_test_guard();
    reset_internal_for_test();
    let ready = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let done = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let worker = {
        let ready = ready.clone();
        let done = done.clone();
        std::thread::spawn(move || {
            let _registration = MutatorRegistration::new();
            let mut object = std::ptr::null_mut();
            willow_push_root(&mut object);
            ready.store(true, Ordering::Release);
            while !done.load(Ordering::Acquire) {
                object = allocate_old(0, 0, 8192, 0);
                unsafe {
                    *object = 42;
                }
            }
            assert_eq!(unsafe { *object }, 42);
            willow_pop_root();
        })
    };
    while !ready.load(Ordering::Acquire) {
        std::thread::yield_now();
    }
    // No generated loop poll or mark assist in the worker: only allocation.
    for _ in 0..3 {
        willow_gc_collect();
    }
    done.store(true, Ordering::Release);
    worker.join().unwrap();
    willow_gc_collect();
    assert_eq!(willow_gc_allocated_bytes(), 0);
    reset_internal_for_test();
}

#[test]
fn sweep_allocation_shared_runway_scales_with_bytes_not_waiter_count() {
    let _guard = runtime_test_guard();
    for size in [8192, 65536, 262144, 2 * 1024 * 1024] {
        for count in [1, 4, 16] {
            reset_internal_for_test();
            cycle_wait::SLEEPS.store(0, Ordering::Relaxed);
            let owner = cycle_wait::CollectorGuard::try_acquire().unwrap();
            {
                let mut state = runtime().heap.lock().unwrap();
                state.sweeping = Some(std::thread::current().id());
                state.sweep_allocation_start = state.total_allocated_bytes;
            }
            let done = Arc::new(std::sync::atomic::AtomicBool::new(false));
            let workers: Vec<_> = (0..count)
                .map(|_| {
                    let done = done.clone();
                    std::thread::spawn(move || {
                        let _registration = MutatorRegistration::new();
                        while !done.load(Ordering::Acquire) {
                            assert!(!allocate_old(0, 0, size as i64, 0).is_null());
                        }
                    })
                })
                .collect();
            let deadline = Instant::now() + Duration::from_secs(10);
            while cycle_wait::SLEEPS.load(Ordering::Acquire) < count {
                assert!(Instant::now() < deadline);
                std::thread::yield_now();
            }
            let state = runtime().heap.lock().unwrap();
            let consumed = state.total_allocated_bytes - state.sweep_allocation_start;
            assert!(consumed >= sweep::ALLOCATION_RUNWAY);
            assert!(consumed < sweep::ALLOCATION_RUNWAY + size as u64 + GC_HEADER_SIZE as u64 + 8);
            println!("sweep size={size} waiters={count} admitted_bytes={consumed}");
            drop(state);
            done.store(true, Ordering::Release);
            runtime().heap.lock().unwrap().sweeping = None;
            drop(owner);
            for worker in workers {
                worker.join().unwrap();
            }
        }
    }
    reset_internal_for_test();
}

#[test]
fn large_allocation_bounds_unelected_request_runway() {
    let _guard = runtime_test_guard();
    reset_internal_for_test();
    // Model a delayed coordinator without depending on OS scheduling timing.
    // The allocation slow path must start a collector without another request.
    let size = 65536;
    let budget = 1024 * 1024;
    for _ in 0..40 {
        let mut state = runtime().heap.lock().unwrap();
        state.threshold_bytes = usize::MAX;
        drop(state);
        assert!(!allocate_old(0, 0, size, 0).is_null());
    }
    runtime().heap.lock().unwrap().threshold_bytes = budget;
    let before = runtime().heap.lock().unwrap().major_collections;
    let _registration = MutatorRegistration::new();
    assert!(!allocate_old(0, 0, size, 0).is_null());
    // Initial publication is sufficient to return; collection may still be
    // marking or sweeping. Cooperatively finish before checking reclamation.
    coordinator::shutdown();
    assert!(runtime().heap.lock().unwrap().major_collections > before);
    assert!(willow_gc_allocated_bytes() < size * 2);
    drop(_registration);
    reset_internal_for_test();
}

static ALLOCATION_RELEASED_TRACE: std::sync::atomic::AtomicBool =
    std::sync::atomic::AtomicBool::new(false);
static ALLOCATION_TRACE_RETRIES: AtomicUsize = AtomicUsize::new(0);
static ELECTED_BEFORE_HANDSHAKE: std::sync::atomic::AtomicBool =
    std::sync::atomic::AtomicBool::new(false);

unsafe fn allocation_retry_legacy(payload: *mut u8, slots: &mut Vec<*mut *mut u8>) {
    assert!(
        ALLOCATION_RELEASED_TRACE.load(Ordering::Acquire),
        "allocator cannot synchronously finish this trace"
    );
    slots.push(payload.cast());
}

unsafe fn allocation_retry_slice(
    payload: *mut u8,
    _: usize,
    _: usize,
    children: &mut Vec<*mut u8>,
) -> TraceSliceProgress {
    if !ALLOCATION_RELEASED_TRACE.load(Ordering::Acquire) {
        ALLOCATION_TRACE_RETRIES.fetch_add(1, Ordering::Relaxed);
        return TraceSliceProgress::Retry;
    }
    children.push(unsafe { *(payload as *mut *mut u8) });
    TraceSliceProgress::Done
}

fn hold_election_until_allocation_joins() {
    ELECTED_BEFORE_HANDSHAKE.store(true, Ordering::Release);
    let deadline = Instant::now() + Duration::from_secs(10);
    while cycle_wait::INITIAL_ALLOCATION_WAITS.load(Ordering::Acquire) == 0
        && cycle_wait::SLEEPS.load(Ordering::Acquire) == 0
    {
        assert!(
            Instant::now() < deadline,
            "allocation did not join initial publication"
        );
        std::thread::yield_now();
    }
    cycle_wait::BEFORE_INITIAL_HANDSHAKE.set(None);
}

fn allocation_retry_before_initial_handshake(elected: bool) {
    let _guard = runtime_test_guard();
    reset_internal_for_test();
    ALLOCATION_RELEASED_TRACE.store(false, Ordering::Relaxed);
    ALLOCATION_TRACE_RETRIES.store(0, Ordering::Relaxed);
    ELECTED_BEFORE_HANDSHAKE.store(false, Ordering::Relaxed);
    cycle_wait::INITIAL_ALLOCATION_WAITS.store(0, Ordering::Relaxed);
    cycle_wait::SLEEPS.store(0, Ordering::Relaxed);
    const TYPE: u32 = 0xFA9B;
    willow_register_type_capability(
        TYPE,
        allocation_retry_legacy,
        NativeTraceCapability::SliceConcurrentSafe(allocation_retry_slice),
    );
    let registration = MutatorRegistration::new();
    runtime().heap.lock().unwrap().threshold_bytes = usize::MAX;
    let mut parent = allocate_old(0, TYPE, 8, 0);
    willow_push_root(&mut parent);
    let child = allocate_old(0, 0, 8, 0);
    unsafe {
        *child = 42;
        *(parent as *mut *mut u8) = child;
    }
    assert!(!allocate_old(0, 0, 3 * 1024 * 1024, 0).is_null());
    runtime().heap.lock().unwrap().threshold_bytes = 1024 * 1024;
    let collector = elected.then(|| {
        std::thread::spawn(|| {
            let _registration = MutatorRegistration::new();
            cycle_wait::BEFORE_INITIAL_HANDSHAKE.set(Some(hold_election_until_allocation_joins));
            collect_internal();
        })
    });
    if elected {
        let deadline = Instant::now() + Duration::from_secs(10);
        while !ELECTED_BEFORE_HANDSHAKE.load(Ordering::Acquire) {
            assert!(Instant::now() < deadline);
            std::thread::yield_now();
        }
        assert!(runtime().coord.0.lock().unwrap().cycle_collector.is_some());
    } else {
        assert!(runtime().coord.0.lock().unwrap().cycle_collector.is_none());
    }
    assert!(runtime().heap.lock().unwrap().concurrent_cycle.is_none());
    // This would self-elect or join whole-cycle completion in the buggy
    // implementation. Trace progress is impossible until it returns.
    let object = allocate_old(0, 0, 65536, 0);
    assert!(!object.is_null());
    assert!(ALLOCATION_TRACE_RETRIES.load(Ordering::Acquire) > 0);
    assert!(!ALLOCATION_RELEASED_TRACE.swap(true, Ordering::AcqRel));
    if let Some(collector) = collector {
        let deadline = Instant::now() + Duration::from_secs(10);
        while !collector.is_finished() {
            willow_gc_safepoint();
            assert!(Instant::now() < deadline);
            std::thread::yield_now();
        }
        collector.join().unwrap();
    } else {
        coordinator::shutdown();
    }
    assert!(runtime().heap.lock().unwrap().major_collections > 0);
    assert_eq!(unsafe { **(parent as *mut *mut u8) }, 42);
    println!(
        "initial_retry elected={elected} retries={} allocation_returned_before_release=true",
        ALLOCATION_TRACE_RETRIES.load(Ordering::Relaxed)
    );
    willow_pop_root();
    drop(registration);
    willow_gc_collect();
    assert_eq!(willow_gc_allocated_bytes(), 0);
    reset_internal_for_test();
}

#[test]
fn large_allocation_retry_progress_without_an_elected_collector() {
    allocation_retry_before_initial_handshake(false);
}

#[test]
fn large_allocation_retry_progress_joining_elected_before_initial_handshake() {
    allocation_retry_before_initial_handshake(true);
}

#[test]
fn stale_sweep_budget_does_not_join_a_later_mark() {
    let _guard = runtime_test_guard();
    reset_internal_for_test();
    let old = cycle_wait::CollectorGuard::try_acquire().unwrap();
    {
        let mut state = runtime().heap.lock().unwrap();
        state.sweeping = Some(std::thread::current().id());
        state.total_allocated_bytes = sweep::ALLOCATION_RUNWAY;
        assert!(sweep::allocation_budget_exhausted(&state));
        // Model completion after allocate_old observed the predicate but
        // before its waiter acquires coord, followed by a new mark election.
        state.sweeping = None;
    }
    drop(old);
    let next = cycle_wait::CollectorGuard::try_acquire().unwrap();
    let (tx, rx) = std::sync::mpsc::channel();
    let worker = std::thread::spawn(move || {
        let _registration = MutatorRegistration::new();
        tx.send(cycle_wait::wait_for_sweep_budget()).unwrap();
    });
    let result = rx.recv_timeout(Duration::from_secs(5));
    drop(next);
    worker.join().unwrap();
    assert!(
        !result.unwrap(),
        "stale sweep observation joined a new collector"
    );
    reset_internal_for_test();
}
