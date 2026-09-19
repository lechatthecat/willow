use super::*;

static ENTERED: AtomicUsize = AtomicUsize::new(0);
static FINISH: AtomicBool = AtomicBool::new(false);

unsafe extern "C" fn deep_affinity_poll(_: *mut c_void) -> i32 {
    fn recurse(depth: usize) -> usize {
        let owner = std::thread::current().id();
        if depth == 0 {
            ENTERED.fetch_add(1, Ordering::SeqCst);
            // All four workers must own a stack before any can run a second
            // task. Bound the rendezvous so a regression fails instead of hangs.
            let deadline = Instant::now() + Duration::from_secs(5);
            while ENTERED.load(Ordering::SeqCst) < 4 {
                assert!(Instant::now() < deadline);
                std::thread::yield_now();
            }
            while !FINISH.load(Ordering::Acquire)
                && willow_sched_is_cancelled(willow_sched_current_task()) == 0
            {
                assert!(crate::native_stack::suspend());
                assert_eq!(std::thread::current().id(), owner);
            }
            return 1;
        }
        let result = recurse(depth - 1);
        assert_eq!(std::thread::current().id(), owner);
        std::hint::black_box(result + depth)
    }
    assert_eq!(recurse(64), 2081);
    RUNTIME_POLL_READY
}

#[test]
fn native_affinity_survives_worker_shrink_and_clears_on_completion_or_cancel() {
    let _guard = crate::gc::runtime_test_guard();
    for cancel in [false, true] {
        reset_global_scheduler_for_test();
        replace_global_scheduler_for_test(4);
        ENTERED.store(0, Ordering::SeqCst);
        FINISH.store(false, Ordering::Release);
        assert_eq!(crate::native_stack::required_workers(), 0);
        let tasks: Vec<_> = (0..4)
            .map(|_| willow_sched_spawn(deep_affinity_poll, std::ptr::null_mut()))
            .collect();
        crate::gc::willow_gc_register_mutator();
        assert_eq!(
            willow_sched_run_parallel(None, 4, Some(Instant::now() + Duration::from_millis(100))),
            0
        );
        assert_eq!(ENTERED.load(Ordering::SeqCst), 4);
        assert_eq!(crate::native_stack::required_workers(), 4);
        if cancel {
            for &task in &tasks {
                willow_sched_cancel(task);
            }
        } else {
            FINISH.store(true, Ordering::Release);
        }
        // Request only one worker. All suspended stacks must still resume on
        // their original OS threads, including workers 1, 2 and 3.
        willow_sched_run_parallel(None, 1, Some(Instant::now() + Duration::from_secs(5)));
        crate::gc::willow_gc_unregister_mutator();
        for task in tasks {
            assert_eq!(willow_sched_task_state(task), -1);
        }
        assert_eq!(crate::native_stack::required_workers(), 0);
    }
    reset_global_scheduler_for_test();
}

#[test]
fn native_affinity_queries_scale_with_drives_not_resident_tasks() {
    let _guard = crate::gc::runtime_test_guard();
    println!("resident_tasks,drives,affinity_queries,affinity_updates");
    for residents in [16, 128, 1024] {
        for drives in [1, 8, 64] {
            reset_global_scheduler_for_test();
            replace_global_scheduler_for_test(1);
            with_global(|scheduler| {
                for _ in 0..residents {
                    scheduler.spawn_parked_placeholder();
                }
            });
            assert_eq!(global_task_table().len(), residents);
            let (queries, updates) = crate::native_stack::affinity_counts_for_test();
            crate::gc::willow_gc_register_mutator();
            for _ in 0..drives {
                assert_eq!(willow_sched_run_parallel(None, 1, None), 0);
            }
            crate::gc::willow_gc_unregister_mutator();
            let (after_queries, after_updates) = crate::native_stack::affinity_counts_for_test();
            assert_eq!(after_queries - queries, drives);
            assert_eq!(after_updates - updates, 0);
            println!(
                "{residents},{drives},{},{}",
                after_queries - queries,
                after_updates - updates
            );
        }
    }
    reset_global_scheduler_for_test();
}
