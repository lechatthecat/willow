//! A class `extends` cycle is rejected by `willow` with E0426 (willow-jlky).
//!
//! Before the checker owned this, `open class A extends B {}` /
//! `open class B extends A {}` compiled to a program, and the same ring with
//! fields reached codegen and aborted with E0800 ("constructor `A` was not
//! lowered to operands"). The focused checker tests live in
//! `src/semantic/type_checker/class_cycle_tests.rs`; these drive the compiler
//! binary end to end, including the module and import shapes a unit test
//! cannot reach.
//!
//! Perspectives:
//!   01 the silent repro is now a compile error
//!   02 the ICE repro is a compile error, not E0800
//!   03 the rendered diagnostic carries both labels, the note and the help
//!   04 the ring is reported once
//!   05 a ring inside an imported module is reported against the module file
//!   06 an entry class extending into a module ring does not ICE
//!   07 a local class named like an imported alias's target is not a ring
//!   08 an acyclic chain through an imported base still compiles and runs
//!   09 the example program that documents the rule compiles and runs
//!   10 `new` of a class extending into a ring adds no E0845 next to E0426

use super::support::{
    assert_compile_error_contains, compile_error_stderr, compile_file_and_run,
    compile_temp_project_and_run, compile_temp_project_error_stderr,
};

const RING: &str =
    "open class A extends B { }\nopen class B extends A { }\nfn main() { println(1); }\n";

const RING_WITH_FIELDS: &str = "open class A extends B { pub x: i64; }\n\
open class B extends A { pub y: i64; }\n\
fn main() { let a = new A(1, 2); println(a.x); }\n";

// 01. Repro 1 from the ticket used to print `compiled [debug]` and exit 0.
#[test]
fn class_cycle_01_silent_ring_is_rejected() {
    assert_compile_error_contains(RING, &["error[E0426]"]);
}

// 02. Repro 2 used to die in codegen; the checker now stops it first.
#[test]
fn class_cycle_02_ring_with_fields_is_e0426_not_an_ice() {
    let stderr = compile_error_stderr(RING_WITH_FIELDS);
    assert!(stderr.contains("error[E0426]"), "{stderr}");
    assert!(
        !stderr.contains("E0800"),
        "must not reach codegen:\n{stderr}"
    );
    assert!(
        !stderr.contains("internal compiler error"),
        "must not reach codegen:\n{stderr}"
    );
}

// 03. What the user reads: both members labelled, the ring spelled out, and a
//     way out.
#[test]
fn class_cycle_03_rendered_diagnostic_is_complete() {
    assert_compile_error_contains(
        RING,
        &[
            "error[E0426]: cyclic class inheritance involving `A`",
            "class cannot transitively extend itself",
            "`B` is part of the cycle",
            "note: inheritance cycle: A extends B extends A",
            "help: remove one `extends` so the chain ends at a class with no base",
        ],
    );
}

// 04. Both members discover the same ring; only one diagnostic is printed.
#[test]
fn class_cycle_04_ring_is_reported_once() {
    let stderr = compile_error_stderr(RING);
    assert_eq!(stderr.matches("error[E0426]").count(), 1, "{stderr}");
    assert!(stderr.contains("aborting due to 1 error"), "{stderr}");
}

// 05. A module's own checker reports its ring, at the module file.
#[test]
fn class_cycle_05_module_ring_is_reported_against_the_module() {
    let shapes = "module shapes;\n\
pub open class Circle extends Square { pub r: i64; }\n\
pub open class Square extends Circle { pub s: i64; }\n";
    let main = "import shapes;\nfn main() { println(1); }\n";
    let stderr =
        compile_temp_project_error_stderr(&[("shapes.wi", shapes), ("main.wi", main)], "main.wi");
    assert!(stderr.contains("error[E0426]"), "{stderr}");
    assert!(stderr.contains("shapes.wi"), "{stderr}");
    assert!(
        stderr.contains("inheritance cycle: Circle extends Square extends Circle"),
        "{stderr}"
    );
}

// 06. An entry class whose chain leads into a module ring: the module's
//     diagnostic is the only one, and nothing downstream recurses or ICEs.
#[test]
fn class_cycle_06_entry_class_extending_into_a_module_ring_does_not_ice() {
    let shapes = "module shapes;\n\
pub open class Circle extends Square { pub r: i64; }\n\
pub open class Square extends Circle { pub s: i64; }\n";
    let main = "import shapes;\n\
class Leaf extends shapes::Circle { pub z: i64; }\n\
fn main() { let l = new Leaf(1, 2, 3); println(l.z); }\n";
    let stderr =
        compile_temp_project_error_stderr(&[("shapes.wi", shapes), ("main.wi", main)], "main.wi");
    assert!(stderr.contains("error[E0426]"), "{stderr}");
    assert!(!stderr.contains("E0800"), "{stderr}");
    assert!(!stderr.contains("internal compiler error"), "{stderr}");
}

// 07. Identity is the registered class, not the spelling: a local `Root`
//     extending an alias of the module's `Root` is a plain two-class chain.
#[test]
fn class_cycle_07_alias_of_a_same_named_import_is_not_a_ring() {
    let shapes = "module shapes;\npub open class Root { pub fn tag(self) -> i64 { return 7; } }\n";
    let main = "import shapes::Root as Alias;\n\
open class Root extends Alias {}\n\
class Other extends Root {}\n\
fn main() { let o = new Other(); println(o.tag()); }\n";
    let (out, ok) =
        compile_temp_project_and_run(&[("shapes.wi", shapes), ("main.wi", main)], "main.wi");
    assert!(ok, "expected the aliased chain to compile");
    assert_eq!(out, "7\n");
}

// 08. A chain that crosses into a module and ends at a root there is not a
//     ring, and the inherited method still dispatches.
#[test]
fn class_cycle_08_acyclic_chain_through_an_imported_base_runs() {
    let shapes = "module shapes;\n\
pub open class Base { pub fn tag(self) -> i64 { return 1; } }\n\
pub open class Mid extends Base { pub fn more(self) -> i64 { return 2; } }\n";
    let main = "import shapes;\n\
class Leaf extends shapes::Mid {}\n\
fn main() { let l = new Leaf(); println(l.tag() + l.more()); }\n";
    let (out, ok) =
        compile_temp_project_and_run(&[("shapes.wi", shapes), ("main.wi", main)], "main.wi");
    assert!(ok, "expected the cross-module chain to compile");
    assert_eq!(out, "3\n");
}

// 09. The example that documents the rule is a runnable program.
#[test]
fn class_cycle_09_example_program_runs() {
    let (out, ok) = compile_file_and_run("example/class_inheritance_cycle_rejected.wi");
    assert!(ok, "example failed:\n{out}");
    assert_eq!(out, "Leaf: 1 2 3\nsum: 6\n");
}

// 10. A class outside the ring that extends into it: constructing it with
//     any argument count is E0426 alone, not E0426 plus a constructor arity
//     error computed from the unusable chain.
#[test]
fn class_cycle_10_new_of_a_reacher_adds_no_arity_error() {
    let src = "open class A extends B { pub x: i64; pub init(self, x: i64) { self.x = x; } }\n\
open class B extends A { pub y: i64; }\n\
class C extends A { pub z: i64; }\n\
fn main() { let c = new C(1); let a = new A(1); println(c.z + a.x); }\n";
    let stderr = compile_error_stderr(src);
    assert!(stderr.contains("error[E0426]"), "{stderr}");
    assert!(!stderr.contains("E0845"), "{stderr}");
    assert!(stderr.contains("aborting due to 1 error"), "{stderr}");
}
