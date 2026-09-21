use super::*;

// Concurrency unification (willow-h2vf Stage 1): an async fn call returns an
// eager Task that can be awaited directly — no `spawn` needed.
// ── Send / Sync marker interfaces (willow-dgwo.1) ────────────────────────────
//
// 20 test perspectives for the compiler-known Send/Sync markers:
//  1. `class C implements Send` is rejected (E2401).
//  2. `class C implements Sync` is rejected (E2401).
//  3. The diagnostic names it a "compiler-known marker interface".
//  4. The help points at Mutex/RwLock/Atomic/Channel/frozen.
//  5. `implements Send` is rejected even with no fields.
//  6. `implements Sync` is rejected even with only immutable fields.
//  7. Markers are in scope with NO import (prelude).
//  8. `interface I extends Send` is allowed.
//  9. `interface I extends Sync` is allowed.
// 10. A class implementing a Send-extending interface compiles and runs.
// 11. A chained `extends` (Pet→Named→Sync) does not produce a false E2401.
// 12. The transitive marker is not mistaken for a manual impl.
// 13. `implements Animal, Send` still flags the Send (manual impl).
// 14. A Send-extending interface value dispatches correctly at runtime.
// 15. Normal programs (no markers) are unaffected by the prelude additions.
// 16. `implements Send` reports at the offending class.
// 17. One bad class does not suppress other valid classes.
// 18. A class can implement a real interface AND not be forced to name markers.
// 19. Markers work as an interface bound across module-free single files.
// 20. Existing interface conformance/dispatch is unchanged (regression suite).
#[test]
fn test_send_marker_manual_impl_rejected_e2401() {
    assert_compile_error_contains(
        "class Bad implements Send { value: i64; pub init(self, value: i64) { self.value = value; } }\nfn main() {}\n",
        &[
            "error[E2401]",
            "`Send` is a compiler-known marker interface",
            "cannot be implemented manually",
        ],
    );
}

#[test]
fn test_sync_marker_manual_impl_rejected_e2401() {
    assert_compile_error_contains(
        "class Bad implements Sync { value: i64; pub init(self, value: i64) { self.value = value; } }\nfn main() {}\n",
        &["error[E2401]", "`Sync`", "cannot be implemented manually"],
    );
}

#[test]
fn test_send_marker_e2401_help_mentions_safe_wrappers() {
    assert_compile_error_contains(
        "class Bad implements Sync {}\nfn main() {}\n",
        &["error[E2401]", "Mutex", "Channel"],
    );
}

#[test]
fn test_send_marker_rejected_even_with_no_fields() {
    assert_compile_error_contains(
        "class Empty implements Send {}\nfn main() {}\n",
        &["error[E2401]"],
    );
}

#[test]
fn test_marker_alongside_real_interface_still_flags_marker() {
    assert_compile_error_contains(
        r#"
interface Animal { fn speak(self) -> String; }
class Dog implements Animal, Send {
    pub fn speak(self) -> String { return "woof"; }
}
fn main() {}
"#,
        &["error[E2401]", "`Send`"],
    );
}

#[test]
fn test_interface_extends_send_is_allowed_and_runs() {
    let (out, ok) = compile_and_run(
        r#"
interface Job extends Send { fn run(self) -> i64; }
class Square implements Job {
    pub value: i64;
    pub fn run(self) -> i64 { return self.value * self.value; }
}
fn use_job(j: Job) -> i64 { return j.run(); }
fn main() { println(use_job(new Square(6))); }
"#,
    );
    assert!(ok);
    assert_eq!(out, "36\n");
}

#[test]
fn test_interface_extends_sync_is_allowed() {
    let (out, ok) = compile_and_run(
        r#"
interface Shared extends Sync { fn tag(self) -> i64; }
class Tag implements Shared {
    pub fn tag(self) -> i64 { return 7; }
}
fn main() { println(new Tag().tag()); }
"#,
    );
    assert!(ok);
    assert_eq!(out, "7\n");
}

#[test]
fn test_chained_extends_marker_no_false_e2401() {
    // Pet -> Named -> Sync; a class implementing Pet transitively "has" Sync but
    // must NOT be flagged as manually implementing it.
    let (out, ok) = compile_and_run(
        r#"
interface Named extends Sync { fn name(self) -> String; }
interface Pet extends Named { fn owner(self) -> String; }
class Dog implements Pet {
    pub fn name(self) -> String { return "Rex"; }
    pub fn owner(self) -> String { return "Sam"; }
}
fn main() { println(new Dog().name()); }
"#,
    );
    assert!(ok);
    assert_eq!(out, "Rex\n");
}

#[test]
fn test_markers_available_without_import() {
    // No `import` line — Send/Sync come from the prelude.
    let (out, ok) = compile_and_run(
        r#"
interface Task2 extends Send { fn go(self) -> i64; }
class Go implements Task2 { pub fn go(self) -> i64 { return 1; } }
fn main() { println(new Go().go()); }
"#,
    );
    assert!(ok);
    assert_eq!(out, "1\n");
}

#[test]
fn test_send_extending_interface_dispatches_at_runtime() {
    let (out, ok) = compile_and_run(
        r#"
interface Job extends Send { fn run(self) -> i64; }
class A implements Job { pub fn run(self) -> i64 { return 10; } }
class B implements Job { pub fn run(self) -> i64 { return 20; } }
fn run_it(j: Job) -> i64 { return j.run(); }
fn main() {
    println(run_it(new A()));
    println(run_it(new B()));
}
"#,
    );
    assert!(ok);
    assert_eq!(out, "10\n20\n");
}

#[test]
fn test_prelude_markers_do_not_break_normal_program() {
    let (out, ok) = compile_and_run("fn main() { println(42); }\n");
    assert!(ok);
    assert_eq!(out, "42\n");
}
