use super::*;

#[test]
fn lirreq_39_eligible_program_compiles_and_runs() {
    // The ordinary case: everything is in the subset, so the build succeeds and
    // the program prints what it should.
    assert_program_output(
        r#"
import std::collections::Array;

fn f() -> i64 {
    let xs = [1, 2, 3];
    xs.push(4);
    return xs.len() + xs[3];
}
fn main() { println(f()); }
"#,
        "8\n",
    );
}

#[test]
fn lirreq_40_ineligible_function_fails_compilation() {
    let (ok, stderr) = compile_with_compiler_env(LIR_MIXED_SOURCE, &PLAIN);
    assert!(
        !ok,
        "a body outside the walker's subset must fail the build: {stderr}"
    );
}

#[test]
fn lirreq_41_diagnostic_names_the_function() {
    let (_ok, stderr) = compile_with_compiler_env(LIR_MIXED_SOURCE, &PLAIN);
    assert!(
        stderr.contains("`unsupported`"),
        "diagnostic must name the function the walker could not take: {stderr}"
    );
}

#[test]
fn lirreq_42_diagnostic_gives_the_reason() {
    let (_ok, stderr) = compile_with_compiler_env(LIR_MIXED_SOURCE, &PLAIN);
    // The reason names the construct that blocked it, not just that something
    // did (willow-0g8j.2): the generic return type is outside the subset.
    assert!(
        stderr.contains("return type `Map<Map<String, i64>, i64>` is outside the walker's subset"),
        "diagnostic must say which construct blocked the walker: {stderr}"
    );
}

#[test]
fn lirreq_43_eligible_neighbour_is_not_reported() {
    let (_ok, stderr) = compile_with_compiler_env(LIR_MIXED_SOURCE, &PLAIN);
    assert!(
        !stderr.contains("`eligible`"),
        "a function that did compile from LIR must not be reported: {stderr}"
    );
}

#[test]
fn lirreq_44_main_args_is_supported() {
    let (ok, stderr) = compile_with_compiler_env(
        r#"
import std::collections::Array;

fn main(args: Array<String>) { println(args.len()); }
"#,
        &PLAIN,
    );
    assert!(
        ok,
        "a void `main(args: Array<String>)` must compile from LIR: {stderr}"
    );
}

#[test]
fn lirreq_45_no_environment_setting_restores_a_fallback() {
    // The kill switch and the "require" mode are gone with the emitter they
    // selected between. Setting either name is now inert: the build fails
    // exactly as it does with a clean environment.
    for env in [
        &[("WILLOW_LIR_BACKEND", "0")][..],
        &[("WILLOW_LIR_REQUIRE", "0")][..],
        &[("WILLOW_LIR_BACKEND", "0"), ("WILLOW_LIR_REQUIRE", "0")][..],
    ] {
        let (ok, stderr) = compile_with_compiler_env(LIR_MIXED_SOURCE, env);
        assert!(
            !ok,
            "the retired switch {env:?} must not compile it: {stderr}"
        );
        assert!(
            stderr.contains("`unsupported`"),
            "and the reason must be unchanged under {env:?}: {stderr}"
        );
    }
}

#[test]
fn lirreq_46_scalar_map_keys_are_inside_the_subset() {
    // The counterexample to 40-42: an `f64` key crosses the runtime ABI as the
    // one word `MapKey::Int` holds, so it compiles, runs, and renders back as a
    // float rather than as its bit pattern (willow-0g8j.3).
    assert_program_output(
        r#"
import std::collections::Map;

fn build() -> Map<f64, i64> {
    let m: Map<f64, i64> = Map::new();
    m.insert(1.5, 10);
    m.insert(2.5, 20);
    return m;
}
fn main() {
    let m = build();
    println(m.get(1.5).unwrap());
    println(m.toString());
}
"#,
        "10\n{1.5: 10, 2.5: 20}\n",
    );
}

#[test]
fn lirreq_47_method_body_outside_the_subset_fails_too() {
    // Methods take the same route (`compile_class_method`), so the same map
    // shape blocks a method and is reported against the method's name.
    let (ok, stderr) = compile_with_compiler_env(
        r#"
import std::collections::Map;

class Registry {
    pub fn nested(self) -> Map<Map<String, i64>, i64> { return Map::new(); }
}

fn main() { println(1); }
"#,
        &PLAIN,
    );
    assert!(!ok, "a method outside the subset must fail the build");
    assert!(
        stderr.contains("nested") && stderr.contains("outside the walker's subset"),
        "and be reported by name, with the reason: {stderr}"
    );
}

#[test]
fn lirreq_48_async_functions_without_language_suspension_are_lir() {
    let (ok, stderr) = compile_with_compiler_env(
        r#"
async fn work(n: i64) -> i64 { return n + 1; }
async fn main() { work(1); }
"#,
        LIR_LOG,
    );
    assert!(
        ok && stderr.contains("[lir] compiling async `work` from lowered IR")
            && stderr.contains("[lir] compiling async `main` from lowered IR"),
        "non-suspending async poll bodies must compile from LIR and report it: {stderr}"
    );
}

#[test]
fn lirreq_48b_awaiting_async_bodies_are_lir_too() {
    // `await` used to be the construct that sent a poll body back to the
    // cooperative AST emitter. Since willow-0g8j.3 there is nowhere to go: the
    // suspending body itself is walked, and says so.
    let (ok, stderr) = compile_with_compiler_env(
        r#"
async fn work(n: i64) -> i64 { return n + 1; }

async fn main() {
    let v = await work(1);
    println(v);
}
"#,
        LIR_LOG,
    );
    assert!(
        ok && stderr.contains("[lir] compiling async `main` from lowered IR"),
        "an awaiting poll body must compile from LIR and report it: {stderr}"
    );
}

#[test]
fn lirreq_49_array_example_is_fully_lir() {
    // The example claims in its header that every function it declares is
    // compiled from the lowered IR; this is what keeps that claim honest.
    let source = include_str!("../../../example/lir_gc_arrays.wi");
    let (ok, stderr) = compile_with_compiler_env(source, &PLAIN);
    assert!(
        ok,
        "example/lir_gc_arrays.wi must compile with every function on the LIR path: {stderr}"
    );
}

#[test]
fn lirreq_50_object_example_is_fully_lir() {
    // Same contract for the class-object example (willow-0g8j.5), and here it
    // covers the method side as well: since willow-0g8j.3 a class method has
    // the same hard LIR requirement its free functions do.
    let source = include_str!("../../../example/lir_gc_objects.wi");
    let (ok, stderr) = compile_with_compiler_env(source, &PLAIN);
    assert!(
        ok,
        "example/lir_gc_objects.wi must compile with every function on the LIR path: {stderr}"
    );
}

#[test]
fn lirreq_51_collections_example_is_fully_lir() {
    // Same contract for the collections example (willow-0g8j.7): its header
    // claims every function is compiled from the lowered IR.
    let source = include_str!("../../../example/lir_gc_collections.wi");
    let (ok, stderr) = compile_with_compiler_env(source, &PLAIN);
    assert!(
        ok,
        "example/lir_gc_collections.wi must compile with every function on the LIR path: {stderr}"
    );
}
