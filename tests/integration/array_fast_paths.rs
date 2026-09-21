use super::support::*;
use std::time::Duration;

#[test]
fn array_fast_paths_scalar_words_growth_and_reference_parameter() {
    let source = include_str!("../../example/array_fast_paths.wi");
    for (out, ok) in [
        compile_and_run(source),
        compile_and_run_release(source),
        compile_and_run_gc_stress(source),
    ] {
        assert!(ok, "{out}");
        assert_eq!(out, "42\ntrue\ntrue\n42\n7\n");
    }
}

#[test]
fn array_fast_paths_bounds_recover_and_defer() {
    let mut source = String::from("import std::collections::Array; fn main() {\n");
    let mut expected = String::new();
    for (array, operation) in [
        ("[1]", "println(xs[-1]);"),
        ("[1]", "xs[-1] = 2;"),
        ("[1]", "println(xs[1]);"),
        ("[1]", "xs[1] = 2;"),
        ("[]", "println(xs[0]);"),
        ("[]", "xs[0] = 2;"),
    ] {
        source.push_str(&format!(
            r#"
        if true {{
            defer match recover() {{ Some(_) => println("caught"), None => println("missing") }}
            defer println("cleanup");
            let xs: Array<i64> = {array};
            {operation}
            println("unreachable");
        }}
        "#
        ));
        expected.push_str("cleanup\ncaught\n");
    }
    source.push('}');
    for (out, ok) in [compile_and_run(&source), compile_and_run_release(&source)] {
        assert!(ok, "{out}");
        assert_eq!(out, expected);
    }
}

#[test]
fn array_fast_paths_frame_and_allocating_reference_coercion() {
    let source = r#"
import std::collections::Array;
interface Value extends Send { fn get() -> i64; }
class Box implements Value { pub n: i64; pub fn get() -> i64 { return self.n; } }
async fn main() {
    let xs = [1, 2];
    let refs: Array<Value> = [new Box(3)];
    await sleep(1);
    xs[0] = 40;
    refs[0] = new Box(7);
    gc_minor_collect();
    println(xs[0] + xs[1]);
    println(refs[0].get());
    let nested = [[4, 5], [6, 7]];
    gc_collect();
    println(nested[1][0]);
    let frozen = xs.freeze();
    await sleep(1);
    println(frozen.len());
    println(frozen[0]);
}
"#;
    let (out, ok) = compile_and_run_with_runtime_env(
        source,
        &[
            ("WILLOW_GC_STRESS", "alloc,scheduler"),
            ("WILLOW_TASK_BUDGET", "1"),
        ],
        Duration::from_secs(30),
    );
    assert!(ok, "{out}");
    assert_eq!(out, "42\n7\n6\n2\n40\n");
}

#[test]
fn array_fast_paths_native_loop_remains_cancellable() {
    let (out, ok) = compile_and_run_with_runtime_env(
        r#"
import std::collections::Array;
fn spin(ready: Channel<i64>) {
    let xs = [1];
    defer println("cleanup");
    ready.send(1);
    while true { xs[0] = xs[0] + xs.len(); }
}
async fn worker(ready: Channel<i64>) { spin(ready); }
async fn main() {
    let ready = Channel<i64>::new();
    let task = worker(ready);
    ready.recv();
    task.cancel();
    await task.result();
    println("done");
}
"#,
        &[("WILLOW_WORKERS", "2"), ("WILLOW_TASK_BUDGET", "1")],
        Duration::from_secs(30),
    );
    assert!(ok, "{out}");
    assert_eq!(out, "cleanup\ndone\n");
}

#[test]
fn array_fast_paths_do_not_add_temporary_root_pairs_per_access() {
    let counts: Vec<_> = [1, 8, 32].into_iter().map(|sites| {
        let source = format!(
            "import std::collections::Array; fn work(xs: Array<i64>, i: i64) {{ {} }} fn main() {{ work([1], 0); }}",
            "xs[i] = xs[i] + xs.len();".repeat(sites),
        );
        let names = compile_and_collect_relocation_targets_mode(&source, &[], true);
        names.iter().filter(|name| name.as_str() == "willow_pop_roots").count()
    }).collect();
    // Function-level and unwind roots remain, but adding access sites must
    // not add a per-store or per-length push/pop pair.
    assert!(counts[0] > 0);
    assert!(counts.iter().all(|count| *count == counts[0]), "{counts:?}");
}
