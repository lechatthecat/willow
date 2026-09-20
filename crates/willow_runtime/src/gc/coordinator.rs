//! Coalesced automatic collection requests. Idle coordinators are unregistered
//! and hold no heap epoch; shutdown waits cooperatively for active readers.
use super::*;

#[derive(Default)]
struct State {
    pending: bool,
    running: bool,
    shutdown: bool,
    failed: bool,
}

struct Coordinator {
    shared: Arc<(Mutex<State>, Condvar)>,
    thread: std::thread::JoinHandle<()>,
}

static COORDINATOR: Mutex<Option<Coordinator>> = Mutex::new(None);

impl Coordinator {
    fn new() -> std::io::Result<Self> {
        let shared = Arc::new((Mutex::new(State::default()), Condvar::new()));
        let worker = shared.clone();
        let thread = std::thread::Builder::new()
            .name("willow-gc-cycle".into())
            .spawn(move || run(worker))?;
        Ok(Self { shared, thread })
    }
}

/// Returns false for the synchronous compatibility path: native callers outside
/// the registry cannot publish roots to a background collector.
pub(super) fn request() -> bool {
    let id = std::thread::current().id();
    if !runtime().coord.0.lock().unwrap().mutators.contains_key(&id) {
        return false;
    }
    let mut coordinator = COORDINATOR.lock().unwrap();
    if coordinator.is_none() {
        match Coordinator::new() {
            Ok(created) => *coordinator = Some(created),
            Err(_) => {
                crate::gc_telemetry::workers::record_failure(
                    crate::gc_telemetry::workers::Failure::Spawn,
                );
                return false;
            }
        }
    }
    let coordinator = coordinator.as_ref().unwrap();
    let (lock, cv) = &*coordinator.shared;
    let mut state = lock.lock().unwrap();
    if state.failed || state.shutdown {
        return false;
    }
    if !state.running {
        state.pending = true;
        cv.notify_one();
    }
    true
}

fn run(shared: Arc<(Mutex<State>, Condvar)>) {
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
        drop(state);
        willow_gc_register_mutator();
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(collect_internal));
        willow_gc_unregister_mutator();
        let failed = result.is_err();
        if let Err(payload) = result {
            discard_callback_panic(payload);
        }
        let mut state = lock.lock().unwrap();
        state.running = false;
        state.failed |= failed;
        cv.notify_all();
        if failed {
            return;
        }
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
    while !coordinator.thread.is_finished() {
        willow_gc_safepoint();
        std::thread::sleep(std::time::Duration::from_millis(1));
    }
    let _ = coordinator.thread.join();
}

#[cfg(test)]
mod tests {
    use super::*;
    static ENTERED: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);
    static RELEASE: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

    unsafe fn legacy(_: *mut u8, _: &mut Vec<*mut *mut u8>) {}
    unsafe fn paused_trace(_: *mut u8, _: &mut Vec<*mut u8>) {
        ENTERED.store(true, Ordering::Release);
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
        while !RELEASE.load(Ordering::Acquire) {
            assert!(
                std::time::Instant::now() < deadline,
                "coordinator test did not release trace"
            );
            std::thread::yield_now();
        }
    }

    #[test]
    fn allocation_resumes_while_automatic_collection_is_tracing() {
        let _guard = runtime_test_guard();
        reset_internal_for_test();
        ENTERED.store(false, Ordering::Relaxed);
        RELEASE.store(false, Ordering::Relaxed);
        const TYPE: u32 = 0xFA88;
        willow_register_type(TYPE, legacy);
        runtime()
            .concurrent_trace_registry
            .lock()
            .unwrap()
            .insert(TYPE, paused_trace);
        let mut root = willow_alloc_object(TYPE as i64, 8);
        willow_push_root(&mut root);
        willow_gc_register_mutator();
        runtime().heap.lock().unwrap().threshold_bytes = 1;
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
    fn unregistered_native_callers_keep_synchronous_compatibility() {
        let _guard = runtime_test_guard();
        reset_internal_for_test();
        assert!(!request());
        assert!(COORDINATOR.lock().unwrap().is_none());
    }
}
