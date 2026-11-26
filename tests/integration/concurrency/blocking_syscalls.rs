use super::*;

#[cfg(target_os = "linux")]
#[test]
fn bsys_01_slow_blocking_io_keeps_scheduler_alive() {
    use std::io::Write;
    let id = unique_test_id();
    let fifo = format!("/tmp/willow_bsys_{id}");
    let marker = format!("/tmp/willow_bsys_blocked_{id}");
    let status = std::process::Command::new("mkfifo")
        .arg(&fifo)
        .status()
        .expect("mkfifo");
    assert!(status.success(), "mkfifo failed");
    let source = format!(
        r#"
async fn main() {{
    test_observe_next_task();
    let t = fs::read_to_string_async("{fifo}");
    let reader = test_observed_task();
    if reader <= 0 {{ panic("missing FIFO reader task"); }}
    while !test_task_blocked_syscall(reader) {{ await yield(); }}
    match fs::write_string("{marker}", "blocked-syscall") {{
        Ok(_) => {{}}, Err(_) => {{ panic("cannot publish blocked reader"); }},
    }}
    match await t {{ Ok(text) => println(text), Err(e) => println("err"), }}
}}
"#
    );
    let writer_path = fifo.clone();
    let writer_marker = marker.clone();
    let writer = std::thread::spawn(move || {
        // A blocking FIFO open would wedge forever if the reader never
        // appears (e.g. the program failed to compile) — and the join below
        // would hang CI with it. Open NON-BLOCKING and retry until a reader
        // connects or a deadline passes (review fix).
        use std::os::unix::fs::OpenOptionsExt;
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(60);
        loop {
            match std::fs::OpenOptions::new()
                .write(true)
                .custom_flags(libc_o_nonblock())
                .open(&writer_path)
            {
                Ok(mut f) => {
                    // Connection alone does not prove scheduler detachment.
                    // The child publishes this marker only after observing
                    // its reader's actual BlockedSyscall lifecycle.
                    while std::fs::read_to_string(&writer_marker).ok().as_deref()
                        != Some("blocked-syscall")
                    {
                        assert!(
                            std::time::Instant::now() < deadline,
                            "FIFO reader never reached BlockedSyscall"
                        );
                        std::thread::yield_now();
                    }
                    f.write_all(b"slow-io").unwrap();
                    break;
                }
                Err(_) if std::time::Instant::now() < deadline => {
                    std::thread::yield_now();
                }
                Err(error) => panic!("FIFO reader did not connect: {error}"),
            }
        }
    });
    let (out, ok) = compile_and_run_with_test_sync(&source, &[("WILLOW_WORKERS", "1")]);
    writer.join().unwrap();
    let _ = std::fs::remove_file(&fifo);
    let _ = std::fs::remove_file(&marker);
    assert!(ok, "{out}");
    assert_eq!(out, "slow-io\n");
}

#[test]
fn seval_03_sync_select_fair_pick() {
    // Sync (eager) select also rotates among simultaneously-ready cases
    // (review fix: only the cooperative form was fair). Both cases must win
    // at least once across 20 rounds.
    let (out, ok) = compile_and_run(
        "fn round(a: Channel<i64>, b: Channel<i64>) -> i64 { a.send(1); b.send(2); let mut picked = 0; select { let _ = a.recv() => { picked = 10; } let v = b.recv() => { picked = v; } } select { let _ = a.recv() => { } let _ = b.recv() => { } default => { } } return picked; }\nfn main() { let a = Channel<i64>::new(); let b = Channel<i64>::new(); let mut saw_first = false; let mut saw_second = false; let mut i = 0; while i < 20 { let picked = round(a, b); if picked == 10 { saw_first = true; } if picked == 2 { saw_second = true; } i = i + 1; } println(saw_first); println(saw_second); }",
    );
    assert!(ok, "{out}");
    assert_eq!(out, "true\ntrue\n");
}
