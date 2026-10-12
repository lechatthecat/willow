//! Budget waiters cooperate with publication/parking, then sleep until a
//! participation transition or completion. All predicates share coord's lock.
use super::*;

std::thread_local! {
    // Preserve the collector's non-waiting budget path even inside an STW
    // callback that already owns coord. Native drop hooks remain leaf-only.
    static IS_COLLECTOR: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

pub(super) struct CollectorGuard(Option<std::sync::MutexGuard<'static, ()>>);

impl CollectorGuard {
    pub(super) fn try_acquire() -> Option<Self> {
        if IS_COLLECTOR.get() {
            return None;
        }
        let mut coord = runtime().coord.0.lock().unwrap_or_else(|p| p.into_inner());
        let serialize = match runtime().collect_lock.try_lock() {
            Ok(guard) => guard,
            Err(std::sync::TryLockError::Poisoned(p)) => p.into_inner(),
            Err(std::sync::TryLockError::WouldBlock) => return None,
        };
        // Election and publication are atomic to waiters, including the gap
        // before the heap exposes a mark epoch and after sweep accounting.
        coord.cycle_collector = Some(std::thread::current().id());
        IS_COLLECTOR.set(true);
        Some(Self(Some(serialize)))
    }
}

impl Drop for CollectorGuard {
    fn drop(&mut self) {
        let mut coord = runtime().coord.0.lock().unwrap_or_else(|p| p.into_inner());
        // Release serialization before publishing completion, including skip
        // and unwind paths. A new election cannot interleave under coord.
        drop(self.0.take());
        IS_COLLECTOR.set(false);
        coord.cycle_collector = None;
        coord.recovery_requested_for = None;
        coord.cycle_generation = coord
            .cycle_generation
            .checked_add(1)
            .expect("collection generation exhausted");
        runtime().budget_cv.notify_all();
    }
}

fn participation_required(coord: &GcCoord) -> bool {
    coord.stop_requested
}

#[cfg(test)]
pub(super) static SLEEPS: AtomicUsize = AtomicUsize::new(0);
#[cfg(test)]
pub(super) static CHECKS: AtomicUsize = AtomicUsize::new(0);

pub(super) fn collect_for_budget() {
    if IS_COLLECTOR.get() {
        return;
    }
    let me = std::thread::current().id();
    let mut coord = runtime().coord.0.lock().unwrap_or_else(|p| p.into_inner());
    let generation = coord.cycle_generation;
    if coord.cycle_collector == Some(me) {
        return;
    }
    if coord.cycle_collector.is_none() {
        drop(coord);
        collect_internal();
        coord = runtime().coord.0.lock().unwrap_or_else(|p| p.into_inner());
    }
    wait_for_cycle(coord, generation);
}

/// Wait for an elected collector without consuming an automatic request.
/// The same condition-variable protocol joins sweep completion and services
/// each subsequent stop while this registered waiter remains enrolled.
pub(super) fn wait_for_current_cycle() {
    let coord = runtime().coord.0.lock().unwrap_or_else(|p| p.into_inner());
    let generation = coord.cycle_generation;
    wait_for_cycle(coord, generation);
}

/// Explicit requests finish an active epoch through classified stopped
/// recovery, wait for its sweep publication, then request a full collection.
/// Reentrant native hooks on the elected collector never wait on themselves.
pub(super) fn collect_explicit() {
    if IS_COLLECTOR.get() {
        return;
    }
    let mut coord = runtime().coord.0.lock().unwrap_or_else(|p| p.into_inner());
    if coord.cycle_collector.is_some() {
        let generation = coord.cycle_generation;
        coord.recovery_requested_for = Some(generation);
        wait_for_cycle(coord, generation);
    } else {
        drop(coord);
    }
    collect_for_budget();
}

/// Requests are tied to the elected generation under the same lock used to
/// retire completion. A late request during remark, Recovery or sweep cannot
/// survive guard drop or target a subsequent election.
pub(super) fn take_recovery_request() -> bool {
    let mut coord = runtime().coord.0.lock().unwrap_or_else(|p| p.into_inner());
    let requested = coord.recovery_requested_for.take();
    coord.cycle_collector == Some(std::thread::current().id())
        && requested == Some(coord.cycle_generation)
}

pub(super) fn wait_for_cycle(mut coord: std::sync::MutexGuard<'_, GcCoord>, generation: u64) {
    loop {
        #[cfg(test)]
        CHECKS.fetch_add(1, Ordering::Relaxed);
        if coord.cycle_generation != generation || coord.cycle_collector.is_none() {
            return;
        }
        if participation_required(&coord) {
            // Never sleep through a request that needs this mutator. The
            // safepoint owns root publication and the STW parking protocol.
            drop(coord);
            willow_gc_safepoint();
            coord = runtime().coord.0.lock().unwrap_or_else(|p| p.into_inner());
            continue;
        }
        #[cfg(test)]
        {
            if runtime().heap.lock().unwrap().sweeping.is_some() {
                SWEEP_BUDGET_WAITING.store(true, Ordering::Release);
            } else {
                ELECTION_BUDGET_WAITING.store(true, Ordering::Release);
            }
            SLEEPS.fetch_add(1, Ordering::Release);
        }
        coord = runtime()
            .budget_cv
            .wait(coord)
            .unwrap_or_else(|p| p.into_inner());
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{Duration, Instant};

    fn await_sleeps(count: usize) {
        let deadline = Instant::now() + Duration::from_secs(10);
        while SLEEPS.load(Ordering::Acquire) < count {
            assert!(Instant::now() < deadline, "budget wait never slept");
            std::thread::yield_now();
        }
        // The counter is incremented under coord just before wait releases it.
        // Acquiring coord proves the waiter reached its atomic sleep boundary.
        drop(runtime().coord.0.lock().unwrap());
    }

    fn waiter() -> std::thread::JoinHandle<()> {
        std::thread::spawn(|| {
            let _mutator = MutatorRegistration::new();
            collect_for_budget();
        })
    }

    fn reset_counts() {
        SLEEPS.store(0, Ordering::Relaxed);
        CHECKS.store(0, Ordering::Relaxed);
    }

    #[test]
    fn explicit_collect_joins_active_cycles_without_busy_polling() {
        let _test = runtime_test_guard();
        reset_internal_for_test();
        for count in [1, 4, 16, 32] {
            reset_counts();
            let elected = CollectorGuard::try_acquire().unwrap();
            let generation = runtime().coord.0.lock().unwrap().cycle_generation;
            // Reentrant callbacks on the elected collector must never wait.
            willow_gc_collect();
            assert_eq!(CHECKS.load(Ordering::Relaxed), 0);
            let waiters: Vec<_> = (0..count)
                .map(|_| {
                    std::thread::spawn(|| {
                        let _mutator = MutatorRegistration::new();
                        willow_gc_collect();
                    })
                })
                .collect();
            let deadline = Instant::now() + Duration::from_secs(10);
            while SLEEPS.load(Ordering::Acquire) < count {
                assert!(
                    waiters.iter().all(|w| !w.is_finished()),
                    "explicit collection returned before the elected cycle completed"
                );
                assert!(
                    Instant::now() < deadline,
                    "explicit collection did not wait"
                );
                std::thread::yield_now();
            }
            drop(runtime().coord.0.lock().unwrap());
            assert!(waiters.iter().all(|w| !w.is_finished()));
            runtime().budget_cv.notify_all();
            await_sleeps(2 * count);
            assert!(waiters.iter().all(|w| !w.is_finished()));
            drop(elected);
            for waiter in waiters {
                waiter.join().unwrap();
            }
            // The joined epoch and the subsequently required full collection
            // are distinct. Contending full requests may coalesce; no waiter
            // may return without a post-join collection completing.
            assert!(CHECKS.load(Ordering::Relaxed) >= 3 * count);
            assert!(SLEEPS.load(Ordering::Relaxed) >= 2 * count);
            assert!(runtime().coord.0.lock().unwrap().cycle_generation >= generation + 2);
            assert!(runtime().heap.lock().unwrap().major_collections >= 1);
            println!(
                "explicit GC waiters={count} sleeps={} checks={}",
                SLEEPS.load(Ordering::Relaxed),
                CHECKS.load(Ordering::Relaxed)
            );
        }
        reset_internal_for_test();
    }

    fn late_request_is_retired(reason: Option<crate::gc_telemetry::stops::StopReason>) {
        let _test = runtime_test_guard();
        reset_internal_for_test();
        reset_counts();
        let elected = CollectorGuard::try_acquire().unwrap();
        // While COORD is held by a late stopped phase, launch a real explicit
        // request. It must await COORD release, then target this same election.
        let waiter = if let Some(reason) = reason {
            with_stw(reason, |_, _| {
                let (ready_tx, ready_rx) = std::sync::mpsc::channel();
                let waiter = std::thread::spawn(move || {
                    ready_tx.send(()).unwrap();
                    collect_explicit();
                });
                ready_rx.recv().unwrap();
                waiter
            })
        } else {
            runtime().heap.lock().unwrap().sweeping = Some(std::thread::current().id());
            std::thread::spawn(collect_explicit)
        };
        await_sleeps(1);
        {
            let coord = runtime().coord.0.lock().unwrap();
            assert_eq!(coord.recovery_requested_for, Some(coord.cycle_generation));
        }
        if reason.is_none() {
            runtime().heap.lock().unwrap().sweeping = None;
        }
        drop(elected);
        waiter.join().unwrap(); // Also finishes its subsequent full collection.
        let next = CollectorGuard::try_acquire().unwrap();
        assert!(
            !take_recovery_request(),
            "late request leaked into next election"
        );
        assert!(
            runtime()
                .coord
                .0
                .lock()
                .unwrap()
                .recovery_requested_for
                .is_none()
        );
        drop(next);
        reset_internal_for_test();
    }

    #[test]
    fn cycle_scoped_recovery_late_remark_request_does_not_leak() {
        late_request_is_retired(Some(crate::gc_telemetry::stops::StopReason::Remark));
    }
    #[test]
    fn cycle_scoped_recovery_completion_request_does_not_leak() {
        late_request_is_retired(Some(crate::gc_telemetry::stops::StopReason::Recovery));
    }
    #[test]
    fn cycle_scoped_recovery_sweep_request_does_not_leak() {
        late_request_is_retired(None);
    }

    #[test]
    fn budget_wait_spurious_wakeups_and_scaling() {
        let _test = runtime_test_guard();
        reset_internal_for_test();
        for count in [1, 4, 16, 32] {
            reset_counts();
            let elected = CollectorGuard::try_acquire().unwrap();
            let generation = runtime().coord.0.lock().unwrap().cycle_generation;
            let waiters: Vec<_> = (0..count).map(|_| waiter()).collect();
            await_sleeps(count);
            for round in 1..=4 {
                let coord = runtime().coord.0.lock().unwrap();
                runtime().budget_cv.notify_all();
                drop(coord);
                await_sleeps(count * (round + 1));
                assert!(waiters.iter().all(|w| !w.is_finished()));
            }
            // Acknowledgement notifications must not wake sleeping waiters.
            let coord = runtime().coord.0.lock().unwrap();
            runtime().coord.1.notify_all();
            assert_eq!(SLEEPS.load(Ordering::Acquire), count * 5);
            drop(coord);
            drop(elected);
            for worker in waiters {
                worker.join().unwrap();
            }
            assert_eq!(CHECKS.load(Ordering::Relaxed), count * 6);
            assert_eq!(
                runtime().coord.0.lock().unwrap().cycle_generation,
                generation.wrapping_add(1)
            );
            eprintln!(
                "budget_wait scaling waiters={count} sleeps={} checks={}",
                count * 5,
                count * 6
            );
        }
        reset_internal_for_test();
    }

    #[test]
    fn budget_wait_participates_in_real_root_rounds_remark_and_recovery() {
        let _test = runtime_test_guard();
        reset_internal_for_test();
        reset_counts();
        let elected = CollectorGuard::try_acquire().unwrap();
        let workers: Vec<_> = (0..16).map(|_| waiter()).collect();
        await_sleeps(16);
        let (_, marking) = root_handshake::begin().unwrap();
        assert!(!runtime().coord.0.lock().unwrap().stop_requested);
        let failed = std::panic::catch_unwind(|| {
            with_stw(
                crate::gc_telemetry::stops::StopReason::Remark,
                |coord, _| {
                    assert_eq!(coord.parked.len(), 16);
                    panic!("injected remark failure");
                },
            );
        });
        assert!(failed.is_err());
        with_stw(
            crate::gc_telemetry::stops::StopReason::Recovery,
            |coord, _| {
                assert_eq!(coord.parked.len(), 16);
                let mut state = runtime().heap.lock().unwrap();
                marking::set_concurrent_cycle(&mut state, None);
                GC_MARK_PHASE.store(0, Ordering::Release);
                marking.queue.end_epoch();
            },
        );
        with_stw(crate::gc_telemetry::stops::StopReason::Minor, |coord, _| {
            assert_eq!(coord.parked.len(), 16);
        });
        drop(elected);
        for worker in workers {
            worker.join().unwrap();
        }
        reset_internal_for_test();
    }

    #[test]
    fn budget_wait_unwind_completes_generation_and_allows_next_election() {
        let _test = runtime_test_guard();
        reset_internal_for_test();
        reset_counts();
        let mut workers = Vec::new();
        let failed = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let _elected = CollectorGuard::try_acquire().unwrap();
            workers = (0..8).map(|_| waiter()).collect();
            await_sleeps(8);
            panic!("injected collector failure before publication");
        }));
        assert!(failed.is_err());
        for worker in workers {
            worker.join().unwrap();
        }
        let next = CollectorGuard::try_acquire().unwrap();
        drop(next);
        reset_internal_for_test();
    }

    #[test]
    fn budget_wait_self_collection_and_completion_before_wait() {
        let _test = runtime_test_guard();
        reset_internal_for_test();
        reset_counts();
        let elected = CollectorGuard::try_acquire().unwrap();
        collect_for_budget();
        with_stw(crate::gc_telemetry::stops::StopReason::Remark, |_, _| {
            // Preserve self-collection semantics while owning coord's mutex.
            collect_for_budget();
        });
        assert_eq!(SLEEPS.load(Ordering::Relaxed), 0);
        drop(elected);
        // Completion before entry must not sleep on an already-published event.
        collect_for_budget();
        assert_eq!(SLEEPS.load(Ordering::Relaxed), 0);
        reset_internal_for_test();
    }

    #[test]
    fn budget_wait_completed_generation_does_not_join_a_later_cycle() {
        let _test = runtime_test_guard();
        reset_internal_for_test();
        reset_counts();
        let old_generation = runtime().coord.0.lock().unwrap().cycle_generation;
        let previous = CollectorGuard::try_acquire().unwrap();
        drop(previous);
        let next = CollectorGuard::try_acquire().unwrap();
        // Deterministically model completion and immediate re-election before
        // a delayed waiter reacquires coord. It owes only the previous cycle.
        std::thread::spawn(move || {
            let coord = runtime().coord.0.lock().unwrap();
            wait_for_cycle(coord, old_generation);
        })
        .join()
        .unwrap();
        assert_eq!(SLEEPS.load(Ordering::Relaxed), 0);
        drop(next);
        reset_internal_for_test();
    }

    // The previous production algorithm, retained only as a measured baseline.
    // Its heap lookup, election probe, safepoint and yield are all preserved.
    fn legacy_budget_wait() -> usize {
        let mut observed = false;
        let mut checks = 0;
        loop {
            checks += 1;
            let collector = {
                let state = runtime().heap.lock().unwrap();
                state
                    .concurrent_cycle
                    .as_ref()
                    .map(|c| c.collector)
                    .or(state.sweeping)
            };
            match collector {
                Some(id) if id == std::thread::current().id() => return checks,
                Some(_) => {
                    observed = true;
                    willow_gc_safepoint();
                    std::thread::yield_now();
                }
                None if observed => {
                    let busy = match runtime().collect_lock.try_lock() {
                        Ok(_) | Err(std::sync::TryLockError::Poisoned(_)) => false,
                        Err(std::sync::TryLockError::WouldBlock) => true,
                    };
                    if !busy {
                        return checks;
                    }
                    willow_gc_safepoint();
                    std::thread::yield_now();
                }
                None => {
                    observed = true;
                    collect_internal();
                }
            }
        }
    }

    #[test]
    #[ignore = "opt-in waiter CPU comparison, 100ms holds per data point"]
    fn budget_wait_cpu_measurement() {
        let _test = runtime_test_guard();
        reset_internal_for_test();
        for count in [1, 4, 16, 32] {
            for legacy in [true, false] {
                reset_counts();
                // Reservation rejection models a hard budget with no room to
                // grow while a collector owns the unpublished election phase.
                runtime().heap.lock().unwrap().memory_limit_bytes = Some(0);
                let elected = CollectorGuard::try_acquire().unwrap();
                let ready = Arc::new(std::sync::Barrier::new(count + 1));
                let workers: Vec<_> = (0..count)
                    .map(|_| {
                        let ready = ready.clone();
                        std::thread::spawn(move || {
                            let _mutator = MutatorRegistration::new();
                            assert!(!can_reserve(&runtime().heap.lock().unwrap(), 1));
                            ready.wait();
                            let before = crate::gc_telemetry::workers::thread_cpu_ns();
                            let checks = if legacy {
                                legacy_budget_wait()
                            } else {
                                collect_for_budget();
                                0
                            };
                            let cpu = before
                                .zip(crate::gc_telemetry::workers::thread_cpu_ns())
                                .map(|(before, after)| after.saturating_sub(before));
                            (checks, cpu)
                        })
                    })
                    .collect();
                ready.wait();
                if !legacy {
                    await_sleeps(count);
                }
                std::thread::sleep(Duration::from_millis(100));
                drop(elected);
                let results: Vec<_> = workers.into_iter().map(|w| w.join().unwrap()).collect();
                let checks = if legacy {
                    results.iter().map(|r| r.0).sum()
                } else {
                    CHECKS.load(Ordering::Relaxed)
                };
                let cpu: Option<u64> = results.iter().map(|r| r.1).sum();
                eprintln!(
                    "budget_wait measurement legacy={legacy} waiters={count} checks={checks} sleeps={} cpu_ns={cpu:?}",
                    SLEEPS.load(Ordering::Relaxed)
                );
                if !legacy {
                    assert_eq!(checks, 2 * count);
                }
            }
        }
        reset_internal_for_test();
    }
}
