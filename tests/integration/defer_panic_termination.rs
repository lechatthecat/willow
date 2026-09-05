//! willow-s9ej.12 — a terminating deferred action must terminate its current
//! Cranelift block; no cleanup/return/jump may be appended afterwards.
//!
//! Perspectives:
//!  01 block defer + fallthrough, 02 block defer + bare return,
//!  03 expression defer + return, 04 panic first in block,
//!  05 panic in the middle, 06 panic last, 07 nested lexical block,
//!  08 value return, 09 `Result` `?` propagation, 10 loop break,
//!  11 loop continue, 12 both if arms terminate, 13 one if arm terminates,
//!  14 all match arms terminate, 15 one match arm terminates,
//!  16 multiple LIFO defers, 17 panic recovered from normal fallthrough,
//!  18 panic recovered from a return path, 19 GC reference survives cleanup,
//!  20 cooperative async return, 21 panic-effects optimizer disabled,
//!  22 release build, 23 ordinary nonterminating defer control,
//!  24 synchronous LIR recovery after an early return.

use super::support::{
    compile_and_run, compile_and_run_release, compile_and_run_with_env, compile_with_compiler_env,
};

fn assert_compiles(label: &str, source: &str, env: &[(&str, &str)]) {
    let (ok, stderr) = compile_with_compiler_env(source, env);
    assert!(ok, "{label} failed to compile:\n{stderr}");
}

#[test]
fn defer_term_01_through_16_cfg_shapes_compile_without_frontend_panic() {
    let cases = [
        (
            "01 block fallthrough",
            r#"fn f() { defer { panic("x"); } println("body"); } fn main() {}"#,
        ),
        (
            "02 block bare return",
            r#"fn f() { defer { panic("x"); } return; } fn main() {}"#,
        ),
        (
            "03 expression return",
            r#"fn f() { defer panic("x"); return; } fn main() {}"#,
        ),
        (
            "04 panic first",
            r#"fn f() { defer { panic("x"); println("dead"); } return; } fn main() {}"#,
        ),
        (
            "05 panic middle",
            r#"fn f() { defer { println("a"); panic("x"); println("dead"); } return; } fn main() {}"#,
        ),
        (
            "06 panic last",
            r#"fn f() { defer { println("a"); panic("x"); } return; } fn main() {}"#,
        ),
        (
            "07 nested block",
            r#"fn f() { if true { defer { if true { panic("x"); } } return; } } fn main() {}"#,
        ),
        (
            "08 value return",
            r#"fn f() -> i64 { defer { panic("x"); } return 42; } fn main() {}"#,
        ),
        (
            "09 try propagation",
            r#"
fn fail() -> Result<i64, String> { return Err("no"); }
fn f() -> Result<i64, String> { defer { panic("x"); } let n = fail()?; return Ok(n); }
fn main() {}
"#,
        ),
        (
            "10 break",
            r#"fn f() { while true { defer { panic("x"); } break; } } fn main() {}"#,
        ),
        (
            "11 continue",
            r#"fn f() { let mut n = 0; while n < 1 { n = n + 1; defer { panic("x"); } continue; } } fn main() {}"#,
        ),
        (
            "12 both if arms",
            r#"fn f(flag: bool) { defer { if flag { panic("a"); } else { panic("b"); } } return; } fn main() {}"#,
        ),
        (
            "13 one if arm",
            r#"fn f(flag: bool) { defer { if flag { panic("a"); } else { println("b"); } } return; } fn main() {}"#,
        ),
        (
            "14 all match arms",
            r#"fn f(n: i64) { defer { match n { 0 => panic("a"), _ => panic("b"), } } return; } fn main() {}"#,
        ),
        (
            "15 one match arm",
            r#"fn f(n: i64) { defer { match n { 0 => panic("a"), _ => println("b"), } } return; } fn main() {}"#,
        ),
        (
            "16 multiple LIFO",
            r#"fn f() { defer println("old"); defer { panic("new"); } return; } fn main() {}"#,
        ),
    ];

    for (label, source) in cases {
        assert_compiles(label, source, &[]);
    }
}

const RECOVERED_PATHS: &str = r#"
fn return_path() {
    if true {
        defer match recover() {
            Some(info) => println("return:" + info.message),
            None => {}
        }
        defer { panic("cleanup"); println("unreachable cleanup"); }
        return;
    }
    println("return-resumed");
}

fn fallthrough_path() {
    if true {
        defer match recover() {
            Some(info) => println("fallthrough:" + info.message),
            None => {}
        }
        defer { panic("cleanup"); println("unreachable cleanup"); }
        println("body");
    }
    println("continued");
}

fn main() {
    return_path();
    println("after-return");
    fallthrough_path();
}
"#;

const RECOVERED_OUTPUT: &str =
    "return:cleanup\nreturn-resumed\nafter-return\nbody\nfallthrough:cleanup\ncontinued\n";

#[test]
fn defer_term_17_18_recovery_resumes_the_owning_scope() {
    let (out, ok) = compile_and_run(RECOVERED_PATHS);
    assert!(ok, "{out}");
    assert_eq!(out, RECOVERED_OUTPUT);
}

#[test]
fn defer_term_19_gc_reference_is_live_until_the_panicking_cleanup_finishes() {
    let source = r#"
class Payload { pub text: String; }

fn exercise() {
    if true {
        let payload = new Payload("still-live");
        defer match recover() {
            Some(_) => println("recovered"),
            None => {}
        }
        defer {
            let noise = "a" + "b" + "c";
            println(noise);
            println(payload.text);
            panic("cleanup");
        }
    }
    println("after");
}

fn main() { exercise(); }
"#;
    let (out, ok) = compile_and_run_with_env(source, &[("WILLOW_GC_STRESS", "alloc")]);
    assert!(ok, "{out}");
    assert_eq!(out, "abc\nstill-live\nrecovered\nafter\n");
}

#[test]
fn defer_term_20_cooperative_return_does_not_append_ready_after_panic() {
    let source = r#"
async fn worker() -> i64 {
    await sleep(0);
    if true {
        defer match recover() {
            Some(info) => println("async:" + info.message),
            None => {}
        }
        defer { panic("cleanup"); println("unreachable cleanup"); }
        return 7;
    }
    return 8;
}

async fn main() { println(await worker()); }
"#;
    let (out, ok) = compile_and_run_with_env(
        source,
        &[("WILLOW_TASK_BUDGET", "1"), ("WILLOW_GC_STRESS", "alloc")],
    );
    assert!(ok, "{out}");
    assert_eq!(out, "async:cleanup\n8\n");
}

#[test]
fn defer_term_21_optimizer_off_keeps_the_reachability_guard() {
    assert_compiles(
        "panic effects disabled",
        r#"fn f() { defer { panic("x"); } return; } fn main() {}"#,
        &[("WILLOW_PANIC_EFFECTS", "0")],
    );
}

#[test]
fn defer_term_22_release_matches_debug() {
    let (out, ok) = compile_and_run_release(RECOVERED_PATHS);
    assert!(ok, "{out}");
    assert_eq!(out, RECOVERED_OUTPUT);
}

#[test]
fn defer_term_23_nonterminating_control_is_unchanged() {
    let (out, ok) = compile_and_run(
        r#"
fn main() {
    defer { println("cleanup"); }
    println("body");
}
"#,
    );
    assert!(ok, "{out}");
    assert_eq!(out, "body\ncleanup\n");
}

/// A synchronous scope that is left by `return` has no fallthrough
/// `LeaveDeferScope` in LIR. Recovery still has to resume at the lexical
/// continuation, not at a backend-invented block after the function body has
/// already been emitted (willow-0g8j.2.15).
#[test]
fn defer_term_24_lir_early_return_recovery_uses_the_lexical_continuation() {
    let (out, ok) = compile_and_run_with_env(RECOVERED_PATHS, &[]);
    assert!(ok, "{out}");
    assert_eq!(out, RECOVERED_OUTPUT);
}
