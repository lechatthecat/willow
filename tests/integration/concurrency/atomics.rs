use super::*;

// ── Atomic primitives AtomicI64 / AtomicBool (willow-dgwo.3) ──────────────────
//
// 20 test perspectives:
//  1. AtomicI64::new + load reads the initial value.
//  2. store then load.
//  3. add returns the PREVIOUS value and updates.
//  4. sub returns the PREVIOUS value and updates.
//  5. swap returns the PREVIOUS value and updates.
//  6. AtomicBool::new(false) + load.
//  7. AtomicBool store + load.
//  8. AtomicBool swap returns previous.
//  9. load() result is an i64 usable in arithmetic.
// 10. AtomicBool load() is a bool usable as a condition.
// 11. An atomic shared across async tasks accumulates exactly.
// 12. Atomics survive GC (they are GC-allocated cells).
// 13. Multiple atomics are independent.
// 14. Atomic passed as a function parameter works.
// 15. AtomicI64::new with wrong arg count is rejected.
// 16. AtomicI64::new with a bool arg is rejected.
// 17. AtomicBool::new with an i64 arg is rejected.
// 18. An unknown atomic method is rejected (E0806).
// 19. AtomicBool has no add/sub (E0806).
// 20. Atomics are in scope with no import (compiler-known).
#[test]
fn test_atomic_i64_basic_ops() {
    let (out, ok) = compile_and_run(
        r#"
fn main() {
    let c = AtomicI64::new(0);
    c.store(10);
    println(c.add(5));    // 10 (previous)
    println(c.load());    // 15
    println(c.sub(3));    // 15 (previous)
    println(c.load());    // 12
    println(c.swap(99));  // 12 (previous)
    println(c.load());    // 99
}
"#,
    );
    assert!(ok);
    assert_eq!(out, "10\n15\n15\n12\n12\n99\n");
}

#[test]
fn test_atomic_bool_basic_ops() {
    let (out, ok) = compile_and_run(
        r#"
fn main() {
    let f = AtomicBool::new(false);
    println(f.load());      // false
    f.store(true);
    println(f.load());      // true
    println(f.swap(false)); // true
    println(f.load());      // false
}
"#,
    );
    assert!(ok);
    assert_eq!(out, "false\ntrue\ntrue\nfalse\n");
}

#[test]
fn test_atomic_load_is_i64_in_arithmetic() {
    let (out, ok) = compile_and_run(
        r#"
fn main() {
    let c = AtomicI64::new(20);
    println(c.load() + 22);   // 42
}
"#,
    );
    assert!(ok);
    assert_eq!(out, "42\n");
}

#[test]
fn test_atomic_bool_load_is_bool_condition() {
    let (out, ok) = compile_and_run(
        r#"
fn main() {
    let f = AtomicBool::new(true);
    if f.load() { println(1); } else { println(0); }
}
"#,
    );
    assert!(ok);
    assert_eq!(out, "1\n");
}

#[test]
fn test_atomic_shared_across_async_tasks() {
    let (out, ok) = compile_and_run(
        r#"
async fn bump(c: AtomicI64, n: i64) -> i64 {
    let mut i = 0;
    while i < n { c.add(1); await sleep(1); i = i + 1; }
    return n;
}
async fn main() {
    let c = AtomicI64::new(0);
    let a = bump(c, 2);
    let b = bump(c, 5);
    await a;
    await b;
    println(c.load());   // 7
}
"#,
    );
    assert!(ok);
    assert_eq!(out, "7\n");
}

#[test]
fn test_atomic_survives_gc_stress() {
    let (out, ok) = compile_and_run_gc_stress(
        r#"
fn main() {
    let c = AtomicI64::new(1);
    let mut i = 0;
    while i < 40 {
        let junk = AtomicI64::new(i);
        c.add(1);
        i = i + 1;
    }
    println(c.load());   // 41
}
"#,
    );
    assert!(ok);
    assert_eq!(out, "41\n");
}

#[test]
fn test_atomics_independent_and_param_passing() {
    let (out, ok) = compile_and_run(
        r#"
fn add_to(a: AtomicI64, n: i64) {
    a.add(n);
}
fn main() {
    let x = AtomicI64::new(0);
    let y = AtomicI64::new(0);
    add_to(x, 3);
    add_to(y, 100);
    println(x.load());   // 3
    println(y.load());   // 100
}
"#,
    );
    assert!(ok);
    assert_eq!(out, "3\n100\n");
}

#[test]
fn test_atomic_i64_new_wrong_arg_count_rejected() {
    assert_compile_error_contains(
        "fn main() { let c = AtomicI64::new(); }\n",
        &["error[E0201]", "expects 1 argument"],
    );
}

#[test]
fn test_atomic_i64_new_bool_arg_rejected() {
    assert_compile_error_contains(
        "fn main() { let c = AtomicI64::new(true); }\n",
        &["error[E0201]", "expects `i64`"],
    );
}

#[test]
fn test_atomic_bool_new_i64_arg_rejected() {
    assert_compile_error_contains(
        "fn main() { let c = AtomicBool::new(1); }\n",
        &["error[E0201]", "expects `bool`"],
    );
}

#[test]
fn test_atomic_unknown_method_rejected() {
    assert_compile_error_contains(
        "fn main() { let c = AtomicI64::new(0); c.frobnicate(); }\n",
        &["error[E0806]", "no method `frobnicate`"],
    );
}

#[test]
fn test_atomic_bool_has_no_add() {
    assert_compile_error_contains(
        "fn main() { let f = AtomicBool::new(false); f.add(1); }\n",
        &["error[E0806]", "no method `add`"],
    );
}
