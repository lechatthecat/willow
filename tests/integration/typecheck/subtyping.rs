use super::super::support::*;

// ── Subtype: Dog passed to fn(Animal) ─────────────────────────────────────

// 1. void return: child passes to parent-typed parameter
#[test]
fn test_subtype_child_passes_to_parent_param() {
    let (out, ok) = compile_and_run(
        r#"
pub open class Animal {}
pub class Dog extends Animal {}
fn feed(a: Animal) { println(1); }
fn main() {
    let d = new Dog();
    feed(d);
}
"#,
    );
    assert!(ok);
    assert_eq!(out, "1\n");
}

// 2. parent method (same name) callable on concrete type — each dispatch to own impl
#[test]
fn test_subtype_parent_method_callable_on_child() {
    let (out, ok) = compile_and_run(
        r#"
pub open class Animal {
    pub open fn kind(self) -> i64 { return 0; }
}
pub class Dog extends Animal {
    pub override fn kind(self) -> i64 { return 1; }
}
fn main() {
    let a = new Animal();
    let d = new Dog();
    println(a.kind());
    println(d.kind());
}
"#,
    );
    assert!(ok);
    assert_eq!(out, "0\n1\n");
}

// 3. function returns child as parent type
#[test]
fn test_subtype_function_returns_child_as_parent() {
    let (out, ok) = compile_and_run(
        r#"
pub open class Animal {
    pub fn tag(self) -> i64 { return 42; }
}
pub class Cat extends Animal {}
fn make() -> Animal { return new Cat(); }
fn main() {
    let a = make();
    println(a.tag());
}
"#,
    );
    assert!(ok);
    assert_eq!(out, "42\n");
}

// 4. child stored in parent-typed variable — compiles, parent method used
#[test]
fn test_subtype_stored_in_parent_typed_var() {
    let (out, ok) = compile_and_run(
        r#"
pub open class Vehicle {
    pub open fn wheels(self) -> i64 { return 4; }
}
pub class Bike extends Vehicle {
    pub override fn wheels(self) -> i64 { return 2; }
}
fn main() {
    let v: Vehicle = new Bike();
    let b = new Bike();
    println(v.wheels());
    println(b.wheels());
}
"#,
    );
    assert!(ok);
    // Dynamic dispatch: v holds a Bike at runtime, so Bike__wheels (2) is called.
    // b is a Bike, so Bike__wheels (2) is called.
    assert_eq!(out, "2\n2\n");
}

// 5. two different subtypes each compile correctly as parent-typed argument
#[test]
fn test_subtype_two_children_same_function() {
    let (out, ok) = compile_and_run(
        r#"
pub open class Shape {
    pub open fn name(self) -> i64 { return 0; }
}
pub class Square extends Shape {
    pub override fn name(self) -> i64 { return 4; }
}
pub class Triangle extends Shape {
    pub override fn name(self) -> i64 { return 3; }
}
fn accept_shape(s: Shape) { println(1); }
fn main() {
    let sq = new Square();
    let tr = new Triangle();
    accept_shape(sq);
    accept_shape(tr);
    println(sq.name());
    println(tr.name());
}
"#,
    );
    assert!(ok);
    assert_eq!(out, "1\n1\n4\n3\n");
}

// 6. child passed through two function calls
#[test]
fn test_subtype_child_passed_through_two_calls() {
    let (out, ok) = compile_and_run(
        r#"
pub open class Base {
    pub fn val(self) -> i64 { return 7; }
}
pub class Child extends Base {}
fn wrap(b: Base) -> i64 { return b.val(); }
fn outer(b: Base) -> i64 { return wrap(b); }
fn main() {
    println(outer(new Child()));
}
"#,
    );
    assert!(ok);
    assert_eq!(out, "7\n");
}

// 7. three-level hierarchy: grandchild compiles as grandparent arg; each type calls own method
#[test]
fn test_subtype_grandchild_to_grandparent_fn() {
    let (out, ok) = compile_and_run(
        r#"
pub open class A {
    pub open fn tag(self) -> i64 { return 1; }
}
pub open class B extends A {
    pub open override fn tag(self) -> i64 { return 2; }
}
pub class C extends B {
    pub override fn tag(self) -> i64 { return 3; }
}
fn accept_a(a: A) { println(1); }
fn main() {
    let c = new C();
    accept_a(c);
    println(new A().tag());
    println(new B().tag());
    println(c.tag());
}
"#,
    );
    assert!(ok);
    assert_eq!(out, "1\n1\n2\n3\n");
}

// 8. child with own field passes to parent-typed function; child's own method works
#[test]
fn test_subtype_child_with_extra_field_passes() {
    let (out, ok) = compile_and_run(
        r#"
pub open class Node {
    pub open fn kind(self) -> i64 { return 0; }
}
pub class Leaf extends Node {
    pub extra: i64;
    pub override fn kind(self) -> i64 { return 1; }
}
fn accept_node(n: Node) { println(1); }
fn main() {
    let leaf = new Leaf(99);
    accept_node(leaf);
    println(leaf.kind());
    println(leaf.extra);
}
"#,
    );
    assert!(ok);
    assert_eq!(out, "1\n1\n99\n");
}

// 9. base type rejected when child type expected (negative test)
#[test]
fn test_subtype_base_rejected_as_child() {
    assert!(expect_compile_error(
        r#"
pub open class Animal {}
pub class Dog extends Animal {}
fn use_dog(d: Dog) { println(1); }
fn main() {
    let a = new Animal();
    use_dog(a);
}
"#
    ));
}

// 10. sibling type rejected (not a subtype)
#[test]
fn test_subtype_sibling_rejected() {
    assert!(expect_compile_error(
        r#"
pub open class Animal {}
pub class Dog extends Animal {}
pub class Cat extends Animal {}
fn use_dog(d: Dog) { println(1); }
fn main() {
    let c = new Cat();
    use_dog(c);
}
"#
    ));
}

// ── Explicit Option<Base> construction from subtypes ─────────────────────

// 1. child passes to nullable parent param
#[test]
fn test_option_subtype_child_to_parent_payload() {
    let (out, ok) = compile_and_run(
        r#"
pub open class Animal {}
pub class Dog extends Animal {}
fn maybe_feed(a: Option<Animal>) { println(a.is_none()); }
fn main() {
    let d = new Dog();
    maybe_feed(Some(d));
}
"#,
    );
    assert!(ok);
    assert_eq!(out, "false\n");
}

// 2. None passes to an optional parent parameter.
#[test]
fn test_option_none_passes_to_parent() {
    let (out, ok) = compile_and_run(
        r#"
pub open class Animal {}
fn maybe_feed(a: Option<Animal>) { println(a.is_none()); }
fn main() {
    maybe_feed(None);
}
"#,
    );
    assert!(ok);
    assert_eq!(out, "true\n");
}

// 3. Option is opened inside the function.
#[test]
fn test_option_subtype_match_in_function() {
    let (out, ok) = compile_and_run(
        r#"
pub open class Animal {
    pub fn kind(self) -> i64 { return 1; }
}
pub class Dog extends Animal {}
fn describe(a: Option<Animal>) -> i64 {
    match a { Some(value) => return value.kind(), None => return -1 }
}
fn main() {
    let d = new Dog();
    println(describe(Some(d)));
    println(describe(None));
}
"#,
    );
    assert!(ok);
    assert_eq!(out, "1\n-1\n");
}

// 4. child stored in nullable parent variable
#[test]
fn test_option_child_stored_in_parent_payload() {
    let (out, ok) = compile_and_run(
        r#"
pub open class Animal {}
pub class Cat extends Animal {}
fn main() {
    let a: Option<Animal> = Some(new Cat());
    println(a.is_none());
}
"#,
    );
    assert!(ok);
    assert_eq!(out, "false\n");
}

// 5. None stored in an optional parent variable.
#[test]
fn test_option_none_stored_in_parent_var() {
    let (out, ok) = compile_and_run(
        r#"
pub open class Animal {}
fn main() {
    let a: Option<Animal> = None;
    println(a.is_none());
}
"#,
    );
    assert!(ok);
    assert_eq!(out, "true\n");
}

// 6. function returning nullable parent from child
#[test]
fn test_option_function_returns_child_as_parent_payload() {
    let (out, ok) = compile_and_run(
        r#"
pub open class Vehicle {
    pub fn tag(self) -> i64 { return 99; }
}
pub class Car extends Vehicle {}
fn maybe_car(use_it: bool) -> Option<Vehicle> {
    if use_it { return Some(new Car()); }
    return None;
}
fn main() {
    let v = maybe_car(true);
    println(v.is_none());
    let n = maybe_car(false);
    println(n.is_none());
}
"#,
    );
    assert!(ok);
    assert_eq!(out, "false\ntrue\n");
}

// 7. child through Option then method call in Some arm.
#[test]
fn test_option_child_then_method() {
    let (out, ok) = compile_and_run(
        r#"
pub open class Node {
    pub fn value(self) -> i64 { return 42; }
}
pub class Leaf extends Node {}
fn get_value(n: Option<Node>) -> i64 {
    match n { Some(value) => return value.value(), None => return 0 }
}
fn main() {
    let leaf = new Leaf();
    println(get_value(Some(leaf)));
    println(get_value(None));
}
"#,
    );
    assert!(ok);
    assert_eq!(out, "42\n0\n");
}

// 8. two different subtypes construct Option<Parent> explicitly.
#[test]
fn test_option_two_children_to_parent() {
    let (out, ok) = compile_and_run(
        r#"
pub open class Fruit {
    pub open fn name(self) -> i64 { return 0; }
}
pub class Apple extends Fruit {
    pub override fn name(self) -> i64 { return 1; }
}
pub class Orange extends Fruit {
    pub override fn name(self) -> i64 { return 2; }
}
fn present(f: Option<Fruit>) -> bool {
    return f.is_some();
}
fn main() {
    let a = new Apple();
    let o = new Orange();
    println(present(Some(a)));
    println(present(Some(o)));
    println(present(None));
    println(a.name());
    println(o.name());
}
"#,
    );
    assert!(ok);
    assert_eq!(out, "true\ntrue\nfalse\n1\n2\n");
}

// 9. child with field passed to nullable parent, field inaccessible through nullable
#[test]
fn test_option_child_field_inaccessible_without_opening() {
    assert!(expect_compile_error(
        r#"
pub open class Animal {}
pub class Dog extends Animal { pub breed: i64; }
fn main() {
    let d: Option<Animal> = Some(new Dog(1));
    println(d.breed);
}
"#
    ));
}

// 10. Option is invariant: Option<Sub> is not Option<Base>.
#[test]
fn test_option_child_does_not_widen_to_parent_option() {
    assert!(expect_compile_error(
        r#"
pub open class Base {}
pub class Sub extends Base {}
fn use_base(b: Option<Base>) -> i64 { return b.is_some() ? 1 : 0; }
fn main() {
    let s: Option<Sub> = Some(new Sub());
    println(use_base(s));
}
"#,
    ));
}
