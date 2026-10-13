use super::*;

// ---------------------------------------------------------------------------
// Task-aware preemption analysis (E0810) across imported modules
// (willow-0a6k.2). The analyzer previously ran only on the entry program, so a
// looping synchronous helper called from an async fn *inside an imported
// module* slipped through. These cover the per-module analysis, the resolved
// module file in the diagnostic, transitive reachability inside a module, and
// the absence of false positives for loop-free module helpers.
// ---------------------------------------------------------------------------

#[cfg(not(any(
    all(
        target_os = "linux",
        target_env = "gnu",
        any(target_arch = "x86_64", target_arch = "aarch64")
    ),
    all(
        target_os = "macos",
        any(target_arch = "x86_64", target_arch = "aarch64")
    ),
    all(target_os = "windows", target_env = "msvc", target_arch = "x86_64")
)))]
#[test]
fn test_module_async_fn_calling_looping_helper_reports_e0810() {
    let worker = r#"
fn heavy(n: i64) -> i64 {
    let mut i = 0;
    while i < n {
        i = i + 1;
    }
    return i;
}

pub async fn run() -> i64 {
    return heavy(10);
}

pub fn ping() -> i64 {
    return 1;
}
"#;
    let main = r#"
import worker;

fn main() {
    println(worker::ping());
}
"#;
    let stderr =
        compile_temp_project_error_stderr(&[("worker.wi", worker), ("main.wi", main)], "main.wi");
    for expected in [
        "error[E0810]",
        "sync helper `heavy` with a loop is not preemptible in task context",
        // The diagnostic must resolve to the module file, not the entry file.
        "worker.wi",
    ] {
        assert!(
            stderr.contains(expected),
            "stderr did not contain `{expected}`:\n{stderr}"
        );
    }
}

#[cfg(not(any(
    all(
        target_os = "linux",
        target_env = "gnu",
        any(target_arch = "x86_64", target_arch = "aarch64")
    ),
    all(
        target_os = "macos",
        any(target_arch = "x86_64", target_arch = "aarch64")
    ),
    all(target_os = "windows", target_env = "msvc", target_arch = "x86_64")
)))]
#[test]
fn test_module_transitive_looping_helper_reports_e0810() {
    let worker = r#"
fn heavy(n: i64) -> i64 {
    let mut i = 0;
    while i < n {
        i = i + 1;
    }
    return i;
}

fn wrapper(n: i64) -> i64 {
    return heavy(n);
}

pub async fn run() -> i64 {
    return wrapper(10);
}

pub fn ping() -> i64 {
    return 1;
}
"#;
    let main = r#"
import worker;

fn main() {
    println(worker::ping());
}
"#;
    let stderr =
        compile_temp_project_error_stderr(&[("worker.wi", worker), ("main.wi", main)], "main.wi");
    assert!(
        stderr.contains("error[E0810]")
            && stderr.contains("sync helper `wrapper` with a loop is not preemptible"),
        "expected transitive module E0810 for `wrapper`:\n{stderr}"
    );
}

#[test]
fn test_module_loop_free_helper_in_async_compiles() {
    let worker = r#"
fn add_one(n: i64) -> i64 {
    return n + 1;
}

pub async fn run() -> i64 {
    return add_one(41);
}

pub fn ping() -> i64 {
    return 1;
}
"#;
    let main = r#"
import worker;

fn main() {
    println(worker::ping());
}
"#;
    let (out, ok) =
        compile_temp_project_and_run(&[("worker.wi", worker), ("main.wi", main)], "main.wi");
    assert!(ok, "loop-free module async helper should compile and run");
    assert_eq!(out, "1\n");
}

#[cfg(not(any(
    all(
        target_os = "linux",
        target_env = "gnu",
        any(target_arch = "x86_64", target_arch = "aarch64")
    ),
    all(
        target_os = "macos",
        any(target_arch = "x86_64", target_arch = "aarch64")
    ),
    all(target_os = "windows", target_env = "msvc", target_arch = "x86_64")
)))]
#[test]
fn test_entry_async_calling_module_looping_helper_reports_e0810() {
    let worker = r#"
pub fn heavy(n: i64) -> i64 {
    let mut i = 0;
    while i < n {
        i = i + 1;
    }
    return i;
}
"#;
    let main = r#"
import worker;

async fn run() -> i64 {
    return worker::heavy(10);
}

async fn main() {
    await run();
}
"#;
    let stderr =
        compile_temp_project_error_stderr(&[("worker.wi", worker), ("main.wi", main)], "main.wi");
    for expected in [
        "error[E0810]",
        "sync helper `worker::heavy` with a loop is not preemptible in task context",
        // Cross-module helper described via a note, not a secondary source label.
        "imported module `worker`",
        "help: make the helper async, or wait for task-aware sync-stack preemption support",
    ] {
        assert!(
            stderr.contains(expected),
            "stderr did not contain `{expected}`:\n{stderr}"
        );
    }
}

#[cfg(not(any(
    all(
        target_os = "linux",
        target_env = "gnu",
        any(target_arch = "x86_64", target_arch = "aarch64")
    ),
    all(
        target_os = "macos",
        any(target_arch = "x86_64", target_arch = "aarch64")
    ),
    all(target_os = "windows", target_env = "msvc", target_arch = "x86_64")
)))]
#[test]
fn test_entry_async_calling_module_transitive_helper_reports_e0810() {
    let worker = r#"
pub fn heavy(n: i64) -> i64 {
    let mut i = 0;
    while i < n {
        i = i + 1;
    }
    return i;
}

pub fn wrapper(n: i64) -> i64 {
    return heavy(n);
}
"#;
    let main = r#"
import worker;

async fn run() -> i64 {
    return worker::wrapper(10);
}

async fn main() {
    await run();
}
"#;
    let stderr =
        compile_temp_project_error_stderr(&[("worker.wi", worker), ("main.wi", main)], "main.wi");
    assert!(
        stderr.contains("error[E0810]")
            && stderr.contains("sync helper `worker::wrapper`")
            && stderr.contains("imported module `worker`"),
        "expected cross-module transitive E0810 for `worker::wrapper`:\n{stderr}"
    );
}

#[test]
fn test_entry_async_calling_module_loop_free_helper_compiles() {
    let worker = r#"
pub fn add_one(n: i64) -> i64 {
    return n + 1;
}
"#;
    let main = r#"
import worker;

async fn run() -> i64 {
    return worker::add_one(41);
}

async fn main() {
    println(await run());
}
"#;
    let (out, ok) =
        compile_temp_project_and_run(&[("worker.wi", worker), ("main.wi", main)], "main.wi");
    assert!(
        ok,
        "loop-free cross-module async call should compile and run"
    );
    assert_eq!(out, "42\n");
}

#[cfg(not(any(
    all(
        target_os = "linux",
        target_env = "gnu",
        any(target_arch = "x86_64", target_arch = "aarch64")
    ),
    all(
        target_os = "macos",
        any(target_arch = "x86_64", target_arch = "aarch64")
    ),
    all(target_os = "windows", target_env = "msvc", target_arch = "x86_64")
)))]
#[test]
fn test_item_imported_looping_helper_from_async_reports_e0810() {
    let worker = r#"
pub fn heavy(n: i64) -> i64 {
    let mut i = 0;
    while i < n {
        i = i + 1;
    }
    return i;
}
"#;
    let main = r#"
import worker::heavy;

async fn run() -> i64 {
    return heavy(10);
}

async fn main() {
    await run();
}
"#;
    let stderr =
        compile_temp_project_error_stderr(&[("worker.wi", worker), ("main.wi", main)], "main.wi");
    for expected in [
        "error[E0810]",
        "sync helper `heavy` with a loop is not preemptible in task context",
        "imported module `worker`",
    ] {
        assert!(
            stderr.contains(expected),
            "stderr did not contain `{expected}`:\n{stderr}"
        );
    }
}

#[test]
fn test_item_imported_loop_free_helper_from_async_compiles() {
    let worker = r#"
pub fn add_one(n: i64) -> i64 {
    return n + 1;
}
"#;
    let main = r#"
import worker::add_one;

async fn run() -> i64 {
    return add_one(41);
}

async fn main() {
    println(await run());
}
"#;
    let (out, ok) =
        compile_temp_project_and_run(&[("worker.wi", worker), ("main.wi", main)], "main.wi");
    assert!(
        ok,
        "loop-free item-imported async call should compile and run"
    );
    assert_eq!(out, "42\n");
}

#[cfg(not(any(
    all(
        target_os = "linux",
        target_env = "gnu",
        any(target_arch = "x86_64", target_arch = "aarch64")
    ),
    all(
        target_os = "macos",
        any(target_arch = "x86_64", target_arch = "aarch64")
    ),
    all(target_os = "windows", target_env = "msvc", target_arch = "x86_64")
)))]
#[test]
fn test_module_to_module_looping_call_from_async_reports_e0810() {
    // main -> a (async) -> b::heavy (looping). The call lives in module `a`, so
    // module `a` must be seeded with module `b`'s helpers.
    let b = r#"
pub fn heavy(n: i64) -> i64 {
    let mut i = 0;
    while i < n {
        i = i + 1;
    }
    return i;
}
"#;
    let a = r#"
import b;

pub async fn run() -> i64 {
    return b::heavy(10);
}
"#;
    let main = r#"
import a;

fn main() {
    println(1);
}
"#;
    let stderr = compile_temp_project_error_stderr(
        &[("b.wi", b), ("a.wi", a), ("main.wi", main)],
        "main.wi",
    );
    for expected in [
        "error[E0810]",
        "sync helper `b::heavy` with a loop is not preemptible in task context",
        "imported module `b`",
        // The offending call is in module a, so the diagnostic resolves there.
        "a.wi",
    ] {
        assert!(
            stderr.contains(expected),
            "stderr did not contain `{expected}`:\n{stderr}"
        );
    }
}

#[test]
fn test_module_to_module_loop_free_call_from_async_compiles() {
    let b = r#"
pub fn add_one(n: i64) -> i64 {
    return n + 1;
}
"#;
    // Module `a`'s async fn calls `b`'s loop-free helper — no E0810. (`run` is
    // exercised internally; `main` only needs the modules to compile.)
    let a = r#"
import b;

pub async fn run() -> i64 {
    return b::add_one(41);
}

pub fn ping() -> i64 {
    return 7;
}
"#;
    let main = r#"
import a;

fn main() {
    println(a::ping());
}
"#;
    let (out, ok) =
        compile_temp_project_and_run(&[("b.wi", b), ("a.wi", a), ("main.wi", main)], "main.wi");
    assert!(
        ok,
        "loop-free module-to-module async call should compile and run"
    );
    assert_eq!(out, "7\n");
}
