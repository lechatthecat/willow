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
    let id = unique_test_id();
    let source_path = temp_path(format!("willow_stdout_{id}.wi"));
    let binary = temp_path(format!("willow_stdout_{id}"));
    let marker = temp_path(format!("willow_stdout_progress_{id}"));
    let marker_literal = format!("{marker:?}");
    let source = format!(
        r#"
import std::fs;
async fn writer() {{
    let mut i = 0;
    while i < 4096 {{ println("{}"); i = i + 1; }}
}}
async fn progress() {{
    await sleep(100);
    gc_collect();
    match fs::write_string({}, "progress") {{ Ok(_) => {{}}, Err(_) => {{}} }}
}}
async fn main() {{ let w = writer(); let p = progress(); await p; await w; }}
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
    let mut child = Command::new(&binary)
        .env("WILLOW_WORKERS", "1")
        .env("WILLOW_BLOCKING_THREADS", "1")
        .env("WILLOW_BLOCKING_QUEUE", "1")
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    // Deliberately do not drain stdout until the other task makes progress.
    let deadline = Instant::now() + Duration::from_secs(5);
    while !Path::new(&marker).exists() && Instant::now() < deadline {
        if child.try_wait().unwrap().is_some() {
            break;
        }
        std::thread::sleep(Duration::from_millis(5));
    }
    let progressed = fs::read_to_string(&marker).ok().as_deref() == Some("progress");
    let _ = child.kill();
    let output = child.wait_with_output().unwrap();
    let _ = fs::remove_file(&source_path);
    let _ = fs::remove_file(&marker);
    remove_output_artifacts(&binary);
    assert!(output.stdout.len() >= 4096, "writer never reached stdout");
    assert!(
        progressed,
        "worker stalled behind stdout: {}",
        String::from_utf8_lossy(&output.stderr)
    );
}

#[test]
fn task_print_completion_does_not_complete_the_next_await() {
    let source = r#"
async fn value(n: i64) -> i64 { await sleep(1); return n; }
async fn main() {
    let mut i = 1;
    while i <= 256 {
        let task = value(i);
        println(i);
        println(await task);
        i = i + 1;
    }
}
"#;
    let expected: String = (1..=256).map(|i| format!("{i}\n{i}\n")).collect();
    for workers in ["1", "5"] {
        let (out, ok, timed_out) = compile_and_run_with_env_timeout(
            source,
            &[("WILLOW_WORKERS", workers), ("WILLOW_BLOCKING_QUEUE", "1")],
            Duration::from_secs(30),
        );
        assert!(!timed_out, "workers={workers}: {out}");
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
