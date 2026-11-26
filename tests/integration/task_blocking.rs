//! Blocking stdout and legacy timer waits must leave one scheduler worker free.
use super::support::*;
use std::time::{Duration, Instant};

#[test]
fn task_print_values_survive_gc_and_preserve_order() {
    let (out, ok, timed_out) = compile_and_run_with_env_timeout(
        r#"
async fn worker() {
    let text = "managed" + " bytes";
    print(42); print(true); print(1.5); println(text);
    defer println("cleanup");
    gc_collect();
    println(text);
}
async fn main() { await worker(); }
"#,
        &[
            ("WILLOW_WORKERS", "1"),
            ("WILLOW_GC_STRESS", "scheduler"),
            ("WILLOW_BLOCKING_QUEUE", "1"),
        ],
        Duration::from_secs(10),
    );
    assert!(!timed_out, "{out}");
    assert!(ok, "{out}");
    assert_eq!(out, "42true1.5managed bytes\nmanaged bytes\ncleanup\n");
}

#[test]
fn full_stdout_pipe_does_not_block_other_tasks() {
    assert_full_stdout_progress(1);
}

#[test]
fn full_stdout_pipe_with_many_writers_does_not_block_other_tasks() {
    for writers in [8, 64] {
        assert_full_stdout_progress(writers);
    }
}

// Observe kernel pipe capacity without consuming any writer output.
#[cfg(unix)]
fn stdout_pipe_full(_: &std::io::PipeReader, writer: &std::io::PipeWriter) -> bool {
    use std::os::fd::AsRawFd;
    #[repr(C)]
    struct PollFd {
        fd: std::ffi::c_int,
        events: std::ffi::c_short,
        revents: std::ffi::c_short,
    }
    #[cfg(target_os = "macos")]
    type Count = std::ffi::c_uint;
    #[cfg(not(target_os = "macos"))]
    type Count = std::ffi::c_ulong;
    unsafe extern "C" {
        fn poll(fds: *mut PollFd, count: Count, timeout: std::ffi::c_int) -> std::ffi::c_int;
    }
    let mut fd = PollFd {
        fd: writer.as_raw_fd(),
        events: 4,
        revents: 0,
    };
    // SAFETY: the writer owns this live descriptor and fd is one writable pollfd.
    let result = unsafe { poll(&raw mut fd, 1, 0) };
    if result < 0 {
        assert_eq!(
            std::io::Error::last_os_error().kind(),
            std::io::ErrorKind::Interrupted
        );
        return false;
    }
    assert_eq!(
        fd.revents & !4,
        0,
        "stdout pipe poll failed: {}",
        fd.revents
    );
    result == 0
}

#[cfg(windows)]
fn stdout_pipe_full(reader: &std::io::PipeReader, _: &std::io::PipeWriter) -> bool {
    use std::os::windows::io::AsRawHandle;
    #[link(name = "kernel32")]
    unsafe extern "system" {
        fn GetNamedPipeInfo(
            handle: *mut std::ffi::c_void,
            flags: *mut u32,
            output_size: *mut u32,
            input_size: *mut u32,
            instances: *mut u32,
        ) -> i32;
        fn PeekNamedPipe(
            handle: *mut std::ffi::c_void,
            buffer: *mut std::ffi::c_void,
            size: u32,
            read: *mut u32,
            available: *mut u32,
            remaining: *mut u32,
        ) -> i32;
    }
    let (mut flags, mut outbound, mut inbound, mut available) = (0, 0, 0, 0);
    // SAFETY: reader owns the pipe handle; output pointers are live and all
    // optional unused outputs are null. Peek does not consume pipe data.
    unsafe {
        assert_ne!(
            GetNamedPipeInfo(
                reader.as_raw_handle(),
                &raw mut flags,
                &raw mut outbound,
                &raw mut inbound,
                std::ptr::null_mut()
            ),
            0
        );
        assert_ne!(
            PeekNamedPipe(
                reader.as_raw_handle(),
                std::ptr::null_mut(),
                0,
                std::ptr::null_mut(),
                &raw mut available,
                std::ptr::null_mut()
            ),
            0
        );
    }
    // Pipe quotas are named from the server's direction. A server reader
    // receives inbound data; a client reader receives outbound data.
    let capacity = if flags & 1 != 0 { inbound } else { outbound };
    assert!(capacity > 0);
    available >= capacity
}

fn assert_full_stdout_progress(writers: usize) {
    let id = unique_test_id();
    let source_path = temp_path(format!("willow_stdout_{id}.wi"));
    let binary = temp_path(format!("willow_stdout_{id}"));
    let marker = temp_path(format!("willow_stdout_progress_{id}"));
    let marker_literal = format!("{marker:?}");
    let release = temp_path(format!("willow_stdout_release_{id}"));
    let release_literal = format!("{release:?}");
    let starts: String = (0..writers)
        .map(|i| format!("let w{i} = writer();"))
        .collect();
    let joins: String = (0..writers).map(|i| format!("await w{i};")).collect();
    let source = format!(
        r#"
import std::fs;
async fn writer() {{
    let mut i = 0;
    while i < 4096 {{ println("{}"); i = i + 1; }}
}}
async fn progress() {{
    while !fs::exists({release_literal}) {{ await yield(); }}
    gc_collect();
    match fs::write_string({}, "progress") {{ Ok(_) => {{}}, Err(_) => {{}} }}
}}
async fn main() {{ {starts} let p = progress(); await p; {joins} }}
"#,
        "x".repeat(4096),
        marker_literal
    );
    fs::write(&source_path, source).unwrap();
    let compiled = Command::new(env!("CARGO_BIN_EXE_willow"))
        .args(["build", &source_path, "-o", &binary])
        .output()
        .unwrap();
    assert!(
        compiled.status.success(),
        "{}",
        String::from_utf8_lossy(&compiled.stderr)
    );
    let (mut stdout, writer) = std::io::pipe().unwrap();
    let mut child = Command::new(&binary)
        .env("WILLOW_WORKERS", "1")
        .env("WILLOW_BLOCKING_THREADS", "1")
        .env("WILLOW_BLOCKING_QUEUE", "1")
        .stdout(writer.try_clone().unwrap())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    // Admit the progress task only after the kernel confirms that stdout
    // cannot accept another atomic write. Never drain the pipe before this.
    let deadline = Instant::now() + Duration::from_secs(30);
    let mut full = false;
    while Instant::now() < deadline {
        if child.try_wait().unwrap().is_some() {
            break;
        }
        if stdout_pipe_full(&stdout, &writer) {
            full = true;
            fs::write(&release, "full").unwrap();
            break;
        }
        std::thread::yield_now();
    }
    while full
        && fs::read_to_string(&marker).ok().as_deref() != Some("progress")
        && Instant::now() < deadline
    {
        if child.try_wait().unwrap().is_some() {
            break;
        }
        std::thread::yield_now();
    }
    let progressed = fs::read_to_string(&marker).ok().as_deref() == Some("progress");
    let _ = child.kill();
    let output = child.wait_with_output().unwrap();
    drop(writer);
    let mut printed = Vec::new();
    std::io::Read::read_to_end(&mut stdout, &mut printed).unwrap();
    let _ = fs::remove_file(&source_path);
    let _ = fs::remove_file(&marker);
    let _ = fs::remove_file(&release);
    remove_output_artifacts(&binary);
    assert!(full, "writer never filled stdout ({writers} writers)");
    assert!(printed.len() >= 4096, "writer never reached stdout");
    assert!(
        progressed,
        "worker stalled behind stdout ({writers} writers): {}",
        String::from_utf8_lossy(&output.stderr)
    );
}

#[test]
fn concurrent_stdout_writers_complete_without_losing_lines() {
    let starts: String = (0..64)
        .map(|i| format!("let w{i} = writer({i});"))
        .collect();
    let joins: String = (0..64).map(|i| format!("await w{i};")).collect();
    let source = format!(
        "async fn writer(n: i64) {{ let mut i = 0; while i < 8 {{ println(n); i = i + 1; }} }}
         async fn main() {{ {starts} {joins} }}"
    );
    for workers in ["1", "5"] {
        let (out, ok, timed_out) = compile_and_run_with_env_timeout(
            &source,
            &[
                ("WILLOW_WORKERS", workers),
                ("WILLOW_BLOCKING_THREADS", "1"),
                ("WILLOW_BLOCKING_QUEUE", "1"),
            ],
            Duration::from_secs(20),
        );
        assert!(!timed_out && ok, "workers={workers}: {out}");
        let mut lines: Vec<usize> = out.lines().map(|line| line.parse().unwrap()).collect();
        lines.sort_unstable();
        let expected: Vec<usize> = (0..64).flat_map(|n| std::iter::repeat_n(n, 8)).collect();
        assert_eq!(lines, expected, "workers={workers}");
    }
}

#[test]
fn task_print_completion_does_not_complete_the_next_await() {
    let source = r#"
async fn value(n: i64, printed: AtomicBool, waiter: i64) -> i64 {
    while !printed.load() { await yield(); }
    while !test_task_parked(waiter) { await yield(); }
    return n;
}
async fn main() {
    let mut i = 1;
    while i <= 256 {
        let printed = AtomicBool::new(false);
        let task = value(i, printed, test_current_task());
        println(i);
        printed.store(true);
        println(await task);
        i = i + 1;
    }
}
"#;
    let expected: String = (1..=256).map(|i| format!("{i}\n{i}\n")).collect();
    for workers in ["1", "5"] {
        let (out, ok) = compile_and_run_with_test_sync(
            source,
            &[("WILLOW_WORKERS", workers), ("WILLOW_BLOCKING_QUEUE", "1")],
        );
        assert!(ok, "workers={workers}: {out}");
        assert_eq!(out, expected, "workers={workers}");
    }
}

#[test]
fn task_await_rechecks_share_one_call_site_per_suspension() {
    for awaits in [1, 8, 64] {
        let source = format!(
            "async fn value() -> i64 {{ return 42; }} \
             async fn main() {{ let task = value(); {} }}",
            "println(await task);".repeat(awaits)
        );
        let targets = compile_and_collect_relocation_targets_all(&source, &[]);
        let calls = targets
            .iter()
            .filter(|name| *name == "willow_frame_await")
            .count();
        assert_eq!(calls, awaits);
        println!("awaits={awaits} terminal_check_call_sites={calls}");
    }
}

#[cfg(target_os = "linux")]
#[test]
fn task_sleep_rechecks_deadline_after_injected_early_wakes() {
    let script = Path::new(env!("CARGO_MANIFEST_DIR")).join("scripts/sleep_wake_repro.py");
    let output = Command::new("python3")
        .arg(script)
        .args(["--compiler", env!("CARGO_BIN_EXE_willow")])
        .output()
        .expect("run deterministic sleep wake regression");
    assert!(
        output.status.success(),
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
}

#[test]
fn task_sleep_deadline_checks_have_linear_code_size() {
    for sleeps in [1, 8, 64] {
        let source = format!("async fn main() {{ {} }}", "await sleep(1);".repeat(sleeps));
        let targets = compile_and_collect_relocation_targets_all(&source, &[]);
        let count = |symbol| targets.iter().filter(|name| *name == symbol).count();
        assert_eq!(count("willow_sched_sleep"), sleeps);
        assert_eq!(count("willow_monotonic_millis"), 2 * sleeps);
        println!(
            "sleeps={sleeps} timer_sites={} clock_sites={}",
            sleeps,
            2 * sleeps
        );
    }
}

#[test]
fn task_sleep_nonpositive_and_repeated_operands_preserve_values() {
    let (out, ok, timed_out) = compile_and_run_with_env_timeout(
        r#"
async fn main() {
    let millis = 1;
    let mut i = 0;
    while i < 3 {
        await sleep(millis);
        println(millis);
        i = i + 1;
    }
    await sleep(0);
    await sleep(-1);
    await sleep(-9223372036854775807 - 1);
    println("done");
}
"#,
        &[("WILLOW_WORKERS", "1"), ("WILLOW_TASK_BUDGET", "1")],
        Duration::from_secs(10),
    );
    assert!(!timed_out, "{out}");
    assert!(ok, "{out}");
    assert_eq!(out, "1\n1\n1\ndone\n");
}
