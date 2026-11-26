//! An unhandled language panic is an expected program failure: the process
//! reports it and exits with status 101 on every platform, after the panic's
//! defers ran and everything printed before it was flushed. It never raises
//! `SIGABRT`, so no crash handler (core dump, apport, Windows Error Reporting)
//! gets involved. Other failures keep their own status.
use super::support::*;

const PANIC_EXIT: i32 = 101;

fn run(name: &str, source: &str, env: &[(&str, &str)]) -> (Option<i32>, String, String) {
    let project = TestProject::new(name, &[("main.wi", source)]);
    let compile = project.compile("main.wi");
    assert!(
        compile.status.success(),
        "{name}: {}",
        String::from_utf8_lossy(&compile.stderr)
    );
    let output = project.run_with_env(env);
    #[cfg(unix)]
    {
        use std::os::unix::process::ExitStatusExt;
        assert_eq!(output.status.signal(), None, "{name}: ended by a signal");
    }
    (
        output.status.code(),
        String::from_utf8_lossy(&output.stdout).into_owned(),
        String::from_utf8_lossy(&output.stderr).into_owned(),
    )
}

#[test]
fn unhandled_panics_exit_with_the_panic_status() {
    for (name, source, stdout, report) in [
        (
            "explicit",
            "fn main() { println(\"before\"); panic(\"boom\"); }",
            "before\n",
            "runtime panic: boom",
        ),
        (
            "index",
            "fn main() { let xs = [1]; println(\"before\"); println(xs[5]); }",
            "before\n",
            "index out of bounds",
        ),
        (
            "unwrap",
            "fn main() { let o: Option<i64> = Option::None; println(o.unwrap()); }",
            "",
            "runtime panic:",
        ),
        (
            "division",
            "fn div(a: i64, b: i64) -> i64 { return a / b; }\nfn main() { println(div(1, 0)); }",
            "",
            "division by zero",
        ),
        (
            "nested call",
            "fn inner() { panic(\"deep\"); }\nfn outer() { inner(); }\nfn main() { outer(); }",
            "",
            "runtime panic: deep",
        ),
        (
            "defers run first",
            "fn main() { defer println(\"deferred\"); panic(\"after defer\"); }",
            "deferred\n",
            "runtime panic: after defer",
        ),
        (
            "panic inside a defer",
            "fn main() { defer panic(\"second\"); panic(\"first\"); }",
            "",
            "runtime panic:",
        ),
        (
            "awaited task",
            "async fn work() -> i64 { panic(\"task boom\"); return 1; }\nasync fn main() { println(\"started\"); println(await work()); }",
            "started\n",
            "runtime panic: task boom",
        ),
        (
            "spawned task awaited later",
            "async fn work(n: i64) -> i64 { if n == 2 { panic(\"late\"); } return n; }\nasync fn main() { let t = work(2); println(\"spawned\"); println(await t); }",
            "spawned\n",
            "runtime panic: late",
        ),
        (
            "parallel mapper",
            "import std::collections::Array;\nimport std::parallel;\nfn checked(v: i64) -> i64 { if v == 2 { panic(\"mapper\"); } return v; }\nasync fn main() { let xs: Array<i64> = [1, 2, 3]; println((await parallel::map(xs.freeze(), checked)).to_string()); }",
            "",
            "runtime panic: mapper",
        ),
    ] {
        for workers in ["1", "4"] {
            let (code, out, err) = run(
                &format!("panic_exit_{}", name.replace(' ', "_")),
                source,
                &[("WILLOW_WORKERS", workers)],
            );
            assert_eq!(code, Some(PANIC_EXIT), "{name} workers={workers}: {err}");
            // A spawned task runs concurrently with its spawner, so it may
            // panic before or after the spawner's next print.
            let accepted = if name == "spawned task awaited later" {
                vec!["", stdout]
            } else {
                vec![stdout]
            };
            assert!(
                accepted.contains(&out.as_str()),
                "{name} workers={workers}: {out:?}\n{err}"
            );
            assert!(err.contains(report), "{name} workers={workers}: {err}");
        }
    }
}

#[test]
fn recovered_panic_and_other_failures_keep_their_status() {
    for (name, source, expected) in [
        (
            "recovered",
            "fn main() { if true { defer match recover() { Some(i) => println(i.message), None => {} } panic(\"caught\"); } println(\"after\"); }",
            Some(0),
        ),
        (
            "main returns Err",
            "fn main() -> Result<void, String> { return Result::Err(\"bad\"); }",
            Some(1),
        ),
        ("success", "fn main() { println(\"ok\"); }", Some(0)),
    ] {
        let (code, _, err) = run(
            &format!("panic_status_{}", name.replace(' ', "_")),
            source,
            &[],
        );
        assert_eq!(code, expected, "{name}: {err}");
    }
}

#[test]
fn willow_run_forwards_the_panic_status() {
    let dir = std::env::temp_dir().join(format!("willow_panic_exit_run_{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let source = dir.join("main.wi");
    std::fs::write(&source, "fn main() { panic(\"via run\"); }\n").unwrap();
    let output = std::process::Command::new(env!("CARGO_BIN_EXE_willow"))
        .arg("run")
        .arg(&source)
        .output()
        .expect("run willow");
    let _ = std::fs::remove_dir_all(&dir);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert_eq!(output.status.code(), Some(PANIC_EXIT), "{stderr}");
    assert!(stderr.contains("runtime panic: via run"), "{stderr}");
}

#[test]
fn panic_exit_status_example_exits_with_the_panic_status() {
    let output = std::process::Command::new(env!("CARGO_BIN_EXE_willow"))
        .arg("run")
        .arg("example/panic_exit_status.wi")
        .output()
        .expect("run willow");
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert_eq!(output.status.code(), Some(PANIC_EXIT), "{stderr}");
    assert_eq!(
        String::from_utf8_lossy(&output.stdout),
        "saving\nclosing log\n"
    );
    assert!(
        stderr.contains("runtime panic: disk full while saving 3 items"),
        "{stderr}"
    );
}
