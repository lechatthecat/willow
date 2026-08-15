//! willow-s9ej.8 — conservative may-panic analysis and fast-path coverage.

use super::support::{
    TestProject, compile_and_collect_relocation_targets,
    compile_and_collect_relocation_targets_all, compile_and_run_release, compile_and_run_with_env,
    compile_with_compiler_env,
};

const PURE_RECURSION: &str = r#"
fn fib(n: i64) -> i64 {
    if n <= 1 { return n; }
    return fib(n - 1) + fib(n - 2);
}
fn main() { println(fib(10)); }
"#;

const SELF_DISPATCH_PANIC: &str = r#"
open class Base {
    pub open fn hook(self) -> i64 { return 1; }
    pub fn run(self) -> i64 { return self.hook(); }
}

class Child extends Base {
    pub override fn hook(self) -> i64 {
        panic("child hook");
        return 0;
    }
}

fn exercise() {
    if true {
        defer match recover() {
            Some(info) => println("recovered:" + info.message),
            None => println("clean")
        }
        println(new Child().run());
    }
    println("after");
}

fn main() { exercise(); }
"#;

fn has_target(targets: &[String], name: &str) -> bool {
    targets.iter().any(|target| target == name)
}

#[test]
fn pe_01_pure_recursive_function_is_reported_no_panic() {
    let (ok, stderr) =
        compile_with_compiler_env(PURE_RECURSION, &[("WILLOW_PANIC_EFFECTS_LOG", "1")]);
    assert!(ok, "{stderr}");
    assert!(stderr.contains("[panic-effects] fib: no-panic"), "{stderr}");
}

#[test]
fn pe_02_transitive_explicit_panic_is_reported_may_panic() {
    let source = r#"
fn leaf() { panic("boom"); }
fn middle() { leaf(); }
fn top() { middle(); }
fn main() {}
"#;
    let (ok, stderr) = compile_with_compiler_env(source, &[("WILLOW_PANIC_EFFECTS_LOG", "1")]);
    assert!(ok, "{stderr}");
    for function in ["leaf", "middle", "top"] {
        assert!(
            stderr.contains(&format!("[panic-effects] {function}: may-panic")),
            "{stderr}"
        );
    }
}

#[test]
fn pe_03_proven_pure_call_graph_emits_no_panic_depth_relocation() {
    let targets = compile_and_collect_relocation_targets(PURE_RECURSION, &[]);
    assert!(!has_target(&targets, "willow_panic_depth"), "{targets:?}");
    assert!(!has_target(&targets, "willow_root_depth"), "{targets:?}");
}

#[test]
fn pe_04_analysis_off_restores_conservative_depth_checks() {
    let targets =
        compile_and_collect_relocation_targets(PURE_RECURSION, &[("WILLOW_PANIC_EFFECTS", "0")]);
    assert!(has_target(&targets, "willow_panic_depth"), "{targets:?}");
    assert!(has_target(&targets, "willow_root_depth"), "{targets:?}");
}

#[test]
fn pe_05_runtime_guard_keeps_the_check() {
    let source = r#"
import std::collections::Array;
fn first(xs: Array<i64>) -> i64 { return xs[0]; }
fn main() { println(first([7])); }
"#;
    let targets = compile_and_collect_relocation_targets(source, &[]);
    assert!(has_target(&targets, "willow_panic_depth"), "{targets:?}");
}

#[test]
fn pe_06_indirect_function_value_call_stays_conservative() {
    let source = r#"
fn plus_one(n: i64) -> i64 { return n + 1; }
fn apply(f: fn(i64) -> i64, n: i64) -> i64 { return f(n); }
fn main() { println(apply(plus_one, 41)); }
"#;
    let targets = compile_and_collect_relocation_targets(source, &[]);
    assert!(has_target(&targets, "willow_panic_depth"), "{targets:?}");
}

#[test]
fn pe_07_interface_dispatch_stays_conservative() {
    let source = r#"
interface Value { fn get(self) -> i64; }
class Number implements Value {
    pub n: i64;
    pub fn get(self) -> i64 { return self.n; }
}
fn read(value: Value) -> i64 { return value.get(); }
fn main() { println(read(new Number(42))); }
"#;
    let targets = compile_and_collect_relocation_targets(source, &[]);
    assert!(has_target(&targets, "willow_panic_depth"), "{targets:?}");
}

#[test]
fn pe_08_safe_static_method_omits_its_call_check() {
    let source = r#"
class Math { pub static fn twice(n: i64) -> i64 { return n * 2; } }
fn main() { println(Math::twice(21)); }
"#;
    let targets = compile_and_collect_relocation_targets(source, &[]);
    assert!(!has_target(&targets, "willow_panic_depth"), "{targets:?}");
}

#[test]
fn pe_09_safe_constructor_omits_its_call_check() {
    let source = r#"
class Crate {
    pub init(self) {}
}
fn make() -> Crate { return new Crate(); }
fn main() { let value = make(); println(42); }
"#;
    let targets = compile_and_collect_relocation_targets(source, &[]);
    assert!(!has_target(&targets, "willow_panic_depth"), "{targets:?}");
}

#[test]
fn pe_10_all_safe_class_dispatch_targets_omit_shared_check() {
    let source = r#"
open class Base { pub open fn value(self) -> i64 { return 20; } }
class Child extends Base { pub override fn value(self) -> i64 { return 22; } }
fn read(value: Base) -> i64 { return value.value(); }
fn main() { println(read(new Child())); }
"#;
    let (ok, stderr) = compile_with_compiler_env(source, &[("WILLOW_PANIC_EFFECTS_LOG", "1")]);
    assert!(ok, "{stderr}");
    assert!(
        stderr.contains("[panic-effects] Base__value: no-panic"),
        "{stderr}"
    );
    assert!(
        stderr.contains("[panic-effects] Child__value: no-panic"),
        "{stderr}"
    );
    let optimized = compile_and_collect_relocation_targets_all(source, &[]);
    let baseline =
        compile_and_collect_relocation_targets_all(source, &[("WILLOW_PANIC_EFFECTS", "0")]);
    let optimized_checks = optimized
        .iter()
        .filter(|target| target.as_str() == "willow_panic_depth")
        .count();
    let baseline_checks = baseline
        .iter()
        .filter(|target| target.as_str() == "willow_panic_depth")
        .count();
    assert!(
        optimized_checks < baseline_checks,
        "optimized={optimized_checks}, baseline={baseline_checks}"
    );
}

#[test]
fn pe_11_one_panicking_override_keeps_shared_dispatch_check() {
    let source = r#"
open class Base { pub open fn value(self) -> i64 { return 20; } }
class Child extends Base {
    pub override fn value(self) -> i64 { panic("child"); return 0; }
}
fn read(value: Base) -> i64 { return value.value(); }
fn main() { println(read(new Base())); }
"#;
    let targets = compile_and_collect_relocation_targets(source, &[]);
    assert!(has_target(&targets, "willow_panic_depth"), "{targets:?}");
}

#[test]
fn pe_12_analysis_on_off_has_identical_pure_output() {
    let (optimized, optimized_ok) = compile_and_run_with_env(PURE_RECURSION, &[]);
    let (baseline, baseline_ok) =
        compile_and_run_with_env(PURE_RECURSION, &[("WILLOW_PANIC_EFFECTS", "0")]);
    assert!(optimized_ok && baseline_ok);
    assert_eq!(optimized, "55\n");
    assert_eq!(optimized, baseline);
}

#[test]
fn pe_13_analysis_on_off_has_identical_recovery_output() {
    let source = r#"
fn request(fail: bool) {
    if true {
        defer match recover() {
            Some(info) => println("recovered:" + info.message),
            None => println("clean")
        }
        if fail { panic("boom"); }
        println("body");
    }
    println("after");
}
fn main() { request(false); request(true); }
"#;
    let (optimized, optimized_ok) = compile_and_run_with_env(source, &[]);
    let (baseline, baseline_ok) =
        compile_and_run_with_env(source, &[("WILLOW_PANIC_EFFECTS", "0")]);
    assert!(optimized_ok && baseline_ok, "{optimized}\n{baseline}");
    assert_eq!(optimized, baseline);
}

#[test]
fn pe_14_ast_and_lir_paths_have_identical_effect_behavior() {
    let (lir, lir_ok) = compile_and_run_with_env(
        PURE_RECURSION,
        &[("WILLOW_LIR_BACKEND", "1"), ("WILLOW_LIR_REQUIRE", "1")],
    );
    let (ast, ast_ok) = compile_and_run_with_env(PURE_RECURSION, &[("WILLOW_LIR_BACKEND", "0")]);
    assert!(lir_ok && ast_ok);
    assert_eq!(lir, ast);
}

#[test]
fn pe_15_cross_module_pure_summary_reaches_entry_call_site() {
    let project = TestProject::new(
        "panic_effect_module",
        &[
            (
                "math.wi",
                "module math; pub fn add(a: i64, b: i64) -> i64 { return a + b; }",
            ),
            (
                "main.wi",
                "import math; fn main() { println(math::add(20, 22)); }",
            ),
        ],
    );
    let compile = project.compile_with_env("main.wi", &[("WILLOW_PANIC_EFFECTS_LOG", "1")]);
    let stderr = String::from_utf8_lossy(&compile.stderr);
    assert!(compile.status.success(), "{stderr}");
    assert!(
        stderr.contains("[panic-effects] math__add: no-panic"),
        "{stderr}"
    );
    let run = project.run();
    assert!(run.status.success());
    assert_eq!(String::from_utf8_lossy(&run.stdout), "42\n");
}

#[test]
fn pe_16_cross_module_static_method_alias_carries_no_panic_summary() {
    let project = TestProject::new(
        "panic_effect_module_method",
        &[
            (
                "math.wi",
                "module math; pub class Math { pub static fn twice(n: i64) -> i64 { return n * 2; } }",
            ),
            (
                "main.wi",
                "import math::Math; fn main() { println(Math::twice(21)); }",
            ),
        ],
    );
    let compile = project.compile_with_env("main.wi", &[("WILLOW_PANIC_EFFECTS_LOG", "1")]);
    let stderr = String::from_utf8_lossy(&compile.stderr);
    assert!(compile.status.success(), "{stderr}");
    assert!(
        stderr.contains("[panic-effects] math__Math__twice: no-panic"),
        "{stderr}"
    );
    let run = project.run();
    assert!(run.status.success());
    assert_eq!(String::from_utf8_lossy(&run.stdout), "42\n");
}

#[test]
fn pe_17_cross_module_panicking_summary_propagates_to_entry_wrapper() {
    let project = TestProject::new(
        "panic_effect_module_panic",
        &[
            (
                "danger.wi",
                "module danger; pub fn fail() { panic(\"module panic\"); }",
            ),
            (
                "main.wi",
                "import danger::fail; fn wrapper() { fail(); } fn main() {}",
            ),
        ],
    );
    let compile = project.compile_with_env("main.wi", &[("WILLOW_PANIC_EFFECTS_LOG", "1")]);
    let stderr = String::from_utf8_lossy(&compile.stderr);
    assert!(compile.status.success(), "{stderr}");
    assert!(
        stderr.contains("[panic-effects] danger__fail: may-panic"),
        "{stderr}"
    );
    assert!(
        stderr.contains("[panic-effects] wrapper: may-panic"),
        "{stderr}"
    );
}

// willow-s9ej.11 perspectives 18-24: the integration layer pins the logged
// summary, emitted checks, runtime recovery, optimizer on/off parity,
// AST/LIR selection parity, grandchild propagation, and debug/release parity.
#[test]
fn pe_18_virtual_self_call_with_panicking_override_marks_caller_may_panic() {
    let (ok, stderr) =
        compile_with_compiler_env(SELF_DISPATCH_PANIC, &[("WILLOW_PANIC_EFFECTS_LOG", "1")]);
    assert!(ok, "{stderr}");
    assert!(
        stderr.contains("[panic-effects] Base__run: may-panic"),
        "{stderr}"
    );
}

#[test]
fn pe_19_virtual_self_call_propagates_child_panic_to_recover() {
    let (out, ok) = compile_and_run_with_env(SELF_DISPATCH_PANIC, &[]);
    assert!(ok, "{out}");
    assert_eq!(out, "recovered:child hook\nafter\n");
}

#[test]
fn pe_20_virtual_self_call_matches_when_effect_optimization_is_disabled() {
    let (optimized, optimized_ok) = compile_and_run_with_env(SELF_DISPATCH_PANIC, &[]);
    let (baseline, baseline_ok) =
        compile_and_run_with_env(SELF_DISPATCH_PANIC, &[("WILLOW_PANIC_EFFECTS", "0")]);
    assert!(optimized_ok && baseline_ok, "{optimized}\n{baseline}");
    assert_eq!(optimized, baseline);
}

#[test]
fn pe_21_all_safe_virtual_self_targets_still_remove_shared_checks() {
    let source = r#"
open class Base {
    pub open fn hook(self) -> i64 { return 20; }
    pub fn run(self) -> i64 { return self.hook(); }
}
class Child extends Base {
    pub override fn hook(self) -> i64 { return 22; }
}
fn main() { println(new Child().run()); }
"#;
    let (ok, stderr) = compile_with_compiler_env(source, &[("WILLOW_PANIC_EFFECTS_LOG", "1")]);
    assert!(ok, "{stderr}");
    assert!(
        stderr.contains("[panic-effects] Base__run: no-panic"),
        "{stderr}"
    );
    let optimized = compile_and_collect_relocation_targets_all(source, &[]);
    let baseline =
        compile_and_collect_relocation_targets_all(source, &[("WILLOW_PANIC_EFFECTS", "0")]);
    let optimized_checks = optimized
        .iter()
        .filter(|target| target.as_str() == "willow_panic_depth")
        .count();
    let baseline_checks = baseline
        .iter()
        .filter(|target| target.as_str() == "willow_panic_depth")
        .count();
    assert!(
        optimized_checks < baseline_checks,
        "optimized={optimized_checks}, baseline={baseline_checks}"
    );
}

#[test]
fn pe_22_virtual_self_call_includes_grandchild_override() {
    let source = r#"
open class Base {
    pub open fn hook(self) -> i64 { return 1; }
    pub fn run(self) -> i64 { return self.hook(); }
}
open class Middle extends Base {}
class Leaf extends Middle {
    pub override fn hook(self) -> i64 { panic("leaf"); return 0; }
}
fn main() {}
"#;
    let (ok, stderr) = compile_with_compiler_env(source, &[("WILLOW_PANIC_EFFECTS_LOG", "1")]);
    assert!(ok, "{stderr}");
    assert!(
        stderr.contains("[panic-effects] Base__run: may-panic"),
        "{stderr}"
    );
}

#[test]
fn pe_23_virtual_self_call_agrees_across_ast_and_lir_selection() {
    let (ast, ast_ok) =
        compile_and_run_with_env(SELF_DISPATCH_PANIC, &[("WILLOW_LIR_BACKEND", "0")]);
    let (lir, lir_ok) =
        compile_and_run_with_env(SELF_DISPATCH_PANIC, &[("WILLOW_LIR_BACKEND", "1")]);
    assert!(ast_ok && lir_ok, "{ast}\n{lir}");
    assert_eq!(ast, lir);
}

#[test]
fn pe_24_virtual_self_call_agrees_in_release() {
    let (debug, debug_ok) = compile_and_run_with_env(SELF_DISPATCH_PANIC, &[]);
    let (release, release_ok) = compile_and_run_release(SELF_DISPATCH_PANIC);
    assert!(debug_ok && release_ok, "{debug}\n{release}");
    assert_eq!(debug, release);
}
