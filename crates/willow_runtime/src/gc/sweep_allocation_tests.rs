use super::*;
use std::time::{Duration, Instant};

#[test]
fn sweep_allocation_runway_boundaries_and_saturating_clock() {
    let _guard = isolated_runtime();
    // SAFETY: The test controls its isolated runtime at this reset boundary; no other mutator
    // or subsequent access relies on the discarded heap.
    unsafe { reset_internal_for_test() };
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
    // SAFETY: The test controls its isolated runtime at this reset boundary; no other mutator
    // or subsequent access relies on the discarded heap.
    unsafe { reset_internal_for_test() };
}

#[test]
fn sweep_allocation_waits_cooperatively_and_collector_is_reentrant() {
    let _guard = isolated_runtime();
    for count in [1, 4, 16] {
        // SAFETY: The test controls its isolated runtime at this reset boundary; no other
        // mutator or subsequent access relies on the discarded heap.
        unsafe { reset_internal_for_test() };
        runtime()
            .wait_observation
            .sleeps
            .store(0, Ordering::Relaxed);
        runtime()
            .wait_observation
            .checks
            .store(0, Ordering::Relaxed);
        let owner = cycle_wait::CollectorGuard::try_acquire().unwrap();
        {
            let mut state = runtime().heap.lock().unwrap();
            state.sweeping = Some(std::thread::current().id());
            state.sweep_allocation_start = 0;
            state.total_allocated_bytes = sweep::ALLOCATION_RUNWAY;
        }
        // Native collector callbacks retain the non-waiting allocation path.
        // SAFETY: This fixture selects the payload size and tracing layout for its object and
        // initializes any native fields before collection.
        assert!(!unsafe { allocate_old(0, 0, 8192, 0) }.is_null());
        let registered = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let workers: Vec<_> = (0..count)
            .map(|i| {
                let registered = Arc::clone(&registered);
                std::thread::spawn(crate::runtime::serving(move || {
                    let _registration = MutatorRegistration::new();
                    registered.fetch_add(1, Ordering::Release);
                    let size = if i % 2 == 0 {
                        8192
                    } else {
                        GC_LARGE_OBJECT_THRESHOLD + 1
                    };
                    // SAFETY: This fixture selects the payload size and tracing layout for
                    // its object and initializes any native fields before collection.
                    let object = unsafe { allocate_old(0, 0, size as i64, 0) };
                    assert!(!object.is_null());
                    // SAFETY: The test retains this nonempty allocation and accesses a byte within
                    // its requested size.
                    unsafe {
                        *object = 42;
                    }
                }))
            })
            .collect();
        // Every allocator is registered and the sweep wait has slept at least
        // once per allocator. A condvar may wake spuriously, so the sleep
        // count is only a lower bound and does not identify each waiter.
        let deadline = Instant::now() + Duration::from_secs(10);
        while registered.load(Ordering::Acquire) < count
            || runtime().wait_observation.sleeps.load(Ordering::Acquire) < count
        {
            assert!(Instant::now() < deadline, "allocators did not join sweep");
            std::thread::yield_now();
        }
        assert!(workers.iter().all(|worker| !worker.is_finished()));
        // A sleeping allocation still participates in subsequent stop rounds.
        // Dedicated GC markers park in the same rounds, so check membership.
        let ids: Vec<_> = workers.iter().map(|worker| worker.thread().id()).collect();
        let parked = with_stw(crate::gc_telemetry::stops::StopReason::Minor, |coord, _| {
            ids.iter().filter(|id| coord.parked.contains(id)).count()
        });
        assert_eq!(parked, count, "every allocator parks in the stop");
        runtime().heap.lock().unwrap().sweeping = None;
        drop(owner);
        for worker in workers {
            worker.join().unwrap();
        }
        println!(
            "sweep waiters={count} condition_checks={} sleeps={}",
            runtime().wait_observation.checks.load(Ordering::Relaxed),
            runtime().wait_observation.sleeps.load(Ordering::Relaxed)
        );
        willow_gc_collect();
        assert_eq!(willow_gc_allocated_bytes(), 0);
    }
    // SAFETY: The test controls its isolated runtime at this reset boundary; no other mutator
    // or subsequent access relies on the discarded heap.
    unsafe { reset_internal_for_test() };
}

#[test]
fn large_runtime_allocations_service_initial_handshake_without_loop_polls() {
    let _guard = isolated_runtime();
    // SAFETY: The test controls its isolated runtime at this reset boundary; no other mutator
    // or subsequent access relies on the discarded heap.
    unsafe { reset_internal_for_test() };
    let ready = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let done = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let worker = {
        let ready = ready.clone();
        let done = done.clone();
        std::thread::spawn(crate::runtime::serving(move || {
            let _registration = MutatorRegistration::new();
            let object = std::cell::Cell::new(std::ptr::null_mut());
            // SAFETY: The initialized slot stays at this address until its registration is
            // popped; collection may rewrite it there.
            unsafe { willow_push_root(object.as_ptr()) };
            ready.store(true, Ordering::Release);
            while !done.load(Ordering::Acquire) {
                // SAFETY: This fixture selects the payload size and tracing layout for its
                // object and initializes any native fields before collection.
                object.set(unsafe { allocate_old(0, 0, 8192, 0) });
                // SAFETY: The test retains this nonempty allocation and accesses a byte within its
                // requested size.
                unsafe {
                    *object.get() = 42;
                }
            }
            // SAFETY: The test retains this nonempty allocation and accesses a byte within its
            // requested size.
            assert_eq!(unsafe { *object.get() }, 42);
            // SAFETY: The removed shadow-stack entries belong to this operation and their
            // slot storage is still live.
            unsafe { willow_pop_root() };
        }))
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
    // SAFETY: The test controls its isolated runtime at this reset boundary; no other mutator
    // or subsequent access relies on the discarded heap.
    unsafe { reset_internal_for_test() };
}

#[test]
fn sweep_allocation_shared_runway_scales_with_bytes_not_waiter_count() {
    let _guard = isolated_runtime();
    for size in [8192, 65536, 262144, 2 * 1024 * 1024] {
        for count in [1, 4, 16] {
            // SAFETY: The test controls its isolated runtime at this reset boundary; no other
            // mutator or subsequent access relies on the discarded heap.
            unsafe { reset_internal_for_test() };
            runtime()
                .wait_observation
                .sleeps
                .store(0, Ordering::Relaxed);
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
                    std::thread::spawn(crate::runtime::serving(move || {
                        let _registration = MutatorRegistration::new();
                        while !done.load(Ordering::Acquire) {
                            // SAFETY: This fixture selects the payload size and tracing
                            // layout for its object and initializes any native fields before
                            // collection.
                            assert!(!unsafe { allocate_old(0, 0, size as i64, 0) }.is_null());
                        }
                    }))
                })
                .collect();
            let deadline = Instant::now() + Duration::from_secs(10);
            while runtime().wait_observation.sleeps.load(Ordering::Acquire) < count {
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
    // SAFETY: The test controls its isolated runtime at this reset boundary; no other mutator
    // or subsequent access relies on the discarded heap.
    unsafe { reset_internal_for_test() };
}

#[test]
fn large_allocation_bounds_unelected_request_runway() {
    let _guard = isolated_runtime();
    // SAFETY: The test controls its isolated runtime at this reset boundary; no other mutator
    // or subsequent access relies on the discarded heap.
    unsafe { reset_internal_for_test() };
    // Model a delayed coordinator without depending on OS scheduling timing.
    // The allocation slow path must start a collector without another request.
    let size = 65536;
    let budget = 1024 * 1024;
    for _ in 0..40 {
        let mut state = runtime().heap.lock().unwrap();
        state.threshold_bytes = usize::MAX;
        drop(state);
        // SAFETY: This fixture selects the payload size and tracing layout for its object and
        // initializes any native fields before collection.
        assert!(!unsafe { allocate_old(0, 0, size, 0) }.is_null());
    }
    runtime().heap.lock().unwrap().threshold_bytes = budget;
    let before = runtime().heap.lock().unwrap().major_collections;
    let _registration = MutatorRegistration::new();
    // SAFETY: This fixture selects the payload size and tracing layout for its object and
    // initializes any native fields before collection.
    assert!(!unsafe { allocate_old(0, 0, size, 0) }.is_null());
    // Initial publication is sufficient to return; collection may still be
    // marking or sweeping. Cooperatively finish before checking reclamation.
    coordinator::shutdown();
    assert!(runtime().heap.lock().unwrap().major_collections > before);
    assert!(willow_gc_allocated_bytes() < size * 2);
    drop(_registration);
    // SAFETY: The test controls its isolated runtime at this reset boundary; no other mutator
    // or subsequent access relies on the discarded heap.
    unsafe { reset_internal_for_test() };
}

static ALLOCATION_RELEASED_TRACE: crate::runtime::PerRuntime<std::sync::atomic::AtomicBool> =
    crate::runtime::PerRuntime::new(|| std::sync::atomic::AtomicBool::new(false));
static ALLOCATION_TRACE_RETRIES: crate::runtime::PerRuntime<AtomicUsize> =
    crate::runtime::PerRuntime::new(|| AtomicUsize::new(0));
static ELECTED_BEFORE_HANDSHAKE: crate::runtime::PerRuntime<std::sync::atomic::AtomicBool> =
    crate::runtime::PerRuntime::new(|| std::sync::atomic::AtomicBool::new(false));

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
    // SAFETY: The GC allocator word-aligns this payload and the fixture reserves the accessed
    // words.
    children.push(unsafe { *(payload as *mut *mut u8) });
    TraceSliceProgress::Done
}

fn hold_election_until_allocation_joins() {
    ELECTED_BEFORE_HANDSHAKE.store(true, Ordering::Release);
    let deadline = Instant::now() + Duration::from_secs(10);
    while runtime()
        .wait_observation
        .initial_waits
        .load(Ordering::Acquire)
        == 0
        && runtime().wait_observation.sleeps.load(Ordering::Acquire) == 0
    {
        assert!(
            Instant::now() < deadline,
            "allocation did not join initial publication"
        );
        std::thread::yield_now();
    }
    runtime().wait_observation.set_initial_hook(None);
}

fn allocation_retry_before_initial_handshake(elected: bool) {
    let _guard = isolated_runtime();
    // SAFETY: The test controls its isolated runtime at this reset boundary; no other mutator
    // or subsequent access relies on the discarded heap.
    unsafe { reset_internal_for_test() };
    ALLOCATION_RELEASED_TRACE.store(false, Ordering::Relaxed);
    ALLOCATION_TRACE_RETRIES.store(0, Ordering::Relaxed);
    ELECTED_BEFORE_HANDSHAKE.store(false, Ordering::Relaxed);
    runtime()
        .wait_observation
        .initial_waits
        .store(0, Ordering::Relaxed);
    runtime()
        .wait_observation
        .sleeps
        .store(0, Ordering::Relaxed);
    const TYPE: u32 = 0xFA9B;
    // SAFETY: The fixture controls every instance of this ID and installs the matching
    // stopped tracer and synchronized snapshot capability, if present.
    unsafe {
        willow_register_type_capability(
            TYPE,
            allocation_retry_legacy,
            NativeTraceCapability::SliceConcurrentSafe(allocation_retry_slice),
        )
    };
    let registration = MutatorRegistration::new();
    runtime().heap.lock().unwrap().threshold_bytes = usize::MAX;
    // SAFETY: This fixture selects the payload size and tracing layout for its object and
    // initializes any native fields before collection.
    let mut parent = unsafe { allocate_old(0, TYPE, 8, 0) };
    // SAFETY: The initialized slot stays at this address until its registration is popped;
    // collection may rewrite it there.
    unsafe { willow_push_root(&raw mut parent) };
    // SAFETY: This fixture selects the payload size and tracing layout for its object and
    // initializes any native fields before collection.
    let child = unsafe { allocate_old(0, 0, 8, 0) };
    // SAFETY: The GC allocator word-aligns this payload and the fixture reserves the accessed
    // words.
    unsafe {
        *child = 42;
        *(parent as *mut *mut u8) = child;
    }
    // SAFETY: This fixture selects the payload size and tracing layout for its object and
    // initializes any native fields before collection.
    assert!(!unsafe { allocate_old(0, 0, 3 * 1024 * 1024, 0) }.is_null());
    runtime().heap.lock().unwrap().threshold_bytes = 1024 * 1024;
    let collector = elected.then(|| {
        std::thread::spawn(crate::runtime::serving(|| {
            let _registration = MutatorRegistration::new();
            let _hook = runtime()
                .wait_observation
                .initial_hook(hold_election_until_allocation_joins);
            collect_internal();
        }))
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
    // SAFETY: This fixture selects the payload size and tracing layout for its object and
    // initializes any native fields before collection.
    let object = unsafe { allocate_old(0, 0, 65536, 0) };
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
    // SAFETY: The GC allocator word-aligns this payload and the fixture reserves the accessed
    // words.
    assert_eq!(unsafe { **(parent as *mut *mut u8) }, 42);
    println!(
        "initial_retry elected={elected} retries={} allocation_returned_before_release=true",
        ALLOCATION_TRACE_RETRIES.load(Ordering::Relaxed)
    );
    // SAFETY: The removed shadow-stack entries belong to this operation and their slot
    // storage is still live.
    unsafe { willow_pop_root() };
    drop(registration);
    willow_gc_collect();
    assert_eq!(willow_gc_allocated_bytes(), 0);
    // SAFETY: The test controls its isolated runtime at this reset boundary; no other mutator
    // or subsequent access relies on the discarded heap.
    unsafe { reset_internal_for_test() };
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
    let _guard = isolated_runtime();
    // SAFETY: The test controls its isolated runtime at this reset boundary; no other mutator
    // or subsequent access relies on the discarded heap.
    unsafe { reset_internal_for_test() };
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
    let worker = std::thread::spawn(crate::runtime::serving(move || {
        let _registration = MutatorRegistration::new();
        tx.send(cycle_wait::wait_for_sweep_budget()).unwrap();
    }));
    let result = rx.recv_timeout(Duration::from_secs(5));
    drop(next);
    worker.join().unwrap();
    assert!(
        !result.unwrap(),
        "stale sweep observation joined a new collector"
    );
    // SAFETY: The test controls its isolated runtime at this reset boundary; no other mutator
    // or subsequent access relies on the discarded heap.
    unsafe { reset_internal_for_test() };
}
