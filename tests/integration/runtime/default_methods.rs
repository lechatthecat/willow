use super::*;

// ── Default interface methods (willow-1js.3) ─────────────────────────────────

#[test]
fn default_method_01_used_when_not_overridden() {
    let (out, ok) = compile_and_run(
        r#"
interface Greeter {
    fn name(self) -> String;
    fn greet(self) -> String { return "Hi " + self.name(); }
}
class Dog implements Greeter {
    pub fn name(self) -> String { return "Rex"; }
}
fn main() { println(new Dog().greet()); }
"#,
    );
    assert!(ok, "{out}");
    assert_eq!(out, "Hi Rex\n");
}

#[test]
fn default_method_02_override_wins() {
    let (out, ok) = compile_and_run(
        r#"
interface Greeter {
    fn name(self) -> String;
    fn greet(self) -> String { return "Hi " + self.name(); }
}
class Cat implements Greeter {
    pub fn name(self) -> String { return "Tom"; }
    pub fn greet(self) -> String { return "Meow " + self.name(); }
}
fn main() { println(new Cat().greet()); }
"#,
    );
    assert!(ok, "{out}");
    assert_eq!(out, "Meow Tom\n");
}

#[test]
fn default_method_03_dispatch_through_interface() {
    let (out, ok) = compile_and_run(
        r#"
interface Greeter {
    fn name(self) -> String;
    fn greet(self) -> String { return "Hi " + self.name(); }
}
class Dog implements Greeter { pub fn name(self) -> String { return "Rex"; } }
fn run(g: Greeter) { println(g.greet()); }
fn main() { run(new Dog()); }
"#,
    );
    assert!(ok, "{out}");
    assert_eq!(out, "Hi Rex\n");
}

#[test]
fn default_method_04_default_calls_default() {
    let (out, ok) = compile_and_run(
        r#"
interface Calc {
    fn base(self) -> i64;
    fn doubled(self) -> i64 { return self.base() * 2; }
    fn plus(self, n: i64) -> i64 { return self.doubled() + n; }
}
class Num implements Calc { pub fn base(self) -> i64 { return 5; } }
fn main() {
    let n = new Num();
    println(n.doubled());
    println(n.plus(3));
}
"#,
    );
    assert!(ok, "{out}");
    assert_eq!(out, "10\n13\n");
}

#[test]
fn default_method_05_override_seen_by_other_default() {
    // shout() is a default that calls greet(); when greet() is overridden, the
    // default shout() must call the override (dynamic self-dispatch).
    let (out, ok) = compile_and_run(
        r#"
interface Greeter {
    fn name(self) -> String;
    fn greet(self) -> String { return "Hi " + self.name(); }
    fn shout(self) -> String { return self.greet() + "!"; }
}
class Robot implements Greeter {
    pub fn name(self) -> String { return "R2"; }
    pub fn greet(self) -> String { return "BEEP " + self.name(); }
}
fn main() { println(new Robot().shout()); }
"#,
    );
    assert!(ok, "{out}");
    assert_eq!(out, "BEEP R2!\n");
}

#[test]
fn default_method_06_required_method_still_enforced() {
    // A non-default (required) method must still be implemented.
    assert!(expect_compile_error(
        r#"
interface I {
    fn req(self) -> i64;
    fn opt(self) -> i64 { return 1; }
}
class C implements I {}
fn main() {}
"#,
    ));
}

#[test]
fn default_method_07_no_self_default_rejected() {
    // A default body requires a `self` receiver (E0420).
    assert!(expect_compile_error(
        r#"
interface I { fn f() { return; } }
fn main() {}
"#,
    ));
}
