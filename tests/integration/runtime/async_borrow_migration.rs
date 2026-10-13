use super::*;
use std::collections::HashMap;
use std::time::Duration;

/// Isolate the opt-in runtime from normal compiler auto-builds and parallel
/// integration tests. There are no new ABI symbols or compiler options.
fn probe_runtime() -> PathBuf {
    let target = Path::new(env!("CARGO_MANIFEST_DIR")).join("target/borrow-migration-probe");
    let out = Command::new(env!("CARGO"))
        .args([
            "build",
            "-p",
            "willow_runtime",
            "--features",
            "borrow-migration-probe",
            "--target-dir",
        ])
        .arg(&target)
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    target.join("debug").join(if cfg!(target_env = "msvc") {
        "willow_runtime.lib"
    } else {
        "libwillow_runtime.a"
    })
}

#[test]
fn task_owned_shadow_roots_forced_native_migration() {
    let runtime = probe_runtime();
    let runtime = runtime.to_str().unwrap();
    let project = TestProject::new(
        "task_owned_shadow_roots",
        &[(
            "main.wi",
            include_str!("../../../example/task_owned_shadow_roots.wi"),
        )],
    );
    for release in [false, true] {
        let mut args = vec!["--runtime-lib", runtime];
        if release {
            args.push("--release");
        }
        let compiled = project.compile_with_args("main.wi", &args);
        assert!(
            compiled.status.success(),
            "{}",
            String::from_utf8_lossy(&compiled.stderr)
        );
        for stress in ["", "relocate"] {
            let out = project.run_with_env_timeout(
                &[
                    ("WILLOW_WORKERS", "2"),
                    ("WILLOW_TASK_BUDGET", "1"),
                    ("WILLOW_GC_STRESS", stress),
                ],
                Duration::from_secs(30),
            );
            let stderr = String::from_utf8_lossy(&out.stderr);
            assert!(out.status.success(), "{stderr}");
            assert_eq!(String::from_utf8_lossy(&out.stdout), "76\n");
            let migrations = stderr
                .lines()
                .filter(|line| line.starts_with("native-stack-migration "))
                .count();
            assert!(
                migrations >= 8,
                "missing actual native migrations: {stderr}"
            );
            println!("release={release} stress={stress:?} checked_native_migrations={migrations}");
        }
    }
    let compiled = project.compile_with_env("main.wi", &[("WILLOW_RUNTIME_LIB", runtime)]);
    assert!(
        compiled.status.success(),
        "{}",
        String::from_utf8_lossy(&compiled.stderr)
    );
}

fn run_probe(project: &TestProject, expected: &str, tasks: usize, stress: &str) -> usize {
    let out = project.run_with_env_timeout(
        &[("WILLOW_WORKERS", "2"), ("WILLOW_GC_STRESS", stress)],
        Duration::from_secs(120),
    );
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(out.status.success(), "{stderr}");
    assert_eq!(String::from_utf8_lossy(&out.stdout), expected, "{stderr}");
    let mut probes = HashMap::<u64, (usize, usize)>::new();
    for line in stderr
        .lines()
        .filter(|line| line.starts_with("borrow-migration "))
    {
        let fields: Vec<_> = line
            .split_whitespace()
            .skip(1)
            .map(|field| field.split_once('=').unwrap().1.parse::<u64>().unwrap())
            .collect();
        let probe = probes.entry(fields[0]).or_default();
        probe.0 += 1;
        probe.1 += fields[2] as usize;
        assert_eq!(fields[1] as usize, probe.0);
        assert_eq!(fields[3], 1, "one checked heap-owned borrow per poll");
    }
    assert_eq!(probes.len(), tasks, "fixture not observed: {stderr}");
    for (id, (polls, parked)) in &probes {
        assert!(
            *polls >= 2 && *parked >= 1,
            "task {id} never resumed after parking: {stderr}"
        );
    }
    probes.values().map(|p| p.0).sum()
}

#[test]
fn async_borrow_migration_forced_generated_polls() {
    let runtime = probe_runtime();
    let runtime = runtime.to_str().unwrap();
    // 24 explicit perspectives: i64/f64/bool × shared/mutable × direct,
    // forwarded, deferred access, and scope-owned task. Every marked task
    // must park, resume on a distinct OS thread and retain a heap-owned slot.
    let mut source = String::new();
    let mut main = String::from("async fn main() {");
    let mut expected = String::new();
    let mut tasks = 0;
    for (ty, initial, updated) in [
        ("i64", "41", "42"),
        ("f64", "2.5", "4.25"),
        ("bool", "false", "true"),
    ] {
        for mutable in [false, true] {
            for shape in 0..4 {
                let name = format!("probe_{ty}_{mutable}_{shape}");
                let mode = if mutable { "&mut" } else { "&" };
                let access = if mutable {
                    format!("x = {updated};")
                } else {
                    "println(x);".into()
                };
                let body = if shape == 2 {
                    format!("defer {{ {access} }} await sleep(10);")
                } else {
                    format!("await sleep(10); {access}")
                };
                source.push_str(&format!(
                    "async fn {name}(marker: i64, x: {mode} {ty}) {{ {body} }}"
                ));
                let call = if shape == 1 {
                    source.push_str(&format!("async fn forward_{name}(marker: i64, x: {mode} {ty}) {{ await sleep(10); await {name}(marker, &x); }}"));
                    tasks += 1;
                    format!("forward_{name}")
                } else {
                    name.clone()
                };
                main.push_str(&format!("if true {{ let mut x = {initial};"));
                if shape == 3 {
                    main.push_str(&format!(
                        "let s = TaskScope::new(); s.add({call}(731207731, &x)); await s.finish();"
                    ));
                } else {
                    main.push_str(&format!("await {call}(731207731, &x);"));
                }
                main.push_str("println(x); }");
                if !mutable {
                    expected.push_str(&format!("{initial}\n"));
                }
                expected.push_str(&format!("{}\n", if mutable { updated } else { initial }));
                tasks += 1;
            }
        }
    }
    source.push_str(&main);
    source.push('}');
    let project = TestProject::new("borrow_migration", &[("main.wi", &source)]);
    for release in [false, true] {
        let mut args = vec!["--runtime-lib", runtime];
        if release {
            args.push("--release");
        }
        let out = project.compile_with_args("main.wi", &args);
        assert!(
            out.status.success(),
            "{}",
            String::from_utf8_lossy(&out.stderr)
        );
        for stress in ["", "minor", "relocate"] {
            let polls = run_probe(&project, &expected, tasks, stress);
            println!(
                "matrix release={release} stress={stress:?} tasks={tasks} checked_polls={polls}"
            );
        }
    }

    let example = TestProject::new(
        "borrow_migration_example",
        &[(
            "main.wi",
            include_str!("../../../example/async_borrow_migration.wi"),
        )],
    );
    // Exercise the environment override as well as --runtime-lib.
    let out = example.compile_with_env("main.wi", &[("WILLOW_RUNTIME_LIB", runtime)]);
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    run_probe(&example, "42\n", 2, "");

    let out = example.run_with_env_timeout(&[("WILLOW_WORKERS", "1")], Duration::from_secs(20));
    assert!(!out.status.success(), "single-worker probe silently passed");
    assert!(
        String::from_utf8_lossy(&out.stderr)
            .contains("migration probe requires at least two workers")
    );

    // Deterministic operation counts, including repeated equivalent calls and
    // fan-out to distinct slots in one caller frame. One suspension means two
    // checked polls per leaf, regardless of queue steals/routing retries.
    for count in [1, 8, 32] {
        for fanout in [false, true] {
            let mut source = String::from(
                "async fn update(marker: i64, x: &mut i64) { await sleep(10); x = x + 1; } async fn main() {",
            );
            let expected = if fanout {
                source.push_str("let s = TaskScope::new();");
                for i in 0..count {
                    source.push_str(&format!(
                        "let mut x{i} = {i}; s.add(update(731207731, &x{i}));"
                    ));
                }
                source.push_str("await s.finish();");
                for i in 0..count {
                    source.push_str(&format!("println(x{i});"));
                }
                (1..=count).map(|n| format!("{n}\n")).collect::<String>()
            } else {
                source.push_str("let mut x = 0;");
                for _ in 0..count {
                    source.push_str("await update(731207731, &x);");
                }
                source.push_str("println(x);");
                format!("{count}\n")
            };
            source.push('}');
            let project = TestProject::new("borrow_migration_scaling", &[("main.wi", &source)]);
            let out = project.compile_with_args("main.wi", &["--runtime-lib", runtime]);
            assert!(
                out.status.success(),
                "{}",
                String::from_utf8_lossy(&out.stderr)
            );
            let polls = run_probe(&project, &expected, count, "");
            assert_eq!(polls, 2 * count, "count={count} fanout={fanout}");
            println!("scaling count={count} fanout={fanout} checked_polls={polls}");
        }

        let mut source =
            String::from("async fn f0(marker: i64, x: &mut i64) { await sleep(10); x = x + 1; }");
        for depth in 1..count {
            source.push_str(&format!("async fn f{depth}(marker: i64, x: &mut i64) {{ await sleep(10); await f{}(marker, &x); x = x + 1; }}", depth - 1));
        }
        source.push_str(&format!(
            "async fn main() {{ let mut x = 0; await f{}(731207731, &x); println(x); }}",
            count - 1
        ));
        let project = TestProject::new("borrow_migration_chain", &[("main.wi", &source)]);
        let out = project.compile_with_args("main.wi", &["--runtime-lib", runtime]);
        assert!(
            out.status.success(),
            "{}",
            String::from_utf8_lossy(&out.stderr)
        );
        let polls = run_probe(&project, &format!("{count}\n"), count, "relocate");
        // A child's completion can race its parent's await. There are two
        // polls per level, plus at most one child-await resumption per parent.
        assert!((2 * count..=3 * count - 1).contains(&polls));
        println!("chain depth={count} checked_polls={polls}");
    }

    let (out, ok) = compile_and_run(include_str!("../../../example/async_borrow_migration.wi"));
    assert!(ok, "{out}");
    assert_eq!(out, "42\n");
}

#[test]
fn async_mut_sync_preemption_forced_migration() {
    let runtime = probe_runtime();
    let source =
        "fn heavy(x: &mut i64) { let mut i = 0; while i < 10000 { x = x + 1; i = i + 1; } }
        async fn work(marker: i64, x: &mut i64) { heavy(&x); await sleep(10); heavy(&x); }
        async fn main() { let mut x = 0; await work(731207731, &x); println(x); }";
    let project = TestProject::new("mut_sync_migrate", &[("main.wi", source)]);
    for release in [false, true] {
        let mut args = vec!["--runtime-lib", runtime.to_str().unwrap()];
        if release {
            args.push("--release");
        }
        let out = project.compile_with_args("main.wi", &args);
        assert!(
            out.status.success(),
            "{}",
            String::from_utf8_lossy(&out.stderr)
        );
        for stress in ["", "minor", "relocate"] {
            run_probe(&project, "20000\n", 1, stress);
        }
    }
}
