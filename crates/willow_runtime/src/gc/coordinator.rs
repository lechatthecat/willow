//! Coalesced automatic collection requests. Idle coordinators are unregistered
//! and hold no heap epoch; shutdown waits cooperatively for active readers.
use super::*;

#[derive(Default)]
struct State {
    pending: bool,
    requested: u64,
    finished: Arc<AtomicU64>,
    running: bool,
    shutdown: bool,
}

struct Coordinator {
    shared: Arc<(Mutex<State>, Condvar)>,
    thread: std::thread::JoinHandle<()>,
    completed: Arc<std::sync::atomic::AtomicBool>,
}

static COORDINATOR: Mutex<Option<Coordinator>> = Mutex::new(None);

impl Coordinator {
    fn new() -> std::io::Result<Self> {
        let shared = Arc::new((Mutex::new(State::default()), Condvar::new()));
        let worker = shared.clone();
        let completed = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let completion = completed.clone();
        let thread = std::thread::Builder::new()
            .name("willow-gc-cycle".into())
            .spawn(move || run(worker, completion))?;
        Ok(Self {
            shared,
            thread,
            completed,
        })
    }
}

/// Returns false for the synchronous compatibility path: native callers outside
/// the registry cannot publish roots to a background collector.
pub(super) fn request() -> bool {
    request_tracked().is_some()
}

/// A receipt is completed only by a successful coordinator collection which
/// selected this request. An unrelated synchronous collection cannot satisfy it.
#[derive(Clone)]
pub struct Request {
    sequence: u64,
    finished: Arc<AtomicU64>,
}
impl Request {
    /// False only when an unregistered/spawn-failure compatibility collection
    /// completed synchronously before the receipt was returned.
    pub fn is_asynchronous(&self) -> bool {
        self.sequence != 0
    }
    pub fn is_complete(&self) -> bool {
        self.finished.load(Ordering::Acquire) >= self.sequence
    }
    pub(super) fn synchronous() -> Self {
        Self {
            sequence: 0,
            finished: Arc::new(AtomicU64::new(0)),
        }
    }
}

pub(super) fn request_tracked() -> Option<Request> {
    let id = std::thread::current().id();
    if !runtime().coord.0.lock().unwrap().mutators.contains_key(&id) {
        return None;
    }
    let mut coordinator = COORDINATOR.lock().unwrap();
    if coordinator.is_none() {
        match Coordinator::new() {
            Ok(created) => *coordinator = Some(created),
            Err(_) => {
                crate::gc_telemetry::workers::record_failure(
                    crate::gc_telemetry::workers::Failure::Spawn,
                );
                return None;
            }
        }
    }
    let coordinator = coordinator.as_ref().unwrap();
    let (lock, cv) = &*coordinator.shared;
    let mut state = lock.lock().unwrap();
    if state.shutdown {
        return None;
    }
    // Retain requests arriving before the current sweep publishes free spans.
    // The single coordinator starts the pending cycle only after collect returns.
    state.requested = state
        .requested
        .checked_add(1)
        .expect("GC request sequence exhausted");
    let receipt = Request {
        sequence: state.requested,
        finished: state.finished.clone(),
    };
    state.pending = true;
    cv.notify_one();
    Some(receipt)
}

fn run(shared: Arc<(Mutex<State>, Condvar)>, completion: Arc<std::sync::atomic::AtomicBool>) {
    crate::failure::ffi_boundary("GC coordinator", || {
        run_loop(shared);
        // Publish under coord so completion and stop requests share the
        // waiter's predicate lock. run_loop has released every GC owner.
        let _coord = runtime().coord.0.lock().unwrap();
        completion.store(true, Ordering::Relaxed);
        runtime().budget_cv.notify_all();
    });
}

fn run_loop(shared: Arc<(Mutex<State>, Condvar)>) {
    let (lock, cv) = &*shared;
    loop {
        let mut state = lock.lock().unwrap();
        while !state.pending && !state.shutdown {
            state = cv.wait(state).unwrap();
        }
        if state.shutdown {
            return;
        }
        state.pending = false;
        state.running = true;
        let mut selected = state.requested;
        drop(state);
        let succeeded = loop {
            willow_gc_register_mutator();
            #[cfg(test)]
            if PANIC_BEFORE_COLLECTION.load(Ordering::Relaxed) {
                panic!("injected coordinator collection panic");
            }
            let attempt = try_collect_internal();
            willow_gc_unregister_mutator();
            if attempt == CollectionAttempt::Completed {
                break true;
            }
            // Keep the selected requests owned until an actual collection.
            let mut state = lock.lock().unwrap();
            state.pending = true;
            if attempt == CollectionAttempt::ForeignOwner {
                // Legacy root owners need not safepoint and have no common
                // notification edge. Retry at most once per 10ms while fully
                // unregistered; shutdown interrupts this wait. New requests and
                // spurious wakes cannot shorten the backoff into a busy loop.
                state = cv
                    .wait_timeout_while(state, std::time::Duration::from_millis(10), |state| {
                        !state.shutdown
                    })
                    .unwrap()
                    .0;
            } else {
                drop(state);
                cycle_wait::wait_for_current_cycle();
                state = lock.lock().unwrap();
            }
            if state.shutdown {
                break false;
            }
            state.pending = false;
            selected = state.requested;
        };
        let mut state = lock.lock().unwrap();
        if succeeded {
            state.finished.store(selected, Ordering::Release);
        }
        state.running = false;
        cv.notify_all();
    }
}

pub(super) fn shutdown() {
    let Some(coordinator) = COORDINATOR.lock().unwrap().take() else {
        return;
    };
    {
        let (lock, cv) = &*coordinator.shared;
        let mut state = lock.lock().unwrap();
        state.pending = false;
        state.shutdown = true;
        cv.notify_one();
    }
    // The caller may still be a registered mutator. A blocking join would
    // deadlock a coordinator awaiting its root snapshot or fallback safepoint.
    let mut coord = runtime().coord.0.lock().unwrap();
    while !coordinator.completed.load(Ordering::Relaxed) {
        if coord.stop_requested {
            drop(coord);
            willow_gc_safepoint();
            coord = runtime().coord.0.lock().unwrap();
        } else {
            #[cfg(test)]
            SHUTDOWN_WAITS.fetch_add(1, Ordering::Release);
            coord = runtime().budget_cv.wait(coord).unwrap();
        }
    }
    drop(coord);
    // Completion proves heap quiescence, even if OS/TLS teardown is delayed.
    if coordinator.thread.is_finished() {
        let _ = coordinator.thread.join();
    }
}

#[cfg(test)]
static SHUTDOWN_WAITS: AtomicUsize = AtomicUsize::new(0);

#[cfg(test)]
static PANIC_BEFORE_COLLECTION: std::sync::atomic::AtomicBool =
    std::sync::atomic::AtomicBool::new(false);

#[cfg(test)]
mod tests {
    use super::*;
    static ENTERED: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);
    static RELEASE: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

    unsafe fn legacy(_: *mut u8, _: &mut Vec<*mut *mut u8>) {}
    unsafe fn paused_trace(
        _: *mut u8,
        _: usize,
        _: usize,
        _: &mut Vec<*mut u8>,
    ) -> TraceSliceProgress {
        ENTERED.store(true, Ordering::Release);
        if RELEASE.load(Ordering::Acquire) {
            TraceSliceProgress::Done
        } else {
            TraceSliceProgress::Retry
        }
    }

    #[test]
    fn shutdown_notification_spurious_wakes_and_stop_rounds() {
        let _guard = runtime_test_guard();
        reset_internal_for_test();
        let _mutator = MutatorRegistration::new();
        SHUTDOWN_WAITS.store(0, Ordering::Relaxed);
        let completed = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let completion = completed.clone();
        let caller = std::thread::current().id();
        let thread = std::thread::spawn(move || {
            let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
            for round in 1..=5 {
                while SHUTDOWN_WAITS.load(Ordering::Acquire) < round {
                    assert!(
                        std::time::Instant::now() < deadline,
                        "shutdown did not sleep"
                    );
                    std::thread::yield_now();
                }
                // Acquiring the predicate lock proves the waiter reached wait.
                let _coord = runtime().coord.0.lock().unwrap();
                assert!(!completion.load(Ordering::Relaxed));
                runtime().budget_cv.notify_all();
            }
            for reason in [
                crate::gc_telemetry::stops::StopReason::Remark,
                crate::gc_telemetry::stops::StopReason::Recovery,
                crate::gc_telemetry::stops::StopReason::Minor,
            ] {
                with_stw(reason, |coord, _| assert!(coord.parked.contains(&caller)));
            }
            let _coord = runtime().coord.0.lock().unwrap();
            completion.store(true, Ordering::Relaxed);
            runtime().budget_cv.notify_all();
        });
        *COORDINATOR.lock().unwrap() = Some(Coordinator {
            shared: Arc::new((Mutex::new(State::default()), Condvar::new())),
            thread,
            completed,
        });
        shutdown();
        assert!(SHUTDOWN_WAITS.load(Ordering::Relaxed) >= 5);
        assert_eq!(registered_mutator_count(), 1);
        drop(_mutator);
        reset_internal_for_test();
    }

    #[test]
    fn shutdown_notification_idle_repeated_and_before_wait() {
        let _guard = runtime_test_guard();
        reset_internal_for_test();
        shutdown(); // Absent coordinator.
        for _ in 0..64 {
            *COORDINATOR.lock().unwrap() = Some(Coordinator::new().unwrap());
            shutdown();
            assert!(COORDINATOR.lock().unwrap().is_none());
        }
        // Completion may be published before the shutdown waiter acquires coord.
        // Delay OS thread teardown to ensure shutdown never joins it blindly.
        let (release, released) = std::sync::mpsc::channel();
        let (done, completed) = std::sync::mpsc::channel();
        let thread = std::thread::spawn(move || {
            released.recv().unwrap();
            done.send(()).unwrap();
        });
        *COORDINATOR.lock().unwrap() = Some(Coordinator {
            shared: Arc::new((Mutex::new(State::default()), Condvar::new())),
            thread,
            completed: Arc::new(std::sync::atomic::AtomicBool::new(true)),
        });
        shutdown();
        release.send(()).unwrap();
        completed
            .recv_timeout(std::time::Duration::from_secs(2))
            .unwrap();
        reset_internal_for_test();
    }

    #[test]
    fn coordinator_panic_terminates_instead_of_disabling_collection() {
        use super::super::failure_policy_tests::{GC_FATAL_CASE, assert_fatal_child};
        if std::env::var_os(GC_FATAL_CASE).is_some() {
            reset_internal_for_test();
            willow_gc_register_mutator();
            PANIC_BEFORE_COLLECTION.store(true, Ordering::Relaxed);
            assert!(request());
            loop {
                std::thread::park();
            }
        }
        let stderr = assert_fatal_child(
            "gc::coordinator::tests::coordinator_panic_terminates_instead_of_disabling_collection",
            "coordinator",
            "runtime fatal: Rust panic in GC coordinator",
        );
        assert!(stderr.contains("injected coordinator collection panic"));
    }

    #[test]
    fn allocation_resumes_while_automatic_collection_is_tracing() {
        let _guard = runtime_test_guard();
        reset_internal_for_test();
        ENTERED.store(false, Ordering::Relaxed);
        RELEASE.store(false, Ordering::Relaxed);
        const TYPE: u32 = 0xFA88;
        willow_register_type_capability(
            TYPE,
            legacy,
            NativeTraceCapability::SliceConcurrentSafe(paused_trace),
        );
        let mut root = willow_alloc_object(TYPE as i64, 8);
        willow_push_root(&mut root);
        willow_gc_register_mutator();
        runtime().heap.lock().unwrap().threshold_bytes = 1;
        crate::gc::invalidate_runtime_policy_budgets();
        assert!(!willow_alloc(8).is_null());
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
        while !ENTERED.load(Ordering::Acquire) {
            willow_gc_safepoint();
            assert!(
                std::time::Instant::now() < deadline,
                "automatic collector did not start"
            );
            std::thread::yield_now();
        }
        for _ in 0..32 {
            assert!(!willow_alloc(8).is_null());
            assert!(request()); // Coalesces into the already-running cycle.
        }
        RELEASE.store(true, Ordering::Release);
        shutdown();
        assert_eq!(runtime().heap.lock().unwrap().major_collections, 1);
        assert_eq!(registered_mutator_count(), 1);
        willow_pop_root();
        willow_gc_unregister_mutator();
        collect_internal();
        assert_eq!(willow_gc_allocated_bytes(), 0);
        reset_internal_for_test();
    }

    #[test]
    fn automatic_request_survives_explicit_collector_election_loss() {
        let _guard = runtime_test_guard();
        reset_internal_for_test();
        ENTERED.store(false, Ordering::Relaxed);
        RELEASE.store(false, Ordering::Relaxed);
        cycle_wait::SLEEPS.store(0, Ordering::Relaxed);
        const TYPE: u32 = 0xFA89;
        willow_register_type_capability(
            TYPE,
            legacy,
            NativeTraceCapability::SliceConcurrentSafe(paused_trace),
        );
        let _mutator = MutatorRegistration::new();
        let mut root = willow_alloc_object(TYPE as i64, 8);
        willow_push_root(&mut root);
        runtime().heap.lock().unwrap().threshold_bytes = usize::MAX;
        let explicit = std::thread::spawn(|| {
            let _mutator = MutatorRegistration::new();
            willow_gc_collect();
        });
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
        while !ENTERED.load(Ordering::Acquire) {
            willow_gc_safepoint();
            assert!(
                std::time::Instant::now() < deadline,
                "explicit marker did not start"
            );
            std::thread::yield_now();
        }
        let receipt = request_tracked().unwrap(); // Only automatic request; no later allocations.
        assert!(!receipt.is_complete());
        while cycle_wait::SLEEPS.load(Ordering::Acquire) == 0 {
            willow_gc_safepoint();
            assert!(
                std::time::Instant::now() < deadline,
                "automatic collector did not join"
            );
            std::thread::yield_now();
        }
        {
            let coordinator = COORDINATOR.lock().unwrap();
            let state = coordinator.as_ref().unwrap().shared.0.lock().unwrap();
            assert!(
                state.pending && state.running,
                "failed election consumed the request"
            );
        }
        assert_eq!(runtime().heap.lock().unwrap().major_collections, 0);
        RELEASE.store(true, Ordering::Release);
        while !explicit.is_finished() || runtime().heap.lock().unwrap().major_collections < 2 {
            willow_gc_safepoint();
            assert!(
                std::time::Instant::now() < deadline,
                "pending post-sweep cycle did not run"
            );
            std::thread::yield_now();
        }
        explicit.join().unwrap();
        while !receipt.is_complete() {
            willow_gc_safepoint();
            assert!(std::time::Instant::now() < deadline);
            std::thread::yield_now();
        }
        shutdown();
        assert!(receipt.is_complete());
        assert_eq!(runtime().heap.lock().unwrap().major_collections, 2);
        assert_eq!(willow_gc_allocated_bytes(), (GC_HEADER_SIZE + 8) as i64);
        println!(
            "explicit_cycles=1 retained_automatic_cycles=1 additional_requests=0 cooperative_sleeps={}",
            cycle_wait::SLEEPS.load(Ordering::Relaxed)
        );
        willow_pop_roots(1);
        drop(_mutator);
        reset_internal_for_test();
    }

    #[test]
    fn m3_receipts_defer_foreign_roots_before_and_at_handshake() {
        let _guard = runtime_test_guard();
        for asynchronous in [false, true] {
            for handshake in [false, true] {
                reset_internal_for_test();
                RECEIPT_TEST_HANDSHAKE_ONLY.store(handshake, Ordering::Relaxed);
                let (ready_tx, ready_rx) = std::sync::mpsc::channel();
                let (release_tx, release_rx) = std::sync::mpsc::channel();
                let owner = std::thread::spawn(move || {
                    let mut root = willow_alloc(32);
                    willow_push_root(&mut root);
                    ready_tx.send(()).unwrap();
                    release_rx.recv().unwrap();
                    willow_pop_root();
                });
                ready_rx.recv().unwrap();
                let registration = asynchronous.then(MutatorRegistration::new);
                let before = gc_progress();
                let receipt = opportunity::request_tracked(opportunity::Priority::Required, None);
                let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
                if asynchronous {
                    let receipt = receipt.as_ref().unwrap();
                    assert!(receipt.is_asynchronous());
                    while gc_progress().skipped_collections == before.skipped_collections {
                        willow_gc_safepoint();
                        assert!(std::time::Instant::now() < deadline);
                        std::thread::yield_now();
                    }
                    let began = std::time::Instant::now();
                    let skips = gc_progress().skipped_collections;
                    while began.elapsed() < std::time::Duration::from_millis(35) {
                        assert!(!receipt.is_complete());
                        willow_gc_safepoint();
                        // Exercise spurious wakeups during the timed backoff.
                        if let Some(coordinator) = COORDINATOR.lock().unwrap().as_ref() {
                            coordinator.shared.1.notify_all();
                        }
                        std::thread::sleep(std::time::Duration::from_millis(1));
                    }
                    // Count attempts, not CPU timing: even spurious wakes cannot
                    // turn the foreign-owner retry path into an unbounded loop.
                    assert!(
                        gc_progress().skipped_collections - skips
                            <= began.elapsed().as_millis() as u64 / 10 + 2
                    );
                } else {
                    assert!(
                        receipt.is_none(),
                        "skipped synchronous attempt invented completion"
                    );
                }
                let skipped = gc_progress();
                assert_eq!(
                    skipped.successful_collections,
                    before.successful_collections
                );
                assert_eq!(
                    skipped.last_skip_reason,
                    if handshake {
                        GcSkipReason::ForeignOwnerAtHandshake as u64
                    } else {
                        GcSkipReason::ForeignOwner as u64
                    }
                );
                release_tx.send(()).unwrap();
                while !owner.is_finished() {
                    willow_gc_safepoint();
                    assert!(std::time::Instant::now() < deadline);
                    std::thread::yield_now();
                }
                owner.join().unwrap();
                let receipt = receipt.unwrap_or_else(|| {
                    opportunity::request_tracked(opportunity::Priority::Required, None).unwrap()
                });
                while !receipt.is_complete() {
                    willow_gc_safepoint();
                    assert!(
                        std::time::Instant::now() < deadline,
                        "retained request did not resume"
                    );
                    std::thread::yield_now();
                }
                assert!(gc_progress().successful_collections > before.successful_collections);
                assert_eq!(willow_gc_allocated_bytes(), 0);
                shutdown();
                drop(registration);
                RECEIPT_TEST_HANDSHAKE_ONLY.store(false, Ordering::Relaxed);
                println!(
                    "receipt asynchronous={asynchronous} handshake_skip={handshake} skipped_not_completed=true recovered=true"
                );
            }
        }
        reset_internal_for_test();
    }

    #[test]
    fn m3_foreign_root_deferred_receipt_shutdown_stays_incomplete() {
        let _guard = runtime_test_guard();
        reset_internal_for_test();
        let (ready_tx, ready_rx) = std::sync::mpsc::channel();
        let (release_tx, release_rx) = std::sync::mpsc::channel();
        let owner = std::thread::spawn(move || {
            let mut root = willow_alloc(32);
            willow_push_root(&mut root);
            ready_tx.send(()).unwrap();
            release_rx.recv().unwrap();
            willow_pop_root();
        });
        ready_rx.recv().unwrap();
        let registration = MutatorRegistration::new();
        let receipt = request_tracked().unwrap();
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        while gc_progress().skipped_collections == 0 {
            willow_gc_safepoint();
            assert!(std::time::Instant::now() < deadline);
            std::thread::yield_now();
        }
        shutdown(); // Must interrupt the backoff while the foreign owner remains.
        assert!(!receipt.is_complete());
        assert_eq!(gc_progress().successful_collections, 0);
        release_tx.send(()).unwrap();
        owner.join().unwrap();
        willow_gc_collect();
        assert!(
            !receipt.is_complete(),
            "an unrelated collection satisfied a canceled request"
        );
        assert_eq!(willow_gc_allocated_bytes(), 0);
        drop(registration);
        reset_internal_for_test();
    }

    #[test]
    fn unregistered_native_callers_keep_synchronous_compatibility() {
        let _guard = runtime_test_guard();
        reset_internal_for_test();
        assert!(!request());
        assert!(COORDINATOR.lock().unwrap().is_none());
    }
}
