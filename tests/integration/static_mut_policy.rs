//! willow-9tls.23: runtime worker count, static initialization, and ABI emission.
use super::support::*;

const SLOT: &str = "class S { pub static mut n: i64 = 0; }";
const TASK: &str = "async fn main() { S::n = S::n + 1; println(S::n); }";
const GUARD: &str = "willow_sched_require_single_worker_statics";

#[test]
fn static_mut_policy_worker_matrix() {
    // 1–5: single worker accepted; 2, 3, 8, and 32 workers rejected before user code.
    let project = TestProject::new(
        "static_mut_workers",
        &[("main.wi", &format!("{SLOT} {TASK}"))],
    );
    let build = project.compile_with_env("main.wi", &[("WILLOW_WORKERS", "1")]);
    assert!(
        build.status.success(),
        "{}",
        String::from_utf8_lossy(&build.stderr)
    );
    assert!(String::from_utf8_lossy(&build.stderr).contains("W2407"));
    for workers in ["1", "2", "3", "8", "32"] {
        let run = project.run_with_env(&[("WILLOW_WORKERS", workers)]);
        assert_eq!(run.status.success(), workers == "1");
        if workers == "1" {
            assert_eq!(String::from_utf8_lossy(&run.stdout), "1\n");
        } else {
            assert!(run.stdout.is_empty());
            assert!(
                String::from_utf8_lossy(&run.stderr).contains("static mut requires single-worker")
            );
        }
    }
}

#[test]
fn static_mut_policy_sync_program_and_atomic_alternative() {
    // 6: ordinary synchronous static mutation needs no worker pool.
    let (out, ok) = compile_and_run_with_env(
        &format!("{SLOT} fn main() {{ S::n = 7; println(S::n); }}"),
        &[("WILLOW_WORKERS", "8")],
    );
    assert!(ok);
    assert_eq!(out, "7\n");
    // 7–8: immutable atomic static is safe at both worker counts.
    for workers in ["1", "8"] {
        let (out, ok) = compile_and_run_with_env(
            include_str!("../../example/static_atomic.wi"),
            &[("WILLOW_WORKERS", workers)],
        );
        assert!(ok, "{out}");
        assert_eq!(out, "2\n");
    }
}

#[test]
fn static_mut_policy_storage_shapes() {
    // 9–16: every slot type/visibility and inherited or unused storage imposes the restriction.
    for declaration in [
        "class S { static mut n: i64 = 0; }",
        "class S { pub static mut n: bool = false; }",
        "class S { pub static mut n: f64 = 0.0; }",
        "class S { pub static mut n: String = \"x\"; }",
        "class S { pub static mut n: Option<i64> = Option::None; }",
        "class S { pub static mut n: AtomicI64 = AtomicI64::new(0); }",
        "open class S { pub static mut n: i64 = 0; } class Child extends S {}",
        "class S { pub static mut n: i64 = 0; pub static mut m: i64 = 1; }",
    ] {
        let source = format!("{declaration} async fn main() {{ println(99); }}");
        let project = TestProject::new("static_mut_shape", &[("main.wi", &source)]);
        let build = project.compile("main.wi");
        assert!(
            build.status.success(),
            "{}",
            String::from_utf8_lossy(&build.stderr)
        );
        let run = project.run_with_env(&[("WILLOW_WORKERS", "2")]);
        assert!(!run.status.success(), "{source}");
        assert!(String::from_utf8_lossy(&run.stderr).contains("static mut requires single-worker"));
        assert!(run.stdout.is_empty());
    }
}

#[test]
fn static_mut_policy_cross_module() {
    // 17: imported, unused mutable storage is registered before entry task execution.
    let project = TestProject::new(
        "static_mut_import",
        &[
            (
                "dep.wi",
                "module dep; pub class S { pub static mut n: i64 = 0; }",
            ),
            ("main.wi", "import dep; async fn main() { println(99); }"),
        ],
    );
    let build = project.compile("main.wi");
    assert!(
        build.status.success(),
        "{}",
        String::from_utf8_lossy(&build.stderr)
    );
    let run = project.run_with_env(&[("WILLOW_WORKERS", "2")]);
    assert!(!run.status.success());
    assert!(String::from_utf8_lossy(&run.stderr).contains("static mut requires single-worker"));
    assert!(run.stdout.is_empty());
}

#[test]
fn static_mut_policy_codegen_scaling() {
    // 18–22: no registration for immutable-only programs; exactly one for
    // increasing mutable slot counts, including many independent classes.
    for count in [0, 1, 8, 32, 128] {
        let mut source = String::new();
        for n in 0..count {
            source.push_str(&format!("class S{n} {{ pub static mut n: i64 = {n}; }}\n"));
        }
        source.push_str("fn main() {}");
        let targets = compile_and_collect_relocation_targets_all(&source, &[]);
        assert_eq!(
            targets.iter().filter(|name| name.ends_with(GUARD)).count(),
            usize::from(count > 0),
            "{count}"
        );
    }
}

#[test]
fn static_mut_policy_compile_environment_and_diagnostics() {
    // 23–25: compiler worker settings cannot remove the runtime guard.
    for workers in ["1", "2", "invalid"] {
        let (ok, diagnostics) =
            compile_with_compiler_env(&format!("{SLOT} {TASK}"), &[("WILLOW_WORKERS", workers)]);
        assert!(ok, "{diagnostics}");
        assert!(diagnostics.contains("W2407"), "{diagnostics}");
    }
    // 26: immutable statics do not warn or restrict worker count.
    let (ok, diagnostics) = compile_with_compiler_env(
        "class S { pub static n: i64 = 0; } async fn main() { println(S::n); }",
        &[],
    );
    assert!(ok, "{diagnostics}");
    assert!(!diagnostics.contains("W2407"));
}

#[test]
fn static_mut_policy_single_worker_helpers_and_gc() {
    // 27–29: transitive synchronous helpers, repeated awaits, and a GC-managed
    // replacement retain their semantics with one worker.
    let source = r#"
class S { pub static mut text: String = "old"; }
fn helper() { S::text = S::text + "!"; }
async fn work() { helper(); }
async fn main() {
    await work();
    await work();
    println(S::text);
}
"#;
    let (out, ok) = compile_and_run_with_env(
        source,
        &[("WILLOW_WORKERS", "1"), ("WILLOW_GC_STRESS", "alloc")],
    );
    assert!(ok, "{out}");
    assert_eq!(out, "old!!\n");
}
