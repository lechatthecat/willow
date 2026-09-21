use super::*;

// ── Nested-place field assignment (willow-qzxg) ──────────────────────────────
// 10 runtime perspectives completing the 20 with the parser tests: 11 two-level
// write, 12 three-level write, 13 array-element field write, 14 call-result
// field write (mutates the returned object), 15 write then read back through
// the same path, 16 nested write inside a loop, 17 nested write in a method
// body via self, 18 mixed with one-level writes, 19 optional intermediate is
// rejected before a nested write, 20 checker still rejects a private field.

#[test]
fn nestassign_11_two_level_write() {
    let (out, ok) = compile_and_run(
        "class B { pub v: i64; } class A { pub b: B; }\nfn main() { let a = new A(new B(1)); a.b.v = 2; println(a.b.v); }",
    );
    assert!(ok, "{out}");
    assert_eq!(out, "2\n");
}

#[test]
fn nestassign_12_three_level_write() {
    let (out, ok) = compile_and_run(
        "class C { pub v: i64; } class B { pub c: C; } class A { pub b: B; }\nfn main() { let a = new A(new B(new C(1))); a.b.c.v = 9; println(a.b.c.v); }",
    );
    assert!(ok, "{out}");
    assert_eq!(out, "9\n");
}

#[test]
fn nestassign_13_array_element_field_write() {
    let (out, ok) = compile_and_run(
        "class P { pub x: i64; }\nfn main() { let ps = [new P(1), new P(2)]; ps[1].x = 7; println(ps[0].x); println(ps[1].x); }",
    );
    assert!(ok, "{out}");
    assert_eq!(out, "1\n7\n");
}

#[test]
fn nestassign_14_call_result_field_write() {
    let (out, ok) = compile_and_run(
        "class P { pub x: i64; }\nfn pick(p: P) -> P { return p; }\nfn main() { let p = new P(1); pick(p).x = 5; println(p.x); }",
    );
    assert!(ok, "{out}");
    assert_eq!(out, "5\n");
}

#[test]
fn nestassign_15_write_then_read_same_path() {
    let (out, ok) = compile_and_run(
        "class B { pub v: i64; } class A { pub b: B; }\nfn main() { let a = new A(new B(0)); a.b.v = 3; a.b.v = a.b.v + 4; println(a.b.v); }",
    );
    assert!(ok, "{out}");
    assert_eq!(out, "7\n");
}

#[test]
fn nestassign_16_write_inside_loop() {
    let (out, ok) = compile_and_run(
        "class B { pub v: i64; } class A { pub b: B; }\nfn main() { let a = new A(new B(0)); for i in 0..4 { a.b.v = a.b.v + i; } println(a.b.v); }",
    );
    assert!(ok, "{out}");
    assert_eq!(out, "6\n");
}

#[test]
fn nestassign_17_write_via_self_in_method() {
    let (out, ok) = compile_and_run(
        "class B { pub v: i64; } class A { pub b: B; pub fn set(self, n: i64) { self.b.v = n; } }\nfn main() { let a = new A(new B(1)); a.set(42); println(a.b.v); }",
    );
    assert!(ok, "{out}");
    assert_eq!(out, "42\n");
}

#[test]
fn nestassign_18_mixed_with_one_level() {
    let (out, ok) = compile_and_run(
        "class B { pub v: i64; } class A { pub b: B; pub n: i64; }\nfn main() { let a = new A(new B(1), 10); a.n = 20; a.b.v = 30; println(a.n + a.b.v); }",
    );
    assert!(ok, "{out}");
    assert_eq!(out, "50\n");
}

#[test]
fn nestassign_19_nil_intermediate_rejected_by_checker() {
    // An Option intermediate must be explicitly opened before nested access.
    let (ok, stderr) = compile_with_compiler_env(
        "class B { pub v: i64; } class A { pub b: Option<B>; }\nfn main() { let a = new A(None); a.b.v = 2; }",
        &[],
    );
    assert!(!ok, "nullable intermediate must be rejected");
    assert!(
        stderr.contains("E0201") && stderr.contains("handling absence"),
        "{stderr}"
    );
}

#[test]
fn nestassign_20_private_field_still_rejected() {
    let (ok, stderr) = compile_with_compiler_env(
        "class B { v: i64; } class A { pub b: B; }\nfn main() { let a = new A(new B(1)); a.b.v = 2; }",
        &[],
    );
    assert!(!ok, "private nested field write must be rejected");
    assert!(!stderr.is_empty());
}
