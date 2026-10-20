//! Focused measurement/cost reproduction; no generated language code required.
#![no_main]
use std::time::Instant;
use willow_runtime::{
    gc,
    gc_telemetry::{
        self as telemetry,
        observations::{self, Kind, Span},
    },
};

fn endpoint() -> String {
    let generation = telemetry::snapshot().reset_generation;
    telemetry::flush_observations();
    let errors = telemetry::trace_errors();
    let s = observations::snapshot();
    format!(
        "{{\"sequence\":{},\"flags\":{},\"dropped\":{},\"active\":{},\"registered\":{},\"unregistered\":{},\"progress\":{},\"trace_errors\":{},\"reset_generation\":{}}}",
        s.sequence,
        s.flags,
        s.dropped,
        s.active_intervals,
        s.registered,
        s.unregistered,
        s.progress,
        errors,
        generation
    )
}
#[unsafe(no_mangle)]
pub extern "C" fn willow_user_main() {
    gc::willow_gc_init();
    let before = endpoint();
    let mut counter_ns = 0;
    let mut allocation_ns = 0;
    let mut snapshots = Vec::with_capacity(10_000);
    let mut v4_samples = Vec::with_capacity(1000);
    let mut v4 = std::mem::MaybeUninit::<telemetry::WillowGcStatsV4>::uninit();
    let v4_size = std::mem::size_of::<telemetry::WillowGcStatsV4>() as i64;
    for _ in 0..200 {
        {
            let _registration = gc::MutatorRegistration::new();
            let start = Instant::now();
            for _ in 0..50 {
                let _span = Span::new(Kind::Assist);
            }
            counter_ns += start.elapsed().as_nanos();
            telemetry::flush_observations();
            let start = Instant::now();
            for _ in 0..1000 {
                std::hint::black_box(gc::willow_alloc(16));
                gc::willow_gc_safepoint();
            }
            telemetry::flush_observations();
            allocation_ns += start.elapsed().as_nanos();
            for sample in 0..50 {
                let start = Instant::now();
                std::hint::black_box(telemetry::snapshot());
                snapshots.push(start.elapsed().as_nanos());
                if sample < 5 {
                    let start = Instant::now();
                    assert_eq!(
                        telemetry::willow_gc_stats_snapshot(4, v4.as_mut_ptr().cast(), v4_size),
                        v4_size
                    );
                    v4_samples.push(start.elapsed().as_nanos());
                }
            }
        }
        telemetry::flush_observations();
    }
    let after = endpoint();
    telemetry::flush_observations();
    snapshots.sort_unstable();
    v4_samples.sort_unstable();
    println!("{{\"before\":{before},\"after\":{after}}}");
    println!(
        "{{\"counter_record_ns\":{},\"snapshot_p50_ns\":{},\"snapshot_p99_ns\":{},\"v4_snapshot_p50_ns\":{},\"v4_snapshot_p99_ns\":{},\"allocations\":200000,\"allocation_ns\":{},\"allocations_per_second\":{}}}",
        counter_ns / 10_000,
        snapshots[5_000],
        snapshots[9_900],
        v4_samples[500],
        v4_samples[990],
        allocation_ns,
        200_000.0 * 1e9 / allocation_ns as f64
    );
}
#[unsafe(no_mangle)]
pub extern "C" fn __willow_static_init() {}
