//! Class `extends` cycles are a language error (willow-jlky → E0426).
//!
//! `open class A extends B {}` with `open class B extends A {}` used to pass
//! the checker: every hierarchy walker guards against repeats, so nothing
//! looped, but nothing spoke either. A cycle with no fields compiled to a
//! program; one with fields reached codegen and died with E0800 ("constructor
//! `A` was not lowered to operands"). Only interfaces had a cycle diagnostic
//! (E0423).
//!
//! The base chain is now walked when a class's inheritance is checked. A chain
//! that returns to the class is reported once per cycle, by the first member
//! in declaration order, with a label on every member and a note spelling the
//! whole cycle. A member's other hierarchy-derived checks (override rules,
//! static hiding, base constructor rules, interface conformance) are skipped
//! for that class -- each would judge the class against itself -- while its
//! fields and bodies are still checked. A class that only extends INTO a cycle
//! says nothing of its own and skips the same chain-derived checks, and `new`
//! of either kind skips constructor arity: the memberwise field list and the
//! inherited `init` both depend on a chain that never ends.
//!
//! Perspectives:
//!  1 self-extends                          2 two-class cycle
//!  3 three-class cycle                     4 every member is labelled
//!  5 labels point at member declarations   6 first declared member reports
//!  7 two independent cycles                8 extending into a cycle is silent
//!  9 fields in the cycle                  10 same-named methods: no E0702
//! 11 same-named statics: no E0839         12 constructors: no E0848
//! 13 `implements` on a member: deferred   14 non-open member: cycle wins
//! 15 unrelated declaration errors kept    16 `new` of a member: no cascade
//! 17 other classes still checked          18 member bodies still checked
//! 19 module-scoped cycle                  20 deep acyclic chain is clean
//! 21 shared base is clean                 22 five-member cycle
//! 23 exact message, note and help         24 member-to-member assignment
//! 25 reacher's own inheritance is silent  26 code is distinct from E0423
//! 27 `new` of a reacher: no arity error  28 `new` of a member: no arity error
//! 29 reacher with init/implements: silent 30 `new` args keep own diagnostics

use crate::diagnostics::label::LabelKind;
use crate::diagnostics::{Diagnostic, ErrorCode};

fn check(src: &str) -> Vec<Diagnostic> {
    let tokens = crate::lexer::Lexer::new(src).tokenize().expect("lex");
    let (program, parse_errors) = crate::parser::Parser::new(tokens).parse();
    assert!(parse_errors.is_empty(), "parse errors: {parse_errors:?}");
    let mut checker = crate::semantic::TypeChecker::new();
    crate::register_prelude(&mut checker).expect("prelude");
    checker.check_program(&program);
    checker.errors
}

/// Check `src` as the body of module `m`, the way `typecheck_phase` checks an
/// imported module.
fn check_module(src: &str) -> Vec<Diagnostic> {
    let tokens = crate::lexer::Lexer::new(src).tokenize().expect("lex");
    let (program, parse_errors) = crate::parser::Parser::new(tokens).parse();
    assert!(parse_errors.is_empty(), "parse errors: {parse_errors:?}");
    let mut checker = crate::semantic::TypeChecker::new();
    crate::register_prelude(&mut checker).expect("prelude");
    checker.set_module_path("m");
    checker.check_module_program(&program);
    checker.errors
}

fn cycles(diagnostics: &[Diagnostic]) -> Vec<&Diagnostic> {
    diagnostics
        .iter()
        .filter(|d| d.code == ErrorCode::E0426)
        .collect()
}

fn codes(diagnostics: &[Diagnostic]) -> Vec<ErrorCode> {
    diagnostics.iter().map(|d| d.code).collect()
}

/// The one E0426 in `src`, with nothing else reported.
#[track_caller]
fn expect_only_cycle(src: &str) -> Diagnostic {
    let found = check(src);
    assert_eq!(
        codes(&found),
        vec![ErrorCode::E0426],
        "expected exactly one E0426 and nothing else, got {found:?}"
    );
    found.into_iter().next().unwrap()
}

fn note(d: &Diagnostic) -> &str {
    d.notes
        .iter()
        .find(|n| n.starts_with("inheritance cycle: "))
        .map(String::as_str)
        .unwrap_or_else(|| panic!("no cycle note on {d:?}"))
}

// 1. The shortest cycle: a class that names itself as its base.
#[test]
fn c01_self_extends_is_a_cycle() {
    let d = expect_only_cycle("open class A extends A {} fn main() {}");
    assert_eq!(d.message, "cyclic class inheritance involving `A`");
    assert_eq!(note(&d), "inheritance cycle: A extends A");
    assert_eq!(d.labels.len(), 1, "{d:?}");
    assert_eq!(d.labels[0].kind, LabelKind::Primary);
}

// 2. The repro: two classes extending each other. Both members find the same
//    cycle; only one diagnostic comes out.
#[test]
fn c02_two_class_cycle_is_reported_once() {
    let d = expect_only_cycle(
        "open class A extends B {}\nopen class B extends A {}\nfn main() { println(1); }",
    );
    assert_eq!(d.message, "cyclic class inheritance involving `A`");
    assert_eq!(note(&d), "inheritance cycle: A extends B extends A");
}

// 3. A longer ring is one cycle: one diagnostic, the note walks the ring in
//    chain order from the reporting class.
#[test]
fn c03_three_class_cycle_note_walks_the_ring() {
    let d = expect_only_cycle(
        "open class A extends B {}\nopen class B extends C {}\nopen class C extends A {}\nfn main() {}",
    );
    assert_eq!(
        note(&d),
        "inheritance cycle: A extends B extends C extends A"
    );
}

// 4. Every member of the ring carries a label: the reporter's is primary, the
//    others secondary, so an editor marks all of them.
#[test]
fn c04_every_member_is_labelled() {
    let d = expect_only_cycle(
        "open class A extends B {}\nopen class B extends C {}\nopen class C extends A {}\nfn main() {}",
    );
    assert_eq!(d.labels.len(), 3, "{d:?}");
    assert_eq!(d.labels[0].kind, LabelKind::Primary);
    assert_eq!(
        d.labels[0].message,
        "class cannot transitively extend itself"
    );
    assert_eq!(d.labels[1].kind, LabelKind::Secondary);
    assert_eq!(d.labels[1].message, "`B` is part of the cycle");
    assert_eq!(d.labels[2].kind, LabelKind::Secondary);
    assert_eq!(d.labels[2].message, "`C` is part of the cycle");
}

// 5. The labels sit on the member declarations, one per source line here.
#[test]
fn c05_labels_point_at_member_declarations() {
    let d = expect_only_cycle(
        "open class A extends B {}\nopen class B extends C {}\nopen class C extends A {}\nfn main() {}",
    );
    let lines: Vec<usize> = d.labels.iter().map(|l| l.span.line).collect();
    assert_eq!(lines, vec![1, 2, 3], "{d:?}");
}

// 6. Which member reports is decided by declaration order, not by the
//    direction of the `extends` edges: the first class in the file owns the
//    primary label whichever way the ring runs.
#[test]
fn c06_first_declared_member_reports() {
    let d = expect_only_cycle("open class B extends A {}\nopen class A extends B {}\nfn main() {}");
    assert_eq!(d.message, "cyclic class inheritance involving `B`");
    assert_eq!(note(&d), "inheritance cycle: B extends A extends B");
    assert_eq!(d.labels[0].span.line, 1);
}

// 7. Two rings that do not touch are two diagnostics, in declaration order.
#[test]
fn c07_two_independent_cycles_are_two_diagnostics() {
    let found = check(
        "open class A extends B {}\nopen class B extends A {}\n\
         open class X extends Y {}\nopen class Y extends X {}\nfn main() {}",
    );
    assert_eq!(codes(&found), vec![ErrorCode::E0426, ErrorCode::E0426]);
    assert_eq!(found[0].message, "cyclic class inheritance involving `A`");
    assert_eq!(found[1].message, "cyclic class inheritance involving `X`");
}

// 8. A class whose chain reaches a ring without being on it gets no
//    diagnostic of its own: the ring's diagnostic already explains why the
//    chain is unusable, and the class becomes valid the moment the ring is cut.
#[test]
fn c08_extending_into_a_cycle_is_silent() {
    let d = expect_only_cycle(
        "open class A extends B {}\nopen class B extends A {}\nclass C extends A {}\nfn main() {}",
    );
    assert_eq!(d.labels.len(), 2, "C must not be labelled: {d:?}");
}

// 9. The repro that used to ICE in codegen: members with fields.
#[test]
fn c09_fields_in_the_cycle() {
    expect_only_cycle(
        "open class A extends B { pub x: i64; }\nopen class B extends A { pub y: i64; }\n\
         fn main() { let a = new A(1, 2); println(a.x); }",
    );
}

// 10. Same-named methods on two members would read as each class overriding
//     itself through the ring; the override rules are skipped for members.
#[test]
fn c10_same_named_methods_do_not_raise_override_errors() {
    expect_only_cycle(
        "open class A extends B { pub open fn name(self) -> i64 { return 1; } }\n\
         open class B extends A { pub open fn name(self) -> i64 { return 2; } }\n\
         fn main() {}",
    );
}

// 11. Same-named static members would read as each class hiding its own.
#[test]
fn c11_same_named_statics_do_not_raise_hiding_errors() {
    expect_only_cycle(
        "open class A extends B { pub static count: i64 = 0; pub static fn make() -> i64 { return 1; } }\n\
         open class B extends A { pub static count: i64 = 1; pub static fn make() -> i64 { return 2; } }\n\
         fn main() {}",
    );
}

// 12. A member's constructor is not asked to `super.init(...)` a base that is
//     ultimately itself.
#[test]
fn c12_constructors_do_not_raise_base_init_errors() {
    expect_only_cycle(
        "open class A extends B { pub x: i64; pub init(self, x: i64) { self.x = x; } }\n\
         open class B extends A { pub y: i64; pub init(self, y: i64) { self.y = y; } }\n\
         fn main() {}",
    );
}

// 13. Conformance through inherited methods is undefined on a ring, so a
//     member's `implements` is judged only once the ring is cut.
#[test]
fn c13_implements_on_a_member_is_deferred() {
    expect_only_cycle(
        "interface Named { fn name(self) -> i64; }\n\
         open class A extends B implements Named {}\n\
         open class B extends A {}\n\
         fn main() {}",
    );
}

// 14. A ring through a non-`open` class: the cycle is the root cause and the
//     only diagnostic; E0701 is not stacked on top of it.
#[test]
fn c14_non_open_member_cycle_wins_over_e0701() {
    let found = check("open class A extends B {}\nclass B extends A {}\nfn main() {}");
    assert_eq!(codes(&found), vec![ErrorCode::E0426], "{found:?}");
}

// 15. The ring does not swallow other declaration errors in the same file.
#[test]
fn c15_unrelated_declaration_errors_are_kept() {
    let found = check(
        "open class A extends B {}\nopen class B extends A {}\n\
         class C extends Missing {}\nfn main() {}",
    );
    assert_eq!(
        codes(&found),
        vec![ErrorCode::E0426, ErrorCode::E0350],
        "{found:?}"
    );
}

// 16. Constructing a member does not cascade: the memberwise constructor is
//     computed over the guarded chain, so `new A(..)` itself is not an error.
#[test]
fn c16_new_of_a_member_does_not_cascade() {
    expect_only_cycle(
        "open class A extends B { pub x: i64; }\nopen class B extends A { pub y: i64; }\n\
         fn main() { let a = new A(1, 2); let b = new B(3, 4); println(a.x + b.y); }",
    );
}

// 17. Partial recovery: an unrelated class keeps its own body diagnostics.
#[test]
fn c17_other_classes_are_still_checked() {
    let found = check(
        "open class A extends B {}\nopen class B extends A {}\n\
         class Other { pub fn go(self) { self.missing(); } }\nfn main() {}",
    );
    assert_eq!(
        codes(&found),
        vec![ErrorCode::E0426, ErrorCode::E0502],
        "{found:?}"
    );
}

// 18. Partial recovery: a member's own method bodies are still checked.
#[test]
fn c18_member_bodies_are_still_checked() {
    let found = check(
        "open class A extends B { pub fn go(self) { self.missing(); } }\n\
         open class B extends A {}\nfn main() {}",
    );
    assert_eq!(
        codes(&found),
        vec![ErrorCode::E0426, ErrorCode::E0502],
        "{found:?}"
    );
}

// 19. A module's own checker reports a ring among the module's classes.
#[test]
fn c19_module_scoped_cycle() {
    let found = check_module(
        "pub open class Circle extends Square { pub r: i64; }\n\
         pub open class Square extends Circle { pub s: i64; }",
    );
    assert_eq!(codes(&found), vec![ErrorCode::E0426], "{found:?}");
    assert_eq!(
        note(&found[0]),
        "inheritance cycle: Circle extends Square extends Circle"
    );
}

// 20. A long chain that ends at a root is not a cycle.
#[test]
fn c20_deep_acyclic_chain_is_clean() {
    let found = check(
        "open class D {}\nopen class C extends D {}\nopen class B extends C {}\n\
         class A extends B {}\nfn main() { let a = new A(); }",
    );
    assert!(found.is_empty(), "{found:?}");
}

// 21. Two classes sharing one base meet at the base, which is not a repeat on
//     either chain.
#[test]
fn c21_shared_base_is_clean() {
    let found = check(
        "open class Base {}\nclass Left extends Base {}\nclass Right extends Base {}\n\
         fn main() { let l = new Left(); let r = new Right(); }",
    );
    assert!(found.is_empty(), "{found:?}");
}

// 22. A five-member ring is still one diagnostic with five labels.
#[test]
fn c22_five_member_cycle() {
    let d = expect_only_cycle(
        "open class A extends B {}\nopen class B extends C {}\nopen class C extends D {}\n\
         open class D extends E {}\nopen class E extends A {}\nfn main() {}",
    );
    assert_eq!(d.labels.len(), 5, "{d:?}");
    assert_eq!(
        note(&d),
        "inheritance cycle: A extends B extends C extends D extends E extends A"
    );
}

// 23. The wording a user reads.
#[test]
fn c23_message_note_and_help() {
    let d = expect_only_cycle("open class A extends B {}\nopen class B extends A {}\nfn main() {}");
    assert_eq!(d.message, "cyclic class inheritance involving `A`");
    assert_eq!(d.notes, vec!["inheritance cycle: A extends B extends A"]);
    assert_eq!(
        d.helps,
        vec!["remove one `extends` so the chain ends at a class with no base"]
    );
}

// 24. Using members as values of each other's type neither loops nor panics;
//     whatever subtyping says, the ring stays the only structural error.
#[test]
fn c24_member_to_member_assignment_does_not_loop() {
    let found = check(
        "open class A extends B {}\nopen class B extends A {}\n\
         fn main() { let a: A = new A(); let b: B = a; let c: A = b; }",
    );
    assert!(cycles(&found).len() == 1, "{found:?}");
}

// 25. A class extending into a ring also skips its override checks: its
//     `override fn` cannot be matched against an unusable chain, and the ring
//     remains the one diagnostic.
#[test]
fn c25_reacher_inheritance_is_silent() {
    expect_only_cycle(
        "open class A extends B { pub open fn name(self) -> i64 { return 1; } }\n\
         open class B extends A {}\n\
         class C extends A { pub override fn name(self) -> i64 { return 3; } }\n\
         fn main() {}",
    );
}

// 26. The class code is its own; the interface ring keeps E0423.
#[test]
fn c26_class_and_interface_cycle_codes_are_distinct() {
    let found = check(
        "interface I extends J {}\ninterface J extends I {}\n\
         open class A extends B {}\nopen class B extends A {}\nfn main() {}",
    );
    assert!(
        found.iter().any(|d| d.code == ErrorCode::E0423),
        "{found:?}"
    );
    assert_eq!(cycles(&found).len(), 1, "{found:?}");
    assert_ne!(ErrorCode::E0426.as_str(), ErrorCode::E0423.as_str());
}

// 27. Constructing a class that extends into a ring: its memberwise
//     constructor would count every field on the unusable chain, so the arity
//     check is skipped rather than reporting E0845 next to the ring.
#[test]
fn c27_new_of_a_reacher_has_no_arity_error() {
    expect_only_cycle(
        "open class A extends B { pub x: i64; pub init(self, x: i64) { self.x = x; } }\n\
         open class B extends A { pub y: i64; }\n\
         class C extends A { pub z: i64; }\n\
         fn main() { let c = new C(1); println(c.z); }",
    );
}

// 28. The same for a member whose argument count does not happen to match
//     the walked field list (16 only matched by accident).
#[test]
fn c28_new_of_a_member_has_no_arity_error() {
    expect_only_cycle(
        "open class A extends B { pub x: i64; }\nopen class B extends A { pub y: i64; }\n\
         fn main() { let a = new A(1); let b = new B(); println(a.x + b.y); }",
    );
}

// 29. A reacher with its own `init`, an `implements`, a static and an
//     override: every chain-derived rule stays quiet, the ring is the one
//     diagnostic.
#[test]
fn c29_reacher_with_init_implements_static_and_override_is_silent() {
    expect_only_cycle(
        "interface Named { fn name(self) -> i64; }\n\
         open class A extends B { pub x: i64; pub static count: i64 = 0;\n\
             pub init(self, x: i64) { self.x = x; }\n\
             pub open fn name(self) -> i64 { return 1; } }\n\
         open class B extends A { pub y: i64; }\n\
         class C extends A implements Named { pub static count: i64 = 1;\n\
             pub init(self) { self.x = 0; }\n\
             pub override fn name(self) -> i64 { return 3; } }\n\
         fn main() { let c = new C(); println(c.name()); }",
    );
}

// 30. Skipping the arity check must not skip the arguments: their own errors
//     are still reported.
#[test]
fn c30_new_arguments_keep_their_own_diagnostics() {
    let found = check(
        "open class A extends B { pub x: i64; }\nopen class B extends A { pub y: i64; }\n\
         fn main() { let a = new A(missing, 2); println(a.x); }",
    );
    assert_eq!(
        codes(&found),
        vec![ErrorCode::E0426, ErrorCode::E0350],
        "{found:?}"
    );
}
