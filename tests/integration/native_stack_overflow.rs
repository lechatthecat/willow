//! Native stack exhaustion is fatal, with a fixed diagnostic; it is not a
//! language panic and cannot run recover/defer on an exhausted stack.
//! Twenty perspectives: ten call/frame shapes in debug and release, each with
//! a successful shallow control and an overflowing run of the same executable.
use super::support::*;
use std::time::{Duration, Instant};

fn run_bounded(binary: &str, overflow: bool) -> std::process::Output {
    #[cfg(unix)]
    let mut command = {
        let mut command = Command::new("sh");
        // Match Windows' common 1 MiB native stack and avoid large core files.
        // The binary is a separate positional argument, never shell source.
        command.args([
            "-c",
            "ulimit -s 1024; ulimit -c 0; exec \"$@\"",
            "willow-stack-test",
            binary,
        ]);
        command
    };
    #[cfg(not(unix))]
    let mut command = Command::new(binary);
    command.stdout(Stdio::piped()).stderr(Stdio::piped());
    if overflow {
        command.arg("overflow");
    }
    let mut child = command.spawn().expect("run stack fixture");
    let deadline = Instant::now() + Duration::from_secs(30);
    while child.try_wait().expect("poll stack fixture").is_none() {
        if Instant::now() >= deadline {
            let _ = child.kill();
            let _ = child.wait();
            panic!("native stack fixture timed out");
        }
        std::thread::sleep(Duration::from_millis(5));
    }
    child.wait_with_output().expect("collect stack fixture")
}

#[test]
fn native_stack_overflow_twenty_call_and_frame_perspectives() {
    check_call_and_frame_perspectives(false);
}

#[test]
fn task_stack_overflow_twenty_call_and_frame_perspectives() {
    check_call_and_frame_perspectives(true);
}

fn check_call_and_frame_perspectives(task: bool) {
    let direct = "fn dive(n: i64) -> i64 { if n == 0 { return 0; } return dive(n - 1) + 1; }";
    let mut cases: Vec<(&str, String, String)> = vec![
        ("direct", direct.into(), "dive(depth)".into()),
        ("mutual", "fn dive(n: i64) -> i64 { if n == 0 { return 0; } return other(n - 1) + 1; } fn other(n: i64) -> i64 { if n == 0 { return 0; } return dive(n - 1) + 1; }".into(), "dive(depth)".into()),
        ("indirect", "fn dive(n: i64) -> i64 { if n == 0 { return 0; } let callback: fn(i64) -> i64 = dive; return callback(n - 1) + 1; }".into(), "dive(depth)".into()),
        ("method", "class Dive { pub fn run(self, n: i64) -> i64 { if n == 0 { return 0; } return self.run(n - 1) + 1; } }".into(), "new Dive().run(depth)".into()),
        ("static", "class Dive { pub static fn run(n: i64) -> i64 { if n == 0 { return 0; } return Dive::run(n - 1) + 1; } }".into(), "Dive::run(depth)".into()),
        ("virtual", "open class Base { pub open fn run(self, n: i64) -> i64 { return 0; } } class Dive extends Base { pub override fn run(self, n: i64) -> i64 { if n == 0 { return 0; } return invoke(self, n - 1) + 1; } } fn invoke(value: Base, n: i64) -> i64 { return value.run(n); }".into(), "invoke(new Dive(), depth)".into()),
        ("interface", "interface Runner { fn run(self, n: i64) -> i64; } class Dive implements Runner { pub fn run(self, n: i64) -> i64 { if n == 0 { return 0; } return invoke(self, n - 1) + 1; } } fn invoke(value: Runner, n: i64) -> i64 { return value.run(n); }".into(), "invoke(new Dive(), depth)".into()),
        ("closure", "fn dive(n: i64) -> i64 { if n == 0 { return 0; } let step = n - 1; let callback = || dive(step); return callback() + 1; }".into(), "dive(depth)".into()),
        ("constructor", "class Dive { pub value: i64; pub init(self, n: i64) { self.value = 0; if n > 0 { self.value = new Dive(n - 1).value + 1; } } }".into(), "new Dive(depth).value".into()),
    ];
    // More than a page of outgoing stack arguments must be probed, including
    // in release where unused local variables can disappear completely.
    let params = (0..600).map(|i| format!(", a{i}: i64")).collect::<String>();
    let args = (0..600).map(|i| format!(", a{i}")).collect::<String>();
    let initial = ", 0".repeat(600);
    cases.push(("wide_frame", format!("fn dive(n: i64{params}) -> i64 {{ if n == 0 {{ return 0; }} return dive(n - 1{args}) + 1; }}"), format!("dive(depth{initial})")));
    for (name, declarations, call) in cases {
        for release in [false, true] {
            let id = unique_test_id();
            let source_path = temp_path(format!("willow_stack_{id}.wi"));
            let binary_path = temp_path(format!(
                "willow_stack_{id}{}",
                if cfg!(windows) { ".exe" } else { "" }
            ));
            let entry = if task { "async fn" } else { "fn" };
            let source = format!(
                "import std::collections::Array;\n{declarations}\n{entry} main(args: Array<String>) {{ let depth = args.len() == 0 ? 8 : 10000000; println({call}); }}"
            );
            fs::write(&source_path, source).unwrap();
            let mut command = Command::new(env!("CARGO_BIN_EXE_willow"));
            command.args(["build", &source_path, "-o", &binary_path]);
            if release {
                command.arg("--release");
            }
            let compiled = command.output().expect("compile stack fixture");
            if !compiled.status.success() {
                let _ = fs::remove_file(&source_path);
                remove_output_artifacts(&binary_path);
                panic!(
                    "{name} release={release}: {}",
                    String::from_utf8_lossy(&compiled.stderr)
                );
            }
            let shallow = run_bounded(&binary_path, false);
            let overflow = run_bounded(&binary_path, true);
            let _ = fs::remove_file(&source_path);
            remove_output_artifacts(&binary_path);
            assert!(
                shallow.status.success(),
                "{name} release={release}: {:?}",
                shallow
            );
            assert_eq!(
                String::from_utf8_lossy(&shallow.stdout).trim(),
                "8",
                "{name} release={release}"
            );
            assert!(!overflow.status.success(), "{name} release={release}");
            let stderr = String::from_utf8_lossy(&overflow.stderr);
            let diagnostic = if task {
                "Willow runtime error: task stack overflow while executing synchronous helper "
            } else {
                "Willow runtime error: native stack overflow"
            };
            assert!(
                stderr.contains(diagnostic),
                "{name} release={release}: {overflow:?}"
            );
            if task {
                assert!(
                    !stderr.contains("<unknown>"),
                    "{name} release={release}: {stderr}"
                );
                let expected: &[&str] = match name {
                    "mutual" => &["dive", "other"],
                    "method" | "static" => &["Dive::run"],
                    "virtual" | "interface" => &["Dive::run", "invoke"],
                    "constructor" => &["Dive::init"],
                    "closure" => &["dive", "$lambda."],
                    _ => &["dive"],
                };
                let helper = stderr.split(diagnostic).nth(1).unwrap();
                assert!(
                    expected.iter().any(|name| helper.starts_with(name)),
                    "{name} release={release}: {stderr}"
                );
            }
        }
    }
}

// GNU link wrapping injects a real guard-page overflow inside a non-panicking
// runtime operation after a differently named helper has returned. Other hosts
// still exercise the portable task/native call-and-frame matrix above.
#[cfg(target_os = "linux")]
#[test]
fn task_stack_overflow_attributes_safepoint_after_callee_return_to_caller() {
    check_after_return_runtime(false);
}

#[cfg(target_os = "linux")]
#[test]
fn task_stack_overflow_attributes_cleanup_after_callee_return_to_caller() {
    check_after_return_runtime(true);
}

#[cfg(target_os = "linux")]
fn check_after_return_runtime(cleanup: bool) {
    for release in [false, true] {
        let id = unique_test_id();
        let binary = temp_path(format!("willow_after_return_{id}"));
        let shim = temp_path(format!("willow_after_return_{id}.c"));
        fs::write(
            &shim,
            include_str!("../fixtures/native_stack_overflow/after_return.c"),
        )
        .unwrap();
        let mut compile = Command::new(env!("CARGO_BIN_EXE_willow"));
        let source = if cleanup {
            "tests/fixtures/native_stack_overflow/after_return_cleanup.wi"
        } else {
            "tests/fixtures/native_stack_overflow/after_return.wi"
        };
        compile.args(["build", source, "-o", &binary]);
        compile.env("WILLOW_KEEP_OBJECT", "1");
        if release {
            compile.arg("--release");
        }
        let output = compile.output().unwrap();
        assert!(output.status.success(), "{output:?}");
        let shallow = run_bounded(&binary, false);
        assert!(shallow.status.success(), "release={release}: {shallow:?}");
        assert_eq!(
            String::from_utf8_lossy(&shallow.stdout),
            if cleanup { "7\nvalue\n" } else { "7\n100000\n" }
        );
        let object = object_path(&binary);
        // Acquire the runtime's link lock only after the compiler child exits.
        let runtime = build_runtime_staticlib(release);
        let mut linker = Command::new("cc");
        if cleanup {
            linker.arg("-DOVERFLOW_CLEANUP");
        }
        let link = linker
            .args(["-O0", "-fno-optimize-sibling-calls", &shim, &object])
            .arg(runtime.as_ref())
            .args([
                "-no-pie",
                "-lm",
                "-lpthread",
                "-ldl",
                "-Wl,--wrap=willow_println_i64",
                "-Wl,--wrap=willow_sync_safepoint",
                "-Wl,--wrap=willow_pop_roots",
                "-o",
                &binary,
            ])
            .output()
            .unwrap();
        assert!(link.status.success(), "{link:?}");
        let output = run_bounded(&binary, false);
        fs::remove_file(shim).unwrap();
        fs::remove_file(object).unwrap();
        remove_output_artifacts(&binary);
        assert_eq!(
            output.status.code(),
            Some(101),
            "release={release}: {output:?}"
        );
        assert_eq!(
            String::from_utf8_lossy(&output.stderr),
            "Willow runtime error: task stack overflow while executing synchronous helper foo\n",
            "release={release}"
        );
    }
}

// Linux exposes a reproducible process stack budget through RLIMIT_STACK.
// Other hosts retain the portable overflow perspectives above.
#[cfg(target_os = "linux")]
mod depth_regression {
    use super::*;

    const STEP: usize = 128;

    fn succeeds(binary: &str, depth: usize, stack_kib: usize) -> bool {
        let mut child = Command::new("sh")
            .args([
                "-c",
                "set -e; ulimit -s \"$1\"; ulimit -c 0; shift; exec \"$@\"",
                "willow-depth-test",
                &stack_kib.to_string(),
                binary,
                &"x".repeat(depth / STEP),
            ])
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .expect("run depth fixture");
        let deadline = Instant::now() + Duration::from_secs(30);
        while child.try_wait().expect("poll depth fixture").is_none() {
            if Instant::now() >= deadline {
                let _ = child.kill();
                let _ = child.wait();
                panic!("depth fixture timed out at {depth}");
            }
            std::thread::sleep(Duration::from_millis(5));
        }
        let output = child.wait_with_output().expect("collect depth fixture");
        if output.status.success() {
            assert_eq!(
                String::from_utf8_lossy(&output.stdout).trim(),
                depth.to_string()
            );
            true
        } else {
            assert!(
                String::from_utf8_lossy(&output.stderr)
                    .contains("Willow runtime error: native stack overflow"),
                "unexpected failure at depth {depth}: {output:?}"
            );
            false
        }
    }

    fn maximum_depth(binary: &str, stack_kib: usize) -> (usize, usize) {
        let (mut low, mut high) = (0, STEP);
        assert!(succeeds(binary, low, stack_kib));
        while succeeds(binary, high, stack_kib) {
            low = high;
            high *= 2;
            assert!(high <= 4 * 1024 * 1024, "missing stack boundary");
        }
        while high - low > STEP {
            let middle = ((low + high) / (2 * STEP)) * STEP;
            if succeeds(binary, middle, stack_kib) {
                low = middle;
            } else {
                high = middle;
            }
        }
        (low, high)
    }

    fn check_result_depth(release: bool) {
        let id = unique_test_id();
        let source = temp_path(format!("willow_result_depth_{id}.wi"));
        let binary = temp_path(format!("willow_result_depth_{id}"));
        fs::write(
            &source,
            include_str!("../fixtures/native_stack_depth/result.wi"),
        )
        .unwrap();
        let mut command = Command::new(env!("CARGO_BIN_EXE_willow"));
        command.args(["build", &source, "-o", &binary]);
        if release {
            command.arg("--release");
        }
        let output = command.output().expect("compile depth fixture");
        assert!(output.status.success(), "{output:?}");
        // Include 8 MiB so the regression crosses the originally reported
        // 32k boundary, rather than only measuring shallower recursion.
        let depths = [1024, 2048, 4096, 8192].map(|stack_kib| {
            let interval = maximum_depth(&binary, stack_kib);
            eprintln!("Result release={release}: {stack_kib} KiB={interval:?}");
            interval
        });
        let _ = fs::remove_file(source);
        remove_output_artifacts(&binary);
        // A hidden root-count/depth cap cannot scale with the stack budget.
        // Tolerate ASLR, process startup frames and the 128-level search interval.
        assert!(depths[0].0 >= 2048, "unexpected per-frame cost: {depths:?}");
        for pair in depths.windows(2) {
            let [small, large] = pair else { unreachable!() };
            assert!(
                large.0 * 10 >= small.1 * 18,
                "nonproportional depth: {small:?}, {large:?}"
            );
            assert!(
                large.1 * 10 <= small.0 * 22,
                "unexpected depth ratio: {small:?}, {large:?}"
            );
        }
    }

    #[test]
    fn result_maximum_depth_scales_with_stack_debug() {
        check_result_depth(false);
    }

    #[test]
    fn result_maximum_depth_scales_with_stack_release() {
        check_result_depth(true);
    }
}
