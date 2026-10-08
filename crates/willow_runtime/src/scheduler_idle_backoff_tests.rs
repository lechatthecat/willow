//! Full-scheduler publication-to-poll probe, including the stale-generation path.
use super::*;
use std::sync::atomic::AtomicU64;

static ACTIVE: AtomicBool = AtomicBool::new(false);
static FORCE_WINDOW: AtomicBool = AtomicBool::new(false);
static PRODUCER_DONE: AtomicBool = AtomicBool::new(false);
static FAILED_PROBES: AtomicUsize = AtomicUsize::new(0);
static PUBLISHED_ROUND: AtomicUsize = AtomicUsize::new(0);
static STALE_RETRIES: AtomicUsize = AtomicUsize::new(0);
static PROBE: Mutex<Option<Arc<Probe>>> = Mutex::new(None);
thread_local! {
    static LAST_STALE: Cell<Option<Instant>> = const { Cell::new(None) };
}

pub(super) fn record_failed_probe() {
    if !ACTIVE.load(Ordering::Relaxed) {
        return;
    }
    let round = PUBLISHED_ROUND.load(Ordering::Acquire);
    FAILED_PROBES.fetch_add(1, Ordering::Release);
    // Controlled adverse interleaving: real workers have failed their queue
    // probes, and the real producer poll publishes before they try to park.
    // The natural-contention arm leaves this gate disabled.
    if FORCE_WINDOW.load(Ordering::Relaxed) {
        let deadline = Instant::now() + Duration::from_secs(10);
        while PUBLISHED_ROUND.load(Ordering::Acquire) == round
            && !PRODUCER_DONE.load(Ordering::Acquire)
            && Instant::now() < deadline
        {
            crate::gc::relocatable_safepoint();
            std::hint::spin_loop();
        }
    }
}

pub(super) fn record_stale_retry() {
    if ACTIVE.load(Ordering::Relaxed) {
        STALE_RETRIES.fetch_add(1, Ordering::Relaxed);
        LAST_STALE.set(Some(Instant::now()));
    }
}

struct Sample {
    published: AtomicU64,
    latency: AtomicU64,
    crossed_stale_retry: AtomicBool,
}

struct Probe {
    start: Instant,
    ids: Vec<RuntimeTaskId>,
    samples: Vec<Sample>,
    burst: usize,
    timed_out: AtomicBool,
}

unsafe extern "C" fn consumer(frame: *mut c_void) -> i32 {
    let now = Instant::now();
    let stale = LAST_STALE.take();
    // SAFETY: setup allocates one i64 frame slot and writes its sample index;
    // the task keeps that frame rooted until this poll completes.
    let index = unsafe {
        *frame
            .cast::<u8>()
            .add(crate::async_frame::async_frame_slot_offset(0))
            .cast::<i64>() as usize
    };
    let probe = PROBE.lock().unwrap().as_ref().unwrap().clone();
    let sample = &probe.samples[index];
    let published = sample.published.load(Ordering::Acquire);
    let elapsed = now.duration_since(probe.start).as_nanos() as u64;
    sample
        .latency
        .store(elapsed.saturating_sub(published).max(1), Ordering::Release);
    sample.crossed_stale_retry.store(
        stale.is_some_and(|s| s.duration_since(probe.start).as_nanos() >= u128::from(published)),
        Ordering::Release,
    );
    RUNTIME_POLL_READY
}

unsafe extern "C" fn producer(_: *mut c_void) -> i32 {
    LAST_STALE.set(None);
    let probe = PROBE.lock().unwrap().as_ref().unwrap().clone();
    let mut previous_probes = 0;
    let deadline = Instant::now() + Duration::from_secs(10);
    for (batch, ids) in probe.ids.chunks(probe.burst).enumerate() {
        // Give empty thieves an opportunity to race a new publication rather
        // than measuring only an already-full queue's drain throughput.
        while FAILED_PROBES.load(Ordering::Acquire) <= previous_probes {
            if Instant::now() >= deadline {
                probe.timed_out.store(true, Ordering::Release);
                break;
            }
            crate::gc::relocatable_safepoint();
            std::hint::spin_loop();
        }
        previous_probes = FAILED_PROBES.load(Ordering::Acquire);
        for (offset, &id) in ids.iter().enumerate() {
            probe.samples[batch * probe.burst + offset]
                .published
                .store(probe.start.elapsed().as_nanos() as u64, Ordering::Release);
            willow_sched_wake(id);
        }
        PUBLISHED_ROUND.fetch_add(1, Ordering::Release);
    }
    PRODUCER_DONE.store(true, Ordering::Release);
    notify_all_idle_waiters();
    RUNTIME_POLL_READY
}

fn run_probe(workers: usize, count: usize, burst: usize, forced: bool) {
    crate::gc::reset_internal_for_test();
    reset_global_scheduler_for_test();
    replace_global_scheduler_for_test(workers);
    let mut ids = Vec::with_capacity(count);
    for index in 0..count {
        let frame = crate::async_frame::willow_async_frame_alloc(1, 0);
        // SAFETY: this is the single allocated i64 slot, initialized before
        // the task is made runnable on another worker.
        unsafe {
            *frame
                .cast::<u8>()
                .add(crate::async_frame::async_frame_slot_offset(0))
                .cast::<i64>() = index as i64;
        }
        let id = willow_sched_spawn_cooperative(consumer, frame.cast()) as u64;
        assert_eq!(
            claim_global_ready_for_worker(0, None).map(|p| p.0),
            Some(id)
        );
        finish_global_poll_boundary(id, GlobalPollBoundary::Pending);
        ids.push(id);
    }
    set_current_task(None);
    let probe = Arc::new(Probe {
        start: Instant::now(),
        ids,
        samples: (0..count)
            .map(|_| Sample {
                published: AtomicU64::new(0),
                latency: AtomicU64::new(0),
                crossed_stale_retry: AtomicBool::new(false),
            })
            .collect(),
        burst,
        timed_out: AtomicBool::new(false),
    });
    *PROBE.lock().unwrap() = Some(Arc::clone(&probe));
    FAILED_PROBES.store(0, Ordering::Relaxed);
    PUBLISHED_ROUND.store(0, Ordering::Relaxed);
    STALE_RETRIES.store(0, Ordering::Relaxed);
    PRODUCER_DONE.store(false, Ordering::Relaxed);
    FORCE_WINDOW.store(forced, Ordering::Relaxed);
    ACTIVE.store(true, Ordering::Release);
    willow_sched_spawn_cooperative(producer, std::ptr::null_mut());
    let state = Arc::new(ParallelRunState::default());
    let deadline = Instant::now() + Duration::from_secs(30);
    std::thread::scope(|scope| {
        for worker in 0..workers {
            let state = Arc::clone(&state);
            scope.spawn(move || run_parallel_worker(worker, None, state, Some(deadline)));
        }
    });
    ACTIVE.store(false, Ordering::Release);
    *PROBE.lock().unwrap() = None;
    assert!(!probe.timed_out.load(Ordering::Acquire));
    let mut all = Vec::with_capacity(count);
    let mut stale = Vec::new();
    for sample in &probe.samples {
        let latency = sample.latency.load(Ordering::Acquire);
        assert_ne!(latency, 0, "published task was not polled");
        all.push(latency);
        if sample.crossed_stale_retry.load(Ordering::Acquire) {
            stale.push(latency);
        }
    }
    all.sort_unstable();
    stale.sort_unstable();
    if forced {
        assert!(!stale.is_empty(), "probe never exercised the changed path");
    }
    for (path, values) in [("all", &all), ("stale", &stale)] {
        if !values.is_empty() {
            println!(
                "latency workers={workers} count={count} burst={burst} forced={forced} path={path} samples={} p50_ns={} p99_ns={} stale_retries={} failed_probes={}",
                values.len(),
                values[values.len() / 2],
                values[values.len() * 99 / 100],
                STALE_RETRIES.load(Ordering::Relaxed),
                FAILED_PROBES.load(Ordering::Relaxed),
            );
        }
    }
    assert_eq!(global_run_queues().len(), 0);
    reset_global_scheduler_for_test();
    crate::gc::reset_internal_for_test();
}

#[test]
fn publication_during_failed_probes_reaches_real_task_polls() {
    let _guard = crate::gc::runtime_test_guard();
    for workers in [2, 8] {
        run_probe(workers, 128, 8, true);
    }
}

#[test]
#[ignore = "release full-scheduler latency measurement; run arms interleaved on an idle host"]
fn full_scheduler_publication_latency() {
    let _guard = crate::gc::runtime_test_guard();
    for workers in [2, 8, 16] {
        for forced in [false, true] {
            run_probe(workers, 4096, 8, forced);
        }
    }
}
