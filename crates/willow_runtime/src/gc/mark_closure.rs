//! Stopped ownership transfer seals SATB producers; mark work is drained
//! concurrently, then stopped remark publishes phase Inactive.
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
    wait: impl FnMut(std::time::Duration),
) -> Option<(crate::gc_telemetry::MarkWork, sweep::SweepPlan)> {
    finish_with_hooks(cycle, started, wait, || {})
}

// The initial seal and optimistic rechecks leave the third remark for the
// authoritative stopped completion. No producer can extend these limits.
pub(super) const MAX_REMARK_ROUNDS: usize = 3;
pub(super) const MAX_DRAIN_WAVES: usize = 128;

// Queue outstanding counts private segments and work in hand. Flushing once
// at the seal makes hidden jobs stealable; the election does not repeatedly
// scan worker slots or mutator buffers. Ownership covers pre-enqueue claims,
// root enumeration and metadata merges after the last queue item completes.
fn request_remark(cycle: &ConcurrentCycle) -> bool {
    let generation = cycle.termination.sample();
    let publication = cycle.queue.publication_generation();
    cycle.termination.request(
        generation,
        cycle.closing.load(Ordering::Acquire)
            && cycle.satb_shards.is_drained()
            && cycle.queue.current_epoch() == cycle.mark_epoch
            && cycle.queue.snapshot().is_drained()
            && cycle.active_drains.load(Ordering::Acquire) == 0
            && cycle.queue.publication_generation() == publication,
    )
}

fn require_stopped_completion(
    cycle: &ConcurrentCycle,
) -> Option<(crate::gc_telemetry::MarkWork, sweep::SweepPlan)> {
    cycle
        .stopped_completion_required
        .store(true, Ordering::Release);
    None
}

pub(super) fn finish_with_hooks(
    cycle: &Arc<ConcurrentCycle>,
    started: std::time::Instant,
    mut wait: impl FnMut(std::time::Duration),
    mut before_recheck: impl FnMut(),
) -> Option<(crate::gc_telemetry::MarkWork, sweep::SweepPlan)> {
    // No new allocation assists are needed during closure. Already-enrolled
    // assists must finish both payload reads and their batched metadata merge.
    with_stw(
        crate::gc_telemetry::stops::StopReason::Remark,
        |coord, _| {
            let mut state = runtime().heap.lock().unwrap();
            GC_MARK_PHASE.store(2, Ordering::Release);
            cycle.closing.store(true, Ordering::Release);
            // Switch deletion publishers to immediate enqueue before this one
            // whole-buffer drain. Closure never rescans M buffers for each work wave.
            // The drain also seals the shard buffers: from here on every barrier
            // publication takes this heap lock.
            flush_satb_all_locked(coord, &mut state);
            cycle.queue.flush_all_locals();
            GC_MARK_PHASE.store(1, Ordering::Release);
        },
    );
    let mut remark_rounds = 1;
    for wave in 0..MAX_DRAIN_WAVES {
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
            if wave + 1 == MAX_DRAIN_WAVES {
                return require_stopped_completion(cycle);
            }
            wait(delay);
            continue;
        }
        if remark_rounds >= MAX_REMARK_ROUNDS - 1 {
            return require_stopped_completion(cycle);
        }
        cycle.termination.restart();
        if !request_remark(cycle) {
            wait(delay);
            continue;
        }
        before_recheck();
        remark_rounds += 1;
        let finished = with_stw(
            crate::gc_telemetry::stops::StopReason::Remark,
            |coord, _| {
                let mut state = runtime().heap.lock().unwrap();
                GC_MARK_PHASE.store(2, Ordering::Release);
                flush_satb_all_locked(coord, &mut state);
                // The flush may publish fresh work and invalidate the optimistic
                // candidate. Elect again under the stopped producer boundary.
                cycle.termination.restart();
                if request_remark(cycle) && cycle.termination.requested() {
                    // Read exception state after the reader acquire. A finishing drain
                    // can merge a deferred hook just before dropping its reader count.
                    if cycle.worker_failed.load(Ordering::Acquire)
                        || !cycle.deferred.lock().unwrap().is_empty()
                    {
                        GC_MARK_PHASE.store(1, Ordering::Release);
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
                            panic!(
                                "willow gc: invalid GC pointer in concurrent closure: {message}"
                            );
                        }
                    }
                    let mut work = *cycle.work.lock().unwrap();
                    work.mark_ns = crate::gc_telemetry::elapsed_ns(started);
                    // Buffers bump without the heap mutex. Retire them first so none
                    // allocates white after phase 0 into a region missing sweep_pending.
                    old_lab::retire_all(&mut state);
                    GC_MARK_PHASE.store(0, Ordering::Release);
                    marking::set_concurrent_cycle(&mut state, None);
                    assert_eq!(
                        cycle.queue.end_epoch(),
                        0,
                        "concurrent closure lost a publisher"
                    );
                    let plan = sweep::prepare(&mut state, Some(cycle.clone()));
                    return Some((work, plan));
                }
                cycle.termination.restart();
                GC_MARK_PHASE.store(1, Ordering::Release);
                None
            },
        );
        if finished.is_some()
            || cycle.worker_failed.load(Ordering::Acquire)
            || !cycle.deferred.lock().unwrap().is_empty()
        {
            return finished;
        }
        if remark_rounds >= MAX_REMARK_ROUNDS - 1 {
            return require_stopped_completion(cycle);
        }
        wait(delay);
    }
    require_stopped_completion(cycle)
}

/// Authoritative stopped completion shared by the collector and cap tests.
pub(super) fn finish_stopped(
    marking: &Arc<ConcurrentCycle>,
    started: std::time::Instant,
) -> (
    crate::gc_telemetry::MarkWork,
    Option<sweep::SweepPlan>,
    usize,
) {
    with_stw(
        crate::gc_telemetry::stops::StopReason::Remark,
        |coord, stop_work| {
            GC_MARK_PHASE.store(2, Ordering::Release);
            let mut state = runtime().heap.lock().unwrap();
            flush_satb_all_locked(coord, &mut state);
            retire_tlabs_with_work(&mut state, stop_work);
            drop(state);
            let root_job = marking.termination.enter();
            let mut roots = all_registered_stack_roots(coord);
            roots.extend(runtime_roots_snapshot());
            stop_work.root_values += roots.len() as u64;
            for &root in &roots {
                checked_payload_to_header(root, "GC remark root");
                marking.enqueue(root);
            }
            drop(root_job);
            let mut fallback = marking.worker_failed.load(Ordering::Acquire)
                || marking.stopped_completion_required.load(Ordering::Acquire)
                // Deferred STW-only hooks can coexist with indefinitely retrying
                // slices. Never drain concurrent hooks without a bound here.
                || !marking.deferred.lock().unwrap().is_empty();
            marking.queue.flush_all_locals();
            if !fallback {
                // Roots discovered at this stop may introduce legacy hooks or
                // Retry slices even when the concurrent sample contained none.
                marking.drain_checked_until(256, None);
                fallback = marking.worker_failed.load(Ordering::Acquire)
                    || !marking.queue.snapshot().is_drained()
                    || !marking.deferred.lock().unwrap().is_empty();
            }
            if fallback {
                crate::gc_telemetry::workers::record_failure(
                    crate::gc_telemetry::workers::Failure::Fallback,
                );
            }
            for &address in marking.unindexed.lock().unwrap().iter() {
                checked_payload_to_header(address as *mut u8, "GC concurrent graph edge");
            }
            // This remaining remark pass also supplies fallback candidates;
            // never enumerate the entire heap a second time on failure.
            let objects = if fallback {
                epoch_objects(&runtime().heap.lock().unwrap(), stop_work)
            } else {
                Vec::new()
            };
            let deferred = if fallback {
                // Re-trace every discovered object, including SATB-deleted
                // roots and a job whose concurrent hook unwound. The STW
                // hook is the authoritative fallback for native containers.
                objects
                    .iter()
                    .map(|object| object.payload().as_ptr() as usize)
                    .filter(|&address| marking.is_marked(address))
                    .collect()
            } else {
                marking.deferred.lock().unwrap().clone()
            };
            let deferred_set: HashSet<_> = deferred.iter().copied().collect();
            {
                for object in objects {
                    let address = object.payload().as_ptr() as usize;
                    if (!marking.objects.contains(address) || marking.is_marked(address))
                        && !deferred_set.contains(&address)
                    {
                        object.begin_trace();
                    }
                }
            }
            // Extension callbacks without a concurrent contract remain sound.
            let legacy = mark_worklist(
                deferred.into_iter().map(|value| value as *mut u8).collect(),
                (!fallback).then_some((&marking.objects, &deferred_set)),
            );
            let mut work = *marking.work.lock().unwrap();
            // Authoritative fallback retraces the entire discovered graph.
            // Keep unique live bytes, while scan/CPU work includes repeated work.
            work.marked_bytes = if fallback {
                legacy.marked_bytes
            } else {
                work.marked_bytes.saturating_add(legacy.marked_bytes)
            };
            work.scanned_bytes = work.scanned_bytes.saturating_add(legacy.scanned_bytes);
            work.root_scan_bytes = work
                .root_scan_bytes
                .saturating_add(roots.len() as u64 * std::mem::size_of::<usize>() as u64);
            work.mark_ns = crate::gc_telemetry::elapsed_ns(started);
            let mut state = runtime().heap.lock().unwrap();
            // No buffer may allocate white between phase 0 and sweep_pending.
            old_lab::retire_all(&mut state);
            GC_MARK_PHASE.store(0, Ordering::Release);
            marking::set_concurrent_cycle(&mut state, None);
            drop(state);
            let remaining = marking.queue.end_epoch();
            assert!(
                fallback || remaining == 0,
                "completed mark epoch retained work"
            );
            if fallback {
                (work, None, sweep_with_work(stop_work))
            } else {
                let plan =
                    sweep::prepare(&mut runtime().heap.lock().unwrap(), Some(marking.clone()));
                (work, Some(plan), 0)
            }
        },
    )
}

#[cfg(test)]
mod termination_tests {
    use super::*;
    use crate::gc_mark_queue::{MarkWork, MarkWorkItem, ObjectRef};

    fn cycle() -> ConcurrentCycle {
        ConcurrentCycle::new([], IdSet::default(), IdMap::default(), 0)
    }

    fn seal(cycle: &ConcurrentCycle) {
        cycle.closing.store(true, Ordering::Release);
        let coord = GcCoord {
            stop_requested: true,
            ..Default::default()
        };
        cycle.satb_shards.seal_stopped(&coord, |values| {
            cycle.enqueue_satb_batch(values);
        });
    }

    #[test]
    fn termination_requires_satb_seal_and_root_job_completion() {
        let cycle = cycle();
        assert!(!request_remark(&cycle));
        let root_job = cycle.termination.enter();
        seal(&cycle);
        assert!(!request_remark(&cycle));
        drop(root_job);
        assert!(request_remark(&cycle));
    }

    #[test]
    fn termination_hidden_published_and_last_in_hand_work_defeat_election() {
        for local in [false, true] {
            let cycle = cycle();
            seal(&cycle);
            let mut worker = cycle.queue.register_assist();
            let item = MarkWorkItem::Object(ObjectRef::from_addr(8).unwrap());
            if local {
                worker.push(item).unwrap();
            } else {
                cycle
                    .queue
                    .inject(MarkWork::new(cycle.mark_epoch, item))
                    .unwrap();
            }
            assert!(!request_remark(&cycle));
            let generation = cycle.queue.publication_generation();
            if local {
                assert_eq!(cycle.queue.flush_all_locals(), 1);
                assert!(cycle.queue.publication_generation() > generation);
            }
            assert!(!request_remark(&cycle));
            assert!(worker.next_work().is_some());
            assert!(!request_remark(&cycle));
            worker.complete_current();
            // Engine ownership extends beyond queue completion to batch merge.
            let merge = cycle.termination.enter();
            assert!(!request_remark(&cycle));
            drop(merge);
            drop(worker);
            assert!(request_remark(&cycle));
            assert_eq!(cycle.queue.snapshot().counter_underflows, 0);
        }
    }

    #[test]
    fn termination_epoch_rollover_rejects_stale_cycle_and_worker() {
        let cycle = cycle();
        seal(&cycle);
        let mut stale = cycle.queue.register_assist();
        cycle.queue.end_epoch();
        cycle.queue.begin_epoch();
        assert!(!request_remark(&cycle));
        let mut children = Vec::new();
        assert_eq!(cycle.drain_worker(1, &mut children, &mut stale), (0, 0));
        assert_eq!(cycle.active_drains.load(Ordering::Acquire), 0);
        drop(stale);
        assert_eq!(cycle.queue.snapshot().counter_underflows, 0);
    }

    #[test]
    fn termination_repeated_restarts_and_empty_assists_preserve_progress() {
        let cycle = cycle();
        seal(&cycle);
        for _ in 0..256 {
            assert!(request_remark(&cycle));
            // No pacer or worker pool participates in the proof.
            assert_eq!(cycle.drain(1), 0);
            assert!(!cycle.termination.requested());
            assert!(request_remark(&cycle));
            cycle.termination.restart();
        }
        assert!(request_remark(&cycle));
        assert_eq!(cycle.queue.snapshot().counter_underflows, 0);
    }
}
