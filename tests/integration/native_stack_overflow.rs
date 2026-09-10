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
            let source = format!(
                "import std::collections::Array;\n{declarations}\nfn main(args: Array<String>) {{ let depth = args.len() == 0 ? 8 : 1000000; println({call}); }}"
            );
            fs::write(&source_path, source).unwrap();
            let mut command = Command::new(env!("CARGO_BIN_EXE_willowc"));
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
            assert!(
                String::from_utf8_lossy(&overflow.stderr)
                    .contains("Willow runtime error: native stack overflow"),
                "{name} release={release}: {:?}",
                overflow
            );
        }
    }
}
