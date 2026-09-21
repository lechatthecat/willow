use super::super::support::*;

// ── self receiver semantics ─────────────────────────────────────────────────

#[test]
fn test_self_field_read_and_assignment() {
    let (out, ok) = compile_and_run(
        r#"
class Counter {
    pub init(self, n: i64) {
        self.n = n;
    }
    n: i64;
    pub fn inc(self) { self.n = self.n + 1; }
    pub fn add(self, n: i64) { self.n = self.n + n; }
    pub fn get(self) -> i64 { return self.n; }
}
fn main() {
    let c = new Counter(0);
    c.inc();
    c.inc();
    c.add(5);
    println(c.get());
}
"#,
    );
    assert!(ok);
    assert_eq!(out, "7\n");
}

#[test]
fn test_self_method_call_inside_method() {
    let (out, ok) = compile_and_run(
        r#"
class Wrap {
    pub init(self, n: i64) {
        self.n = n;
    }
    n: i64;
    pub fn double(self) -> i64 { return self.n * 2; }
    pub fn compute(self) -> i64 { return self.double() + 1; }
}
fn main() {
    let w = new Wrap(5);
    println(w.compute());
}
"#,
    );
    assert!(ok);
    assert_eq!(out, "11\n");
}

#[test]
fn test_self_accesses_and_assigns_inherited_field() {
    let (out, ok) = compile_and_run(
        r#"
open class Base {
    pub score: i64;
}
class Child extends Base {
    pub fn boost(self, n: i64) { self.score = self.score + n; }
    pub fn get(self) -> i64 { return self.score; }
}
fn main() {
    let c = new Child(10);
    c.boost(5);
    println(c.get());
}
"#,
    );
    assert!(ok);
    assert_eq!(out, "15\n");
}

#[test]
fn test_self_field_assign_inside_control_flow() {
    let (out, ok) = compile_and_run(
        r#"
class Acc {
    pub init(self, total: i64) {
        self.total = total;
    }
    total: i64;
    pub fn accumulate(self, n: i64) {
        let mut i = 0;
        while i < n {
            if i >= 0 {
                self.total = self.total + 1;
            }
            i = i + 1;
        }
    }
    pub fn get(self) -> i64 { return self.total; }
}
fn main() {
    let a = new Acc(0);
    a.accumulate(5);
    println(a.get());
}
"#,
    );
    assert!(ok);
    assert_eq!(out, "5\n");
}

#[test]
fn test_static_and_instance_methods_coexist() {
    let (out, ok) = compile_and_run(
        r#"
class Adder {
    pub init(self, base: i64) {
        self.base = base;
    }
    base: i64;
    pub fn add_base(self, n: i64) -> i64 { return self.base + n; }
    pub static fn pure(a: i64, b: i64) -> i64 { return a + b; }
}
fn main() {
    let a = new Adder(10);
    println(a.add_base(5));
    println(Adder::pure(2, 3));
}
"#,
    );
    assert!(ok);
    assert_eq!(out, "15\n5\n");
}

#[test]
fn test_self_upper_static_call_inside_instance_method() {
    let (out, ok) = compile_and_run(
        r#"
class Counter {
    pub init(self, value: i64) {
        self.value = value;
    }
    value: i64;
    pub static fn make(value: i64) -> Counter { return new Counter(value); }
    pub fn clone_plus(self, n: i64) -> i64 {
        let next = Self::make(self.value + n);
        return next.value;
    }
}
fn main() {
    let c = new Counter(8);
    println(c.clone_plus(4));
}
"#,
    );
    assert!(ok);
    assert_eq!(out, "12\n");
}

#[test]
fn test_self_lower_static_call_inside_instance_method() {
    let (out, ok) = compile_and_run(
        r#"
class Math {
    pub init(self, value: i64) {
        self.value = value;
    }
    value: i64;
    pub static fn pure(a: i64, b: i64) -> i64 { return a + b; }
    pub fn add_to_value(self, n: i64) -> i64 {
        return self::pure(self.value, n);
    }
}
fn main() {
    let m = new Math(20);
    println(m.add_to_value(22));
}
"#,
    );
    assert!(ok);
    assert_eq!(out, "42\n");
}

#[test]
fn test_instance_method_called_with_static_syntax_is_error() {
    assert_compile_error_contains(
        r#"
class Box {
    v: i64;
    pub fn get(self) -> i64 { return self.v; }
    pub fn bad(self) -> i64 { return Self::get(); }
}
fn main() {}
"#,
        &[
            "instance method called with `::`",
            "write `self.get` instead",
        ],
    );
}

#[test]
fn test_static_method_called_with_dot_is_error() {
    assert_compile_error_contains(
        r#"
class Math {
    pub static fn add(a: i64, b: i64) -> i64 { return a + b; }
}
fn main() {
    let m = new Math();
    println(m.add(1, 2));
}
"#,
        &["static method called with `.`", "write `Math::add` instead"],
    );
}

#[test]
fn test_self_static_call_outside_class_is_error() {
    assert_compile_error_contains(
        r#"
fn main() {
    println(Self::make());
}
"#,
        &["`Self` can only be used inside a class method"],
    );
}

#[test]
fn test_legacy_this_receiver_is_error() {
    assert_compile_error_contains(
        r#"
class Box {
    v: i64;
    pub fn get(self) -> i64 { return this.v; }
}
fn main() {}
"#,
        &["receiver alias `this` is not supported", "use `self`"],
    );
}

#[test]
fn test_legacy_this_identifier_declaration_is_error() {
    assert_compile_error_contains(
        r#"
fn main() {
    let this = 1;
}
"#,
        &["identifier `this` is reserved", "use `self`"],
    );
}

#[test]
fn test_self_in_static_method_is_error() {
    assert_compile_error_contains(
        r#"
class Math {
    pub static fn bad() -> i64 {
        return self.value;
    }
}
fn main() {}
"#,
        &["`self` is not available in static method"],
    );
}

#[test]
fn test_assign_to_self_is_error() {
    assert_compile_error_contains(
        r#"
class Box {
    v: i64;
    pub fn bad(self) {
        self = new Box(1);
    }
}
fn main() {}
"#,
        &["cannot assign to `self`"],
    );
}
