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
    let compiled = Command::new(env!("CARGO_BIN_EXE_willowc"))
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
