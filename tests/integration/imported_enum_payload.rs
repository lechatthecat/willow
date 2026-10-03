//! Binding an enum payload whose type is a class or interface declared in a
//! module OTHER than the enum's own (willow-jz15.38).
//!
//! `willow check` accepted these programs and the build then stopped with
//! "E0800 internal compiler error ... the field `hi` on a `bx::Bx` ... has
//! incompatible operands". Two tables held the declaring file's own spelling of
//! the payload (`Bx`, an alias `Box2`, an item-imported interface `Shape`)
//! instead of its identity (`bx::Bx`, `bx::Shape`):
//!
//!   * HIR lowering's enum table, which types a `match` binding, so the binding
//!     slot and every member access on it disagreed;
//!   * the back-end's build-wide enum metadata, registered from each unit's
//!     checker, so an importer met payload names it cannot resolve and refused
//!     every function taking the enum.
//!
//! Both now canonicalize payloads the way field and parameter types already
//! were. These perspectives assert the build succeeds and the values are right.

use super::support::*;

const PLAIN: [(&str, &str); 0] = [];

/// The module that declares every payload type.
const BX: &str = r#"
module bx;

pub class Bx {
    pub hi: i64;

    pub fn get(self) -> i64 {
        return self.hi * 2;
    }
}

pub open class Base {
    pub v: i64;

    pub open fn kind(self) -> i64 {
        return self.v;
    }
}

pub class Derived extends Base {
    pub override fn kind(self) -> i64 {
        return self.v + 1000;
    }
}

pub interface Shape {
    fn area(self) -> i64;
}

pub class Sq implements Shape {
    pub s: i64;

    pub fn area(self) -> i64 {
        return self.s * self.s;
    }
}

pub enum Tag {
    A,
    B(i64),
}
"#;

fn files<'a>(tree: &'a str, main: &'a str) -> [(&'a str, &'a str); 3] {
    [("bx.wi", BX), ("tree.wi", tree), ("main.wi", main)]
}

fn run(tree: &str, main: &str) -> String {
    let (out, ok) = compile_temp_project_with_env_and_run(&files(tree, main), "main.wi", &PLAIN);
    assert!(ok, "build or run failed: {out}");
    out
}

/// The ticket's repro: an enum over an item-imported class, bound in a match
/// inside the declaring module.
const TREE_LEAF: &str = r#"
module tree;
import bx::Bx;
pub enum Node { Leaf(Bx), Empty }
pub fn hi(n: Node) -> i64 { match n { Node::Leaf(b) => { return b.hi; } Node::Empty => { return 0; } } }
pub fn get(n: Node) -> i64 { return match n { Node::Leaf(b) => b.get(), Node::Empty => -1 }; }
"#;

// 1. A field read on the bound payload, in a non-entry module.
#[test]
fn p01_field_read_in_declaring_module() {
    let out = run(
        TREE_LEAF,
        "import tree; import bx::Bx;\nfn main() { println(tree::hi(tree::Node::Leaf(new Bx(5)))); }",
    );
    assert_eq!(out, "5\n");
}

// 2. A method call on the bound payload, in a non-entry module.
#[test]
fn p02_method_call_in_declaring_module() {
    let out = run(
        TREE_LEAF,
        "import tree; import bx::Bx;\nfn main() { println(tree::get(tree::Node::Leaf(new Bx(5)))); println(tree::get(tree::Node::Empty)); }",
    );
    assert_eq!(out, "10\n-1\n");
}

// 3. The same in the ENTRY module: its own enum over an imported class.
#[test]
fn p03_entry_enum_over_imported_class() {
    let out = run(
        "module tree;",
        r#"
import bx::Bx;
enum Node { Leaf(Bx) }
fn visit(n: Node) -> i64 { match n { Node::Leaf(b) => { return b.hi + b.get(); } } }
fn main() { println(visit(Node::Leaf(new Bx(4)))); }
"#,
    );
    assert_eq!(out, "12\n");
}

/// Payloads under every spelling an import gives, matched in the module.
const TREE_SPELLINGS: &str = r#"
module tree;
import bx;
import bx::Bx;
import bx::Bx as Box2;
import bx::Shape;
import bx::Tag;
import std::collections::Array;
pub enum Node {
    Pair(Bx, Box2),
    S(Shape),
    Q(bx::Bx),
    Many(Array<Bx>),
    Maybe(Option<Box2>),
    T(Tag),
    Empty,
}
pub fn visit(n: Node) -> i64 {
    return match n {
        Node::Pair(a, c) => a.get() + c.hi,
        Node::S(s) => s.area(),
        Node::Q(q) => q.get(),
        Node::Many(xs) => xs[0].get() + xs.len(),
        Node::Maybe(m) => match m { Some(b) => b.get(), None => -1 },
        Node::T(t) => match t { Tag::A => 1, Tag::B(v) => v },
        Node::Empty => 0,
    };
}
pub fn take(b: Box2) -> i64 { return b.hi; }
pub fn forward(n: Node) -> i64 { return match n { Node::Pair(_, c) => take(c), _ => 0 }; }
"#;

// 4. An ALIASED item import (`Box2`) as the payload, matched in the module.
#[test]
fn p04_aliased_payload_in_declaring_module() {
    let out = run(
        TREE_SPELLINGS,
        "import tree; import bx::Bx;\nfn main() { println(tree::visit(tree::Node::Pair(new Bx(1), new Bx(2)))); }",
    );
    assert_eq!(out, "4\n");
}

// 5. The importer of that enum: `Box2` and `Shape` mean nothing in `main`,
// which used to make every function taking `tree::Node` unsupported there.
#[test]
fn p05_aliased_and_interface_payloads_matched_by_importer() {
    let out = run(
        TREE_SPELLINGS,
        r#"
import tree;
import bx;
fn local(n: tree::Node) -> i64 {
    return match n {
        tree::Node::Pair(a, c) => a.hi * 10 + c.get(),
        tree::Node::S(s) => s.area() + 1,
        _ => 0,
    };
}
fn main() {
    println(local(tree::Node::Pair(new bx::Bx(1), new bx::Bx(2))));
    println(local(tree::Node::S(new bx::Sq(3))));
}
"#,
    );
    assert_eq!(out, "14\n10\n");
}

// 6. An item-imported INTERFACE payload, dispatched in the module.
#[test]
fn p06_interface_payload_in_declaring_module() {
    let out = run(
        TREE_SPELLINGS,
        "import tree; import bx;\nfn main() { println(tree::visit(tree::Node::S(new bx::Sq(4)))); }",
    );
    assert_eq!(out, "16\n");
}

// 7. A module-qualified payload (`bx::Bx`).
#[test]
fn p07_module_qualified_payload() {
    let out = run(
        TREE_SPELLINGS,
        "import tree; import bx;\nfn main() { println(tree::visit(tree::Node::Q(new bx::Bx(6)))); }",
    );
    assert_eq!(out, "12\n");
}

// 8. A payload that NESTS the imported class: `Array<Bx>`.
#[test]
fn p08_array_of_imported_class_payload() {
    let out = run(
        TREE_SPELLINGS,
        "import tree; import bx::Bx;\nfn main() { println(tree::visit(tree::Node::Many([new Bx(3), new Bx(9)]))); }",
    );
    assert_eq!(out, "8\n");
}

// 9. `Option<Box2>`: the alias inside a type argument, matched again.
#[test]
fn p09_option_of_aliased_class_payload() {
    let out = run(
        TREE_SPELLINGS,
        r#"
import tree; import bx::Bx;
fn main() {
    println(tree::visit(tree::Node::Maybe(Some(new Bx(4)))));
    println(tree::visit(tree::Node::Maybe(None)));
}
"#,
    );
    assert_eq!(out, "8\n-1\n");
}

// 10. An imported ENUM as the payload, matched one level down.
#[test]
fn p10_imported_enum_payload() {
    let out = run(
        TREE_SPELLINGS,
        "import tree; import bx;\nfn main() { println(tree::visit(tree::Node::T(bx::Tag::B(7)))); println(tree::visit(tree::Node::T(bx::Tag::A))); }",
    );
    assert_eq!(out, "7\n1\n");
}

// 11. The bound alias-typed payload handed to a function declared with the
// alias: one identity on both sides.
#[test]
fn p11_bound_payload_passed_as_alias_parameter() {
    let out = run(
        TREE_SPELLINGS,
        "import tree; import bx::Bx;\nfn main() { println(tree::forward(tree::Node::Pair(new Bx(1), new Bx(33)))); }",
    );
    assert_eq!(out, "33\n");
}

// 12. A GENERIC enum mixing a type parameter with an imported class.
#[test]
fn p12_generic_enum_with_imported_class() {
    let out = run(
        r#"
module tree;
import bx::Bx;
pub enum Wrap<T> { W(T, Bx), Nothing }
pub fn unwrap(w: Wrap<i64>) -> i64 { return match w { Wrap::W(v, b) => v + b.get(), Wrap::Nothing => 0 }; }
"#,
        "import tree; import bx::Bx;\nfn main() { println(tree::unwrap(tree::Wrap::W(10, new Bx(1)))); }",
    );
    assert_eq!(out, "12\n");
}

// 13. Unqualified variant patterns (`Leaf(b)`) on the module's own enum.
#[test]
fn p13_unqualified_variant_pattern() {
    let out = run(
        r#"
module tree;
import bx::Bx as B;
pub enum Node { Leaf(B), Empty }
pub fn hi(n: Node) -> i64 { return match n { Leaf(b) => b.hi, Empty => 0 }; }
"#,
        "import tree; import bx::Bx;\nfn main() { println(tree::hi(tree::Node::Leaf(new Bx(21)))); }",
    );
    assert_eq!(out, "21\n");
}

// 14. A payload typed by an imported OPEN base, holding a subclass: the
// binding dispatches virtually.
#[test]
fn p14_imported_base_payload_dispatches_virtually() {
    let out = run(
        r#"
module tree;
import bx::Base;
pub enum Node { Leaf(Base) }
pub fn kind(n: Node) -> i64 { return match n { Node::Leaf(b) => b.kind() + b.v }; }
"#,
        r#"
import tree; import bx;
fn main() {
    println(tree::kind(tree::Node::Leaf(new bx::Base(1))));
    println(tree::kind(tree::Node::Leaf(new bx::Derived(2))));
}
"#,
    );
    assert_eq!(out, "2\n1004\n");
}

// 15. Inside an ASYNC function, across a suspension.
#[test]
fn p15_async_match_across_suspension() {
    let out = run(
        r#"
module tree;
import bx::Bx;
import bx::Bx as Box2;
pub enum Two { P(Bx, Box2), Z }
pub async fn visit(n: Two) -> i64 {
    await yield();
    return match n { Two::P(a, c) => { await yield(); return a.get() + c.hi; }, Two::Z => 0 };
}
"#,
        r#"
import tree; import bx::Bx;
async fn main() { println(await tree::visit(tree::Two::P(new Bx(3), new Bx(4)))); }
"#,
    );
    assert_eq!(out, "10\n");
}

// 16. A closure capturing the bound payloads.
#[test]
fn p16_closure_captures_bound_payload() {
    let out = run(
        r#"
module tree;
import bx::Bx;
import bx::Bx as Box2;
pub enum Two { P(Bx, Box2), Z }
pub fn lam(n: Two) -> i64 {
    return match n {
        Two::P(a, c) => { let f: closure(i64) -> i64 = |k| a.get() + c.hi + k; return f(100); },
        Two::Z => 0,
    };
}
"#,
        "import tree; import bx::Bx;\nfn main() { println(tree::lam(tree::Two::P(new Bx(1), new Bx(2)))); }",
    );
    assert_eq!(out, "104\n");
}

// 17. The entry ITEM-imports the module's enum and matches it bare.
#[test]
fn p17_item_imported_enum_matched_bare() {
    let out = run(
        TREE_LEAF,
        r#"
import tree::Node;
import bx::Bx;
fn f(n: Node) -> i64 { return match n { Node::Leaf(b) => b.get() + b.hi, Node::Empty => 0 }; }
fn main() { println(f(Node::Leaf(new Bx(2)))); }
"#,
    );
    assert_eq!(out, "6\n");
}

// 18. The importer declares a DIFFERENT class with the payload's bare name:
// the binding must keep `bx::Bx`'s identity, not the local `Bx`.
#[test]
fn p18_local_class_with_payload_bare_name() {
    let out = run(
        TREE_LEAF,
        r#"
import tree;
import bx;
class Bx { pub lo: i64; pub fn get(self) -> i64 { return -self.lo; } }
fn f(n: tree::Node) -> i64 { return match n { tree::Node::Leaf(b) => b.get() + b.hi, tree::Node::Empty => 0 }; }
fn main() {
    let mine = new Bx(1);
    println(f(tree::Node::Leaf(new bx::Bx(3))) + mine.get());
}
"#,
    );
    assert_eq!(out, "8\n");
}

// 19. Under allocation-triggered GC stress: the payload binding is a root that
// survives collections inside the arm.
#[test]
fn p19_payload_survives_gc_stress() {
    let (out, ok) = compile_temp_project_with_env_and_run_under(
        &files(
            r#"
module tree;
import bx::Bx as Box2;
pub enum Node { Leaf(Box2), Empty }
pub fn churn(n: Node) -> i64 {
    return match n {
        Node::Leaf(b) => {
            let mut total = 0;
            for i in 0..200 { let t = new Box2(i); total = total + t.hi; }
            return total + b.get();
        },
        Node::Empty => 0,
    };
}
"#,
            "import tree; import bx::Bx;\nfn main() { println(tree::churn(tree::Node::Leaf(new Bx(7)))); }",
        ),
        "main.wi",
        &PLAIN,
        &[("WILLOW_GC_STRESS", "alloc")],
    );
    assert!(ok, "stress run failed: {out}");
    assert_eq!(out, "19914\n");
}

// 20. A release build.
#[test]
fn p20_release_build() {
    let project = TestProject::new(
        "imported_enum_payload_release",
        &files(
            TREE_SPELLINGS,
            r#"
import tree; import bx;
fn local(n: tree::Node) -> i64 { return match n { tree::Node::Pair(a, c) => a.hi + c.get(), _ => 0 }; }
fn main() {
    println(tree::visit(tree::Node::Pair(new bx::Bx(1), new bx::Bx(2))));
    println(local(tree::Node::Pair(new bx::Bx(1), new bx::Bx(2))));
}
"#,
        ),
    );
    let output = project.compile_release("main.wi");
    assert!(
        output.status.success(),
        "release build failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let run = project.run();
    assert_eq!(String::from_utf8_lossy(&run.stdout), "4\n5\n");
}

// 21. Negative control: an unknown member on the bound payload is a user
// diagnostic, never an internal compiler error.
#[test]
fn p21_unknown_member_is_a_diagnostic_not_an_ice() {
    let stderr = compile_temp_project_error_stderr(
        &files(
            r#"
module tree;
import bx::Bx as Box2;
pub enum Node { Leaf(Box2) }
pub fn bad(n: Node) -> i64 { return match n { Node::Leaf(b) => b.missing }; }
"#,
            "import tree; import bx::Bx;\nfn main() { println(tree::bad(tree::Node::Leaf(new Bx(1)))); }",
        ),
        "main.wi",
    );
    assert!(stderr.contains("missing"), "{stderr}");
    assert!(!stderr.contains("E0800"), "{stderr}");
}

// 22. Negative control: the bound `Box2` payload is still NOT a `Shape`.
#[test]
fn p22_payload_identity_still_rejects_wrong_type() {
    let stderr = compile_temp_project_error_stderr(
        &files(
            r#"
module tree;
import bx::Bx as Box2;
import bx::Shape;
pub enum Node { Leaf(Box2) }
fn want(s: Shape) -> i64 { return s.area(); }
pub fn bad(n: Node) -> i64 { return match n { Node::Leaf(b) => want(b) }; }
"#,
            "import tree; import bx::Bx;\nfn main() { println(tree::bad(tree::Node::Leaf(new Bx(1)))); }",
        ),
        "main.wi",
    );
    assert!(stderr.contains("error["), "{stderr}");
    assert!(!stderr.contains("E0800"), "{stderr}");
}

// 23. A unit that never imports `bx` at all: it reaches `bx::Bx` and
// `bx::Shape` only through `tree`'s signature, and binds and uses them.
#[test]
fn p23_importer_reaching_payload_types_only_through_the_enum() {
    let out = run(
        r#"
module tree;
import bx::Bx as Box2;
import bx::Shape;
import bx::Sq;
pub enum Node { Leaf(Box2), S(Shape) }
pub fn leaf(v: i64) -> Node { return Node::Leaf(new Box2(v)); }
pub fn square(v: i64) -> Node { return Node::S(new Sq(v)); }
"#,
        r#"
import tree;
fn f(n: tree::Node) -> i64 { return match n { tree::Node::Leaf(b) => b.get() + b.hi, tree::Node::S(s) => s.area() }; }
fn main() { println(f(tree::leaf(4))); println(f(tree::square(5))); }
"#,
    );
    assert_eq!(out, "12\n25\n");
}

// ---------------------------------------------------------------------------
// The runnable example.
// ---------------------------------------------------------------------------

const EXAMPLE_OUTPUT: &str = "3\n14\n25\n2\n3\nhi!\n25\nhi\nhi!\n42\n25\n";

// 24. It runs.
#[test]
fn p24_the_imported_enum_payload_example_runs() {
    let (out, ok) = compile_file_and_run("example/imported_enum_payload/main.wi");
    assert!(
        ok,
        "example/imported_enum_payload/main.wi failed to compile or run"
    );
    assert_eq!(out, EXAMPLE_OUTPUT);
}
