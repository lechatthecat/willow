//! Bounded collection-time binary spool. Canonical JSON is merged after exit.
use std::io::{BufWriter, Write};
use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
};
use willow_runtime::gc_telemetry as t;

fn words(out: &mut impl Write, tag: u8, values: &[u64]) {
    out.write_all(&[tag]).expect("spool tag");
    // Explicit encoding avoids padding, host endian and unsafe struct casts.
    for value in values {
        out.write_all(&value.to_le_bytes()).expect("spool value");
    }
}

pub(super) fn drain(flag: Arc<AtomicBool>) -> (u64, Option<u64>) {
    let cpu = t::workers::thread_cpu_ns();
    if !flag.load(Ordering::Acquire) {
        // Not the observation consumer (non-V4): never take runtime events.
        return (0, cpu.zip(t::workers::thread_cpu_ns()).map(|(a, b)| b - a));
    }
    let mut output = std::env::var_os("GC_ACCEPTANCE_EVENT_SPOOL").map(|path| {
        let mut out = BufWriter::with_capacity(
            1024 * 1024,
            std::fs::File::create(path).expect("create observation spool"),
        );
        out.write_all(b"WGC4BIN1").expect("spool header");
        out
    });
    let mut flushes = 0u64;
    loop {
        // Sample the flag before draining: events recorded before the owner
        // cleared it are always taken by this final pass.
        let last = !flag.load(Ordering::Acquire);
        if let Some(out) = &mut output {
            let (events, s) = t::observations::take_events();
            for e in events {
                words(
                    out,
                    1,
                    &[
                        e.sequence,
                        e.timestamp_ns,
                        e.epoch,
                        e.thread,
                        e.role,
                        e.event,
                        e.kind,
                        e.identity,
                        e.aborted,
                        e.progress,
                        e.work.objects,
                        e.work.slots,
                        e.work.bytes,
                        e.work.metadata_bytes,
                        e.work.budget_checks,
                        e.work.yields,
                        e.work.restarts,
                    ],
                );
            }
            words(
                out,
                2,
                &[
                    s.sequence,
                    s.flags,
                    s.dropped,
                    s.active_intervals,
                    s.registered,
                    s.unregistered,
                    s.progress,
                ],
            );
        } else {
            t::flush_observations();
        }
        flushes += 1;
        if last {
            break;
        }
        std::thread::sleep(std::time::Duration::from_micros(100));
    }
    if let Some(out) = &mut output {
        out.flush().expect("flush observation spool");
    }
    (
        flushes,
        cpu.zip(t::workers::thread_cpu_ns()).map(|(a, b)| b - a),
    )
}
