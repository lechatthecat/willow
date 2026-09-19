//! Getter wiring plus scheduling-independent compiler perspectives. Runtime
//! tests separately assert exact movement and concurrent accounting.
use super::support::{compile_and_run, compile_and_run_release, compile_with_compiler_env};

const COUNTERS: [&str; 9] = [
    "local_pop_hits",
    "global_pop_hits",
    "global_pop_attempts",
    "steal_attempts",
    "steal_successes",
    "steal_failures",
    "victim_locks",
    "global_pushes",
    "local_pushes",
];

#[test]
fn run_queue_metrics_all_getters_are_typed_lowered_and_linked() {
    let mut source = String::new();
    for counter in COUNTERS {
        source.push_str(&format!(
            "fn read_{counter}() -> i64 {{ return sched_{counter}(); }}\n"
        ));
        let name = format!("willow_sched_{counter}");
        let symbol = willow_abi::runtime_symbol(&name).expect("ABI getter");
        assert!(symbol.params.is_empty());
        assert_eq!(symbol.ret, Some(willow_abi::AbiTy::I64));
        assert_eq!(symbol.effects, willow_abi::RuntimeEffects::NONE);
    }
    source.push_str("fn main() {\n");
    for counter in COUNTERS {
        source.push_str(&format!("println(read_{counter}() >= 0);\n"));
    }
    source.push_str("}\n");
    let (out, ok) = compile_and_run(&source);
    assert!(ok);
    assert_eq!(out, "true\n".repeat(9));
    let (ok, log) = compile_with_compiler_env(&source, &[("WILLOW_LIR_LOG", "1")]);
    assert!(ok, "{log}");
    for counter in COUNTERS {
        assert!(
            log.contains(&format!("[lir] compiling `read_{counter}` from lowered IR")),
            "{log}"
        );
    }
}

#[test]
fn run_queue_metrics_expression_and_control_flow_contexts() {
    let source = r#"
class Probe {
    pub fn read() -> i64 { return sched_local_pushes(); }
    pub static fn value() -> i64 { return sched_victim_locks(); }
}
fn identity(value: i64) -> i64 { return value; }
fn main() {
    let value: i64 = sched_global_pushes();
    sched_global_pop_hits();
    println(identity(sched_local_pushes()) >= 0);
    if sched_steal_attempts() >= 0 { println(true); }
    let mut i = 0;
    while i < 1 && sched_steal_failures() >= 0 { println(true); i = i + 1; }
    println(sched_global_pop_attempts() + value >= 0);
    defer { println(sched_local_pop_hits() >= 0); }
    let probe = new Probe();
    println(probe.read() >= 0);
    println(Probe::value() >= 0);
    let read = || -> i64 { return sched_steal_successes(); };
    println(read() >= 0);
}
"#;
    let (out, ok) = compile_and_run(source);
    assert!(ok);
    assert_eq!(out, "true\n".repeat(8));
}

#[test]
fn run_queue_metrics_reject_arguments_and_wrong_result_type() {
    for source in [
        "fn main() { sched_local_pushes(1); }",
        "fn main() { let value: bool = sched_global_pushes(); println(value); }",
    ] {
        let (ok, _) = compile_with_compiler_env(source, &[]);
        assert!(!ok, "invalid metric call accepted: {source}");
    }
}

#[test]
fn run_queue_metrics_example_async_debug_and_release() {
    let source = include_str!("../../example/run_queue_metrics.wi");
    for compile in [compile_and_run, compile_and_run_release] {
        let (out, ok) = compile(source);
        assert!(ok);
        assert_eq!(out, "true\n".repeat(9));
    }
}
