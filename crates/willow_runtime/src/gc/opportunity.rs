//! Explicit experimental admission for early cycles. Mandatory collection keeps
//! its existing unthrottled path. CPU samples must include the whole process
//! (therefore assists) and the whole system, normalized to available capacity.
use std::sync::atomic::{AtomicU64, Ordering};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Priority {
    Emergency,
    Required,
    Opportunity,
    Background,
}
#[derive(Clone, Copy, Debug)]
pub struct CpuWindowV1 {
    pub version: u32,
    pub start_ns: u64,
    pub end_ns: u64,
    pub process_busy_ppm: u32,
    pub system_idle_ppm: u32,
}
const FRESH_NS: u64 = 100_000_000;
const COOLDOWN_NS: u64 = 1_000_000_000;
static NEXT_OPTIONAL: AtomicU64 = AtomicU64::new(0);

fn eligible(priority: Priority, cpu: Option<CpuWindowV1>, now: u64, queued: usize) -> bool {
    if matches!(priority, Priority::Emergency | Priority::Required) {
        return true;
    }
    let Some(cpu) = cpu else {
        return false;
    };
    cpu.version == 1
        && cpu.start_ns < cpu.end_ns
        && cpu.end_ns <= now
        && now - cpu.end_ns <= FRESH_NS
        && cpu.end_ns - cpu.start_ns <= FRESH_NS
        && cpu.process_busy_ppm <= 500_000
        && cpu.system_idle_ppm >= 250_000
        && cpu.system_idle_ppm <= 1_000_000
        && queued == 0
}
fn reserve(next: &AtomicU64, now: u64) -> bool {
    // A bounded single CAS: racing callers defer optional work, never retry-spin.
    let previous = next.load(Ordering::Acquire);
    now >= previous
        && now.checked_add(COOLDOWN_NS).is_some_and(|deadline| {
            next.compare_exchange(previous, deadline, Ordering::AcqRel, Ordering::Acquire)
                .is_ok()
        })
}
/// Returns whether a request was accepted. This prototype never invents system
/// idleness from empty Willow queues; missing/stale CPU samples reject early GC.
/// Fairness: at most one optional request per second; mandatory requests bypass
/// both telemetry and this quota. Normal hard reservation checks are unchanged.
pub fn request(priority: Priority, cpu: Option<CpuWindowV1>) -> bool {
    request_tracked(priority, cpu).is_some()
}

pub use super::coordinator::Request;

/// Submit once and observe completion without forcing an independent cycle.
/// None means admission failed or a synchronous attempt was deferred (foreign
/// roots or collector contention); no completed receipt is invented for a skip.
pub fn request_tracked(priority: Priority, cpu: Option<CpuWindowV1>) -> Option<Request> {
    let optional = matches!(priority, Priority::Opportunity | Priority::Background);
    if optional {
        let scheduler = crate::scheduler::gc_metrics_v1();
        let now = crate::gc_telemetry::timestamp_ns();
        if scheduler.version != 1
            || !eligible(priority, cpu, now, scheduler.runnable)
            || !reserve(&NEXT_OPTIONAL, now)
        {
            return None;
        }
    }
    if let Some(receipt) = super::coordinator::request_tracked() {
        Some(receipt)
    } else if optional {
        None
    } else {
        crate::failure::ffi_boundary("opportunity GC", || {
            crate::scheduler::release_quiescent_frame_roots(true);
            let _relocation = super::RelocationSafeScope::enter();
            (super::try_collect_internal() == super::CollectionAttempt::Completed)
                .then(Request::synchronous)
        })
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    fn idle() -> CpuWindowV1 {
        CpuWindowV1 {
            version: 1,
            start_ns: 1,
            end_ns: 10,
            process_busy_ppm: 1,
            system_idle_ppm: 900_000,
        }
    }
    #[test]
    fn m3_opportunity_rejects_stale_missing_busy_and_queued_samples() {
        for p in [Priority::Opportunity, Priority::Background] {
            assert!(eligible(p, Some(idle()), 10, 0));
            assert!(!eligible(p, None, 10, 0));
            assert!(!eligible(p, Some(idle()), 10, 1));
            assert!(!eligible(p, Some(idle()), 9, 0));
            assert!(!eligible(p, Some(idle()), FRESH_NS + 11, 0));
            for bad in [
                CpuWindowV1 {
                    version: 2,
                    ..idle()
                },
                CpuWindowV1 {
                    start_ns: 10,
                    ..idle()
                },
                CpuWindowV1 {
                    process_busy_ppm: 500_001,
                    ..idle()
                },
                CpuWindowV1 {
                    system_idle_ppm: 249_999,
                    ..idle()
                },
                CpuWindowV1 {
                    system_idle_ppm: 1_000_001,
                    ..idle()
                },
            ] {
                assert!(!eligible(p, Some(bad), 10, 0));
            }
        }
        for p in [Priority::Emergency, Priority::Required] {
            for cpu in [
                None,
                Some(idle()),
                Some(CpuWindowV1 {
                    system_idle_ppm: 0,
                    process_busy_ppm: 1_000_000,
                    ..idle()
                }),
            ] {
                assert!(eligible(p, cpu, u64::MAX, usize::MAX));
            }
        }
    }
    #[test]
    fn m3_opportunity_quota_has_one_winner_and_no_overflow() {
        let next = AtomicU64::new(0);
        std::thread::scope(|scope| {
            let results: Vec<_> = (0..64).map(|_| scope.spawn(|| reserve(&next, 1))).collect();
            assert_eq!(
                results
                    .into_iter()
                    .map(|t| usize::from(t.join().unwrap()))
                    .sum::<usize>(),
                1
            );
        });
        assert_eq!(next.load(Ordering::Relaxed), 1 + COOLDOWN_NS);
        assert!(!reserve(&next, COOLDOWN_NS));
        assert!(reserve(&next, COOLDOWN_NS + 1));
        assert!(!reserve(&next, u64::MAX));
    }
    #[test]
    fn m3_mandatory_collection_ignores_missing_utilization() {
        let _guard = super::super::runtime_test_guard();
        super::super::reset_internal_for_test();
        super::super::willow_alloc(128);
        assert!(request(Priority::Required, None));
        assert_eq!(super::super::willow_gc_allocated_bytes(), 0);
        super::super::willow_alloc(128);
        assert!(request(Priority::Emergency, None));
        assert_eq!(super::super::willow_gc_allocated_bytes(), 0);
        super::super::reset_internal_for_test();
    }
}

#[cfg(test)]
mod saturation_test {
    use super::*;
    use crate::gc::*;
    #[test]
    fn m3_saturated_mutators_preserve_mandatory_progress_and_hard_budget() {
        let _guard = runtime_test_guard();
        reset_internal_for_test();
        const LIMIT: usize = 8 * GC_OLD_REGION_SIZE;
        runtime().heap.lock().unwrap().memory_limit_bytes = Some(LIMIT);
        let _main = MutatorRegistration::new();
        let stop = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let progress = std::sync::Arc::new(AtomicU64::new(0));
        let threads: Vec<_> = (0..4)
            .map(|_| {
                let (stop, progress) = (stop.clone(), progress.clone());
                std::thread::spawn(move || {
                    let _mutator = MutatorRegistration::new();
                    while !stop.load(Ordering::Acquire) {
                        let mut checksum = 1u64;
                        for _ in 0..1024 {
                            checksum = std::hint::black_box(checksum.wrapping_mul(3));
                        }
                        std::hint::black_box(checksum);
                        willow_alloc(64);
                        willow_gc_safepoint();
                        progress.fetch_add(1, Ordering::Relaxed);
                    }
                })
            })
            .collect();
        let before = runtime().heap.lock().unwrap().major_collections;
        for _ in 0..8 {
            let now = crate::gc_telemetry::timestamp_ns();
            let busy = CpuWindowV1 {
                version: 1,
                start_ns: now.saturating_sub(1),
                end_ns: now,
                process_busy_ppm: 1_000_000,
                system_idle_ppm: 0,
            };
            assert!(!request(Priority::Opportunity, Some(busy)));
            assert!(!request(Priority::Background, Some(busy)));
            let receipt = request_tracked(Priority::Required, Some(busy)).unwrap();
            assert!(receipt.is_asynchronous());
            let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
            while !receipt.is_complete() {
                willow_gc_safepoint();
                assert!(
                    std::time::Instant::now() < deadline,
                    "required request starved"
                );
                assert!(can_reserve(&mut runtime().heap.lock().unwrap(), 0));
                std::thread::yield_now();
            }
            assert!(can_reserve(&mut runtime().heap.lock().unwrap(), 0));
        }
        stop.store(true, Ordering::Release);
        while threads.iter().any(|thread| !thread.is_finished()) {
            willow_gc_safepoint();
            std::thread::yield_now();
        }
        for thread in threads {
            thread.join().unwrap();
        }
        coordinator::shutdown();
        let cycles = runtime().heap.lock().unwrap().major_collections - before;
        assert!(cycles >= 8);
        assert!(progress.load(Ordering::Relaxed) > 0);
        println!(
            "m3_saturation mutators=4 mandatory_requests=8 completed_cycles={cycles} optional_rejections=16 hard_limit={LIMIT} mutator_progress={}",
            progress.load(Ordering::Relaxed)
        );
        drop(_main);
        reset_internal_for_test();
    }
}
