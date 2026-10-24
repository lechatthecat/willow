//! Reproducible M3 application graph: chains, fanout and duplicate edges.
#![no_main]
use std::sync::{
    Arc,
    atomic::{AtomicBool, AtomicPtr, Ordering},
};
use std::time::Instant;
use willow_runtime::{gc, gc_telemetry};
#[unsafe(no_mangle)]
pub extern "C" fn willow_user_main() {
    let n: usize = std::env::args()
        .nth(1)
        .unwrap_or("32768".into())
        .parse()
        .unwrap();
    let fanout: usize = std::env::args()
        .nth(2)
        .unwrap_or("4".into())
        .parse()
        .unwrap();
    assert!(n > 0 && (1..=16).contains(&fanout));
    let start = Instant::now();
    // Root slots have stable addresses before registration.
    let mut roots = vec![std::ptr::null_mut(); n];
    for root in &mut roots {
        gc::willow_push_root(root);
    }
    for root in &mut roots {
        *root = gc::willow_alloc_typed((fanout * 8) as i64, (1 << fanout) - 1);
    }
    for (i, &owner) in roots.iter().enumerate() {
        for j in 0..fanout {
            let value = roots[(i + 1 + j) % n];
            gc::willow_gc_write_barrier(owner, std::ptr::null_mut(), value, 1);
            // SAFETY: initialized pointer-aligned payload with matching reference mask.
            unsafe { &*owner.cast::<AtomicPtr<u8>>().add(j) }.store(value, Ordering::Release);
        }
    }
    gc::willow_pop_roots(n as i32);
    let mut root = roots[0];
    gc::willow_push_root(&mut root);
    let stop = Arc::new(AtomicBool::new(false));
    let stopped = stop.clone();
    let (ready, started) = std::sync::mpsc::channel();
    let mutator = std::thread::spawn(move || {
        let _registration = gc::MutatorRegistration::new();
        let began = Instant::now();
        let cpu = gc_telemetry::workers::thread_cpu_ns();
        ready.send(()).unwrap();
        let mut operations = 0u64;
        while !stopped.load(Ordering::Acquire) {
            let mut value = 1u64;
            for _ in 0..128 {
                value = std::hint::black_box(value.wrapping_mul(3));
            }
            std::hint::black_box(value);
            operations += 128;
            gc::willow_gc_safepoint();
        }
        (
            operations,
            cpu.zip(gc_telemetry::workers::thread_cpu_ns())
                .and_then(|(a, b)| b.checked_sub(a)),
            began.elapsed().as_nanos(),
        )
    });
    // This process has no pending automatic request at this boundary: still
    // cooperate while waiting for registration instead of blocking a mutator.
    loop {
        if started.try_recv().is_ok() {
            break;
        }
        gc::willow_gc_safepoint();
        std::thread::yield_now();
    }
    let mut cycle_ns = Vec::new();
    for _ in 0..32 {
        let began = Instant::now();
        gc::willow_gc_collect();
        cycle_ns.push(began.elapsed().as_nanos());
    }
    stop.store(true, Ordering::Release);
    while !mutator.is_finished() {
        gc::willow_gc_safepoint();
        std::thread::yield_now();
    }
    let (operations, mutator_cpu, mutator_wall) = mutator.join().unwrap();
    println!(
        "{{\"event\":\"mutator\",\"operations\":{operations},\"cpu_ns\":{},\"cpu_valid\":{},\"wall_ns\":{mutator_wall},\"cycle_ns\":{cycle_ns:?}}}",
        mutator_cpu.unwrap_or(0),
        mutator_cpu.is_some()
    );
    // Reading every live payload after repeated collection checks graph survival.
    for &owner in &roots {
        assert!(
            !unsafe { &*owner.cast::<AtomicPtr<u8>>() }
                .load(Ordering::Acquire)
                .is_null()
        );
    }
    gc::willow_pop_root();
    gc::willow_gc_collect();
    let elapsed = start.elapsed().as_nanos();
    println!(
        "{{\"event\":\"application\",\"objects\":{n},\"fanout\":{fanout},\"elapsed_ns\":{elapsed},\"heap_after\":{}}}",
        gc::willow_gc_allocated_bytes()
    );
    let s = gc_telemetry::m3::snapshot();
    for (kind, h) in s.intervals.iter().enumerate() {
        println!(
            "{{\"event\":\"lock\",\"kind\":{kind},\"count\":{},\"total_ns\":{},\"max_ns\":{},\"p99_upper_ns\":{},\"collisions\":{}}}",
            h.count,
            h.total_ns,
            h.max_ns,
            h.percentile_upper_ns(99).unwrap_or(0),
            s.collisions[kind]
        );
    }
    for (index, w) in gc_telemetry::m3::workers()
        .iter()
        .enumerate()
        .filter(|(_, w)| w.jobs != 0)
    {
        println!(
            "{{\"event\":\"worker\",\"index\":{index},\"jobs\":{},\"objects\":{},\"scanned_bytes\":{},\"duplicate_claims\":{},\"steal_attempts\":{},\"steals\":{},\"queue_depth_sample_max\":{},\"cpu_ns\":{},\"cpu_unavailable\":{},\"wall_ns\":{},\"idle_ns\":{}}}",
            w.jobs,
            w.objects,
            w.scanned_bytes,
            w.duplicate_claims,
            w.steal_attempts,
            w.steals,
            w.max_queue_depth,
            w.cpu_ns,
            w.cpu_unavailable,
            w.wall_ns,
            w.idle_ns
        );
    }
}
#[unsafe(no_mangle)]
pub extern "C" fn __willow_static_init() {}
