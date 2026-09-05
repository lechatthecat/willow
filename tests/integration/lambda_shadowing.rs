//! willow-bv9.1 — lexical function values shadow unqualified free functions.
//!
//! Perspectives:
//!  01 same-scope lambda, 02 nested block, 03 fn-valued parameter,
//!  04 fn-valued lambda parameter, 05 shadow ends at scope exit,
//!  06 free function declared after its caller, 07 `format` builtin shadow,
//!  08 `panic` builtin shadow, 09 reserved `recover` remains unshadowable,
//!  10 mutable function local reassignment, 11 two nested shadows,
//!  12 noncapturing block lambda, 13 a CAPTURING shadow is still the callee,
//!  14 local arity diagnostic wins, 15 local parameter-type diagnostic wins,
//!  16 non-callable local blocks free-function fallback, 17 loop scope,
//!  18 deferred indirect call, 19 async function after suspension,
//!  20-21 a shadowing local lambda is the callee, not the free function,
//!  22 debug/release parity, 23 static/method calls remain unaffected,
//!  24 indirect calls remain conservative in panic-effect analysis.

use super::support::{
    assert_compile_error_contains, compile_and_collect_relocation_targets, compile_and_run,
    compile_and_run_release, compile_and_run_with_env,
};

const SHADOW_MATRIX: &str = r#"
fn f(x: i64) -> i64 { return x + 1; }

fn call_parameter(f: fn(i64) -> i64) -> i64 { return f(2); }

fn later_caller() -> i64 {
    let later = |x: i64| -> i64 { return x + 60; };
    return later(3);
}
fn later(x: i64) -> i64 { return x + 600; }

class Box {
    pub fn f(self, x: i64) -> i64 { return x + 700; }
    pub static fn call(x: i64) -> i64 { return x + 800; }
}

fn main() {
    let f = |x: i64| -> i64 { return x + 100; };
    println(f(1));

    if true {
        let f = |x: i64| -> i64 { return x + 200; };
        println(f(1));
        if true {
            let f = |x: i64| -> i64 { return x + 300; };
            println(f(1));
        }
        println(f(2));
    }
    println(f(2));

    println(call_parameter(|x: i64| -> i64 { return x + 400; }));
    let invoke = |f: fn(i64) -> i64| -> i64 { return f(3); };
    println(invoke(|x: i64| -> i64 { return x + 500; }));
    println(later_caller());

    let format = |x: i64| -> i64 { return x + 10; };
    let panic = |x: i64| -> i64 { return x + 20; };
    println(format(1));
    println(panic(1));

    let mut mutable_f: fn(i64) -> i64 = |x: i64| x + 40;
    mutable_f = |x: i64| x + 50;
    println(mutable_f(1));

    let mut n = 0;
    while n < 1 {
        let f = |x: i64| -> i64 { return x + 900; };
        println(f(1));
        n = n + 1;
    }

    println(new Box().f(1));
    println(Box::call(1));
}
"#;

const SHADOW_OUTPUT: &str = "101\n201\n301\n202\n102\n402\n503\n63\n11\n21\n51\n901\n701\n801\n";

#[test]
fn lambda_shadow_01_through_12_and_17_23_runtime_matrix() {
    let (out, ok) = compile_and_run(SHADOW_MATRIX);
    assert!(ok, "{out}");
    assert_eq!(out, SHADOW_OUTPUT);
}

/// Shadowing does not depend on the lambda being a bare code address: since
/// willow-0g8j.2.12 a capturing lambda is a `closure` value, and the local
/// still wins over the free function of the same name — the call goes through
/// the environment rather than to `f`'s symbol.
#[test]
fn lambda_shadow_13_a_capturing_shadow_is_still_the_callee() {
    let (out, ok) = compile_and_run(
        r#"
fn f(x: i64) -> i64 { return x + 1; }
fn main() {
    let offset = 100;
    let f = |x: i64| -> i64 { return x + offset; };
    println(f(1));
}
"#,
    );
    assert!(ok, "{out}");
    assert_eq!(out, "101\n");
    assert_compile_error_contains(
        r#"fn main() { let recover = |x: i64| x; println(recover(1)); }"#,
        &["error[E0351]", "`recover` is a reserved builtin function"],
    );
}

#[test]
fn lambda_shadow_14_arity_uses_the_selected_local_signature() {
    assert_compile_error_contains(
        r#"
fn f(x: i64, y: i64) -> i64 { return x + y; }
fn main() {
    let f = |x: i64| -> i64 { return x; };
    println(f(1, 2));
}
"#,
        &[
            "error[E0201]",
            "function value `f` takes 1 argument(s) but 2 were supplied",
        ],
    );
}

#[test]
fn lambda_shadow_15_16_type_and_noncallable_diagnostics_do_not_fall_through() {
    assert_compile_error_contains(
        r#"
fn f(x: bool) -> i64 { return 1; }
fn main() {
    let f = |x: i64| -> i64 { return x; };
    println(f(true));
}
"#,
        &["error[E0201]", "expected `i64`, found `bool`"],
    );
    assert_compile_error_contains(
        r#"
fn f(x: i64) -> i64 { return x + 1; }
fn main() {
    let f = 7;
    println(f(1));
}
"#,
        &["error[E0201]", "cannot call value `f` of type `i64`"],
    );
}

#[test]
fn lambda_shadow_18_defer_uses_the_local_callee() {
    let source = r#"
fn f(x: i64) { println(x + 1); }

fn deferred() {
    let f = |x: i64| { println(x + 100); };
    defer f(1);
    println(1);
}

fn main() { deferred(); }
"#;
    let (out, ok) = compile_and_run(source);
    assert!(ok, "{out}");
    assert_eq!(out, "1\n101\n");
}

#[test]
fn lambda_shadow_19_async_frame_rejects_function_value_before_codegen() {
    assert_compile_error_contains(
        r#"
fn f(x: i64) -> i64 { return x + 1; }
async fn worker() -> i64 {
    await sleep(0);
    let f = |x: i64| -> i64 { return x + 200; };
    return f(1);
}
async fn main() { println(await worker()); }
"#,
        &[
            "error[E2402]",
            "`fn(i64) -> i64` cannot move between workers",
        ],
    );
}

#[test]
fn lambda_shadow_20_21_a_shadowing_local_lambda_is_the_callee() {
    let (out, ok) = compile_and_run_with_env(
        r#"
fn f(x: i64) -> i64 { return x + 1; }
fn chosen() -> i64 {
    let f = |x: i64| -> i64 { return x + 100; };
    return f(1);
}
fn main() { println(chosen()); }
"#,
        &[],
    );
    assert!(ok, "{out}");
    assert_eq!(out, "101\n");

    // The walker emits the indirect local call itself now
    // (willow-0g8j.2.2), so this shape no longer falls back — and the result
    // proves it resolved the callee to the local slot rather than to the
    // top-level `f`, which would have printed `2`.
    let (out, ok) = compile_and_run_with_env(
        r#"
fn f(x: i64) -> i64 { return x + 1; }
fn chosen() -> i64 { let f = |x: i64| x + 100; return f(1); }
fn main() { println(chosen()); }
"#,
        &[],
    );
    assert!(ok, "{out}");
    assert_eq!(out, "101\n");
}

#[test]
fn lambda_shadow_22_release_matches_debug() {
    let (out, ok) = compile_and_run_release(SHADOW_MATRIX);
    assert!(ok, "{out}");
    assert_eq!(out, SHADOW_OUTPUT);
}

#[test]
fn lambda_shadow_24_indirect_call_keeps_panic_observation() {
    let targets = compile_and_collect_relocation_targets(
        r#"
fn f(x: i64) -> i64 { return x + 1; }
fn chosen() -> i64 {
    let f = |x: i64| -> i64 { return x + 100; };
    return f(1);
}
fn main() { println(chosen()); }
"#,
        &[],
    );
    assert!(
        targets.iter().any(|target| target == "willow_panic_depth"),
        "indirect call must remain fail-closed: {targets:?}"
    );
}
