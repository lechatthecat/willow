//! Deterministic claims on two real OS workers, using the production queue,
//! task-context installation, generated entry ABI and sync safepoint.
use super::*;
use crate::gc::*;

static MODE: AtomicUsize = AtomicUsize::new(0);
static ROOTS: AtomicUsize = AtomicUsize::new(0);
static DEPTH: AtomicUsize = AtomicUsize::new(0);
static FINISHED: AtomicBool = AtomicBool::new(false);
static VISITED: AtomicUsize = AtomicUsize::new(0);
static COUNTER: AtomicUsize = AtomicUsize::new(0);
static ORIGINAL_THREAD: Mutex<Option<std::thread::ThreadId>> = Mutex::new(None);

// This fixture models generated frames: no TLS reference, thread-bound native
// borrow or lock guard crosses the safepoint. Each pointer is reloaded from its
// registered slot. Language panic propagates by return, never Rust unwinding.
unsafe extern "C" fn helper(_: *mut c_void) -> i32 {
    COUNTER.store(
        crate::preempt::willow_sync_poll_counter() as usize,
        Ordering::Relaxed,
    );
    fn recurse(depth: usize) {
        let baseline = willow_root_depth();
        let count = ROOTS.load(Ordering::Relaxed);
        let mut roots = vec![std::ptr::null_mut(); count];
        let mut before = Vec::with_capacity(count);
        for (index, slot) in roots.iter_mut().enumerate() {
            *slot = willow_gc_alloc_slow(willow_gc_current_tlab(), 21, 0, 8, 0);
            unsafe { *slot.cast::<u64>() = (depth * 1000 + index) as u64 };
            before.push(*slot as usize);
            willow_push_root(slot);
        }
        if depth != 0 {
            recurse(depth - 1);
        } else {
            *ORIGINAL_THREAD.lock().unwrap() = Some(std::thread::current().id());
            assert_eq!(current_worker(), 1);
            let name = b"migrating_helper";
            crate::stack_trace::willow_callstack_push(
                name.as_ptr(),
                name.len() as i64,
                std::ptr::null(),
                0,
                1,
                1,
            );
            crate::stack_overflow::willow_stack_helper_set(name.as_ptr(), name.len() as i64);
            crate::reference_debug::willow_debug_reference_call_scope_push();
            let label = crate::string::willow_string_from_str("migration-reference");
            crate::reference_debug::willow_debug_reference_call(
                label, 1, 1, label, label, label, label, label, label,
            );
            if MODE.load(Ordering::Relaxed) == 1 {
                crate::panic_context::raise_language_message("migration unwind");
            }
            let cancelled = crate::preempt::willow_sync_safepoint();
            assert_eq!(cancelled, i32::from(MODE.load(Ordering::Relaxed) == 2));
            assert_eq!(current_worker(), 2);
            assert_ne!(
                Some(std::thread::current().id()),
                *ORIGINAL_THREAD.lock().unwrap()
            );
            assert!(crate::stack_trace::current_call_stack_text().contains("migrating_helper"));
            assert_eq!(
                crate::reference_debug::current_reference_call()
                    .unwrap()
                    .callee,
                "migration-reference"
            );
            crate::reference_debug::clear_current_reference_call();
            assert_eq!(
                crate::stack_overflow::replace_task_helper(Some(name)),
                Some(&name[..])
            );
            assert_eq!(
                crate::panic_context::willow_panic_depth(),
                i32::from(MODE.load(Ordering::Relaxed) == 1)
            );
            crate::stack_trace::willow_callstack_pop();
            // Allocation after migration must acquire this worker's TLAB.
            let fresh = willow_gc_alloc_slow(willow_gc_current_tlab(), 21, 0, 8, 0);
            unsafe { *fresh.cast::<u64>() = 123 };
            assert_eq!(unsafe { *fresh.cast::<u64>() }, 123);
        }
        for (index, slot) in roots.iter().enumerate() {
            assert_ne!(
                *slot as usize, before[index],
                "collector must rewrite the root"
            );
            assert_eq!(
                unsafe { *slot.cast::<u64>() },
                (depth * 1000 + index) as u64
            );
            VISITED.fetch_add(1, Ordering::Relaxed);
        }
        // The saved depth belongs to the task, not either worker's prefix.
        willow_pop_roots(willow_root_depth() - baseline);
        assert_eq!(willow_root_depth(), baseline);
    }
    assert_eq!(willow_root_depth(), 0);
    recurse(DEPTH.load(Ordering::Relaxed));
    assert_eq!(willow_root_depth(), 0);
    FINISHED.store(true, Ordering::Relaxed);
    if MODE.load(Ordering::Relaxed) == 1 {
        RUNTIME_POLL_PANICKED
    } else {
        RUNTIME_POLL_READY
    }
}

unsafe extern "C" fn poll(_: *mut c_void) -> i32 {
    let result = willow_task_stack_enter_generated(helper, std::ptr::null_mut());
    willow_task_stack_leave();
    result
}

unsafe extern "C" fn cached_helper(_: *mut c_void) -> i32 {
    assert_eq!(current_worker(), 2);
    assert_eq!(willow_root_depth(), 0);
    assert!(crate::reference_debug::current_reference_call().is_none());
    assert_eq!(
        crate::preempt::willow_sync_poll_counter() as usize,
        COUNTER.load(Ordering::Relaxed)
    );
    RUNTIME_POLL_READY
}

unsafe extern "C" fn cached_poll(_: *mut c_void) -> i32 {
    let result = willow_task_stack_enter_generated(cached_helper, std::ptr::null_mut());
    willow_task_stack_leave();
    result
}

fn run_claim(id: u64, worker: usize, prefix: usize, terminal: bool) {
    std::thread::spawn(move || {
        CURRENT_WORKER.with(|slot| slot.set(worker));
        let _registration = MutatorRegistration::new();
        let mut outer = vec![std::ptr::null_mut(); prefix];
        for slot in &mut outer {
            willow_push_root(slot);
        }
        let (claimed, work) = claim_global_ready_for_worker(worker, None)
            .expect("claim must not reroute migratable stack");
        assert_eq!(claimed, id);
        assert!(work.direct_poll);
        let (poll, frame, flag) = work.poll.unwrap();
        crate::preempt::begin_quantum(
            crate::preempt::PreemptConfig::from_env_values(Some("1"), Some("100000")),
            flag,
        );
        let result = unsafe { poll(frame) };
        crate::preempt::willow_preempt_end();
        assert_eq!(gc_thread_root_depth(), prefix);
        assert_eq!(willow_root_depth(), prefix as i32);
        assert!(!crate::stack_trace::current_call_stack_text().contains("migrating_helper"));
        if terminal {
            assert!(FINISHED.load(Ordering::Relaxed));
            match MODE.load(Ordering::Relaxed) {
                1 => {
                    assert_eq!(result, RUNTIME_POLL_PANICKED);
                    crate::panic_context::willow_panic_enter_defer();
                    let info = crate::panic_context::willow_panic_recover();
                    assert!(!info.is_null());
                    crate::panic_context::willow_panic_release_recovered(info);
                    crate::panic_context::willow_panic_leave_defer();
                    with_global(|sched| sched.finalize_panicked(id));
                }
                2 => {
                    assert_eq!(result, RUNTIME_POLL_PREEMPTED);
                    with_global(|sched| sched.finalize_cancelled(id));
                }
                _ => {
                    assert_eq!(result, RUNTIME_POLL_READY);
                    with_global(|sched| sched.complete(id));
                }
            }
        } else {
            assert_eq!(result, RUNTIME_POLL_PREEMPTED);
            assert!(!FINISHED.load(Ordering::Relaxed));
            finish_global_poll_boundary(id, GlobalPollBoundary::Runnable);
        }
        set_current_task(None);
        if terminal {
            // Reuse the migrated coroutine while its destination worker stays
            // alive. The trampoline must not reuse its original TLS address.
            let next = willow_sched_spawn_cooperative(cached_poll, std::ptr::null_mut()) as u64;
            let (claimed, work) = claim_global_ready_for_worker(worker, None).unwrap();
            assert_eq!(claimed, next);
            let (poll, frame, flag) = work.poll.unwrap();
            crate::preempt::begin_quantum(
                crate::preempt::PreemptConfig::from_env_values(Some("1"), Some("100000")),
                flag,
            );
            assert_eq!(unsafe { poll(frame) }, RUNTIME_POLL_READY);
            crate::preempt::willow_preempt_end();
            with_global(|sched| sched.complete(next));
            set_current_task(None);
            assert_eq!(gc_thread_root_depth(), prefix);
        }
        willow_pop_roots(prefix as i32);
        assert_eq!(willow_root_depth(), 0);
    })
    .join()
    .unwrap();
}

#[test]
fn generated_stack_migrates_roots_and_context_after_moving_collection() {
    let _guard = runtime_test_guard();
    for mode in 0..3 {
        for count in [0, 1, 8, 64] {
            for depth in [0, 7] {
                reset_global_scheduler_for_test();
                replace_global_scheduler_for_test(3);
                MODE.store(mode, Ordering::Relaxed);
                ROOTS.store(count, Ordering::Relaxed);
                DEPTH.store(depth, Ordering::Relaxed);
                FINISHED.store(false, Ordering::Relaxed);
                VISITED.store(0, Ordering::Relaxed);
                let baseline = runtime_root_count();
                let parked = parked_root_counts_for_test();
                let id = willow_sched_spawn_cooperative(poll, std::ptr::null_mut()) as u64;
                run_claim(id, 1, 1, false);
                assert_eq!(
                    parked_root_counts_for_test(),
                    (parked.0 + 1, parked.1 + count * (depth + 1))
                );
                minor_collect_internal();
                if mode == 2 {
                    willow_sched_cancel(id);
                }
                run_claim(id, 2, 5, true);
                assert_eq!(VISITED.load(Ordering::Relaxed), count * (depth + 1));
                assert_eq!(parked_root_counts_for_test(), parked);
                assert_eq!(runtime_root_count(), baseline);
                assert_eq!(crate::native_stack::required_workers(), 0);
                println!(
                    "mode={mode} depth={depth} roots_per_frame={count} rewritten_slots={}",
                    VISITED.load(Ordering::Relaxed)
                );
            }
        }
    }
    reset_global_scheduler_for_test();
}
