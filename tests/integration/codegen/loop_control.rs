use super::*;

// ── break / continue (willow-kzka) ──────────────────────────────────────────
// 20 perspectives: 1 break in while, 2 break in range-for, 3 break in
// array-for, 4 continue in while (cond re-evaluated), 5 continue in
// range-for STILL INCREMENTS, 6 continue in array-for still advances,
// 7 nested loops: break exits inner only, 8 nested loops: continue targets
// inner, 9 break outside loop = E0904, 10 continue outside loop = E0904,
// 11 break inside lambda body does not see enclosing loop = E0904,
// 12 break under if/else, 13 break inside match arm inside loop, 14 `while
// true` terminated only by break, 15 break on first iteration (body once),
// 16 GC-managed temps + break (root balance under stress), 17 async
// range-for break+continue across awaits, 18 async while break across
// awaits, 19 three-level nesting inner break/continue, 20 mixed
// break+return in the same loop body.

#[test]
fn brk_01_while() {
    let (out, ok) = compile_and_run(
        "fn main() { let mut n = 0; while n < 100 { n = n + 1; if n == 5 { break; } } println(n); }",
    );
    assert!(ok, "{out}");
    assert_eq!(out, "5\n");
}

#[test]
fn brk_02_range_for() {
    let (out, ok) = compile_and_run(
        "fn main() { let mut s = 0; for i in 0..100 { if i == 4 { break; } s = s + i; } println(s); }",
    );
    assert!(ok, "{out}");
    assert_eq!(out, "6\n");
}

#[test]
fn brk_03_array_for() {
    let (out, ok) = compile_and_run(
        "import std::collections::Array;\nfn main() { let xs: Array<i64> = [1, 2, 3, 4]; let mut s = 0; for x in xs { if x == 3 { break; } s = s + x; } println(s); }",
    );
    assert!(ok, "{out}");
    assert_eq!(out, "3\n");
}

#[test]
fn brk_04_continue_while() {
    let (out, ok) = compile_and_run(
        "fn main() { let mut n = 0; let mut s = 0; while n < 6 { n = n + 1; if n == 3 { continue; } s = s + n; } println(s); }",
    );
    assert!(ok, "{out}");
    assert_eq!(out, "18\n");
}

#[test]
fn brk_05_continue_range_for_increments() {
    // Skipping i==2 must still advance the induction variable (no hang).
    let (out, ok) = compile_and_run(
        "fn main() { let mut s = 0; for i in 0..5 { if i == 2 { continue; } s = s + i; } println(s); }",
    );
    assert!(ok, "{out}");
    assert_eq!(out, "8\n");
}

#[test]
fn brk_06_continue_array_for_advances() {
    let (out, ok) = compile_and_run(
        "import std::collections::Array;\nfn main() { let xs: Array<i64> = [1, 2, 3, 4]; let mut s = 0; for x in xs { if x == 2 { continue; } s = s + x; } println(s); }",
    );
    assert!(ok, "{out}");
    assert_eq!(out, "8\n");
}

#[test]
fn brk_07_nested_break_inner_only() {
    let (out, ok) = compile_and_run(
        "fn main() { let mut c = 0; for i in 0..3 { for j in 0..10 { if j == 2 { break; } c = c + 1; } } println(c); }",
    );
    assert!(ok, "{out}");
    assert_eq!(out, "6\n");
}

#[test]
fn brk_08_nested_continue_inner() {
    let (out, ok) = compile_and_run(
        "fn main() { let mut c = 0; for i in 0..3 { for j in 0..4 { if j == 1 { continue; } c = c + 1; } } println(c); }",
    );
    assert!(ok, "{out}");
    assert_eq!(out, "9\n");
}

#[test]
fn brk_09_break_outside_loop_rejected() {
    let (ok, stderr) = compile_with_compiler_env("fn main() { break; }", &[]);
    assert!(!ok);
    assert!(stderr.contains("E0904"), "{stderr}");
}

#[test]
fn brk_10_continue_outside_loop_rejected() {
    let (ok, stderr) = compile_with_compiler_env("fn main() { if true { continue; } }", &[]);
    assert!(!ok);
    assert!(stderr.contains("E0904"), "{stderr}");
}

#[test]
fn brk_11_lambda_is_a_loop_boundary() {
    let (ok, stderr) = compile_with_compiler_env(
        "fn main() { for i in 0..3 { let f = || { break; 1 }; } }",
        &[],
    );
    assert!(!ok);
    assert!(stderr.contains("E0904"), "{stderr}");
}

#[test]
fn brk_12_under_if_else() {
    let (out, ok) = compile_and_run(
        "fn main() { let mut n = 0; while true { if n > 3 { break; } else { n = n + 1; } } println(n); }",
    );
    assert!(ok, "{out}");
    assert_eq!(out, "4\n");
}

#[test]
fn brk_13_inside_match_arm() {
    let (out, ok) = compile_and_run(
        "enum Sig { Go, Stop, }\nfn main() { let mut n = 0; for i in 0..10 { let s = i < 3 ? Sig::Go : Sig::Stop; match s { Go => { n = n + 1; } Stop => { break; } } } println(n); }",
    );
    assert!(ok, "{out}");
    assert_eq!(out, "3\n");
}

#[test]
fn brk_14_while_true_break_only_exit() {
    let (out, ok) = compile_and_run(
        "fn main() { let mut n = 1; while true { n = n * 2; if n > 50 { break; } } println(n); }",
    );
    assert!(ok, "{out}");
    assert_eq!(out, "64\n");
}

#[test]
fn brk_15_first_iteration() {
    let (out, ok) = compile_and_run(
        "fn main() { let mut c = 0; for i in 0..100 { c = c + 1; break; } println(c); }",
    );
    assert!(ok, "{out}");
    assert_eq!(out, "1\n");
}

#[test]
fn brk_16_gc_roots_balanced_on_break() {
    let (out, ok) = compile_and_run_gc_stress(
        "import std::collections::Array;\nfn main() { let xs: Array<String> = [\"a\", \"b\", \"c\", \"d\"]; let mut n = 0; for s in xs { let t = s + \"!\"; println(t); n = n + 1; if n >= 2 { break; } } println(\"end\" + \"!\"); }",
    );
    assert!(ok, "{out}");
    assert_eq!(out, "a!\nb!\nend!\n");
}

#[test]
fn brk_17_async_range_for() {
    let (out, ok) = compile_and_run(
        "async fn main() { let mut n = 0; for i in 0..10 { await sleep(1); if i == 2 { continue; } if i == 5 { break; } n = n + i; } println(n); }",
    );
    assert!(ok, "{out}");
    assert_eq!(out, "8\n");
}

#[test]
fn brk_18_async_while() {
    let (out, ok) = compile_and_run(
        "async fn main() { let mut m = 0; while m < 100 { await sleep(1); m = m + 1; if m == 4 { break; } } println(m); }",
    );
    assert!(ok, "{out}");
    assert_eq!(out, "4\n");
}

#[test]
fn brk_19_three_level_nesting() {
    let (out, ok) = compile_and_run(
        "fn main() { let mut c = 0; for a in 0..2 { for b in 0..3 { for d in 0..10 { if d == 1 { break; } if b == 1 { continue; } c = c + 1; } } } println(c); }",
    );
    assert!(ok, "{out}");
    assert_eq!(out, "4\n");
}

#[test]
fn brk_20_break_and_return_same_loop() {
    let (out, ok) = compile_and_run(
        "fn f(stop_early: bool) -> i64 { for i in 0..10 { if stop_early { return 100; } if i == 3 { break; } } return 1; }\nfn main() { println(f(true)); println(f(false)); }",
    );
    assert!(ok, "{out}");
    assert_eq!(out, "100\n1\n");
}
