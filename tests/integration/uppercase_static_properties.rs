//! willow-jz15.103: bare uppercase class members are property reads.
//! Perspectives: (1) scalar, (2) frozen array index, (3) for head, (4) if head,
//! (5) while head, (6) match head, (7) Self, (8) initializer dependency,
//! (9) mutable store/read, (10) String root, (11) module path, (12) module alias,
//! (13) item import, (14) module-local read, (15) inherited read,
//! (16) private internal read, (17) private external rejection,
//! (18) protected external rejection, (19) immutable store rejection,
//! (20) explicit call stays a method, (21) enum variants, (22) module const,
//! (23) release, (24) GC stress, (25) missing member, (26) increasing read count.
use super::support::*;

#[test]
fn uppercase_static_example() {
    let src = include_str!("../../example/uppercase_static_properties.wi");
    for release in [false, true] {
        let project = TestProject::new("uppercase_static", &[("main.wi", src)]);
        let compiled = if release {
            project.compile_release("main.wi")
        } else {
            project.compile("main.wi")
        };
        assert!(
            compiled.status.success(),
            "{}",
            String::from_utf8_lossy(&compiled.stderr)
        );
        let out = project.run_with_env(&[("WILLOW_GC_STRESS", "1")]);
        assert!(
            out.status.success(),
            "{}",
            String::from_utf8_lossy(&out.stderr)
        );
        assert_eq!(
            String::from_utf8_lossy(&out.stdout),
            "tables\n10\n6\n10\n16\nonce\nsixteen\n"
        );
    }
}

#[test]
fn uppercase_static_modules_and_inheritance() {
    let (out, ok) = compile_temp_project_and_run(
        &[
            (
                "tables.wi",
                r#"
pub const LIMIT: i64 = 9;
pub enum Color { Red, Blue }
pub open class Tables {
    pub static KNIGHT: FrozenArray<i64> = [6, 10].freeze();
    static SECRET: i64 = 3;
    pub static fn secret() -> i64 { return Self::SECRET; }
}
pub fn read() -> i64 { return Tables::KNIGHT[0]; }
"#,
            ),
            (
                "main.wi",
                r#"
import tables;
import tables as t;
import tables::Tables;
class Child extends Tables {}
fn main() {
    println(tables::Tables::KNIGHT[0]);
    println(t::Tables::KNIGHT[1]);
    for n in tables::Tables::KNIGHT { println(n); }
    println(Tables::KNIGHT[1]);
    println(tables::read());
    println(Child::KNIGHT[0]);
    println(Tables::secret());
    println(tables::LIMIT);
    match tables::Color::Red { tables::Color::Red => println(1), tables::Color::Blue => println(2), }
}
"#,
            ),
        ],
        "main.wi",
    );
    assert!(ok, "{out}");
    assert_eq!(out, "6\n10\n6\n10\n10\n6\n6\n3\n9\n1\n");
}

#[test]
fn uppercase_static_explicit_method_call() {
    let (out, ok) = compile_and_run(
        r#"
class C {
    pub static VALUE: i64 = 7;
    pub static fn VALUE() -> i64 { return 8; }
}
fn main() { println(C::VALUE); println(C::VALUE()); }
"#,
    );
    assert!(ok, "{out}");
    assert_eq!(out, "7\n8\n");
}

#[test]
fn uppercase_static_diagnostics() {
    for (src, expected) in [
        (
            "class C { static VALUE: i64 = 1; } fn main() { println(C::VALUE); }",
            "is private",
        ),
        (
            "class C { prot static VALUE: i64 = 1; } fn main() { println(C::VALUE); }",
            "is protected",
        ),
        (
            "class C { pub static VALUE: i64 = 1; } fn main() { C::VALUE = 2; }",
            "immutable static property",
        ),
        (
            "class C { pub static VALUE: i64 = 1; } fn main() { println(C::VALUE()); }",
            "no method `VALUE`",
        ),
        (
            "class C {} fn main() { println(C::MISSING); }",
            "no static property `C::MISSING`",
        ),
    ] {
        assert_compile_error_contains(src, &[expected]);
    }
}

#[test]
fn uppercase_static_linear_hir_reads() {
    // Deterministic generated-IR count, rather than a wall-clock scaling claim.
    for count in [1, 8, 32, 128] {
        let src = format!(
            "class C {{ pub static VALUE: i64 = 7; }} fn main() {{ {} }}",
            "println(C::VALUE);".repeat(count)
        );
        let project = TestProject::new("uppercase_static_scale", &[("main.wi", &src)]);
        let output = project.emit_hir("main.wi");
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        let hir = String::from_utf8_lossy(&output.stdout);
        assert_eq!(
            hir.matches("println(C::VALUE: i64)").count(),
            count,
            "{hir}"
        );
    }
}

#[test]
fn uppercase_static_forward_initializer_and_cycle() {
    let (out, ok) = compile_and_run(
        r#"
class A { pub static VALUE: i64 = seed(); }
fn seed() -> i64 { return B::VALUE; }
class B { pub static VALUE: i64 = 42; }
fn main() { println(A::VALUE); }
"#,
    );
    assert!(ok, "{out}");
    assert_eq!(out, "42\n");
    assert_compile_error_contains(
        "class A { pub static VALUE: i64 = B::VALUE; } class B { pub static VALUE: i64 = A::VALUE; } fn main() {}",
        &["static initialization cycle"],
    );
}

#[test]
fn uppercase_static_module_control_heads_and_store() {
    let (out, ok) = compile_temp_project_and_run(
        &[
            (
                "flags.wi",
                "pub class Flags { pub static YES: bool = true; pub static mut RUN: bool = true; }",
            ),
            (
                "main.wi",
                r#"
import flags;
fn main() {
    if flags::Flags::YES { println(1); }
    while flags::Flags::RUN { println(2); flags::Flags::RUN = false; }
    println(flags::Flags::RUN);
}
"#,
            ),
        ],
        "main.wi",
    );
    assert!(ok, "{out}");
    assert_eq!(out, "1\n2\nfalse\n");
}
