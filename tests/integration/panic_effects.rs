//! willow-s9ej.8 — conservative may-panic analysis and fast-path coverage.

use super::support::{
    TestProject, compile_and_collect_relocation_targets,
    compile_and_collect_relocation_targets_all, compile_and_collect_relocation_targets_mode,
    compile_and_run_release, compile_and_run_with_env, compile_with_compiler_env,
};

const PURE_RECURSION: &str = r#"
fn fib(n: i64) -> i64 {
    if n <= 1 { return n; }
    return fib(n - 1) + fib(n - 2);
}
fn main() { println(fib(10)); }
"#;

const SNAPSHOT_CHAIN: &str = r#"
fn step(n: i64) -> i64 { if n < 0 { panic("negative"); } return n + 1; }
fn chain(n: i64) -> i64 {
    let a = step(n);
    let b = step(a);
    let c = step(b);
    return c;
}
fn main() { println(chain(0)); }
"#;

#[test]
fn panic_snapshots_reduce_three_surviving_straight_line_reads() {
    for release in [false, true] {
        let optimized = compile_and_collect_relocation_targets_mode(SNAPSHOT_CHAIN, &[], release);
        let baseline = compile_and_collect_relocation_targets_mode(
            SNAPSHOT_CHAIN,
            &[("WILLOW_PANIC_SNAPSHOT_REUSE", "0")],
            release,
        );
        let count = |names: &[String]| {
            names
                .iter()
                .filter(|name| name.as_str() == "willow_panic_depth")
                .count()
        };
        assert!(count(&optimized) > 0, "may-panic calls must retain checks");
        let expected_reduction = if release { 2 } else { 0 };
        assert!(
            count(&baseline) >= count(&optimized) + expected_reduction,
            "expected stable-region read reduction, release={release}: baseline={}, optimized={}",
            count(&baseline),
            count(&optimized)
        );
    }
}

#[test]
fn panic_snapshots_do_not_add_reads_to_pure_calls() {
    for release in [false, true] {
        let targets = compile_and_collect_relocation_targets_mode(PURE_RECURSION, &[], release);
        assert!(!has_target(&targets, "willow_panic_depth"));
    }
}

#[test]
fn panic_snapshots_recover_then_raise_again_and_panic_in_second_call() {
    let source = r#"
fn step(n: i64) -> i64 { if n < 0 { panic("negative"); } return n + 1; }
fn active_helper() {
    defer match recover() {
        Some(_) => {
            println(step(1));
            if true {
                defer match recover() { Some(info) => println(info.message), None => {} }
                println(step(2));
                println(step(-1));
                println(999);
            }
            println(step(3));
        },
        None => {}
    }
    panic("outer");
}
fn main() { active_helper(); println(42); }
"#;
    for reuse in ["0", "1"] {
        let (out, ok) = compile_and_run_with_env(source, &[("WILLOW_PANIC_SNAPSHOT_REUSE", reuse)]);
        assert!(ok, "{out}");
        assert_eq!(out, "2\n3\nnegative\n4\n42\n");
    }
    let (out, ok) = compile_and_run_release(source);
    assert!(ok, "{out}");
    assert_eq!(out, "2\n3\nnegative\n4\n42\n");
}

#[test]
fn panic_snapshots_preserve_control_flow_and_active_panic_entry() {
    let fixtures = [
        (
            r#"
fn step(n: i64) -> i64 { if n < 0 { panic("negative"); } return n + 1; }
fn choose(flag: bool) -> i64 {
    let mut value = 0;
    if flag { value = step(1); } else { value = step(2); value = step(value); }
    return step(value);
}
fn main() { println(choose(true)); println(choose(false)); }
"#,
            "3\n5\n",
        ),
        (
            r#"
fn step(n: i64) -> i64 { if n == 2 { panic("iteration"); } return n + 1; }
fn main() {
    let mut i = 0;
    while i < 4 {
        if true {
            defer match recover() { Some(info) => println(info.message), None => {} }
            println(step(i));
        }
        i = i + 1;
    }
    println(42);
}
"#,
            "1\n2\niteration\n4\n42\n",
        ),
        (
            r#"
fn step(n: i64) -> i64 { if n < 0 { panic("negative"); } return n + 1; }
fn chain(n: i64) -> i64 { let a = step(n); let b = step(a); return step(b); }
fn action() {
    defer {
        println(chain(0));
        match recover() { Some(info) => println(info.message), None => {} }
        println(chain(1));
    }
    panic("outer");
}
fn main() { action(); println(42); }
"#,
            "3\nouter\n4\n42\n",
        ),
        (
            r#"
class Math {
    pub value: i64;
    pub init(self, value: i64) { if value < 0 { panic("negative"); } self.value = value; }
    pub fn step(self) -> i64 { if self.value < 0 { panic("negative"); } return self.value + 1; }
    pub static fn next(n: i64) -> i64 { if n < 0 { panic("negative"); } return n + 1; }
}
fn early(n: i64) -> i64 {
    if n == 0 { return Math::next(n); }
    let a = Math::next(n); let b = Math::next(a); return Math::next(b);
}
fn main() { let item = new Math(1); println(item.step()); println(early(0)); println(early(1)); }
"#,
            "2\n1\n4\n",
        ),
    ];
    for (source, expected) in fixtures {
        for reuse in ["0", "1"] {
            let (out, ok) = compile_and_run_with_env(
                source,
                &[
                    ("WILLOW_PANIC_SNAPSHOT_REUSE", reuse),
                    ("WILLOW_GC_STRESS", "alloc"),
                ],
            );
            assert!(ok, "{out}");
            assert_eq!(out, expected);
        }
        let (out, ok) = compile_and_run_release(source);
        assert!(ok, "{out}");
        assert_eq!(out, expected);
    }
}

#[test]
fn nested_recovery_can_panic_again_inside_the_inner_deferred_action() {
    let source = r#"
fn action() {
    defer match recover() {
        Some(_) => {
            if true {
                defer match recover() { Some(info) => println(info.message), None => {} }
                defer { panic("inner defer"); }
                panic("inner body");
            }
            println("resumed");
        },
        None => {}
    }
    panic("outer");
}
fn main() {
    if true {
        defer match recover() { Some(info) => println(info.message), None => {} }
        action();
    }
    println(42);
}
"#;
    for reuse in ["0", "1"] {
        let (out, ok) = compile_and_run_with_env(source, &[("WILLOW_PANIC_SNAPSHOT_REUSE", reuse)]);
        assert!(ok, "{out}");
        assert_eq!(out, "inner defer\ninner body\n42\n");
    }
}

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
    // Task-aware synchronous cancellation restores GC roots independently of
    // panic propagation. Its root-depth relocation does not make this pure
    // call graph require a panic-depth check.
}

#[test]
fn pe_04_analysis_off_restores_conservative_depth_checks() {
    let targets =
        compile_and_collect_relocation_targets(PURE_RECURSION, &[("WILLOW_PANIC_EFFECTS", "0")]);
    assert!(has_target(&targets, "willow_panic_depth"), "{targets:?}");
    // Disabling panic effects must not disable the independent zero-root proof.
    assert!(!has_target(&targets, "willow_root_depth"), "{targets:?}");
}

#[test]
fn pe_zero_root_panicking_chain_omits_root_unwind_calls() {
    let source = r#"
fn divide(n: i64) -> i64 { return 42 / n; }
fn recurse(n: i64) -> i64 {
    if n <= 1 { return divide(n); }
    return recurse(n - 1);
}
class Math { pub static fn calculate(n: i64) -> i64 { return recurse(n); } }
fn main() { println(Math::calculate(2)); }
"#;
    for release in [false, true] {
        for env in [&[][..], &[("WILLOW_PANIC_EFFECTS", "0")][..]] {
            let targets = compile_and_collect_relocation_targets_mode(source, env, release);
            assert!(has_target(&targets, "willow_panic_depth"), "{targets:?}");
            for name in ["willow_root_depth", "willow_pop_roots", "willow_push_root"] {
                assert!(
                    !has_target(&targets, name),
                    "unexpected {name}: {targets:?}"
                );
            }
        }
    }
}

#[test]
fn pe_zero_root_proof_keeps_reference_and_unknown_cleanup() {
    // Each fixture has an empty main: the helper alone must retain the entry
    // snapshot and abnormal-return pop, even with panic effects disabled.
    let fixtures = [
        "fn control(n: i64) -> i64 { let s = \"root\"; return 1 / n; }",
        "class Box {} fn control(n: i64) -> i64 { let b = new Box(); return 1 / n; }",
        "import std::collections::Array; fn control(n: i64) -> i64 { let a: Array<i64> = [1]; return 1 / n; }",
        "import std::collections::Map; fn control(n: i64) -> i64 { let m: Map<i64, i64> = Map::new(); return 1 / n; }",
        "fn control(n: i64) -> i64 { let c = Channel<i64>::new(); return 1 / n; }",
        "interface Value { fn get(self) -> i64; } fn control(v: Value) -> i64 { return v.get(); }",
        "fn control(f: fn(i64) -> i64) -> i64 { return f(0); }",
        "fn control(n: &i64) -> i64 { return 1 / n; }",
        "fn make() -> String { return \"root\"; } fn control() { make(); }",
        "fn control(n: i64) -> i64 { defer { let s = \"root\"; println(s); } return 1 / n; }",
        "fn control(n: i64) -> i64 { match n { 0 => { let s = \"root\"; println(s); }, _ => {} } return 1 / n; }",
        "fn control(n: i64) -> i64 { let mut i = 0; while i < n { let s = \"root\"; println(s); i = i + 1; } return 1 / n; }",
        "class Control { pub fn divide(self, n: i64) -> i64 { return 1 / n; } }",
        "fn control() { panic(\"explicit\"); }",
    ];
    for fixture in fixtures {
        let source = format!("{fixture}\nfn main() {{}}");
        for release in [false, true] {
            let targets = compile_and_collect_relocation_targets_mode(
                &source,
                &[("WILLOW_PANIC_EFFECTS", "0")],
                release,
            );
            for name in ["willow_root_depth", "willow_pop_roots"] {
                assert!(
                    has_target(&targets, name),
                    "missing {name} for {fixture}: {targets:?}"
                );
            }
        }
    }
}

#[test]
fn pe_zero_root_panic_returns_preserve_typed_abi_and_recovery() {
    let source = r#"
fn fail() { panic("explicit"); }
fn integer(n: i64) -> i64 { return 1 / n; }
fn floating() -> f64 { fail(); return 2.5; }
fn boolean() -> bool { fail(); return true; }
fn empty() { fail(); }
class Math { pub static fn divide(n: i64) -> i64 { return integer(n); } }
fn exercise(which: i64) {
    defer match recover() {
        Some(info) => println("recovered"),
        None => println("missed")
    }
    if which == 0 { println(integer(0)); }
    if which == 1 { println(floating()); }
    if which == 2 { println(boolean()); }
    if which == 3 { empty(); }
    if which == 4 { println(Math::divide(0)); }
    println("unreachable");
}
fn main() {
    let mut i = 0;
    while i < 5 { exercise(i); i = i + 1; }
    println("after");
}
"#;
    let expected = "recovered\n".repeat(5) + "after\n";
    for env in [
        &[][..],
        &[("WILLOW_PANIC_EFFECTS", "0")][..],
        &[
            ("WILLOW_GC_STRESS", "alloc"),
            ("WILLOW_GC_VERIFY_BARRIER", "1"),
        ][..],
    ] {
        let (out, ok) = compile_and_run_with_env(source, env);
        assert!(ok, "{out}");
        assert_eq!(out, expected);
    }
    let (out, ok) = compile_and_run_release(source);
    assert!(ok, "{out}");
    assert_eq!(out, expected);
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
        stderr.contains("[panic-effects] Base.value: no-panic"),
        "{stderr}"
    );
    assert!(
        stderr.contains("[panic-effects] Child.value: no-panic"),
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

// The effect summary is computed before codegen, so a recursive pure function
// has to reach the same conclusion under GC stress, where every allocation is a
// collection point and a dropped panic-depth snapshot would show.
#[test]
fn pe_14_a_purely_recursive_program_is_stable_under_gc_stress() {
    let (plain, plain_ok) = compile_and_run_with_env(PURE_RECURSION, &[]);
    assert!(plain_ok, "{plain}");
    let (stressed, stressed_ok) =
        compile_and_run_with_env(PURE_RECURSION, &[("WILLOW_GC_STRESS", "alloc")]);
    assert!(stressed_ok, "{stressed}");
    assert_eq!(plain, stressed);
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
        stderr.contains("[panic-effects] math.add: no-panic"),
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
        stderr.contains("[panic-effects] math.Math.twice: no-panic"),
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
        stderr.contains("[panic-effects] danger.fail: may-panic"),
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
        stderr.contains("[panic-effects] Base.run: may-panic"),
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
        stderr.contains("[panic-effects] Base.run: no-panic"),
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
        stderr.contains("[panic-effects] Base.run: may-panic"),
        "{stderr}"
    );
}

#[test]
fn pe_23_virtual_self_call_survives_gc_stress() {
    // The recovery carries an `info.message` allocated on the panic path, so a
    // collection at every allocation is what would show a missing root.
    let (out, ok) = compile_and_run_with_env(SELF_DISPATCH_PANIC, &[("WILLOW_GC_STRESS", "alloc")]);
    assert!(ok, "{out}");
    assert_eq!(out, "recovered:child hook\nafter\n");
}

#[test]
fn pe_24_virtual_self_call_agrees_in_release() {
    let (debug, debug_ok) = compile_and_run_with_env(SELF_DISPATCH_PANIC, &[]);
    let (release, release_ok) = compile_and_run_release(SELF_DISPATCH_PANIC);
    assert!(debug_ok && release_ok, "{debug}\n{release}");
    assert_eq!(debug, release);
}

// --- Shared structural AST walk (willow-uqzx.1.1) ---
//
// The effect summary reads the shared walk in `parser::visit` rather than its
// own traversal. A child slot the walk never reaches makes a hazardous
// function summarize as NO_PANIC, its caller drops the panic-depth snapshot,
// and a recovery that should fire silently does not. These perspectives pin
// the slots from outside the compiler.

const SHARED_AST_WALK: &str = include_str!("../../example/shared_ast_walk.wi");

const SHARED_AST_WALK_OUTPUT: &str = "ternary <- ternary\n\
binary <- binary\n\
array-element <- array-element\n\
match-arm <- match-arm\n\
method-argument <- method-argument\n\
new-argument <- new-argument\n\
static-argument <- static-argument\n\
unary-operand <- unary-operand\n\
nested-statement <- nested-statement\n\
control survived with 42\n\
control <- clean\n\
5\n";

#[test]
fn pe_25_walk_reaches_a_hazard_in_every_nested_expression_slot() {
    let (ok, stderr) =
        compile_with_compiler_env(SHARED_AST_WALK, &[("WILLOW_PANIC_EFFECTS_LOG", "1")]);
    assert!(ok, "{stderr}");
    for function in [
        "hidden_in_ternary",
        "hidden_in_binary",
        "hidden_in_array_element",
        "hidden_in_match_arm",
        "hidden_in_method_argument",
        "hidden_in_new_argument",
        "hidden_in_static_argument",
        "hidden_in_unary_operand",
        "hidden_in_nested_statement",
    ] {
        assert!(
            stderr.contains(&format!("[panic-effects] {function}: may-panic")),
            "the walk never reached the hazard in `{function}`:\n{stderr}"
        );
    }
    // The controls: same shape, no hazard. If these flipped to may-panic the
    // may-panic assertions above would prove nothing.
    for function in ["hidden_nowhere", "looper", "Cell.of"] {
        assert!(
            stderr.contains(&format!("[panic-effects] {function}: no-panic")),
            "`{function}` has no hazard and must summarize as no-panic:\n{stderr}"
        );
    }
}

#[test]
fn pe_26_walk_summaries_match_unconditional_panic_checks() {
    let (summarized, summarized_ok) = compile_and_run_with_env(SHARED_AST_WALK, &[]);
    let (unconditional, unconditional_ok) =
        compile_and_run_with_env(SHARED_AST_WALK, &[("WILLOW_PANIC_EFFECTS", "0")]);
    assert!(
        summarized_ok && unconditional_ok,
        "{summarized}\n{unconditional}"
    );
    assert_eq!(summarized, SHARED_AST_WALK_OUTPUT);
    assert_eq!(summarized, unconditional);
}

#[test]
fn pe_27_walk_summaries_hold_under_gc_stress() {
    let (out, ok) = compile_and_run_with_env(SHARED_AST_WALK, &[("WILLOW_GC_STRESS", "alloc")]);
    assert!(ok, "{out}");
    assert_eq!(out, SHARED_AST_WALK_OUTPUT);
}

#[test]
fn pe_28_walk_summaries_agree_in_release() {
    let (release, release_ok) = compile_and_run_release(SHARED_AST_WALK);
    assert!(release_ok, "{release}");
    assert_eq!(release, SHARED_AST_WALK_OUTPUT);
}

#[test]
fn pe_29_walk_reaches_a_hazard_in_every_nested_statement_slot() {
    let source = r#"
import std::collections::Array;

fn boom(tag: String) -> i64 {
    panic(tag);
    return 0;
}

fn in_while_body(n: i64) -> i64 {
    let mut i = 0;
    while i < n {
        let caught = boom("while");
        i = i + 1;
    }
    return i;
}

fn in_for_body() -> i64 {
    let xs: Array<i64> = [1, 2];
    for x in xs {
        let caught = boom("for");
    }
    return 1;
}

fn in_match_scrutinee(n: i64) -> i64 {
    return match boom("scrutinee") {
        1 => 1,
        _ => 2
    };
}

fn in_assignment(n: i64) -> i64 {
    let mut total = 0;
    total = boom("assignment");
    return total;
}

fn in_defer_body() -> i64 {
    defer {
        let caught = boom("defer");
    }
    return 1;
}

fn no_hazard(n: i64) -> i64 {
    let mut total = 0;
    let mut i = 0;
    while i < n {
        total = total + i;
        i = i + 1;
    }
    return total;
}

fn main() {
    println(no_hazard(3));
}
"#;
    let (ok, stderr) = compile_with_compiler_env(source, &[("WILLOW_PANIC_EFFECTS_LOG", "1")]);
    assert!(ok, "{stderr}");
    for function in [
        "in_while_body",
        "in_for_body",
        "in_match_scrutinee",
        "in_assignment",
        "in_defer_body",
    ] {
        assert!(
            stderr.contains(&format!("[panic-effects] {function}: may-panic")),
            "the walk never reached the hazard in `{function}`:\n{stderr}"
        );
    }
    assert!(
        stderr.contains("[panic-effects] no_hazard: no-panic"),
        "a loop with no hazard must stay no-panic:\n{stderr}"
    );
}

/// A lambda body is a separate callable. A panic inside one that is never
/// invoked does not belong to the function that merely defines it — the walk
/// stops at the lambda instead of folding its body into the enclosing summary.
#[test]
fn pe_30_uninvoked_lambda_body_panic_does_not_escape_its_definer() {
    let source = r#"
fn defines_only(n: i64) -> i64 {
    let unused: fn(i64) -> i64 = |x: i64| -> i64 {
        panic("never invoked");
        return x;
    };
    return n + 1;
}

fn main() {
    println(defines_only(1));
}
"#;
    let (ok, stderr) = compile_with_compiler_env(source, &[("WILLOW_PANIC_EFFECTS_LOG", "1")]);
    assert!(ok, "{stderr}");
    assert!(
        stderr.contains("[panic-effects] defines_only: no-panic"),
        "{stderr}"
    );
}

/// Invoking a local function value is a different matter: the walk cannot know
/// what it holds, so the call is conservative and the definer may panic. This
/// is the fail-closed direction, and it is what keeps the skip above sound.
#[test]
fn pe_31_invoked_local_function_value_keeps_its_caller_conservative() {
    let source = r#"
fn invokes(n: i64) -> i64 {
    let op: fn(i64) -> i64 = |x: i64| -> i64 { return x + 1; };
    return op(n);
}

fn main() {
    println(invokes(1));
}
"#;
    let (ok, stderr) = compile_with_compiler_env(source, &[("WILLOW_PANIC_EFFECTS_LOG", "1")]);
    assert!(ok, "{stderr}");
    assert!(
        stderr.contains("[panic-effects] invokes: may-panic"),
        "{stderr}"
    );
}
