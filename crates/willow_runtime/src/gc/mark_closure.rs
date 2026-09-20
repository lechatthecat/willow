//! Concurrent termination under the barrier publication lock. No mutator is
//! asked to park. The heap mutex excludes external root/barrier publishers;
//! queue accounting and engine reader accounting exclude tracing publishers.
use super::*;

#[cfg(test)]
pub(super) static READER_WAIT_OBSERVED: std::sync::atomic::AtomicBool =
    std::sync::atomic::AtomicBool::new(false);

pub(super) fn finish(
    cycle: &Arc<ConcurrentCycle>,
    started: std::time::Instant,
) -> Option<(crate::gc_telemetry::MarkWork, sweep::SweepPlan)> {
    finish_with_wait(cycle, started, std::thread::sleep)
}

// Separate waiting from policy so contention duration can be tested with
// deterministic virtual waits, without scheduler-dependent timing assertions.
pub(super) fn finish_with_wait(
    cycle: &Arc<ConcurrentCycle>,
    started: std::time::Instant,
    mut wait: impl FnMut(std::time::Duration),
) -> Option<(crate::gc_telemetry::MarkWork, sweep::SweepPlan)> {
    // No new allocation assists are needed during closure. Already-enrolled
    // assists must finish both payload reads and their batched metadata merge.
    {
        let mut state = runtime().heap.lock().unwrap();
        cycle.closing.store(true, Ordering::Release);
        // Switch deletion publishers to immediate enqueue before this one
        // whole-buffer drain. Closure never rescans M buffers for each work wave.
        flush_satb_all_locked(&mut state);
    }
    loop {
        if cycle.worker_failed.load(Ordering::Acquire) || !cycle.deferred.lock().unwrap().is_empty()
        {
            return None; // Explicit stopped native-hook/failure fallback.
        }
        let wave_start = std::time::Instant::now();
        let cpu_start = crate::gc_telemetry::workers::thread_cpu_ns();
        cycle.drain_checked_until(256, Some(wave_start + std::time::Duration::from_millis(1)));
        // Repay this collector's measured CPU before another unfinished wave.
        // Missing/regressing CPU samples use elapsed wall time conservatively.
        // The 1ms floor also bounds empty-reader polling and fast Retry work.
        let delay = cpu_start
            .zip(crate::gc_telemetry::workers::thread_cpu_ns())
            .and_then(|(start, end)| end.checked_sub(start))
            .map(std::time::Duration::from_nanos)
            .unwrap_or_else(|| wave_start.elapsed())
            .max(std::time::Duration::from_millis(1));
        if !cycle.queue.snapshot().is_drained() || cycle.active_drains.load(Ordering::Acquire) != 0
        {
            #[cfg(test)]
            if cycle.active_drains.load(Ordering::Acquire) != 0 {
                READER_WAIT_OBSERVED.store(true, Ordering::Release);
            }
            wait(delay);
            continue;
        }
        let mut state = runtime().heap.lock().unwrap();
        if cycle.queue.snapshot().is_drained() && cycle.active_drains.load(Ordering::Acquire) == 0 {
            // Read exception state after the reader acquire. A finishing drain
            // can merge a deferred hook just before dropping its reader count.
            if cycle.worker_failed.load(Ordering::Acquire)
                || !cycle.deferred.lock().unwrap().is_empty()
            {
                return None;
            }
            // Engine merges precede active_drains reaching zero. All mutator
            // publications require this heap lock, so empty cannot be invalidated
            // between the check and the phase transition. A late empty assist
            // may enroll but cannot acquire an object after end_epoch().
            #[cfg(debug_assertions)]
            {
                let error = cycle.unindexed.lock().unwrap().iter().find_map(|&address| {
                    validate_payload_pointer_locked(&state, address as *mut u8).err()
                });
                if let Some(message) = error {
                    drop(state); // Invalid user roots must not poison the heap.
                    panic!("willow gc: invalid GC pointer in concurrent closure: {message}");
                }
            }
            let mut work = *cycle.work.lock().unwrap();
            work.mark_ns = crate::gc_telemetry::elapsed_ns(started);
            GC_MARK_PHASE.store(0, Ordering::Release);
            state.concurrent_cycle = None;
            assert_eq!(
                cycle.queue.end_epoch(),
                0,
                "concurrent closure lost a publisher"
            );
            let plan = sweep::prepare(&mut state, Some(cycle.clone()));
            return Some((work, plan));
        }
        drop(state);
        wait(delay);
    }
}
