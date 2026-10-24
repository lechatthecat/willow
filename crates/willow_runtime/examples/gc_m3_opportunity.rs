//! Linux CPU-window experiment. Missing /proc samples fail closed everywhere.
#![no_main]
use gc::opportunity::{CpuWindowV1, Priority};
use std::sync::{
    Arc, Barrier,
    atomic::{AtomicBool, Ordering},
};
use std::time::{Duration, Instant};
use willow_runtime::{gc, gc_telemetry, scheduler};
fn ticks() -> Option<(u64, u64, u64)> {
    let system = std::fs::read_to_string("/proc/stat").ok()?;
    let cpu: Vec<u64> = system
        .lines()
        .next()?
        .split_whitespace()
        .skip(1)
        .take(8)
        .map(str::parse)
        .collect::<Result<_, _>>()
        .ok()?;
    let process = std::fs::read_to_string("/proc/self/stat").ok()?;
    let fields: Vec<_> = process.rsplit_once(") ")?.1.split_whitespace().collect();
    Some((
        cpu.iter().sum(),
        *cpu.get(3)?,
        fields.get(11)?.parse::<u64>().ok()? + fields.get(12)?.parse::<u64>().ok()?,
    ))
}
#[unsafe(no_mangle)]
pub extern "C" fn willow_user_main() {
    let busy = std::env::args().nth(1).as_deref() == Some("busy");
    let _mutator = gc::MutatorRegistration::new();
    for _ in 0..1024 {
        gc::willow_alloc(64);
    }
    let stop = Arc::new(AtomicBool::new(false));
    let count = if busy {
        std::thread::available_parallelism().map_or(1, |n| n.get())
    } else {
        0
    };
    let ready = Arc::new(Barrier::new(count + 1));
    let threads: Vec<_> = (0..count)
        .map(|_| {
            let (stop, ready) = (stop.clone(), ready.clone());
            std::thread::spawn(move || {
                ready.wait();
                while !stop.load(Ordering::Acquire) {
                    let mut n = 1u64;
                    for _ in 0..1024 {
                        n = std::hint::black_box(n.wrapping_mul(3));
                    }
                    std::hint::black_box(n);
                }
            })
        })
        .collect();
    ready.wait();
    let start_ns = scheduler::gc_metrics_v1().sampled_ns;
    let before = ticks();
    std::thread::sleep(Duration::from_millis(50));
    let after = ticks();
    let end_ns = scheduler::gc_metrics_v1().sampled_ns;
    let cpu = before.zip(after).and_then(|(a, b)| {
        let total = b.0.checked_sub(a.0).filter(|&n| n > 0)?;
        Some(CpuWindowV1 {
            version: 1,
            start_ns,
            end_ns,
            process_busy_ppm: (b.2.checked_sub(a.2)? * 1_000_000 / total).min(1_000_000) as u32,
            system_idle_ppm: (b.1.checked_sub(a.1)? * 1_000_000 / total).min(1_000_000) as u32,
        })
    });
    let cycles = gc_telemetry::snapshot().major_cycles;
    let optional = gc::opportunity::request_tracked(Priority::Opportunity, cpu);
    let accepted = optional.is_some();
    let await_request = |receipt: gc::opportunity::Request| {
        assert!(
            receipt.is_asynchronous(),
            "probe requires the background request path"
        );
        let deadline = Instant::now() + Duration::from_secs(10);
        while !receipt.is_complete() {
            gc::willow_gc_safepoint();
            assert!(Instant::now() < deadline, "submitted request starved");
            std::thread::yield_now();
        }
    };
    if let Some(receipt) = optional {
        await_request(receipt);
    }
    let required_start = Instant::now();
    await_request(gc::opportunity::request_tracked(Priority::Required, None).unwrap());
    let required_ns = required_start.elapsed().as_nanos();
    stop.store(true, Ordering::Release);
    for thread in threads {
        thread.join().unwrap();
    }
    println!(
        "{{\"busy\":{busy},\"cpu_valid\":{},\"process_busy_ppm\":{},\"system_idle_ppm\":{},\"optional_accepted\":{accepted},\"required_ns\":{required_ns},\"completed_cycles\":{},\"heap_after\":{}}}",
        cpu.is_some(),
        cpu.map_or(0, |c| c.process_busy_ppm),
        cpu.map_or(0, |c| c.system_idle_ppm),
        gc_telemetry::snapshot().major_cycles - cycles,
        gc::willow_gc_allocated_bytes()
    );
}
#[unsafe(no_mangle)]
pub extern "C" fn __willow_static_init() {}
