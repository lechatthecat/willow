//! Safepoint-free leaf methods and proven interface slots (willow-8hq4.14).
//!
//! A method whose body cannot panic, allocate, call or loop neither roots
//! `self` nor polls the safepoint, and an interface call whose every slot
//! target is proven no-panic and roots its own receiver skips both the
//! `willow_panic_depth` bracket and the call-site receiver root.
//!
//! Perspectives:
//!   01 a field getter adds no root and no poll per method
//!   02 arithmetic and comparisons stay leaves in release; debug overflow
//!      checks (willow-jz15.14) add a panic path, so debug keeps the root/poll
//!   03 a literal divisor other than 0/-1 stays a leaf and keeps no guard
//!   04 a parameter divisor keeps the guard and the entry root
//!   05 a literal 0 divisor still panics
//!   06 a literal -1 divisor still traps `i64::MIN / -1` as a panic
//!   07 a literal `%` divisor computes truncated remainders
//!   08 a loop keeps the poll
//!   09 a call keeps the root and the poll
//!   10 an allocation (concatenation) keeps the root
//!   11 exponentiation keeps the root
//!   12 an async method keeps `self` alive across an await
//!   13 a by-reference parameter keeps the root
//!   14 a free leaf function has no poll
//!   15 direct leaf method calls carry no panic bracket
//!   16 interface calls to all-leaf implementers carry no bracket or receiver root
//!   17 one panicking implementer keeps the interface bracket
//!   18 an `open` implementer (re-dispatching thunk) keeps bracket and root
//!   19 an allocating implementer roots `self` itself: bracket kept, no extra root
//!   20 disabling panic effects keeps the interface bracket
//!   21 leaf results stay correct while a moving minor GC runs between calls
//!   22 interface leaf results stay correct under GC stress
//!   23 field-only free functions are no-panic for their callers
//!   24 the runnable example
//!   25 bounded polymorphic direct calls (1/2/4 targets)
//!   26 over-budget sites retain indirect dispatch (5/64 targets)
//!   27 generated references scale linearly with repeated call sites
//!   28 promoted calls preserve void and floating-point return ABIs
//!   29 promoted calls evaluate reference arguments once
//!   30 a promoted panicking target preserves its failure edge
use super::support::{
    compile_and_collect_relocation_targets_mode, compile_and_run, compile_and_run_gc_stress_all,
    compile_and_run_release,
};

fn count(source: &str, env: &[(&str, &str)], release: bool, symbol: &str) -> usize {
    compile_and_collect_relocation_targets_mode(source, env, release)
        .iter()
        .filter(|name| name.as_str() == symbol)
        .count()
}

/// Per-method cost: the class declares `n` copies of a method with `body`.
fn method_delta(signature: &str, body: &str, symbol: &str) -> [usize; 2] {
    [false, true].map(|release| {
        let source = |n: usize| {
            let methods: String = (0..n)
                .map(|i| format!("pub {signature} m{i}{body}\n"))
                .collect();
            format!(
                "class C {{ pub n: i64; pub s: String;\n{methods}}}\n\
                 fn main() {{ let c = new C(1, \"s\"); println(c.n); }}"
            )
        };
        count(&source(5), &[], release, symbol) - count(&source(1), &[], release, symbol)
    })
}

const ROOT: &str = "willow_push_root";
const POLL: &str = "willow_gc_stop_flag";
const PANIC_DEPTH: &str = "willow_panic_depth";
const NO_EFFECTS: [(&str, &str); 1] = [("WILLOW_PANIC_EFFECTS", "0")];

fn polymorphic_source(targets: usize, calls: usize) -> String {
    let classes: String = (0..targets)
        .map(|i| {
            format!(
                "class C{i} implements Shape {{ pub fn area(self) -> i64 {{ return {i} + 1; }} }}\n"
            )
        })
        .collect();
    let body = "total = total + s.area();".repeat(calls);
    let uses: String = (0..targets)
        .map(|i| format!("println(run(new C{i}()));"))
        .collect();
    format!(
        "interface Shape {{ fn area(self) -> i64; }} {classes}\nfn run(s: Shape) -> i64 {{ let mut total = 0; {body} return total; }} fn main() {{ {uses} }}"
    )
}

#[test]
fn bounded_interface_promotion_preserves_every_target() {
    for n in [1, 2, 4, 5, 64] {
        let (out, ok) = compile_and_run_release(&polymorphic_source(n, 3));
        assert!(ok, "targets={n}: {out}");
        let expected: String = (1..=n).map(|i| format!("{}\n", i * 3)).collect();
        assert_eq!(out, expected, "targets={n}");
    }
}

#[test]
fn interface_promotion_code_growth_is_bounded_and_linear() {
    for n in [1, 2, 4, 5, 64] {
        let refs =
            [1, 2, 8].map(|calls| count(&polymorphic_source(n, calls), &[], true, "C0.area"));
        let per_site = refs[1] - refs[0];
        assert_eq!(refs[2] - refs[0], 7 * per_site, "targets={n}");
        if n <= 4 {
            assert!(per_site > 0, "missing direct target for {n} targets");
        } else {
            assert_eq!(per_site, 0, "unbounded promotion for {n} targets");
        }
    }
}

#[test]
fn promoted_interface_void_float_and_reference_abis() {
    let source = r#"
interface Work { fn update(self, n: &mut i64); fn value(self, x: f64) -> f64; }
class A implements Work {
    pub fn update(self, n: &mut i64) { n = n + 2; }
    pub fn value(self, x: f64) -> f64 { return x + 0.5; }
}
class B implements Work {
    pub fn update(self, n: &mut i64) { n = n + 3; }
    pub fn value(self, x: f64) -> f64 { return x * 2.0; }
}
fn run(w: Work) { let mut n = 10; w.update(&n); println(n); println(w.value(1.5)); }
fn main() { run(new A()); run(new B()); }
"#;
    let (out, ok) = compile_and_run_release(source);
    assert!(ok, "{out}");
    assert_eq!(out, "12\n2\n13\n3\n");
}

#[test]
fn promoted_interface_panic_rejects_neutral_result() {
    let source = r#"
interface Shape { fn area(self) -> i64; }
class Good implements Shape { pub fn area(self) -> i64 { return 7; } }
class Bad implements Shape { pub fn area(self) -> i64 { panic("promoted"); return 0; } }
fn run(s: Shape) { println(s.area()); println("unreachable for Bad"); }
fn main() { run(new Good()); run(new Bad()); println("unreachable"); }
"#;
    let (out, ok) = compile_and_run_release(source);
    assert!(!ok, "{out}");
    assert_eq!(out, "7\nunreachable for Bad\n");
}

#[test]
fn field_getter_adds_no_root_or_poll() {
    let sig = "fn";
    let body = "(self) -> i64 { return self.n; }";
    assert_eq!(method_delta(sig, body, ROOT), [0, 0]);
    assert_eq!(method_delta(sig, body, POLL), [0, 0]);
}

#[test]
fn arithmetic_and_comparison_stay_leaves() {
    let body = "(self, k: i64) -> bool { return (self.n * k + 3 - k) > (self.n * 2); }";
    // `[debug, release]`: checked debug arithmetic can panic, so the method
    // is not a leaf there (willow-jz15.14); release wraps and stays a leaf.
    for symbol in [ROOT, POLL] {
        let [debug, release] = method_delta("fn", body, symbol);
        assert_eq!(release, 0, "{symbol}");
        assert!(debug >= 4, "{symbol}: debug={debug}");
    }
    let compare = "(self, k: i64) -> bool { return self.n > k; }";
    assert_eq!(method_delta("fn", compare, ROOT), [0, 0]);
    assert_eq!(method_delta("fn", compare, POLL), [0, 0]);
}

#[test]
fn literal_divisor_stays_a_leaf() {
    let body = "(self) -> bool { return self.n / 7 > self.n % 3; }";
    assert_eq!(method_delta("fn", body, ROOT), [0, 0]);
    assert_eq!(method_delta("fn", body, POLL), [0, 0]);
}

#[test]
fn parameter_divisor_keeps_guard_root() {
    let body = "(self, d: i64) -> i64 { return self.n / d; }";
    assert!(method_delta("fn", body, ROOT).iter().all(|&d| d >= 4));
}

#[test]
fn loop_keeps_poll() {
    let body = "(self) -> i64 { let mut i = 0; while i < self.n { i = i + 1; } return i; }";
    assert!(method_delta("fn", body, POLL).iter().all(|&d| d >= 4));
}

#[test]
fn call_keeps_root_and_poll() {
    let body = "(self) -> i64 { return helper(self.n); }";
    let with_helper = |symbol| {
        [false, true].map(|release| {
            let source = |n: usize| {
                let methods: String = (0..n).map(|i| format!("pub fn m{i}{body}\n")).collect();
                format!(
                    "fn helper(x: i64) -> i64 {{ return x + 1; }}\n\
                     class C {{ pub n: i64;\n{methods}}}\n\
                     fn main() {{ let c = new C(1); println(c.m0()); }}"
                )
            };
            count(&source(5), &[], release, symbol) - count(&source(1), &[], release, symbol)
        })
    };
    // The callee polls at its own entry, so only the receiver root is pinned.
    assert!(with_helper(ROOT).iter().all(|&d| d >= 4));
}

#[test]
fn allocation_keeps_root() {
    let body = "(self) -> String { return self.s + \"!\"; }";
    assert!(method_delta("fn", body, ROOT).iter().all(|&d| d >= 4));
}

#[test]
fn exponentiation_keeps_root() {
    let body = "(self) -> i64 { return self.n ** 2; }";
    assert!(method_delta("fn", body, ROOT).iter().all(|&d| d >= 4));
}

#[test]
fn async_method_keeps_receiver_across_await() {
    // An async method is never a leaf: its frame stores `self` past awaits.
    let source = "class C { pub n: i64; pub s: String;\n\
         pub fn get(self) -> i64 { return self.n; }\n\
         pub async fn later(self) -> String { await sleep(1); gc_collect(); return self.s + self.get().toString(); } }\n\
         async fn main() { let c = new C(4, \"n=\"); gc_collect(); println(await c.later()); println(c.get()); }";
    let (out, ok) = compile_and_run_gc_stress_all(source);
    assert!(ok, "{out}");
    assert_eq!(out, "n=4\n4\n");
}

#[test]
fn reference_parameter_keeps_root() {
    let body = "(self, out: &mut i64) { out = self.n; }";
    assert!(method_delta("fn", body, ROOT).iter().all(|&d| d >= 4));
}

#[test]
fn free_leaf_function_has_no_poll() {
    for release in [false, true] {
        let source = |n: usize| {
            let fns: String = (0..n)
                .map(|i| {
                    format!(
                        "fn f{i}(a: i64, b: i64) -> bool {{ return a / {} < b; }}\n",
                        i + 2
                    )
                })
                .collect();
            format!("{fns}fn main() {{ println(f0(2, 3)); }}")
        };
        assert_eq!(
            count(&source(5), &[], release, POLL),
            count(&source(1), &[], release, POLL),
            "release={release}"
        );
    }
}

#[test]
fn direct_leaf_calls_have_no_panic_bracket() {
    for release in [false, true] {
        let source = |calls: usize| {
            // `wrapping_add` keeps the caller free of debug overflow checks.
            let body = "total = total.wrapping_add(c.get());".repeat(calls);
            format!(
                "class C {{ pub n: i64; pub fn get(self) -> i64 {{ return self.n; }} }}\n\
                 fn main() {{ let c = new C(1); let mut total = 0; {body} println(total); }}"
            )
        };
        assert_eq!(
            count(&source(8), &[], release, PANIC_DEPTH),
            count(&source(1), &[], release, PANIC_DEPTH),
            "release={release}"
        );
    }
}

/// `calls` interface calls of `Shape.area` with the given implementers. The
/// caller sums with `wrapping_add` and the leaves avoid `+ - *`, so debug
/// overflow checks (willow-jz15.14) do not change either side.
fn interface_source(implementers: &str, calls: usize) -> String {
    let body = "total = total.wrapping_add(s.area());".repeat(calls);
    format!(
        "interface Shape {{ fn area(self) -> i64; }}\n{implementers}\n\
         fn run(s: Shape) -> i64 {{ let mut total = 0; {body} return total; }}\n\
         fn main() {{ println(run(new Square(2))); }}"
    )
}

fn interface_delta(implementers: &str, env: &[(&str, &str)], symbol: &str) -> [usize; 2] {
    [false, true].map(|release| {
        count(&interface_source(implementers, 8), env, release, symbol)
            - count(&interface_source(implementers, 1), env, release, symbol)
    })
}

const LEAVES: &str = "class Square implements Shape { pub side: i64; pub fn area(self) -> i64 { return self.side; } }\n\
     class Line implements Shape { pub len: i64; pub fn area(self) -> i64 { return self.len / 2; } }";

#[test]
fn interface_leaf_calls_skip_bracket_and_receiver_root() {
    // Cold nil-receiver paths reference the depth too, so measure the call
    // bracket against the same source with panic effects disabled.
    let leaf = interface_delta(LEAVES, &[], PANIC_DEPTH);
    let disabled = interface_delta(LEAVES, &NO_EFFECTS, PANIC_DEPTH);
    for (leaf, disabled) in leaf.iter().zip(disabled) {
        assert!(leaf + 7 <= disabled, "leaf={leaf} disabled={disabled}");
    }
    // Each extra call still roots its lowered operand snapshots; compare
    // against the same call with an implementer that needs the receiver root.
    let leaf_roots = interface_delta(LEAVES, &[], ROOT);
    let open = "open class Square implements Shape { pub side: i64; pub open fn area(self) -> i64 { return self.side; } }";
    let open_roots = interface_delta(open, &[], ROOT);
    for (leaf, open) in leaf_roots.iter().zip(open_roots) {
        assert_eq!(leaf + 7, open, "leaf={leaf_roots:?} open={open_roots:?}");
    }
}

#[test]
fn panicking_implementer_keeps_bracket() {
    let implementers = "class Square implements Shape { pub side: i64; pub fn area(self) -> i64 { return self.side; } }\n\
         class Bad implements Shape { pub fn area(self) -> i64 { panic(\"bad\"); return 0; } }";
    assert_eq!(
        interface_delta(implementers, &[], PANIC_DEPTH),
        interface_delta(implementers, &NO_EFFECTS, PANIC_DEPTH)
    );
}

#[test]
fn open_implementer_keeps_bracket() {
    let implementers = "open class Square implements Shape { pub side: i64; pub open fn area(self) -> i64 { return self.side; } }\n\
         class Cube extends Square { pub override fn area(self) -> i64 { return 6; } }";
    assert_eq!(
        interface_delta(implementers, &[], PANIC_DEPTH),
        interface_delta(implementers, &NO_EFFECTS, PANIC_DEPTH)
    );
}

#[test]
fn allocating_implementer_keeps_bracket_but_roots_itself() {
    let implementers = "class Square implements Shape { pub side: i64; pub fn area(self) -> i64 { println(self.side.toString() + \"!\"); return self.side; } }";
    assert_eq!(
        interface_delta(implementers, &[], PANIC_DEPTH),
        interface_delta(implementers, &NO_EFFECTS, PANIC_DEPTH)
    );
    assert_eq!(
        interface_delta(implementers, &[], ROOT),
        interface_delta(LEAVES, &[], ROOT)
    );
}

#[test]
fn disabled_panic_effects_keep_interface_bracket() {
    // The disabled build brackets every call: one more call adds at least
    // one depth read per call beyond the cold nil paths the leaf build has.
    let disabled = interface_delta(LEAVES, &NO_EFFECTS, PANIC_DEPTH);
    let leaf = interface_delta(LEAVES, &[], PANIC_DEPTH);
    assert!(disabled.iter().zip(leaf).all(|(&d, l)| d >= l + 7));
}

#[test]
fn literal_zero_divisor_still_panics() {
    let source = "class C { pub n: i64; pub fn bad(self) -> i64 { return self.n / 0; } }\n\
         fn main() { let c = new C(1); println(c.bad()); }";
    for (out, ok) in [compile_and_run(source), compile_and_run_release(source)] {
        assert!(!ok, "{out}");
        assert!(out.is_empty(), "{out}");
    }
}

#[test]
fn literal_minus_one_divisor_still_panics_on_overflow() {
    let source = "class C { pub n: i64; pub fn neg(self) -> i64 { return self.n / -1; } }\n\
         fn main() { println(new C(5).neg()); let c = new C(-9223372036854775807 - 1); println(c.neg()); }";
    for (out, ok) in [compile_and_run(source), compile_and_run_release(source)] {
        assert!(!ok, "{out}");
        assert_eq!(out, "-5\n");
    }
}

#[test]
fn literal_remainder_truncates() {
    let source = "class C { pub n: i64; pub fn r(self) -> i64 { return self.n % 3; } pub fn q(self) -> i64 { return self.n / 4; } }\n\
         fn main() { for n in [7, -7, 0] { let c = new C(n); println(c.r()); println(c.q()); } }";
    for (out, ok) in [compile_and_run(source), compile_and_run_release(source)] {
        assert!(ok, "{out}");
        assert_eq!(out, "1\n1\n-1\n-1\n0\n0\n");
    }
}

#[test]
fn leaf_methods_survive_moving_gc() {
    let source = r#"
import std::collections::Array;
class P { pub x: i64; pub y: i64; pub name: String;
    pub fn sum(self) -> i64 { return self.x + self.y; }
    pub fn half(self) -> i64 { return self.x / 2; }
}
fn main() {
    let mut ps: Array<P> = [];
    let mut i = 0;
    while i < 50 { ps.push(new P(i, i * 2, "p" + i.toString())); i = i + 1; }
    let mut total = 0;
    for p in ps { gc_collect(); total = total + p.sum() + p.half(); println(p.name); }
    println(total);
}
"#;
    let (out, ok) = compile_and_run_gc_stress_all(source);
    assert!(ok, "{out}");
    let lines: Vec<&str> = out.lines().collect();
    assert_eq!(lines.len(), 51);
    assert_eq!(lines[49], "p49");
    let expected: i64 = (0..50).map(|i| 3 * i + i / 2).sum();
    assert_eq!(lines[50], expected.to_string());
}

#[test]
fn interface_leaf_calls_survive_gc_stress() {
    let source = format!(
        "import std::collections::Array;\ninterface Shape {{ fn area(self) -> i64; }}\n{LEAVES}\n\
         fn main() {{\n\
             let mut shapes: Array<Shape> = [];\n\
             let mut i = 0;\n\
             while i < 40 {{ if i % 2 == 0 {{ shapes.push(new Square(i)); }} else {{ shapes.push(new Line(i)); }} i = i + 1; }}\n\
             let mut total = 0;\n\
             for s in shapes {{ gc_collect(); total = total + s.area(); }}\n\
             println(total);\n\
         }}"
    );
    let (out, ok) = compile_and_run_gc_stress_all(&source);
    assert!(ok, "{out}");
    let expected: i64 = (0..40)
        .map(|i: i64| if i % 2 == 0 { i } else { i / 2 })
        .sum();
    assert_eq!(out, format!("{expected}\n"));
}

#[test]
fn field_only_function_is_no_panic_for_callers() {
    for release in [false, true] {
        let source = "class C { pub n: i64; }\n\
             fn read(c: C) -> i64 { return c.n; }\n\
             fn twice(c: C) -> i64 { return read(c).wrapping_add(read(c)); }\n\
             fn main() { println(twice(new C(4))); }";
        assert_eq!(
            count(source, &[], release, PANIC_DEPTH),
            0,
            "release={release}"
        );
    }
}

#[test]
fn runnable_example() {
    let source = include_str!("../../example/leaf_method_calls.wi");
    for (out, ok) in [
        compile_and_run_gc_stress_all(source),
        compile_and_run_release(source),
    ] {
        assert!(ok, "{out}");
        assert_eq!(out, "385\n1210\n7\n");
    }
}

#[test]
fn implicit_interface_return_keeps_poll_and_receiver_root() {
    for release in [false, true] {
        let source = |n| {
            let methods: String = (0..n)
                .map(|i| format!("pub fn boxed{i}(self) -> Shape {{ return self; }}\n"))
                .collect();
            format!(
                "interface Shape {{ fn area(self) -> i64; }} class C implements Shape {{ pub n: i64; pub fn area(self) -> i64 {{ return self.n; }} {methods} }} fn main() {{ println(new C(7).boxed0().area()); }}"
            )
        };
        assert!(count(&source(5), &[], release, POLL) >= count(&source(1), &[], release, POLL) + 4);
    }
}

#[test]
fn implicit_interface_local_keeps_poll_and_receiver_root() {
    for release in [false, true] {
        let source = |n| {
            let methods: String = (0..n)
                .map(|i| {
                    format!("pub fn boxed{i}(self) -> Shape {{ let s: Shape = self; return s; }}\n")
                })
                .collect();
            format!(
                "interface Shape {{ fn area(self) -> i64; }} class C implements Shape {{ pub n: i64; pub fn area(self) -> i64 {{ return self.n; }} {methods} }} fn main() {{ println(new C(7).boxed0().area()); }}"
            )
        };
        assert!(count(&source(5), &[], release, POLL) >= count(&source(1), &[], release, POLL) + 4);
        let (out, ok) = compile_and_run_gc_stress_all(&source(1));
        assert!(ok, "{out}");
        assert_eq!(out, "7\n");
    }
}

#[test]
fn release_receiver_snapshot_survives_allocating_and_mutating_arguments() {
    let source = r#"
interface View { fn get(self, n: i64) -> String; }
class Item implements View {
    pub s: String;
    pub fn get(self, n: i64) -> String { gc_collect(); return self.s + n.toString(); }
    pub fn same(self) -> Item { gc_collect(); return self; }
}
fn replace(slot: &mut Item) -> i64 { slot = new Item("new"); gc_collect(); return 7; }
fn allocate() -> i64 { gc_collect(); return 8; }
fn main() {
    let mut x = new Item("old");
    println(x.get(replace(&x)));
    println(x.same().get(allocate()));
    let v: View = new Item("interface");
    println(v.get(allocate()));
    let xs = [new Item("array")];
    println(xs[0].same().get(allocate()));
}
"#;
    for (out, ok) in [
        compile_and_run_release(source),
        compile_and_run_gc_stress_all(source),
    ] {
        assert!(ok, "{out}");
        assert_eq!(out, "old7\nnew8\ninterface8\narray8\n");
    }
}

#[test]
fn leaf_method_and_call_counts_scale_without_runtime_overhead() {
    use super::support::compile_and_collect_relocation_targets_mode as relocations;
    for n in [1, 8, 64] {
        let methods: String = (0..n).map(|i| format!("pub fn m{i}(self, k: i64) -> i64 {{ if k > 0 {{ return self.n * k; }} return self.n; }}\n")).collect();
        let calls: String = (0..n)
            .map(|i| format!("sum = sum + c.m{i}(1);\n"))
            .collect();
        let source = format!(
            "class C {{ pub n: i64; {methods} }} fn main() {{ let c = new C(2); let mut sum = 0; {calls} println(sum); }}"
        );
        let names = relocations(&source, &[], true);
        let counts = [ROOT, POLL, PANIC_DEPTH]
            .map(|symbol| names.iter().filter(|name| name.as_str() == symbol).count());
        assert_eq!(counts, [2 * n + 2, 1, 0], "methods/calls={n}");
    }
}
