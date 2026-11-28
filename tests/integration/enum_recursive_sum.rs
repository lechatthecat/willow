//! Native frame/depth regression plus semantic and GC controls for enum reductions.
use super::support::*;
use std::time::Duration;

const DECLARATIONS: &str = r#"
enum Chain { Link(i64, Chain), End }
fn sum(c: Chain) -> i64 {
    return match c { Chain::Link(v, rest) => v + sum(rest), Chain::End => 0 };
}
fn depth(n: i64) -> i64 {
    if n == 0 { return 0; }
    return 1 + depth(n - 1);
}
"#;

/// Keep a fixed stack budget instead of inferring bytes/frame from a crash.
/// Windows' executable stack reserve is set with the MSVC image editor.
fn compile_fixture(source: &str, flags: &[&str]) -> (String, String) {
    let id = unique_test_id();
    let path = temp_path(format!("willow_enum_reduction_{id}.wi"));
    let binary = temp_path(format!(
        "willow_enum_reduction_{id}{}",
        std::env::consts::EXE_SUFFIX
    ));
    fs::write(&path, source).unwrap();
    let output = Command::new(env!("CARGO_BIN_EXE_willow"))
        .args(["build", &path, "-o", &binary])
        .args(flags)
        .output()
        .unwrap();
    assert!(output.status.success(), "{output:?}");
    #[cfg(windows)]
    {
        let mut edit = cc::windows_registry::find_tool("x86_64-pc-windows-msvc", "editbin.exe")
            .expect("MSVC image editor")
            .to_command();
        let edited = edit
            .args(["/NOLOGO", "/STACK:8388608", &binary])
            .output()
            .unwrap();
        assert!(edited.status.success(), "{edited:?}");
    }
    (path, binary)
}

fn run_fixture(binary: &str, args: &[&str], env: &[(&str, &str)]) -> std::process::Output {
    #[cfg(unix)]
    let mut command = {
        let mut command = Command::new("sh");
        command.args([
            "-c",
            "set -e; ulimit -s 8192; ulimit -c 0; exec \"$@\"",
            "willow-depth",
            binary,
        ]);
        command
    };
    #[cfg(not(unix))]
    let mut command = Command::new(binary);
    command
        .args(args)
        .envs(env.iter().copied())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let child = shared_child::SharedChild::spawn(&mut command).unwrap();
    std::thread::scope(|scope| {
        use std::io::Read;
        let mut stdout = child.take_stdout().unwrap();
        let mut stderr = child.take_stderr().unwrap();
        let out = scope.spawn(move || {
            let mut bytes = Vec::new();
            stdout.read_to_end(&mut bytes).unwrap();
            bytes
        });
        let err = scope.spawn(move || {
            let mut bytes = Vec::new();
            stderr.read_to_end(&mut bytes).unwrap();
            bytes
        });
        let (tx, rx) = std::sync::mpsc::channel();
        let waiting = &child;
        scope.spawn(move || {
            let _ = tx.send(waiting.wait());
        });
        let status = match rx.recv_timeout(Duration::from_secs(60)) {
            Ok(status) => status.unwrap(),
            Err(error) => {
                let _ = child.kill();
                panic!("enum reduction process exceeded hang guard: {error}");
            }
        };
        std::process::Output {
            status,
            stdout: out.join().unwrap(),
            stderr: err.join().unwrap(),
        }
    })
}

fn cleanup(path: &str, binary: &str) {
    fs::remove_file(path).unwrap();
    let _ = fs::remove_file(object_path(binary));
    remove_output_artifacts(binary);
}

#[test]
fn enum_reduction_depth_sync_and_async_same_eight_mib_stack() {
    // The unchanged Windows recursion ABI also reserves outgoing home space.
    // Keep its plain-recursion control below that bound; the enum target is
    // 500k on every platform, using the same 8 MiB budget as its control.
    let plain_depth = if cfg!(windows) { 250000 } else { 500000 };
    for entry in ["fn", "async fn"] {
        let source = format!(
            r#"
import std::collections::Array;
{DECLARATIONS}
{entry} main(args: Array<String>) {{
    let n = args.len() == 0 ? 0 : 500000;
    let mut c = Chain::End;
    for i in 0..n {{ c = Chain::Link(1, c); }}
    println(depth(n > {plain_depth} ? {plain_depth} : n));
    println(sum(c));
}}"#
        );
        let (path, binary) = compile_fixture(&source, &["--release"]);
        let deep_output = format!("{plain_depth}\n500000\n");
        for (args, expected) in [(&[][..], "0\n0\n"), (&["deep"][..], deep_output.as_str())] {
            let output = run_fixture(&binary, args, &[]);
            assert!(output.status.success(), "{entry}: {output:?}");
            assert_eq!(String::from_utf8_lossy(&output.stdout), expected);
        }
        cleanup(&path, &binary);
    }
}

#[test]
fn enum_reduction_values_parameter_copies_and_gc_relocation() {
    let source = format!(
        r#"
{DECLARATIONS}
enum Generic<T> {{ Link(T, Generic<T>), End }}
fn generic(c: Generic<i64>) -> i64 {{
    return match c {{ Generic::Link(v, r) => v + generic(r), Generic::End => 7 }};
}}
fn right(c: Chain) -> i64 {{
    return match c {{ Chain::Link(v, r) => right(r) + v, Chain::End => 9 }};
}}
fn swap(c: Chain, a: i64, b: i64) -> i64 {{
    return match c {{ Chain::Link(v, r) => a + swap(r, b, a), Chain::End => b }};
}}
fn negate(c: Chain) -> i64 {{
    match c {{ Chain::Link(v, r) => {{ return -v + negate(r); }}, Chain::End => {{ return -7; }} }}
}}
fn main() {{
    let c = Chain::Link(5, Chain::Link(-2, Chain::Link(8, Chain::End)));
    println(sum(Chain::End));
    println(sum(Chain::Link(42, Chain::End)));
    println(sum(c)); println(right(c)); println(swap(c, 3, 11)); println(negate(c));
    println(generic(Generic::Link(6, Generic::Link(4, Generic::End))));
    let wrap = Chain::Link(9223372036854775807, Chain::Link(1, Chain::End));
    println(sum(wrap));
    let mut deep = Chain::End;
    for i in 0..256 {{ deep = Chain::Link(i, deep); }}
    println(sum(deep));
}}"#
    );
    let (path, binary) = compile_fixture(&source, &["--release"]);
    for stress in ["", "minor", "alloc", "relocate"] {
        let output = run_fixture(&binary, &[], &[("WILLOW_GC_STRESS", stress)]);
        assert!(output.status.success(), "{stress}: {output:?}");
        assert_eq!(
            String::from_utf8_lossy(&output.stdout),
            "0\n42\n11\n20\n20\n-18\n17\n-9223372036854775808\n32640\n",
            "{stress}"
        );
    }
    cleanup(&path, &binary);

    // A task-stack poll can collect or migrate while the next managed
    // parameter is live. Use explicit scheduler stress and a one-step budget.
    let (path, binary) = compile_fixture(
        &source.replace("fn main()", "async fn main()"),
        &["--release"],
    );
    let output = run_fixture(
        &binary,
        &[],
        &[
            ("WILLOW_WORKERS", "1"),
            ("WILLOW_TASK_BUDGET", "1"),
            ("WILLOW_GC_STRESS", "relocate,scheduler"),
        ],
    );
    assert!(output.status.success(), "{output:?}");
    assert_eq!(
        String::from_utf8_lossy(&output.stdout),
        "0\n42\n11\n20\n20\n-18\n17\n-9223372036854775808\n32640\n"
    );
    cleanup(&path, &binary);
}

#[test]
fn enum_reduction_checked_fault_and_cleanup_keep_source_order() {
    let source = format!(
        r#"
{DECLARATIONS}
fn guarded(c: Chain) {{
    defer {{ match recover() {{ Some(_) => println("recovered"), None => {{}} }} }}
    println(sum(c));
}}
fn observable(c: Chain) -> i64 {{
    defer println("exit");
    return match c {{ Chain::Link(v, r) => {{ let n = observable(r); println(v); return v + n; }}, Chain::End => 0 }};
}}
enum Floats {{ Link(f64, Floats), End }}
fn floating(c: Floats) -> f64 {{
    return match c {{ Floats::Link(v, r) => v + floating(r), Floats::End => 0.0 }};
}}
fn subtract(c: Chain) -> i64 {{
    return match c {{ Chain::Link(v, r) => v - subtract(r), Chain::End => 0 }};
}}
fn main() {{
    guarded(Chain::Link(9223372036854775807, Chain::Link(1, Chain::End)));
    println(observable(Chain::Link(3, Chain::Link(5, Chain::End))));
    println(floating(Floats::Link(10000000000000000.0, Floats::Link(-10000000000000000.0, Floats::Link(1.0, Floats::End)))));
    println(subtract(Chain::Link(5, Chain::Link(-2, Chain::Link(8, Chain::End)))));
}}"#
    );
    for flags in [
        &["--debug"][..],
        &["--release", "--overflow-checks", "true"][..],
        &["--release"][..],
    ] {
        let (path, binary) = compile_fixture(&source, flags);
        let output = run_fixture(&binary, &[], &[]);
        assert!(output.status.success(), "{flags:?}: {output:?}");
        let first = if flags == ["--release"] {
            "-9223372036854775808"
        } else {
            "recovered"
        };
        assert_eq!(
            String::from_utf8_lossy(&output.stdout),
            format!("{first}\nexit\n5\nexit\n3\nexit\n8\n0.0\n15\n")
        );
        cleanup(&path, &binary);
    }
}

#[test]
fn enum_reduction_runnable_example_debug_and_release() {
    let source = include_str!("../../example/enum_recursive_sum.wi");
    for run in [compile_and_run, compile_and_run_release] {
        let (out, ok) = run(source);
        assert!(ok, "{out}");
        assert_eq!(out, "4950\n100\n");
    }
}

#[test]
fn enum_reduction_backedge_remains_cancellable_and_roots_balance() {
    let source = r#"
enum Chain { Link(i64, Chain), End }
fn forever(c: Chain) -> i64 {
    return match c { Chain::Link(v, rest) => v + forever(c), Chain::End => 0 };
}
fn run(ready: Channel<i64>) {
    defer println("sync cleanup");
    ready.send(1);
    println(forever(Chain::Link(1, Chain::End)));
}
async fn worker(ready: Channel<i64>) {
    defer println("async cleanup");
    run(ready);
}
async fn main() {
    let ready = Channel<i64>::new();
    let task = worker(ready);
    ready.recv();
    task.cancel();
    await task.result();
    gc_minor_collect();
    println("done");
}
"#;
    let (path, binary) = compile_fixture(source, &["--release"]);
    let output = run_fixture(
        &binary,
        &[],
        &[
            ("WILLOW_WORKERS", "1"),
            ("WILLOW_TASK_BUDGET", "3"),
            ("WILLOW_GC_STRESS", "relocate"),
        ],
    );
    assert!(output.status.success(), "{output:?}");
    assert_eq!(
        String::from_utf8_lossy(&output.stdout),
        "sync cleanup\nasync cleanup\ndone\n"
    );
    cleanup(&path, &binary);
}
