use super::*;

#[test]
fn persistent_worker_completes_each_drive_once_and_exits_on_disconnect() {
    static ENTERED: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
    fn count_entry() {
        ENTERED.fetch_add(1, Ordering::Relaxed);
    }
    let _guard = crate::gc::runtime_test_guard();
    reset_global_scheduler_for_test();
    for drives in [1, 8, 64] {
        ENTERED.store(0, Ordering::Relaxed);
        let (sender, receiver) = std::sync::mpsc::channel();
        let worker = std::thread::spawn(move || persistent_worker_loop(0, receiver));
        let finished = Arc::new(ParallelCompletion::new(drives));
        for _ in 0..drives {
            sender
                .send(WorkerDrive {
                    target: None,
                    state: Arc::new(ParallelRunState::default()),
                    deadline: None,
                    finished: finished.clone(),
                    entry_hook: Some(count_entry),
                })
                .unwrap();
        }
        drop(sender);
        worker.join().unwrap();
        assert_eq!(ENTERED.load(Ordering::Relaxed), drives);
        assert_eq!(*finished.remaining.lock().unwrap(), 0);
    }
    reset_global_scheduler_for_test();
}

#[test]
fn collector_panic_on_worker_terminates_process() {
    const CHILD: &str = "WILLOW_TEST_COLLECTOR_WORKER_PANIC";
    if std::env::var_os(CHILD).is_some() {
        let _guard = crate::gc::runtime_test_guard();
        crate::gc::reset_internal_for_test();
        crate::gc::willow_gc_add_runtime_root(std::ptr::dangling_mut::<u8>());
        let (sender, receiver) = std::sync::mpsc::channel();
        std::thread::Builder::new()
            .name("willow-worker-0".into())
            .spawn(move || persistent_worker_loop(0, receiver))
            .unwrap();
        let finished = Arc::new(ParallelCompletion::new(1));
        sender
            .send(WorkerDrive {
                target: None,
                state: Arc::new(ParallelRunState::default()),
                deadline: None,
                finished: finished.clone(),
                entry_hook: Some(crate::gc::collect_for_worker_panic_test),
            })
            .unwrap();
        finished.wait();
        panic!("collector panic unexpectedly completed the drive");
    }

    let mut child = std::process::Command::new(std::env::current_exe().unwrap())
        .args([
            "--exact",
            "scheduler::worker_failure_tests::collector_panic_on_worker_terminates_process",
            "--nocapture",
        ])
        .env(CHILD, "1")
        .env("RUST_BACKTRACE", "0")
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .unwrap();
    let deadline = Instant::now() + std::time::Duration::from_secs(10);
    loop {
        if child.try_wait().unwrap().is_some() {
            break;
        }
        if Instant::now() >= deadline {
            child.kill().unwrap();
            let output = child.wait_with_output().unwrap();
            panic!(
                "collector panic hung the worker drive: {}",
                String::from_utf8_lossy(&output.stderr)
            );
        }
        std::thread::sleep(std::time::Duration::from_millis(10));
    }
    let output = child.wait_with_output().unwrap();
    assert!(!output.status.success());
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("willow-worker-0"), "{stderr}");
    assert!(stderr.contains("invalid GC pointer"), "{stderr}");
}
